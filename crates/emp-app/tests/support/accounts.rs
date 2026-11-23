use serde_json::json;

use super::Emp;

pub fn import_account(emp: &Emp, id: &str, token: &str, owner: &str) {
    let response = emp.post(
        "/api/accounts/import",
        &json!({"id": id, "name": id, "prefix": id,
            "auth_json": {"tokens": {"access_token": token,
                "refresh_token": "fixture-refresh", "account_id": owner}}}),
    );
    assert_eq!(response.status, 200, "{}", response.text());
}
