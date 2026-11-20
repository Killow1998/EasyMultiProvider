use super::{Failure, Result, policy};
use crate::app::ServerState;
use emp_modules::team::{DiscoveredWorkspace, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::Mutex,
    time::Duration,
};
use tokio::{net::UdpSocket as AsyncSocket, task::AbortHandle, time::timeout};

const PORT: u16 = 43219;
const QUERY: &[u8] = b"EMP-team-origin-v1";
const DISCOVERY_WINDOW: Duration = Duration::from_millis(900);

#[derive(Deserialize)]
struct Announcement {
    base: String,
    workspace: String,
}

#[derive(Default)]
pub(super) struct Discovery(Mutex<Option<AbortHandle>>);

impl Discovery {
    pub(super) fn announce(&self, state: &ServerState, origin: &str, reply: &Value) -> Result<()> {
        let mut announcement = self.0.lock().map_err(|_| Failure::unavailable())?;
        if announcement
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return Ok(());
        }
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PORT)).map_err(network_error)?;
        socket.set_nonblocking(true).map_err(network_error)?;
        let networks = local_networks()?;
        let response = json!({"base":origin,"workspace":reply["workspace"]}).to_string();
        let active =
            Duration::from_secs(reply["expires_in"].as_u64().ok_or_else(Failure::invalid)?);
        let runtime = &state.backend.transport.runtime;
        let socket = {
            let _runtime = runtime.enter();
            AsyncSocket::from_std(socket).map_err(network_error)?
        };
        let task = runtime.spawn(async move {
            let _ = timeout(active, async {
                let mut buffer = [0; 128];
                while let Ok((size, peer)) = socket.recv_from(&mut buffer).await {
                    if &buffer[..size] == QUERY && local_peer(peer, &networks) {
                        let _ = socket.send_to(response.as_bytes(), peer).await;
                    }
                }
            })
            .await;
        });
        *announcement = Some(task.abort_handle());
        Ok(())
    }

    pub(super) fn find(&self, state: &ServerState) -> Result<Value> {
        let networks = local_networks()?;
        let found = state
            .backend
            .transport
            .runtime
            .block_on(async {
                let socket = AsyncSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
                socket.set_broadcast(true)?;
                for (ip, mask) in &networks {
                    let broadcast = Ipv4Addr::from(u32::from(*ip) | !u32::from(*mask));
                    let _ = socket.send_to(QUERY, (broadcast, PORT)).await;
                }
                let mut buffer = [0; 1024];
                let mut found = Vec::new();
                let _ = timeout(DISCOVERY_WINDOW, async {
                    while let Ok((size, peer)) = socket.recv_from(&mut buffer).await {
                        if peer.port() != PORT || !local_peer(peer, &networks) {
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
            })
            .map_err(network_error)?;
        let workspaces: Vec<DiscoveredWorkspace> =
            policy(state, Request::Discovery { candidates: found })?;
        Ok(json!({"workspaces":workspaces}))
    }

    pub(super) fn stop(&self) {
        if let Ok(mut announcement) = self.0.lock()
            && let Some(task) = announcement.take()
        {
            task.abort();
        }
    }
}

fn local_networks() -> Result<Vec<(Ipv4Addr, Ipv4Addr)>> {
    Ok(if_addrs::get_if_addrs()
        .map_err(network_error)?
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

fn network_error(_: std::io::Error) -> Failure {
    Failure(503, "workspace_discovery_unavailable".into())
}
