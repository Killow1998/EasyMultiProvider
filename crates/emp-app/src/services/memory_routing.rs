//! Route existing Codex memory jobs; Codex still owns memory generation and files.
use crate::app::ServerState;
use crate::services::request_preparation::{
    PreparedRequest, RequestOperation, RequestPreparationError, prepare_request,
    prepare_with_config,
};
use emp_core::{Dialect, RouteSource};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
pub(crate) struct MemoryStatus(Mutex<Value>);

impl MemoryStatus {
    pub(crate) fn snapshot(&self) -> Value {
        self.0.lock().map(|value| value.clone()).unwrap_or_default()
    }
    fn update(&self, value: Value) -> bool {
        let Ok(mut current) = self.0.lock() else {
            return false;
        };
        if *current == value {
            return false;
        }
        *current = value;
        true
    }
}

pub(crate) fn kind(body: &Value, headers: &BTreeMap<String, String>) -> Option<&'static str> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    match header("x-openai-subagent") {
        Some("memory_consolidation") => return Some("memory_consolidation"),
        Some("memory" | "memory_extraction") => return Some("memory_extraction"),
        _ => {}
    }
    let metadata = body["client_metadata"]["x-codex-turn-metadata"]
        .as_str()
        .or_else(|| header("x-codex-turn-metadata"))
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_default();
    match metadata["kind"]
        .as_str()
        .or_else(|| metadata["call_kind"].as_str())
    {
        Some("memory" | "memory_extraction") => Some("memory_extraction"),
        Some("memory_consolidation") => Some("memory_consolidation"),
        _ => None,
    }
}

/// Both HTTP and WebSocket use this boundary so the selected account/model agree.
pub(crate) fn prepare(
    state: &ServerState,
    mut body: Value,
    incoming: &BTreeMap<String, String>,
) -> Result<PreparedRequest, RequestPreparationError> {
    let Some(kind) = kind(&body, incoming) else {
        return prepare_request(state, body, RequestOperation::Responses);
    };
    let config = state
        .backend
        .configuration
        .read()
        .map_err(|_| RequestPreparationError::ConfigurationUnavailable)?
        .clone();
    let field = if kind == "memory_extraction" {
        "memory_extraction_model"
    } else {
        "memory_model"
    };
    let selected = config[field].as_str().unwrap_or_default().to_owned();
    if selected.is_empty() {
        return prepare_with_config(state, config, body, RequestOperation::Responses);
    }
    let fail = |reason: &'static str| {
        if state
            .backend
            .memory_status
            .update(json!({"state":"paused","reason":reason,"model":selected,"kind":kind}))
        {
            state
                .backend
                .management_events
                .publish(crate::services::management_events::Change::Quota);
        }
        RequestPreparationError::MemoryUnavailable(reason)
    };
    let prior = if kind == "memory_extraction" {
        let anchor = body
            .as_object()
            .map(|body| emp_history::request_history_anchor(body, incoming))
            .transpose()
            .map_err(|_| fail("source_conflict"))?;
        let thread = anchor
            .as_ref()
            .and_then(|anchor| anchor.thread_id.as_deref())
            .or_else(|| {
                incoming
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("session-id"))
                    .map(|(_, value)| value.as_str())
            });
        match thread {
            Some(thread) => state
                .backend
                .usage
                .ledger
                .latest_conversation_call(thread)
                .map_err(|_| fail("source_conflict"))?,
            None => None,
        }
    } else {
        None
    };
    let account_id = prior
        .as_ref()
        .filter(|call| call["usage_category"] == "subscription")
        .and_then(|call| call["account_id"].as_str())
        .unwrap_or("@native");
    let route_model = if account_id == "@native" {
        if !crate::services::accounts::regular_file(&state.backend.accounts.native_auth_path) {
            return Err(fail("native_unavailable"));
        }
        let quota = state
            .backend
            .accounts
            .native_quota
            .lock()
            .ok()
            .and_then(|quota| quota.clone());
        if crate::services::subscription_quota::current_headroom(quota.as_ref())
            .is_some_and(|quota| quota <= 0.0)
        {
            return Err(fail("quota_exhausted"));
        }
        selected.clone()
    } else {
        let account = config["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|account| {
                account["id"] == account_id
                    && account["enabled"] != false
                    && account["credential_status"] != "invalid"
            })
            .ok_or_else(|| fail("account_unavailable"))?;
        if crate::services::subscription_quota::current_headroom(account.get("quota"))
            .is_some_and(|quota| quota <= 0.0)
        {
            return Err(fail("quota_exhausted"));
        }
        let prefix = account["prefix"]
            .as_str()
            .filter(|prefix| !prefix.is_empty())
            .ok_or_else(|| fail("account_unavailable"))?;
        format!("{prefix}/{selected}")
    };
    body["model"] = json!(route_model);
    let prepared = prepare_with_config(state, config, body, RequestOperation::Responses)
        .map_err(|_| fail("model_unavailable"))?;
    if prepared.route.dialect != Dialect::CodexNative
        || !matches!(
            prepared.route.source,
            RouteSource::ImplicitNative | RouteSource::SubscriptionAccount
        )
    {
        return Err(fail("model_unavailable"));
    }
    if let Ok(mut status) = state.backend.memory_status.0.lock() {
        *status = json!({"state":"ready","model":selected,"account_id":account_id,"kind":kind});
    }
    Ok(prepared)
}

pub(crate) fn error_message(reason: &str) -> &'static str {
    match reason {
        "quota_exhausted" => {
            "Memory generation paused: the conversation's subscription has no quota. Wait for its reset; external conversations and global consolidation use Native. 记忆任务已暂停：所用订阅额度不足，请等待重置；外部会话及全局整理使用 Native。"
        }
        "native_unavailable" => {
            "Memory generation paused: Native authentication is unavailable. Sign in to Native Codex first. 记忆任务已暂停：Native 未登录，请先完成原生登录。"
        }
        "account_unavailable" => {
            "Memory generation paused: the conversation's subscription is unavailable. Restore that subscription in EMP. 记忆任务已暂停：会话订阅不可用，请在 EMP 中恢复该订阅。"
        }
        "source_conflict" => {
            "Memory generation paused: the conversation source could not be confirmed. Check the request's conversation metadata. 记忆任务已暂停：无法确认会话来源，请检查请求的会话标识。"
        }
        _ => {
            "Memory generation paused: the selected GPT model is unavailable on the conversation's account. Choose an available GPT model in EMP Routing settings. 记忆任务已暂停：所选 GPT 模型在此账号不可用，请在 EMP 路由设置中调整。"
        }
    }
}

pub(crate) fn observe(state: &ServerState, event: &Value) {
    if !matches!(
        event["route"].as_str(),
        Some("memory_extraction" | "memory_consolidation")
    ) {
        return;
    }
    let reason = if event["error_class"] == "usage_limit_reached" {
        Some("quota_exhausted")
    } else if event["status"] == 429 {
        Some("rate_limited")
    } else if event["success"] == true {
        None
    } else if event["status"] == 401 || event["status"] == 403 {
        Some("account_unavailable")
    } else {
        Some("request_failed")
    };
    if state.backend.memory_status.update(
        json!({"state":if reason.is_some() {"paused"} else {"ready"},"reason":reason,
            "model":event["upstream_model"],"account_id":event["account_id"],"kind":event["route"]}),
    ) {
        state
            .backend
            .management_events
            .publish(crate::services::management_events::Change::Quota);
    }
}
