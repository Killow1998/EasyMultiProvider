//! Detect usable local networks without opening a shell or sending discovery traffic.
use if_addrs::{IfOperStatus, Interface};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};

fn route_address() -> Option<Ipv4Addr> {
    // UDP connect selects a route without sending a packet.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) => Some(ip),
        _ => None,
    }
}

fn select(interfaces: Vec<Interface>, route: Option<Ipv4Addr>) -> Option<Ipv4Addr> {
    interfaces
        .into_iter()
        .filter_map(|interface| {
            let IpAddr::V4(address) = interface.ip() else {
                return None;
            };
            if !address.is_private()
                || !matches!(
                    interface.oper_status,
                    IfOperStatus::Up | IfOperStatus::Unknown
                )
            {
                return None;
            }
            let name = interface.name.to_lowercase();
            let virtual_network = interface.is_p2p()
                || [
                    "tun",
                    "tap",
                    "utun",
                    "wg",
                    "tailscale",
                    "docker",
                    "veth",
                    "br-",
                    "virbr",
                ]
                .iter()
                .any(|prefix| name.starts_with(prefix))
                || ["vpn", "clash", "wireguard", "vethernet"]
                    .iter()
                    .any(|part| name.contains(part));
            Some((virtual_network, Some(address) != route, address))
        })
        // Prefer a LAN interface over a VPN/TUN default route.
        .min()
        .map(|(_, _, address)| address)
}

pub(super) fn address() -> Option<Ipv4Addr> {
    select(
        if_addrs::get_if_addrs().unwrap_or_default(),
        route_address(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn interface(name: &str, ip: &str, up: bool) -> Interface {
        Interface {
            name: name.into(),
            addr: if_addrs::IfAddr::V4(if_addrs::Ifv4Addr {
                ip: ip.parse().unwrap(),
                netmask: Ipv4Addr::new(255, 255, 255, 0),
                prefixlen: 24,
                broadcast: None,
            }),
            index: None,
            oper_status: if up {
                IfOperStatus::Up
            } else {
                IfOperStatus::Down
            },
            is_p2p: name.starts_with("tun"),
            #[cfg(windows)]
            adapter_name: name.into(),
        }
    }

    #[test]
    fn lan_is_preferred_over_a_tunnel_default_route() {
        let address = select(
            vec![
                interface("tun0", "10.0.0.1", true),
                interface("Wi-Fi", "192.168.1.5", true),
            ],
            Some(Ipv4Addr::new(10, 0, 0, 1)),
        );
        assert_eq!(address, Some(Ipv4Addr::new(192, 168, 1, 5)));
    }

    #[test]
    fn unusable_addresses_are_not_offered() {
        let address = select(
            vec![
                interface("lo", "127.0.0.1", true),
                interface("offline", "192.168.2.4", false),
                interface("public", "8.8.8.8", true),
            ],
            None,
        );
        assert_eq!(address, None);
    }
}
