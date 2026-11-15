use crate::app::ServerState;
use emp_codex::history::repair::IdleHistoryRepair;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub(crate) fn run(state: Arc<ServerState>) {
    state.backend.transport.runtime.block_on(async {
        let integration = &state.backend.integration;
        let mut repair = None;
        let mut delay = Duration::from_secs(15);
        let mut reasons = BTreeMap::<String, usize>::new();
        loop {
            tokio::select! {
                biased;
                _ = state.wait_for_shutdown() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            if !integration.owned.load(Ordering::Acquire) || !state.backend.activity.is_idle() {
                delay = Duration::from_secs(2);
                continue;
            }
            if !integration
                .manager
                .status()
                .is_ok_and(|status| status.same_instance && status.state == "active")
            {
                delay = Duration::from_secs(60);
                continue;
            }
            let worker = match &mut repair {
                Some(worker) => worker,
                pending @ None => match IdleHistoryRepair::new(
                    &integration.codex_home,
                    Arc::clone(&state.shutdown),
                ) {
                    Ok(worker) => pending.insert(worker),
                    Err(_) => {
                        delay = Duration::from_secs(60);
                        continue;
                    }
                },
            };
            let started = Instant::now();
            delay = Duration::from_millis(500);
            match worker.step() {
                Ok(Some(report)) => {
                    for (reason, count) in report
                        .deferred_reasons
                        .iter()
                        .chain(&report.skipped_reasons)
                    {
                        *reasons.entry(reason.clone()).or_default() += count;
                    }
                    if report.checkpoints_converted > 0 {
                        state.backend.diagnostics.journal.event(
                            "info",
                            "history_checkpoints_converted",
                            &serde_json::json!({"threads":report.threads_repaired,
                                "checkpoints":report.checkpoints_converted,
                                "elapsed_ms":started.elapsed().as_millis()}),
                        );
                    }
                }
                Ok(None) => {
                    if !reasons.is_empty() {
                        state.backend.diagnostics.journal.event(
                            "debug",
                            "history_migration_deferred",
                            &serde_json::json!({"reasons":reasons}),
                        );
                        reasons.clear();
                    }
                    if worker.refresh().is_err() {
                        repair = None;
                    }
                    delay = Duration::from_secs(60);
                }
                Err(error) if error.reason() == "history_repair_cancelled" => return,
                Err(error) => {
                    *reasons.entry(error.reason().to_owned()).or_default() += 1;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_wait_is_cancelled_immediately_on_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.json");
        std::fs::write(&config, b"{\"auto_enable_on_start\":false}").unwrap();
        let server = crate::lifecycle::ServerHandle::start_with_config(
            "127.0.0.1".parse().unwrap(),
            0,
            &config,
        )
        .unwrap();
        let state = Arc::clone(&server.state);
        let worker = std::thread::spawn(move || run(state));
        let started = Instant::now();
        server.state.request_shutdown();
        worker.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        server.shutdown().unwrap();
    }
}
