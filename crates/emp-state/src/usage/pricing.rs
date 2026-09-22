//! Public API price snapshots and Decimal-compatible per-request estimates.
use super::decimal::Decimal;
use super::token_count;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
pub const PRICE_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
pub const PRICE_INTERVAL: f64 = 86400.0;
const BASES: [&str; 5] = [
    "input_cost_per_token",
    "output_cost_per_token",
    "output_cost_per_reasoning_token",
    "cache_read_input_token_cost",
    "cache_creation_input_token_cost",
];
fn rate_key(key: &str) -> bool {
    let key = key
        .strip_suffix("_priority")
        .or_else(|| key.strip_suffix("_flex"))
        .unwrap_or(key);
    BASES.iter().any(|base| {
        key.strip_prefix(base).is_some_and(|rest| {
            rest.is_empty()
                || rest == "_above_1hr"
                || rest
                    .strip_prefix("_above_")
                    .and_then(|value| value.strip_suffix("k_tokens"))
                    .is_some_and(|digits| {
                        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
                    })
        })
    })
}
pub fn normalize_prices(payload: &Value) -> Result<Value, &'static str> {
    let source = payload.as_object().ok_or("Invalid price catalog")?;
    let mut prices = Map::new();
    for (model, entry) in source {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let mut rates = Map::new();
        let mut valid = true;
        for (key, value) in entry {
            if !rate_key(key) {
                continue;
            }
            let text = match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                _ => {
                    valid = false;
                    break;
                }
            };
            let Some(number) = Decimal::parse(&text) else {
                valid = false;
                break;
            };
            rates.insert(key.clone(), json!(number.canonical()));
        }
        if valid
            && rates.contains_key("input_cost_per_token")
            && rates.contains_key("output_cost_per_token")
        {
            prices.insert(model.clone(), Value::Object(rates));
        }
    }
    if prices.is_empty() {
        return Err("No valid token prices");
    }
    Ok(Value::Object(prices))
}
fn model_key<'a>(model: &str, prices: &'a Map<String, Value>) -> Option<&'a str> {
    if let Some((key, _)) = prices.get_key_value(model) {
        return Some(key);
    }
    for (prefix, family) in [
        ("gemini-", "gemini"),
        ("claude-", "anthropic"),
        ("gpt-", "openai"),
        ("deepseek-", "deepseek"),
        ("grok-", "xai"),
    ] {
        if model.starts_with(prefix)
            && let Some((key, _)) = prices.get_key_value(&format!("{family}/{model}"))
        {
            return Some(key);
        }
    }
    None
}
fn rate(rates: &Map<String, Value>, key: &str, total: u64, suffix: &str) -> Option<Decimal> {
    let prefix = format!("{key}_above_");
    let ending = format!("k_tokens{suffix}");
    let threshold = rates
        .keys()
        .filter_map(|candidate| {
            candidate
                .strip_prefix(&prefix)?
                .strip_suffix(&ending)?
                .parse::<u64>()
                .ok()?
                .checked_mul(1000)
        })
        .filter(|value| total > *value)
        .max()
        .unwrap_or(0);
    let context = if threshold > 0 {
        format!("_above_{}k_tokens", threshold / 1000)
    } else {
        String::new()
    };
    rates
        .get(&format!("{key}{context}{suffix}"))
        .or_else(|| rates.get(&format!("{key}{suffix}")))?
        .as_str()
        .and_then(Decimal::parse)
}
fn issue(name: &str) -> Value {
    json!({"cost_nanos":null,"price_issue":name,"rates":{}})
}
pub fn estimate_tokens(usage: &Value, rates: &Value, tier: &str) -> Result<Value, &'static str> {
    let (Some(total), Some(output)) = (
        token_count(usage.get("input_tokens")),
        token_count(usage.get("output_tokens")),
    ) else {
        return Ok(issue("missing_usage"));
    };
    let suffix = match tier {
        "default" | "auto" | "standard" => "",
        "fast" | "priority" => "_priority",
        "flex" => "_flex",
        _ => return Ok(issue("unknown_tier")),
    };
    let empty = Map::new();
    let rates = rates.as_object().unwrap_or(&empty);
    let get = |key| rate(rates, key, total, suffix);
    let cached = token_count(usage.get("cached_input_tokens"));
    let optional = |key| token_count(usage.get(key).or(Some(&Value::from(0))));
    let (Some(written), Some(hour), Some(reasoning)) = (
        optional("cache_write_tokens"),
        optional("cache_write_1h_tokens"),
        optional("reasoning_tokens"),
    ) else {
        return Ok(issue("invalid_usage"));
    };
    if hour > written || reasoning > output {
        return Ok(issue("invalid_usage"));
    }
    let cached = match cached {
        None if total > 0 && get("cache_read_input_token_cost").is_some() => {
            return Ok(issue("missing_cache_usage"));
        }
        value => value.unwrap_or(0),
    };
    if cached + written > total {
        return Ok(issue("invalid_usage"));
    }
    let reasoning_rate = get("output_cost_per_reasoning_token");
    let output_rate = get("output_cost_per_token");
    if output > 0
        && usage.get("reasoning_tokens").is_none()
        && reasoning_rate.as_ref().is_some_and(|rate| {
            output_rate
                .as_ref()
                .is_none_or(|output| !rate.numeric_equal(output))
        })
    {
        return Ok(issue("missing_reasoning_usage"));
    }
    let components = [
        (
            "input",
            total - cached - written,
            get("input_cost_per_token"),
        ),
        ("cache_read", cached, get("cache_read_input_token_cost")),
        (
            "cache_write",
            written - hour,
            get("cache_creation_input_token_cost"),
        ),
        (
            "cache_write_1h",
            hour,
            get("cache_creation_input_token_cost_above_1hr"),
        ),
        ("output", output - reasoning, output_rate.clone()),
        ("reasoning", reasoning, reasoning_rate.or(output_rate)),
    ];
    if components
        .iter()
        .any(|(_, count, rate)| *count > 0 && rate.is_none())
    {
        return Ok(issue("missing_rate"));
    }
    let mut cost = Decimal::zero();
    let mut applied = Map::new();
    for (name, count, rate) in components {
        if let Some(rate) = rate {
            cost = cost
                .add(&rate.multiply(count).ok_or("price arithmetic failed")?)
                .ok_or("price arithmetic failed")?;
            applied.insert(name.into(), json!(rate.canonical()));
        }
    }
    Ok(
        json!({"cost_nanos":cost.nanos().ok_or("price arithmetic failed")?,"price_issue":null,"rates":applied}),
    )
}
struct PriceData {
    prices: Value,
    fetched: f64,
    revision: String,
    error: Option<&'static str>,
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
                fetched: 0.0,
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
            catalog.replace(prices, fetched);
        }
        catalog
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn replace(&self, prices: Value, fetched: f64) {
        let revision = format!(
            "{:x}",
            Sha256::digest(super::python_json(&prices, false).as_bytes())
        );
        *self.data.lock().expect("price catalog") = PriceData {
            prices,
            fetched,
            revision,
            error: None,
        };
    }
    pub fn refresh_failed(&self) {
        self.data.lock().expect("price catalog").error = Some("refresh_failed");
    }
    pub fn revision(&self) -> String {
        self.data.lock().expect("price catalog").revision.clone()
    }
    pub fn quote(&self, event: &Value) -> Result<Value, &'static str> {
        let data = self.data.lock().map_err(|_| "price catalog unavailable")?;
        let key = model_key(
            event["upstream_model"].as_str().unwrap_or(""),
            data.prices.as_object().unwrap(),
        );
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
        quote["price_key"] = json!(key);
        quote["price_fetched_at"] = json!(data.fetched);
        quote["price_revision"] = json!(data.revision);
        Ok(quote)
    }
    pub fn snapshot(&self, now: f64) -> Value {
        let data = self.data.lock().expect("price catalog");
        json!({"source":"LiteLLM","url":PRICE_URL,"fetched_at":if data.fetched>0.0{json!(data.fetched)}else{Value::Null},"stale":now-data.fetched>=PRICE_INTERVAL,"error":data.error,"model_count":data.prices.as_object().unwrap().len(),"interval_hours":24})
    }
}
