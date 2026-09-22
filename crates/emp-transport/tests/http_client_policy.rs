use emp_transport::{
    ConnectionPoolPolicy, HttpClientPolicy, HttpClientPolicyError, HttpMethod, IdleConnectionPool,
    ProxyEnvironment, ProxyPolicy, StreamingReadState, TimeoutPolicy,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("authorization".to_string(), "Bearer fixture".to_string()),
        ("accept".to_string(), "text/event-stream".to_string()),
    ])
}

#[test]
fn plan_uses_same_origin_and_default_identity_encoding() {
    let policy = HttpClientPolicy::default();
    let plan = policy
        .plan(
            HttpMethod::Post,
            "https://upstream.example:8443/v1/responses?tenant=fixture#ignored",
            headers(),
            true,
        )
        .expect("valid upstream plan");
    assert_eq!(plan.method.as_str(), "POST");
    assert_eq!(plan.route.scheme, "https");
    assert_eq!(plan.route.host, "upstream.example");
    assert_eq!(plan.route.port, Some(8443));
    assert_eq!(plan.route.path, "/v1/responses");
    assert_eq!(plan.route.query.as_deref(), Some("tenant=fixture"));
    assert!(!plan.route.route_url().contains("#ignored"));
    assert_eq!(
        plan.headers.get("accept-encoding"),
        Some(&"identity".to_owned())
    );
    assert_eq!(
        plan.redirect_policy,
        emp_transport::RedirectPolicy::Disabled
    );
    assert_eq!(plan.retry_policy, emp_transport::RetryPolicy::Disabled);
    assert_eq!(plan.timeout_policy.stream_idle, Duration::from_secs(300));

    let custom_encoding = policy
        .plan(
            HttpMethod::Get,
            "https://upstream.example/v1",
            BTreeMap::from([("Accept-Encoding".to_owned(), "gzip".to_owned())]),
            false,
        )
        .unwrap();
    assert_eq!(custom_encoding.headers.len(), 1);
    assert_eq!(custom_encoding.headers["accept-encoding"], "gzip");
}

#[test]
fn credentials_are_never_reused_or_exposed() {
    let policy = HttpClientPolicy::default();
    let error = policy
        .plan(
            HttpMethod::Post,
            "https://user:password@upstream.example/v1/responses",
            headers(),
            true,
        )
        .expect_err("origin credentials are invalid");
    assert_eq!(error, HttpClientPolicyError::InvalidUrl);

    let proxy = ProxyPolicy::explicit(Some(
        "http://proxy-user:proxy-password@127.0.0.1:8080".to_owned(),
    ));
    let proxy_policy = HttpClientPolicy::new(proxy.clone(), TimeoutPolicy::default());
    let plan = proxy_policy
        .plan(
            HttpMethod::Post,
            "https://upstream.example/v1/responses",
            headers(),
            true,
        )
        .expect("origin plan");
    assert_eq!(plan.route.proxy_origin.pool_token().len(), 64);
    let debug = format!("{proxy:?}{plan:?}");
    assert!(!debug.contains("proxy-password"));
    assert!(!debug.contains("proxy-user"));
    assert!(!debug.contains("Bearer fixture"));
}

#[test]
fn loopback_never_uses_proxy_and_environment_order_matches_python() {
    let environment = ProxyEnvironment {
        https: Some("http://192.0.2.10:8118".to_owned()),
        ..ProxyEnvironment::default()
    };
    let policy = HttpClientPolicy::new(
        ProxyPolicy::from_environment(environment),
        TimeoutPolicy::default(),
    );
    let loopback = policy
        .plan(
            HttpMethod::Post,
            "http://127.0.0.1:8080/v1",
            headers(),
            false,
        )
        .expect("loopback plan");
    assert!(!loopback.route.proxy_origin.is_proxy());

    let external = policy
        .plan(
            HttpMethod::Post,
            "https://upstream.example/v1",
            headers(),
            false,
        )
        .expect("external plan");
    assert!(external.route.proxy_origin.is_proxy());

    let ws = ProxyEnvironment {
        wss: Some("http://192.0.2.20:3128".to_owned()),
        socks: Some("socks5h://192.0.2.30:1080".to_owned()),
        ..ProxyEnvironment::default()
    };
    let ws_policy =
        HttpClientPolicy::new(ProxyPolicy::from_environment(ws), TimeoutPolicy::default());
    let wss_plan = ws_policy
        .plan(
            HttpMethod::Post,
            "https://upstream.example/v1",
            headers(),
            false,
        )
        .expect("wss proxy order");
    assert!(wss_plan.route.proxy_origin.is_proxy());
}

#[test]
fn timeout_boundaries_are_configurable_and_fail_closed() {
    let policy = TimeoutPolicy {
        first_event: Duration::from_secs(120),
        ..TimeoutPolicy::default()
    };
    let mut state = StreamingReadState::new(policy, Instant::now()).expect("valid policy");
    assert!(!state.output_emitted());
    state.observe_output();
    assert!(state.output_emitted());
    assert_eq!(
        state.read_timeout_at(Instant::now()).unwrap(),
        Duration::from_secs(300)
    );

    let invalid = TimeoutPolicy {
        cleanup: Duration::ZERO,
        ..TimeoutPolicy::default()
    };
    let error = HttpClientPolicy::new(ProxyPolicy::default(), invalid)
        .plan(
            HttpMethod::Get,
            "https://upstream.example",
            headers(),
            false,
        )
        .expect_err("zero timeout");
    assert_eq!(error, HttpClientPolicyError::InvalidTimeout);

    let streaming = StreamingReadState::new(TimeoutPolicy::default(), Instant::now()).unwrap();
    assert_eq!(
        streaming.read_timeout_at(Instant::now()).unwrap(),
        Duration::from_secs(300)
    );
}

#[test]
fn pool_key_and_idle_pool_are_route_scoped() {
    let policy = HttpClientPolicy::default();
    let a = policy
        .plan(
            HttpMethod::Post,
            "https://a.example/v1/responses",
            headers(),
            true,
        )
        .expect("route a")
        .route
        .connection_pool_key();
    let a_direct = policy
        .plan(
            HttpMethod::Post,
            "https://a.example/v1/responses?x=1",
            headers(),
            true,
        )
        .expect("route a direct")
        .route
        .connection_pool_key();
    assert_eq!(a, a_direct, "paths and queries reuse the same origin pool");

    let b = policy
        .plan(
            HttpMethod::Post,
            "https://b.example/v1/responses",
            headers(),
            true,
        )
        .expect("route b")
        .route
        .connection_pool_key();
    assert_ne!(a, b);

    let first_proxy = HttpClientPolicy::new(
        ProxyPolicy::explicit(Some("http://one:pass@proxy.example:8080".to_owned())),
        TimeoutPolicy::default(),
    )
    .plan(HttpMethod::Post, "https://a.example/v1", headers(), true)
    .unwrap()
    .route
    .connection_pool_key();
    let second_proxy = HttpClientPolicy::new(
        ProxyPolicy::explicit(Some("http://two:pass@proxy.example:8080".to_owned())),
        TimeoutPolicy::default(),
    )
    .plan(HttpMethod::Post, "https://a.example/v1", headers(), true)
    .unwrap()
    .route
    .connection_pool_key();
    assert_ne!(first_proxy, second_proxy);

    let mut pool = IdleConnectionPool::new(ConnectionPoolPolicy {
        max_idle_per_route: 1,
        max_idle_total: 2,
        idle_timeout: Duration::from_secs(60),
    });
    pool.check_in("one", Instant::now()).unwrap();
    pool.check_in("two", Instant::now()).unwrap();
    pool.check_in("one", Instant::now()).unwrap();
    assert_eq!(pool.total_idle(), 2);
    assert_eq!(pool.route_idle("one"), 1);
    assert!(pool.check_out("two", Instant::now()).is_some());

    let now = Instant::now();
    pool.check_in("future", now + Duration::from_secs(1))
        .unwrap();
    assert!(pool.check_out("future", now).is_none());
    assert_eq!(pool.route_idle("future"), 0);

    let mut invalid = IdleConnectionPool::new(ConnectionPoolPolicy {
        max_idle_per_route: 0,
        max_idle_total: 0,
        idle_timeout: Duration::ZERO,
    });
    assert_eq!(
        invalid.check_in("route", now).unwrap_err(),
        HttpClientPolicyError::InvalidPoolKey
    );
}
