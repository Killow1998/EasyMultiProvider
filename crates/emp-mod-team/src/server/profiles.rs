use super::*;

pub(super) const DAY: i64 = 86_400;
pub(super) fn pst_day(at: i64) -> i64 {
    (at - 8 * 3600).div_euclid(DAY) * DAY + 8 * 3600
}

#[derive(Deserialize)]
pub(super) struct ProfileRequest {
    member: String,
}

#[derive(Deserialize)]
pub(super) struct Visibility {
    visible: bool,
}

pub(super) fn record_joined(db: &Connection, member: &str, at: i64) -> Result<()> {
    db.execute(
        "INSERT INTO member_joined VALUES (?,?) ON CONFLICT DO NOTHING",
        params![member, at],
    )?;
    Ok(())
}

impl Workspace {
    pub(super) fn heartbeat(&self, who: &Identity) -> Result<Value> {
        let at = now();
        self.db.execute(
            "INSERT INTO member_days VALUES (?,?,?) ON CONFLICT DO NOTHING",
            params![who.member, pst_day(at), at],
        )?;
        Ok(json!({"ok":true}))
    }

    fn profile_policy(&self) -> Result<(Option<i64>, bool)> {
        Ok(self
            .db
            .query_row(
                "SELECT created_at,profiles_visible FROM workspace_profile WHERE slot=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, false)))
    }

    pub(super) fn set_profile_visibility(
        &mut self,
        who: &Identity,
        settings: Visibility,
    ) -> Result<Value> {
        if !who.role.can_manage() {
            return Err(forbidden());
        }
        let tx = self.db.transaction()?;
        let changed = tx.execute(
            "INSERT INTO workspace_profile(slot,profiles_visible) VALUES (1,?)
             ON CONFLICT(slot) DO UPDATE SET profiles_visible=excluded.profiles_visible
             WHERE workspace_profile.profiles_visible IS NOT excluded.profiles_visible",
            [settings.visible],
        )?;
        if changed != 0 {
            audit::record(
                &tx,
                who,
                None,
                "profile_visibility",
                json!({"visible":settings.visible}),
            )?;
        }
        tx.commit()?;
        Ok(json!({"visible":settings.visible}))
    }

    pub(super) fn profiles(&self, who: &Identity) -> Result<Value> {
        let (created, publicly_visible) = self.profile_policy()?;
        let day = pst_day(now());
        let mut query = self.db.prepare(
            "SELECT m.id,COALESCE(e.nickname,m.name),m.role,j.joined_at,d.online_at
             FROM members m LEFT JOIN enrollment e ON e.member=m.id
             LEFT JOIN member_joined j ON j.member=m.id
             LEFT JOIN member_days d ON d.member=m.id AND d.day=?
             WHERE m.enabled=1 AND (m.id=? OR (? AND m.role!='vps_admin')) ORDER BY m.name",
        )?;
        let rows = query.query_map(params![day,who.member,publicly_visible || who.role.can_manage()], |row| {
            Ok(json!({"member":row.get::<_,String>(0)?,"name":row.get::<_,String>(1)?,"role":row.get::<_,String>(2)?,
                "joined_at":row.get::<_,Option<i64>>(3)?,"online_at":row.get::<_,Option<i64>>(4)?}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({"created_at":created,"profiles_visible":publicly_visible,
            "can_manage":who.role.can_manage(),"day":day,"members":rows}),
        )
    }

    pub(super) fn profile(&self, who: &Identity, request: ProfileRequest) -> Result<Value> {
        let (created, publicly_visible) = self.profile_policy()?;
        if who.member != request.member && !who.role.can_manage() && !publicly_visible {
            return Err(forbidden());
        }
        let (name, joined): (String, Option<i64>) = self
            .db
            .query_row(
                "SELECT COALESCE(e.nickname,m.name),j.joined_at FROM members m
             LEFT JOIN enrollment e ON e.member=m.id LEFT JOIN member_joined j ON j.member=m.id
             WHERE m.id=? AND m.enabled=1 AND (m.role!='vps_admin' OR m.id=?)",
                params![request.member, who.member],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(missing)?;
        let lifetime = self.db.query_row(
            "SELECT SUM(input_tokens),SUM(output_tokens),SUM(estimate_nanos),SUM(actual_nanos),
                    COUNT(CASE WHEN estimate_nanos IS NULL AND actual_nanos IS NULL THEN 1 END),
                    SUM(COALESCE(actual_nanos,estimate_nanos))
             FROM usage WHERE member=?", [&request.member], |row| {
                Ok(json!({"input_tokens":row.get::<_,Option<i64>>(0)?,"output_tokens":row.get::<_,Option<i64>>(1)?,
                    "estimated_usd_nanos":row.get::<_,Option<i64>>(2)?,"actual_usd_nanos":row.get::<_,Option<i64>>(3)?,
                    "unknown_cost_requests":row.get::<_,i64>(4)?,"spend_nanos":row.get::<_,Option<i64>>(5)?}))
            },
        )?;
        let day = pst_day(now());
        let mut query = self.db.prepare(
            "SELECT d.day,d.online_at,SUM(u.input_tokens),SUM(u.output_tokens) FROM member_days d
             LEFT JOIN usage u ON u.member=d.member AND u.finished_at>=d.day AND u.finished_at<d.day+86400
             WHERE d.member=? AND d.day>=? GROUP BY d.day,d.online_at ORDER BY d.day DESC",
        )?;
        let activity = query.query_map(params![request.member,day-29*DAY], |row| {
            Ok(json!({"day":row.get::<_,i64>(0)?,"online_at":row.get::<_,i64>(1)?,
                "input_tokens":row.get::<_,Option<i64>>(2)?,"output_tokens":row.get::<_,Option<i64>>(3)?}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({"member":request.member,"name":name,"joined_at":joined,"created_at":created,
            "lifetime":lifetime,"activity":activity,"day":day}),
        )
    }
}
