use emp_modules::team::DiscoveredWorkspace;
use serde::Deserialize;
use std::{
    io,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::Duration,
};
use tokio::{net::UdpSocket as AsyncSocket, runtime::Runtime, task::AbortHandle, time::timeout};

const PORT: u16 = 43219;
const QUERY: &[u8] = b"EMP-team-origin-v1";
const DISCOVERY_WINDOW: Duration = Duration::from_millis(900);

#[derive(Deserialize)]
struct Announcement {
    base: String,
    workspace: String,
}

pub(crate) struct AnnouncementSocket(UdpSocket);

impl AnnouncementSocket {
    pub(crate) fn bind() -> io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PORT))?;
        socket.set_nonblocking(true)?;
        Ok(Self(socket))
    }

    pub(crate) fn announce(
        self,
        runtime: &Runtime,
        networks: Vec<(Ipv4Addr, Ipv4Addr)>,
        response: String,
        active: Duration,
    ) -> io::Result<AbortHandle> {
        let socket = {
            let _runtime = runtime.enter();
            AsyncSocket::from_std(self.0)?
        };
        Ok(runtime
            .spawn(async move {
                let _ = timeout(active, async {
                    let mut buffer = [0; 128];
                    while let Ok((size, peer)) = socket.recv_from(&mut buffer).await {
                        if &buffer[..size] == QUERY && local_peer(peer, &networks) {
                            let _ = socket.send_to(response.as_bytes(), peer).await;
                        }
                    }
                })
                .await;
            })
            .abort_handle())
    }
}

pub(crate) async fn find_announcements(
    networks: &[(Ipv4Addr, Ipv4Addr)],
) -> io::Result<Vec<DiscoveredWorkspace>> {
    let socket = AsyncSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    for (ip, mask) in networks {
        let broadcast = Ipv4Addr::from(u32::from(*ip) | !u32::from(*mask));
        let _ = socket.send_to(QUERY, (broadcast, PORT)).await;
    }
    let mut buffer = [0; 1024];
    let mut found = Vec::new();
    let _ = timeout(DISCOVERY_WINDOW, async {
        while let Ok((size, peer)) = socket.recv_from(&mut buffer).await {
            if peer.port() != PORT || !local_peer(peer, networks) {
                continue;
            }
            if let Ok(info) = serde_json::from_slice::<Announcement>(&buffer[..size]) {
                found.push(DiscoveredWorkspace {
                    base: info.base,
                    name: info.workspace,
                });
            }
        }
    })
    .await;
    Ok(found)
}

pub(crate) fn local_networks() -> io::Result<Vec<(Ipv4Addr, Ipv4Addr)>> {
    Ok(if_addrs::get_if_addrs()?
        .into_iter()
        .filter_map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(addr)
                if addr.ip.is_private()
                    && addr.broadcast.is_some()
                    && matches!(
                        interface.oper_status,
                        if_addrs::IfOperStatus::Up | if_addrs::IfOperStatus::Unknown
                    ) =>
            {
                Some((addr.ip, addr.netmask))
            }
            _ => None,
        })
        .collect())
}

fn local_peer(peer: SocketAddr, networks: &[(Ipv4Addr, Ipv4Addr)]) -> bool {
    let SocketAddr::V4(peer) = peer else {
        return false;
    };
    networks.iter().any(|(ip, mask)| {
        let mask = u32::from(*mask);
        u32::from(*peer.ip()) & mask == u32::from(*ip) & mask
    })
}
