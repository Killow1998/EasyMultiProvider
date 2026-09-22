//! Fetch the authenticated Codex model catalog without redirects or retries.
use crate::{RouterError, RouterErrorKind};
use emp_transport::{HttpClient, HttpMethod, HttpTransportErrorKind, status_error_class};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;
pub const CLIENT_VERSION: &str = "0.155.0";
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
) -> Result<Value, RouterError> {
    let url = format!(
        "{}/models?client_version={CLIENT_VERSION}",
        base_url.trim_end_matches('/')
    );
    let mut headers = headers.clone();
    headers.insert("Accept".into(), "application/json".into());
    headers.insert(
        "User-Agent".into(),
        format!("codex_cli_rs/{CLIENT_VERSION}"),
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
