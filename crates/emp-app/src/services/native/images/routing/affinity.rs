//! Keep server-side image references with their originating subscription.
use super::*;

struct Asset {
    id: String,
    account: String,
    updated: Instant,
}

#[derive(Default)]
pub(super) struct Assets(VecDeque<Asset>);

pub(super) fn account_id(route: &ResolvedRoute) -> Option<&str> {
    if route.provider.value().get("implicit_native") == Some(&Value::Bool(true)) {
        Some("@native")
    } else {
        route
            .provider
            .value()
            .get("account")
            .and_then(|account| account["id"].as_str())
    }
}

impl Assets {
    pub(super) fn account(
        &self,
        body: &Value,
        strict: bool,
    ) -> Result<Option<String>, NativeHttpError> {
        let mut account = None;
        for id in body["images"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|image| image["file_id"].as_str())
        {
            let owners = self
                .0
                .iter()
                .filter(|asset| asset.id == id && asset.updated.elapsed() < RETENTION)
                .map(|asset| asset.account.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            if owners.len() > 1 || (strict && owners.is_empty()) {
                return Err(reference_error());
            }
            if let Some(owner) = owners.first() {
                if account.as_deref().is_some_and(|account| account != *owner) {
                    return Err(reference_error());
                }
                account = Some((*owner).to_owned());
            }
        }
        Ok(account)
    }

    pub(super) fn remember(&mut self, route: &ResolvedRoute, value: &Value) {
        let Some(account) = account_id(route) else {
            return;
        };
        self.0.retain(|asset| asset.updated.elapsed() < RETENTION);
        for id in value["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|image| image["generation_id"].as_str())
            .filter(|id| !id.is_empty() && id.len() <= 512)
        {
            self.0
                .retain(|asset| asset.id != id || asset.account != account);
            if self.0.len() == CAPACITY {
                self.0.pop_front();
            }
            self.0.push_back(Asset {
                id: id.to_owned(),
                account: account.to_owned(),
                updated: Instant::now(),
            });
        }
    }
}

pub(super) fn reference_error() -> NativeHttpError {
    NativeHttpError::router(
        409,
        "This image reference cannot be routed to that account. Use the original image account or attach the local image file.",
    )
}
