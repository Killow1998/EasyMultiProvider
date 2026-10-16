use emp_state::{merge_web_update, normalize_configuration};
use serde_json::json;

#[test]
fn image_routing_defaults_and_saved_preferences_survive_older_web_clients() {
    for input in [None, Some(json!({}))] {
        assert_eq!(
            normalize_configuration(input.as_ref()).unwrap()["image_routing"],
            json!({"mode":"follow","account_id":""})
        );
    }
    for (mode, account) in [
        ("follow", ""),
        ("auto", ""),
        ("account", "@native"),
        ("account", "imported"),
    ] {
        let current = normalize_configuration(Some(&json!({"auto_review_fallback":false,
            "image_routing":{"mode":mode,"account_id":account}})))
        .unwrap();
        let saved =
            merge_web_update(&current, &json!({"accounts":[],"providers":[],"models":[]})).unwrap();
        assert_eq!(saved["image_routing"], current["image_routing"]);
        assert_eq!(saved["auto_review_fallback"], false);
        let public =
            emp_state::public_configuration_with_file_status(&saved, &Default::default(), |_| {
                false
            })
            .unwrap();
        assert_eq!(
            merge_web_update(&saved, &public).unwrap()["image_routing"],
            current["image_routing"]
        );
    }
}

#[test]
fn invalid_image_routes_are_rejected_without_resetting_to_native() {
    for input in [
        json!(null),
        json!(false),
        json!({"mode":"unknown"}),
        json!({"mode":1}),
        json!({"mode":"account"}),
        json!({"mode":"account","account_id":"  "}),
        json!({"mode":"account","account_id":123}),
        json!({"mode":"account","account_id":"bad\naccount"}),
    ] {
        let error = normalize_configuration(Some(&json!({"image_routing":input}))).unwrap_err();
        assert!(error.to_string().contains("image_routing"), "{error}");
    }
}
