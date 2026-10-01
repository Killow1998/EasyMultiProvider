//! Durable usage observations, price refresh and read-only rollout scanning.
use crate::app::ServerState;
use crate::util::system_now;
use emp_codex::usage_history::UsageHistoryScanner;
use emp_state::usage::{
    ledger::UsageLedger,
    pricing::{PRICE_INTERVAL, PRICE_URL, PriceCatalog, normalize_prices},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

pub(crate) struct UsageState {
    pub(crate) ledger: Arc<UsageLedger>,
    pub(crate) history: UsageHistoryScanner,
    progress: Mutex<ScanProgress>,
    pub(crate) event_revision: AtomicU64,
    scan_wake: Condvar,
    price_wake: Condvar,
}
#[derive(Default)]
struct ScanProgress {
    requested: u64,
    acknowledged: u64,
    completed: u64,
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
            progress: Mutex::new(ScanProgress::default()),
            event_revision: AtomicU64::new(0),
            scan_wake: Condvar::new(),
            price_wake: Condvar::new(),
        }
    }
    pub(crate) fn queue_scan(&self, state: &ServerState) -> Option<u64> {
        let mut progress = self.progress.lock().expect("history refresh");
        if self.history.stop.load(Ordering::Acquire) || state.shutdown.load(Ordering::Acquire) {
            return None;
        }
        progress.requested = progress.requested.checked_add(1)?;
        let id = progress.requested;
        state.backend.diagnostics.journal.event(
            "info",
            "usage_scan_queued",
            &json!({"command_id":id}),
        );
        self.scan_wake.notify_all();
        drop(progress);
        self.notify_scan(state);
        Some(id)
    }
    pub(crate) fn status(&self) -> Value {
        let progress = self.progress.lock().expect("history progress");
        let mut status = self.history.status();
        status["requested"] = json!(progress.requested);
        status["acknowledged"] = json!(progress.acknowledged);
        status["completed"] = json!(progress.completed);
        status["queued"] = json!(progress.requested > progress.acknowledged);
        status["running"] = json!(
            status["running"] == true
                || (!self.history.stop.load(Ordering::Acquire)
                    && progress.acknowledged > progress.completed)
        );
        status["cancelled"] = json!(
            self.history.stop.load(Ordering::Acquire) && progress.completed < progress.requested
        );
        status
    }
    fn publish_scan(&self, state: &ServerState, event: &str, id: u64, summary: &Value) {
        state.backend.diagnostics.journal.event(
            "info",
            event,
            &json!({
                "command_id":id, "files":summary["files"], "updated":summary["updated"],
                "errors":summary["errors"], "cancelled":self.history.stop.load(Ordering::Acquire) || state.shutdown.load(Ordering::Acquire),
            }),
        );
        self.notify_scan(state);
    }
    fn notify_scan(&self, state: &ServerState) {
        // Use the SSE wait mutex to prevent a notification falling between its
        // predicate check and sleep. No quota change is implied by this event.
        let _guard = state
            .backend
            .accounts
            .quota_revision
            .lock()
            .expect("event wakeup");
        self.event_revision.fetch_add(1, Ordering::Release);
        state.backend.accounts.quota_condition.notify_all();
    }
    pub(crate) fn stop(&self) {
        let _guard = self.progress.lock().expect("usage wakeup");
        self.history.stop.store(true, Ordering::Release);
        self.scan_wake.notify_all();
        self.price_wake.notify_all();
    }
    fn wait_for_scan(&self, state: &ServerState, revision: u64) {
        let guard = self.progress.lock().expect("usage wakeup");
        let _ = self
            .scan_wake
            .wait_timeout_while(guard, Duration::from_secs(60), |progress| {
                !state.shutdown.load(Ordering::Acquire)
                    && !self.history.stop.load(Ordering::Acquire)
                    && progress.requested == revision
            });
    }
    fn wait_for_prices(&self, state: &ServerState, seconds: f64) {
        let guard = self.progress.lock().expect("price wakeup");
        let _ = self.price_wake.wait_timeout_while(
            guard,
            Duration::from_secs_f64(seconds.max(0.0)),
            |_| {
                !state.shutdown.load(Ordering::Acquire)
                    && !self.history.stop.load(Ordering::Acquire)
            },
        );
    }
}

pub(crate) fn workers(state: &Arc<ServerState>) -> std::io::Result<Vec<JoinHandle<()>>> {
    let history_state = Arc::clone(state);
    let history = std::thread::Builder::new()
        .name("emp-usage-history".into())
        .spawn(move || {
            let usage = &history_state.backend.usage;
            while !history_state.shutdown.load(Ordering::Acquire)
                && !usage.history.stop.load(Ordering::Acquire)
            {
                let revision = {
                    let mut progress = usage.progress.lock().expect("history refresh");
                    progress.acknowledged = progress.requested;
                    progress.requested
                };
                usage.publish_scan(&history_state, "usage_scan_started", revision, &json!({}));
                let config = history_state
                    .backend
                    .configuration
                    .config
                    .lock()
                    .ok()
                    .map(|config| config.clone())
                    .unwrap_or(json!({}));
                if usage.ledger.prices.set_aliases(&config["pricing_aliases"]) {
                    usage.ledger.price_pending(&history_state.shutdown);
                }
                usage.history.scan(
                    &usage.ledger,
                    &history_state.backend.accounts.codex_home,
                    &config,
                    system_now(),
                );
                if !history_state.shutdown.load(Ordering::Acquire)
                    && !usage.history.stop.load(Ordering::Acquire)
                {
                    usage.progress.lock().expect("history completion").completed = revision;
                }
                usage.publish_scan(
                    &history_state,
                    "usage_scan_finished",
                    revision,
                    &usage.history.status(),
                );
                usage.wait_for_scan(&history_state, revision);
            }
        })
        .inspect_err(|_| {
            state.backend.diagnostics.journal.event(
                "warning",
                "worker_spawn_failed",
                &json!({"worker":"usage_history"}),
            )
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
            while !price_state.shutdown.load(Ordering::Acquire)
                && !usage.history.stop.load(Ordering::Acquire)
            {
                let snapshot = usage.ledger.prices.snapshot(system_now());
                let delay = (snapshot["fetched_at"].as_f64().unwrap_or(0.0) + PRICE_INTERVAL
                    - system_now())
                .max(0.0);
                if delay > 0.0 {
                    usage.wait_for_prices(&price_state, delay.min(3600.0));
                    continue;
                }
                if refresh_prices(&price_state) {
                    usage.ledger.price_pending(&price_state.shutdown);
                } else if !price_state.shutdown.load(Ordering::Acquire) {
                    usage.ledger.prices.refresh_failed();
                    usage.wait_for_prices(&price_state, 3600.0);
                }
            }
        });
    let prices = match prices {
        Ok(worker) => worker,
        Err(error) => {
            state.backend.diagnostics.journal.event(
                "warning",
                "worker_spawn_failed",
                &json!({"worker":"price_refresh"}),
            );
            state.backend.usage.stop();
            let _ = history.join();
            return Err(error);
        }
    };
    Ok(vec![history, prices])
}
fn refresh_prices(state: &ServerState) -> bool {
    let started = std::time::Instant::now();
    let journal = &state.backend.diagnostics.journal;
    journal.event("info", "price_refresh_started", &json!({}));
    let result = fetch_prices(state);
    journal.event(if result.is_ok() {"info"} else {"warning"}, "price_refresh_finished", &json!({
        "success":result.is_ok(), "model_count":result.as_ref().ok(),
        "error_class":result.err().unwrap_or("none"), "duration_ms":started.elapsed().as_millis() as u64,
    }));
    result.is_ok()
}
fn fetch_prices(state: &ServerState) -> Result<usize, &'static str> {
    let fetched = state.backend.transport.runtime.block_on(async {
        let operation = async {
            let response = state
                .backend
                .transport
                .client
                .open(
                    emp_transport::HttpMethod::Get,
                    PRICE_URL,
                    BTreeMap::from([
                        ("User-Agent".into(), "EMP-price-catalog".into()),
                        ("Accept".into(), "application/json".into()),
                    ]),
                    None,
                    false,
                )
                .await
                .map_err(|_| "transport_error")?;
            if !(200..300).contains(&response.status()) {
                return Err("upstream_rejected");
            }
            let raw = response.read_limited(16 * 1024 * 1024).await.map_err(|_| "read_failed")?;
            normalize_prices(&serde_json::from_slice::<Value>(&raw).map_err(|_| "invalid_json")?).map_err(|_| "invalid_prices")
        };
        tokio::select! {
            result=tokio::time::timeout(Duration::from_secs(20),operation)=>result.map_err(|_| "timeout")?,
            _=state.wait_for_shutdown()=>Err("cancelled"),
        }
    });
    let prices = fetched?;
    let now = system_now();
    let catalog = &state.backend.usage.ledger.prices;
    emp_state::filesystem::atomic_write_private_state(
        catalog.path(),
        &serde_json::to_vec(&json!({"prices":prices,"fetched_at":now})).expect("price JSON"),
    )
    .map_err(|_| "storage_error")?;
    let count = prices.as_object().map_or(0, |prices| prices.len());
    catalog.replace(prices, now);
    Ok(count)
}
