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
                // SAFETY: malloc_trim is thread-safe and only releases free allocator pages.
                unsafe { libc::malloc_trim(0) };
            }
        }
    });
}
