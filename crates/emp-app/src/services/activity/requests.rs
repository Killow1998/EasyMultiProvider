//! Public, content-free request receipts shared with the activity event stream.
use super::{ACTIVITY_RETENTION_SECONDS, ActivityIdentity};
use serde_json::{Value, json};
use std::collections::VecDeque;

const CAPACITY: usize = 128;

#[derive(Default)]
pub(super) struct Requests {
    records: VecDeque<Value>,
}

impl Requests {
    fn entry(&mut self, id: &Value, now: u64) -> Option<&mut Value> {
        let id = emp_state::diagnostics::schema::id(id);
        if id.is_empty() {
            return None;
        }
        self.records.retain(|record| {
            now.saturating_sub(record["updated_at"].as_u64().unwrap_or(0))
                <= ACTIVITY_RETENTION_SECONDS
                || record["state"] == "active"
        });
        if let Some(index) = self
            .records
            .iter()
            .position(|record| record["request_id"] == id)
        {
            return self.records.get_mut(index);
        }
        if self.records.len() == CAPACITY {
            self.records.pop_front();
        }
        self.records.push_back(json!({
            "request_id":id,"started_at":now,"updated_at":now,
            "state":"active","retries":[],"attempts":0
        }));
        self.records.back_mut()
    }

    pub(super) fn observe(
        &mut self,
        event: &Value,
        identity: Option<&ActivityIdentity>,
        finished: bool,
        now: u64,
    ) -> bool {
        let Some(identity) = identity else {
            return false;
        };
        let Some(record) = self.entry(&event["request_id"], now) else {
            return false;
        };
        let previous = record.clone();
        if event["dispatch_started"] == true {
            record["attempts"] = json!(record["attempts"].as_u64().unwrap_or(0).saturating_add(1));
            if record["attempt_routes"].is_null() {
                record["attempt_routes"] = json!([]);
            }
            let routes = record["attempt_routes"].as_array_mut().unwrap();
            if routes.len() < 16 {
                routes.push(json!({"at":now,"upstream_model":emp_state::diagnostics::schema::id(&event["upstream_model"]),"protocol":emp_state::diagnostics::schema::id(&event["resolved_protocol"])}));
            }
        }
        for field in [
            "client_model",
            "upstream_model",
            "response_model",
            "resolved_protocol",
            "transport",
            "error_class",
            "error_origin",
            "error_code",
            "failure_reason",
            "session_id",
            "thread_id",
            "parent_thread_id",
            "turn_id",
            "selected_name",
            "provider_name",
            "model_name_status",
        ] {
            let value = emp_state::diagnostics::schema::id(&event[field]);
            if !value.is_empty() {
                record[field] = json!(value);
            }
        }
        for field in [
            "input_tokens",
            "output_tokens",
            "cached_input_tokens",
            "cache_write_tokens",
            "cache_write_1h_tokens",
            "reasoning_tokens",
            "duration_ms",
            "ttft_ms",
            "first_content_ms",
            "tokens_per_second",
        ] {
            if event[field].is_number() {
                record[field] = event[field].clone();
            }
        }
        if let Some(declarations) = event["model_declarations"].as_array() {
            record["model_declarations"] = json!(declarations.iter().take(8).collect::<Vec<_>>());
        }
        record["model_id"] = json!(identity.model_id);
        record["provider_id"] = json!(identity.provider_id);
        record["account_id"] = json!(identity.account_id);
        record["http_status"] = event["status"]
            .as_u64()
            .filter(|status| (100..=599).contains(status))
            .map_or(Value::Null, |status| json!(status));
        record["state"] = json!(if !finished {
            "active"
        } else {
            emp_state::usage::call_state(event)
        });
        if finished {
            record["finished_at"] = json!(now);
            record["duration_ms"] = event["duration_ms"].clone();
        }
        let changed = *record != previous;
        if changed {
            record["updated_at"] = json!(now);
        }
        changed
    }

    pub(super) fn retry(&mut self, id: &Value, reason: &str, status: u16, now: u64) -> bool {
        let Some(record) = self.entry(id, now) else {
            return false;
        };
        let retries = record["retries"].as_array_mut().expect("request retries");
        if retries.len() < 16 {
            retries.push(json!({"reason":reason,"status":status,"at":now}));
        }
        record["updated_at"] = json!(now);
        true
    }

    pub(super) fn snapshot(&self, now: u64) -> Vec<Value> {
        self.records
            .iter()
            .rev()
            .filter(|record| {
                now.saturating_sub(record["updated_at"].as_u64().unwrap_or(0))
                    <= ACTIVITY_RETENTION_SECONDS
                    || record["state"] == "active"
            })
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_keep_parallel_requests_retries_and_reported_model_without_content() {
        let identity =
            ActivityIdentity::new("demo/model".into(), Some("demo".into()), None).unwrap();
        let mut requests = Requests::default();
        requests.retry(&json!("first"), "network", 502, 10);
        let first = json!({"request_id":"first","client_model":"demo/model","upstream_model":"model","prompt":"secret","api_key":"secret","dispatch_started":true});
        requests.observe(&first, Some(&identity), false, 11);
        requests.observe(
            &json!({"request_id":"second","upstream_model":"model"}),
            Some(&identity),
            false,
            12,
        );
        let mut complete = first;
        complete["dispatch_started"] = json!(false);
        complete["response_model"] = json!("model-revision");
        complete["status"] = json!(200);
        complete["success"] = json!(true);
        requests.observe(&complete, Some(&identity), true, 13);
        let records = requests.snapshot(13);
        assert_eq!(records[0]["state"], "active");
        assert_eq!(records[1]["state"], "completed");
        assert_eq!(records[1]["response_model"], "model-revision");
        assert_eq!(records[1]["attempts"], 1); // Scheduled retries are not sends.
        assert_eq!(records[1]["retries"][0]["reason"], "network");
        assert!(!serde_json::to_string(&records).unwrap().contains("secret"));
        complete["error_class"] = json!("client_disconnect");
        complete["success"] = json!(false);
        requests.observe(&complete, Some(&identity), true, 14);
        assert_eq!(requests.snapshot(14)[1]["state"], "cancelled");
    }
}
