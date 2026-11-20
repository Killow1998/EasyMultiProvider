use super::*;

const INVITATION_SECONDS: i64 = 202;
const CODE_COUNT: u32 = 10_000_000;
const ATTEMPT_WINDOW: i64 = 300;
const MAX_ATTEMPTS: i64 = 30;

#[derive(Deserialize)]
pub(super) struct Join {
    nickname: String,
    name: String,
    password: String,
    code: String,
}

impl Workspace {
    pub(super) fn join_code(&self, who: &Identity, at: i64, issue: bool) -> Result<Value> {
        if !who.role.can_manage() {
            return Err(forbidden());
        }
        let active = self
            .db
            .query_row(
                "SELECT code,expires FROM join_invites WHERE issuer=? AND expires>?",
                params![who.member, at],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        if let Some((code, expires)) = active {
            return Ok(
                json!({"code":code,"expires_in":expires-at,"lifetime":INVITATION_SECONDS,"workspace":self.name}),
            );
        }
        if !issue {
            return Ok(json!({"code":null}));
        }
        self.db
            .execute("DELETE FROM join_invites WHERE expires<=?", [at])?;
        loop {
            let bytes = u32::from_le_bytes(random()?);
            if bytes >= u32::MAX / CODE_COUNT * CODE_COUNT {
                continue;
            }
            let code = format!("{:07}", bytes % CODE_COUNT);
            let inserted = self.db.execute(
                "INSERT INTO join_invites VALUES (?,?,?) ON CONFLICT(code) DO NOTHING",
                params![who.member, code, at + INVITATION_SECONDS],
            )?;
            if inserted == 1 {
                return Ok(
                    json!({"code":code,"expires_in":INVITATION_SECONDS,"lifetime":INVITATION_SECONDS,"workspace":self.name}),
                );
            }
        }
    }

    pub(super) fn join(&mut self, join: Join, at: i64) -> Result<Value> {
        let nickname = join.nickname.trim();
        if nickname.is_empty()
            || nickname.chars().count() > 48
            || nickname.chars().any(char::is_control)
            || join.code.len() != 7
            || !join.code.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(invalid());
        }
        let window = at.div_euclid(ATTEMPT_WINDOW);
        self.db
            .execute("DELETE FROM join_limits WHERE window<?", [window])?;
        let attempts: i64 = self.db.query_row(
            "INSERT INTO join_limits VALUES (?,1) ON CONFLICT(window) DO UPDATE SET attempts=attempts+1 RETURNING attempts",
            [window],
            |row| row.get(0),
        )?;
        if attempts > MAX_ATTEMPTS {
            return Err(Failure(429, "workspace_join_limited"));
        }
        let valid: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM join_invites i JOIN members m ON m.id=i.issuer
             WHERE i.code=? AND i.expires>? AND m.enabled=1 AND m.role IN ('vps_admin','team_admin'))",
            params![join.code, at],
            |row| row.get(0),
        )?;
        if !valid {
            return Err(Failure(403, "workspace_join_code_invalid"));
        }
        let tx = self.db.transaction()?;
        let id = accounts::create(&tx, &join.name, &join.password, Role::Member, false, true)?;
        tx.execute("UPDATE members SET enabled=0 WHERE id=?", [&id])?;
        tx.execute(
            "INSERT INTO enrollment VALUES (?,?,1)",
            params![id, nickname],
        )?;
        tx.commit()?;
        Ok(json!({"pending":true}))
    }
}
