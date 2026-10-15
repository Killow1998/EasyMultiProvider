//! Exercise real dial failures through both routing paths and delivery modes.
use emp_core::{Dialect, Protocol, ResolvedRoute, RouteSource};
use emp_router::{ExternalRouter, ProjectionIds, native_http::NativeRouter};
use emp_transport::{FailureClass, HttpClient, HttpClientPolicy, ProxyPolicy, TimeoutPolicy};
use serde_json::json;
use std::collections::BTreeMap;

fn route(dialect: Dialect) -> ResolvedRoute {
    let provider = json!({
        "id":"fixture", "base_url":"http://fixture.invalid/v1",
        "auth_mode":if dialect == Dialect::CodexNative { "forward" } else { "api_key" },
        "api_key":"synthetic-key", "protocol":"responses"
    });
    ResolvedRoute::new(
        "fixture-model",
        "fixture-upstream",
        RouteSource::ExplicitModel,
        provider.as_object().unwrap().clone(),
        json!({"id":"fixture-model", "upstream_id":"fixture-upstream"})
            .as_object()
            .unwrap()
            .clone(),
        Protocol::Responses,
        dialect,
        "fixture",
        format!("sha256:{}", "1".repeat(64)),
        "default",
    )
    .unwrap()
}

#[tokio::test]
async fn refused_proxy_survives_native_and_external_http_and_sse_routing() {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = socket.local_addr().unwrap();
    let client = HttpClient::new(HttpClientPolicy::new(
        ProxyPolicy::explicit(Some(format!(
            "http://fixture-user:fixture-password@{address}"
        ))),
        TimeoutPolicy::default(),
    ))
    .unwrap();
    let body = json!({"model":"fixture-model", "input":"fixture input"});
    let mut stream_body = body.clone();
    stream_body["stream"] = true.into();
    let ids = ProjectionIds::new(
        "resp_fixture",
        "msg_fixture",
        "rs_fixture",
        "rs_late_fixture",
    );
    let native = NativeRouter::new(&client);
    let native_route = route(Dialect::CodexNative);
    let complete = native
        .execute_complete(
            &native_route,
            body.as_object().unwrap(),
            false,
            false,
            |_| Ok(BTreeMap::new()),
        )
        .await
        .err()
        .expect("native complete fails");
    let stream = native
        .open_stream(
            &native_route,
            stream_body.as_object().unwrap(),
            false,
            &ids,
            |_| Ok(BTreeMap::new()),
        )
        .await
        .err()
        .expect("native SSE fails");
    for error in [complete, stream] {
        assert_eq!(error.status, 503);
        assert_eq!(error.body["error"]["type"], "proxy_unavailable");
        assert_eq!(error.body["error"]["origin"], "transport");
        assert_eq!(error.body["error"]["failure_reason"], "proxy_unavailable");
        let public = error.body.to_string();
        for secret in [
            "fixture-password",
            "fixture-user",
            "fixture.invalid",
            &address.to_string(),
        ] {
            assert!(!public.contains(secret));
        }
    }
    let external = ExternalRouter::new(&client);
    let external_route = route(Dialect::PortableResponses);
    let complete = external
        .execute_complete(&external_route, &body, &BTreeMap::new(), &ids)
        .await
        .expect_err("external complete fails");
    let stream = external
        .open_stream(&external_route, &stream_body, &BTreeMap::new(), &ids)
        .await
        .expect_err("external SSE fails");
    for error in [complete, stream] {
        assert_eq!(error.status(), 503, "{error:?}");
        assert_eq!(error.error_class(), FailureClass::ProxyUnavailable);
        assert_eq!(error.failure_reason(), Some("proxy_unavailable"));
    }
}
