//! Fetch the authenticated Codex model catalog without redirects or retries.
use crate::{RouterError, RouterErrorKind};
use emp_transport::{HttpClient, HttpMethod, HttpTransportErrorKind, status_error_class};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

fn error(status: u16, message: &'static str) -> RouterError {
    RouterError::new(
        RouterErrorKind::InvalidRequest,
        status,
        status_error_class(Some(status)),
        None,
        None,
        message,
    )
}
pub async fn fetch(
    client: &HttpClient,
    base_url: &str,
    headers: &BTreeMap<String, String>,
    client_version: &str,
) -> Result<Value, RouterError> {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_version", client_version)
        .finish();
    let url = format!("{}/models?{query}", base_url.trim_end_matches('/'));
    let mut headers = headers.clone();
    headers.insert("Accept".into(), "application/json".into());
    headers.insert(
        "User-Agent".into(),
        format!("codex_cli_rs/{client_version}"),
    );
    let operation = async {
        let response=client.open(HttpMethod::Get,&url,headers,None,false).await.map_err(|_|error(503,"Cannot connect to the subscription model catalog; check the network proxy and retry"))?;
        if response.status() != 200 {
            return Err(error(
                response.status(),
                "Subscription model catalog request failed",
            ));
        }
        let raw=response.read_limited(crate::discovery::MAX_DISCOVERY_BODY_BYTES).await.map_err(|failure| match failure.kind(){HttpTransportErrorKind::ResponseTooLarge=>error(502,"upstream subscription model catalog is too large"),_=>error(503,"Cannot connect to the subscription model catalog; check the network proxy and retry")})?;
        let catalog: Value = serde_json::from_slice(&raw)
            .map_err(|_| error(502, "Subscription model catalog is not valid JSON"))?;
        let models = catalog["models"]
            .as_array()
            .ok_or_else(|| error(502, "Subscription model catalog is missing models"))?;
        if models
            .iter()
            .any(|model| !model.is_object() || !model["slug"].is_string())
        {
            return Err(error(
                502,
                "Subscription model catalog has invalid model entries",
            ));
        }
        Ok(catalog)
    };
    tokio::time::timeout(Duration::from_secs(30),operation).await.map_err(|_|error(503,"Cannot connect to the subscription model catalog; check the network proxy and retry"))?
}

#[cfg(test)]
mod tests {
    use super::fetch;
    use emp_transport::{HttpClient, HttpClientPolicy};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn fake_response(status: u16) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind catalog fixture");
        let address = listener.local_addr().expect("catalog fixture address");
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept catalog request");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("request timeout");
            let mut request = Vec::new();
            loop {
                let mut buffer = [0_u8; 1024];
                let count = stream.read(&mut buffer).expect("read catalog request");
                assert_ne!(count, 0, "catalog request ended before headers");
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).expect("HTTP request headers");
            let body = br#"{"models":[{"slug":"fixture-model"}]}"#;
            write!(
                stream,
                "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write catalog response head");
            if status == 200 {
                stream.write_all(body).expect("write catalog response");
            }
            request
        });
        (format!("http://{address}/v1"), worker)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn selected_versions_reach_the_models_query_and_user_agent() {
        let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
        let headers =
            BTreeMap::from([("Authorization".to_owned(), "Bearer test-token".to_owned())]);
        for version in ["0.158.4", "0.159.0-beta.1+build.2"] {
            let (base_url, worker) = fake_response(200);
            let catalog = fetch(&client, &base_url, &headers, version)
                .await
                .expect("catalog fetch");
            assert_eq!(catalog["models"][0]["slug"], json!("fixture-model"));

            let request = worker.join().expect("join catalog fixture");
            let encoded =
                url::form_urlencoded::byte_serialize(version.as_bytes()).collect::<String>();
            assert!(
                request.starts_with(&format!(
                    "GET /v1/models?client_version={encoded} HTTP/1.1\r\n"
                )),
                "unexpected catalog request: {request}"
            );
            let lowered = request.to_ascii_lowercase();
            assert!(lowered.contains("authorization: bearer test-token\r\n"));
            assert!(lowered.contains(&format!("user-agent: codex_cli_rs/{version}\r\n")));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upstream_status_errors_keep_their_status() {
        let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
        let (base_url, worker) = fake_response(401);
        let error = fetch(&client, &base_url, &BTreeMap::new(), "0.158.0")
            .await
            .expect_err("401 catalog response must fail");
        assert_eq!(error.status(), 401);
        worker.join().expect("join catalog fixture");
    }
}
