use crate::usage::{decimal::Decimal, token_count};
use serde_json::{Map, Value, json};

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
        .or_else(|| key.strip_suffix("_ultrafast"))
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
    normalize_rates(source).map(Value::Object)
}

pub(super) fn normalize_rates(
    source: &Map<String, Value>,
) -> Result<Map<String, Value>, &'static str> {
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
    Ok(prices)
}
fn model_key<'a>(model: &str, prices: &'a Map<String, Value>) -> Option<&'a str> {
    if let Some((key, _)) = prices.get_key_value(model) {
        return Some(key);
    }
    const FAMILIES: [(&str, &str); 5] = [
        ("gpt-", "openai"),
        ("claude-", "anthropic"),
        ("gemini-", "gemini"),
        ("deepseek-", "deepseek"),
        ("grok-", "xai"),
    ];
    let family_for = |name: &str| {
        FAMILIES
            .iter()
            .find(|(prefix, _)| name.starts_with(prefix))
            .map(|(_, family)| *family)
    };
    let name = model
        .split_once('/')
        .and_then(|(vendor, name)| {
            let family = family_for(name)?;
            (vendor == family || (vendor == "google" && family == "gemini")).then_some(name)
        })
        .unwrap_or(model);
    if name != model
        && let Some((key, _)) = prices.get_key_value(name)
    {
        return Some(key);
    }
    let family = family_for(name)?;
    prices
        .get_key_value(&format!("{family}/{name}"))
        .map(|(key, _)| key.as_str())
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
        "ultrafast" => "_ultrafast",
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
/// User aliases take precedence over reported upstream IDs.
/// Account/provider prefixes are removed only from route-only records.
pub(super) fn price_source<'a>(
    event: &Value,
    prices: &'a Map<String, Value>,
    aliases: &Map<String, Value>,
) -> Option<&'a str> {
    let upstream = event["upstream_model"].as_str().unwrap_or("");
    let route = event["route_model"].as_str().unwrap_or("");
    let alias = [route, upstream]
        .into_iter()
        .filter(|name| !name.is_empty())
        .find_map(|name| aliases.get(name).and_then(Value::as_str));
    if let Some(alias) = alias {
        return model_key(alias, prices);
    }
    if !upstream.is_empty() {
        return model_key(upstream, prices);
    }
    model_key(route, prices).or_else(|| {
        route
            .split_once('/')
            .and_then(|(_, name)| model_key(name, prices))
    })
}
