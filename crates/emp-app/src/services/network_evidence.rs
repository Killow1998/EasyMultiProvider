//! Content-free startup network evidence shared by CLI status and support reports.
use emp_transport::{
    HttpClientPolicy, HttpMethod, ProxyEnvironment, ProxyOrigin, ProxyPolicy, ProxySource,
    TimeoutPolicy,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub(crate) struct NetworkSnapshot {
    pub(crate) source_at_startup: &'static str,
    chatgpt_route: &'static str,
    proxy_scheme: Option<String>,
}

impl NetworkSnapshot {
    pub(crate) fn capture(environment: &ProxyEnvironment, source: ProxySource) -> Self {
        let policy = HttpClientPolicy::new(
            ProxyPolicy::from_environment(environment.clone()),
            TimeoutPolicy::default(),
        );
        let route = policy
            .plan(
                HttpMethod::Get,
                "https://chatgpt.com",
                BTreeMap::new(),
                false,
            )
            .ok()
            .map(|plan| plan.route.proxy_origin);
        let (chatgpt_route, proxy_scheme) = match route {
            Some(ProxyOrigin::Direct) => ("direct", None),
            Some(ProxyOrigin::Proxy { origin, .. }) => {
                let scheme = origin
                    .split_once("://")
                    .map(|(scheme, _)| scheme.to_owned());
                ("proxy", scheme)
            }
            None => ("unknown", None),
        };
        Self {
            source_at_startup: source.as_str(),
            chatgpt_route,
            proxy_scheme,
        }
    }

    pub(crate) fn report(&self) -> Value {
        let allowed_schemes = ["http", "https", "socks4", "socks4a", "socks5", "socks5h"];
        let scheme = self
            .proxy_scheme
            .as_deref()
            .filter(|scheme| allowed_schemes.contains(scheme));
        json!({
            "source_at_startup": if ["environment", "system", "direct"].contains(&self.source_at_startup) { self.source_at_startup } else { "unknown" },
            "chatgpt_route": self.chatgpt_route,
            "proxy_scheme": if self.chatgpt_route == "proxy" { scheme } else { None },
            "connectivity_probe": "not_run",
        })
    }
}
