use super::account_writes::{expect_upstream_get, normalize_body};
use super::*;
use crate::tests::{fixture_tls, live_journal, wait_for};
use emp_state::filesystem::count_commits;
use emp_storage::quota_history::SAMPLE_INTERVAL_SECONDS;
use emp_test_support::test_timeout;
use emp_transport::{HttpClient, HttpClientConfig, HttpClientPolicy};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::{io::BufRead, path::Path};
use tokio::net::TcpListener as AsyncListener;

const DISCOVER: &str = "/api/providers/discover";
const DECISION: &str = "/api/decisions/models";
const REFRESH: &str = "/api/catalog/refresh";
const PREFERENCE: &str = "/api/catalog/context-preference";
const QUOTA: &str = "/api/providers/fixture%2Dcpa/quota?fixture=1";
const BALANCE: &str = "/api/providers/%64eepseek/balance?fixture=1";
const BALANCE_HOST: &str = "api.deepseek.com";
const SELECT: &[u8] = br#"{"provider":"fixture","cached":true,"selected":["gpt-6-luna"]}"#;
const DISCOVER_MODELS: &[u8] = br#"{"data":[{"id":"gpt-6-luna","name":"Fixture model"}]}"#;
const POOL_QUOTA: &[u8] = br#"{"scope":"shared_pool","provider":"claude","accounts":[{"id":"pool-account","enabled":true,"stale":false,"status":"ok","windows":{"five_hour":{"remaining_percent":73}}}]}"#;
const BALANCE_BODY: &[u8] = br#"{"balance_infos":[{"currency":"USD","total_balance":"7.50"}]}"#;

fn fixture(upstream: SocketAddr, balance: SocketAddr, root_certificate: &[u8]) -> Fixture {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = root.join("config.json");
    std::fs::write(
        &config,
        json!({
            "native_catalog_path":root.join("native.json"),
            "account_store_path":root.join("accounts"),
            "providers":[
                {"id":"fixture","base_url":format!("http://{upstream}/v1"),"protocol":"chat_completions","api_key":"fixture-key"},
                {"id":"fixture-cpa","base_url":format!("http://{upstream}/v1"),"protocol":"anthropic_messages","execution_backend":"claude_cli","api_key":"fixture-key"},
                {"id":"deepseek","base_url":format!("https://{BALANCE_HOST}/v1"),"protocol":"chat_completions","api_key":"fixture-key"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let mut transport = HttpClientConfig::default();
    transport
        .add_root_certificate_der(root_certificate)
        .unwrap();
    transport.use_only_configured_root_certificates().unwrap();
    transport.add_dns_override(BALANCE_HOST, balance).unwrap();
    let server = RunningServer::start_with_test_options(
        LOOPBACK.ip(),
        &config,
        root.join("codex/auth.json"),
        "missing-test-codex",
        Some(HttpClient::with_config(HttpClientPolicy::default(), transport).unwrap()),
    )
    .unwrap();
    Fixture {
        directory,
        server,
        upstream: Arc::default(),
    }
}

fn listener() -> TcpListener {
    let upstream = TcpListener::bind(LOOPBACK).unwrap();
    upstream.set_nonblocking(true).unwrap();
    upstream
}

fn read_file(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn published(fixture: &Fixture) -> Value {
    read_file(&crate::services::catalog::generated_catalog_path(
        &fixture.server.state,
    ))
}

fn normalize_root(reply: Vec<u8>, fixture: &Fixture) -> Vec<u8> {
    normalize_body(reply, |body| {
        body.replace(
            fixture
                .directory
                .path()
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap(),
            "/fixture",
        )
    })
}

fn normalize_time(reply: Vec<u8>, field: &str) -> Vec<u8> {
    let value = json_body(&reply);
    let replacement = match field {
        "updated_at" => {
            let stamp = value[field].as_str().expect("updated_at must be a string");
            time::OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
                .expect("updated_at must be RFC 3339");
            r#""2000-01-01T00:00:00Z""#
        }
        "observed_at" => {
            assert!(
                value["observation"][field]
                    .as_f64()
                    .is_some_and(|stamp| stamp > 0.0),
                "observed_at must be a positive number"
            );
            "0"
        }
        _ => unreachable!("unexpected catalog timestamp field"),
    };
    normalize_body(reply, |body| {
        let key = format!("\"{field}\":");
        let (head, tail) = body.split_once(&key).unwrap();
        let end = tail.find([',', '}']).unwrap();
        let start = tail.len() - tail.trim_start().len();
        let end = tail[..end].trim_end().len();
        format!("{head}{key}{}{replacement}{}", &tail[..start], &tail[end..])
    })
}

async fn send_http(mut peer: TcpStream, body: &[u8]) {
    let reply = crate::http::response::response("HTTP/1.1 200 OK", "application/json", body, &[]);
    peer.write_all(&reply).await.unwrap();
}

async fn serve_model_query(upstream: &AsyncListener, path: &str, body: &[u8]) {
    let peer = expect_upstream_get(upstream, path, "fixture-key").await;
    send_http(peer, body).await;
}

async fn wait_for_balance(
    listener: &AsyncListener,
    tls: &Arc<ServerConfig>,
) -> StreamOwned<ServerConnection, std::net::TcpStream> {
    let (peer, _) = timeout(test_timeout(Duration::from_secs(5)), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let peer = peer.into_std().unwrap();
    let tls = Arc::clone(tls);
    tokio::task::spawn_blocking(move || {
        peer.set_nonblocking(false).unwrap();
        peer.set_read_timeout(Some(test_timeout(Duration::from_secs(5))))
            .unwrap();
        peer.set_write_timeout(Some(test_timeout(Duration::from_secs(5))))
            .unwrap();
        let mut peer =
            std::io::BufReader::new(StreamOwned::new(ServerConnection::new(tls).unwrap(), peer));
        let mut line = String::new();
        peer.read_line(&mut line).unwrap();
        assert_eq!(line, "GET /user/balance HTTP/1.1\r\n");
        let mut authorized = false;
        loop {
            line.clear();
            assert_ne!(peer.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            authorized |= line.eq_ignore_ascii_case("Authorization: Bearer fixture-key\r\n");
        }
        assert!(authorized);
        peer.into_inner()
    })
    .await
    .unwrap()
}

async fn send_balance(mut peer: StreamOwned<ServerConnection, std::net::TcpStream>) {
    tokio::task::spawn_blocking(move || {
        let reply = crate::http::response::response(
            "HTTP/1.1 200 OK",
            "application/json",
            BALANCE_BODY,
            &[],
        );
        peer.write_all(&reply).unwrap();
        peer.conn.send_close_notify();
        peer.flush().unwrap();
    })
    .await
    .unwrap();
}

async fn get(fixture: &Fixture, address: SocketAddr, path: &str) -> Value {
    let wire = request(fixture.port(), "GET", path, &[&fixture.session()], b"");
    let reply = exchange_wire(address, &wire, false).await;
    assert_status(&reply, 200);
    json_body(&reply)
}

fn quota_used(observation: &Value) -> &Value {
    &observation["quota"]["rate_limits_by_limit_id"]["claude"]["primary"]["used_percent"]
}

fn assert_selected(fixture: &Fixture) {
    let config = read_file(&fixture.directory.path().join("config.json"));
    let selected: Vec<_> = config["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|model| model["id"] == "fixture/gpt-6-luna")
        .collect();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0]["enabled"], true);
    assert!(
        published(fixture)["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["slug"] == "fixture/gpt-6-luna")
    );
}

#[test]
fn catalog_writes_persist_models_and_provider_observations() {
    let upstream = listener();
    let balance = listener();
    let (tls, root) = fixture_tls(BALANCE_HOST);

    fixture(
        upstream.local_addr().unwrap(),
        balance.local_addr().unwrap(),
        &root,
    )
    .run_wire(async |fixture, endpoint| {
        let upstream = AsyncListener::from_std(upstream).unwrap();
        let balance = AsyncListener::from_std(balance).unwrap();
        for path in [DISCOVER, DECISION, REFRESH, PREFERENCE, QUOTA, BALANCE] {
            fixture
                .request_json(|port, _| json_request(port, path, &[], b"{"), 401)
                .await;
            fixture
                .request_json(
                    |port, session| {
                        json_request(
                            port,
                            path,
                            &[session, "Origin: https://other.invalid"],
                            b"{",
                        )
                    },
                    403,
                )
                .await;
            assert!(
                fixture.post(path, b"{", 400).await["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("valid JSON")
            );
            assert_eq!(
                fixture.post(path, b"[]", 400).await["error"]["message"],
                "request body must be a JSON object"
            );
            let oversized = fixture
                .request_json(
                    |port, session| {
                        request(
                            port,
                            "POST",
                            path,
                            &[
                                session,
                                "Content-Type: application/json",
                                "Content-Length: 5242881",
                            ],
                            b"",
                        )
                    },
                    413,
                )
                .await;
            assert_eq!(oversized["error"]["limit_bytes"], 5242880);
        }

        for path in [DISCOVER, DECISION] {
            for body in [
                b"{}".as_slice(),
                br#"{"provider":false}"#,
                br#"{"provider":""}"#,
            ] {
                assert_eq!(
                    fixture.post(path, body, 400).await["error"]["message"],
                    "provider is required"
                );
            }
            let absent = fixture
                .post(
                    path,
                    br#"{"provider":"absent","protocol":"openai","selected":false}"#,
                    400,
                )
                .await;
            assert_eq!(
                absent["error"]["message"],
                "provider is missing or disabled: absent"
            );
        }
        for body in [
            br#"{"provider":"absent"}"#.as_slice(),
            br#"{"provider":"absent","protocol":false}"#,
        ] {
            assert_eq!(
                fixture.post(DECISION, body, 400).await["error"]["message"],
                "decision API type is invalid"
            );
        }
        for (path, absent) in [
            (QUOTA, "/api/providers/absent/quota"),
            (BALANCE, "/api/providers/absent/balance"),
        ] {
            fixture.post(absent, b"{}", 404).await;
            for body in [br#"{"force":null}"#.as_slice(), br#"{"force":7}"#] {
                assert_eq!(
                    fixture.post(path, body, 400).await["error"]["message"],
                    "force must be boolean"
                );
                assert_eq!(
                    fixture.post(absent, body, 400).await["error"]["message"],
                    "force must be boolean"
                );
            }
        }
        fixture.post("/api/providers/quota", b"{}", 404).await;
        for body in [
            b"{}".as_slice(),
            br#"{"catalog_show_context":false,"ignored":true}"#,
        ] {
            assert_eq!(
                fixture.post(PREFERENCE, body, 400).await["error"]["message"],
                "catalog preference request must contain only catalog_show_context"
            );
        }
        for body in [
            br#"{"other":false}"#.as_slice(),
            br#"{"catalog_show_context":"false"}"#,
        ] {
            assert_eq!(
                fixture.post(PREFERENCE, body, 400).await["error"]["message"],
                "catalog_show_context must be boolean"
            );
        }

        let (decision, ()) = tokio::join!(
            fixture.post(
                DECISION,
                br#"{"provider":"fixture","protocol":"openai","ignored":true}"#,
                200
            ),
            serve_model_query(&upstream, "/v1/models", DISCOVER_MODELS),
        );
        assert_eq!(
            decision,
            json!({"models":[{"id":"gpt-6-luna","name":"Fixture model"}]})
        );

        let (invalid_selection, ()) = tokio::join!(
            fixture.post(
                DISCOVER,
                br#"{"provider":"fixture","cached":"true","selected":false}"#,
                400
            ),
            serve_model_query(&upstream, "/v1/models", DISCOVER_MODELS),
        );
        assert_eq!(
            invalid_selection["error"]["message"],
            "selected models must be a list of model IDs"
        );
        let preview = fixture
            .request_checked(
                |port, session| {
                    json_request(
                        port,
                        DISCOVER,
                        &[session],
                        br#"{"provider":"fixture","cached":true,"selected":null,"ignored":true}"#,
                    )
                },
                200,
                |reply| normalize_time(reply, "updated_at"),
            )
            .await;
        assert_eq!(preview["models"][0]["upstream_id"], "gpt-6-luna");
        let fetched = fixture
            .request_checked(
                |port, session| {
                    request(
                        port,
                        "GET",
                        "/api/providers/fixture/models",
                        &[session],
                        b"",
                    )
                },
                200,
                |reply| normalize_time(reply, "updated_at"),
            )
            .await;
        assert_eq!(fetched["models"], preview["models"]);
        assert_eq!(
            fixture
                .post(
                    DISCOVER,
                    br#"{"provider":"fixture","cached":true,"selected":["missing"]}"#,
                    400
                )
                .await["error"]["message"],
            "selected model is not in the discovered list"
        );
        let config_commits = count_commits(&fixture.server.state.backend.configuration.config_path);
        let catalog_commits = count_commits(&crate::services::catalog::generated_catalog_path(
            &fixture.server.state,
        ));
        let selected = fixture
            .request_checked(
                |port, session| json_request(port, DISCOVER, &[session], SELECT),
                200,
                |reply| normalize_root(reply, fixture),
            )
            .await;
        assert_eq!(selected["added"], 1);
        assert_eq!(config_commits.get(), 1, "selection configuration commits");
        assert_eq!(catalog_commits.get(), 1, "selection catalog commits");
        {
            assert_selected(fixture);
        }

        let refresh = fixture
            .request_checked(
                |port, session| json_request(port, REFRESH, &[session], br#"{"ignored":true}"#),
                200,
                |reply| normalize_root(reply, fixture),
            )
            .await;
        assert_eq!(refresh["status"], "ok");
        assert_eq!(
            catalog_commits.get(),
            1,
            "unchanged catalog is not rewritten"
        );

        let preference = fixture
            .post(PREFERENCE, br#"{"catalog_show_context":false}"#, 200)
            .await;
        assert_eq!(preference, json!({"catalog_show_context":false}));
        {
            assert_eq!(
                read_file(&fixture.directory.path().join("config.json"))["catalog_show_context"],
                false
            );
        }

        let (quota, ()) = tokio::join!(
            fixture.request_checked(
                |port, session| json_request(port, QUOTA, &[session], br#"{"ignored":true}"#),
                200,
                |reply| normalize_time(reply, "observed_at")
            ),
            serve_model_query(&upstream, "/v1/quota", POOL_QUOTA),
        );
        assert_eq!(quota_used(&quota["observation"]), 27.0);
        let (amounts, ()) = tokio::join!(
            fixture.request_checked(
                |port, session| json_request(
                    port,
                    BALANCE,
                    &[session],
                    br#"{"force":true,"ignored":true}"#
                ),
                200,
                |reply| normalize_time(reply, "observed_at")
            ),
            async {
                {
                    send_balance(wait_for_balance(&balance, &tls).await).await;
                }
            },
        );
        assert_eq!(
            amounts["observation"]["balance"]["amounts"],
            json!([{"currency":"USD","available":7.5}])
        );

        {
            let (fixture, address) = (fixture, endpoint);
            let mut config = get(fixture, address, "/api/config").await;
            assert_eq!(config["catalog_show_context"], false);
            for (service, id, expected) in [
                ("provider_quotas", "fixture-cpa", &quota),
                ("provider_balances", "deepseek", &amounts),
            ] {
                let entry = &mut config[service][id];
                assert_eq!(entry["identity"], expected["identity"]);
                assert!(entry["observation"]["observed_at"].as_f64().unwrap() > 0.0);
                entry["observation"]["observed_at"] = json!(0);
                assert_eq!(entry["observation"], expected["observation"]);
            }
            assert!(
                fixture
                    .server
                    .state
                    .lifecycle
                    .updates()
                    .drain_for_test(Duration::ZERO)
            );
        }

        for path in [DISCOVER, DECISION, REFRESH, PREFERENCE, QUOTA, BALANCE] {
            for session_required in [false, true] {
                let denied = fixture
                    .request_json(
                        |port, session| {
                            let headers = [session];
                            json_request(
                                port,
                                path,
                                if session_required { &headers } else { &[] },
                                b"{",
                            )
                        },
                        503,
                    )
                    .await;
                assert_eq!(denied["error"]["code"], "updating");
            }
        }
        assert_eq!(
            fixture.read("/v1/models/fixture/gpt-6-luna").await["id"],
            "fixture/gpt-6-luna"
        );
        {
            fixture
                .server
                .state
                .backend
                .modules
                .mark_unavailable(emp_modules::Module::Decision, "fixture_disabled");
        }
        let disabled = fixture
            .request_json(|port, _| json_request(port, DECISION, &[], b"{"), 404)
            .await;
        assert_eq!(disabled["error"]["code"], "module_disabled");
        assert!(timeout(Duration::ZERO, upstream.accept()).await.is_err());
        assert!(timeout(Duration::ZERO, balance.accept()).await.is_err());
    });
}

async fn submit(
    fixture: &Fixture,
    path: &str,
    body: &[u8],
) -> (TcpStream, tokio::task::JoinHandle<()>) {
    let (mut client, task) = connect_submitted_write(fixture).await;
    let wire = json_request(fixture.port(), path, &[&fixture.session()], body);
    client.write_all(&wire).await.unwrap();
    (client, task)
}

async fn wait_for_write(fixture: &Fixture) {
    let state = Arc::clone(&fixture.server.state);
    tokio::task::spawn_blocking(move || {
        assert!(
            state
                .lifecycle
                .admission
                .quiesce(0, Duration::from_secs(5))
                .is_some()
        );
    })
    .await
    .unwrap();
}

async fn refresh_after_disconnect(fixture: &Fixture) {
    let state = &fixture.server.state;
    let catalog = crate::services::catalog::generated_catalog_path(state);
    let commits = count_commits(&catalog);
    std::fs::remove_file(&catalog).unwrap();
    let body = b"{}";
    let wire = json_request(fixture.port(), REFRESH, &[&fixture.session()], body);
    let (mut client, task) = connect_submitted_write(fixture).await;
    client
        .write_all(&wire[..wire.len() - body.len()])
        .await
        .unwrap();
    let admitted = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        wait_for(test_timeout(Duration::from_secs(5)), || {
            admitted.lifecycle.updates().active_for_test() == 1
        });
    })
    .await
    .unwrap();

    let (locked, ready) = tokio::sync::oneshot::channel();
    let (release, unlocked) = tokio::sync::oneshot::channel();
    let gated = Arc::clone(state);
    let hold_config = tokio::task::spawn_blocking(move || {
        let _lock = gated.backend.configuration.test_config().lock().unwrap();
        locked.send(()).unwrap();
        let _ = unlocked.blocking_recv();
    });
    ready.await.unwrap();
    client.write_all(body).await.unwrap();
    let root = fixture.directory.path().to_owned();
    tokio::task::spawn_blocking(move || {
        wait_for(test_timeout(Duration::from_secs(5)), || {
            live_journal(&root).iter().any(|record| {
                record["event"] == "operation_stage_started"
                    && record["fields"]["operation"] == "catalog_refresh"
                    && record["fields"]["last_stage"] == "publish_catalog"
            })
        });
    })
    .await
    .unwrap();
    assert!(!state.lifecycle.updates().drain_for_test(Duration::ZERO));
    disconnect_submitted_write(fixture, client, task).await;
    assert!(!catalog.exists());
    release.send(()).unwrap();
    hold_config.await.unwrap();
    wait_for_write(fixture).await;
    assert_selected(fixture);
    assert_eq!(commits.get(), 1, "catalog refresh commits");

    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut pending = {
        let state = Arc::clone(state);
        let stopped = Arc::clone(&stopped);
        tokio::task::spawn_blocking(move || {
            state
                .catalog_refresh
                .wait_for_work(|| stopped.load(Ordering::Acquire))
        })
    };
    let found = timeout(test_timeout(Duration::from_secs(5)), &mut pending).await;
    if found.is_err() {
        stopped.store(true, Ordering::Release);
        state.catalog_refresh.stop();
        pending.await.unwrap();
    }
    assert_eq!(found.unwrap().unwrap(), Some(false));
    state.catalog_refresh.finish_pass();
}

#[test]
fn submitted_catalog_writes_finish_once_after_disconnect() {
    let upstream = listener();
    let balance = listener();
    let (tls, root) = fixture_tls(BALANCE_HOST);

    let new = fixture(
        upstream.local_addr().unwrap(),
        balance.local_addr().unwrap(),
        &root,
    );
    new.run_wire(async |fixture, endpoint| {
        let upstream = AsyncListener::from_std(upstream).unwrap();
        let balance = AsyncListener::from_std(balance).unwrap();
        { let (fixture, address) = (fixture, endpoint);
            let state = &fixture.server.state;
            let config_commits = count_commits(&state.backend.configuration.config_path);
            let catalog_commits = count_commits(&crate::services::catalog::generated_catalog_path(state));
            let (client, task) = submit(fixture, DISCOVER, br#"{"provider":"fixture","selected":["gpt-6-luna"]}"#).await;
            let peer = expect_upstream_get(&upstream, "/v1/models", "fixture-key").await;
            assert!(!state.lifecycle.updates().drain_for_test(Duration::ZERO));
            disconnect_submitted_write(fixture, client, task).await;
            send_http(peer, DISCOVER_MODELS).await;
            wait_for_write(fixture).await;
            assert_selected(fixture);
            assert_eq!(get(fixture, address, "/api/providers/fixture/models").await["models"][0]["upstream_id"], "gpt-6-luna");
            assert_eq!(config_commits.get(), 1, "selection configuration commits");
            assert_eq!(catalog_commits.get(), 1, "selection catalog commits");
            drop(catalog_commits);
            refresh_after_disconnect(fixture).await;

            let start = system_now() as i64 - SAMPLE_INTERVAL_SECONDS;
            let (client, task) = submit(fixture, QUOTA, br#"{"force":true}"#).await;
            let peer = expect_upstream_get(&upstream, "/v1/quota", "fixture-key").await;
            assert!(!state.lifecycle.updates().drain_for_test(Duration::ZERO));
            disconnect_submitted_write(fixture, client, task).await;
            send_http(peer, POOL_QUOTA).await;
            wait_for_write(fixture).await;

            let (client, task) = submit(fixture, BALANCE, b"{}").await;
            let peer = wait_for_balance(&balance, &tls).await;
            assert!(!state.lifecycle.updates().drain_for_test(Duration::ZERO));
            disconnect_submitted_write(fixture, client, task).await;
            send_balance(peer).await;
            wait_for_write(fixture).await;

            let config = get(fixture, address, "/api/config").await;
            let quota = &config["provider_quotas"]["fixture-cpa"]["observation"];
            assert_eq!(quota_used(quota), 27.0);
            let observed_at = quota["observed_at"].as_f64().unwrap() as i64;
            let bucket = observed_at - observed_at.rem_euclid(SAMPLE_INTERVAL_SECONDS);
            assert_eq!(config["provider_balances"]["deepseek"]["observation"]["balance"]["amounts"],
                json!([{"currency":"USD","available":7.5}]));
            let end = system_now() as i64 + 1;
            let history = get(fixture, address, &format!("/api/providers/fixture-cpa/quota-history?start={start}&end={end}")).await;
            assert_eq!(history["scope"], "shared_pool");
            let mut series = history["series"].as_array().unwrap().clone();
            assert_eq!(series.len(), 2);
            for row in &mut series {
                let points = row["points"].as_array_mut().unwrap();
                assert_eq!(points.len(), 1);
                assert_eq!(points[0]["remaining_percent"], 73.0);
                assert_eq!(points[0]["observed_at"], bucket);
            }

            for path in [QUOTA, BALANCE] {
                let wire = json_request(fixture.port(), path, &[&fixture.session()], b"{}");
                assert_status(&exchange_wire(address, &wire, false).await, 200);
            }
            assert!(timeout(Duration::ZERO, upstream.accept()).await.is_err());
            assert!(timeout(Duration::ZERO, balance.accept()).await.is_err());
        }

        {
            assert_writes_completed_once(&live_journal(fixture.directory.path()));
        }
    });
}

fn assert_writes_completed_once(records: &[Value]) {
    for (operation, stage) in [
        ("model_discovery", "commit_selection"),
        ("catalog_refresh", "publish_catalog"),
    ] {
        let finished: Vec<_> = records
            .iter()
            .filter(|record| {
                record["event"] == "operation_finished"
                    && record["fields"]["operation"] == operation
            })
            .collect();
        assert_eq!(finished.len(), 1);
        let fields = &finished[0]["fields"];
        assert_eq!(fields["outcome"], "completed");
        assert_eq!(
            fields["stages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|step| step["stage"] == stage)
                .count(),
            1
        );
        if operation == "catalog_refresh" {
            assert_eq!(fields["facts"]["subscription_refresh"], "scheduled");
        }
    }
    for event in ["cpa_quota_refresh", "provider_balance_refresh"] {
        let finished: Vec<_> = records
            .iter()
            .filter(|record| record["event"] == event)
            .collect();
        assert_eq!(finished.len(), 1);
        assert!(finished[0]["fields"]["error_code"].is_null());
        if event == "cpa_quota_refresh" {
            assert_eq!(finished[0]["fields"]["history_saved"], true);
        }
    }
}
