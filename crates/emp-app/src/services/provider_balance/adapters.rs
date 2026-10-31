use emp_transport::{HttpClient, HttpMethod};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use url::Url;

#[derive(Clone, Copy)]
pub(super) enum Adapter {
    DeepSeek,
    OpenRouter,
    Moonshot(&'static str),
}

pub(super) struct OfficialService {
    pub(super) billing_url: &'static str,
    pub(super) adapter: Option<Adapter>,
}

pub(super) fn official(base_url: &str) -> Option<OfficialService> {
    let url = Url::parse(base_url).ok()?;
    if url.scheme() != "https" || url.port().is_some() {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    let (billing_url, adapter) = match (url.host_str()?, path) {
        ("api.deepseek.com", "" | "/v1") => (
            "https://platform.deepseek.com/usage",
            Some(Adapter::DeepSeek),
        ),
        ("openrouter.ai", "/api/v1") => (
            "https://openrouter.ai/settings/credits",
            Some(Adapter::OpenRouter),
        ),
        ("api.moonshot.ai", "/v1") => (
            "https://platform.kimi.ai/console/account",
            Some(Adapter::Moonshot("USD")),
        ),
        ("api.moonshot.cn", "/v1") => (
            "https://platform.kimi.com/console/account",
            Some(Adapter::Moonshot("CNY")),
        ),
        ("api.openai.com", "/v1") => (
            "https://platform.openai.com/settings/organization/billing/overview",
            None,
        ),
        ("api.anthropic.com", "/v1" | "") => ("https://platform.claude.com/settings/billing", None),
        ("generativelanguage.googleapis.com", "/v1beta/openai" | "/v1beta") => {
            ("https://aistudio.google.com/usage", None)
        }
        ("api.x.ai", "/v1") => ("https://console.x.ai", None),
        ("api.z.ai", "/api/paas/v4" | "/api/coding/paas/v4") => {
            ("https://z.ai/manage-apikey/billing", None)
        }
        ("open.bigmodel.cn", "/api/paas/v4" | "/api/coding/paas/v4") => {
            ("https://bigmodel.cn/finance-center/finance/overview", None)
        }
        ("api.inceptionlabs.ai", "/v1") => ("https://platform.inceptionlabs.ai", None),
        _ => return None,
    };
    Some(OfficialService {
        billing_url,
        adapter,
    })
}

#[derive(Clone, Serialize)]
pub(crate) struct Money {
    pub(crate) currency: String,
    pub(crate) available: f64,
}

#[derive(Clone, Default, Serialize)]
pub(crate) struct Balance {
    pub(crate) kind: &'static str,
    pub(crate) amounts: Vec<Money>,
}

fn amount(value: &Value) -> Result<f64, &'static str> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|value| value.is_finite())
        .ok_or("balance_invalid_response")
}

pub(super) fn decode(adapter: Adapter, value: &Value) -> Result<Balance, &'static str> {
    let money = |currency: &str, value: &Value| -> Result<Money, &'static str> {
        Ok(Money {
            currency: currency.into(),
            available: amount(value)?,
        })
    };
    let amounts = match adapter {
        Adapter::DeepSeek => value["balance_infos"]
            .as_array()
            .filter(|rows| !rows.is_empty())
            .ok_or("balance_invalid_response")?
            .iter()
            .map(|row| {
                let currency = row["currency"]
                    .as_str()
                    .filter(|currency| matches!(*currency, "USD" | "CNY"))
                    .ok_or("balance_invalid_response")?;
                money(currency, &row["total_balance"])
            })
            .collect::<Result<Vec<_>, _>>()?,
        Adapter::Moonshot(currency) => {
            if value["code"] != 0 || value["status"] != true {
                return Err("balance_invalid_response");
            }
            vec![money(currency, &value["data"]["available_balance"])?]
        }
        Adapter::OpenRouter => {
            let data = &value["data"];
            if !data.is_object() || data.get("limit_remaining").is_none() {
                return Err("balance_invalid_response");
            }
            if data["limit_remaining"].is_null() {
                return Ok(Balance {
                    kind: "no_key_budget",
                    amounts: Vec::new(),
                });
            }
            vec![money("USD", &data["limit_remaining"])?]
        }
    };
    Ok(Balance {
        kind: if matches!(adapter, Adapter::OpenRouter) {
            "key_budget"
        } else {
            "account_balance"
        },
        amounts,
    })
}

pub(super) async fn query(
    client: &HttpClient,
    adapter: Adapter,
    base_url: &str,
    key: &str,
) -> Result<Balance, &'static str> {
    let base = Url::parse(base_url).expect("normalized provider URL");
    let path = match adapter {
        Adapter::DeepSeek => "/user/balance",
        Adapter::OpenRouter => "/api/v1/key",
        Adapter::Moonshot(_) => "/v1/users/me/balance",
    };
    let url = base.join(path).expect("official balance path");
    let headers = BTreeMap::from([
        ("Authorization".into(), format!("Bearer {key}")),
        ("Accept".into(), "application/json".into()),
    ]);
    let response = client
        .open(HttpMethod::Get, url.as_str(), headers, None, false)
        .await
        .map_err(|_| "balance_connection_failed")?;
    match response.status() {
        200 => {}
        401 | 403 => return Err("balance_access_denied"),
        429 => return Err("balance_rate_limited"),
        _ => return Err("balance_service_unavailable"),
    }
    let bytes = response
        .read_limited(64 * 1024)
        .await
        .map_err(|_| "balance_connection_failed")?;
    let value = serde_json::from_slice(&bytes).map_err(|_| "balance_invalid_response")?;
    decode(adapter, &value)
}

#[cfg(test)]
mod tests;
