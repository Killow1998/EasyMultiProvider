//! Pure native response-header metadata projection.
//!
//! This is the bounded Rust port of the Python response boundary: select the
//! headers native Codex consumes, project only known upstream model headers,
//! and preserve the event's response model.

use serde_json::{Map, Value};

const RESPONSE_HEADER_NAMES: [&str; 18] = [
    "cf-ray",
    "openai-model",
    "x-openai-model",
    "x-error-json",
    "x-oai-request-id",
    "x-openai-authorization-error",
    "x-request-id",
    "x-models-etag",
    "x-reasoning-included",
    "x-codex-active-limit",
    "x-codex-credits-balance",
    "x-codex-credits-has-credits",
    "x-codex-credits-unlimited",
    "x-codex-promo-message",
    "x-codex-rate-limit-reached-type",
    "x-codex-safety-buffering-enabled",
    "x-codex-safety-buffering-faster-model",
    "x-codex-turn-state",
];

fn is_rate_header(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("x-") else {
        return false;
    };
    for metric in ["used-percent", "window-minutes", "reset-at"] {
        let Some(body) = rest.strip_suffix(metric) else {
            continue;
        };
        let Some(after_side) = body.strip_suffix('-') else {
            return false;
        };
        let Some((provider, side)) = after_side.rsplit_once('-') else {
            return false;
        };
        return matches!(side, "primary" | "secondary")
            && !provider.is_empty()
            && provider.chars().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
            });
    }
    false
}

fn is_rate_limit_name_header(name: &str) -> bool {
    let Some(provider) = name
        .strip_prefix("x-")
        .and_then(|value| value.strip_suffix("-limit-name"))
    else {
        return false;
    };
    !provider.is_empty()
        && provider.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

fn allowed_header(name: &str) -> bool {
    RESPONSE_HEADER_NAMES.contains(&name) || is_rate_header(name) || is_rate_limit_name_header(name)
}

// Python casefold differs from lowercase for Unicode aliases. These exceptions
// follow the Python oracle's Unicode 16.0.0 data; Cherokee folds to uppercase.
fn casefold_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\u{13a0}'..='\u{13f5}' => result.push(character),
            '\u{13f8}'..='\u{13fd}' | '\u{ab70}'..='\u{abbf}' => {
                result.extend(character.to_uppercase())
            }
            '\u{b5}' => result.push('\u{3bc}'),
            '\u{df}' => result.push_str("\u{73}\u{73}"),
            '\u{149}' => result.push_str("\u{2bc}\u{6e}"),
            '\u{17f}' => result.push('\u{73}'),
            '\u{1f0}' => result.push_str("\u{6a}\u{30c}"),
            '\u{345}' => result.push('\u{3b9}'),
            '\u{390}' => result.push_str("\u{3b9}\u{308}\u{301}"),
            '\u{3b0}' => result.push_str("\u{3c5}\u{308}\u{301}"),
            '\u{3c2}' => result.push('\u{3c3}'),
            '\u{3d0}' => result.push('\u{3b2}'),
            '\u{3d1}' => result.push('\u{3b8}'),
            '\u{3d5}' => result.push('\u{3c6}'),
            '\u{3d6}' => result.push('\u{3c0}'),
            '\u{3f0}' => result.push('\u{3ba}'),
            '\u{3f1}' => result.push('\u{3c1}'),
            '\u{3f5}' => result.push('\u{3b5}'),
            '\u{587}' => result.push_str("\u{565}\u{582}"),
            '\u{1c80}' => result.push('\u{432}'),
            '\u{1c81}' => result.push('\u{434}'),
            '\u{1c82}' => result.push('\u{43e}'),
            '\u{1c83}' => result.push('\u{441}'),
            '\u{1c84}' => result.push('\u{442}'),
            '\u{1c85}' => result.push('\u{442}'),
            '\u{1c86}' => result.push('\u{44a}'),
            '\u{1c87}' => result.push('\u{463}'),
            '\u{1c88}' => result.push('\u{a64b}'),
            '\u{1e96}' => result.push_str("\u{68}\u{331}"),
            '\u{1e97}' => result.push_str("\u{74}\u{308}"),
            '\u{1e98}' => result.push_str("\u{77}\u{30a}"),
            '\u{1e99}' => result.push_str("\u{79}\u{30a}"),
            '\u{1e9a}' => result.push_str("\u{61}\u{2be}"),
            '\u{1e9b}' => result.push('\u{1e61}'),
            '\u{1e9e}' => result.push_str("\u{73}\u{73}"),
            '\u{1f50}' => result.push_str("\u{3c5}\u{313}"),
            '\u{1f52}' => result.push_str("\u{3c5}\u{313}\u{300}"),
            '\u{1f54}' => result.push_str("\u{3c5}\u{313}\u{301}"),
            '\u{1f56}' => result.push_str("\u{3c5}\u{313}\u{342}"),
            '\u{1f80}' => result.push_str("\u{1f00}\u{3b9}"),
            '\u{1f81}' => result.push_str("\u{1f01}\u{3b9}"),
            '\u{1f82}' => result.push_str("\u{1f02}\u{3b9}"),
            '\u{1f83}' => result.push_str("\u{1f03}\u{3b9}"),
            '\u{1f84}' => result.push_str("\u{1f04}\u{3b9}"),
            '\u{1f85}' => result.push_str("\u{1f05}\u{3b9}"),
            '\u{1f86}' => result.push_str("\u{1f06}\u{3b9}"),
            '\u{1f87}' => result.push_str("\u{1f07}\u{3b9}"),
            '\u{1f88}' => result.push_str("\u{1f00}\u{3b9}"),
            '\u{1f89}' => result.push_str("\u{1f01}\u{3b9}"),
            '\u{1f8a}' => result.push_str("\u{1f02}\u{3b9}"),
            '\u{1f8b}' => result.push_str("\u{1f03}\u{3b9}"),
            '\u{1f8c}' => result.push_str("\u{1f04}\u{3b9}"),
            '\u{1f8d}' => result.push_str("\u{1f05}\u{3b9}"),
            '\u{1f8e}' => result.push_str("\u{1f06}\u{3b9}"),
            '\u{1f8f}' => result.push_str("\u{1f07}\u{3b9}"),
            '\u{1f90}' => result.push_str("\u{1f20}\u{3b9}"),
            '\u{1f91}' => result.push_str("\u{1f21}\u{3b9}"),
            '\u{1f92}' => result.push_str("\u{1f22}\u{3b9}"),
            '\u{1f93}' => result.push_str("\u{1f23}\u{3b9}"),
            '\u{1f94}' => result.push_str("\u{1f24}\u{3b9}"),
            '\u{1f95}' => result.push_str("\u{1f25}\u{3b9}"),
            '\u{1f96}' => result.push_str("\u{1f26}\u{3b9}"),
            '\u{1f97}' => result.push_str("\u{1f27}\u{3b9}"),
            '\u{1f98}' => result.push_str("\u{1f20}\u{3b9}"),
            '\u{1f99}' => result.push_str("\u{1f21}\u{3b9}"),
            '\u{1f9a}' => result.push_str("\u{1f22}\u{3b9}"),
            '\u{1f9b}' => result.push_str("\u{1f23}\u{3b9}"),
            '\u{1f9c}' => result.push_str("\u{1f24}\u{3b9}"),
            '\u{1f9d}' => result.push_str("\u{1f25}\u{3b9}"),
            '\u{1f9e}' => result.push_str("\u{1f26}\u{3b9}"),
            '\u{1f9f}' => result.push_str("\u{1f27}\u{3b9}"),
            '\u{1fa0}' => result.push_str("\u{1f60}\u{3b9}"),
            '\u{1fa1}' => result.push_str("\u{1f61}\u{3b9}"),
            '\u{1fa2}' => result.push_str("\u{1f62}\u{3b9}"),
            '\u{1fa3}' => result.push_str("\u{1f63}\u{3b9}"),
            '\u{1fa4}' => result.push_str("\u{1f64}\u{3b9}"),
            '\u{1fa5}' => result.push_str("\u{1f65}\u{3b9}"),
            '\u{1fa6}' => result.push_str("\u{1f66}\u{3b9}"),
            '\u{1fa7}' => result.push_str("\u{1f67}\u{3b9}"),
            '\u{1fa8}' => result.push_str("\u{1f60}\u{3b9}"),
            '\u{1fa9}' => result.push_str("\u{1f61}\u{3b9}"),
            '\u{1faa}' => result.push_str("\u{1f62}\u{3b9}"),
            '\u{1fab}' => result.push_str("\u{1f63}\u{3b9}"),
            '\u{1fac}' => result.push_str("\u{1f64}\u{3b9}"),
            '\u{1fad}' => result.push_str("\u{1f65}\u{3b9}"),
            '\u{1fae}' => result.push_str("\u{1f66}\u{3b9}"),
            '\u{1faf}' => result.push_str("\u{1f67}\u{3b9}"),
            '\u{1fb2}' => result.push_str("\u{1f70}\u{3b9}"),
            '\u{1fb3}' => result.push_str("\u{3b1}\u{3b9}"),
            '\u{1fb4}' => result.push_str("\u{3ac}\u{3b9}"),
            '\u{1fb6}' => result.push_str("\u{3b1}\u{342}"),
            '\u{1fb7}' => result.push_str("\u{3b1}\u{342}\u{3b9}"),
            '\u{1fbc}' => result.push_str("\u{3b1}\u{3b9}"),
            '\u{1fbe}' => result.push('\u{3b9}'),
            '\u{1fc2}' => result.push_str("\u{1f74}\u{3b9}"),
            '\u{1fc3}' => result.push_str("\u{3b7}\u{3b9}"),
            '\u{1fc4}' => result.push_str("\u{3ae}\u{3b9}"),
            '\u{1fc6}' => result.push_str("\u{3b7}\u{342}"),
            '\u{1fc7}' => result.push_str("\u{3b7}\u{342}\u{3b9}"),
            '\u{1fcc}' => result.push_str("\u{3b7}\u{3b9}"),
            '\u{1fd2}' => result.push_str("\u{3b9}\u{308}\u{300}"),
            '\u{1fd3}' => result.push_str("\u{3b9}\u{308}\u{301}"),
            '\u{1fd6}' => result.push_str("\u{3b9}\u{342}"),
            '\u{1fd7}' => result.push_str("\u{3b9}\u{308}\u{342}"),
            '\u{1fe2}' => result.push_str("\u{3c5}\u{308}\u{300}"),
            '\u{1fe3}' => result.push_str("\u{3c5}\u{308}\u{301}"),
            '\u{1fe4}' => result.push_str("\u{3c1}\u{313}"),
            '\u{1fe6}' => result.push_str("\u{3c5}\u{342}"),
            '\u{1fe7}' => result.push_str("\u{3c5}\u{308}\u{342}"),
            '\u{1ff2}' => result.push_str("\u{1f7c}\u{3b9}"),
            '\u{1ff3}' => result.push_str("\u{3c9}\u{3b9}"),
            '\u{1ff4}' => result.push_str("\u{3ce}\u{3b9}"),
            '\u{1ff6}' => result.push_str("\u{3c9}\u{342}"),
            '\u{1ff7}' => result.push_str("\u{3c9}\u{342}\u{3b9}"),
            '\u{1ffc}' => result.push_str("\u{3c9}\u{3b9}"),
            '\u{fb00}' => result.push_str("\u{66}\u{66}"),
            '\u{fb01}' => result.push_str("\u{66}\u{69}"),
            '\u{fb02}' => result.push_str("\u{66}\u{6c}"),
            '\u{fb03}' => result.push_str("\u{66}\u{66}\u{69}"),
            '\u{fb04}' => result.push_str("\u{66}\u{66}\u{6c}"),
            '\u{fb05}' => result.push_str("\u{73}\u{74}"),
            '\u{fb06}' => result.push_str("\u{73}\u{74}"),
            '\u{fb13}' => result.push_str("\u{574}\u{576}"),
            '\u{fb14}' => result.push_str("\u{574}\u{565}"),
            '\u{fb15}' => result.push_str("\u{574}\u{56b}"),
            '\u{fb16}' => result.push_str("\u{57e}\u{576}"),
            '\u{fb17}' => result.push_str("\u{574}\u{56d}"),
            other => result.extend(other.to_lowercase()),
        }
    }
    result
}

fn rewrite_model_value(value: Value, requested_model: &str, upstream_model: &str) -> Value {
    if requested_model.is_empty() || upstream_model.is_empty() || !matches!(value, Value::String(_))
    {
        return value;
    }
    let text = value.as_str().expect("string model value");
    if casefold_string(text) == casefold_string(upstream_model) {
        Value::String(requested_model.to_owned())
    } else {
        value
    }
}

/// Rewrite model-valued native headers in a JSON headers object.
pub fn rewrite_native_model_headers(
    headers: &Value,
    requested_model: &str,
    upstream_model: &str,
) -> Map<String, Value> {
    let Some(items) = headers.as_object() else {
        return Map::new();
    };
    let mut result = Map::new();
    for (name, value) in items {
        if name.to_lowercase() == "openai-model" || name.to_lowercase() == "x-openai-model" {
            result.insert(
                name.clone(),
                rewrite_model_value(value.clone(), requested_model, upstream_model),
            );
        } else {
            result.insert(name.clone(), value.clone());
        }
    }
    result
}

/// Rewrite native model headers in `event.headers` and `event.response.headers`.
pub fn rewrite_native_model_event(
    event: &Value,
    requested_model: &str,
    upstream_model: &str,
) -> Value {
    let Some(object) = event.as_object() else {
        return event.clone();
    };
    let mut changed = false;
    let mut rewritten = object.clone();

    let top_headers = rewritten.get("headers").cloned();
    if let Some(headers @ Value::Object(_)) = top_headers {
        let projected = rewrite_native_model_headers(&headers, requested_model, upstream_model);
        if headers.as_object() != Some(&projected) {
            rewritten.insert("headers".to_owned(), Value::Object(projected));
            changed = true;
        }
    }

    let response = rewritten.get("response").cloned();
    if let Some(response) = response {
        let headers = response.get("headers").cloned();
        if let Some(headers @ Value::Object(_)) = headers {
            let projected = rewrite_native_model_headers(&headers, requested_model, upstream_model);
            if headers.as_object() != Some(&projected) {
                let mut projected_response = response.as_object().cloned().unwrap_or_default();
                projected_response.insert("headers".to_owned(), Value::Object(projected));
                rewritten.insert("response".to_owned(), Value::Object(projected_response));
                changed = true;
            }
        }
    }

    if changed {
        Value::Object(rewritten)
    } else {
        event.clone()
    }
}

/// Select and project non-credential headers from a response-like JSON value.
pub fn native_response_headers(
    response: &Value,
    requested_model: &str,
    upstream_model: &str,
) -> Map<String, Value> {
    let Some(headers) = response.get("headers").and_then(Value::as_object) else {
        return Map::new();
    };
    let mut selected = Map::new();
    for (raw_name, raw_value) in headers {
        let Value::String(_) = raw_value else {
            continue;
        };
        let name = raw_name.to_lowercase();
        if !allowed_header(&name) {
            continue;
        }
        selected.insert(name, raw_value.clone());
    }
    let projected =
        rewrite_native_model_headers(&Value::Object(selected), requested_model, upstream_model);
    projected
        .into_iter()
        .filter(|(_, value)| matches!(value, Value::String(_)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_header_shapes_are_bounded() {
        assert!(is_rate_header("x-example-primary-used-percent"));
        assert!(is_rate_header("x-example2-secondary-window-minutes"));
        assert!(is_rate_limit_name_header("x-example-limit-name"));
        assert!(!is_rate_header("x-example-tertiary-used-percent"));
        assert!(!is_rate_limit_name_header("x--limit-name"));
        assert!(is_rate_limit_name_header("x---limit-name"));
        assert!(is_rate_header("x---primary-used-percent"));
    }

    #[test]
    fn response_model_is_preserved() {
        let event = serde_json::json!({
            "response": {
                "model": "gpt-6",
                "headers": {"openai-model": "GPT-6"},
            }
        });
        let projected = rewrite_native_model_event(&event, "native/gpt-6", "gpt-6");
        assert_eq!(projected["response"]["model"], "gpt-6");
        assert_eq!(
            projected["response"]["headers"]["openai-model"],
            "native/gpt-6"
        );
    }
}
