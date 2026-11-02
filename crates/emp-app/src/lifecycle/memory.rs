//! Return free glibc heap pages between model requests.
use crate::app::ServerState;
use std::sync::Arc;
use std::time::Duration;

pub(super) fn run(state: Arc<ServerState>) {
    state.backend.transport.runtime.block_on(async {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                biased;
                _ = state.wait_for_shutdown() => return,
                _ = interval.tick() => {}
            }
            if state.backend.activity.is_idle() {
                // Idle is a scheduling hint; malloc_trim is thread-safe and
                // releases only whole free pages, including arena interiors.
                unsafe { libc::malloc_trim(0) };
            }
        }
    });
}
