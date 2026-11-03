use super::*;
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener,
};

#[test]
fn account_balances_preserve_currency_and_actual_funds() {
    let balance = decode(Adapter::DeepSeek, &json!({"balance_infos":[{"currency":"CNY","total_balance":"0.00"},{"currency":"USD","total_balance":"12.50"}]})).unwrap();
    assert_eq!(balance.amounts[0].available, 0.0);
    assert_eq!(balance.amounts[1].currency, "USD");
    for (currency, available) in [("CNY", 12.5), ("USD", 0.0), ("USD", -1.5)] {
        let balance = decode(
            Adapter::Moonshot(currency),
            &json!({"code":0,"status":true,"data":{"available_balance":available}}),
        )
        .unwrap();
        assert_eq!(balance.kind, "account_balance");
        assert_eq!(balance.amounts[0].currency, currency);
        assert_eq!(balance.amounts[0].available, available);
    }
    for data in [
        json!({"limit_remaining":null}),
        json!({"limit_remaining":100}),
        json!({"available_balance":null}),
    ] {
        assert!(
            decode(
                Adapter::Moonshot("USD"),
                &json!({"code":0,"status":true,"data":data})
            )
            .is_err()
        );
    }
    assert!(
        decode(
            Adapter::Moonshot("CNY"),
            &json!({"code":1,"status":false,"data":{"available_balance":9.0}})
        )
        .is_err()
    );
    assert!(official("http://api.deepseek.com").is_none());
    assert!(official("https://api.deepseek.com.example/v1").is_none());
    assert!(official("https://example.com/v1").is_none());
    for base_url in [
        "https://openrouter.ai/api/v1",
        "https://api.openai.com/v1",
        "https://api.anthropic.com/v1",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "https://api.x.ai/v1",
        "https://api.z.ai/api/paas/v4",
        "https://open.bigmodel.cn/api/paas/v4",
        "https://api.inceptionlabs.ai/v1",
    ] {
        assert!(official(base_url).is_none(), "{base_url}");
    }
}

#[test]
fn balance_http_uses_official_paths_without_retrying_errors() {
    for (adapter, path, body, status) in [
        (
            Adapter::DeepSeek,
            "/user/balance",
            json!({"balance_infos":[{"currency":"USD","total_balance":"4.20"}]}),
            200,
        ),
        (
            Adapter::Moonshot("USD"),
            "/v1/users/me/balance",
            json!({"code":0,"status":true,"data":{"available_balance":0}}),
            200,
        ),
        (
            Adapter::Moonshot("CNY"),
            "/v1/users/me/balance",
            json!({"error":{"message":"not echoed"}}),
            403,
        ),
        (
            Adapter::DeepSeek,
            "/user/balance",
            json!({"error":{"message":"not echoed"}}),
            401,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut service = official(match adapter {
            Adapter::DeepSeek => "https://api.deepseek.com",
            Adapter::Moonshot("CNY") => "https://api.moonshot.cn/v1",
            Adapter::Moonshot(_) => "https://api.moonshot.ai/v1",
        })
        .unwrap();
        assert_eq!(service.query_url.path(), path);
        service.query_url =
            Url::parse(&format!("http://{}{path}", listener.local_addr().unwrap())).unwrap();
        let task = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let length = socket.read(&mut buffer).unwrap();
                assert!(length > 0);
                request.extend_from_slice(&buffer[..length]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
            assert!(
                request
                    .to_lowercase()
                    .contains("authorization: bearer fixture-only")
            );
            let bytes = serde_json::to_vec(&body).unwrap();
            write!(socket,"HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();
            socket.write_all(&bytes).unwrap();
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let client = HttpClient::new(emp_transport::HttpClientPolicy::new(
            emp_transport::ProxyPolicy::explicit(None),
            emp_transport::TimeoutPolicy::default(),
        ))
        .unwrap();
        let result = runtime.block_on(service.query(&client, "fixture-only"));
        if status == 200 {
            assert!(!result.unwrap().amounts.is_empty());
        } else {
            assert_eq!(result.err(), Some("balance_access_denied"));
        }
        task.join().unwrap();
    }
}
