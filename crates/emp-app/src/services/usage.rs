//! Durable usage observations, price refresh and read-only rollout scanning.
use crate::app::ServerState;
use crate::util::system_now;
use emp_codex::usage_history::UsageHistoryScanner;
use emp_core::ResolvedRoute;
use emp_state::usage::{
    account_owner,
    ledger::UsageLedger,
    pricing::{PRICE_INTERVAL, PRICE_URL, PriceCatalog, normalize_prices},
    reported_usage,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, atomic::Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

pub(crate) struct UsageState {
    pub(crate) ledger: Arc<UsageLedger>,
    pub(crate) history: UsageHistoryScanner,
    revision: Mutex<u64>,
    wake: Condvar,
}
impl UsageState {
    pub(crate) fn new(root: &Path) -> Self {
        let prices = Arc::new(PriceCatalog::new(
            root.join("api_prices.json"),
            system_now(),
        ));
        Self {
            ledger: Arc::new(UsageLedger::new(root.join("usage.sqlite3"), prices)),
            history: UsageHistoryScanner::default(),
            revision: Mutex::new(0),
            wake: Condvar::new(),
        }
    }
    pub(crate) fn queue_scan(&self) {
        self.history.queued();
        *self.revision.lock().expect("history refresh") += 1;
        self.wake.notify_all();
    }
    pub(crate) fn stop(&self) {
        self.history.stop.store(true, Ordering::Release);
        self.wake.notify_all();
    }
    fn wait(&self, state: &ServerState, seconds: f64, revision: u64) {
        let guard = self.revision.lock().expect("usage wakeup");
        if !state.shutdown.load(Ordering::Acquire) && *guard == revision {
            let _ = self
                .wake
                .wait_timeout(guard, Duration::from_secs_f64(seconds.max(0.0)));
        }
    }
}

pub(crate) fn workers(state: &Arc<ServerState>) -> std::io::Result<Vec<JoinHandle<()>>> {
    let history_state = Arc::clone(state);
    let history = std::thread::Builder::new()
        .name("emp-usage-history".into())
        .spawn(move || {
            let usage = &history_state.backend.usage;
            while !history_state.shutdown.load(Ordering::Acquire) {
                let revision = *usage.revision.lock().expect("history refresh");
                let config = history_state
                    .backend
                    .configuration
                    .config
                    .lock()
                    .ok()
                    .map(|config| config.clone())
                    .unwrap_or(json!({}));
                usage.history.scan(
                    &usage.ledger,
                    &history_state.backend.accounts.codex_home,
                    &config,
                    system_now(),
                );
                usage.wait(&history_state, 60.0, revision);
            }
        })?;
    let price_state = Arc::clone(state);
    let prices = std::thread::Builder::new()
        .name("emp-api-prices".into())
        .spawn(move || {
            let usage = &price_state.backend.usage;
            if usage.ledger.prices.snapshot(system_now())["model_count"]
                .as_u64()
                .unwrap_or(0)
                > 0
            {
                usage.ledger.price_pending(&price_state.shutdown);
            }
            while !price_state.shutdown.load(Ordering::Acquire) {
                let snapshot = usage.ledger.prices.snapshot(system_now());
                let delay = (snapshot["fetched_at"].as_f64().unwrap_or(0.0) + PRICE_INTERVAL
                    - system_now())
                .max(0.0);
                if delay > 0.0 {
                    let revision = *usage.revision.lock().expect("price wakeup");
                    usage.wait(&price_state, delay.min(3600.0), revision);
                    continue;
                }
                if refresh_prices(&price_state) {
                    usage.ledger.price_pending(&price_state.shutdown);
                } else if !price_state.shutdown.load(Ordering::Acquire) {
                    usage.ledger.prices.refresh_failed();
                    let revision = *usage.revision.lock().expect("price wakeup");
                    usage.wait(&price_state, 3600.0, revision);
                }
            }
        })?;
    Ok(vec![history, prices])
}
fn refresh_prices(state: &ServerState) -> bool {
    let fetched=state.backend.transport.runtime.block_on(async{
        let operation=async {
            let response=state.backend.transport.client.open(emp_transport::HttpMethod::Get,PRICE_URL,BTreeMap::from([("User-Agent".into(),"EMP-price-catalog".into()),("Accept".into(),"application/json".into())]),None,false).await.ok()?;
            if !(200..300).contains(&response.status()){return None;}
            let raw=response.read_limited(16*1024*1024).await.ok()?;
            normalize_prices(&serde_json::from_slice::<Value>(&raw).ok()?).ok()
        };
        tokio::select! {
            result=tokio::time::timeout(Duration::from_secs(20),operation)=>result.ok().flatten(),
            _=async {while !state.shutdown.load(Ordering::Acquire){tokio::time::sleep(Duration::from_millis(25)).await;}}=>None,
        }
    });
    let Some(prices) = fetched else {
        return false;
    };
    let now = system_now();
    let catalog = &state.backend.usage.ledger.prices;
    if emp_state::filesystem::atomic_write_private_state(
        catalog.path(),
        &serde_json::to_vec(&json!({"prices":prices,"fetched_at":now})).expect("price JSON"),
    )
    .is_err()
    {
        return false;
    }
    catalog.replace(prices, now);
    true
}

pub(crate) struct Observation {
    ledger: Arc<UsageLedger>,
    event: Value,
    finalized: bool,
}
impl Observation {
    pub(crate) fn new(
        state: &ServerState,
        route: &ResolvedRoute,
        body: &Value,
        incoming: &BTreeMap<String, String>,
        owner: Option<&str>,
        operation: &str,
    ) -> Self {
        let category = match route
            .provider
            .value()
            .get("auth_mode")
            .and_then(Value::as_str)
        {
            Some("account") => "subscription",
            Some("forward" | "native") => "native",
            _ => "external",
        };
        let owner = if category == "external" {
            route.provider_id.clone()
        } else {
            owner
                .map(str::to_owned)
                .unwrap_or_else(|| account_owner(incoming))
        };
        let owner = if owner.is_empty() {
            format!("unconfirmed:{}", route.provider_id)
        } else {
            owner
        };
        let turn = body
            .as_object()
            .and_then(|body| emp_history::request_history_anchor(body, incoming).ok())
            .and_then(|anchor| anchor.turn_id)
            .unwrap_or_default();
        Self {
            finalized: false,
            ledger: Arc::clone(&state.backend.usage.ledger),
            event: json!({"route":operation,"usage_category":category,"usage_owner":owner,"upstream_model":route.upstream_model,"route_model":route.requested_model,"usage_turn":turn,"service_tier":body["service_tier"].as_str().filter(|s|!s.is_empty()).unwrap_or("default")}),
        }
    }
    pub(crate) fn observe(&mut self, event: &Value) {
        self.event
            .as_object_mut()
            .unwrap()
            .extend(reported_usage(event));
        if matches!(
            event["type"].as_str(),
            Some("response.completed" | "response.incomplete" | "response.failed" | "error")
        ) {
            self.finish();
        }
    }
    pub(crate) fn finish(&mut self) {
        if !self.finalized {
            self.ledger.record(&self.event, system_now());
            self.finalized = true;
        }
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.finish();
    }
}
