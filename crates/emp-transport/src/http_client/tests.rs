use super::*;
use crate::http_policy::{HttpClientPolicy, ProxyEnvironment, ProxyPolicy, TimeoutPolicy};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn resolver_failure_is_classified_without_a_real_dns_query() {
    struct FailedDns;
    impl reqwest::dns::Resolve for FailedDns {
        fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
            Box::pin(async { Err(std::io::Error::other("private resolver details").into()) })
        }
    }
    let client = Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(FailedDns))
        .build()
        .unwrap();
    let error = client
        .get("http://fixture.invalid/private?key=secret")
        .send()
        .await
        .unwrap_err();
    let error = map_send_error(error, false);
    assert_eq!(error.kind(), HttpTransportErrorKind::DnsFailure);
    assert!(!format!("{error:?} {error}").contains("private"));
}

#[test]
fn redirect_origin_rejects_host_port_and_scheme_changes() {
    let origin = RedirectOrigin {
        scheme: "https".to_owned(),
        host: "api.example.test".to_owned(),
        port: 443,
    };
    assert!(origin.matches(&Url::parse("https://api.example.test/next").unwrap()));
    assert!(!origin.matches(&Url::parse("https://other.example.test/next").unwrap()));
    assert!(!origin.matches(&Url::parse("https://api.example.test:444/next").unwrap()));
    assert!(!origin.matches(&Url::parse("http://api.example.test/next").unwrap()));

    let ipv6 = RedirectOrigin {
        scheme: "http".to_owned(),
        host: "::1".to_owned(),
        port: 8080,
    };
    assert!(ipv6.matches(&Url::parse("http://[::1]:8080/next").unwrap()));
    assert!(!ipv6.matches(&Url::parse("http://[::2]:8080/next").unwrap()));
}

#[test]
fn new_requests_and_websockets_resolve_proxy_again_and_pool_is_bounded() {
    let settings = Arc::new(Mutex::new(ProxyEnvironment::default()));
    let resolver_settings = Arc::clone(&settings);
    let policy = HttpClientPolicy::new(
        ProxyPolicy::dynamic_with_resolver(move || resolver_settings.lock().unwrap().clone()),
        TimeoutPolicy::default(),
    );
    let client = HttpClient::new(policy).expect("valid dynamic client");

    let mut plan_tokens = Vec::new();
    for index in 0..6 {
        let proxy = format!("http://proxy-{index}.example:8080");
        *settings.lock().unwrap() = ProxyEnvironment {
            https: Some(proxy.clone()),
            wss: Some(proxy.clone()),
            ..ProxyEnvironment::default()
        };
        let plan = client
            .policy
            .plan(
                HttpMethod::Get,
                "https://upstream.example/v1",
                BTreeMap::new(),
                false,
            )
            .expect("dynamic request plan");
        plan_tokens.push(plan.route.proxy_origin.pool_token().to_owned());

        if index == 0 {
            *settings.lock().unwrap() = ProxyEnvironment {
                https: Some("http://changed.example:8080".to_owned()),
                ..ProxyEnvironment::default()
            };
            assert_eq!(
                client
                    .policy
                    .transport_proxy_for(&plan.route)
                    .unwrap()
                    .as_deref(),
                Some("http://proxy-0.example:8080"),
                "an existing plan retains its selected proxy"
            );
            client
                .client_for(&plan, RedirectMode::Reject)
                .expect("client uses the plan's selected proxy");
            continue;
        }
        client
            .client_for(&plan, RedirectMode::Reject)
            .expect("client uses current proxy");
    }

    let websocket_proxy = client
        .websocket_proxy_for("wss://upstream.example/socket")
        .expect("first websocket proxy");
    assert_eq!(
        websocket_proxy.as_deref(),
        Some("http://proxy-5.example:8080")
    );
    *settings.lock().unwrap() = ProxyEnvironment {
        wss: Some("http://websocket-updated.example:8080".to_owned()),
        ..ProxyEnvironment::default()
    };
    assert_eq!(
        client
            .websocket_proxy_for("wss://upstream.example/socket")
            .expect("updated websocket proxy")
            .as_deref(),
        Some("http://websocket-updated.example:8080")
    );

    let clients = client
        .clients
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(clients.len(), 4, "the proxy pool cache remains bounded");
    assert!(clients.iter().all(|entry| {
        plan_tokens
            .iter()
            .skip(2)
            .any(|token| entry.identity.starts_with(token))
    }));
}
