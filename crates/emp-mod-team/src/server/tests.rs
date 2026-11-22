use super::*;

const PASSWORD: &str = "workspace-fixture-password";
const DEVICE: &str = "12345678901234561234567890123456";

fn workspace() -> Workspace {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    initialize_schema(&db).unwrap();
    Workspace {
        db,
        id: "fixture".into(),
        name: "Lab".into(),
        key: FernetKey::from_encoded(&URL_SAFE.encode([0; 32])).unwrap(),
    }
}

fn post(
    workspace: &mut Workspace,
    action: &str,
    token: Option<&str>,
    body: Value,
) -> Result<Value> {
    workspace.request(
        &Method::Post,
        &format!("/workspace/v1/{action}"),
        token,
        &serde_json::to_vec(&body).unwrap(),
    )
}

fn sign_in(workspace: &mut Workspace, name: &str, password: &str) -> Result<Value> {
    post(
        workspace,
        "login",
        None,
        json!({"name":name,"password":password,"device":DEVICE}),
    )
}

fn enrollment_code(workspace: &mut Workspace, token: &str) -> Value {
    post(workspace, "join-code", Some(token), json!({})).unwrap()["code"].clone()
}

fn assert_failure(result: Result<Value>, status: u16, code: &str) {
    let Failure(actual_status, actual_code) = result.unwrap_err();
    assert_eq!((actual_status, actual_code), (status, code));
}

#[test]
fn removal_archives_members_and_releases_the_login_for_a_new_application() {
    let mut workspace = workspace();
    workspace
        .db
        .execute("ALTER TABLE members DROP COLUMN archived_name", [])
        .unwrap();
    initialize_schema(&workspace.db).unwrap();
    let admin_id = accounts::create(
        &workspace.db,
        "admin",
        PASSWORD,
        Role::TeamAdmin,
        true,
        true,
    )
    .unwrap();
    let member =
        accounts::create(&workspace.db, "member", PASSWORD, Role::Member, true, true).unwrap();
    let admin_login = sign_in(&mut workspace, "admin", PASSWORD).unwrap();
    let admin = admin_login["token"].as_str();
    let member_login = sign_in(&mut workspace, "member", PASSWORD).unwrap();
    let token = member_login["token"].as_str();
    assert_failure(
        post(
            &mut workspace,
            "remove-member",
            admin,
            json!({"id":admin_id}),
        ),
        403,
        "workspace_permission_required",
    );
    workspace.db.execute(
        "INSERT INTO shares(id,owner,source,version,encrypted) VALUES ('share',?,'fixture',1,X'AB')",
        [&member],
    ).unwrap();
    assert_eq!(
        post(&mut workspace, "remove-member", admin, json!({"id":member})).unwrap(),
        json!({"ok":true})
    );
    assert_failure(
        workspace.request(&Method::Get, "/workspace/v1/status", token, b""),
        401,
        "workspace_authentication_required",
    );
    assert_failure(
        sign_in(&mut workspace, "member", PASSWORD),
        401,
        "workspace_login_failed",
    );
    let removed_share: (i64, bool) = workspace
        .db
        .query_row(
            "SELECT length(encrypted), removed_at IS NOT NULL FROM shares WHERE id='share'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(removed_share, (0, true));
    let code = enrollment_code(&mut workspace, admin.unwrap());
    post(
        &mut workspace,
        "join",
        None,
        json!({"nickname":"Member","name":"member","password":PASSWORD,"code":code}),
    )
    .unwrap();
    let roster = workspace
        .request(&Method::Get, "/workspace/v1/members", admin, b"")
        .unwrap();
    let pending = roster["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "member")
        .unwrap();
    assert_ne!(pending["id"], member);
    assert_eq!(pending["pending"], true);
    assert_failure(
        post(
            &mut workspace,
            "remove-member",
            admin,
            json!({"id":pending["id"]}),
        ),
        409,
        "workspace_member_pending",
    );
}

#[test]
fn weekly_spend_uses_completion_time_and_prefers_actual_cost() {
    let workspace = workspace();
    let member =
        accounts::create(&workspace.db, "member", PASSWORD, Role::Member, true, true).unwrap();
    let start = 1_000_000;
    let end = start + 3600;
    let used = || {
        workspace
            .weekly_usd_status(&member, start, end)
            .unwrap()
            .used
    };
    workspace.db.execute(
        "INSERT INTO shares(id,owner,source,version,encrypted) VALUES ('share',?,'fixture',1,X'')",
        [&member],
    ).unwrap();
    let report = |id, device, completed, estimate: Option<i64>, actual: Option<i64>| {
        workspace
            .db
            .execute(
                "INSERT INTO usage VALUES (?, ?, ?, 'share', 'fixture', ?, 1, NULL, 1, ?, ?)",
                params![&member, device, id, completed, estimate, actual],
            )
            .unwrap();
    };
    report("previous", "one", start - 1, Some(200), Some(50));
    assert_eq!(used(), Some(0));
    report("unknown", "two", start, None, None);
    assert_eq!(used(), None);
    report("known", "two", start + 1, Some(9), Some(3));
    report("late", "one", start, Some(4), None);
    report("future", "one", end + 1, Some(30), None);
    assert_eq!(used(), Some(7));
}

#[test]
fn refreshing_a_subscription_carries_only_current_approval_for_the_same_account() {
    let mut workspace = workspace();
    for (name, role, publish) in [
        ("admin", Role::TeamAdmin, false),
        ("publisher", Role::Member, true),
        ("reader", Role::Member, false),
    ] {
        accounts::create(&workspace.db, name, PASSWORD, role, publish, true).unwrap();
    }
    let admin = sign_in(&mut workspace, "admin", PASSWORD).unwrap();
    let publisher = sign_in(&mut workspace, "publisher", PASSWORD).unwrap();
    let reader = sign_in(&mut workspace, "reader", PASSWORD).unwrap();
    let admin = admin["token"].as_str().unwrap();
    let publisher = publisher["token"].as_str().unwrap();
    let reader = reader["token"].as_str().unwrap();
    let source = emp_modules::workspace::NATIVE_SHARE;
    let expiry = now() + 3600;
    let upload = |workspace: &mut Workspace, version: i64, account: &str, user: Option<&str>| {
        let token = user.map_or_else(
            || format!("fixture-{version}"),
            |user| {
                let claims = json!({"https://api.openai.com/auth":{
                    "chatgpt_account_id":account,"chatgpt_user_id":user,
                }});
                format!(
                    "fixture.{}.signature",
                    URL_SAFE.encode(serde_json::to_vec(&claims).unwrap())
                )
            },
        );
        post(
            workspace,
            "publish",
            Some(publisher),
            json!({"source":source,"expected_version":version,"payload":{
                "provider":{"id":source,"name":"Native","auth_mode":"codex_subscription"},
                "models":[],"subscription":{"account_id":account,
                    "access_token":token,"expires_at":expiry+version}
            }}),
        )
        .unwrap()
    };
    let publish = |workspace: &mut Workspace, version, account: &str| {
        upload(workspace, version, account, None)
    };
    let sync = |workspace: &mut Workspace| {
        workspace
            .request(&Method::Get, "/workspace/v1/shares", Some(reader), b"")
            .unwrap()["shares"]
            .as_array()
            .unwrap()
            .clone()
    };
    let first = publish(&mut workspace, 0, "first-account");
    let share = &first["id"];
    let review = |workspace: &mut Workspace, version, shared| {
        post(
            workspace,
            "review-service",
            Some(admin),
            json!({"id":share,"version":version,"shared":shared}),
        )
        .unwrap()
    };
    assert_eq!(first["review_required"], true);
    assert!(sync(&mut workspace).is_empty());
    assert_eq!(
        publish(&mut workspace, 1, "first-account")["review_required"],
        true
    );
    assert!(sync(&mut workspace).is_empty());
    review(&mut workspace, 2, true);
    let refreshed = publish(&mut workspace, 2, "first-account");
    assert_eq!(
        refreshed,
        json!({"id":share,"version":3,"review_required":false})
    );
    assert_eq!(publish(&mut workspace, 2, "first-account"), refreshed);
    let synced = sync(&mut workspace);
    assert_eq!(synced.len(), 1);
    assert_eq!(synced[0]["version"], 3);
    assert_eq!(
        synced[0]["payload"]["subscription"]["access_token"],
        "fixture-2"
    );
    let audit = post(&mut workspace, "audit", Some(admin), json!({})).unwrap();
    let events = audit["entries"].as_array().unwrap();
    assert_eq!(events[0]["actor"], "publisher");
    assert_eq!(events[0]["kind"], "share_refreshed");
    assert_eq!(
        events[0]["details"],
        json!({"share":share,"version":3,"review_required":false})
    );
    assert_eq!(
        events
            .iter()
            .filter(|entry| entry["details"]["version"] == 3)
            .count(),
        1
    );

    assert_eq!(
        publish(&mut workspace, 3, "other-account")["review_required"],
        true
    );
    assert!(sync(&mut workspace).is_empty());
    review(&mut workspace, 4, true);
    review(&mut workspace, 4, false);
    assert_eq!(
        publish(&mut workspace, 4, "other-account")["review_required"],
        true
    );
    assert!(sync(&mut workspace).is_empty());

    review(&mut workspace, 5, true);
    workspace
        .db
        .execute("UPDATE shares SET encrypted=x'00'", [])
        .unwrap();
    assert_eq!(
        publish(&mut workspace, 5, "other-account")["review_required"],
        true
    );
    assert!(sync(&mut workspace).is_empty());

    assert_eq!(
        upload(&mut workspace, 6, "business", Some("first-user"))["review_required"],
        true
    );
    review(&mut workspace, 7, true);
    assert_eq!(
        upload(&mut workspace, 7, "business", Some("first-user"))["review_required"],
        false
    );
    assert_eq!(sync(&mut workspace).len(), 1);
    assert_eq!(
        upload(&mut workspace, 8, "business", Some("other-user"))["review_required"],
        true
    );
    assert!(sync(&mut workspace).is_empty());
}

#[test]
fn removing_a_shared_subscription_discards_credentials_and_accepts_late_usage() {
    let mut workspace = workspace();
    workspace
        .db
        .execute("ALTER TABLE shares DROP COLUMN removed_at", [])
        .unwrap();
    initialize_schema(&workspace.db).unwrap();
    for (name, role, publish) in [
        ("admin", Role::TeamAdmin, false),
        ("publisher", Role::Member, true),
        ("reader", Role::Member, false),
    ] {
        accounts::create(&workspace.db, name, PASSWORD, role, publish, true).unwrap();
    }
    let admin_login = sign_in(&mut workspace, "admin", PASSWORD).unwrap();
    let publisher_login = sign_in(&mut workspace, "publisher", PASSWORD).unwrap();
    let reader_login = sign_in(&mut workspace, "reader", PASSWORD).unwrap();
    let admin = admin_login["token"].as_str();
    let publisher = publisher_login["token"].as_str();
    let reader = reader_login["token"].as_str();
    let source = emp_modules::workspace::NATIVE_SHARE;
    let payload = json!({
        "provider":{"id":source,"name":"Native","auth_mode":"codex_subscription"},
        "models":[],"subscription":{"account_id":"fixture","access_token":"fixture-secret","expires_at":now()+3600}
    });
    let upload = json!({"source":source,"expected_version":0,"payload":payload});
    let published = post(&mut workspace, "publish", publisher, upload.clone()).unwrap();
    let id = published["id"].as_str().unwrap();
    let removal = json!({"id":id,"version":1});
    post(
        &mut workspace,
        "review-service",
        admin,
        json!({"id":id,"version":1,"shared":true}),
    )
    .unwrap();
    // An expired VPS copy cannot be downloaded, but Native can use renewed
    // local credentials for the same approved account and still report usage.
    let mut expired = payload.clone();
    expired["subscription"]["expires_at"] = json!(now() - 1);
    let encrypted = encode_vault(&workspace.key, &serde_json::to_vec(&expired).unwrap());
    workspace
        .db
        .execute(
            "UPDATE shares SET encrypted=? WHERE id=?",
            params![encrypted, id],
        )
        .unwrap();
    let status = workspace
        .request(&Method::Get, "/workspace/v1/status", reader, b"")
        .unwrap();
    assert_eq!(status["native_usage"][0]["share"], id);
    assert!(
        workspace
            .request(&Method::Get, "/workspace/v1/shares", reader, b"")
            .unwrap()["shares"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_failure(
        post(&mut workspace, "remove-service", reader, removal.clone()),
        403,
        "workspace_permission_required",
    );
    assert_failure(
        post(
            &mut workspace,
            "remove-service",
            admin,
            json!({"id":id,"version":0}),
        ),
        409,
        "workspace_share_version_conflict",
    );
    assert_eq!(
        post(&mut workspace, "remove-service", admin, removal).unwrap(),
        json!({"ok":true})
    );
    let stored: (i64, i64) = workspace
        .db
        .query_row(
            "SELECT length(encrypted),removed_at FROM shares WHERE id=?",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored.0, 0);
    assert!(stored.1 > 0);
    for (endpoint, token, field) in [
        ("shares", reader, "shares"),
        ("status", reader, "native_usage"),
        ("shared-services", admin, "services"),
        ("subscriptions", publisher, "subscriptions"),
    ] {
        let result = workspace
            .request(
                &Method::Get,
                &format!("/workspace/v1/{endpoint}"),
                token,
                b"",
            )
            .unwrap();
        assert_eq!(result[field], json!([]), "{result}");
    }
    let receipt = post(
        &mut workspace,
        "report",
        reader,
        json!({"events":[{
            "id":"queued-before-removal","share":id,"model":"gpt-6.2-sol","finished_at":now(),
            "input_tokens":8,"output_tokens":2,"estimated_usd_nanos":100
        }]}),
    )
    .unwrap();
    assert_eq!(receipt["inserted"], 1);
    let audit = post(&mut workspace, "audit", admin, json!({})).unwrap();
    assert_eq!(audit["entries"][0]["kind"], "share_removed");
    assert_failure(
        post(&mut workspace, "publish", publisher, upload),
        409,
        "workspace_share_version_conflict",
    );
    let renewed = post(
        &mut workspace,
        "publish",
        publisher,
        json!({
            "source":source,"expected_version":2,"payload":payload
        }),
    )
    .unwrap();
    assert_eq!(renewed, json!({"id":id,"version":3,"review_required":true}));
}

#[test]
fn rejecting_enrollment_releases_name_and_preserves_audit() {
    let mut workspace = workspace();
    let admin = accounts::create(
        &workspace.db,
        "admin",
        PASSWORD,
        Role::TeamAdmin,
        true,
        true,
    )
    .unwrap();
    let operator = sign_in(&mut workspace, "admin", PASSWORD).unwrap();
    let token = operator["token"].as_str().unwrap();
    let code = enrollment_code(&mut workspace, token);
    let application =
        json!({"name":"researcher","nickname":"研究员","password":PASSWORD,"code":code});
    assert_eq!(
        post(&mut workspace, "join", None, application.clone()).unwrap(),
        json!({"pending":true})
    );
    assert_failure(
        sign_in(&mut workspace, "researcher", PASSWORD),
        403,
        "workspace_join_pending",
    );
    let roster = workspace
        .request(&Method::Get, "/workspace/v1/members", Some(token), b"")
        .unwrap();
    let applicant = roster["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["pending"] == true)
        .unwrap();
    let id = applicant["id"].clone();
    post(
        &mut workspace,
        "member-tiers",
        Some(token),
        json!({"id":id,"fast":true,"ultrafast":false}),
    )
    .unwrap();

    assert_eq!(
        post(
            &mut workspace,
            "reject-member",
            Some(token),
            json!({"id":id})
        )
        .unwrap(),
        json!({"ok":true})
    );
    assert_failure(
        sign_in(&mut workspace, "researcher", "incorrect password"),
        401,
        "workspace_login_failed",
    );
    for _ in 0..2 {
        assert_failure(
            sign_in(&mut workspace, "researcher", PASSWORD),
            403,
            "workspace_join_rejected",
        );
    }
    let roster = workspace
        .request(&Method::Get, "/workspace/v1/members", Some(token), b"")
        .unwrap();
    assert_eq!(roster["members"].as_array().unwrap().len(), 1);
    let audit = post(&mut workspace, "audit", Some(token), json!({})).unwrap();
    for (entry, kind) in audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .zip(["member_rejected", "member_tiers"])
    {
        assert_eq!(entry["kind"], kind);
        assert_eq!(entry["actor"], "admin");
        assert_eq!(entry["target"], "研究员");
        assert_eq!(entry["target_login"], "researcher");
        assert!(entry["details"].get("former_target").is_none());
    }
    assert_eq!(audit["entries"].as_array().unwrap().len(), 2);

    workspace
        .db
        .execute("UPDATE rejected_enrollments SET expires=0", [])
        .unwrap();
    assert_failure(
        sign_in(&mut workspace, "researcher", PASSWORD),
        401,
        "workspace_login_failed",
    );
    workspace
        .db
        .execute("UPDATE join_invites SET expires=0 WHERE issuer=?", [&admin])
        .unwrap();
    let mut application = application;
    application["password"] = json!("different-fixture-password");
    application["code"] = enrollment_code(&mut workspace, token);
    assert_eq!(
        post(&mut workspace, "join", None, application).unwrap(),
        json!({"pending":true})
    );
    let roster = workspace
        .request(&Method::Get, "/workspace/v1/members", Some(token), b"")
        .unwrap();
    let applicant = roster["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["pending"] == true)
        .unwrap();
    assert_ne!(applicant["id"], id);
    assert_eq!(
        workspace
            .db
            .query_row("SELECT COUNT(*) FROM rejected_enrollments", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    assert_failure(
        sign_in(&mut workspace, "researcher", "different-fixture-password"),
        403,
        "workspace_join_pending",
    );
    assert_failure(
        sign_in(&mut workspace, "researcher", PASSWORD),
        401,
        "workspace_login_failed",
    );
}

#[test]
fn rejection_requires_management_permission_and_a_pending_application() {
    let mut workspace = workspace();
    let vps = accounts::create(&workspace.db, "vps", PASSWORD, Role::VpsAdmin, true, true).unwrap();
    let admin = accounts::create(
        &workspace.db,
        "admin",
        PASSWORD,
        Role::TeamAdmin,
        true,
        true,
    )
    .unwrap();
    let member =
        accounts::create(&workspace.db, "member", PASSWORD, Role::Member, false, true).unwrap();
    let operator = sign_in(&mut workspace, "admin", PASSWORD).unwrap();
    let token = operator["token"].as_str().unwrap();
    let vps_operator = sign_in(&mut workspace, "vps", PASSWORD).unwrap();
    let vps_token = vps_operator["token"].as_str().unwrap();
    let reader = sign_in(&mut workspace, "member", PASSWORD).unwrap();
    let reader_token = reader["token"].as_str().unwrap();

    for (caller, target) in [(reader_token, &member), (token, &vps), (token, &admin)] {
        assert_failure(
            post(
                &mut workspace,
                "reject-member",
                Some(caller),
                json!({"id":target}),
            ),
            403,
            "workspace_permission_required",
        );
    }
    for (caller, target) in [(token, &member), (vps_token, &admin)] {
        assert_failure(
            post(
                &mut workspace,
                "reject-member",
                Some(caller),
                json!({"id":target}),
            ),
            409,
            "workspace_member_not_pending",
        );
    }
    assert_failure(
        post(
            &mut workspace,
            "reject-member",
            Some(token),
            json!({"id":"invalid"}),
        ),
        400,
        "invalid_workspace_request",
    );
    assert_failure(
        post(
            &mut workspace,
            "reject-member",
            Some(token),
            json!({"id":DEVICE}),
        ),
        404,
        "workspace_member_not_found",
    );

    let code = enrollment_code(&mut workspace, vps_token);
    post(
        &mut workspace,
        "join",
        None,
        json!({"name":"applicant","nickname":"Researcher","password":PASSWORD,"code":code}),
    )
    .unwrap();
    let roster = workspace
        .request(&Method::Get, "/workspace/v1/members", Some(vps_token), b"")
        .unwrap();
    let applicant = roster["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["pending"] == true)
        .unwrap();
    assert_failure(
        post(
            &mut workspace,
            "reject-member",
            Some(reader_token),
            json!({"id":applicant["id"]}),
        ),
        403,
        "workspace_permission_required",
    );
    assert_eq!(
        post(
            &mut workspace,
            "reject-member",
            Some(vps_token),
            json!({"id":applicant["id"]})
        )
        .unwrap(),
        json!({"ok":true})
    );
}
