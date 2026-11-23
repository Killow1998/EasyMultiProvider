use crate::{
    app::ServerState,
    http::request::{Request, query_values},
    server::http::error::JsonReply,
};
use emp_state::usage::ledger::UsageLedger;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) enum GatewayRead {
    Connection,
    Clients,
    Usage,
}

impl GatewayRead {
    pub(crate) fn for_path(path: &str) -> Option<Self> {
        match path {
            "/api/gateway" | "/api/decisions/connection" => Some(Self::Connection),
            "/api/gateway/clients" => Some(Self::Clients),
            "/api/gateway/usage" => Some(Self::Usage),
            _ => None,
        }
    }

    pub(crate) fn read(self, request: Request<'_>, state: &ServerState) -> JsonReply {
        match self {
            Self::Connection => json_reply(&json!({"api_key":state.backend.gateway_key})),
            Self::Clients => json_reply(&json!({"clients":state.backend.gateway_clients.list()})),
            Self::Usage => read_usage(request, state),
        }
    }
}

fn read_usage(request: Request<'_>, state: &ServerState) -> JsonReply {
    let now = crate::util::system_now();
    let number = |name, default| {
        query_values(request.target, name)
            .first()
            .map_or(Ok(default), |value| value.parse::<f64>())
    };
    let (start, end) = match (number("start", now - 86400.0), number("end", now)) {
        (Ok(start), Ok(end)) if UsageLedger::valid_period(start, end, "all") => (start, end),
        _ => {
            return JsonReply::error(400, "Invalid report period", None);
        }
    };
    let by_device = query_values(request.target, "group")
        .first()
        .map(String::as_str)
        == Some("device");
    let devices = by_device.then(|| state.backend.gateway_clients.devices());
    let device_ids = devices.as_ref().map(|devices| {
        devices
            .iter()
            .map(|device| device.id.as_str())
            .collect::<Vec<_>>()
    });
    match state
        .backend
        .usage
        .ledger
        .query_callers(start, end, device_ids.as_deref())
    {
        Ok(mut report) => {
            if let Some(devices) = devices {
                let rows = report["rows"].as_array_mut().expect("caller usage rows");
                let mut recorded: BTreeMap<_, _> = std::mem::take(rows)
                    .into_iter()
                    .map(|row| {
                        (
                            row["caller_id"].as_str().expect("caller ID").to_owned(),
                            row,
                        )
                    })
                    .collect();
                for device in devices {
                    let mut row = recorded.remove(&device.id).unwrap_or_else(|| {
                        json!({
                            "caller_id": device.id,
                            "calls": 0,
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "cost_nanos": 0,
                            "reported_calls": 0,
                            "priced_calls": 0,
                            "models": []
                        })
                    });
                    row["caller_name"] = json!(device.name);
                    rows.push(row);
                }
            }
            json_reply(&report)
        }
        Err(_) => JsonReply::error(503, "Caller usage is unavailable", None),
    }
}

pub(crate) fn json_reply(value: &impl Serialize) -> JsonReply {
    JsonReply::new(200, serde_json::to_vec(value).expect("gateway response"))
        .with_header("Cache-Control", "no-store")
}
