//! Validation and reasoning-effort policy shared by complete and streamed requests.
use super::{
    Dialect, FailureClass, Protocol, ResolvedRoute, RouterError, RouterErrorKind, invalid_request,
};
use serde_json::Value;
use std::borrow::Cow;

pub(super) fn validate(
    route: &ResolvedRoute,
    body: &Value,
    streaming: bool,
) -> Result<(), RouterError> {
    let body = body
        .as_object()
        .ok_or_else(|| invalid_request("request body must be an object"))?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| invalid_request("request.model is required"))?;
    if model != route.requested_model {
        return Err(invalid_request(
            "resolved route does not match request.model",
        ));
    }
    if (body.get("stream").and_then(Value::as_bool) == Some(true)) != streaming {
        return Err(invalid_request(if streaming {
            "stream routing requires stream=true"
        } else {
            "complete routing does not accept a streamed request"
        }));
    }
    if !matches!(
        (route.dialect, route.protocol),
        (Dialect::PortableResponses, Protocol::Responses)
            | (Dialect::ChatCompletions, Protocol::ChatCompletions)
            | (Dialect::AnthropicMessages, Protocol::AnthropicMessages)
    ) {
        let (reason, message) = if streaming {
            (
                "unsupported_stream_dialect",
                "streaming external dialect is not implemented",
            )
        } else {
            (
                "unsupported_complete_dialect",
                "complete external dialect is not implemented",
            )
        };
        return Err(RouterError::new(
            RouterErrorKind::UnsupportedProtocol,
            501,
            FailureClass::ProtocolRejection,
            Some(reason.to_owned()),
            None,
            message,
        ));
    }
    Ok(())
}

pub(super) fn body_with_supported_effort<'a>(
    route: &ResolvedRoute,
    body: &'a Value,
) -> Cow<'a, Value> {
    let Some(source) = body.as_object() else {
        return Cow::Borrowed(body);
    };
    let provider = route.provider.value();
    if matches!(
        provider.get("auth_mode").and_then(Value::as_str),
        Some("account" | "forward")
    ) {
        return Cow::Borrowed(body);
    }
    let Some(reasoning) = source.get("reasoning").and_then(Value::as_object) else {
        return Cow::Borrowed(body);
    };
    let Some(effort) = reasoning.get("effort") else {
        return Cow::Borrowed(body);
    };
    let model = route.model.value();
    let levels = model.get("reasoning_levels").and_then(Value::as_array);
    let persistent_alias = route.protocol == Protocol::Responses
        && effort.as_str() == Some("disabled")
        && levels.is_some_and(|levels| {
            levels
                .iter()
                .any(|level| level.as_str() == Some("persistent"))
        });
    let unsupported = model.get("supports_reasoning").and_then(Value::as_bool) == Some(false)
        || levels.is_some_and(|levels| {
            !levels.is_empty() && !levels.contains(effort) && !persistent_alias
        });
    if !unsupported {
        return Cow::Borrowed(body);
    }
    let mut projected = source.clone();
    let mut reasoning = reasoning.clone();
    reasoning.remove("effort");
    if reasoning.is_empty() {
        projected.remove("reasoning");
    } else {
        projected.insert("reasoning".to_owned(), Value::Object(reasoning));
    }
    Cow::Owned(Value::Object(projected))
}

#[cfg(test)]
mod body_with_supported_effort_tests {
    use super::{Dialect, Protocol, body_with_supported_effort};
    use emp_core::{ResolvedRoute, RouteSource};
    use serde_json::json;
    use std::borrow::Cow;

    fn route() -> ResolvedRoute {
        ResolvedRoute::new(
            "demo/model",
            "upstream-model",
            RouteSource::ExplicitModel,
            json!({
                "id":"demo-provider", "base_url":"https://example.test/v1",
                "protocol":"responses", "auth_mode":"api_key", "api_key":"fixture"
            })
            .as_object()
            .expect("provider object")
            .clone(),
            json!({
                "id":"demo/model", "supports_reasoning":true,
                "reasoning_levels":["low"]
            })
            .as_object()
            .expect("model object")
            .clone(),
            Protocol::Responses,
            Dialect::PortableResponses,
            "demo-provider",
            format!("sha256:{}", "1".repeat(64)),
            "default",
        )
        .expect("resolved route")
    }

    #[test]
    fn supported_effort_returns_borrowed_body() {
        let route = route();
        let body = json!({
            "model":"demo/model", "input":"large request body",
            "reasoning":{"effort":"low"}
        });

        assert!(matches!(
            body_with_supported_effort(&route, &body),
            Cow::Borrowed(value) if std::ptr::eq(value, &body)
        ));
    }

    #[test]
    fn unsupported_effort_returns_owned_projected_body() {
        let route = route();
        let body = json!({
            "model":"demo/model", "input":"large request body",
            "reasoning":{"effort":"high", "summary":"auto"}
        });

        let projected = body_with_supported_effort(&route, &body);
        let Cow::Owned(projected) = projected else {
            panic!("unsupported effort must own the changed body");
        };
        assert_eq!(projected["input"], body["input"]);
        assert_eq!(projected["reasoning"]["summary"], "auto");
        assert!(projected["reasoning"].get("effort").is_none());
        assert_eq!(body["reasoning"]["effort"], "high");
    }
}
