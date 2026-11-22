use super::*;

const LOGIN_WINDOW: i64 = 300;
const SESSION_TTL: i64 = 7 * 86400;
const REJECTION_TTL: i64 = 86400;

#[derive(Deserialize)]
pub(super) struct Login {
    name: String,
    password: String,
    device: String,
}

#[derive(Deserialize)]
pub(super) struct NewMember {
    name: String,
    password: String,
    role: Role,
    publish: bool,
    sync: bool,
}

#[derive(Deserialize)]
pub(super) struct Access {
    id: String,
    role: Role,
    publish: bool,
    sync: bool,
    enabled: bool,
}

#[derive(Deserialize)]
pub(super) struct MemberTarget {
    id: String,
}

#[derive(Deserialize)]
pub(super) struct Tiers {
    id: String,
    fast: bool,
    ultrafast: bool,
}

#[derive(Deserialize)]
pub(super) struct Handover {
    successor: String,
}

struct Account {
    id: Option<String>,
    salt: Vec<u8>,
    hash: Vec<u8>,
    enabled: bool,
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@'))
}

fn role(raw: &str) -> rusqlite::Result<Role> {
    match raw {
        "vps_admin" => Ok(Role::VpsAdmin),
        "team_admin" => Ok(Role::TeamAdmin),
        "member" => Ok(Role::Member),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

pub(super) fn create(
    db: &Connection,
    name: &str,
    password: &str,
    role: Role,
    publish: bool,
    sync: bool,
) -> Result<String> {
    if !valid_name(name) || !(12..=256).contains(&password.len()) {
        return Err(invalid());
    }
    let id = hex_id()?;
    let salt = random::<16>()?;
    let hash = derive(password, &salt);
    let inserted = db.execute(
        "INSERT INTO members(id,name,password_hash,salt,role,publish,sync) VALUES (?,?,?,?,?,?,?)
         ON CONFLICT(name) DO NOTHING",
        params![id, name, hash, salt, role.as_str(), publish, sync],
    )?;
    if inserted == 0 {
        return Err(Failure(409, "workspace_member_exists"));
    }
    db.execute("DELETE FROM rejected_enrollments WHERE name=?", [name])?;
    Ok(id)
}

impl Workspace {
    pub(super) fn login(&mut self, login: Login) -> Result<Value> {
        if !valid_name(&login.name) || login.password.len() > 256 || !identifier(&login.device) {
            return Err(invalid());
        }
        let at = now();
        let window = at.div_euclid(LOGIN_WINDOW);
        self.db
            .execute("DELETE FROM login_limits WHERE window<?", [window])?;
        let limits: (i64, i64) = self.db.query_row(
            "SELECT COALESCE(SUM(CASE WHEN name=? THEN attempts ELSE 0 END),0),
                    COALESCE(SUM(attempts),0) FROM login_limits WHERE window=?",
            params![login.name, window],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if limits.0 >= 6 || limits.1 >= 60 {
            return Err(Failure(429, "workspace_login_limited"));
        }
        let account = self
            .db
            .query_row(
                "SELECT id,salt,password_hash,enabled FROM members WHERE name=?1
                 UNION ALL
                 SELECT NULL,salt,password_hash,0 FROM rejected_enrollments WHERE name=?1 AND expires>?2",
                params![login.name, at],
                |row| {
                    Ok(Account {
                        id: row.get(0)?,
                        salt: row.get(1)?,
                        hash: row.get(2)?,
                        enabled: row.get(3)?,
                    })
                },
            )
            .optional()?;
        let hash = derive(
            &login.password,
            account
                .as_ref()
                .map_or(&[0; 16], |member| member.salt.as_slice()),
        );
        let Some(account) = account.filter(|member| fixed_eq(&hash, &member.hash)) else {
            self.db.execute(
                "INSERT INTO login_limits VALUES (?,?,1) ON CONFLICT(name) DO UPDATE SET attempts=attempts+1",
                params![login.name,window],
            )?;
            return Err(Failure(401, "workspace_login_failed"));
        };
        let Some(account_id) = account.id else {
            return Err(Failure(403, "workspace_join_rejected"));
        };
        if !account.enabled {
            let pending = self
                .db
                .query_row(
                    "SELECT pending FROM enrollment WHERE member=?",
                    [&account_id],
                    |row| row.get::<_, bool>(0),
                )
                .optional()?
                .unwrap_or(false);
            return Err(if pending {
                Failure(403, "workspace_join_pending")
            } else {
                Failure(401, "workspace_login_failed")
            });
        }
        let token = URL_SAFE.encode(random::<32>()?);
        let expires = now() + SESSION_TTL;
        let tx = self.db.transaction()?;
        tx.execute(
            "DELETE FROM sessions WHERE expires<=? OR (member=? AND device=?)",
            params![now(), account_id, login.device],
        )?;
        tx.execute("DELETE FROM login_limits WHERE name=?", [&login.name])?;
        tx.execute(
            "INSERT INTO sessions VALUES (?,?,?,?)",
            params![digest(&token), account_id, login.device, expires],
        )?;
        tx.commit()?;
        let identity = self.authenticate(Some(&token))?;
        self.heartbeat(&identity)?;
        let mut status = self.status(&identity)?;
        status["token"] = json!(token);
        Ok(status)
    }

    pub(super) fn authenticate(&self, token: Option<&str>) -> Result<Identity> {
        let required = || Failure(401, "workspace_authentication_required");
        let token = token
            .filter(|token| token.len() == 44)
            .ok_or_else(required)?;
        self.db
            .query_row(
                "SELECT s.member,s.device,m.role,m.publish,m.sync,
                        COALESCE(t.fast,m.role!='member'),COALESCE(t.ultrafast,m.role!='member')
             FROM sessions s JOIN members m ON s.member=m.id
             LEFT JOIN member_tiers t ON t.member=m.id
             WHERE s.token=? AND s.expires>? AND m.enabled=1",
                params![digest(token), now()],
                |r| {
                    Ok(Identity {
                        member: r.get(0)?,
                        device: r.get(1)?,
                        role: role(&r.get::<_, String>(2)?)?,
                        publish: r.get(3)?,
                        sync: r.get(4)?,
                        fast: r.get(5)?,
                        ultrafast: r.get(6)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(required)
    }

    pub(super) fn status(&self, who: &Identity) -> Result<Value> {
        let (name, nickname): (String, Option<String>) = self.db.query_row(
            "SELECT m.name,e.nickname FROM members m LEFT JOIN enrollment e ON e.member=m.id WHERE m.id=?",
            [&who.member],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(json!({"workspace":{"id":self.id, "name":self.name},
            "native_usage":self.native_usage(who)?, "member":{
            "id":who.member,"name":name,"nickname":nickname,"role":who.role,"publish":who.publish,"sync":who.sync,
            "fast":who.fast,"ultrafast":who.ultrafast
        }}))
    }

    pub(super) fn members(&self, who: &Identity) -> Result<Value> {
        if !who.role.can_manage() {
            return Err(forbidden());
        }
        let mut statement = self.db.prepare(
            "SELECT m.id,m.name,m.role,m.publish,m.sync,m.enabled,m.budget_nanos,e.nickname,COALESCE(e.pending,0),
                    COALESCE(t.fast,m.role!='member'),COALESCE(t.ultrafast,m.role!='member'),w.usd_nanos
             FROM members m LEFT JOIN enrollment e ON e.member=m.id
             LEFT JOIN member_tiers t ON t.member=m.id
             LEFT JOIN weekly_usd_limits w ON w.member=m.id
             WHERE m.archived_name IS NULL AND (m.role!='vps_admin' OR m.id=?)
             ORDER BY COALESCE(e.pending,0) DESC,m.name",
        )?;
        let rows = statement
            .query_map([&who.member], |row| {
                let role = role(&row.get::<_, String>(2)?)?;
                let id = row.get::<_, String>(0)?;
                let pending: bool = row.get(8)?;
                Ok(json!({"id":id,"name":row.get::<_,String>(1)?,
                "role":role, "publish":row.get::<_,bool>(3)?,"sync":row.get::<_,bool>(4)?,
                "enabled":row.get::<_,bool>(5)?,"budget_nanos":row.get::<_,Option<i64>>(6)?,
                "nickname":row.get::<_,Option<String>>(7)?,"pending":pending,
                "fast":row.get::<_,bool>(9)?,"ultrafast":row.get::<_,bool>(10)?,
                "weekly_usd_limit":row.get::<_,Option<i64>>(11)?,
                "can_manage":who.role.manages(role),"can_budget":who.may_set_budget(&id,role),
                "can_remove":!pending && who.role.manages(role)}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({"members":rows,"weekly_reset":self.weekly_reset()?,"budget_mode":self.budget_mode()?}),
        )
    }

    pub(super) fn add_member(&mut self, who: &Identity, body: NewMember) -> Result<Value> {
        if !who.role.manages(body.role) {
            return Err(forbidden());
        }
        let tx = self.db.transaction()?;
        let id = create(
            &tx,
            &body.name,
            &body.password,
            body.role,
            body.publish,
            body.sync,
        )?;
        super::profiles::record_joined(&tx, &id, now())?;
        audit::record(
            &tx,
            who,
            Some(&id),
            "member_created",
            json!({"role":body.role,"publish":body.publish,"sync":body.sync}),
        )?;
        tx.commit()?;
        Ok(json!({"id":id}))
    }

    pub(super) fn member_access(&mut self, who: &Identity, body: Access) -> Result<Value> {
        if !who.role.manages(body.role) || !who.role.manages(self.role_for(&body.id)?) {
            return Err(forbidden());
        }
        let transaction = self.db.transaction()?;
        let (role, publish, sync, enabled): (String, bool, bool, bool) = transaction.query_row(
            "SELECT role,publish,sync,enabled FROM members WHERE id=?",
            [&body.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        transaction.execute(
            "UPDATE members SET role=?,publish=?,sync=?,enabled=? WHERE id=?",
            params![
                body.role.as_str(),
                body.publish,
                body.sync,
                body.enabled,
                body.id
            ],
        )?;
        let approved = if !body.enabled {
            transaction.execute("DELETE FROM sessions WHERE member=?", [&body.id])?;
            false
        } else if transaction.execute(
            "UPDATE enrollment SET pending=0 WHERE member=? AND pending=1",
            [&body.id],
        )? != 0
        {
            super::profiles::record_joined(&transaction, &body.id, now())?;
            true
        } else {
            false
        };
        let changes = [
            ("role", json!(role), json!(body.role)),
            ("publish", json!(publish), json!(body.publish)),
            ("sync", json!(sync), json!(body.sync)),
            ("enabled", json!(enabled), json!(body.enabled)),
        ]
        .into_iter()
        .filter(|(_, old, new)| old != new)
        .map(|(key, _, new)| (key.to_owned(), new))
        .collect::<serde_json::Map<_, _>>();
        if approved || !changes.is_empty() {
            audit::record(
                &transaction,
                who,
                Some(&body.id),
                if approved {
                    "member_approved"
                } else {
                    "member_access"
                },
                Value::Object(changes),
            )?;
        }
        transaction.commit()?;
        Ok(json!({"ok":true}))
    }

    pub(super) fn reject_member(&mut self, who: &Identity, body: MemberTarget) -> Result<Value> {
        if !who.role.can_manage() {
            return Err(forbidden());
        }
        if !identifier(&body.id) {
            return Err(invalid());
        }
        let tx = self.db.transaction()?;
        let (role, name, nickname) = tx
            .query_row(
                "SELECT m.role,m.name,CASE WHEN e.pending=1 AND m.enabled=0 THEN e.nickname END
                 FROM members m LEFT JOIN enrollment e ON e.member=m.id
                 WHERE m.id=? AND m.archived_name IS NULL",
                [&body.id],
                |row| {
                    Ok((
                        role(&row.get::<_, String>(0)?)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or(Failure(404, "workspace_member_not_found"))?;
        if !who.role.manages(role) {
            return Err(forbidden());
        }
        let nickname = nickname.ok_or(Failure(409, "workspace_member_not_pending"))?;
        let at = now();
        tx.execute("DELETE FROM rejected_enrollments WHERE expires<=?", [at])?;
        tx.execute(
            "INSERT INTO rejected_enrollments(name,salt,password_hash,expires)
             SELECT name,salt,password_hash,? FROM members WHERE id=?",
            params![at + REJECTION_TTL, body.id],
        )?;
        audit::record(&tx, who, Some(&body.id), "member_rejected", json!({}))?;
        audit::detach_target(&tx, &body.id, &name, &nickname)?;
        tx.execute("DELETE FROM member_tiers WHERE member=?", [&body.id])?;
        tx.execute("DELETE FROM enrollment WHERE member=?", [&body.id])?;
        tx.execute("DELETE FROM members WHERE id=?", [&body.id])?;
        tx.commit()?;
        Ok(json!({"ok":true}))
    }

    pub(super) fn remove_member(&mut self, who: &Identity, target: MemberTarget) -> Result<Value> {
        if !who.role.can_manage() || !who.role.manages(self.role_for(&target.id)?) {
            return Err(forbidden());
        }
        let tx = self.db.transaction()?;
        let changed = tx.execute(
            "UPDATE members SET archived_name=name,name='#'||id,password_hash=X'',salt=X'',
                enabled=0,publish=0,sync=0 WHERE id=? AND NOT EXISTS
                (SELECT 1 FROM enrollment WHERE member=?1 AND pending=1)",
            [&target.id],
        )?;
        if changed == 0 {
            return Err(Failure(409, "workspace_member_pending"));
        }
        tx.execute("DELETE FROM sessions WHERE member=?", [&target.id])?;
        tx.execute("DELETE FROM join_invites WHERE issuer=?", [&target.id])?;
        tx.execute(
            "UPDATE shares SET encrypted=X'',version=version+1,removed_at=? WHERE owner=? AND removed_at IS NULL",
            params![now(), target.id],
        )?;
        audit::record(&tx, who, Some(&target.id), "member_removed", json!({}))?;
        tx.commit()?;
        Ok(json!({"ok":true}))
    }

    pub(super) fn role_for(&self, id: &str) -> Result<Role> {
        self.db
            .query_row(
                "SELECT role FROM members WHERE id=? AND archived_name IS NULL",
                [id],
                |row| role(&row.get::<_, String>(0)?),
            )
            .optional()?
            .ok_or(Failure(404, "workspace_member_not_found"))
    }

    pub(super) fn member_tiers(&mut self, who: &Identity, tiers: Tiers) -> Result<Value> {
        if !who.role.can_manage() || !who.role.manages(self.role_for(&tiers.id)?) {
            return Err(forbidden());
        }
        let tx = self.db.transaction()?;
        let (fast, ultrafast): (bool, bool) = tx.query_row(
            "SELECT COALESCE(t.fast,m.role!='member'),COALESCE(t.ultrafast,m.role!='member')
             FROM members m LEFT JOIN member_tiers t ON t.member=m.id WHERE m.id=?",
            [&tiers.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        tx.execute(
            "INSERT INTO member_tiers VALUES (?,?,?) ON CONFLICT(member)
             DO UPDATE SET fast=excluded.fast,ultrafast=excluded.ultrafast",
            params![tiers.id, tiers.fast, tiers.ultrafast],
        )?;
        let mut changes = serde_json::Map::new();
        if fast != tiers.fast {
            changes.insert("fast".into(), json!(tiers.fast));
        }
        if ultrafast != tiers.ultrafast {
            changes.insert("ultrafast".into(), json!(tiers.ultrafast));
        }
        if !changes.is_empty() {
            audit::record(
                &tx,
                who,
                Some(&tiers.id),
                "member_tiers",
                Value::Object(changes),
            )?;
        }
        tx.commit()?;
        Ok(json!({"ok":true}))
    }

    pub(super) fn handover(&mut self, who: &Identity, body: Handover) -> Result<Value> {
        if who.role != Role::TeamAdmin {
            return Err(forbidden());
        }
        let tx = self.db.transaction()?;
        let appointed = tx.execute(
            "UPDATE members SET role='team_admin' WHERE id=? AND role='member' AND enabled=1
             AND NOT EXISTS(SELECT 1 FROM enrollment WHERE member=? AND pending=1)",
            params![body.successor, body.successor],
        )?;
        if appointed == 0 {
            return Err(Failure(409, "workspace_handover_successor_required"));
        }
        tx.execute("UPDATE members SET role='member' WHERE id=?", [&who.member])?;
        tx.execute("DELETE FROM join_invites WHERE issuer=?", [&who.member])?;
        audit::record(&tx, who, Some(&body.successor), "handover", json!({}))?;
        tx.commit()?;
        Ok(json!({"ok":true}))
    }
}

pub(super) fn fixed_eq(left: &[u8], right: &[u8]) -> bool {
    right.len() == left.len()
        && left
            .iter()
            .zip(right)
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
}
