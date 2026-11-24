use super::{account_writes::normalize_body, *};
use emp_modules::Module;
use emp_test_support::test_timeout;
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

    async fn submit(self, fixture: &Fixture, body: &[u8], status: u16) -> Value {
        fixture
            .request_checked(
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
        .server
        .state
        .backend
        .configuration
        .vault
        .read_encrypted_json(&path)
        .unwrap()
}

async fn clients(fixture: &Fixture) -> Value {
    fixture.read(CLIENTS).await
}

#[test]
fn module_and_gateway_writes_preserve_drain_order() {
    fixture_with_decision().run_wire(async |fixture, _| {
        for route in [MODULE, ISSUE, REVOKE] {
            fixture
                .request_checked(
                    |port, _| route.wire(port, None, b"{"),
                    401,
                    normalize_clients,
                )
                .await;
            fixture
                .request_checked(
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
            route.submit(fixture, b"{", 400).await;
            assert_eq!(
                route.submit(fixture, b"[]", 400).await["error"]["message"],
                "request body must be a JSON object",
            );
            let limit = fixture
                .request_checked(
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
                MODULE.submit(fixture, body, 400).await["error"]["message"],
                "Unknown EMP module"
            );
        }
        for body in [
            br#"{"id":"decision"}"#.as_slice(),
            br#"{"id":"decision","action":"not-an-action"}"#,
            br#"{"id":"decision","action":false}"#,
        ] {
            assert_eq!(
                MODULE.submit(fixture, body, 409).await["error"]["code"],
                "invalid_module_action",
            );
        }
        assert_eq!(
            MODULE
                .submit(fixture, br#"{"action":"update","id":"team"}"#, 409)
                .await["error"]["code"],
            "module_not_installed",
        );
        assert_eq!(
            MODULE
                .submit(fixture, br#"{"action":"uninstall","id":"decision"}"#, 409)
                .await["error"]["code"],
            "module_disable_before_uninstall",
        );
        let initial = fixture.read(MODULES).await;
        assert_eq!(decision(&initial)["active"], true);
        let disable = br#"{"action":"disable","id":"decision","revision":10,"ignored":true}"#;
        let disabled = MODULE.submit(fixture, disable, 200).await;
        assert_eq!(decision(&disabled)["active"], false);
        assert_eq!(decision(&disabled)["enabled"], false);
        let enabled = MODULE
            .submit(
                fixture,
                br#"{"action":"enable","id":"decision","revision":"ignored"}"#,
                200,
            )
            .await;
        assert_eq!(enabled, initial);
        MODULE.submit(fixture, disable, 200).await;
        let removed = MODULE
            .submit(fixture, br#"{"action":"uninstall","id":"decision"}"#, 200)
            .await;
        assert!(decision(&removed)["installed_version"].is_null());
        assert_eq!(fixture.read(MODULES).await, removed);
        {
            let registry = fixture.directory.path().join("state/modules/modules.json");
            assert_eq!(
                serde_json::from_slice::<Value>(&fs::read(registry).unwrap()).unwrap(),
                json!({})
            );
        }

        assert_eq!(
            MODULE
                .submit(
                    fixture,
                    br#"{"action":"check_updates","id":false,"revision":[],"ignored":true}"#,
                    200,
                )
                .await,
            json!({"updates":[]}),
        );
        for body in [b"{}".as_slice(), br#"{"name":false}"#, br#"{"name":"   "}"#] {
            assert_eq!(
                ISSUE.submit(fixture, body, 400).await["error"]["message"],
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
                route.submit(fixture, body, 400).await["error"]["message"],
                "A device key ID is required",
            );
        }
        assert_eq!(
            ISSUE.submit(fixture, br#"{"id":"missing"}"#, 400).await["error"]["message"],
            "Device key was not found",
        );
        assert_eq!(
            REVOKE
                .submit(fixture, br#"{"id":"missing","ignored":true}"#, 200)
                .await,
            json!({"ok":true})
        );
        {
            let value = clients(fixture).await;
            assert!(value["clients"] == json!([]));
        }

        let issued = fixture
            .request_json(
                |port, session| {
                    ISSUE.wire(
                        port,
                        Some(session),
                        br#"{"name":" Device ","ignored":true}"#,
                    )
                },
                200,
            )
            .await;
        let before = clients(fixture).await;
        {
            let client = &before["clients"][0];
            assert_eq!(issued["name"], "Device");
            assert!(client["id"] == issued["id"] && client["api_key"] == issued["api_key"]);
        }
        let replaced = fixture
            .request_json(
                |port, session| {
                    let body = json!({"id":issued["id"],"name":false,"ignored":true});
                    ISSUE.wire(port, Some(session), body.to_string().as_bytes())
                },
                200,
            )
            .await;
        let listed = clients(fixture).await;
        {
            let client = &listed["clients"][0];
            assert_eq!(listed["clients"].as_array().unwrap().len(), 1);
            assert!(replaced["id"] == issued["id"]);
            assert!(replaced["api_key"] != issued["api_key"]);
            assert_eq!(replaced["name"], "Device");
            assert!(client["id"] == replaced["id"] && client["api_key"] == replaced["api_key"]);
            assert_eq!(client["created_at"], before["clients"][0]["created_at"]);
            assert!(persisted_clients(fixture)[0]["api_key"] == client["api_key"]);
        }
        let revoked = fixture
            .request_json(
                |port, session| {
                    let body = json!({"id":replaced["id"],"ignored":true});
                    REVOKE.wire(port, Some(session), body.to_string().as_bytes())
                },
                200,
            )
            .await;
        assert_eq!(revoked, json!({"ok":true}));
        {
            let reply = clients(fixture).await;
            assert!(reply["clients"] == json!([]));
            assert!(persisted_clients(fixture) == json!([]));
            assert!(
                fixture
                    .server
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
                route.submit(fixture, body, 503).await["error"]["code"],
                "updating"
            );
            let denied = fixture
                .request_checked(
                    |port, _| route.wire(port, None, b"{"),
                    503,
                    normalize_clients,
                )
                .await;
            assert_eq!(denied["error"]["code"], "updating");
        }
        assert_eq!(fixture.read(MODULES).await, removed);
        {
            let reply = clients(fixture).await;
            assert!(reply["clients"] == json!([]));
            assert!(persisted_clients(fixture) == json!([]));
        }

        {
            fixture
                .server
                .state
                .backend
                .modules
                .mark_unavailable(Module::Gateway, "fixture_disabled");
        }
        for route in [ISSUE, REVOKE] {
            let rejection = fixture
                .request_checked(
                    |port, _| route.wire(port, None, b"{"),
                    404,
                    normalize_clients,
                )
                .await;
            assert_eq!(rejection["error"]["code"], "module_disabled");
        }
    });
}

#[test]
fn submitted_gateway_issue_saves_once_after_disconnect_and_holds_drain() {
    Fixture::new().run_wire(async |fixture, _| {
        {
            let state = &fixture.server.state;
            let (ready, release) = state.backend.gateway_clients.gate_next_save_for_test();
            let (mut client, task) = connect_submitted_write(fixture).await;
            let wire = ISSUE.wire(
                fixture.port(),
                Some(&fixture.session()),
                br#"{"name":"Disconnected device"}"#,
            );
            client.write_all(&wire).await.unwrap();
            timeout(test_timeout(Duration::from_secs(5)), ready)
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
                        .drain_for_test(test_timeout(Duration::from_secs(5)))
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
        {
            let value = clients(fixture).await;
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
}
