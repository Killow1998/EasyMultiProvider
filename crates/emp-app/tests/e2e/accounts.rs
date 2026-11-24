use crate::support::{Emp, Upstream, Workspace, import_account};
use serde_json::json;

fn assert_removed(emp: &Emp, workspace: &Workspace, upstream: &Upstream) {
    let accounts = emp.get("/api/accounts");
    assert_eq!(accounts.status, 200, "{}", accounts.text());
    let accounts = accounts.json();
    let ids: Vec<_> = accounts["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .map(|account| account["id"].as_str().expect("account id"))
        .collect();
    assert_eq!(ids, ["retained"]);
    assert!(
        !workspace
            .root
            .join("accounts/removed/auth.json.enc")
            .exists()
    );
    let rejected = emp.responses(&json!({"model":"removed/model", "input":"deleted route"}));
    assert_ne!(rejected.status, 200, "{}", rejected.text());
    upstream.assert_idle();
    let response = emp.responses(&json!({"model":"retained/model", "input":"retained route"}));
    assert_eq!(response.status, 200, "{}", response.text());
    let request = upstream.next_request();
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(request.headers["authorization"], "Bearer retained-token");
    assert_eq!(request.headers["chatgpt-account-id"], "retained-owner");
    upstream.assert_idle();
}

#[test]
fn deleted_accounts_stay_absent_after_restart_and_other_credentials_remain_usable() {
    let upstream = Upstream::start();
    upstream.reply_json(&json!({"id":"resp_account", "object":"response",
        "status":"completed", "model":"model", "output":[]}));
    let workspace = Workspace::new();
    workspace.write_config(json!({"codex_base_url":upstream.base_url(),
        "account_store_path":workspace.root.join("accounts")}));
    let emp = workspace.start_without_path_cli();
    for (id, token, owner) in [
        ("removed", "removed-token", "removed-owner"),
        ("retained", "retained-token", "retained-owner"),
    ] {
        import_account(&emp, id, token, owner);
        let response =
            emp.responses(&json!({"model":format!("{id}/model"), "input":"account fixture"}));
        assert_eq!(response.status, 200, "{}", response.text());
        let request = upstream.next_forwarded();
        assert_eq!(request.headers["authorization"], format!("Bearer {token}"));
        assert_eq!(request.headers["chatgpt-account-id"], owner);
    }
    let deleted = emp.api("DELETE", "/api/accounts/removed", None);
    assert_eq!(deleted.status, 200, "{}", deleted.text());
    assert_removed(&emp, &workspace, &upstream);
    emp.stop();
    let restarted = workspace.start_without_path_cli();
    assert_removed(&restarted, &workspace, &upstream);
}
