use super::{
    account_writes::{Pair, normalize_body},
    *,
};
use emp_modules::Module;
use std::{fs, path::PathBuf};

const MODULES: &str = "/api/modules";
const CLIENTS: &str = "/api/gateway/clients";
const CLIENT_FILE: &str = "state/gateway-clients.json";

#[derive(Clone, Copy)]
struct Write {
    method: &'static str,
    path: &'static str,
}

const MODULE: Write = Write {
    method: "POST",
    path: MODULES,
};
const ISSUE: Write = Write {
    method: "POST",
    path: CLIENTS,
};
const REVOKE: Write = Write {
    method: "DELETE",
    path: CLIENTS,
};

impl Write {
    fn wire(self, port: u16, session: Option<&str>, body: &[u8]) -> Vec<u8> {
        let length = format!("Content-Length: {}", body.len());
        let mut headers = vec!["Content-Type: application/json", &length];
        headers.extend(session);
        request(port, self.method, self.path, &headers, body)
    }

    async fn parity(self, pair: &Pair<'_>, body: &[u8], status: u16) -> Value {
        pair.parity(
            |port, session| self.wire(port, Some(session), body),
            status,
            normalize_clients,
        )
        .await
    }
}

fn normalize_clients(reply: Vec<u8>) -> Vec<u8> {
    let value = json_body(&reply);
    let clients = if let Some(clients) = value.get("clients").and_then(Value::as_array) {
        clients.as_slice()
    } else if value.get("api_key").is_some() {
        std::slice::from_ref(&value)
    } else {
        return reply;
    };
    normalize_body(reply, |body| {
        let mut body = body.to_owned();
        for client in clients {
            let id = client["id"].as_str().expect("device ID");
            assert!(id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()));
            let key = client["api_key"].as_str().expect("device key");
            assert!(key.strip_prefix("emp-client-").is_some_and(|hex| {
                hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
            }));
            body = body.replace(&format!(r#""id":"{id}""#), r#""id":"fixture-id""#);
            body = body.replace(
                &format!(r#""api_key":"{key}""#),
                r#""api_key":"fixture-key""#,
            );
            if let Some(created_at) = client.get("created_at") {
                assert!(created_at.as_f64().is_some_and(|time| time > 0.0));
                for delimiter in [',', '}'] {
                    body = body.replace(
                        &format!(r#""created_at":{created_at}{delimiter}"#),
                        &format!(r#""created_at":1.0{delimiter}"#),
                    );
                }
            }
        }
        body
    })
}

fn fixture_with_decision() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    install_panel_fixture(directory.path(), Module::Decision, "fixture panel");
    Fixture::with_directory(directory)
}

fn decision(value: &Value) -> &Value {
    value["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|module| module["id"] == "decision")
        .unwrap()
}

fn client_file(fixture: &Fixture) -> PathBuf {
    fixture.directory.path().join(CLIENT_FILE)
}

fn persisted_clients(fixture: &Fixture) -> Value {
    let path = client_file(fixture);
    assert!(
        fs::read(&path)
            .unwrap()
            .starts_with(emp_state::vault::VAULT_MAGIC)
    );
    fixture
        .legacy
        .state
        .backend
        .configuration
        .vault
        .read_encrypted_json(&path)
        .unwrap()
}

fn comparison_endpoints<'a>(pair: &Pair<'a>) -> [(&'a Fixture, SocketAddr); 2] {
    [
        (pair.old, pair.old.legacy.local_addr()),
        (pair.new, pair.candidate),
    ]
}

async fn compare_client_wire(
    pair: &Pair<'_>,
    make: impl Fn(usize, u16, &str) -> Vec<u8>,
    status: u16,
) -> [Value; 2] {
    let [(old, old_addr), (new, new_addr)] = comparison_endpoints(pair);
    let old_wire = make(0, old.port(), &old.session());
    let new_wire = make(1, new.port(), &new.session());
    let (old_reply, new_reply) = tokio::join!(
        exchange_wire(old_addr, &old_wire, false),
        exchange_wire(new_addr, &new_wire, false),
    );
    let (old_normalized, new_normalized) = (
        normalize_clients(old_reply.clone()),
        normalize_clients(new_reply.clone()),
    );
    assert_wire_response_parity(&old_wire, &old_normalized, &new_normalized);
    assert_status(&old_normalized, status);
    [json_body(&old_reply), json_body(&new_reply)]
}

async fn compare_client_lists(pair: &Pair<'_>) -> [Value; 2] {
    compare_client_wire(
        pair,
        |_, port, session| request(port, "GET", CLIENTS, &[session], b""),
        200,
    )
    .await
}

#[test]
fn module_and_gateway_writes_match_legacy_wire_and_drain_order() {
    let old = fixture_with_decision();
    fixture_with_decision().run_candidate(async |new, candidate| {
        let pair = Pair {
            old: &old,
            new,
            candidate,
        };
        for route in [MODULE, ISSUE, REVOKE] {
            pair.parity(
                |port, _| route.wire(port, None, b"{"),
                401,
                normalize_clients,
            )
            .await;
            pair.parity(
                |port, session| {
                    request(
                        port,
                        route.method,
                        route.path,
                        &[
                            session,
                            "Origin: https://other.invalid",
                            "Content-Length: -1",
                        ],
                        b"",
                    )
                },
                401,
                normalize_clients,
            )
            .await;
            route.parity(&pair, b"{", 400).await;
            assert_eq!(
                route.parity(&pair, b"[]", 400).await["error"]["message"],
                "request body must be a JSON object",
            );
            let limit = pair
                .parity(
                    |port, session| {
                        request(
                            port,
                            route.method,
                            route.path,
                            &[
                                session,
                                "Content-Length: 5242881",
                                "Content-Type: application/json",
                            ],
                            b"",
                        )
                    },
                    413,
                    normalize_clients,
                )
                .await;
            assert_eq!(limit["error"]["limit_bytes"], 5 * 1024 * 1024);
        }

        for body in [
            b"{}".as_slice(),
            br#"{"action":"not-an-action","id":"unknown"}"#,
            br#"{"action":"disable","id":false}"#,
            br#"{"action":"disable","id":"unknown"}"#,
        ] {
            assert_eq!(
                MODULE.parity(&pair, body, 400).await["error"]["message"],
                "Unknown EMP module"
            );
        }
        for body in [
            br#"{"id":"decision"}"#.as_slice(),
            br#"{"id":"decision","action":"not-an-action"}"#,
            br#"{"id":"decision","action":false}"#,
        ] {
            assert_eq!(
                MODULE.parity(&pair, body, 409).await["error"]["code"],
                "invalid_module_action",
            );
        }
        assert_eq!(
            MODULE
                .parity(&pair, br#"{"action":"update","id":"team"}"#, 409)
                .await["error"]["code"],
            "module_not_installed",
        );
        assert_eq!(
            MODULE
                .parity(&pair, br#"{"action":"uninstall","id":"decision"}"#, 409)
                .await["error"]["code"],
            "module_disable_before_uninstall",
        );
        let initial = pair.read(MODULES).await;
        assert_eq!(decision(&initial)["active"], true);
        let disable = br#"{"action":"disable","id":"decision","revision":10,"ignored":true}"#;
        let disabled = MODULE.parity(&pair, disable, 200).await;
        assert_eq!(decision(&disabled)["active"], false);
        assert_eq!(decision(&disabled)["enabled"], false);
        let enabled = MODULE
            .parity(
                &pair,
                br#"{"action":"enable","id":"decision","revision":"ignored"}"#,
                200,
            )
            .await;
        assert_eq!(enabled, initial);
        MODULE.parity(&pair, disable, 200).await;
        let removed = MODULE
            .parity(&pair, br#"{"action":"uninstall","id":"decision"}"#, 200)
            .await;
        assert!(decision(&removed)["installed_version"].is_null());
        assert_eq!(pair.read(MODULES).await, removed);
        for (fixture, _) in comparison_endpoints(&pair) {
            let registry = fixture.directory.path().join("state/modules/modules.json");
            assert_eq!(
                serde_json::from_slice::<Value>(&fs::read(registry).unwrap()).unwrap(),
                json!({})
            );
        }

        assert_eq!(
            MODULE
                .parity(
                    &pair,
                    br#"{"action":"check_updates","id":false,"revision":[],"ignored":true}"#,
                    200,
                )
                .await,
            json!({"updates":[]}),
        );
        for body in [b"{}".as_slice(), br#"{"name":false}"#, br#"{"name":"   "}"#] {
            assert_eq!(
                ISSUE.parity(&pair, body, 400).await["error"]["message"],
                "Caller name must contain 1–64 characters",
            );
        }
        for (route, body) in [
            (ISSUE, br#"{"name":"valid","id":null}"#.as_slice()),
            (ISSUE, br#"{"name":"valid","id":4}"#),
            (ISSUE, br#"{"id":""}"#),
            (REVOKE, b"{}"),
            (REVOKE, br#"{"id":null}"#),
            (REVOKE, br#"{"id":[]}"#),
            (REVOKE, br#"{"id":""}"#),
        ] {
            assert_eq!(
                route.parity(&pair, body, 400).await["error"]["message"],
                "A device key ID is required",
            );
        }
        assert_eq!(
            ISSUE.parity(&pair, br#"{"id":"missing"}"#, 400).await["error"]["message"],
            "Device key was not found",
        );
        assert_eq!(
            REVOKE
                .parity(&pair, br#"{"id":"missing","ignored":true}"#, 200)
                .await,
            json!({"ok":true})
        );
        for value in compare_client_lists(&pair).await {
            assert!(value["clients"] == json!([]));
        }

        let issued = compare_client_wire(
            &pair,
            |_, port, session| {
                ISSUE.wire(
                    port,
                    Some(session),
                    br#"{"name":" Device ","ignored":true}"#,
                )
            },
            200,
        )
        .await;
        let clients = compare_client_lists(&pair).await;
        for (issued, list) in issued.iter().zip(&clients) {
            let client = &list["clients"][0];
            assert_eq!(issued["name"], "Device");
            assert!(client["id"] == issued["id"] && client["api_key"] == issued["api_key"]);
        }
        let replaced = compare_client_wire(
            &pair,
            |index, port, session| {
                let body = json!({"id":issued[index]["id"],"name":false,"ignored":true});
                ISSUE.wire(port, Some(session), body.to_string().as_bytes())
            },
            200,
        )
        .await;
        let listed = compare_client_lists(&pair).await;
        for (index, (fixture, _)) in comparison_endpoints(&pair).into_iter().enumerate() {
            let client = &listed[index]["clients"][0];
            assert_eq!(listed[index]["clients"].as_array().unwrap().len(), 1);
            assert!(replaced[index]["id"] == issued[index]["id"]);
            assert!(replaced[index]["api_key"] != issued[index]["api_key"]);
            assert_eq!(replaced[index]["name"], "Device");
            assert!(
                client["id"] == replaced[index]["id"]
                    && client["api_key"] == replaced[index]["api_key"]
            );
            assert_eq!(
                client["created_at"],
                clients[index]["clients"][0]["created_at"]
            );
            assert!(persisted_clients(fixture)[0]["api_key"] == client["api_key"]);
        }
        let revoked = compare_client_wire(
            &pair,
            |index, port, session| {
                let body = json!({"id":replaced[index]["id"],"ignored":true});
                REVOKE.wire(port, Some(session), body.to_string().as_bytes())
            },
            200,
        )
        .await;
        for reply in revoked {
            assert_eq!(reply, json!({"ok":true}));
        }
        for ((fixture, _), reply) in comparison_endpoints(&pair)
            .into_iter()
            .zip(compare_client_lists(&pair).await)
        {
            assert!(reply["clients"] == json!([]));
            assert!(persisted_clients(fixture) == json!([]));
            assert!(
                fixture
                    .legacy
                    .state
                    .lifecycle
                    .updates()
                    .drain_for_test(Duration::ZERO)
            );
        }

        for (route, body) in [
            (MODULE, br#"{"action":"check_updates"}"#.as_slice()),
            (ISSUE, br#"{"name":"Denied"}"#),
            (REVOKE, br#"{"id":"missing"}"#),
        ] {
            assert_eq!(
                route.parity(&pair, body, 503).await["error"]["code"],
                "updating"
            );
            let denied = pair
                .parity(
                    |port, _| route.wire(port, None, b"{"),
                    503,
                    normalize_clients,
                )
                .await;
            assert_eq!(denied["error"]["code"], "updating");
        }
        assert_eq!(pair.read(MODULES).await, removed);
        for ((fixture, _), reply) in comparison_endpoints(&pair)
            .into_iter()
            .zip(compare_client_lists(&pair).await)
        {
            assert!(reply["clients"] == json!([]));
            assert!(persisted_clients(fixture) == json!([]));
        }

        for (fixture, _) in comparison_endpoints(&pair) {
            fixture
                .legacy
                .state
                .backend
                .modules
                .mark_unavailable(Module::Gateway, "fixture_disabled");
        }
        for route in [ISSUE, REVOKE] {
            let rejection = pair
                .parity(
                    |port, _| route.wire(port, None, b"{"),
                    404,
                    normalize_clients,
                )
                .await;
            assert_eq!(rejection["error"]["code"], "module_disabled");
        }
    });
    old.legacy.shutdown().unwrap();
}

#[test]
fn submitted_gateway_issue_saves_once_after_disconnect_and_holds_drain() {
    let old = Fixture::new();
    Fixture::new().run_candidate(async |new, candidate| {
        let pair = Pair {
            old: &old,
            new,
            candidate,
        };
        for (fixture, address) in comparison_endpoints(&pair) {
            let state = &fixture.legacy.state;
            let (ready, release) = state.backend.gateway_clients.gate_next_save_for_test();
            let (mut client, task) = connect_submitted_write(fixture, address).await;
            let wire = ISSUE.wire(
                fixture.port(),
                Some(&fixture.session()),
                br#"{"name":"Disconnected device"}"#,
            );
            client.write_all(&wire).await.unwrap();
            timeout(Duration::from_secs(5), ready)
                .await
                .expect("write reached the vault gate")
                .unwrap();
            assert!(!client_file(fixture).exists());
            assert!(!state.lifecycle.updates().drain_for_test(Duration::ZERO));

            disconnect_submitted_write(fixture, client, task).await;
            release.send(()).unwrap();
            let state = state.clone();
            tokio::task::spawn_blocking(move || {
                assert!(
                    state
                        .lifecycle
                        .updates()
                        .drain_for_test(Duration::from_secs(5))
                );
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
        let lists = compare_client_lists(&pair).await;
        for ((fixture, _), value) in comparison_endpoints(&pair).into_iter().zip(lists) {
            let clients = value["clients"].as_array().unwrap();
            let saved = persisted_clients(fixture);
            assert_eq!(clients.len(), 1);
            assert_eq!(saved.as_array().unwrap().len(), 1);
            assert_eq!(clients[0]["name"], "Disconnected device");
            assert!(
                clients[0]["id"] == saved[0]["id"] && clients[0]["api_key"] == saved[0]["api_key"]
            );
        }
    });
    old.legacy.shutdown().unwrap();
}
