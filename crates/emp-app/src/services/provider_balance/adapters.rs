use emp_transport::{HttpClient, HttpMethod};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use url::Url;

#[derive(Clone, Copy)]
enum Adapter {
    DeepSeek,
    Moonshot(&'static str),
}

pub(super) struct OfficialService {
    pub(super) billing_url: &'static str,
    adapter: Adapter,
    query_url: Url,
}

pub(super) fn official(base_url: &str) -> Option<OfficialService> {
    let url = Url::parse(base_url).ok()?;
    if url.scheme() != "https" || url.port().is_some() {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    let (billing_url, adapter) = match (url.host_str()?, path) {
        ("api.deepseek.com", "" | "/v1") => {
            ("https://platform.deepseek.com/usage", Adapter::DeepSeek)
        }
        ("api.moonshot.ai", "/v1") => (
            "https://platform.kimi.ai/console/account",
            Adapter::Moonshot("USD"),
        ),
        ("api.moonshot.cn", "/v1") => (
            "https://platform.kimi.com/console/account",
            Adapter::Moonshot("CNY"),
        ),
        _ => return None,
    };
    Some(OfficialService {
        billing_url,
        adapter,
        query_url: url
            .join(match adapter {
                Adapter::DeepSeek => "/user/balance",
                Adapter::Moonshot(_) => "/v1/users/me/balance",
            })
            .expect("official balance path"),
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

fn decode(adapter: Adapter, value: &Value) -> Result<Balance, &'static str> {
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
    };
    Ok(Balance {
        kind: "account_balance",
        amounts,
    })
}

impl OfficialService {
    pub(super) async fn query(
        &self,
        client: &HttpClient,
        key: &str,
    ) -> Result<Balance, &'static str> {
        let headers = BTreeMap::from([
            ("Authorization".into(), format!("Bearer {key}")),
            ("Accept".into(), "application/json".into()),
        ]);
        let response = client
            .open(
                HttpMethod::Get,
                self.query_url.as_str(),
                headers,
                None,
                false,
            )
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
        decode(self.adapter, &value)
    }
}

#[cfg(test)]
mod tests;
