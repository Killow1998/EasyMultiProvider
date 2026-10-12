//! Selected exports preserve source state and exclude unchosen credentials/preferences.
use emp_state::migration::select_migration_payload;
use serde_json::{Value, json};

fn fixture() -> Value {
    let account = json!({"id":"a","prefix":"a","name":"Account","auth_file":"private/auth",
        "model_context_windows":{"gpt-test":1000}});
    json!({"schema":"easy-multi-provider-migration","version":1,
        "config":{"accounts":[account],"providers":[{"id":"p","name":"API","api_key_file":"private/key"},{"id":"q"}],
            "models":[{"id":"p/m","provider":"p","upstream_id":"m"},{"id":"q/m","provider":"q","upstream_id":"m"}],
            "catalog_presentations":{"a/gpt-test":{"catalog_alias":"Old"},"q/m":{"catalog_alias":"Unselected"}},
            "catalog_family_presentations":{"gpt-test":{"catalog_alias":"Old"},"unselected":{"catalog_alias":"Other"}}},
        "accounts":[{"metadata":account,"auth":{"tokens":{"access_token":"fixture"}}}],
        "provider_keys":{"p":"selected-key","q":"other-key"}})
}

#[test]
fn selected_provider_export_excludes_other_credentials_without_mutating_source() {
    let source = fixture();
    let before = source.clone();
    let result = select_migration_payload(
        &source,
        &[json!({"id":"p","kind":"provider","name":"Edited",
        "models":[{"id":"p/m","name":"Model","selected":true,"context_window":2048}]})],
        true,
        true,
    )
    .unwrap();
    assert_eq!(source, before);
    assert_eq!(result["provider_keys"], json!({"p":"selected-key"}));
    assert_eq!(result["accounts"], json!([]));
    assert_eq!(result["config"]["catalog_presentations"], json!({}));
    assert!(
        result["config"]["providers"][0]
            .get("api_key_file")
            .is_none()
    );
    assert_eq!(result["config"]["models"][0]["context_window"], 2048);
}

#[test]
fn subscription_export_options_keep_visibility_but_omit_context_and_aliases() {
    let result=select_migration_payload(&fixture(), &[json!({"id":"a","kind":"account","name":"Renamed",
        "models":[{"id":"gpt-test","name":"Alias","original_name":"GPT","context_window":2000,"selected":false}]})],false,false).unwrap();
    let metadata = &result["accounts"][0]["metadata"];
    assert!(metadata.get("auth_file").is_none());
    assert!(metadata.get("model_context_windows").is_none());
    assert_eq!(metadata["hidden_models"], json!(["gpt-test"]));
    assert!(
        result["config"]["catalog_presentations"]["a/gpt-test"]
            .get("catalog_alias")
            .is_none()
    );
    assert_eq!(result["model_previews"]["a"][0]["name"], "GPT");
    assert!(
        result["model_previews"]["a"][0]
            .get("context_window")
            .is_none()
    );
}

#[test]
fn native_export_remaps_selected_routes_and_rejects_reserved_identity() {
    let mut source = fixture();
    source["accounts"][0]["metadata"]["id"] = json!("native-login");
    source["accounts"][0]["metadata"]["prefix"] = json!("native-login");
    source["accounts"][0]["metadata"]["name"] = json!("Native login");
    source["config"]["accounts"] = json!([source["accounts"][0]["metadata"]]);
    source["config"]["catalog_presentations"] = json!({"gpt-test":{"catalog_alias":"Chosen"}});
    let mut choice = json!({"id":"@native","kind":"account","export_id":"a9z2","models":[{"id":"gpt-test","selected":true,"name":"Chosen"}]});
    let result = select_migration_payload(&source, &[choice.clone()], true, true).unwrap();
    assert_eq!(result["accounts"][0]["metadata"]["id"], "a9z2");
    assert_eq!(result["accounts"][0]["metadata"]["prefix"], "a9z2");
    assert!(
        result["config"]["catalog_presentations"]
            .get("a9z2/gpt-test")
            .is_some()
    );
    assert!(
        result["config"]["catalog_presentations"]
            .get("gpt-test")
            .is_none()
    );
    choice["export_id"] = json!("q");
    assert!(select_migration_payload(&source, &[choice], true, true).is_err());
}
