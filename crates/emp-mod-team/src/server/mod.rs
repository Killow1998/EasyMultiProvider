mod accounts;
mod audit;
mod backup;
mod enrollment;
mod profiles;
mod shares;
#[cfg(test)]
mod tests;
mod usage;
mod weekly_budget;

use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use emp_modules::workspace::Role;
use emp_state::{
    fernet::FernetKey,
    vault::{decode_vault, encode_vault},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    net::SocketAddr,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tiny_http::{Header, Method, Request, Response, Server};

const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_SHARES: i64 = 80;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS workspace (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, salt BLOB NOT NULL, check_key BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS members (
    id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, password_hash BLOB NOT NULL, salt BLOB NOT NULL,
    role TEXT NOT NULL, publish INTEGER NOT NULL, sync INTEGER NOT NULL, enabled INTEGER NOT NULL DEFAULT 1,
    budget_nanos INTEGER, archived_name TEXT
);
CREATE TABLE IF NOT EXISTS sessions (
    token BLOB PRIMARY KEY, member TEXT NOT NULL REFERENCES members(id), device TEXT NOT NULL,
    expires INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS login_limits (
    name TEXT PRIMARY KEY, window INTEGER NOT NULL, attempts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS enrollment (
    member TEXT PRIMARY KEY REFERENCES members(id), nickname TEXT NOT NULL, pending INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS rejected_enrollments (
    name TEXT PRIMARY KEY, salt BLOB NOT NULL, password_hash BLOB NOT NULL, expires INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS member_tiers (
    member TEXT PRIMARY KEY REFERENCES members(id), fast INTEGER NOT NULL, ultrafast INTEGER NOT NULL
);
DROP TABLE IF EXISTS daily_join_code;
CREATE TABLE IF NOT EXISTS join_invites (
    issuer TEXT PRIMARY KEY REFERENCES members(id), code TEXT NOT NULL UNIQUE, expires INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS join_limits (
    window INTEGER PRIMARY KEY, attempts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS shares (
    id TEXT PRIMARY KEY, owner TEXT NOT NULL REFERENCES members(id), source TEXT NOT NULL,
    version INTEGER NOT NULL, encrypted BLOB NOT NULL, removed_at INTEGER, UNIQUE(owner,source)
);
CREATE TABLE IF NOT EXISTS share_reviews (
    share TEXT PRIMARY KEY REFERENCES shares(id), approved_version INTEGER NOT NULL
);
INSERT OR IGNORE INTO share_reviews SELECT id,version FROM shares;
CREATE TABLE IF NOT EXISTS usage (
    member TEXT NOT NULL REFERENCES members(id), device TEXT NOT NULL, id TEXT NOT NULL,
    share TEXT NOT NULL REFERENCES shares(id), model TEXT NOT NULL, finished_at INTEGER NOT NULL,
    input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER,
    estimate_nanos INTEGER, actual_nanos INTEGER,
    PRIMARY KEY(member,device,id)
);
CREATE INDEX IF NOT EXISTS use_by_member_date ON usage(member,finished_at);
CREATE TABLE IF NOT EXISTS weekly_usd_limits (
    member TEXT PRIMARY KEY REFERENCES members(id) ON DELETE CASCADE, usd_nanos INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS weekly_reset (
    slot INTEGER PRIMARY KEY CHECK(slot=1), weekday INTEGER NOT NULL, hour INTEGER NOT NULL, timezone TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS budget_mode (
    slot INTEGER PRIMARY KEY CHECK(slot=1), weekly INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS workspace_profile (
    slot INTEGER PRIMARY KEY CHECK(slot=1), created_at INTEGER, profiles_visible INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS member_joined (
    member TEXT PRIMARY KEY REFERENCES members(id), joined_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS member_days (
    member TEXT REFERENCES members(id), day INTEGER, online_at INTEGER NOT NULL, PRIMARY KEY(member,day)
);
CREATE TABLE IF NOT EXISTS subscription_reminders (
    share TEXT REFERENCES shares(id), day INTEGER, sender TEXT REFERENCES members(id), sent_at INTEGER NOT NULL,
    PRIMARY KEY(share,day)
);
CREATE TABLE IF NOT EXISTS workspace_audit (
    id INTEGER PRIMARY KEY, at INTEGER NOT NULL, actor TEXT NOT NULL REFERENCES members(id),
    target TEXT REFERENCES members(id), kind TEXT NOT NULL, details TEXT NOT NULL
);";

struct Workspace {
    db: Connection,
    id: String,
    name: String,
    key: FernetKey,
}

#[derive(Clone)]
struct Identity {
    member: String,
    device: String,
    role: Role,
    publish: bool,
    sync: bool,
    fast: bool,
    ultrafast: bool,
}

impl Identity {
    fn may_set_budget(&self, member: &str, role: Role) -> bool {
        self.role.manages(role) || (self.role == Role::TeamAdmin && self.member == member)
    }
}

#[derive(Debug)]
struct Failure(u16, &'static str);

type Result<T> = std::result::Result<T, Failure>;

impl From<rusqlite::Error> for Failure {
    fn from(_: rusqlite::Error) -> Self {
        Self(503, "workspace_storage_unavailable")
    }
}

impl From<serde_json::Error> for Failure {
    fn from(_: serde_json::Error) -> Self {
        Self(400, "invalid_workspace_request")
    }
}

fn missing() -> Failure {
    Failure(404, "workspace_route_not_found")
}
fn invalid() -> Failure {
    Failure(400, "invalid_workspace_request")
}
fn forbidden() -> Failure {
    Failure(403, "workspace_permission_required")
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(body)?)
}

fn as_json<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(Into::into)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn random<const SIZE: usize>() -> Result<[u8; SIZE]> {
    let mut bytes = [0; SIZE];
    getrandom::getrandom(&mut bytes).map_err(|_| Failure(503, "workspace_random_unavailable"))?;
    Ok(bytes)
}

fn hex_id() -> Result<String> {
    Ok(random::<16>()?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn identifier(raw: &str) -> bool {
    raw.len() == 32 && raw.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn digest(text: &str) -> Vec<u8> {
    Sha256::digest(text.as_bytes()).to_vec()
}

fn derive(secret: &str, salt: &[u8]) -> [u8; 32] {
    let mut hash = [0; 32];
    scrypt::scrypt(
        secret.as_bytes(),
        salt,
        &scrypt::Params::new(15, 8, 1).expect("scrypt settings"),
        &mut hash,
    )
    .expect("32-byte key");
    hash
}

fn initialize_schema(db: &Connection) -> Result<()> {
    db.execute_batch(SCHEMA)?;
    let migrated: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('shares') WHERE name='removed_at')",
        [],
        |row| row.get(0),
    )?;
    if !migrated {
        db.execute("ALTER TABLE shares ADD COLUMN removed_at INTEGER", [])?;
    }
    let archives_members: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('members') WHERE name='archived_name')",
        [],
        |row| row.get(0),
    )?;
    if !archives_members {
        db.execute("ALTER TABLE members ADD COLUMN archived_name TEXT", [])?;
    }
    Ok(())
}

impl Workspace {
    fn open(path: &Path, password: &str) -> Result<Self> {
        emp_state::filesystem::create_private_directory(path)
            .map_err(|_| Failure(503, "workspace_directory_unavailable"))?;
        let mut db = Connection::open(path.join("workspace.sqlite3"))?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        initialize_schema(&db)?;
        let existing: Option<(String, String, Vec<u8>, Vec<u8>)> = db
            .query_row("SELECT id,name,salt,check_key FROM workspace", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .optional()?;
        let (id, name, key) = if let Some((id, name, salt, check)) = existing {
            let key = FernetKey::from_encoded(&URL_SAFE.encode(derive(password, &salt)))
                .expect("Fernet key from scrypt");
            decode_vault(&key, &check)
                .map_err(|_| Failure(401, "incorrect_workspace_encryption_password"))?;
            (id, name, key)
        } else {
            let salt = random::<16>()?;
            let key = FernetKey::from_encoded(&URL_SAFE.encode(derive(password, &salt)))
                .expect("Fernet key from scrypt");
            let id = hex_id()?;
            let name = std::env::var("EMP_TEAM_NAME").unwrap_or_else(|_| "EMP Workspace".into());
            let admin = std::env::var("EMP_TEAM_ADMIN").map_err(|_| invalid())?;
            let admin_password = std::env::var("EMP_TEAM_ADMIN_PASSWORD").map_err(|_| invalid())?;
            let tx = db.transaction()?;
            let owner = accounts::create(&tx, &admin, &admin_password, Role::VpsAdmin, true, true)?;
            profiles::record_joined(&tx, &owner, now())?;
            tx.execute(
                "INSERT INTO workspace_profile(slot,created_at) VALUES (1,?)",
                [now()],
            )?;
            tx.execute(
                "INSERT INTO workspace VALUES (?,?,?,?)",
                params![id, name, salt, encode_vault(&key, b"EMP workspace")],
            )?;
            tx.commit()?;
            (id, name, key)
        };
        Ok(Self { db, id, name, key })
    }

    fn request(
        &mut self,
        method: &Method,
        path: &str,
        token: Option<&str>,
        body: &[u8],
    ) -> Result<Value> {
        if method == &Method::Get && path == "/healthz" {
            return Ok(json!({"ok":true,"mode":"workspace"}));
        }
        let operation = path.strip_prefix("/workspace/v1/").ok_or_else(missing)?;
        if method == &Method::Post && operation == "login" {
            return self.login(parse(body)?);
        }
        if method == &Method::Post && operation == "join" {
            return self.join(parse(body)?, now());
        }
        let identity = self.authenticate(token)?;
        match (method, operation) {
            (Method::Get, "join-code") => self.join_code(&identity, now(), false),
            (Method::Post, "join-code") => self.join_code(&identity, now(), true),
            (Method::Get, "status") => self.status(&identity),
            (Method::Get, "members") => self.members(&identity),
            (Method::Post, "members") => self.add_member(&identity, parse(body)?),
            (Method::Post, "member-access") => self.member_access(&identity, parse(body)?),
            (Method::Post, "reject-member") => self.reject_member(&identity, parse(body)?),
            (Method::Post, "remove-member") => self.remove_member(&identity, parse(body)?),
            (Method::Post, "member-tiers") => self.member_tiers(&identity, parse(body)?),
            (Method::Post, "budget") => self.budget(&identity, parse(body)?),
            (Method::Post, "weekly-budget") => self.weekly_budget(&identity, parse(body)?),
            (Method::Get, "budget-status") => as_json(self.budget_status(&identity)?),
            (Method::Get, "overview") => self.overview(&identity),
            (Method::Post, "audit") => self.audit(&identity, parse(body)?),
            (Method::Post, "handover") => self.handover(&identity, parse(body)?),
            (Method::Get, "backup") => self.backup(&identity),
            (Method::Get, "profiles") => self.profiles(&identity),
            (Method::Post, "profile") => self.profile(&identity, parse(body)?),
            (Method::Post, "profile-visibility") => {
                self.set_profile_visibility(&identity, parse(body)?)
            }
            (Method::Get, "heartbeat") => self.heartbeat(&identity),
            (Method::Get, "subscriptions") => self.subscriptions(&identity),
            (Method::Post, "subscription-reminder") => {
                self.subscription_reminder(&identity, parse(body)?)
            }
            (Method::Post, "publish") => self.publish(&identity, parse(body)?),
            (Method::Get, "publish-version") => self.publish_version(&identity),
            (Method::Get, "shared-services") => self.reviewable_shares(&identity),
            (Method::Post, "review-service") => self.review_share(&identity, parse(body)?),
            (Method::Post, "remove-service") => self.remove_share(&identity, parse(body)?),
            (Method::Post, "reset-authority") => self.reset_authority(&identity, parse(body)?),
            (Method::Get, "shares") => self.shares(&identity),
            (Method::Get, "usage") => self.usage(&identity, usage::Period::Today),
            (Method::Post, "usage") => {
                self.usage(&identity, parse::<usage::UsageFilter>(body)?.period)
            }
            (Method::Post, "report") => self.report(&identity, parse(body)?),
            (Method::Post, "logout") => {
                self.db.execute(
                    "DELETE FROM sessions WHERE token=?",
                    [digest(token.expect("authenticated token"))],
                )?;
                Ok(json!({"ok":true}))
            }
            _ => Err(missing()),
        }
    }
}

pub(super) fn run(args: &[String]) -> std::result::Result<(), String> {
    if let [command, state_arg, state, from_arg, file] = args
        && command == "restore"
        && state_arg == "--state"
        && from_arg == "--from"
    {
        let password = workspace_password()?;
        return backup::restore(Path::new(state), Path::new(file), &password)
            .map_err(|error| error.1.into());
    }
    let [command, state_arg, state, listen_arg, address] = args else {
        return Err("usage: empmod-team serve --state DIR --listen 127.0.0.1:PORT".into());
    };
    if command != "serve" || state_arg != "--state" || listen_arg != "--listen" {
        return Err("usage: empmod-team serve --state DIR --listen 127.0.0.1:PORT".into());
    }
    let address: SocketAddr = address.parse().map_err(|_| "Invalid loopback address")?;
    if !address.ip().is_loopback() {
        return Err("Workspace server requires a loopback listener behind HTTPS".into());
    }
    let password = workspace_password()?;
    let _owner = lock_state(Path::new(state)).map_err(|error| error.1)?;
    let mut workspace = Workspace::open(Path::new(state), &password).map_err(|error| error.1)?;
    drop(password);
    let server = Server::http(address).map_err(|error| error.to_string())?;
    eprintln!("EMP workspace listening on {}", server.server_addr());
    for mut request in server.incoming_requests() {
        let result = serve(&mut workspace, &mut request);
        let (status, payload) = match result {
            Ok(payload) => (200, payload),
            Err(Failure(status, error)) => {
                (status, json!({"error":{"code":error, "message":error}}))
            }
        };
        let response = Response::from_string(payload.to_string())
            .with_status_code(status)
            .with_header(
                Header::from_bytes("Content-Type", "application/json").expect("static header"),
            )
            .with_header(Header::from_bytes("Cache-Control", "no-store").expect("static header"));
        let _ = request.respond(response);
    }
    Ok(())
}

fn workspace_password() -> std::result::Result<String, String> {
    std::env::var("EMP_TEAM_SPACE_PASSWORD")
        .ok()
        .filter(|password| password.len() >= 12)
        .ok_or_else(|| "Configure EMP_TEAM_SPACE_PASSWORD with at least 12 characters".into())
}

fn lock_state(state: &Path) -> Result<emp_state::IntegrationFileLock> {
    emp_state::filesystem::create_private_directory(state)
        .map_err(|_| Failure(503, "workspace_directory_unavailable"))?;
    emp_state::IntegrationFileLock::acquire(
        &state.join("workspace.lock"),
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(20),
    )
    .map_err(|_| Failure(409, "workspace_directory_locked"))
}

fn serve(workspace: &mut Workspace, request: &mut Request) -> Result<Value> {
    let method = request.method().clone();
    let path = request.url().to_owned();
    if request.body_length().is_some_and(|size| size > MAX_BODY) {
        return Err(Failure(413, "workspace_request_too_large"));
    }
    let mut tokens = request
        .headers()
        .iter()
        .filter(|header| header.field.equiv("Authorization"));
    let token = tokens
        .next()
        .and_then(|header| header.value.as_str().strip_prefix("Bearer "))
        .map(str::to_owned);
    if tokens.next().is_some() {
        return Err(Failure(401, "workspace_authentication_required"));
    }
    let mut body = Vec::new();
    if method == Method::Post {
        request
            .as_reader()
            .take(MAX_BODY as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|_| invalid())?;
        if body.len() > MAX_BODY {
            return Err(Failure(413, "workspace_request_too_large"));
        }
    }
    workspace.request(&method, &path, token.as_deref(), &body)
}
