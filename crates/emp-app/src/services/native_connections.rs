//! Bounded retry suppression shared by downstream WebSocket connections.
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Transient only, never serialized or included in diagnostics.
pub(crate) type RouteKey = (String, Option<String>, BTreeMap<String, String>);

#[derive(Default)]
pub(crate) struct NativeConnections {
    cooldowns: Mutex<BTreeMap<RouteKey, Instant>>,
}

impl NativeConnections {
    pub(crate) fn allowed(&self, route: &RouteKey) -> bool {
        let mut cooldowns = self.cooldowns.lock().expect("native connection cooldowns");
        cooldowns.retain(|_, deadline| *deadline > Instant::now());
        !cooldowns.contains_key(route)
    }

    pub(crate) fn defer(&self, route: RouteKey) {
        let mut cooldowns = self.cooldowns.lock().expect("native connection cooldowns");
        if cooldowns.len() >= 32 {
            let oldest = cooldowns
                .iter()
                .min_by_key(|(_, deadline)| **deadline)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                cooldowns.remove(&oldest);
            }
        }
        cooldowns.insert(route, Instant::now() + Duration::from_secs(30));
    }

    pub(crate) fn available(&self, route: &RouteKey) {
        self.cooldowns
            .lock()
            .expect("native connection cooldowns")
            .remove(route);
    }
}
