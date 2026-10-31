use emp_state::normalize_configuration;
use serde_json::json;

#[test]
fn loading_known_model_modalities_preserves_saved_limits_and_reasoning() {
    for endpoint in [
        "https://api.openai.com/v1",
        "https://relay.example.invalid/v1",
    ] {
        let mut raw = json!({
            "providers":[{"id":"service","protocol":"responses","base_url":endpoint}],
            "models":[{
                "id":"service/model","provider":"service","upstream_id":"gpt-5.6-sol",
                "context_window":64000,"output_limit":1024,"reasoning_levels":["low"],
                "input_modalities":["text"],
                "capability_sources":{
                    "context_window":{"source":"official"},
                    "output_limit":{"source":"official"},
                    "reasoning_levels":{"source":"official"},
                    "input_modalities":{"source":"unknown"}
                }
            }]
        });
        let loaded = normalize_configuration(Some(&raw)).unwrap();
        let model = &loaded["models"][0];
        assert_eq!(model["input_modalities"], json!(["text", "image"]));
        assert_eq!(
            model["capability_sources"]["input_modalities"]["source"],
            "official"
        );
        assert_eq!(model["context_window"], 64000);
        assert_eq!(model["output_limit"], 1024);
        assert_eq!(model["reasoning_levels"], json!(["low"]));
        assert_eq!(normalize_configuration(Some(&loaded)).unwrap(), loaded);

        raw["models"][0]["capability_sources"]["input_modalities"]["source"] = json!("manual");
        let restricted = normalize_configuration(Some(&raw)).unwrap();
        assert_eq!(restricted["models"][0]["input_modalities"], json!(["text"]));
    }
}
