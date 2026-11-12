use emp_state::config::{
    client_catalog, client_route_aliases, resolve_client_model, validate_service_names,
};
use emp_state::{merge_web_update, normalize_configuration};
use serde_json::json;

#[test]
fn versioned_models_keep_route_and_family_display_settings() {
    let model = "claude-sonnet-4@20250514";
    let config = normalize_configuration(Some(&json!({
        "providers":[{"id":"vertex","base_url":"https://example.invalid/v1"}],
        "models":[{"id":format!("vertex/{model}"),"provider":"vertex","upstream_id":model}]
    })))
    .unwrap();
    let mut edited = config.clone();
    edited["catalog_family_presentations"][model] = json!({"catalog_alias":"My Claude"});
    edited["catalog_presentations"][format!("vertex/{model}")] =
        json!({"catalog_alias":"My route"});
    let saved = merge_web_update(&config, &edited).unwrap();
    assert_eq!(
        saved["catalog_family_presentations"][model]["catalog_alias"],
        "My Claude"
    );
    assert_eq!(
        saved["catalog_presentations"][format!("vertex/{model}")]["catalog_alias"],
        "My route"
    );
}

#[test]
fn imported_or_updated_historical_names_cannot_break_an_existing_route() {
    let current = json!({"accounts":[{"id":"subscription","name":"Subscription","prefix":"other"}],"providers":[
        {"id":"a","name":"First","route_names":["Old"]},
        {"id":"b","name":"Second"}
    ]});
    let mut candidate = current.clone();
    candidate["providers"][1]["route_names"] = json!(["Old"]);
    assert_eq!(
        validate_service_names(&current, &candidate),
        Err("core.service_name_already_in_use")
    );
    candidate["providers"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"c","name":"Third","route_names":["Old"]}));
    candidate["providers"][1]["route_names"] = json!([]);
    assert_eq!(
        validate_service_names(&current, &candidate),
        Err("core.service_name_already_in_use")
    );
    candidate["providers"][2]["route_names"] = json!(["Unused"]);
    assert!(validate_service_names(&current, &candidate).is_ok());
    candidate["accounts"][0]["prefix"] = json!("Old");
    assert_eq!(
        validate_service_names(&current, &candidate),
        Err("core.service_name_already_in_use")
    );
}

#[test]
fn new_and_renamed_services_reject_slashes_without_blocking_legacy_configuration() {
    let legacy = json!({"providers":[{"id":"api","name":"Old/Name"}],"accounts":[]});
    assert!(validate_service_names(&legacy, &legacy).is_ok());
    let mut changed = legacy.clone();
    changed["providers"][0]["name"] = "Other/Name".into();
    assert_eq!(
        validate_service_names(&legacy, &changed),
        Err("core.service_name_contains_slash")
    );
    changed["providers"][0]["name"] = "New Name".into();
    assert!(validate_service_names(&legacy, &changed).is_ok());
    let empty = json!({"providers":[],"accounts":[]});
    assert_eq!(
        validate_service_names(&empty, &legacy),
        Err("core.service_name_contains_slash")
    );
    let account =
        json!({"providers":[],"accounts":[{"id":"a","prefix":"a","name":"Work/Personal"}]});
    assert_eq!(
        validate_service_names(&empty, &account),
        Err("core.service_name_contains_slash")
    );
}

#[test]
fn subscription_name_changes_preserve_old_conversation_routes() {
    let config = normalize_configuration(Some(&json!({
        "accounts":[{"id":"account-1","prefix":"ho91","name":"工作","auth_file":"auth.json"}]
    })))
    .unwrap();
    let catalog =
        json!({"models":[{"slug":"ho91/gpt-6.1-sol","availability_nux":{"message":"Try it"}}]});
    assert_eq!(
        client_catalog(&config, catalog.clone())["models"][0]["slug"],
        "工作/gpt-6.1-sol"
    );
    assert_eq!(
        resolve_client_model(&config, "工作/gpt-6.1-sol").as_deref(),
        Some("ho91/gpt-6.1-sol")
    );
    let mut changed = config.clone();
    changed["accounts"][0]["name"] = "家庭".into();
    let changed = merge_web_update(&config, &changed).unwrap();
    assert_eq!(changed["accounts"][0]["route_names"], json!(["工作"]));
    assert_eq!(
        resolve_client_model(&changed, "工作/gpt-6.1-sol").as_deref(),
        Some("ho91/gpt-6.1-sol")
    );
    assert_eq!(
        resolve_client_model(&changed, "家庭/gpt-6.1-sol").as_deref(),
        Some("ho91/gpt-6.1-sol")
    );
    let visible = client_catalog(&changed, catalog);
    assert_eq!(visible["models"][0]["slug"], "家庭/gpt-6.1-sol");
    assert_eq!(
        visible["models"][0]["availability_nux"]["message"],
        "Try it"
    );

    let mut reused = changed.clone();
    reused["accounts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"other","prefix":"other","name":"工作"}));
    assert!(validate_service_names(&changed, &reused).is_err());
    reused["accounts"][1]["name"] = "家庭".into();
    assert!(validate_service_names(&changed, &reused).is_err());
    reused["accounts"][1]["name"] = "同事".into();
    assert!(validate_service_names(&changed, &reused).is_ok());
    reused["providers"] = json!([{"id":"api","name":" 家庭 "}]);
    assert!(validate_service_names(&changed, &reused).is_err());
    reused["providers"][0]["name"] = "工作".into();
    assert!(validate_service_names(&changed, &reused).is_err());
}

#[test]
fn existing_duplicate_names_do_not_change_identity_or_block_unrelated_edits() {
    let config = json!({"accounts":[
        {"id":"a","prefix":"account-a","name":"same"},
        {"id":"b","prefix":"account-b","name":"same"}
    ],"providers":[]});
    let catalog = json!({"models":[{"slug":"account-a/gpt"},{"slug":"account-b/gpt"}]});
    assert_eq!(client_catalog(&config, catalog.clone()), catalog);
    assert!(resolve_client_model(&config, "same/gpt").is_none());
    assert!(validate_service_names(&config, &config).is_ok());
}

#[test]
fn readable_catalog_names_preserve_canonical_routes_and_old_names_after_rename() {
    let config = normalize_configuration(Some(&json!({
        "providers":[{"id":"service-1234abcd","name":"床","base_url":"https://example.invalid/v1"}],
        "models":[{"id":"service-1234abcd/vendor/deepseek","provider":"service-1234abcd","upstream_id":"vendor/deepseek"}]
    }))).unwrap();
    let route = "service-1234abcd/vendor/deepseek";
    let catalog = json!({"models":[{"slug":route},{"slug":"gpt-6.1-sol"}]});
    assert_eq!(
        client_catalog(&config, catalog.clone())["models"][0]["slug"],
        "床/vendor/deepseek"
    );
    assert_eq!(client_route_aliases(&config)["床/vendor/deepseek"], route);
    let mut renamed = config.clone();
    renamed["providers"][0]["name"] = "新服务".into();
    let renamed = merge_web_update(&config, &renamed).unwrap();
    let reloaded = normalize_configuration(Some(&renamed)).unwrap();
    assert_eq!(reloaded["models"][0]["id"], route);
    assert_eq!(client_route_aliases(&reloaded)["床/vendor/deepseek"], route);
    assert_eq!(
        client_route_aliases(&reloaded)["新服务/vendor/deepseek"],
        route
    );
    assert_eq!(
        client_catalog(&reloaded, catalog)["models"][0]["slug"],
        "新服务/vendor/deepseek"
    );
}

#[test]
fn duplicate_labels_and_subscription_prefixes_never_redirect_to_another_source() {
    let config = json!({"providers":[{"id":"a","name":"same"},{"id":"b","name":"same"},{"id":"c","name":"other"}],
        "models":[{"id":"a/model","provider":"a"},{"id":"b/model","provider":"b"},{"id":"c/model","provider":"c"}],
        "accounts":[{"prefix":"other"}],"model_groups":[{"id":"auto/model"}]});
    let aliases = client_route_aliases(&config);
    assert!(!aliases.contains_key("same/model"));
    assert!(!aliases.contains_key("other/model"));
    let catalog = json!({"models":[{"slug":"a/model"},{"slug":"b/model"},{"slug":"c/model"},{"slug":"auto/model"}]});
    assert_eq!(client_catalog(&config, catalog.clone()), catalog);
}

#[test]
fn aggregate_route_names_remain_readable_and_existing_sessions_survive_changes() {
    let config = normalize_configuration(Some(&json!({
        "catalog_family_presentations":{"gpt-6.1-sol":{"catalog_alias":"⭐"}},
        "model_groups":[{"id":"auto/stable", "name_prefix":"auto222", "policy":"priority",
            "sources":["gpt-6.1-sol", "zhou/gpt-6.1-sol"]}]
    })))
    .unwrap();
    let route = "auto/stable";
    let catalog = json!({"models":[{"slug":"gpt-6.1-sol"}, {"slug":route}]});
    assert_eq!(
        client_catalog(&config, catalog.clone())["models"][1]["slug"],
        "auto222 · ⭐"
    );
    assert_eq!(client_route_aliases(&config)["auto222 · ⭐"], route);

    let mut changed = config.clone();
    changed["model_groups"][0]["name_prefix"] = "auto333".into();
    let renamed = merge_web_update(&config, &changed).unwrap();
    assert_eq!(client_route_aliases(&renamed)["auto222 · ⭐"], route);
    assert_eq!(client_route_aliases(&renamed)["auto333 · ⭐"], route);
    assert_eq!(
        client_catalog(&renamed, catalog.clone())["models"][1]["slug"],
        "auto333 · ⭐"
    );
    let mut reused = renamed.clone();
    reused["model_groups"].as_array_mut().unwrap().push(json!({
        "id":"auto/new", "name_prefix":"auto222", "policy":"priority",
        "sources":["gpt-6.1-sol", "zhou/gpt-6.1-sol"]
    }));
    let reused = merge_web_update(&renamed, &reused).unwrap();
    assert_eq!(client_route_aliases(&reused)["auto222 · ⭐"], route);
    assert_eq!(
        client_catalog(&reused, json!({"models":[{"slug":"auto/new"}]}))["models"][0]["slug"],
        "auto/new"
    );

    let mut changed = renamed.clone();
    changed["catalog_family_presentations"]["gpt-6.1-sol"]["catalog_alias"] = "☀".into();
    let retitled = merge_web_update(&renamed, &changed).unwrap();
    assert_eq!(client_route_aliases(&retitled)["auto333 · ⭐"], route);
    assert_eq!(client_route_aliases(&retitled)["auto333 · ☀"], route);

    let mut changed = retitled.clone();
    changed["model_groups"][0]["sources"] = json!(["gpt-6-astra", "zhou/gpt-6-astra"]);
    let switched = merge_web_update(&retitled, &changed).unwrap();
    let aliases = client_route_aliases(&switched);
    assert_eq!(aliases["auto222 · ⭐"], route);
    assert_eq!(aliases["auto333 · ⭐"], route);
    assert_eq!(aliases["auto333 · ☀"], route);
    assert_eq!(aliases["auto333 · gpt-6-astra"], route);
    assert_eq!(switched["model_groups"][0]["id"], route);
    assert_eq!(normalize_configuration(Some(&switched)).unwrap(), switched);
}

#[test]
fn aggregate_name_collision_does_not_redirect_a_provider_or_account() {
    let config = json!({"providers":[{"id":"service-id","name":"auto222"}],
        "models":[{"id":"service-id/gpt-6.1-sol", "provider":"service-id"}],
        "accounts":[{"prefix":"auto222"}],
        "model_groups":[{"id":"auto/stable","name_prefix":"auto222","sources":["gpt-6.1-sol","zhou/gpt-6.1-sol"]}]});
    let catalog = json!({"models":[{"slug":"service-id/gpt-6.1-sol"},{"slug":"auto/stable"}]});
    assert_eq!(
        client_route_aliases(&config)["auto222 · gpt-6.1-sol"],
        "auto/stable"
    );
    assert!(!client_route_aliases(&config).contains_key("auto222/gpt-6.1-sol"));
    assert_eq!(
        client_catalog(&config, catalog.clone())["models"][1]["slug"],
        "auto222 · gpt-6.1-sol"
    );

    let mut collision = config.clone();
    collision["model_groups"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id":"auto/other", "name_prefix":"auto222", "sources":["gpt-6.1-sol","zhou/gpt-6.1-sol"]
        }));
    assert!(!client_route_aliases(&collision).contains_key("auto222 · gpt-6.1-sol"));
    assert_eq!(
        client_catalog(&collision, catalog)["models"][1]["slug"],
        "auto/stable"
    );

    let mut slash_prefix = config;
    slash_prefix["model_groups"][0]["name_prefix"] = "auto222/extra".into();
    assert!(
        !client_route_aliases(&slash_prefix)
            .values()
            .any(|route| route == "auto/stable")
    );
}
