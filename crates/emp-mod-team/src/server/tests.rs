use super::*;

const PASSWORD: &str = "workspace-fixture-password";
const DEVICE: &str = "12345678901234561234567890123456";

fn workspace() -> Workspace {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    db.execute_batch(SCHEMA).unwrap();
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
