//! Named API callers with retrievable vault-encrypted credentials.
use emp_modules::gateway::{CreatePlan, RemovePlan, ReplacePlan, Request};
use emp_modules::{Command, GatewayIdentity, Module};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{io, path::PathBuf, sync::Mutex};

use super::modules::Modules;
pub(crate) use emp_modules::gateway::Caller;

#[derive(Serialize)]
pub(crate) struct ListedClient {
    id: String,
    name: String,
    created_at: f64,
    api_key: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct IssuedClient {
    id: String,
    name: String,
    api_key: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Client {
    id: String,
    name: String,
    digest: String,
    created_at: f64,
    #[serde(default)]
    api_key: Option<String>,
}

#[cfg(all(test, unix))]
type SaveGate = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(all(test, unix))]
type IdentifyGate = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

pub(crate) struct GatewayClients {
    path: PathBuf,
    clients: Mutex<Vec<Client>>,
    #[cfg(all(test, unix))]
    save_gate: Mutex<Option<SaveGate>>,
    #[cfg(all(test, unix))]
    identify_gate: Mutex<Option<IdentifyGate>>,
}

impl GatewayClients {
    pub(crate) fn empty(path: PathBuf) -> Self {
        Self {
            path,
            clients: Mutex::new(Vec::new()),
            #[cfg(all(test, unix))]
            save_gate: Mutex::default(),
            #[cfg(all(test, unix))]
            identify_gate: Mutex::default(),
        }
    }

    pub(crate) fn open(
        path: PathBuf,
        vault: &emp_state::VaultStore,
    ) -> Result<Self, crate::error::AppError> {
        let clients = match std::fs::read(&path) {
            Ok(raw) => {
                let value = if raw.starts_with(emp_state::vault::VAULT_MAGIC) {
                    vault.read_encrypted_json(&path)?
                } else {
                    serde_json::from_slice(&raw)
                        .map_err(|_| emp_state::FilesystemError::InvalidJson)?
                };
                serde_json::from_value(value)
                    .map_err(|_| emp_state::FilesystemError::InvalidJson)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            clients: Mutex::new(clients),
            #[cfg(all(test, unix))]
            save_gate: Mutex::default(),
            #[cfg(all(test, unix))]
            identify_gate: Mutex::default(),
        })
    }

    pub(crate) fn identify(&self, key: &str, modules: &Modules) -> Option<Caller> {
        #[cfg(all(test, unix))]
        {
            let gate = self.identify_gate.lock().unwrap().take();
            if let Some((reached, released)) = gate {
                let _ = reached.send(());
                let _ = released.recv();
            }
        }
        let digest = format!("{:x}", Sha256::digest(key.as_bytes()));
        let clients = self.clients.lock().ok()?;
        let candidates = clients
            .iter()
            .map(|client| GatewayIdentity {
                id: client.id.clone(),
                name: client.name.clone(),
                digest: client.digest.clone(),
            })
            .collect();
        call(modules, Request::Identify { digest, candidates })
            .ok()
            .flatten()
    }

    pub(crate) fn list(&self) -> Vec<ListedClient> {
        self.clients
            .lock()
            .expect("gateway clients")
            .iter()
            .map(|client| ListedClient {
                id: client.id.clone(),
                name: client.name.clone(),
                created_at: client.created_at,
                api_key: client.api_key.clone(),
            })
            .collect()
    }

    pub(crate) fn devices(&self) -> Vec<Caller> {
        self.clients
            .lock()
            .expect("gateway clients")
            .iter()
            .map(|client| Caller {
                id: client.id.clone(),
                name: client.name.clone(),
            })
            .collect()
    }

    pub(crate) fn issue(
        &self,
        name: &str,
        replace: Option<&str>,
        vault: &emp_state::VaultStore,
        modules: &Modules,
    ) -> io::Result<IssuedClient> {
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| io::Error::other("gateway clients"))?;
        let mut updated = clients.clone();
        let index = if let Some(id) = replace {
            let plan: ReplacePlan = call(
                modules,
                Request::Replace {
                    id: id.into(),
                    device_ids: device_ids(&clients),
                },
            )?;
            plan.index
        } else {
            let plan: CreatePlan = call(modules, Request::Create { name: name.into() })?;
            updated.push(Client {
                id: crate::util::random_hex(16).map_err(io::Error::other)?,
                name: plan.name,
                digest: String::new(),
                created_at: crate::util::system_now(),
                api_key: None,
            });
            updated.len() - 1
        };
        let key = format!(
            "emp-client-{}",
            crate::util::random_hex(32).map_err(io::Error::other)?
        );
        let client = updated
            .get_mut(index)
            .ok_or_else(|| io::Error::other("invalid_gateway_plan"))?;
        client.digest = format!("{:x}", Sha256::digest(key.as_bytes()));
        client.api_key = Some(key.clone());
        let result = IssuedClient {
            id: client.id.clone(),
            name: client.name.clone(),
            api_key: key,
        };
        self.save(vault, &mut clients, updated)?;
        Ok(result)
    }

    pub(crate) fn remove(
        &self,
        id: &str,
        vault: &emp_state::VaultStore,
        modules: &Modules,
    ) -> io::Result<()> {
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| io::Error::other("gateway clients"))?;
        let plan: RemovePlan = call(
            modules,
            Request::Remove {
                id: id.into(),
                device_ids: device_ids(&clients),
            },
        )?;
        let updated = plan
            .keep
            .into_iter()
            .map(|index| {
                clients
                    .get(index)
                    .cloned()
                    .ok_or_else(|| io::Error::other("invalid_gateway_plan"))
            })
            .collect::<io::Result<_>>()?;
        self.save(vault, &mut clients, updated)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn gate_next_identify_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        *self.identify_gate.lock().unwrap() = Some((reached, released));
        (ready, release)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn gate_next_save_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached, ready) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        *self.save_gate.lock().unwrap() = Some((reached, released));
        (ready, release)
    }

    fn save(
        &self,
        vault: &emp_state::VaultStore,
        clients: &mut Vec<Client>,
        updated: Vec<Client>,
    ) -> io::Result<()> {
        #[cfg(all(test, unix))]
        if let Some((reached, released)) = self.save_gate.lock().unwrap().take() {
            let _ = reached.send(());
            let _ = released.blocking_recv();
        }
        vault
            .write_encrypted_json(&self.path, &serde_json::to_value(&updated)?)
            .map_err(io::Error::other)?;
        *clients = updated;
        Ok(())
    }
}

fn device_ids(clients: &[Client]) -> Vec<String> {
    clients.iter().map(|client| client.id.clone()).collect()
}

fn call<T: DeserializeOwned>(modules: &Modules, request: Request) -> io::Result<T> {
    modules
        .call_typed(Module::Gateway, Command::Gateway { request })
        .map_err(|error| {
            let message = match error.as_str() {
                "invalid_device_name" => "Caller name must contain 1–64 characters",
                "missing_device_id" => "A device key ID is required",
                "device_key_not_found" => "Device key was not found",
                _ => return io::Error::other(error),
            };
            io::Error::new(io::ErrorKind::InvalidInput, message)
        })
}
