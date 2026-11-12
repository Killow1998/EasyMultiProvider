//! Persistent Gemini tool signatures used to resume Codex conversations.

use rusqlite::{Connection, OptionalExtension, params};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct GeminiSignatures {
    path: PathBuf,
    connection: Mutex<Option<Connection>>,
}

impl GeminiSignatures {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            connection: Mutex::new(None),
        }
    }

    fn with_connection<T>(
        &self,
        operation: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> io::Result<T> {
        let mut lock = self
            .connection
            .lock()
            .map_err(|_| io::Error::other("signature database lock is poisoned"))?;
        if lock.is_none() {
            let parent = self.path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "signature path has no parent")
            })?;
            crate::filesystem::create_private_directory(parent).map_err(io::Error::other)?;
            let conn = Connection::open(&self.path).map_err(io::Error::other)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
            }
            conn.execute_batch("CREATE TABLE IF NOT EXISTS signatures (scope TEXT NOT NULL, call_id TEXT NOT NULL, signature TEXT NOT NULL, PRIMARY KEY (scope, call_id))").map_err(io::Error::other)?;
            *lock = Some(conn);
        }
        operation(lock.as_ref().expect("signature database opened")).map_err(io::Error::other)
    }

    pub fn put(&self, scope: &str, call_id: &str, signature: &str) -> io::Result<()> {
        self.with_connection(|db| {
            db.execute(
                "INSERT OR REPLACE INTO signatures(scope, call_id, signature) VALUES (?1, ?2, ?3)",
                params![scope, call_id, signature],
            )?;
            Ok(())
        })
    }

    pub fn get(&self, scope: &str, call_id: &str) -> io::Result<Option<String>> {
        self.with_connection(|db| {
            db.query_row(
                "SELECT signature FROM signatures WHERE scope=?1 AND call_id=?2",
                params![scope, call_id],
                |row| row.get(0),
            )
            .optional()
        })
    }
}
