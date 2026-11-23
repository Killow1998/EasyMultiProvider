//! Public API price snapshots and Decimal-compatible per-request estimates.
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

mod catalog;
mod rates;
pub use catalog::{ModelMetadata, apply_catalog_metadata};
use rates::price_source;
pub use rates::{estimate_tokens, normalize_prices};

pub const PRICE_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
pub const PRICE_INTERVAL: f64 = 86400.0;
const RATE_FORMAT: u64 = 2;
/// Bump when the lookup rules change so unpriced rows are quoted again.
const LOOKUP_RULES: u32 = 4;
struct PriceData {
    prices: Value,
    models: catalog::Catalog,
    aliases: Map<String, Value>,
    fetched: f64,
    rate_format: u64,
    revision: String,
    error: Option<&'static str>,
}
impl PriceData {
    fn revise(&mut self) {
        let basis = json!({"prices":self.prices,"aliases":self.aliases,"rules":LOOKUP_RULES});
        self.revision = format!(
            "{:x}",
            Sha256::digest(super::python_json(&basis, false).as_bytes())
        );
    }
}
pub struct PriceCatalog {
    path: PathBuf,
    data: Mutex<PriceData>,
}
impl PriceCatalog {
    pub fn new(path: PathBuf, now: f64) -> Self {
        let catalog = Self {
            path,
            data: Mutex::new(PriceData {
                prices: json!({}),
                models: catalog::Catalog::new(),
                aliases: Map::new(),
                fetched: 0.0,
                rate_format: 0,
                revision: String::new(),
                error: None,
            }),
        };
        if let Ok(raw) = std::fs::read(&catalog.path)
            && let Ok(value) = serde_json::from_slice::<Value>(&raw)
            && let Some(fetched) = value["fetched_at"]
                .as_f64()
                .filter(|stamp| *stamp > 0.0 && *stamp <= now)
            && let Ok(prices) = normalize_prices(&value["prices"])
        {
            let models =
                serde_json::from_value(value["model_catalog"].clone()).unwrap_or_else(|_| {
                    let legacy: Vec<ModelMetadata> =
                        serde_json::from_value(value["vertex_models"].clone()).unwrap_or_default();
                    catalog::Catalog::from([(
                        "vertex_ai".into(),
                        legacy
                            .into_iter()
                            .map(|m| (m.upstream_id.clone(), m))
                            .collect(),
                    )])
                });
            let format = value["rate_format"].as_u64().unwrap_or_default();
            catalog.replace_snapshot(prices, models, fetched, format);
        }
        catalog
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn replace(&self, prices: Value, fetched: f64) {
        self.replace_snapshot(prices, catalog::Catalog::new(), fetched, RATE_FORMAT);
    }
    fn replace_snapshot(
        &self,
        prices: Value,
        models: catalog::Catalog,
        fetched: f64,
        rate_format: u64,
    ) {
        let mut data = self.data.lock().expect("price catalog");
        data.prices = prices;
        data.models = models;
        data.fetched = fetched;
        data.rate_format = rate_format;
        data.error = None;
        data.revise();
    }
    pub fn save_litellm_snapshot(
        &self,
        payload: &Value,
        fetched: f64,
    ) -> Result<usize, &'static str> {
        let entries = payload.as_object().ok_or("invalid_prices")?;
        let prices = rates::normalize_rates(entries).map_err(|_| "invalid_prices")?;
        let count = prices.len();
        let models = catalog::from_litellm(entries);
        crate::filesystem::atomic_write_private_state(
            self.path(),
            &serde_json::to_vec(&json!({
                "prices":prices,"fetched_at":fetched,"model_catalog":models,"rate_format":RATE_FORMAT,
            }))
            .expect("price JSON"),
        )
        .map_err(|_| "storage_error")?;
        self.replace_snapshot(Value::Object(prices), models, fetched, RATE_FORMAT);
        Ok(count)
    }
    pub fn vertex_models(&self) -> Vec<ModelMetadata> {
        let data = self.data.lock().expect("price catalog");
        data.models
            .get("vertex_ai")
            .into_iter()
            .flat_map(|rows| rows.values())
            .cloned()
            .collect()
    }

    pub fn enrich_models(&self, provider: &Map<String, Value>, models: &mut [Value]) {
        let official = crate::official_registry::identify_provider(provider);
        let source = if provider.contains_key("vertex") {
            Some("vertex_ai")
        } else {
            match official.as_deref() {
                Some("google_gemini") => Some("gemini"),
                Some("zhipu_glm")
                    if provider["base_url"]
                        .as_str()
                        .is_some_and(|url| url.starts_with("https://open.bigmodel.cn/")) =>
                {
                    Some("bigmodel")
                }
                Some("zhipu_glm") => Some("zai"),
                other => other,
            }
        };
        let data = self.data.lock().expect("price catalog");
        for model in models {
            let Some(id) = model["upstream_id"].as_str() else {
                continue;
            };
            let row = match source {
                Some(source) => data.models.get(source).and_then(|rows| rows.get(id)),
                None => id
                    .split_once('/')
                    .and_then(|(source, id)| data.models.get(source)?.get(id)),
            };
            if let Some(row) = row {
                row.apply(model);
            }
        }
    }
    /// Apply the configured `pricing_aliases`; returns whether they changed.
    pub fn set_aliases(&self, aliases: &Value) -> bool {
        let aliases = aliases.as_object().cloned().unwrap_or_default();
        let mut data = self.data.lock().expect("price catalog");
        if data.aliases == aliases {
            return false;
        }
        data.aliases = aliases;
        data.revise();
        true
    }
    pub fn refresh_failed(&self) {
        self.data.lock().expect("price catalog").error = Some("refresh_failed");
    }
    pub fn revision(&self) -> String {
        self.data.lock().expect("price catalog").revision.clone()
    }
    pub fn quote(&self, event: &Value) -> Result<Value, &'static str> {
        let data = self.data.lock().map_err(|_| "price catalog unavailable")?;
        let key = price_source(event, data.prices.as_object().unwrap(), &data.aliases);
        let tier = event["service_tier"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("default");
        let mut quote = estimate_tokens(
            event,
            key.map(|key| &data.prices[key]).unwrap_or(&Value::Null),
            tier,
        )?;
        if key.is_none() {
            let name = quote["price_issue"].as_str().unwrap_or("");
            if !matches!(name, "missing_usage" | "invalid_usage") {
                quote["price_issue"] = json!("unknown_model");
            }
            quote["cost_nanos"] = Value::Null;
            quote["rates"] = json!({});
        }
        // Preserve the ledger's legacy placeholder; readers must check price_issue.
        if matches!(
            quote["price_issue"].as_str(),
            Some("unknown_model" | "missing_rate")
        ) {
            quote["cost_nanos"] = json!(0);
        }
        quote["price_key"] = json!(key);
        quote["price_fetched_at"] = json!(data.fetched);
        quote["price_revision"] = json!(data.revision);
        Ok(quote)
    }
    pub fn snapshot(&self, now: f64) -> Value {
        let data = self.data.lock().expect("price catalog");
        json!({"source":"LiteLLM","url":PRICE_URL,"fetched_at":if data.fetched>0.0{json!(data.fetched)}else{Value::Null},"stale":data.rate_format != RATE_FORMAT || now-data.fetched>=PRICE_INTERVAL,"error":data.error,"model_count":data.prices.as_object().unwrap().len(),"interval_hours":24})
    }
}

#[cfg(test)]
mod lookup_tests {
    use super::PriceCatalog;
    use serde_json::{Value, json};

    fn catalog() -> PriceCatalog {
        let catalog = PriceCatalog::new(std::path::PathBuf::from("/nonexistent/prices.json"), 1.0);
        catalog.replace(
            json!({"gpt-5":{"input_cost_per_token":"0.000001","output_cost_per_token":"0.000002"}}),
            1.0,
        );
        catalog
    }
    fn request(route: &str, upstream: &str) -> Value {
        json!({"route_model":route,"upstream_model":upstream,"input_tokens":1000,"output_tokens":10})
    }

    #[test]
    fn account_prefixed_routes_use_the_public_price() {
        let quote = catalog().quote(&request("work/gpt-5", "gpt-5")).unwrap();
        assert_eq!(quote["price_key"], "gpt-5");
        assert_eq!(quote["cost_nanos"], 1_020_000);
        for upstream in ["openai/gpt-5", "gpt-5", ""] {
            assert_eq!(
                catalog().quote(&request("work/gpt-5", upstream)).unwrap()["price_key"],
                "gpt-5"
            );
        }
        for upstream in ["private/gpt-5", "local-model"] {
            let quote = catalog().quote(&request("work/gpt-5", upstream)).unwrap();
            assert_eq!(quote["price_issue"], "unknown_model", "{quote}");
        }
    }

    #[test]
    fn upgrading_a_price_snapshot_loads_ultrafast_rates_and_keeps_standard_available() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("api_prices.json");
        let prices = json!({"gpt-6.2-sol":{
            "input_cost_per_token":0.000002,"output_cost_per_token":0.00001,
        }});
        std::fs::write(&path, json!({"fetched_at":1,"prices":prices}).to_string()).unwrap();
        let catalog = PriceCatalog::new(path.clone(), 2.0);
        let mut event = request("team/gpt-6.2-sol", "gpt-6.2-sol");
        assert_eq!(catalog.quote(&event).unwrap()["cost_nanos"], 2_100_000);
        assert_eq!(catalog.snapshot(2.0)["stale"], true);
        let mut refreshed = prices;
        refreshed["gpt-6.2-sol"]["input_cost_per_token_ultrafast"] = json!(0.000006);
        refreshed["gpt-6.2-sol"]["output_cost_per_token_ultrafast"] = json!(0.00003);
        catalog.save_litellm_snapshot(&refreshed, 2.0).unwrap();
        let reloaded = PriceCatalog::new(path, 3.0);
        event["service_tier"] = json!("ultrafast");
        let quote = reloaded.quote(&event).unwrap();
        assert_eq!(quote["price_issue"], Value::Null);
        assert_eq!(quote["cost_nanos"], 6_300_000);
        assert_eq!(reloaded.snapshot(3.0)["stale"], false);
    }

    #[test]
    fn unknown_models_cost_nothing_until_an_alias_prices_them() {
        let catalog = catalog();
        let before = catalog.revision();
        let unknown = catalog
            .quote(&request("lab/local-model", "local-model"))
            .unwrap();
        assert_eq!(unknown["price_issue"], "unknown_model");
        assert_eq!(unknown["cost_nanos"], 0);
        assert!(catalog.set_aliases(&json!({"lab/local-model":"gpt-5"})));
        assert_ne!(
            catalog.revision(),
            before,
            "changed aliases must re-price old rows"
        );
        assert!(!catalog.set_aliases(&json!({"lab/local-model":"gpt-5"})));
        let priced = catalog
            .quote(&request("lab/local-model", "local-model"))
            .unwrap();
        assert_eq!(priced["cost_nanos"], 1_020_000);
        assert_eq!(priced["price_issue"], Value::Null);
        let unreported = catalog
            .quote(&json!({"route_model":"lab/local-model","upstream_model":"local-model"}))
            .unwrap();
        assert_eq!(unreported["price_issue"], "missing_usage");
        assert_eq!(unreported["cost_nanos"], Value::Null);
    }
}
