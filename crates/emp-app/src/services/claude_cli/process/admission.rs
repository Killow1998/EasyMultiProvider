//! One resource gate for inference, authentication and quota CLI processes.
use super::{Cancellation, CancellationReason};
use emp_transport::MemoryStatus;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

const PROCESS_BYTES: usize = 192 * 1024 * 1024;
const HEADROOM_BYTES: usize = 128 * 1024 * 1024;
const MAX_WAIT: Duration = Duration::from_secs(15);
static ADMISSION: OnceLock<Admission> = OnceLock::new();

#[derive(Default)]
struct Admission {
    active: Mutex<usize>,
    changed: Condvar,
}

pub(super) struct Permit<'a>(&'a Admission);

pub(super) fn acquire(
    cancelled: &Cancellation,
    cancellation: &mut impl FnMut() -> Option<CancellationReason>,
) -> Result<Permit<'static>, &'static str> {
    ADMISSION.get_or_init(Admission::default).acquire(
        Instant::now() + MAX_WAIT,
        || {
            if let Some(reason) = cancellation() {
                return Some(super::cancellation_error(reason));
            }
            cancelled.is_cancelled().then_some("claude_cli_interrupted")
        },
        emp_transport::system_memory_status,
    )
}

impl Admission {
    fn acquire(
        &self,
        deadline: Instant,
        mut cancellation: impl FnMut() -> Option<&'static str>,
        memory: impl Fn() -> Option<MemoryStatus>,
    ) -> Result<Permit<'_>, &'static str> {
        loop {
            if let Some(reason) = cancellation() {
                return Err(reason);
            }
            let available = memory();
            // Reserve estimated memory for admitted children as well: several
            // spawns can arrive before the OS accounts for their allocations.
            let maximum = available.map_or(1, |status| {
                status
                    .available
                    .min(status.total.unwrap_or(usize::MAX))
                    .saturating_sub(HEADROOM_BYTES)
                    / PROCESS_BYTES
            });
            let mut active = self
                .active
                .lock()
                .map_err(|_| "claude_cli_capacity_unavailable")?;
            if *active < maximum {
                *active += 1;
                return Ok(Permit(self));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("claude_cli_capacity_unavailable");
            }
            let _ = self
                .changed
                .wait_timeout(active, remaining.min(Duration::from_millis(50)))
                .map_err(|_| "claude_cli_capacity_unavailable")?;
        }
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.0.active.lock() {
            *active = active.saturating_sub(1);
            self.0.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn limited_memory() -> Option<MemoryStatus> {
        Some(MemoryStatus {
            total: Some(16 * 1024 * 1024 * 1024),
            ..MemoryStatus::available(512 * 1024 * 1024)
        })
    }

    #[test]
    fn six_jobs_share_two_slots_and_release_them_on_failure() {
        let gate = Admission::default();
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _index in 0..6 {
                let (gate, running, peak) = (&gate, &running, &peak);
                scope.spawn(move || {
                    let _permit = gate
                        .acquire(
                            Instant::now() + Duration::from_secs(2),
                            || None,
                            limited_memory,
                        )
                        .unwrap();
                    let active = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(active, Ordering::SeqCst);
                    #[cfg(unix)]
                    {
                        let status = std::process::Command::new("/bin/sh")
                            .args([
                                "-c",
                                if _index == 0 {
                                    "sleep 0.02; exit 1"
                                } else {
                                    "sleep 0.02"
                                },
                            ])
                            .status()
                            .unwrap();
                        assert_eq!(status.success(), _index != 0);
                    }
                    #[cfg(not(unix))]
                    std::thread::sleep(Duration::from_millis(20));
                    running.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(*gate.active.lock().unwrap(), 0);
    }

    #[test]
    fn full_or_low_memory_waits_are_bounded_and_cancellable() {
        let gate = Admission::default();
        let first = gate
            .acquire(Instant::now(), || None, limited_memory)
            .unwrap();
        let second = gate
            .acquire(Instant::now(), || None, limited_memory)
            .unwrap();
        assert_eq!(
            gate.acquire(Instant::now(), || None, limited_memory).err(),
            Some("claude_cli_capacity_unavailable")
        );
        assert_eq!(
            gate.acquire(
                Instant::now(),
                || Some("claude_cli_disconnected"),
                limited_memory
            )
            .err(),
            Some("claude_cli_disconnected")
        );
        drop((first, second));
        assert_eq!(
            gate.acquire(
                Instant::now(),
                || None,
                || Some(MemoryStatus::available(16 * 1024 * 1024))
            )
            .err(),
            Some("claude_cli_capacity_unavailable")
        );
        assert!(
            gate.acquire(Instant::now(), || None, limited_memory)
                .is_ok()
        );
    }
}
