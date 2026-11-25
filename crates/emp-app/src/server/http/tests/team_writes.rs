use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use emp_modules::{
    Module,
    workspace::{NATIVE_SHARE, SharePayload, SubscriptionSnapshot},
};
use emp_test_support::test_timeout;
use std::{fs, path::PathBuf};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpListener as AsyncListener,
};

const WORKSPACE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MEMBER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHARE: &str = "cccccccccccccccccccccccccccccccc";
const DEVICE: &str = "dddddddddddddddddddddddddddddddd";
const SESSION: &str = "fixture-team-session";
const PROFILE: &str = "state/team-profile.json";
const RETAINED: &str = "state/local-key-subscription.json";
const IGNORED: &[u8] = br#"{"extra":{"future":[1,null]}}"#;

pub(super) fn identity(name: &str) -> Value {
    json!({
        "workspace":{"id":WORKSPACE,"name":name},
        "member":{"id":MEMBER,"role":"team_admin","sync":true,"publish":true}
    })
}

fn subscription(owner: &str) -> SubscriptionSnapshot {
    let claims = json!({
        "exp":4_102_444_800_i64,
        "https://api.openai.com/auth":{"chatgpt_plan_type":"pro"},
    });
    let token = format!(
        "fixture.{}.fixture",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    SubscriptionSnapshot {
        account_id: owner.into(),
        access_token: token.clone(),
        id_token: Some(token),
        expires_at: 4_102_444_800,
        plan: Some("pro".into()),
    }
}

fn payload(owner: &str) -> SharePayload {
    SharePayload {
        provider: json!({"id":NATIVE_SHARE,"name":"Native","auth_mode":"codex_subscription"}),
        models: Vec::new(),
        subscription: Some(subscription(owner)),
    }
}

fn fixture(base: &str) -> Fixture {
    let directory = emp_test_support::fixture_dir().unwrap();
    let worker = format!(
        r#"
while read -r line; do
    case "$line" in
        *'"action":"endpoint"'*)
            operation=${{line##*'"operation":"'}}
            operation=${{operation%%'"'*}}
            method=post
            path=$(printf '%s' "$operation" | tr _ -)
            case "$operation" in
                issue_invite) path=join-code ;;
                usage_filtered) path=usage ;;
                list_members) path=members; method=get ;;
                status|profiles|overview|backup|subscriptions|shared_services|join_code|publish_version|shares|usage)
                    method=get ;;
            esac
            printf '{{"value":{{"url":"{base}/workspace/v1/%s","method":"%s"}}}}\n' "$path" "$method"
            ;;
        *'"action":"identity"'*'"role":"member"'*) printf '%s\n' '{{"value":{{"migration_allowed":false,"sync_allowed":true}}}}' ;;
        *'"action":"identity"'*) printf '%s\n' '{{"value":{{"migration_allowed":true,"sync_allowed":true}}}}' ;;
        *'"action":"sync_shares"'*) printf '%s\n' '{{"value":{{"selected":[0],"skipped_native":0}}}}' ;;
        *'"action":"discovery"'*) printf '%s\n' '{{"value":[]}}' ;;
        *) printf '%s\n' '{{"value":null,"error":"unexpected_fixture_command"}}' ;;
    esac
done
"#
    );
    let root = directory.path();
    install_worker_fixture(root, Module::Team, "export const panel = {};", &worker);
    fs::write(root.join("state/team-device-id"), DEVICE).unwrap();
    let auth = root.join("accounts/retained/auth.json.enc");
    fs::create_dir_all(auth.parent().unwrap()).unwrap();
    let vault = emp_state::VaultStore::from_sources(None, &root.join("state/master.key")).unwrap();
    vault
        .write_encrypted_json(&auth, &subscription("retained-account").auth_document())
        .unwrap();
    fs::write(
        root.join("config.json"),
        json!({
            "accounts":[{"id":"retained","prefix":"retained","name":"Retained","auth_file":auth}],
            "account_store_path":root.join("accounts"),
            "codex_base_url":format!("{base}/v1")
        })
        .to_string(),
    )
    .unwrap();
    fs::create_dir(root.join("codex")).unwrap();
    fs::write(
        root.join("codex/auth.json"),
        br#"{"emp_local_connection":true,"OPENAI_API_KEY":"emp-local-fixture"}"#,
    )
    .unwrap();

    let fixture = Fixture::with_directory(directory);
    fixture
        .server
        .state
        .backend
        .team
        .isolate_discovery_for_test();
    fixture
        .server
        .state
        .backend
        .team
        .pause_background_for_test();
    fixture
}

pub(super) fn run_team(test: impl AsyncFnOnce(&Fixture, &AsyncListener, &str)) {
    let upstream = TcpListener::bind(LOOPBACK).unwrap();
    upstream.set_nonblocking(true).unwrap();
    let base = format!("http://{}", upstream.local_addr().unwrap());

    fixture(&base).run_wire(async |fixture, _| {
        test(fixture, &AsyncListener::from_std(upstream).unwrap(), &base).await;
    });
}

struct UpstreamRequest {
    stream: TcpStream,
    head: String,
    body: Vec<u8>,
}

impl UpstreamRequest {
    async fn accept(listener: &AsyncListener, path: &str) -> Self {
        timeout(test_timeout(Duration::from_secs(5)).duration(), async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            Self {
                stream: stream.into_inner(),
                head,
                body,
            }
        })
        .await
        .unwrap_or_else(|_| panic!("Team upstream request to {path} timed out"))
    }

    fn expect(&self, path: &str, body: Option<&Value>) {
        let method = if body.is_some() { "POST" } else { "GET" };
        assert!(
            self.head
                .starts_with(&format!("{method} /workspace/v1/{path} HTTP/1.1\r\n")),
            "unexpected upstream request: {}",
            self.head.lines().next().unwrap()
        );
        let headers = self.head.to_ascii_lowercase();
        assert_eq!(
            headers
                .lines()
                .find(|line| line.starts_with("authorization:")),
            (!matches!(path, "login" | "join"))
                .then_some("authorization: bearer fixture-team-session"),
        );
        assert_eq!(
            headers
                .lines()
                .any(|line| line == "content-type: application/json"),
            body.is_some()
        );
        let expected = body
            .map(|body| serde_json::to_vec(body).unwrap())
            .unwrap_or_default();
        assert!(self.body == expected, "{path} request body differs");
    }

    async fn reply(mut self, status: u16, body: &Value) {
        let body = serde_json::to_vec(body).unwrap();
        let head = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        self.stream.write_all(head.as_bytes()).await.unwrap();
        self.stream.write_all(&body).await.unwrap();
    }
}

pub(super) async fn respond(
    upstream: &AsyncListener,
    path: &str,
    body: Option<&Value>,
    status: u16,
    reply: &Value,
) {
    let request = UpstreamRequest::accept(upstream, path).await;
    request.expect(path, body);
    request.reply(status, reply).await;
}

fn credentials(base: &str) -> Value {
    json!({"base":base,"name":"Manager","password":"fixture-password","extra":{"unused":true}})
}

fn login_payload() -> Value {
    json!({"name":"Manager","password":"fixture-password","device":DEVICE})
}

fn login_reply() -> Value {
    let mut reply = identity("Initial team");
    reply["token"] = SESSION.into();
    reply
}

pub(super) async fn login(fixture: &Fixture, upstream: &AsyncListener, base: &str) -> Value {
    let body = credentials(base).to_string();
    let (reply, ()) = tokio::join!(
        fixture.post("/api/team/login?fixture=1", body.as_bytes(), 200),
        async {
            respond(
                upstream,
                "login",
                Some(&login_payload()),
                200,
                &login_reply(),
            )
            .await
        },
    );
    assert_eq!(
        reply,
        json!({"joined":true,"base":base,"identity":identity("Initial team")})
    );
    reply
}

async fn forward(
    fixture: &Fixture,
    upstream: &AsyncListener,
    operation: &str,
    path: &str,
    data: Option<&Value>,
    reply: &Value,
) {
    let (actual, ()) = tokio::join!(
        async {
            fixture
                .post(&format!("/api/team/{operation}"), IGNORED, 200)
                .await
        },
        respond(upstream, path, data, 200, reply),
    );
    assert_eq!(actual, *reply);
}

#[test]
fn team_writes_preserve_upstream_bytes_and_gate_order() {
    run_team(async |fixture, upstream, base| {
        let unknown = "/api/team/missing";
        assert_eq!(
            fixture.post(unknown, b"{}", 401).await["error"]["code"],
            "workspace_login_required"
        );
        for path in ["/api/team/login", "/api/team/join"] {
            assert_eq!(
                fixture.post(path, b"{}", 400).await["error"]["code"],
                "invalid_workspace_request"
            );
        }
        let discovered = fixture.post("/api/team/discover", IGNORED, 200).await;
        assert_eq!(discovered, json!({"workspaces":[]}));

        let join = json!({"base":base,"name":"Applicant","nickname":"A","password":"example","code":"fixture","extra":{"future":[1,null]}});
        let (joined, ()) = tokio::join!(
            async {
                fixture
                    .post("/api/team/join", join.to_string().as_bytes(), 200)
                    .await
            },
            async { respond(upstream, "join", Some(&join), 200, &json!({"pending":true})).await },
        );
        assert_eq!(joined, json!({"pending":true}));
        assert_eq!(fixture.read("/api/team/status").await["joined"], false);
        login(fixture, upstream, base).await;

        let whole_object = serde_json::from_slice::<Value>(IGNORED).unwrap();
        for (operation, path) in [
            ("members", "members"),
            ("member_access", "member-access"),
            ("reject_member", "reject-member"),
            ("remove_member", "remove-member"),
            ("budget", "budget"),
            ("weekly_budget", "weekly-budget"),
            ("member_tiers", "member-tiers"),
            ("review_service", "review-service"),
            ("remove_service", "remove-service"),
            ("profile", "profile"),
            ("profile_visibility", "profile-visibility"),
            ("subscription_reminder", "subscription-reminder"),
            ("audit", "audit"),
        ] {
            forward(
                fixture,
                upstream,
                operation,
                path,
                Some(&whole_object),
                &json!({"entries":[],"ok":true}),
            )
            .await;
        }
        for (operation, path) in [
            ("profiles", "profiles"),
            ("overview", "overview"),
            ("backup", "backup"),
            ("subscriptions", "subscriptions"),
            ("list_members", "members"),
            ("shared_services", "shared-services"),
            ("join_code", "join-code"),
        ] {
            forward(
                fixture,
                upstream,
                operation,
                path,
                None,
                &json!({"members":[],"ok":true}),
            )
            .await;
        }
        forward(
            fixture,
            upstream,
            "issue_invite",
            "join-code",
            Some(&json!({})),
            &json!({"code":"fixture","expires_in":60}),
        )
        .await;
        forward(
            fixture,
            upstream,
            "usage",
            "usage",
            None,
            &json!({"rows":[]}),
        )
        .await;
        for period in [json!("lifetime"), Value::Null] {
            let body = json!({"period":period,"extra":true}).to_string();
            let (actual, ()) = tokio::join!(
                fixture.post("/api/team/usage", body.as_bytes(), 200),
                async {
                    respond(
                        upstream,
                        "usage",
                        Some(&json!({"period":period})),
                        200,
                        &json!({"rows":[]}),
                    )
                    .await
                },
            );
            assert_eq!(actual, json!({"rows":[]}));
        }

        for path in [
            unknown,
            "/api/team/",
            "/api/team/members/extra",
            "/api/team/%6dembers",
            "/api/team/notices",
            "/api/team/publish_source#fragment",
        ] {
            assert_eq!(
                fixture.post(path, b"{}", 400).await["error"]["code"],
                "invalid_workspace_request"
            );
        }
        let path = "/api/team/members";
        fixture
            .request_json(|port, _| json_request(port, path, &[], b"{"), 401)
            .await;
        fixture
            .request_json(
                |port, session| {
                    json_request(
                        port,
                        path,
                        &[session, "Origin: https://foreign.invalid"],
                        b"[",
                    )
                },
                401,
            )
            .await;
        assert_eq!(
            fixture.post(path, b"[]", 400).await["error"]["message"],
            "request body must be a JSON object"
        );
        fixture.post(path, b"{", 400).await;
        assert_eq!(
            fixture
                .request_json(
                    |port, session| request(
                        port,
                        "POST",
                        path,
                        &[
                            session,
                            "Content-Type: application/json",
                            "Content-Length: 5242881"
                        ],
                        b""
                    ),
                    413,
                )
                .await["error"]["limit_bytes"],
            5242880
        );

        {
            assert!(
                fixture
                    .server
                    .state
                    .lifecycle
                    .updates()
                    .drain_for_test(Duration::ZERO)
            );
        }
        for (path, code, status) in [(path, "updating", 503), (unknown, "updating", 503)] {
            let rejected = fixture
                .request_json(|port, _| json_request(port, path, &[], b"{"), status)
                .await;
            assert_eq!(rejected["error"]["code"], code);
        }
        assert_eq!(fixture.read("/api/team/status").await["joined"], true);
        {
            fixture
                .server
                .state
                .backend
                .modules
                .mark_unavailable(Module::Team, "fixture-disabled");
        }
        let disabled = fixture
            .request_json(|port, _| json_request(port, path, &[], b"{"), 404)
            .await;
        assert_eq!(disabled["error"]["code"], "module_disabled");
    });
}

pub(super) fn read_team_profile(fixture: &Fixture) -> Value {
    let path = fixture.directory.path().join(PROFILE);
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

fn saved_config(fixture: &Fixture) -> Value {
    let root = fixture.directory.path().canonicalize().unwrap();
    let config = fs::read_to_string(root.join("config.json")).unwrap();
    serde_json::from_str::<Value>(&config.replace(root.to_str().unwrap(), "/fixture")).unwrap()
}

fn saved_profile(fixture: &Fixture) -> Value {
    read_team_profile(fixture)
}

fn shared_auth(fixture: &Fixture) -> PathBuf {
    fixture
        .directory
        .path()
        .join(format!("accounts/team-{SHARE}/auth.json.enc"))
}

fn catalog_path(fixture: &Fixture) -> PathBuf {
    crate::services::catalog::generated_catalog_path(&fixture.server.state)
}

fn expect_catalog_refresh(fixture: &Fixture) {
    {
        let path = catalog_path(fixture);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"{\"stale\":true}").unwrap();
    }
}

fn assert_refreshed_catalog(fixture: &Fixture) {
    let catalog = fs::read(catalog_path(fixture)).unwrap();
    assert!(serde_json::from_slice::<Value>(&catalog).unwrap()["models"].is_array());
}

async fn sync_share(fixture: &Fixture, upstream: &AsyncListener, current_identity: &Value) {
    expect_catalog_refresh(fixture);
    let (synced, ()) = tokio::join!(fixture.post("/api/team/sync", IGNORED, 200), async {
        respond(upstream, "status", None, 200, current_identity).await;
        let shares = json!({"workspace_id":WORKSPACE,"shares":[
            {"id":SHARE,"version":1,"owner":"Shared Codex","payload":payload("shared-account")}
        ]});
        respond(upstream, "shares", None, 200, &shares).await;
    });
    assert_eq!(synced, json!({"synced":1,"unchanged":0,"skipped_native":0}));
    let profile = saved_profile(fixture);
    assert_eq!(profile["session"]["identity"], *current_identity);
    let share_id = format!("team-{SHARE}");
    assert_eq!(profile["imported"][&share_id]["available"], true);
    assert!(
        saved_config(fixture)["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["id"] == share_id && row["workspace_shared"] == true })
    );
    let expected = subscription("shared-account").auth_document();
    {
        let path = shared_auth(fixture);
        assert!(
            fs::read(&path)
                .unwrap()
                .starts_with(emp_state::vault::VAULT_MAGIC)
        );
        let auth = fixture
            .server
            .state
            .backend
            .configuration
            .vault
            .read_encrypted_json(&path)
            .unwrap();
        assert_eq!(auth, expected);
    }
    assert_refreshed_catalog(fixture);
}

async fn assert_signed_out(fixture: &Fixture) {
    assert_eq!(
        fixture.read("/api/team/status").await,
        json!({"joined":false,"recovery_required":false})
    );
    {
        assert!(!fixture.directory.path().join(PROFILE).exists());
        assert!(!shared_auth(fixture).exists());
        assert!(
            fixture
                .directory
                .path()
                .join("accounts/retained/auth.json.enc")
                .is_file()
        );
    }
    let config = saved_config(fixture);
    let accounts = config["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["id"], "retained");
    assert_refreshed_catalog(fixture);
}

#[test]
fn team_login_status_publish_sync_handover_and_logout_persist_the_same_state() {
    run_team(async |fixture, upstream, base| {
        expect_catalog_refresh(fixture);
        let signed_in = login(fixture, upstream, base).await;
        assert_eq!(fixture.read("/api/team/status").await, signed_in);
        assert_eq!(
            saved_profile(fixture)["session"],
            json!({"base":base,"token":SESSION,"identity":identity("Initial team")})
        );
        assert_refreshed_catalog(fixture);

        let latest = identity("Renamed team");
        expect_catalog_refresh(fixture);
        let (status, ()) = tokio::join!(
            fixture.post("/api/team/status", IGNORED, 200),
            respond(upstream, "status", None, 200, &latest),
        );
        assert_eq!(status["identity"], latest);
        assert_eq!(fixture.read("/api/team/status").await, status);
        assert_eq!(saved_profile(fixture)["session"]["identity"], latest);
        assert_refreshed_catalog(fixture);

        assert_eq!(
            fixture
                .post(
                    "/api/team/publish",
                    br#"{"provider":false,"account_id":null}"#,
                    400
                )
                .await["error"]["code"],
            "workspace_only_native_subscription"
        );
        let version_reply = json!({"version":3});
        let (invalid, ()) = tokio::join!(
            fixture.post("/api/team/publish", br#"{"account_id":null}"#, 400),
            respond(upstream, "publish-version", None, 200, &version_reply),
        );
        assert_eq!(invalid["error"]["code"], "invalid_workspace_request");

        let original_config = saved_config(fixture);
        let original_profile = saved_profile(fixture);
        let (published, ()) = tokio::join!(
            fixture.post(
                "/api/team/publish",
                br#"{"provider":"subscription:@native","account_id":"retained","extra":true}"#,
                200
            ),
            async {
                respond(upstream, "publish-version", None, 200, &version_reply).await;
                let data = json!({"source":NATIVE_SHARE,"expected_version":3,"payload":payload("retained-account")});
                respond(upstream, "publish", Some(&data), 200, &json!({"version":4})).await;
            }
        );
        assert_eq!(published, json!({"version":4}));
        let selection = json!({
            "status":"kept","account_id":"retained",
            "identity":emp_state::accounts::account_auth_identity(&subscription("retained-account").auth_document()),
        });
        {
            let selected: Value =
                serde_json::from_slice(&fs::read(fixture.directory.path().join(RETAINED)).unwrap())
                    .unwrap();
            assert_eq!(selected, selection);
        }
        assert_eq!(saved_config(fixture), original_config);
        assert_eq!(saved_profile(fixture), original_profile);
        assert_refreshed_catalog(fixture);

        sync_share(fixture, upstream, &latest).await;
        expect_catalog_refresh(fixture);
        let handover = json!({"successor":MEMBER,"extra":{"future":true}});
        let handover_body = handover.to_string();
        let (handed_over, ()) = tokio::join!(
            fixture.post("/api/team/handover", handover_body.as_bytes(), 200),
            async {
                respond(upstream, "handover", Some(&handover), 200, &json!({})).await;
                respond(upstream, "logout", Some(&json!({})), 200, &json!({})).await;
            }
        );
        assert_eq!(handed_over["joined"], false);
        assert_signed_out(fixture).await;

        login(fixture, upstream, base).await;
        sync_share(fixture, upstream, &latest).await;
        expect_catalog_refresh(fixture);
        let (logged_out, ()) =
            tokio::join!(fixture.post("/api/team/logout", IGNORED, 200), async {
                respond(upstream, "logout", Some(&json!({})), 200, &json!({})).await
            },);
        assert_eq!(logged_out, handed_over);
        assert_signed_out(fixture).await;
    });
}

#[test]
fn team_upstream_401_clears_the_persisted_session_and_refreshes_the_catalog() {
    run_team(async |fixture, upstream, base| {
        login(fixture, upstream, base).await;
        sync_share(fixture, upstream, &identity("Initial team")).await;
        let before = saved_profile(fixture);
        assert_eq!(before["session"]["token"], SESSION);
        expect_catalog_refresh(fixture);

        let code = "workspace_authentication_required";
        let (error, ()) = tokio::join!(fixture.post("/api/team/overview", IGNORED, 401), async {
            respond(
                upstream,
                "overview",
                None,
                401,
                &json!({"error":{"code":code}}),
            )
            .await
        },);
        assert_eq!(error["error"]["code"], code);
        assert_eq!(
            fixture.read("/api/team/status").await,
            json!({"joined":false,"recovery_required":true})
        );
        let cleared = saved_profile(fixture);
        assert_eq!(cleared["session"], Value::Null);
        assert_eq!(cleared["imported"], before["imported"]);
        assert_refreshed_catalog(fixture);
        assert_eq!(
            fixture.post("/api/team/overview", IGNORED, 401).await["error"]["code"],
            "workspace_login_required"
        );

        {
            let state = &fixture.server.state;
            let config = state.backend.configuration.read().unwrap();
            let reopened = crate::services::team::TeamState::open(
                fixture.directory.path().join(PROFILE),
                &state.backend.configuration.vault,
                &config,
            );
            assert_eq!(
                reopened.local_status().ok(),
                Some(json!({"joined":false,"recovery_required":true}))
            );
        }
    });
}

#[test]
fn submitted_team_login_survives_disconnect_once_and_holds_drain() {
    run_team(async |fixture, upstream, base| {
        expect_catalog_refresh(fixture);
        let body = credentials(base).to_string();
        {
            let state = &fixture.server.state;
            let (mut client, task) = connect_submitted_write(fixture).await;
            let request = json_request(
                fixture.port(),
                "/api/team/login",
                &[&fixture.session()],
                body.as_bytes(),
            );
            client.write_all(&request).await.unwrap();
            let request = UpstreamRequest::accept(upstream, "login").await;
            request.expect("login", Some(&login_payload()));
            assert!(!fixture.directory.path().join(PROFILE).exists());
            assert!(!state.lifecycle.updates().drain_for_test(Duration::ZERO));
            disconnect_submitted_write(fixture, client, task).await;
            request.reply(200, &login_reply()).await;

            let state = state.clone();
            tokio::task::spawn_blocking(move || {
                assert!(
                    state
                        .lifecycle
                        .updates()
                        .drain_for_test(test_timeout(Duration::from_secs(5)).duration())
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
        let accept = std::pin::pin!(upstream.accept());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            accept.poll(&mut context).is_pending(),
            "duplicate upstream login"
        );
        assert_eq!(
            fixture.read("/api/team/status").await["identity"],
            identity("Initial team")
        );
        assert_eq!(saved_profile(fixture)["session"]["token"], SESSION);
        assert_refreshed_catalog(fixture);
    });
}
