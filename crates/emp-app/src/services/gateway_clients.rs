//! Named API callers. Only key digests are persisted; names are not hardware identities.
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
}

pub(crate) struct GatewayClients {
    path: PathBuf,
    clients: Mutex<Vec<Client>>,
}

impl GatewayClients {
    pub(crate) fn open(path: PathBuf) -> io::Result<Self> {
        let clients = match std::fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
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
                    json!({"id":client.id,"name":client.name,"created_at":client.created_at})
                })
                .collect::<Vec<_>>()
        )
    }

    pub(crate) fn create(&self, name: &str) -> io::Result<Value> {
        if name.trim().is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Caller name must contain 1–64 characters",
            ));
        }
        let key = format!(
            "emp-client-{}",
            crate::util::random_hex(32).map_err(io::Error::other)?
        );
        let client = Client {
            id: crate::util::random_hex(16).map_err(io::Error::other)?,
            name: name.trim().into(),
            digest: format!("{:x}", Sha256::digest(key.as_bytes())),
            created_at: crate::util::system_now(),
        };
        let result = json!({"id":client.id,"name":client.name,"api_key":key});
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| io::Error::other("gateway clients"))?;
        let mut updated = clients.clone();
        updated.push(client);
        emp_state::atomic_write_private_state(&self.path, &serde_json::to_vec(&updated)?)
            .map_err(io::Error::other)?;
        *clients = updated;
        Ok(result)
    }

    pub(crate) fn remove(&self, id: &str) -> io::Result<()> {
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| io::Error::other("gateway clients"))?;
        let updated: Vec<_> = clients
            .iter()
            .filter(|client| client.id != id)
            .cloned()
            .collect();
        emp_state::atomic_write_private_state(&self.path, &serde_json::to_vec(&updated)?)
            .map_err(io::Error::other)?;
        *clients = updated;
        Ok(())
    }
}
