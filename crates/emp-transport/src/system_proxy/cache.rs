//! One system-proxy reader at a time; warm readers retain the last valid snapshot.
use super::{ProxyEnvironment, SYSTEM_PROXY_READ_TIMEOUT};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
const SYSTEM_PROXY_CACHE_TTL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct SystemProxyCache {
    state: Mutex<ProxyCacheState>,
    ready: Condvar,
}

#[derive(Default)]
struct ProxyCacheState {
    value: Option<ProxyEnvironment>,
    last_attempt: Option<Instant>,
    reading: bool,
}

impl SystemProxyCache {
    pub(super) fn resolve_with<C, R>(&self, clock: C, reader: R) -> Option<ProxyEnvironment>
    where
        C: Fn() -> Instant,
        R: FnOnce() -> Result<Option<ProxyEnvironment>, ()>,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.reading {
            // Only a cold cache waits: sending direct before the first proxy
            // answer can make startup requests fail on proxy-only networks.
            if state.last_attempt.is_none() {
                state = self
                    .ready
                    .wait_timeout_while(state, SYSTEM_PROXY_READ_TIMEOUT, |state| state.reading)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            return state.value.clone();
        }
        let now = clock();
        let is_fresh = state
            .last_attempt
            .and_then(|last| now.checked_duration_since(last))
            .is_some_and(|elapsed| elapsed < SYSTEM_PROXY_CACHE_TTL);
        if is_fresh {
            return state.value.clone();
        }
        state.reading = true;
        drop(state);
        let result = reader();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Ok(entry) = result {
            state.value = entry;
        }
        state.last_attempt = Some(clock());
        state.reading = false;
        self.ready.notify_all();
        state.value.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn system_proxy_cache_uses_ttl_and_keeps_last_valid_on_error() {
        let cache = SystemProxyCache::default();
        let start = Instant::now();
        let calls = std::cell::Cell::new(0);
        let first = ProxyEnvironment {
            https: Some("http://first.system:8080".to_owned()),
            ..ProxyEnvironment::default()
        };
        assert_eq!(
            cache.resolve_with(
                || start,
                || {
                    calls.set(calls.get() + 1);
                    Ok(Some(first.clone()))
                }
            ),
            Some(first.clone())
        );
        assert_eq!(
            cache.resolve_with(
                || start + Duration::from_millis(999),
                || -> Result<Option<ProxyEnvironment>, ()> { panic!("fresh cache must not read") }
            ),
            Some(first)
        );
        let refreshed = ProxyEnvironment {
            https: Some("http://second.system:8080".to_owned()),
            ..ProxyEnvironment::default()
        };
        assert_eq!(
            cache.resolve_with(
                || start + Duration::from_secs(1),
                || {
                    calls.set(calls.get() + 1);
                    Ok(Some(refreshed.clone()))
                }
            ),
            Some(refreshed.clone())
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(
            cache.resolve_with(|| start + Duration::from_secs(2), || Err(())),
            Some(refreshed.clone()),
            "a failed read keeps the last valid proxy snapshot"
        );
        assert_eq!(
            cache.resolve_with(|| start + Duration::from_secs(3), || Ok(None)),
            None,
            "a successful direct read clears the previous proxy"
        );
    }

    #[test]
    fn refreshing_system_proxy_does_not_block_cached_readers() {
        use std::sync::{Arc, mpsc};
        let cache = Arc::new(SystemProxyCache::default());
        let start = Instant::now();
        let first = ProxyEnvironment {
            https: Some("http://cached.system:8080".into()),
            ..Default::default()
        };
        cache.resolve_with(|| start, || Ok(Some(first.clone())));
        let (started, running) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let worker_cache = Arc::clone(&cache);
        let worker = thread::spawn(move || {
            worker_cache.resolve_with(
                || start + Duration::from_secs(2),
                || {
                    started.send(()).unwrap();
                    let _ = blocked.recv();
                    Ok(None)
                },
            )
        });
        running.recv_timeout(Duration::from_secs(2)).unwrap();
        let (sent, received) = mpsc::channel();
        let reader_cache = Arc::clone(&cache);
        let reader = thread::spawn(move || {
            sent.send(reader_cache.resolve_with(
                || start + Duration::from_secs(2),
                || panic!("duplicate refresh"),
            ))
            .unwrap();
        });
        assert_eq!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            Some(first)
        );
        release.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), None);
        reader.join().unwrap();
        assert_eq!(
            cache.resolve_with(
                || start + Duration::from_secs(2),
                || panic!("fresh direct cache")
            ),
            None
        );
    }

    #[test]
    fn cold_system_proxy_readers_share_the_first_answer() {
        use std::sync::{Arc, mpsc};
        let cache = Arc::new(SystemProxyCache::default());
        let (started, running) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let first = ProxyEnvironment {
            https: Some("http://first.system:8080".into()),
            ..Default::default()
        };
        let expected = first.clone();
        let worker_cache = Arc::clone(&cache);
        let worker = thread::spawn(move || {
            worker_cache.resolve_with(Instant::now, || {
                started.send(()).unwrap();
                let _ = blocked.recv();
                Ok(Some(first))
            })
        });
        running.recv_timeout(Duration::from_secs(2)).unwrap();
        let reader_cache = Arc::clone(&cache);
        let reader = thread::spawn(move || {
            reader_cache.resolve_with(Instant::now, || panic!("duplicate cold read"))
        });
        release.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), Some(expected.clone()));
        assert_eq!(reader.join().unwrap(), Some(expected));
    }
}
