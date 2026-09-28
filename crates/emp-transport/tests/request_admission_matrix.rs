//! Admission decisions users observe: growth, pressure, hard limits, and the
//! diagnostic snapshot EMP reports when requests are reshaped by memory
//! pressure, with direct expectations over the same boundary.

use emp_transport::{MemoryStatus, RequestLimits, RequestLimitsConfig, TransportKind};

fn limits(available: usize) -> std::sync::Arc<RequestLimits> {
    RequestLimits::new(
        RequestLimitsConfig {
            baseline: 64,
            maximum: 256,
            growth_quantum: 64,
            memory_headroom: 0,
        },
        move || Some(MemoryStatus::available(available)),
        || 0,
        "run-test",
    )
    .expect("limits")
}

#[test]
fn admission_tracks_two_transports_and_the_snapshot_reports_pressure() {
    let limits = limits(2_048);
    let mut first = limits.request(TransportKind::Http);
    let mut second = limits.request(TransportKind::WebSocket);

    first.ensure(64).expect("baseline admission");
    first.ensure(65).expect("first growth");
    first.ensure(129).expect("second growth");

    let memory = second
        .ensure(65)
        .expect_err("second transport is out of memory");
    assert_eq!(
        memory.reason,
        emp_transport::RequestCapacityReason::MemoryLimit
    );
    assert_eq!(
        memory.limit, 128,
        "the blocked target is the quantized size"
    );
    assert_eq!(memory.available_bytes, 2_048);
    assert_eq!(memory.required_memory_bytes, 128 * 8 + 192 * 8);
    assert_eq!(memory.http_status(), 503);

    // Releasing the first transport hands the freed memory to the second.
    first.release();
    second.ensure(65).expect("freed memory is reusable");

    let mut third = limits.request(TransportKind::Http);
    let hard = third.ensure(257).expect_err("beyond the hard maximum");
    assert_eq!(hard.reason, emp_transport::RequestCapacityReason::HardLimit);
    assert_eq!(hard.limit, 256, "hard limit reports the configured maximum");
    assert_eq!(hard.available_bytes, 0);
    assert_eq!(hard.required_memory_bytes, 0);
    let snapshot = limits.snapshot().expect("admission snapshot");
    assert_eq!(
        snapshot.reserved_bytes,
        128 * 8,
        "only the websocket reservation remains"
    );
    let notices = snapshot
        .notices
        .iter()
        .map(|notice| {
            (
                notice.kind,
                notice.transport,
                notice.target_bytes,
                notice.limit_bytes,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        notices,
        [
            ("expanded", "http", 128, 128),
            ("expanded", "http", 192, 192),
            ("blocked", "websocket", 128, 64),
            ("expanded", "websocket", 128, 128),
            ("blocked", "http", 256, 256),
        ],
        "every growth and every blocked attempt is reported in order"
    );
}
