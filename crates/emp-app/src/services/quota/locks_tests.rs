use crate::lifecycle::ServerHandle;
use crate::services::accounts::{quota_refresh_lock, replacing_account_credentials};
use emp_test_support::{Deadline, test_timeout};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[test]
fn replacement_quota_refresh_waits_in_migration_lock_order() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let modules = root.join("state/modules");
    fs::create_dir_all(&modules).unwrap();
    let team = emp_modules::Module::Team;
    let hello = json!({
        "module":team,"version":crate::VERSION,"module_version":team.version(),
        "protocol":emp_modules::PROTOCOL,"host_api":emp_modules::HOST_API,
    });
    let worker = format!(
        "#!/bin/sh\nprintf '%s\\n' '{}'\nwhile read -r line; do printf '%s\\n' '{{\"value\":\"panel\",\"error\":null}}'; done\n",
        hello,
    );
    let digest = format!("{:x}", Sha256::digest(&worker));
    let path = modules.join(format!("team-{digest}"));
    fs::write(&path, worker).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        modules.join("modules.json"),
        json!({"team":{"enabled":true,"versions":{},"artifact":{"digest":digest,"version":team.version()}}}).to_string(),
    )
    .unwrap();

    let vault = emp_state::VaultStore::from_sources(None, &root.join("state/master.key")).unwrap();
    let auth = json!({"tokens":{"access_token":"fixture-token","account_id":"fixture-upstream"}});
    let account_identity = emp_state::accounts::account_auth_identity(&auth);
    let workspace = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let member = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let share = "dddddddddddddddddddddddddddddddd";
    let personal = "z-local";
    let shared = format!("team-{share}");
    let store = root.join("state/accounts");
    let personal_auth = store.join(personal).join("auth.json.enc");
    let shared_auth = store.join(&shared).join("auth.json.enc");
    for path in [&personal_auth, &shared_auth] {
        vault.write_encrypted_json(path, &auth).unwrap();
    }
    vault.write_encrypted_json(&root.join("state/team-profile.json"), &json!({
        "session":{"base":"http://127.0.0.1:9","token":"fixture-session",
            "identity":{"workspace":{"id":workspace},"member":{"id":member,"role":"team_admin","sync":true}}},
        "imported":{shared.clone():{"workspace":workspace,"share":share,"version":1,
            "available":true,"expires_at":(crate::util::system_now() as i64)+3600,"account_identity":account_identity}},
    })).unwrap();

    let config = root.join("config.json");
    fs::write(&config, json!({
        "codex_base_url":"http://127.0.0.1:9/v1","account_store_path":store,
        "accounts":[
            {"id":personal,"name":"Local","prefix":personal,"auth_file":personal_auth,"credential_status":"invalid"},
            {"id":shared,"name":"Shared","prefix":"shared","auth_file":shared_auth,"workspace_shared":true},
        ],
    }).to_string()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "/bin/false",
        root.join("codex/auth.json"),
    )
    .unwrap();
    let state = &server.state;
    assert_eq!(
        super::replacement_account(state, personal),
        Some(shared.clone())
    );
    let personal_lock = quota_refresh_lock(state, personal).unwrap();
    let shared_lock = quota_refresh_lock(state, &shared).unwrap();

    for manual in [false, true] {
        let shared_guard = shared_lock.lock().unwrap();
        let shared_count = Arc::strong_count(&shared_lock);
        let state = Arc::clone(state);
        let (done_tx, done_rx) = mpsc::channel();
        let refresh = std::thread::spawn(move || {
            if manual {
                let _ = super::commands::execute(
                    None,
                    &state,
                    personal,
                    super::commands::QuotaCommand::Refresh,
                );
            } else {
                let _ = super::refresh_account_serialized(&state, personal);
            }
            done_tx.send(()).unwrap();
        });
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        while Arc::strong_count(&shared_lock) == shared_count && !deadline.is_expired() {
            #[expect(
                clippy::disallowed_methods,
                reason = "配置锁占用没有通知接口，以固定间隔轮询"
            )]
            std::thread::sleep(Duration::from_millis(5));
        }
        let queued_for_shared =
            !deadline.is_expired() && Arc::strong_count(&shared_lock) > shared_count;
        let personal_is_free = personal_lock.try_lock().is_ok();
        drop(shared_guard);
        done_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        refresh.join().unwrap();
        assert!(queued_for_shared);
        assert!(
            personal_is_free,
            "the refresh acquired the later lock first"
        );
    }

    assert_eq!(replacing_account_credentials(state, |_| true), Some(true));
    server.shutdown().unwrap();
}
