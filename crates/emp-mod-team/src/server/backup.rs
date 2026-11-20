use super::*;
use std::{fs, io};

const FORMAT: &str = "emp-team-backup-v1";
const MAX_DATABASE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Backup {
    format: String,
    salt: String,
    data: String,
}

fn io_error(_: io::Error) -> Failure {
    Failure(503, "workspace_backup_io_failed")
}

impl Workspace {
    pub(super) fn backup(&self, who: &Identity) -> Result<Value> {
        if who.role != Role::VpsAdmin {
            return Err(forbidden());
        }
        let bytes: i64 = self.db.query_row(
            "SELECT (page_count-freelist_count)*page_size
             FROM pragma_page_count(),pragma_freelist_count(),pragma_page_size()",
            [],
            |row| row.get(0),
        )?;
        if bytes > MAX_DATABASE_BYTES as i64 {
            return Err(Failure(413, "workspace_backup_too_large"));
        }
        let temp = tempfile::tempdir().map_err(io_error)?;
        let snapshot = temp.path().join("workspace.sqlite3");
        let path = snapshot.to_str().ok_or_else(invalid)?;
        self.db.execute("VACUUM INTO ?", [path])?;
        let copy = Connection::open(&snapshot)?;
        copy.execute_batch(
            "PRAGMA secure_delete=ON;
             BEGIN;
             DELETE FROM sessions;
             DELETE FROM join_invites;
             DELETE FROM login_limits;
             DELETE FROM join_limits;
             COMMIT;",
        )?;
        drop(copy);
        let bytes = fs::read(snapshot).map_err(io_error)?;
        if bytes.len() > MAX_DATABASE_BYTES {
            return Err(Failure(413, "workspace_backup_too_large"));
        }
        let salt: Vec<u8> = self
            .db
            .query_row("SELECT salt FROM workspace", [], |row| row.get(0))?;
        let encoded = encode_vault(&self.key, &bytes);
        as_json(Backup {
            format: FORMAT.into(),
            salt: URL_SAFE.encode(salt),
            data: String::from_utf8(encoded).expect("ASCII vault format"),
        })
    }
}

pub(super) fn restore(state: &Path, path: &Path, password: &str) -> Result<()> {
    let mut buffer = Vec::new();
    fs::File::open(path)
        .map_err(io_error)?
        .take(emp_modules::team::MAX_BACKUP_WIRE_BYTES as u64 + 1)
        .read_to_end(&mut buffer)
        .map_err(io_error)?;
    if buffer.len() > emp_modules::team::MAX_BACKUP_WIRE_BYTES {
        return Err(Failure(413, "workspace_backup_too_large"));
    }
    let backup: Backup = parse(&buffer)?;
    let salt = URL_SAFE.decode(backup.salt).map_err(|_| invalid())?;
    if backup.format != FORMAT || salt.len() != 16 {
        return Err(invalid());
    }
    let key = FernetKey::from_encoded(&URL_SAFE.encode(derive(password, &salt)))
        .expect("Fernet key from scrypt");
    let database = decode_vault(&key, backup.data.as_bytes())
        .map_err(|_| Failure(401, "incorrect_workspace_encryption_password"))?;
    if database.len() > MAX_DATABASE_BYTES {
        return Err(Failure(413, "workspace_backup_too_large"));
    }
    let _owner = lock_state(state)?;
    for name in [
        "workspace.sqlite3",
        "workspace.sqlite3-wal",
        "workspace.sqlite3-shm",
    ] {
        match fs::symlink_metadata(state.join(name)) {
            Ok(_) => return Err(Failure(409, "workspace_restore_destination_exists")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    let temp = tempfile::tempdir_in(state).map_err(io_error)?;
    let snapshot = temp.path().join("workspace.sqlite3");
    emp_state::atomic_write_private_state(&snapshot, &database)
        .map_err(|_| Failure(503, "workspace_backup_io_failed"))?;
    let copy = Connection::open_with_flags(&snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let check: Vec<u8> = copy.query_row(
        "SELECT check_key FROM workspace WHERE salt=?",
        [salt],
        |row| row.get(0),
    )?;
    decode_vault(&key, &check).map_err(|_| invalid())?;
    let valid: String = copy.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if valid != "ok" {
        return Err(invalid());
    }
    drop(copy);
    fs::rename(snapshot, state.join("workspace.sqlite3")).map_err(io_error)
}
