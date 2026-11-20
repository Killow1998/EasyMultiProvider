use super::*;
use emp_modules::workspace::NATIVE_SHARE;
use emp_modules::workspace::{Publish, SharePayload, SharedService};

#[derive(Deserialize)]
pub(super) struct Review {
    id: String,
    version: i64,
    shared: bool,
}

#[derive(Deserialize)]
pub(super) struct ResetAuthority {
    share: String,
    version: i64,
}

#[derive(Deserialize)]
pub(super) struct SubscriptionReminder {
    share: String,
}

impl Workspace {
    pub(super) fn publish_version(&self, who: &Identity) -> Result<Value> {
        if !who.publish {
            return Err(forbidden());
        }
        let version: Option<i64> = self
            .db
            .query_row(
                "SELECT version FROM shares WHERE owner=? AND source=?",
                params![who.member, NATIVE_SHARE],
                |row| row.get(0),
            )
            .optional()?;
        Ok(json!({"version":version.unwrap_or_default()}))
    }

    pub(super) fn publish(&mut self, who: &Identity, body: Publish) -> Result<Value> {
        if !who.publish {
            return Err(forbidden());
        }
        if body.source != NATIVE_SHARE
            || !body.payload.valid()
            || body
                .payload
                .subscription
                .as_ref()
                .is_some_and(|auth| auth.expires_at <= now())
        {
            return Err(invalid());
        }
        let tx = self.db.transaction()?;
        let existing: Option<(String, i64, Vec<u8>, bool)> = tx
            .query_row(
                "SELECT s.id,s.version,s.encrypted,r.approved_version=s.version
                 FROM shares s JOIN share_reviews r ON r.share=s.id WHERE s.owner=? AND s.source=?",
                params![who.member, body.source],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let mut plaintext = serde_json::to_vec(&body.payload)?;
        let mut same_account = false;
        if let Some((id, version, encrypted, approved)) = &existing
            && let Ok(previous) = decode_vault(&self.key, encrypted)
        {
            if body.expected_version <= *version && *previous == plaintext {
                return Ok(json!({"id":id,"version":version,"review_required":!approved}));
            }
            if let Ok(prior) = parse::<SharePayload>(&previous)
                && let (Some(prior), Some(incoming)) =
                    (prior.subscription, &body.payload.subscription)
                && prior.account_id == incoming.account_id
            {
                if prior.expires_at > incoming.expires_at {
                    return Err(Failure(409, "workspace_share_version_conflict"));
                }
                same_account = true;
            }
        }
        if existing.as_ref().map_or(0, |(_, version, _, _)| *version) != body.expected_version {
            return Err(Failure(409, "workspace_share_version_conflict"));
        }
        if plaintext.len() > MAX_BODY / 8 {
            return Err(Failure(413, "workspace_share_too_large"));
        }
        let encrypted = encode_vault(&self.key, &plaintext);
        plaintext.fill(0);
        let version = body.expected_version + 1;
        let mut review_required = true;
        let id = if let Some((id, _, _, approved)) = existing {
            tx.execute(
                "UPDATE shares SET version=?,encrypted=? WHERE id=?",
                params![version, encrypted, id],
            )?;
            if same_account {
                review_required = !approved;
                if approved {
                    tx.execute(
                        "UPDATE share_reviews SET approved_version=? WHERE share=?",
                        params![version, id],
                    )?;
                }
                audit::record(
                    &tx,
                    who,
                    Some(&who.member),
                    "share_refreshed",
                    json!({"share":id,"version":version,"review_required":review_required}),
                )?;
            }
            id
        } else {
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM shares", [], |row| row.get(0))?;
            if count >= MAX_SHARES {
                return Err(Failure(409, "workspace_share_limit_reached"));
            }
            let id = hex_id()?;
            tx.execute(
                "INSERT INTO shares VALUES (?,?,?,?,?)",
                params![id, who.member, body.source, version, encrypted],
            )?;
            tx.execute("INSERT INTO share_reviews VALUES (?,0)", [&id])?;
            id
        };
        tx.commit()?;
        Ok(json!({"id":id,"version":version,"review_required":review_required}))
    }

    pub(super) fn shares(&self, who: &Identity) -> Result<Value> {
        if !who.sync {
            return Err(forbidden());
        }
        let mut stmt = self.db.prepare(
            "SELECT s.id,s.version,s.encrypted,COALESCE(e.nickname,m.name) FROM shares s JOIN members m ON m.id=s.owner
             JOIN share_reviews r ON r.share=s.id AND r.approved_version=s.version
             LEFT JOIN enrollment e ON e.member=m.id
             WHERE m.enabled=1 AND m.publish=1 AND s.source=? ORDER BY s.id",
        )?;
        let mut rows = stmt.query([NATIVE_SHARE])?;
        let mut shares = Vec::new();
        let mut size = 0;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(2)?;
            let plaintext = decode_vault(&self.key, &bytes)
                .map_err(|_| Failure(503, "workspace_share_unavailable"))?;
            let payload: SharePayload = parse(&plaintext)?;
            if payload
                .subscription
                .as_ref()
                .is_some_and(|auth| auth.expires_at <= now())
            {
                continue;
            }
            size += plaintext.len();
            if size > MAX_BODY * 4 {
                return Err(Failure(413, "workspace_snapshot_too_large"));
            }
            shares.push(SharedService {
                id: row.get(0)?,
                version: row.get(1)?,
                owner: payload
                    .subscription
                    .is_some()
                    .then(|| row.get::<_, String>(3))
                    .transpose()?,
                payload,
            });
        }
        as_json(json!({"workspace_id":self.id,"shares":shares}))
    }

    pub(super) fn reviewable_shares(&self, who: &Identity) -> Result<Value> {
        if !who.role.can_manage() {
            return Err(forbidden());
        }
        let mut stmt = self.db.prepare(
            "SELECT s.id,s.version,s.encrypted,r.approved_version=s.version,COALESCE(e.nickname,m.name),m.enabled AND m.publish
             FROM shares s JOIN members m ON m.id=s.owner JOIN share_reviews r ON r.share=s.id
             LEFT JOIN enrollment e ON e.member=m.id WHERE s.source=?
             ORDER BY r.approved_version=s.version,s.id",
        )?;
        let mut usage = self.share_usage()?;
        let mut rows = stmt.query([NATIVE_SHARE])?;
        let mut shares = Vec::new();
        let at = now();
        while let Some(row) = rows.next()? {
            let payload = decode_vault(&self.key, &row.get::<_, Vec<u8>>(2)?)
                .map_err(|_| Failure(503, "workspace_share_unavailable"))?;
            let payload: SharePayload = parse(&payload)?;
            let id: String = row.get(0)?;
            let totals = usage.remove(&id);
            let available: bool = row.get(5)?;
            let valid = available
                && payload
                    .subscription
                    .as_ref()
                    .is_some_and(|auth| auth.expires_at > at);
            shares.push(json!({"id":id,"version":row.get::<_,i64>(1)?,
                "name":payload.provider["name"],"models":payload.models.len(),"shared":row.get::<_,bool>(3)?,
                "owner":row.get::<_,String>(4)?,"available":available,"valid":valid,
                "plan":payload.subscription.and_then(|auth| auth.plan),"usage":totals}));
        }
        Ok(json!({"services":shares}))
    }

    pub(super) fn review_share(&mut self, who: &Identity, review: Review) -> Result<Value> {
        if !who.role.can_manage() {
            return Err(forbidden());
        }
        let approved = if review.shared { review.version } else { 0 };
        let tx = self.db.transaction()?;
        let (owner, previous): (String, i64) = tx.query_row(
            "SELECT s.owner,r.approved_version FROM shares s JOIN share_reviews r ON r.share=s.id
             WHERE s.id=? AND s.version=? AND s.source=?",
            params![review.id, review.version, NATIVE_SHARE],
            |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?.ok_or(Failure(409, "workspace_share_version_conflict"))?;
        if approved != previous {
            tx.execute(
                "UPDATE share_reviews SET approved_version=? WHERE share=?",
                params![approved, review.id],
            )?;
            audit::record(
                &tx,
                who,
                Some(&owner),
                "share_review",
                json!({"shared":review.shared,"version":review.version}),
            )?;
        }
        tx.commit()?;
        Ok(json!({"shared":review.shared}))
    }

    pub(super) fn reset_authority(&self, who: &Identity, body: ResetAuthority) -> Result<Value> {
        if !who.role.can_manage() || !who.sync {
            return Err(forbidden());
        }
        let owner: Option<String> = self
            .db
            .query_row(
                "SELECT s.owner FROM shares s JOIN members m ON m.id=s.owner
             JOIN share_reviews r ON r.share=s.id AND r.approved_version=s.version
             WHERE s.id=? AND s.version=? AND s.source=? AND m.enabled=1 AND m.publish=1",
                params![body.share, body.version, NATIVE_SHARE],
                |row| row.get(0),
            )
            .optional()?;
        let owner = owner.ok_or_else(forbidden)?;
        audit::record(&self.db, who, Some(&owner), "reset_requested", json!({}))?;
        Ok(json!({"workspace":self.id}))
    }

    pub(super) fn subscriptions(&self, who: &Identity) -> Result<Value> {
        let mut usage = self.share_usage()?;
        let mut query = self.db.prepare(
            "SELECT s.id,s.owner,s.encrypted,COALESCE(e.nickname,m.name),r.approved_version,
                    s.version,(SELECT MAX(sent_at) FROM subscription_reminders WHERE share=s.id)
             FROM shares s JOIN members m ON m.id=s.owner JOIN share_reviews r ON r.share=s.id
             LEFT JOIN enrollment e ON e.member=m.id
             WHERE s.source=? AND m.enabled=1 AND m.publish=1
               AND (r.approved_version>0 OR s.owner=? OR ?) ORDER BY m.name,s.id",
        )?;
        let mut entries = query.query(params![NATIVE_SHARE, who.member, who.role.can_manage()])?;
        let mut subscriptions = Vec::new();
        while let Some(row) = entries.next()? {
            let bytes: Vec<u8> = row.get(2)?;
            let data = decode_vault(&self.key, &bytes)
                .map_err(|_| Failure(503, "workspace_share_unavailable"))?;
            let payload: SharePayload = parse(&data)?;
            let Some(auth) = payload.subscription else {
                continue;
            };
            let owner = row.get::<_, String>(1)? == who.member;
            let approved = row.get::<_, i64>(4)?;
            let current = row.get::<_, i64>(5)?;
            let share: String = row.get(0)?;
            let totals = usage.remove(&share);
            subscriptions.push(json!({
                "share":share,"owner":row.get::<_,String>(3)?,"yours":owner,
                "name":payload.provider["name"],"plan":auth.plan,"expires_at":auth.expires_at,
                "status":if approved != current {"review"} else if auth.expires_at<=now() {"expired"} else {"available"},
                "reminded_at":if owner {row.get::<_,Option<i64>>(6)?} else {None},
                "usage":totals,
            }));
        }
        Ok(json!({"subscriptions":subscriptions}))
    }

    fn share_usage(&self) -> Result<std::collections::BTreeMap<String, Value>> {
        self.db.prepare(
            "SELECT share, SUM(input_tokens), SUM(output_tokens),
                    SUM(COALESCE(actual_nanos,estimate_nanos)) FROM usage GROUP BY share",
        )?
        .query_map([], |row| {
            Ok((row.get(0)?,json!({"input_tokens":row.get::<_,Option<i64>>(1)?,
                "output_tokens":row.get::<_,Option<i64>>(2)?,"spend_nanos":row.get::<_,Option<i64>>(3)?})))
        })?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
    }

    pub(super) fn subscription_reminder(
        &self,
        who: &Identity,
        reminder: SubscriptionReminder,
    ) -> Result<Value> {
        let (owner, bytes): (String, Vec<u8>) = self.db.query_row(
            "SELECT s.owner,s.encrypted FROM shares s JOIN share_reviews r ON s.id=r.share AND s.version=r.approved_version
             JOIN members m ON m.id=s.owner WHERE s.id=? AND s.source=? AND m.enabled=1 AND m.publish=1",
            params![reminder.share, NATIVE_SHARE],|row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?.ok_or_else(missing)?;
        let data = decode_vault(&self.key, &bytes)
            .map_err(|_| Failure(503, "workspace_share_unavailable"))?;
        let payload: SharePayload = parse(&data)?;
        let at = now();
        if owner == who.member || payload.subscription.is_none_or(|auth| auth.expires_at > at) {
            return Err(Failure(409, "workspace_subscription_current"));
        }
        self.db.execute(
            "INSERT INTO subscription_reminders VALUES (?,?,?,?) ON CONFLICT DO NOTHING",
            params![reminder.share, super::profiles::pst_day(at), who.member, at],
        )?;
        Ok(json!({"ok":true}))
    }
}
