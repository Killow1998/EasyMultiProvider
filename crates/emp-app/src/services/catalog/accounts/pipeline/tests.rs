use super::*;

#[test]
fn known_models_do_not_cross_accounts_or_override_explicit_upstream_updates() {
    let source = RefreshSource {
        id: "@native".into(),
        path: PathBuf::from("native.json"),
        base: "https://catalog.example/v1".into(),
        headers: BTreeMap::new(),
        owner: "owner-a".into(),
        credential_fingerprint: String::new(),
        client_version: "0.159.2".into(),
        generation: 0,
        native: true,
        worker_eligible: false,
    };
    let mut previous = json!({"account_owner":"owner-a", "base_url":source.base,
        "models":[{"slug":"known"}, {"slug":"disabled","supported_in_api":true}]});
    for (owner, base, retain) in [
        ("owner-a", "https://catalog.example/v1", true),
        ("owner-b", "https://catalog.example/v1", false),
        ("owner-a", "https://other.example/v1", false),
    ] {
        previous["account_owner"] = json!(owner);
        previous["base_url"] = json!(base);
        let mut fresh = json!({"models":[{"slug":"disabled", "supported_in_api":false}]});
        retain_known_models(&mut fresh, &previous, &source);
        assert_eq!(
            fresh["models"].as_array().unwrap().len(),
            if retain { 2 } else { 1 }
        );
        assert_eq!(fresh["models"][0]["supported_in_api"], false);
        if retain {
            assert_eq!(fresh["models"][1]["_emp_catalog_retained"], true);
        }
        let mut reappeared = json!({"models":[{"slug":"known", "visibility":"list"}]});
        retain_known_models(&mut reappeared, &fresh, &source);
        assert!(
            reappeared["models"][0]
                .get("_emp_catalog_retained")
                .is_none()
        );
    }
}
