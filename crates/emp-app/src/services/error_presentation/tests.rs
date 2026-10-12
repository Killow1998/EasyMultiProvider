use super::*;
use serde_json::json;

#[test]
fn summaries_preserve_retry_codes_details_and_successful_wire() {
    for chinese in [false, true] {
        let presenter = ErrorPresentation {
            chinese,
            service: "Codex",
            selected: true,
            thread: None,
        };
        for (origin, code, status, zh, en) in [
            (
                "transport",
                "network_error",
                502,
                "EMP 网络连接失败",
                "EMP network connection failed",
            ),
            (
                "upstream",
                "rate_limit_exceeded",
                429,
                "Codex 请求受限",
                "Codex rate limit reached",
            ),
            (
                "upstream",
                "model_overloaded",
                503,
                "Codex 模型拥堵",
                "Codex model overloaded",
            ),
            (
                "upstream",
                "server_error",
                503,
                "Codex 服务暂时不可用",
                "Codex service temporarily unavailable",
            ),
            (
                "emp",
                "router_error",
                400,
                "EMP 请求处理出错",
                "EMP request processing failed",
            ),
        ] {
            let error = json!({"message":"Original English details","code":code,"origin":origin,"status":status,"headers":{"Retry-After":"3"},"future":{"opaque":true}});
            let event = json!({"type":"error","status":status,"error":error});
            let ws = presenter.event(&event, false).unwrap();
            let expected = if chinese { zh } else { en };
            assert_eq!(
                ws["error"]["message"],
                format!("{expected}\nOriginal English details")
            );
            for key in ["code", "origin", "status", "headers", "future"] {
                assert_eq!(ws["error"][key], event["error"][key]);
            }
            let failed = json!({"type":"response.failed","response":{"id":"response-fixture","error":error}});
            let frame =
                format!("event: response.failed\nid: fixture\nretry: 3\ndata: {failed}\n\n");
            let sse = String::from_utf8(presenter.frame(frame.as_bytes(), false).unwrap()).unwrap();
            assert!(sse.contains("id: fixture\nretry: 3\n"));
            assert!(sse.contains(expected));
            let body = json!({"error":error}).to_string();
            let wire = format!(
                "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nRetry-After: 3\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let http = String::from_utf8(presenter.http(wire.as_bytes()).unwrap()).unwrap();
            let (head, body) = http.split_once("\r\n\r\n").unwrap();
            assert!(head.contains("Retry-After: 3"));
            assert!(head.contains(&format!("Content-Length: {}", body.len())));
            assert_eq!(
                serde_json::from_str::<Value>(body).unwrap()["error"],
                ws["error"]
            );
        }
        assert!(
            presenter
                .event(
                    &json!({"type":"response.output_text.delta","delta":"unchanged"}),
                    true
                )
                .is_none()
        );
        assert!(
            presenter
                .http(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .is_none()
        );
    }
}
#[test]
fn history_command_is_targeted_and_never_uses_untrusted_shell_text() {
    let mut presenter = ErrorPresentation {
        chinese: true,
        ..Default::default()
    };
    let event = json!({"type":"error","error":{"origin":"emp","message":"English history detail","code":"history_reconstruction_failed","category":"checkpoint"}});
    presenter.thread(Some("01234567-89ab-cdef-0123-456789abcdef"));
    let value = presenter.event(&event, false).unwrap();
    assert_eq!(
        value["error"]["repair_command"],
        "EMP migrate-history --thread-id 01234567-89ab-cdef-0123-456789abcdef"
    );
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("关闭此对话")
    );
    for thread in [
        "$(echo secret)",
        "not-a-thread",
        "----0123456789abcdef0123456789abcdef",
    ] {
        presenter.thread(Some(thread));
        assert!(
            presenter.event(&event, false).unwrap()["error"]
                .get("repair_command")
                .is_none()
        );
    }
}
