//! One-use LAN delivery of an existing encrypted migration package. The code
//! carries the endpoint and secret; the wire carries a proof, never that secret.
mod network;
mod wire;
use super::{ExportOptions, MigrationError};
use crate::app::ServerState;
use serde_json::{Value, json};
use std::net::{Ipv4Addr, TcpListener};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CODE_LIFETIME: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub(crate) struct LanMigration(Mutex<Option<Sender>>);

impl LanMigration {
    pub(crate) fn cancel(&self) {
        if let Ok(mut sender) = self.0.lock() {
            sender.take();
        }
    }
}
struct Sender {
    code: wire::Code,
    expires: Instant,
    cancel: Arc<AtomicBool>,
    shared: Arc<Mutex<Delivery>>,
    worker: Option<JoinHandle<()>>,
}
struct Delivery {
    status: &'static str,
    receiver: Option<(std::net::IpAddr, [u8; 32])>,
    approved: bool,
    bundle: Option<Vec<u8>>,
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn receive(
    state: &ServerState,
    session: &str,
    code: &str,
    receiver: &str,
) -> Result<Value, MigrationError> {
    let code = wire::Code::parse(code).map_err(MigrationError::invalid)?;
    let nonce = wire::nonce(receiver).map_err(MigrationError::invalid)?;
    match wire::receive(&code, &nonce).map_err(MigrationError::invalid)? {
        Some(bundle) => state.backend.migration_previews.decode(
            session,
            &bundle,
            &code.password(),
            state.backend.configuration.official_models,
        ),
        None => Ok(json!({"status":"waiting"})),
    }
}

pub(crate) enum SenderCommand {
    Start {
        address: Option<String>,
        options: ExportOptions,
    },
    Cancel,
    Approve(String),
    Status,
}

pub(crate) fn send(state: &ServerState, command: SenderCommand) -> Result<Value, MigrationError> {
    let mut sender = state
        .backend
        .lan_migration
        .0
        .lock()
        .map_err(|_| MigrationError::Internal)?;
    match command {
        SenderCommand::Start { address, options } => {
            if options
                .selection
                .as_ref()
                .is_none_or(|selection| selection.items.is_empty())
            {
                return Err(MigrationError::invalid("Select services"));
            }
            if sender.as_ref().is_some_and(|sender| {
                sender.expires > Instant::now() && !sender.cancel.load(Ordering::Acquire)
            }) {
                return Err(MigrationError::invalid("lan_already_active"));
            }
            let address: Ipv4Addr = match address {
                Some(value) => value
                    .parse()
                    .map_err(|_| MigrationError::invalid("lan_address_required"))?,
                None => network::address()
                    .ok_or_else(|| MigrationError::invalid("lan_address_required"))?,
            };
            if !wire::local_address(address) {
                return Err(MigrationError::invalid("lan_address_required"));
            }
            let listener = TcpListener::bind((address, 0))
                .map_err(|_| MigrationError::invalid("lan_cannot_listen"))?;
            listener
                .set_nonblocking(true)
                .map_err(|_| MigrationError::Internal)?;
            let code = wire::Code::new(
                address,
                listener
                    .local_addr()
                    .map_err(|_| MigrationError::Internal)?
                    .port(),
            )
            .map_err(MigrationError::invalid)?;
            let exported = super::export(None, state, &code.password(), &options)?;
            if exported.bundle.len() > wire::MAX_BUNDLE {
                return Err(MigrationError::invalid("lan_package_too_large"));
            }
            let expires = Instant::now() + CODE_LIFETIME;
            let shared = Arc::new(Mutex::new(Delivery {
                status: "waiting",
                receiver: None,
                approved: false,
                bundle: Some(exported.bundle),
            }));
            let cancel = Arc::new(AtomicBool::new(false));
            let worker = wire::serve(
                listener,
                code.clone(),
                expires,
                shared.clone(),
                cancel.clone(),
            );
            *sender = Some(Sender {
                code,
                expires,
                shared,
                cancel,
                worker: Some(worker),
            });
        }
        SenderCommand::Cancel => {
            *sender = None;
            return Ok(json!({"status":"cancelled"}));
        }
        SenderCommand::Approve(code) => {
            let sender = sender
                .as_ref()
                .ok_or_else(|| MigrationError::invalid("lan_expired"))?;
            if code != sender.code.encode() {
                return Err(MigrationError::invalid("lan_expired"));
            }
            let mut delivery = sender.shared.lock().map_err(|_| MigrationError::Internal)?;
            if sender.expires <= Instant::now()
                || delivery.receiver.is_none()
                || delivery.status != "confirmation"
            {
                return Err(MigrationError::invalid("lan_expired"));
            }
            delivery.approved = true;
        }
        SenderCommand::Status => {}
    }
    Ok(sender.as_ref().map_or_else(|| json!({"status":"idle"}), |sender| {
        let delivery = sender.shared.lock().unwrap();
        json!({"status":delivery.status,"code":sender.code.encode(),"remaining_seconds":sender.expires.saturating_duration_since(Instant::now()).as_secs(),"receiver":delivery.receiver.as_ref().map(|(ip,_)|ip.to_string())})
    }))
}
