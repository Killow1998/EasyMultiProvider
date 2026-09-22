use emp_transport::{
    MemoryStatus, RequestCapacityReason, RequestLimits, RequestLimitsConfig, TransportKind,
};
use std::sync::{Arc, Barrier};

fn limits(available: usize) -> Arc<RequestLimits> {
    RequestLimits::new(
        RequestLimitsConfig {
            baseline: 64,
            maximum: 256,
            growth_quantum: 64,
            memory_headroom: 0,
        },
        move || Some(MemoryStatus::available(available)),
        || 1_790_000_000,
        "run-test",
    )
    .expect("limits")
}

#[test]
fn growth_is_quantized_per_request_and_drop_releases_reservation() {
    let limits = limits(16_384);
    {
        let mut first = limits.request(TransportKind::Http);
        first.ensure(64).expect("baseline");
        assert!(limits.snapshot().unwrap().notices.is_empty());
        first.ensure(65).expect("first growth");
        assert_eq!(first.limit(), 128);
        first.ensure(129).expect("second growth");
        assert_eq!(first.limit(), 192);
        assert_eq!(first.reserved(), 192 * 8);
        assert_eq!(limits.request(TransportKind::Http).limit(), 64);
        assert_eq!(limits.snapshot().unwrap().reserved_bytes, 192 * 8);
    }
    assert_eq!(limits.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn concurrent_requests_cannot_spend_the_same_available_memory() {
    let limits = limits(2_048);
    let barrier = Arc::new(Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let limits = Arc::clone(&limits);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut budget = limits.request(TransportKind::Http);
                barrier.wait();
                let admitted = budget.ensure(65).is_ok();
                barrier.wait();
                admitted
            })
        })
        .collect::<Vec<_>>();
    let admitted = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|admitted| *admitted)
        .count();
    assert_eq!(admitted, 2);
    assert_eq!(limits.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn hard_and_memory_limits_have_distinct_retry_semantics() {
    let limits = limits(100);
    let mut budget = limits.request(TransportKind::WebSocket);
    let memory = budget.ensure(65).expect_err("memory pressure");
    assert_eq!(memory.reason, RequestCapacityReason::MemoryLimit);
    assert!(!memory.decoded);
    assert_eq!(memory.http_status(), 503);
    assert_eq!(memory.websocket_close_code(), 1013);
    assert_eq!(memory.required_memory_bytes, 1024);
    assert_eq!(budget.limit(), 64);

    let hard = budget.ensure(257).expect_err("hard limit");
    assert_eq!(hard.reason, RequestCapacityReason::HardLimit);
    assert_eq!(hard.http_status(), 413);
    assert_eq!(hard.websocket_close_code(), 1009);
    assert_eq!(hard.limit, 256);
}

#[test]
fn missing_memory_information_fails_closed_without_reserving() {
    let limits = RequestLimits::new(
        RequestLimitsConfig {
            baseline: 64,
            maximum: 256,
            growth_quantum: 64,
            memory_headroom: 0,
        },
        || None,
        || 1_790_000_000,
        "run-test",
    )
    .unwrap();
    let mut budget = limits.request(TransportKind::Http);
    let error = budget.ensure(65).expect_err("missing memory data");
    assert_eq!(error.reason, RequestCapacityReason::MemoryLimit);
    assert_eq!(budget.limit(), 64);
    assert_eq!(limits.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn notice_history_is_bounded_and_contains_no_request_content() {
    let limits = limits(16_384);
    for _ in 0..25 {
        let mut budget = limits.request(TransportKind::Http);
        budget.ensure(65).expect("growth");
    }
    let snapshot = limits.snapshot().unwrap();
    assert_eq!(snapshot.notices.len(), 20);
    assert_eq!(snapshot.notices.first().unwrap().id, 6);
    assert!(
        serde_json::to_string(&snapshot)
            .unwrap()
            .contains("expanded")
    );
}

#[test]
fn realistic_growth_uses_quantization_instead_of_doubling() {
    let mib = 1024 * 1024;
    let limits = RequestLimits::new(
        RequestLimitsConfig::default(),
        move || Some(MemoryStatus::available(1_992_433_664)),
        || 1_790_000_000,
        "run-test",
    )
    .unwrap();
    let mut budget = limits.request(TransportKind::Http);
    budget.ensure(86 * mib).expect("96 MiB growth");
    assert_eq!(budget.limit(), 96 * mib);
    assert_eq!(budget.reserved(), 96 * mib * 8);
}
