use super::{budget, fixture};
use crate::{
    modules::{hooks::HookOperation, process::ModuleProcess, registry::ModuleRegistry},
    services::observation::diagnostics::Diagnostics,
};
use emp_modules::{
    manifest::HookPhase::{self, CallFinished, CheckAccess, ProjectCatalog, ResolveRoute},
    peer::{CallError, Handlers, Peer, Side},
};
use emp_storage::diagnostics::ServiceOwner;
use emp_test_support::{fixture_dir, test_timeout};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{self, BufReader},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime},
};
use tempfile::TempDir;
use tokio::{
    sync::{mpsc as events, oneshot},
    task::JoinHandle,
    time::{sleep_until, timeout},
};

#[derive(Default, Deserialize, Serialize)]
struct NotificationReceipts {
    call_finished: usize,
    cancellations: Vec<SystemTime>,
}

pub(super) fn child(output: File) {
    let manifest: Value =
        serde_json::from_str(&std::env::var("EMP_TEST_HOOK_MANIFEST").unwrap()).unwrap();
    let behaviors: Value =
        serde_json::from_str(&std::env::var("EMP_TEST_HOOK_BEHAVIORS").unwrap()).unwrap();
    let (release, gated) = mpsc::channel();
    let gated = Arc::new(Mutex::new(gated));
    let mut handlers = Handlers::default();
    for phase in [CallFinished, CheckAccess, ProjectCatalog, ResolveRoute] {
        let method = phase.method();
        let behavior = behaviors[method].clone();
        let gated = gated.clone();
        handlers.register(method, move |params, token, peer| {
            peer.call(
                "host.hook",
                json!({"method":method,"params":params}),
                budget(),
            )
            .unwrap();
            if matches!(behavior.as_str(), Some("exit" | "block")) {
                gated
                    .lock()
                    .unwrap()
                    .recv_timeout(test_timeout(Duration::from_secs(25)).duration())
                    .unwrap();
                if behavior == "exit" {
                    std::process::exit(0);
                }
                peer.call(
                    "host.hook",
                    json!({"method":"cancelled","params":token.is_cancelled()}),
                    budget(),
                )
                .unwrap();
            }
            if behavior.is_null() || behavior == "block" {
                Ok(json!({"status":"declined"}))
            } else {
                Ok(behavior.clone())
            }
        });
    }
    handlers.register("module.initialize", move |_, _, _| {
        Ok(json!({"manifest": manifest}))
    });
    handlers.register("module.release", move |_, _, _| {
        release.send(()).unwrap();
        Ok(Value::Null)
    });
    let receipts = Arc::new(Mutex::new(NotificationReceipts::default()));
    let reported = receipts.clone();
    handlers.register("module.receipts", move |_, _, _| {
        Ok(serde_json::to_value(&*reported.lock().unwrap()).unwrap())
    });
    Peer::connect_recording_notifications(
        Side::Module,
        BufReader::new(io::stdin()),
        output,
        handlers,
        move |method| match method {
            "$/cancelRequest" => {
                let received = SystemTime::now();
                receipts.lock().unwrap().cancellations.push(received);
            }
            method if method == CallFinished.method() => {
                receipts.lock().unwrap().call_finished += 1;
            }
            _ => {}
        },
    )
    .unwrap()
    .wait()
    .unwrap();
}

struct Fixture {
    registry: Arc<ModuleRegistry>,
    diagnostics: Arc<Diagnostics>,
    report: events::UnboundedSender<Value>,
    seen: events::UnboundedReceiver<Value>,
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = fixture_dir().unwrap();
        let state_root = root.path().join("state");
        let mut diagnostics = Diagnostics::new(&state_root);
        diagnostics
            .journal
            .attach(Arc::new(ServiceOwner::acquire(&state_root).unwrap()));
        let (report, seen) = events::unbounded_channel();
        Self {
            registry: Arc::default(),
            diagnostics: Arc::new(diagnostics),
            report,
            seen,
            root,
        }
    }

    async fn install(
        &self,
        id: &str,
        generation: u64,
        phases: &[HookPhase],
        behaviors: Value,
    ) -> ModuleProcess {
        let manifest = json!({
            "id":id, "module_version":"1.0.0", "host_api":{"major":3,"min_minor":0},
            "contributions": phases.iter().map(|phase| json!({"type":"hook","phase":phase})).collect::<Vec<_>>()
        });
        let mut command = fixture("hooks");
        command.env("EMP_TEST_HOOK_MANIFEST", manifest.to_string());
        command.env("EMP_TEST_HOOK_BEHAVIORS", behaviors.to_string());
        let registry = self.registry.clone();
        let module = id.to_owned();
        let report = self.report.clone();
        let mut handlers = Handlers::default();
        handlers.register("host.hook", move |mut call, _, _| {
            assert!(registry.get(&module).is_some());
            call["module_id"] = json!(module);
            report.send(call).unwrap();
            Ok(Value::Null)
        });
        let registry = self.registry.clone();
        let module = id.to_owned();
        let (process, manifest) = ModuleProcess::start_with_connector(
            command,
            self.root.path(),
            handlers,
            budget(),
            Peer::connect,
            move || registry.revoke(&module, generation),
        )
        .await
        .unwrap();
        self.registry
            .publish(manifest, generation, &process, 0, 0)
            .unwrap();
        process
    }

    fn capture(&self, id: &str) -> Arc<HookOperation> {
        HookOperation::capture(&self.registry, &self.diagnostics, Some(id), None).unwrap()
    }

    async fn event(&mut self) -> Value {
        timeout(
            test_timeout(Duration::from_secs(10)).duration(),
            self.seen.recv(),
        )
        .await
        .unwrap()
        .unwrap()
    }

    fn failures(&self, operation_id: &str) -> Vec<(String, String)> {
        self.diagnostics.journal.flush_blocking();
        crate::tests::live_journal(self.root.path())
            .into_iter()
            .filter(|record| {
                record["event"] == "module_hook_failed"
                    && record["fields"]["operation_id"] == operation_id
            })
            .map(|record| {
                let fields = &record["fields"];
                (
                    fields["module_id"].as_str().unwrap().to_owned(),
                    fields["reason"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
}

fn dispatch(
    operation: Arc<HookOperation>,
    phase: HookPhase,
    input: Value,
) -> JoinHandle<Result<Option<bool>, ()>> {
    tokio::task::spawn_blocking(move || {
        operation.dispatch(phase, || input, |_, value| Ok(Some(value)))
    })
}

async fn fixture_call(process: &ModuleProcess, method: &'static str) -> Result<Value, CallError> {
    let connection = process.connection();
    tokio::task::spawn_blocking(move || connection.call(method, Value::Null, budget()))
        .await
        .unwrap()
}

async fn receipts(process: &ModuleProcess) -> NotificationReceipts {
    serde_json::from_value(fixture_call(process, "module.receipts").await.unwrap()).unwrap()
}

#[tokio::test]
async fn hooks_follow_id_order_and_allow_callbacks_during_dispatch() {
    let mut fixture = Fixture::new();
    assert!(HookOperation::capture(&fixture.registry, &fixture.diagnostics, None, None).is_none());
    let zeta = fixture
        .install(
            "zeta",
            1,
            &[CheckAccess],
            json!({(CheckAccess.method()):{"status":"claimed","value":false}}),
        )
        .await;
    let last = fixture
        .install("zzzz", 1, &[CheckAccess], Value::Null)
        .await;
    let alpha = fixture
        .install("alpha", 1, &[CheckAccess], Value::Null)
        .await;
    let operation = fixture.capture("ordered");
    operation.bind_caller("fixture");
    let input = json!({"model":"demo/model"});
    assert_eq!(
        dispatch(operation.clone(), CheckAccess, input.clone())
            .await
            .unwrap(),
        Ok(Some(false))
    );
    for id in ["alpha", "zeta"] {
        assert_eq!(
            fixture.event().await,
            json!({
                "module_id":id, "method":CheckAccess.method(),
                "params":{"operation_id":"ordered", "caller_id":"fixture", "input":input}
            })
        );
    }
    assert!(fixture.seen.try_recv().is_err());
    assert!(fixture.failures("ordered").is_empty());
    alpha.stop().await.unwrap();
    zeta.stop().await.unwrap();
    last.stop().await.unwrap();
}

#[tokio::test]
async fn call_finished_reaches_a_live_subscriber_after_the_first_exits() {
    let mut fixture = Fixture::new();
    let first = fixture
        .install("alpha", 1, &[CallFinished], Value::Null)
        .await;
    let unsubscribed = fixture
        .install("observer", 1, &[CheckAccess], Value::Null)
        .await;
    let later = fixture
        .install("zeta", 1, &[CallFinished], Value::Null)
        .await;
    let operation = fixture.capture("finished");
    operation.bind_caller("fixture");
    first.stop().await.unwrap();

    operation.notify_finished(json!({"result":"success"}));
    assert_eq!(receipts(&unsubscribed).await.call_finished, 0);
    assert_eq!(
        fixture.event().await,
        json!({
            "module_id":"zeta", "method":CallFinished.method(),
            "params":{"operation_id":"finished", "caller_id":"fixture", "input":{"result":"success"}}
        })
    );
    assert_eq!(
        fixture.failures("finished"),
        [("alpha".into(), "Exited".into())]
    );
    unsubscribed.stop().await.unwrap();
    later.stop().await.unwrap();
}

#[tokio::test]
async fn hooks_share_a_phase_deadline_and_report_timeout() {
    let mut fixture = Fixture::new();
    let first = fixture
        .install(
            "alpha",
            1,
            &[ProjectCatalog],
            json!({(ProjectCatalog.method()):"block"}),
        )
        .await;
    let later = fixture
        .install(
            "zeta",
            1,
            &[ProjectCatalog],
            json!({(ProjectCatalog.method()):"block"}),
        )
        .await;
    let phase_budget = Duration::from_secs(5);
    let operation = fixture.capture("deadline");
    let (start, started) = oneshot::channel();
    let running = tokio::task::spawn_blocking(move || {
        start.send((Instant::now(), SystemTime::now())).unwrap();
        operation.dispatch::<bool, _>(ProjectCatalog, || Value::Null, |_, value| Ok(Some(value)))
    });
    let (started, wall_started) = started.await.unwrap();
    let entered = fixture.event().await;
    assert_eq!(entered["module_id"], "alpha");
    assert_eq!(entered["method"], ProjectCatalog.method());
    sleep_until((started + phase_budget / 2).into()).await;
    fixture_call(&first, "module.release").await.unwrap();
    assert_eq!(
        fixture.event().await,
        json!({"module_id":"alpha","method":"cancelled","params":false})
    );
    let entered = fixture.event().await;
    assert_eq!(entered["module_id"], "zeta");
    assert_eq!(entered["method"], ProjectCatalog.method());
    assert_eq!(
        timeout(test_timeout(Duration::from_secs(10)).duration(), running)
            .await
            .unwrap()
            .unwrap(),
        Ok(None)
    );
    let received = receipts(&later).await.cancellations;
    let [received] = received.as_slice() else {
        panic!("expected exactly one cancellation frame, got {received:?}");
    };
    let elapsed = received.duration_since(wall_started).unwrap();
    let window = phase_budget..=phase_budget + phase_budget / 4;
    assert!(
        window.contains(&elapsed),
        "expected a cancellation frame in {window:?}, got {elapsed:?}"
    );
    assert_eq!(
        fixture.failures("deadline"),
        [("zeta".into(), "Timeout".into())]
    );
    first.stop().await.unwrap();
    later.stop().await.unwrap();
}

#[tokio::test]
async fn hook_exit_fails_closed_without_revoking_the_new_generation() {
    for phase in [CheckAccess, ResolveRoute] {
        let mut fixture = Fixture::new();
        let old = fixture
            .install("router", 1, &[phase], json!({(phase.method()):"exit"}))
            .await;
        let running = dispatch(fixture.capture("old"), phase, Value::Null);
        assert_eq!(fixture.event().await["params"]["operation_id"], "old");

        let new = fixture
            .install(
                "router",
                2,
                &[phase],
                json!({(phase.method()):{"status":"claimed","value":true}}),
            )
            .await;
        assert!(matches!(
            fixture_call(&old, "module.release").await,
            Ok(_) | Err(CallError::Disconnected)
        ));
        assert_eq!(running.await.unwrap(), Err(()), "{phase:?}");
        assert!(old.stop().await.unwrap().success());
        assert_eq!(
            fixture.failures("old"),
            [("router".into(), "Exited".into())]
        );

        assert_eq!(fixture.registry.get("router").unwrap().generation, 2);
        let running = dispatch(fixture.capture("new"), phase, Value::Null);
        assert_eq!(fixture.event().await["params"]["operation_id"], "new");
        assert_eq!(running.await.unwrap(), Ok(Some(true)));
        new.stop().await.unwrap();
    }
}

#[tokio::test]
async fn cancelling_a_hook_sends_the_peer_cancellation() {
    let mut fixture = Fixture::new();
    let process = fixture
        .install(
            "access",
            1,
            &[CheckAccess],
            json!({(CheckAccess.method()):"block"}),
        )
        .await;
    let operation = fixture.capture("cancel");
    let running = dispatch(operation.clone(), CheckAccess, Value::Null);
    assert_eq!(fixture.event().await["params"]["operation_id"], "cancel");
    operation.cancel();
    assert_eq!(running.await.unwrap(), Err(()));
    assert_eq!(
        fixture.failures("cancel"),
        [("access".into(), "Cancelled".into())]
    );

    fixture_call(&process, "module.release").await.unwrap();
    let cancelled = fixture.event().await;
    assert_eq!(cancelled["method"], "cancelled");
    assert_eq!(cancelled["params"], true);
    process.stop().await.unwrap();
}

#[tokio::test]
async fn malformed_hook_replies_fail_closed() {
    for reply in [
        json!({"status":"declined","value":true}),
        json!(["declined"]),
        json!(["claimed", true]),
    ] {
        let mut fixture = Fixture::new();
        let process = fixture
            .install(
                "access",
                1,
                &[CheckAccess],
                json!({(CheckAccess.method()):reply}),
            )
            .await;
        let running = dispatch(fixture.capture("invalid-reply"), CheckAccess, Value::Null);
        assert_eq!(
            fixture.event().await["params"]["operation_id"],
            "invalid-reply"
        );
        assert_eq!(running.await.unwrap(), Err(()), "{reply}");
        assert_eq!(
            fixture.failures("invalid-reply"),
            [("access".into(), "InvalidReply".into())]
        );
        process.stop().await.unwrap();
    }
}

#[tokio::test]
async fn oversized_hook_frame_does_not_fail_another_call_on_the_same_peer() {
    let mut fixture = Fixture::new();
    let process = fixture
        .install(
            "access",
            1,
            &[CheckAccess, ProjectCatalog],
            json!({(ProjectCatalog.method()):"block"}),
        )
        .await;
    let small = process
        .connection()
        .start_request(ProjectCatalog.method(), json!({"operation_id":"small"}))
        .unwrap();
    assert_eq!(fixture.event().await["params"]["operation_id"], "small");

    let input = Value::String("\"".repeat(8 * 1024 * 1024));
    let large = dispatch(fixture.capture("large"), CheckAccess, input);
    assert_eq!(large.await.unwrap(), Err(()));
    assert_eq!(
        fixture.failures("large"),
        [("access".into(), "InvalidFrame".into())]
    );
    fixture_call(&process, "module.release").await.unwrap();
    let completion = fixture.event().await;
    assert_eq!(completion["method"], "cancelled");
    assert_eq!(completion["params"], false);
    assert_eq!(small.wait(budget()).unwrap(), json!({"status":"declined"}));
    process.stop().await.unwrap();
}
