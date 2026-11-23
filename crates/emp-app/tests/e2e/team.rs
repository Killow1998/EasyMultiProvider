use crate::support::http;
use crate::support::modules::{BuiltModule, Fault};
use crate::support::team::{MODEL, Team, denied, next_event, poll, report, team};
use serde_json::json;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[test]
#[ignore = "needs current and next-version module binaries; see tests/support/modules.rs"]
fn m03_roles_require_enrollment_review_and_enforce_management_permissions() {
    let fixture = Team::start();
    for (device, name, role, members_status, backup_status) in [
        (0, "deployment", "vps_admin", 200, 200),
        (0, "manager", "team_admin", 200, 403),
        (1, "researcher", "member", 403, 403),
    ] {
        fixture.login(device, name, role);
        let emp = &fixture.devices[device];
        team(emp, "list_members", json!({}), members_status);
        let backup = team(emp, "backup", json!({}), backup_status);
        if backup_status == 200 {
            assert_eq!(backup["format"], "emp-team-backup-v1");
            assert!(
                !backup["data"]
                    .as_str()
                    .expect("encrypted backup")
                    .is_empty()
            );
        }
        let promotion = json!({"id":fixture.member,"role":"team_admin",
            "publish":false,"sync":true,"enabled":true});
        let expected = if role == "vps_admin" { 200 } else { 403 };
        team(emp, "member_access", promotion, expected);
        if role == "vps_admin" {
            team(
                emp,
                "member_access",
                json!({"id":fixture.member,"role":"member",
                    "publish":false,"sync":true,"enabled":true}),
                200,
            );
        }
        if role == "member" {
            for action in ["issue_invite", "shared_services", "publish_source"] {
                let denied = team(emp, action, json!({}), 403);
                assert_eq!(denied["error"]["code"], "workspace_permission_required");
            }
        }
    }
    fixture.stop();
}

#[test]
#[ignore = "needs current and next-version module binaries; see tests/support/modules.rs"]
fn m03_two_devices_forward_shared_subscription_and_keep_personal_data_after_uninstall() {
    let fixture = Team::start();
    let native_auth = std::fs::read(fixture.workspaces[1].codex_home.join("auth.json"))
        .expect("personal native credentials");
    let shared = fixture.share();
    let config = report(&fixture.devices[1], "/api/config");
    let account = config["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .find(|row| row["prefix"] == shared.split('/').next().expect("route prefix"))
        .expect("shared account")["id"]
        .as_str()
        .expect("account ID")
        .to_owned();
    let first = fixture.call(1, &shared, "resp_m03_member", &fixture.subscription);
    let call = fixture.local_call(1, &first);
    assert_eq!(call["account_id"], account);
    assert_eq!(call["provider_id"], account);
    assert_eq!(call["session_id"], "resp_m03_member");
    assert_eq!(call["output_tokens"], 3);
    let row = fixture.usage(&fixture.member, 1);
    assert_eq!(row["input_tokens"], 7);
    assert_eq!(row["output_tokens"], 3);
    assert_eq!(row["models"][0]["model"], MODEL);
    assert_eq!(fixture.usage(&fixture.manager, 0)["requests"], 0);
    let first_report = fixture.relay.next_report();
    assert_eq!(first_report.events.len(), 1);
    assert_eq!(first_report.inserted, 1);
    assert_eq!(
        first_report.events[0]["share"],
        account.trim_start_matches("team-")
    );

    fixture.login(0, "researcher", "member");
    assert_eq!(
        team(&fixture.devices[0], "sync", json!({}), 200)["skipped_native"],
        1
    );
    let second = fixture.call(0, MODEL, "resp_m03_other_device", &fixture.subscription);
    fixture.local_call(0, &second);
    let combined = fixture.usage(&fixture.member, 2);
    for emp in &fixture.devices {
        let usage = team(emp, "usage", json!({"period":"lifetime"}), 200);
        assert_eq!(
            usage["rows"].as_array().expect("member-only usage").len(),
            1
        );
        assert_eq!(usage["rows"][0], combined);
    }
    assert_eq!(combined["input_tokens"], 14);
    assert_eq!(combined["output_tokens"], 6);
    let second_report = fixture.relay.next_report();
    assert_eq!(second_report.events.len(), 1);
    assert_eq!(second_report.inserted, 1);
    assert_eq!(
        second_report.events[0]["share"],
        first_report.events[0]["share"]
    );
    assert_ne!(second_report.events[0]["id"], first_report.events[0]["id"]);

    let personal_request =
        fixture.call(1, "personal/model", "resp_m03_personal", &fixture.personal);
    let personal_call = fixture.local_call(1, &personal_request);
    assert_eq!(personal_call["provider_id"], "personal");
    assert!(personal_call["account_id"].is_null());
    let before = report(&fixture.devices[1], "/api/usage?start=0&end=4102444800");
    assert_eq!(before["totals"]["requests"], 2);
    let config = report(&fixture.devices[1], "/api/config");
    let personal = config["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|row| row["id"] == "personal")
        .expect("personal provider")
        .clone();
    let pid = fixture.devices[1].active(&fixture.current);
    fixture.devices[1].change("team", "disable");
    fixture.devices[1].assert_gone(pid);
    fixture.devices[1].change("team", "uninstall");
    assert!(fixture.devices[1].status("team")["installed_version"].is_null());
    let config = report(&fixture.devices[1], "/api/config");
    assert!(
        config["providers"]
            .as_array()
            .expect("providers")
            .contains(&personal)
    );
    assert!(
        !config["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .any(|row| row["id"] == account)
    );
    assert_eq!(
        std::fs::read(fixture.workspaces[1].codex_home.join("auth.json"))
            .expect("personal native credentials after uninstall"),
        native_auth
    );
    let after = report(&fixture.devices[1], "/api/usage?start=0&end=4102444800");
    assert_eq!(after["totals"], before["totals"]);
    assert_eq!(fixture.local_call(1, &personal_request), personal_call);
    fixture.call(
        1,
        "personal/model",
        "resp_m03_personal_after_uninstall",
        &fixture.personal,
    );
    denied(
        fixture.devices[1].post("/api/team/sync", &json!({})),
        404,
        "module_disabled",
    );
    fixture.stop();
}

#[test]
#[ignore = "needs current and next-version module binaries; see tests/support/modules.rs"]
fn m03_offline_completion_retries_unacknowledged_usage_without_double_billing() {
    let mut fixture = Team::start();
    let shared = fixture.share();
    let (mut client, gate) = fixture.stream(&shared, "resp_m03_offline");
    fixture.server.stop();
    gate.finish();
    assert_eq!(
        next_event(&mut client, "response.completed")["response"]["id"],
        "resp_m03_offline"
    );
    let local = poll("offline completion in local ledger", || {
        let usage = report(&fixture.devices[1], "/api/usage?start=0&end=4102444800");
        (usage["totals"]["requests"] == 1).then_some(usage)
    });
    assert_eq!(local["totals"]["requests"], 1);
    denied(
        fixture.devices[1].responses(&json!({"model":shared,"input":"offline admission"})),
        503,
        "workspace_budget_unavailable",
    );
    fixture.subscription.assert_idle();

    fixture.relay.drop_next_report_reply();
    fixture.server.restart();
    team(
        &fixture.devices[1],
        "usage",
        json!({"period":"lifetime"}),
        200,
    );
    let first = fixture.relay.next_report();
    assert_eq!(first.events.len(), 1, "{first:?}");
    assert_eq!(first.inserted, 1, "{first:?}");
    let row = fixture.usage(&fixture.member, 1);
    assert_eq!(row["input_tokens"], 7);
    team(
        &fixture.devices[1],
        "usage",
        json!({"period":"lifetime"}),
        200,
    );
    let replay = fixture.relay.next_report();
    assert_eq!(
        replay.events, first.events,
        "the same durable event must be retried"
    );
    assert_eq!(replay.inserted, 0, "server must deduplicate the replay");
    let final_usage = fixture.usage(&fixture.member, 1);
    for field in ["requests", "input_tokens", "output_tokens", "models"] {
        assert_eq!(final_usage[field], row[field], "replay changed {field}");
    }
    fixture.stop();
}

#[test]
#[ignore = "needs current and next-version module binaries; see tests/support/modules.rs"]
fn m03_budget_and_revocation_block_shared_requests_before_the_upstream() {
    for (sync, enabled, sync_status) in [(false, true, 403), (true, false, 401)] {
        let fixture = Team::start();
        let shared = fixture.share();
        fixture.call(1, &shared, "resp_m03_before_revoke", &fixture.subscription);
        fixture.usage(&fixture.member, 1);
        team(
            &fixture.devices[0],
            "budget",
            json!({"member":fixture.member,"daily_usd_nanos":0}),
            200,
        );
        denied(
            fixture.devices[1].responses(&json!({"model":shared,"input":"over budget"})),
            403,
            "workspace_budget_reached",
        );
        fixture.subscription.assert_idle();
        team(
            &fixture.devices[0],
            "budget",
            json!({"member":fixture.member,"daily_usd_nanos":null}),
            200,
        );
        team(
            &fixture.devices[0],
            "member_access",
            json!({"id":fixture.member,"role":"member",
                "publish":false,"sync":sync,"enabled":enabled}),
            200,
        );
        if !enabled {
            denied(
                fixture.devices[1].responses(&json!({"model":shared,"input":"revoked member"})),
                403,
                "workspace_permission_required",
            );
        }
        let rejected = team(&fixture.devices[1], "sync", json!({}), sync_status);
        assert_eq!(
            rejected["error"]["code"],
            if enabled {
                "workspace_permission_required"
            } else {
                "workspace_login_required"
            }
        );
        let rejected =
            fixture.devices[1].responses(&json!({"model":shared,"input":"revoked route"}));
        assert!(
            matches!(rejected.status, 400 | 403 | 404),
            "revoked route: {}",
            rejected.text()
        );
        let models = fixture.devices[1].codex("GET", "/v1/models", None);
        assert_eq!(models.status, 200, "{}", models.text());
        assert!(
            !models.json()["data"]
                .as_array()
                .expect("models")
                .iter()
                .any(|row| row["id"] == shared)
        );
        fixture.subscription.assert_idle();
        fixture.call(
            1,
            "personal/model",
            "resp_m03_personal_after_revoke",
            &fixture.personal,
        );
        fixture.usage(&fixture.member, 1);
        fixture.stop();
    }
}

#[test]
#[ignore = "needs current and next-version module binaries; see tests/support/modules.rs"]
fn m03_hot_update_waits_for_reporting_and_delivers_each_usage_event_once() {
    let fixture = Team::start();
    let next = BuiltModule::next("team");
    assert_ne!(
        next.version, fixture.current.version,
        "higher Team version required"
    );
    assert_ne!(
        next.digest, fixture.current.digest,
        "distinct Team build required"
    );
    let shared = fixture.share();
    fixture.release.publish(&next, Fault::None);
    let offers = fixture.devices[1].post("/api/modules", &json!({"action":"check_updates"}));
    assert_eq!(offers.status, 200, "{}", offers.text());
    let offers = offers.json();
    let offer = offers["updates"]
        .as_array()
        .expect("updates")
        .iter()
        .find(|row| row["id"] == "team")
        .expect("higher-version Team offer");
    assert_eq!(offer["latest_version"], next.version);
    let old_pid = fixture.devices[1].active(&fixture.current);
    let other_pid = fixture.devices[0].active(&fixture.current);
    let core_pid = fixture.devices[1].pid();
    let (mut client, gate) = fixture.stream(&shared, "resp_m03_during_update");
    let release_receipt = fixture.relay.hold_next_report_reply();
    gate.finish();
    next_event(&mut client, "response.completed");
    let in_flight = fixture.relay.next_report();
    assert_eq!(in_flight.events.len(), 1);
    assert_eq!(in_flight.inserted, 1);

    let port = fixture.devices[1].port;
    let session = fixture.devices[1].session.clone();
    let update = json!({"id":"team","action":"update","revision":offer["revision"]});
    let (started, starting) = mpsc::channel();
    let (finished, result) = mpsc::channel();
    let updater = thread::spawn(move || {
        started.send(()).expect("update started");
        let response = http(
            port,
            "POST",
            "/api/modules",
            &[
                ("X-EMP-Session", &session),
                ("Content-Type", "application/json"),
            ],
            update.to_string().as_bytes(),
        );
        let _ = finished.send(response);
    });
    starting
        .recv_timeout(Duration::from_secs(5))
        .expect("update dispatched");
    assert!(
        matches!(
            result.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "Team update must wait for the held report receipt"
    );
    release_receipt
        .send(())
        .expect("acknowledge real server report");
    let updated = result
        .recv_timeout(Duration::from_secs(30))
        .expect("Team update completed");
    updater.join().expect("update request stopped");
    assert_eq!(updated.status, 200, "{}", updated.text());
    assert_eq!(fixture.devices[1].pid(), core_pid);
    let new_pid = fixture.devices[1].active(&next);
    assert_ne!(new_pid, old_pid);
    fixture.devices[1].assert_gone(old_pid);
    assert_eq!(fixture.devices[0].active(&fixture.current), other_pid);
    fixture.usage(&fixture.member, 1);
    fixture.call(1, &shared, "resp_m03_after_update", &fixture.subscription);
    let after = fixture.relay.next_report();
    assert_eq!(after.events.len(), 1);
    assert_ne!(after.events[0]["id"], in_flight.events[0]["id"]);
    assert_eq!(after.inserted, 1);
    fixture.usage(&fixture.member, 2);
    for event in [&in_flight.events[0], &after.events[0]] {
        fixture.relay.assert_single_delivery(&event["id"]);
    }
    fixture.stop();
}
