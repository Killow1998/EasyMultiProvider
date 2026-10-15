//! Client-facing loopback URLs, independent of the numeric listener address.
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub(crate) fn codex_base_url(address: SocketAddr) -> String {
    base_url_for_platform(address, cfg!(windows))
}

fn base_url_for_platform(address: SocketAddr, windows: bool) -> String {
    // Windows proxy exception lists commonly name localhost. Some installed
    // Codex engines still proxy the numeric address despite a 127.* exception.
    if windows && address.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST) {
        format!("http://localhost:{}/v1", address.port())
    } else {
        format!("http://{address}/v1")
    }
}

pub(crate) fn is_previous_loopback_url(previous: Option<&str>, current: &str) -> bool {
    let Some(port) = current
        .strip_prefix("http://localhost:")
        .and_then(|value| value.strip_suffix("/v1"))
        .and_then(|value| value.parse::<u16>().ok())
    else {
        return false;
    };
    previous == Some(format!("http://127.0.0.1:{port}/v1").as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_advertises_localhost_without_changing_other_platforms() {
        let address = "127.0.0.1:4200".parse().unwrap();
        assert_eq!(
            base_url_for_platform(address, true),
            "http://localhost:4200/v1"
        );
        assert_eq!(
            base_url_for_platform(address, false),
            "http://127.0.0.1:4200/v1"
        );
        assert_eq!(
            base_url_for_platform("[::1]:4200".parse().unwrap(), true),
            "http://[::1]:4200/v1"
        );
        assert_eq!(
            base_url_for_platform("127.0.0.2:4200".parse().unwrap(), true),
            "http://127.0.0.2:4200/v1"
        );
    }

    #[test]
    fn migration_accepts_only_the_previous_exact_address_on_the_same_port() {
        let current = "http://localhost:4200/v1";
        assert!(is_previous_loopback_url(
            Some("http://127.0.0.1:4200/v1"),
            current
        ));
        for previous in [
            None,
            Some("http://127.0.0.1:4201/v1"),
            Some("http://localhost:4200/v1"),
            Some("http://127.0.0.2:4200/v1"),
            Some("http://127.0.0.1:4200/other"),
            Some("https://127.0.0.1:4200/v1"),
        ] {
            assert!(!is_previous_loopback_url(previous, current));
        }
        assert!(!is_previous_loopback_url(
            Some(current),
            "http://127.0.0.1:4200/v1"
        ));
    }
}
