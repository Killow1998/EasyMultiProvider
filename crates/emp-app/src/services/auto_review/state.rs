//! Auto-review eligibility feedback. Telemetry does not own routing cooldowns.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct ReviewState {
    cooldowns: Mutex<BTreeMap<String, Instant>>,
}

impl ReviewState {
    pub(super) fn cooling(&self) -> BTreeSet<String> {
        let now = Instant::now();
        self.cooldowns
            .lock()
            .map(|cooldowns| {
                cooldowns
                    .iter()
                    .filter(|(_, until)| **until > now)
                    .map(|(account, _)| account.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn clear(&self, account: &str) {
        if let Ok(mut cooldowns) = self.cooldowns.lock() {
            cooldowns.remove(account);
        }
    }

    pub(crate) fn record_result(
        &self,
        model: &str,
        provider: &str,
        success: bool,
        reason: &str,
        error: &str,
    ) {
        if !super::is_auto_review_model(model) || provider.is_empty() {
            return;
        }
        let account = if provider == "codex-native" {
            "@native"
        } else {
            provider
        };
        if success {
            self.clear(account);
            return;
        }
        let unavailable = matches!(
            reason,
            "quota_exhausted" | "rate_limited" | "payment_required" | "auth_rejected"
        ) || matches!(error, "rate_limit" | "auth");
        if unavailable && let Ok(mut cooldowns) = self.cooldowns.lock() {
            cooldowns.insert(
                account.to_owned(),
                Instant::now() + Duration::from_secs(300),
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn test_cooldowns(&self) -> &Mutex<BTreeMap<String, Instant>> {
        &self.cooldowns
    }
}
