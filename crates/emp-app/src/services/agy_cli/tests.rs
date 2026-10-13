use super::*;
use serde_json::json;

fn request() -> Value {
    json!({"model":"agy/demo","instructions":"Preserve this system instruction.","input":[{"role":"user","content":"Use the four client tools."}],"tools":[{"type":"namespace","name":"functions","tools":[
        {"type":"function","name":"read_file","parameters":{"type":"object"}},
        {"type":"function","name":"write_file","parameters":{"type":"object"}},
        {"type":"custom","name":"apply_patch","format":{"type":"text"}},
        {"type":"function","name":"exec_command","parameters":{"type":"object"}},
        {"type":"function","name":"send_email","parameters":{"type":"object"}}
    ]}]})
}

fn output(proposal: Value) -> Vec<u8> {
    json!({"event":"result","result":{"status":"SUCCESS","structured_output":proposal,"usage":{"input_tokens":75,"cache_read_tokens":25,"output_tokens":10,"thinking_tokens":3}}}).to_string().into_bytes()
}

#[test]
fn four_client_tools_round_trip_without_running_or_dropping_history() {
    let mut body = request();
    let prepared = projection::prepare(&body).unwrap();
    assert_eq!(prepared.tools.len(), 4);
    assert!(
        prepared
            .instructions
            .contains("Preserve this system instruction.")
    );
    let calls: Vec<_> = prepared.tools.iter().map(|t|json!({"name":t.name,"namespace":t.namespace,"arguments":{"path":"client-only.txt"},"input":"*** Begin Patch\n*** End Patch"})).collect();
    let response = projection::complete(
        &output(json!({"answer":"","tool_calls":calls})),
        &body,
        &prepared.tools,
    )
    .unwrap()
    .body;
    assert_eq!(response["output"].as_array().unwrap().len(), 4);
    assert_eq!(response["usage"]["input_tokens"], 100);
    assert_eq!(response["usage"]["total_tokens"], 110);
    let mut history = body["input"].as_array().unwrap().clone();
    for call in response["output"].as_array().unwrap() {
        assert_eq!(call["namespace"], "functions");
        history.push(call.clone());
        history.push(json!({"type":if call["type"]=="custom_tool_call" {"custom_tool_call_output"}else{"function_call_output"},"call_id":call["call_id"],"output":"CLIENT_RESULT_SENTINEL"}));
    }
    body["input"] = json!(history);
    let next = projection::prepare(&body).unwrap();
    let line: Value = serde_json::from_slice(&next.stdin).unwrap();
    let transcript: Value =
        serde_json::from_str(line["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(transcript["input"], body["input"]);
    assert!(!transcript["tools"].to_string().contains("send_email"));
    assert_eq!(
        projection::complete(
            &output(json!({"answer":"Four client results received.","tool_calls":[]})),
            &body,
            &next.tools
        )
        .unwrap()
        .body["usage"],
        response["usage"]
    );
}

#[test]
fn invalid_proposals_media_and_unavailable_tool_choices_fail_closed() {
    let mut body = request();
    let prepared = projection::prepare(&body).unwrap();
    let bytes = output(
        json!({"answer":"","tool_calls":[{"name":"send_email","namespace":"functions","arguments":{},"input":""}]}),
    );
    assert!(matches!(
        projection::complete(&bytes, &body, &prepared.tools),
        Err(Error("agy_cli_unknown_tool_proposal"))
    ));
    body["tool_choice"] = json!({"type":"function","name":"send_email","namespace":"functions"});
    assert!(matches!(
        projection::prepare(&body),
        Err(Error("agy_cli_tool_choice_unavailable"))
    ));
    body["input"] = json!([{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,synthetic"}]}]);
    assert_eq!(preflight(&body), Err(Error("agy_cli_unsupported_input")));
    let missing = b"{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\",\"response\":\"not a structured result\"}}";
    assert!(matches!(
        projection::complete(missing, &request(), &prepared.tools),
        Err(Error("agy_cli_invalid_output"))
    ));
    let mut constrained = request();
    constrained["max_output_tokens"] = json!(100);
    assert_eq!(
        preflight(&constrained),
        Err(Error("agy_cli_unsupported_parameter"))
    );
    constrained
        .as_object_mut()
        .unwrap()
        .remove("max_output_tokens");
    constrained["tool_choice"] = json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"read_file","namespace":"functions"}]});
    let prepared = projection::prepare(&constrained).unwrap();
    assert_eq!(prepared.tools.len(), 1);
    assert!(matches!(
        projection::complete(
            &output(json!({"answer":"missing required call","tool_calls":[]})),
            &constrained,
            &prepared.tools
        ),
        Err(Error("agy_cli_invalid_output"))
    ));
}

#[test]
fn host_customizations_are_rejected_without_changing_the_profile() {
    let temp = tempfile::tempdir().unwrap();
    let cli = temp.path().join(".gemini/antigravity-cli");
    std::fs::create_dir_all(&cli).unwrap();
    let config = temp.path().join(".gemini/config");
    std::fs::create_dir_all(config.join("projects")).unwrap();
    std::fs::write(config.join("mcp_config.json"), "").unwrap();
    std::fs::write(
        config.join("projects/default-cli-project.json"),
        r#"{"id":"default-cli-project","name":"CLI Project","projectResources":{}}"#,
    )
    .unwrap();
    let settings = cli.join("settings.json");
    for value in [
        json!({"hooks":{"PreInvocation":[{"command":"sentinel"}]}}),
        json!({"statusLine":{"command":"sentinel"}}),
        json!({"title":{"command":"sentinel"}}),
    ] {
        let original = value.to_string();
        std::fs::write(&settings, &original).unwrap();
        assert_eq!(
            profile::validate_home(temp.path()),
            Err(Error("agy_cli_customizations_present"))
        );
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);
    }
    std::fs::write(&settings, r#"{"modelProvider":"gemini"}"#).unwrap();
    assert_eq!(
        profile::validate_home(temp.path()),
        Err(Error("agy_cli_login_mode_required"))
    );
    std::fs::write(
        &settings,
        r#"{"modelProvider":"antigravity","colorScheme":"dark"}"#,
    )
    .unwrap();
    assert_eq!(profile::validate_home(temp.path()), Ok(()));
    std::fs::create_dir_all(cli.join("plugins/example")).unwrap();
    assert_eq!(
        profile::validate_home(temp.path()),
        Err(Error("agy_cli_customizations_present"))
    );
}

#[cfg(unix)]
#[test]
fn cancellation_and_output_overflow_stop_owned_processes() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    fn command(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        cmd
    }
    let started = std::time::Instant::now();
    assert_eq!(
        process::run(command("sleep 30"), b"", Duration::from_secs(5), || Some(
            Error("agy_cli_interrupted")
        )),
        Err(Error("agy_cli_interrupted"))
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        process::run(
            command("head -c 5000000 /dev/zero; sleep 30"),
            b"",
            Duration::from_secs(5),
            || None
        ),
        Err(Error("agy_cli_output_too_large"))
    );
    let started = std::time::Instant::now();
    assert_eq!(
        process::run(
            command(
                "printf '%s\\n' '{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"tool\",\"tool_name\":\"run_command\"}}'; sleep 30"
            ),
            b"",
            Duration::from_secs(5),
            || None
        ),
        Err(Error("agy_cli_local_tool_attempt"))
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        process::run(command("sleep 30"), b"", Duration::from_millis(50), || None),
        Err(Error("agy_cli_timeout"))
    );
}
