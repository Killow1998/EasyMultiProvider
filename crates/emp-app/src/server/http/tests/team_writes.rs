use super::{account_writes::Pair, *};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use emp_modules::{
    Module,
    workspace::{NATIVE_SHARE, SharePayload, SubscriptionSnapshot},
};
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

fn fixture(base: &str, driver: TestDriver) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
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

    let fixture = Fixture::with_driver(directory, driver);
    fixture
        .legacy
        .state
        .backend
        .team
        .isolate_discovery_for_test();
    fixture
        .legacy
        .state
        .backend
        .team
        .pause_background_for_test();
    fixture
}

pub(super) fn run_pair(test: impl AsyncFnOnce(&Pair<'_>, &AsyncListener, &str)) {
    let upstream = TcpListener::bind(LOOPBACK).unwrap();
    upstream.set_nonblocking(true).unwrap();
    let base = format!("http://{}", upstream.local_addr().unwrap());
    let old = fixture(&base, TestDriver::reference());
    fixture(&base, TestDriver::Candidate).run_wire(async |new, candidate| {
        let pair = Pair {
            old: &old,
            new,
            candidate,
        };
        test(&pair, &AsyncListener::from_std(upstream).unwrap(), &base).await;
    });
    old.legacy.shutdown().unwrap();
}

fn endpoints<'a>(pair: &Pair<'a>) -> [(&'a Fixture, SocketAddr); 2] {
    [
        (pair.old, pair.old.legacy.local_addr()),
        (pair.new, pair.candidate),
    ]
}

struct UpstreamRequest {
    stream: TcpStream,
    head: String,
    body: Vec<u8>,
}

impl UpstreamRequest {
    async fn accept(listener: &AsyncListener, path: &str) -> Self {
        timeout(Duration::from_secs(5), async {
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
    let first = UpstreamRequest::accept(upstream, path).await;
    first.expect(path, body);
    let second = UpstreamRequest::accept(upstream, path).await;
    assert_eq!(first.head, second.head);
    assert!(first.body == second.body, "{path} upstream bodies differ");
    tokio::join!(first.reply(status, reply), second.reply(status, reply));
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

pub(super) async fn login(pair: &Pair<'_>, upstream: &AsyncListener, base: &str) -> Value {
    let body = credentials(base).to_string();
    let (reply, ()) = tokio::join!(
        pair.post("/api/team/login?fixture=1", body.as_bytes(), 200),
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
    pair: &Pair<'_>,
    upstream: &AsyncListener,
    operation: &str,
    path: &str,
    data: Option<&Value>,
    reply: &Value,
) {
    let (actual, ()) = tokio::join!(
        async {
            pair.post(&format!("/api/team/{operation}"), IGNORED, 200)
                .await
        },
        respond(upstream, path, data, 200, reply),
    );
    assert_eq!(actual, *reply);
}

#[test]
fn team_writes_match_legacy_wire_upstream_bytes_and_gate_order() {
    run_pair(async |pair, upstream, base| {
        let unknown = "/api/team/missing";
        assert_eq!(
            pair.post(unknown, b"{}", 401).await["error"]["code"],
            "workspace_login_required"
        );
        for path in ["/api/team/login", "/api/team/join"] {
            assert_eq!(
                pair.post(path, b"{}", 400).await["error"]["code"],
                "invalid_workspace_request"
            );
        }
        let discovered = pair.post("/api/team/discover", IGNORED, 200).await;
        assert_eq!(discovered, json!({"workspaces":[]}));

        let join = json!({"base":base,"name":"Applicant","nickname":"A","password":"example","code":"fixture","extra":{"future":[1,null]}});
        let (joined, ()) = tokio::join!(
            async {
                pair.post("/api/team/join", join.to_string().as_bytes(), 200)
                    .await
            },
            async { respond(upstream, "join", Some(&join), 200, &json!({"pending":true})).await },
        );
        assert_eq!(joined, json!({"pending":true}));
        assert_eq!(pair.read("/api/team/status").await["joined"], false);
        login(pair, upstream, base).await;

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
                pair,
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
                pair,
                upstream,
                operation,
                path,
                None,
                &json!({"members":[],"ok":true}),
            )
            .await;
        }
        forward(
            pair,
            upstream,
            "issue_invite",
            "join-code",
            Some(&json!({})),
            &json!({"code":"fixture","expires_in":60}),
        )
        .await;
        forward(pair, upstream, "usage", "usage", None, &json!({"rows":[]})).await;
        for period in [json!("lifetime"), Value::Null] {
            let body = json!({"period":period,"extra":true}).to_string();
            let (actual, ()) =
                tokio::join!(pair.post("/api/team/usage", body.as_bytes(), 200), async {
                    respond(
                        upstream,
                        "usage",
                        Some(&json!({"period":period})),
                        200,
                        &json!({"rows":[]}),
                    )
                    .await
                },);
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
                pair.post(path, b"{}", 400).await["error"]["code"],
                "invalid_workspace_request"
            );
        }
        let path = "/api/team/members";
        pair.parity(
            |port, _| json_request(port, path, &[], b"{"),
            401,
            |reply| reply,
        )
        .await;
        pair.parity(
            |port, session| {
                json_request(
                    port,
                    path,
                    &[session, "Origin: https://foreign.invalid"],
                    b"[",
                )
            },
            401,
            |reply| reply,
        )
        .await;
        assert_eq!(
            pair.post(path, b"[]", 400).await["error"]["message"],
            "request body must be a JSON object"
        );
        pair.post(path, b"{", 400).await;
        assert_eq!(
            pair.parity(
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
                |reply| reply,
            )
            .await["error"]["limit_bytes"],
            5242880
        );

        for (fixture, _) in endpoints(pair) {
            assert!(
                fixture
                    .legacy
                    .state
                    .lifecycle
                    .updates()
                    .drain_for_test(Duration::ZERO)
            );
        }
        for (path, code, status) in [(path, "updating", 503), (unknown, "updating", 503)] {
            let rejected = pair
                .parity(
                    |port, _| json_request(port, path, &[], b"{"),
                    status,
                    |reply| reply,
                )
                .await;
            assert_eq!(rejected["error"]["code"], code);
        }
        assert_eq!(pair.read("/api/team/status").await["joined"], true);
        for (fixture, _) in endpoints(pair) {
            fixture
                .legacy
                .state
                .backend
                .modules
                .mark_unavailable(Module::Team, "fixture-disabled");
        }
        let disabled = pair
            .parity(
                |port, _| json_request(port, path, &[], b"{"),
                404,
                |reply| reply,
            )
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
        .legacy
        .state
        .backend
        .configuration
        .vault
        .read_encrypted_json(&path)
        .unwrap()
}

fn saved_config(pair: &Pair<'_>) -> Value {
    let [old, new] = [pair.old, pair.new].map(|fixture| {
        let root = fixture.directory.path().canonicalize().unwrap();
        let config = fs::read_to_string(root.join("config.json")).unwrap();
        serde_json::from_str::<Value>(&config.replace(root.to_str().unwrap(), "/fixture")).unwrap()
    });
    assert_eq!(old, new);
    old
}

fn saved_profile(pair: &Pair<'_>) -> Value {
    let old = read_team_profile(pair.old);
    assert_eq!(old, read_team_profile(pair.new));
    old
}

fn shared_auth(fixture: &Fixture) -> PathBuf {
    fixture
        .directory
        .path()
        .join(format!("accounts/team-{SHARE}/auth.json.enc"))
}

fn catalog_path(fixture: &Fixture) -> PathBuf {
    crate::services::catalog::generated_catalog_path(&fixture.legacy.state)
}

fn expect_catalog_refresh(pair: &Pair<'_>) {
    for (fixture, _) in endpoints(pair) {
        let path = catalog_path(fixture);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"{\"stale\":true}").unwrap();
    }
}

fn assert_refreshed_catalog(pair: &Pair<'_>) {
    let [old, new] = [pair.old, pair.new].map(|fixture| fs::read(catalog_path(fixture)).unwrap());
    assert_eq!(old, new);
    assert!(serde_json::from_slice::<Value>(&old).unwrap()["models"].is_array());
}

async fn sync_share(pair: &Pair<'_>, upstream: &AsyncListener, current_identity: &Value) {
    expect_catalog_refresh(pair);
    let (synced, ()) = tokio::join!(pair.post("/api/team/sync", IGNORED, 200), async {
        respond(upstream, "status", None, 200, current_identity).await;
        let shares = json!({"workspace_id":WORKSPACE,"shares":[
            {"id":SHARE,"version":1,"owner":"Shared Codex","payload":payload("shared-account")}
        ]});
        respond(upstream, "shares", None, 200, &shares).await;
    });
    assert_eq!(synced, json!({"synced":1,"unchanged":0,"skipped_native":0}));
    let profile = saved_profile(pair);
    assert_eq!(profile["session"]["identity"], *current_identity);
    let share_id = format!("team-{SHARE}");
    assert_eq!(profile["imported"][&share_id]["available"], true);
    assert!(
        saved_config(pair)["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["id"] == share_id && row["workspace_shared"] == true })
    );
    let expected = subscription("shared-account").auth_document();
    for (fixture, _) in endpoints(pair) {
        let path = shared_auth(fixture);
        assert!(
            fs::read(&path)
                .unwrap()
                .starts_with(emp_state::vault::VAULT_MAGIC)
        );
        let auth = fixture
            .legacy
            .state
            .backend
            .configuration
            .vault
            .read_encrypted_json(&path)
            .unwrap();
        assert_eq!(auth, expected);
    }
    assert_refreshed_catalog(pair);
}

async fn assert_signed_out(pair: &Pair<'_>) {
    assert_eq!(
        pair.read("/api/team/status").await,
        json!({"joined":false,"recovery_required":false})
    );
    for (fixture, _) in endpoints(pair) {
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
    let config = saved_config(pair);
    let accounts = config["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["id"], "retained");
    assert_refreshed_catalog(pair);
}

#[test]
fn team_login_status_publish_sync_handover_and_logout_persist_the_same_state() {
    run_pair(async |pair, upstream, base| {
        expect_catalog_refresh(pair);
        let signed_in = login(pair, upstream, base).await;
        assert_eq!(pair.read("/api/team/status").await, signed_in);
        assert_eq!(
            saved_profile(pair)["session"],
            json!({"base":base,"token":SESSION,"identity":identity("Initial team")})
        );
        assert_refreshed_catalog(pair);

        let latest = identity("Renamed team");
        expect_catalog_refresh(pair);
        let (status, ()) = tokio::join!(
            pair.post("/api/team/status", IGNORED, 200),
            respond(upstream, "status", None, 200, &latest),
        );
        assert_eq!(status["identity"], latest);
        assert_eq!(pair.read("/api/team/status").await, status);
        assert_eq!(saved_profile(pair)["session"]["identity"], latest);
        assert_refreshed_catalog(pair);

        assert_eq!(
            pair.post(
                "/api/team/publish",
                br#"{"provider":false,"account_id":null}"#,
                400
            )
            .await["error"]["code"],
            "workspace_only_native_subscription"
        );
        let version_reply = json!({"version":3});
        let (invalid, ()) = tokio::join!(
            pair.post("/api/team/publish", br#"{"account_id":null}"#, 400),
            respond(upstream, "publish-version", None, 200, &version_reply),
        );
        assert_eq!(invalid["error"]["code"], "invalid_workspace_request");

        let original_config = saved_config(pair);
        let original_profile = saved_profile(pair);
        let (published, ()) = tokio::join!(
            pair.post(
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
        for (fixture, _) in endpoints(pair) {
            let selected: Value =
                serde_json::from_slice(&fs::read(fixture.directory.path().join(RETAINED)).unwrap())
                    .unwrap();
            assert_eq!(selected, selection);
        }
        assert_eq!(saved_config(pair), original_config);
        assert_eq!(saved_profile(pair), original_profile);
        assert_refreshed_catalog(pair);

        sync_share(pair, upstream, &latest).await;
        expect_catalog_refresh(pair);
        let handover = json!({"successor":MEMBER,"extra":{"future":true}});
        let handover_body = handover.to_string();
        let (handed_over, ()) = tokio::join!(
            pair.post("/api/team/handover", handover_body.as_bytes(), 200),
            async {
                respond(upstream, "handover", Some(&handover), 200, &json!({})).await;
                respond(upstream, "logout", Some(&json!({})), 200, &json!({})).await;
            }
        );
        assert_eq!(handed_over["joined"], false);
        assert_signed_out(pair).await;

        login(pair, upstream, base).await;
        sync_share(pair, upstream, &latest).await;
        expect_catalog_refresh(pair);
        let (logged_out, ()) = tokio::join!(pair.post("/api/team/logout", IGNORED, 200), async {
            respond(upstream, "logout", Some(&json!({})), 200, &json!({})).await
        },);
        assert_eq!(logged_out, handed_over);
        assert_signed_out(pair).await;
    });
}

#[test]
fn team_upstream_401_clears_the_persisted_session_and_refreshes_the_catalog() {
    run_pair(async |pair, upstream, base| {
        login(pair, upstream, base).await;
        sync_share(pair, upstream, &identity("Initial team")).await;
        let before = saved_profile(pair);
        assert_eq!(before["session"]["token"], SESSION);
        expect_catalog_refresh(pair);

        let code = "workspace_authentication_required";
        let (error, ()) = tokio::join!(pair.post("/api/team/overview", IGNORED, 401), async {
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
            pair.read("/api/team/status").await,
            json!({"joined":false,"recovery_required":true})
        );
        let cleared = saved_profile(pair);
        assert_eq!(cleared["session"], Value::Null);
        assert_eq!(cleared["imported"], before["imported"]);
        assert_refreshed_catalog(pair);
        assert_eq!(
            pair.post("/api/team/overview", IGNORED, 401).await["error"]["code"],
            "workspace_login_required"
        );

        for (fixture, _) in endpoints(pair) {
            let state = &fixture.legacy.state;
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
    run_pair(async |pair, upstream, base| {
        expect_catalog_refresh(pair);
        let body = credentials(base).to_string();
        let mut captured = Vec::new();
        for (fixture, address) in endpoints(pair) {
            let state = &fixture.legacy.state;
            let (mut client, task) = connect_submitted_write(fixture, address).await;
            let request = json_request(
                fixture.port(),
                "/api/team/login",
                &[&fixture.session()],
                body.as_bytes(),
            );
            client.write_all(&request).await.unwrap();
            let request = UpstreamRequest::accept(upstream, "login").await;
            request.expect("login", Some(&login_payload()));
            captured.push((request.head.clone(), request.body.clone()));
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
        assert_eq!(captured[0], captured[1]);
        let accept = std::pin::pin!(upstream.accept());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            accept.poll(&mut context).is_pending(),
            "duplicate upstream login"
        );
        assert_eq!(
            pair.read("/api/team/status").await["identity"],
            identity("Initial team")
        );
        assert_eq!(saved_profile(pair)["session"]["token"], SESSION);
        assert_refreshed_catalog(pair);
    });
}
