//! Parse only the official model command's tab-separated ID/display-name rows.
use super::{Error, invocation::Invocation, process};
use crate::app::ServerState;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::Duration;

pub(crate) fn discover(state: &ServerState) -> Result<Vec<Value>, Error> {
    let invocation = Invocation::new(super::projection::PROMPT)?;
    let bytes = process::run(invocation.models(), b"", Duration::from_secs(20), || {
        super::cancellation(state, &mut None)
    })?;
    project(&bytes)
}

fn project(bytes: &[u8]) -> Result<Vec<Value>, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error("agy_cli_invalid_model_list"))?;
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (id, name) = line
            .split_once('\t')
            .ok_or(Error("agy_cli_invalid_model_list"))?;
        if id.is_empty()
            || id.len() > 256
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._/".contains(&c))
            || name.is_empty()
            || name.len() > 512
            || name.chars().any(char::is_control)
        {
            return Err(Error("agy_cli_invalid_model_list"));
        }
        if seen.insert(id) {
            result.push(json!({"upstream_id":id,"display_name":name,"supported_protocols":["responses"],"input_modalities":["text"],"supports_images":false,"capability_sources":{"input_modalities":{"source":"adapter"},"supports_images":{"source":"adapter"}}}));
        }
        if result.len() > emp_router::discovery::MAX_DISCOVERED_MODELS {
            return Err(Error("agy_cli_invalid_model_list"));
        }
    }
    if result.is_empty() {
        return Err(Error("agy_cli_invalid_model_list"));
    }
    Ok(result)
}
