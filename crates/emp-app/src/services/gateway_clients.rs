//! Named API callers with retrievable vault-encrypted credentials.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io, path::PathBuf, sync::Mutex};

#[derive(Clone)]
pub(crate) struct Caller {
    pub(crate) id: String,
    pub(crate) name: String,
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

pub(crate) struct GatewayClients {
    path: PathBuf,
    clients: Mutex<Vec<Client>>,
}

impl GatewayClients {
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
        })
    }

    pub(crate) fn identify(&self, key: &str) -> Option<Caller> {
        let digest = format!("{:x}", Sha256::digest(key.as_bytes()));
        self.clients
            .lock()
            .ok()?
            .iter()
            .find(|client| client.digest == digest)
            .map(|client| Caller {
                id: client.id.clone(),
                name: client.name.clone(),
            })
    }

    pub(crate) fn list(&self) -> Value {
        json!(
            self.clients
                .lock()
                .expect("gateway clients")
                .iter()
                .map(|client| {
                    json!({"id":client.id,"name":client.name,"created_at":client.created_at,"api_key":client.api_key})
                })
                .collect::<Vec<_>>()
        )
    }

    pub(crate) fn issue(
        &self,
        name: &str,
        replace: Option<&str>,
        vault: &emp_state::VaultStore,
    ) -> io::Result<Value> {
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| io::Error::other("gateway clients"))?;
        let mut updated = clients.clone();
        let index = if let Some(id) = replace {
            updated
                .iter()
                .position(|client| client.id == id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Device key was not found")
                })?
        } else {
            if name.trim().is_empty()
                || name.chars().count() > 64
                || name.chars().any(char::is_control)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Caller name must contain 1–64 characters",
                ));
            }
            updated.push(Client {
                id: crate::util::random_hex(16).map_err(io::Error::other)?,
                name: name.trim().into(),
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
        let client = &mut updated[index];
        client.digest = format!("{:x}", Sha256::digest(key.as_bytes()));
        client.api_key = Some(key.clone());
        let result = json!({"id":client.id,"name":client.name,"api_key":key});
        vault
            .write_encrypted_json(&self.path, &serde_json::to_value(&updated)?)
            .map_err(io::Error::other)?;
        *clients = updated;
        Ok(result)
    }

    pub(crate) fn remove(&self, id: &str, vault: &emp_state::VaultStore) -> io::Result<()> {
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| io::Error::other("gateway clients"))?;
        let updated: Vec<_> = clients
            .iter()
            .filter(|client| client.id != id)
            .cloned()
            .collect();
        vault
            .write_encrypted_json(&self.path, &serde_json::to_value(&updated)?)
            .map_err(io::Error::other)?;
        *clients = updated;
        Ok(())
    }
}
