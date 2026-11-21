use super::*;
use emp_modules::team::{BudgetKind, BudgetStatus, BudgetStatuses};
use emp_modules::workspace::{MAX_USAGE_MODEL_BYTES, UsageEvent};
use std::collections::BTreeMap;

const NANOS_PER_DOLLAR: i64 = 1_000_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
pub(super) const MAX_MONEY: i64 = 1_000_000 * NANOS_PER_DOLLAR;

#[derive(Deserialize)]
pub(super) struct Budget {
    member: String,
    daily_usd_nanos: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct Report {
    events: Vec<UsageEvent>,
}

#[derive(Clone, Copy, Deserialize)]
pub(super) enum Period {
    #[serde(rename = "today")]
    Today,
    #[serde(rename = "1h")]
    Hour,
    #[serde(rename = "1d")]
    Day,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
    #[serde(rename = "lifetime")]
    Lifetime,
}

#[derive(Deserialize)]
pub(super) struct UsageFilter {
    pub(super) period: Period,
}

impl Workspace {
    pub(super) fn budget(&mut self, who: &Identity, body: Budget) -> Result<Value> {
        if !who.role.can_manage() || !who.may_set_budget(&body.member, self.role_for(&body.member)?)
        {
            return Err(forbidden());
        }
        if body
            .daily_usd_nanos
            .is_some_and(|nanos| !(0..=MAX_MONEY).contains(&nanos))
        {
            return Err(invalid());
        }
        let tx = self.db.transaction()?;
        let changed = tx.execute(
            "UPDATE members SET budget_nanos=?1 WHERE id=?2 AND budget_nanos IS NOT ?1",
            params![body.daily_usd_nanos, body.member],
        )?;
        if changed != 0 {
            audit::record(
                &tx,
                who,
                Some(&body.member),
                "budget",
                json!({"daily_usd_nanos":body.daily_usd_nanos}),
            )?;
        }
        tx.commit()?;
        Ok(json!({"ok":true}))
    }

    pub(super) fn report(&mut self, who: &Identity, body: Report) -> Result<Value> {
        if !who.sync {
            return Err(forbidden());
        }
        if body.events.len() > 250 {
            return Err(invalid());
        }
        let tx = self.db.transaction()?;
        let mut inserted = 0;
        for event in body.events {
            if event.id.is_empty()
                || event.id.len() > 320
                || !identifier(&event.share)
                || event.model.is_empty()
                || event.model.len() > MAX_USAGE_MODEL_BYTES
                || event.finished_at <= 0
                || event.finished_at > now() + 600
                || [
                    event.input_tokens,
                    event.cached_input_tokens,
                    event.output_tokens,
                ]
                .into_iter()
                .flatten()
                .any(|tokens| !(0..=i64::from(i32::MAX)).contains(&tokens))
                || [event.estimated_usd_nanos, event.actual_usd_nanos]
                    .into_iter()
                    .flatten()
                    .any(|cost| !(0..=MAX_MONEY).contains(&cost))
            {
                return Err(invalid());
            }
            let share_exists = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM shares WHERE id=?)",
                [&event.share],
                |row| row.get::<_, bool>(0),
            )?;
            if !share_exists {
                return Err(Failure(404, "workspace_share_not_found"));
            }
            inserted += tx.execute(
                "INSERT INTO usage VALUES (?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(member,device,id) DO NOTHING",
                params![who.member,who.device,event.id,event.share,event.model,event.finished_at,event.input_tokens,
                    event.cached_input_tokens,event.output_tokens,event.estimated_usd_nanos,event.actual_usd_nanos],
            )?;
        }
        tx.commit()?;
        Ok(json!({"inserted":inserted,"budget":self.budget_status(who)?}))
    }

    pub(super) fn budget_status(&self, who: &Identity) -> Result<BudgetStatuses> {
        let at = now();
        let day = at.div_euclid(SECONDS_PER_DAY) * SECONDS_PER_DAY;
        let mode = self.budget_mode()?;
        let (limit, used): (Option<i64>, Option<i64>) = if mode == BudgetKind::DailyUsd {
            self.db.query_row(
            "SELECT m.budget_nanos,
                    SUM(COALESCE(u.actual_nanos,u.estimate_nanos))
             FROM members m LEFT JOIN usage u ON u.member=m.id AND u.finished_at>=? AND u.finished_at<=?
             WHERE m.id=? GROUP BY m.id",
            params![day, at, who.member],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
        } else {
            (None, None)
        };
        let reset = if mode == BudgetKind::WeeklyUsd {
            self.weekly_reset()?
        } else {
            None
        };
        let weekly = match reset {
            Some(reset) => Some(self.weekly_usd_status(&who.member, reset.window(at)?.0, at)?),
            None => None,
        };
        Ok(BudgetStatuses {
            daily: BudgetStatus {
                day,
                used,
                limit,
                kind: BudgetKind::DailyUsd,
            },
            weekly,
        })
    }

    pub(super) fn overview(&self, who: &Identity) -> Result<Value> {
        let at = now();
        let day = at.div_euclid(SECONDS_PER_DAY) * SECONDS_PER_DAY;
        let (created, joined, limit): (Option<i64>, Option<i64>, Option<i64>) = self.db.query_row(
            "SELECT p.created_at,j.joined_at,m.budget_nanos FROM members m
             LEFT JOIN member_joined j ON j.member=m.id LEFT JOIN workspace_profile p ON p.slot=1
             WHERE m.id=?",
            [&who.member],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let members: Option<i64> = if who.role.can_manage() {
            Some(self.db.query_row(
                "SELECT COUNT(*) FROM members WHERE enabled=1 AND role<>'vps_admin'",
                [],
                |row| row.get(0),
            )?)
        } else {
            None
        };
        let mut query = self.db.prepare(
            "SELECT COUNT(*), SUM(input_tokens), SUM(output_tokens),
                    SUM(COALESCE(actual_nanos,estimate_nanos)),
                    COUNT(CASE WHEN actual_nanos IS NULL AND estimate_nanos IS NULL THEN 1 END)
             FROM usage WHERE (member=? OR ?) AND finished_at>=? AND finished_at<=?",
        )?;
        let mut period = |start| {
            query.query_row(params![who.member,who.role.can_manage(),start,at], |row| {
                Ok(json!({"requests":row.get::<_,i64>(0)?,
                    "input_tokens":row.get::<_,Option<i64>>(1)?,"output_tokens":row.get::<_,Option<i64>>(2)?,
                    "spend_nanos":row.get::<_,Option<i64>>(3)?,"unknown_cost_requests":row.get::<_,i64>(4)?}))
            })
        };
        let mode = self.budget_mode()?;
        let reset = self.weekly_reset()?;
        let weekly = match (&reset, mode) {
            (Some(reset), BudgetKind::WeeklyUsd) => {
                let (start, end) = reset.window(at)?;
                let status = self.weekly_usd_status(&who.member, start, at)?;
                json!({"reset":reset,"reset_at":end,"used":status.used,"limit":status.limit})
            }
            _ => Value::Null,
        };
        Ok(
            json!({"created_at":created,"joined_at":joined,"members":members,
            "budget_nanos":limit,"today":period(day)?,"lifetime":period(0)?,"weekly_budget":weekly,
            "weekly_reset":reset,"budget_mode":mode}),
        )
    }

    pub(super) fn usage(&self, who: &Identity, period: Period) -> Result<Value> {
        let end = now() + 1;
        let day = (end - 1).div_euclid(SECONDS_PER_DAY) * SECONDS_PER_DAY;
        let daily = self.budget_mode()? == BudgetKind::DailyUsd;
        let start = match period {
            Period::Today => day,
            Period::Hour => end - 3600,
            Period::Day => end - SECONDS_PER_DAY,
            Period::Week => end - 7 * SECONDS_PER_DAY,
            Period::Month => end - 30 * SECONDS_PER_DAY,
            Period::Lifetime => 0,
        };
        let mut models: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut model_query = self.db.prepare(
            "SELECT member,model,SUM(input_tokens),SUM(cached_input_tokens),SUM(output_tokens),
                    SUM(estimate_nanos), SUM(actual_nanos),COUNT(CASE WHEN actual_nanos IS NULL AND estimate_nanos IS NULL THEN 1 END),
                    SUM(COALESCE(actual_nanos,estimate_nanos))
             FROM usage WHERE finished_at>=? AND finished_at<? AND (member=? OR ?)
             GROUP BY member,model ORDER BY model",
        )?;
        let mut rows = model_query.query(params![start, end, who.member, who.role.can_manage()])?;
        while let Some(row) = rows.next()? {
            models.entry(row.get(0)?).or_default().push(json!({
                "model":row.get::<_,String>(1)?,"input_tokens":row.get::<_,Option<i64>>(2)?,
                "cached_input_tokens":row.get::<_,Option<i64>>(3)?,"output_tokens":row.get::<_,Option<i64>>(4)?,
                "estimated_usd_nanos":row.get::<_,Option<i64>>(5)?,"actual_usd_nanos":row.get::<_,Option<i64>>(6)?,
                "unknown_cost_requests":row.get::<_,i64>(7)?,
                "spend_nanos":row.get::<_,Option<i64>>(8)?,
            }));
        }
        let mut statement = self.db.prepare(
            "SELECT m.id,COALESCE(e.nickname,m.archived_name,m.name),m.budget_nanos,COUNT(u.id),SUM(u.input_tokens),SUM(u.cached_input_tokens),
                    SUM(u.output_tokens),SUM(u.estimate_nanos),SUM(u.actual_nanos),
                    SUM(CASE WHEN u.id IS NOT NULL AND u.actual_nanos IS NULL AND u.estimate_nanos IS NULL THEN 1 ELSE 0 END),
                    CASE WHEN ? THEN (SELECT SUM(COALESCE(d.actual_nanos,d.estimate_nanos)) FROM usage d WHERE d.member=m.id
                     AND d.finished_at>=? AND d.finished_at<?) END,
                    SUM(COALESCE(u.actual_nanos,u.estimate_nanos))
             FROM members m LEFT JOIN usage u ON u.member=m.id AND u.finished_at>=? AND u.finished_at<?
             LEFT JOIN enrollment e ON e.member=m.id
             WHERE m.id=? OR ? GROUP BY m.id ORDER BY COALESCE(m.archived_name,m.name)"
        )?;
        let rows = statement.query_map(params![daily,day,day+SECONDS_PER_DAY,start,end,who.member,who.role.can_manage()], |r| {
            let member: String = r.get(0)?;
            let budget: Option<i64> = r.get(2)?;
            let budget_usage: Option<i64> = r.get(10)?;
            Ok(json!({
                "member":member, "name":r.get::<_,String>(1)?,"budget_nanos":budget,"models":models.remove(&member).unwrap_or_default(),
                "requests":r.get::<_,i64>(3)?, "input_tokens":r.get::<_,Option<i64>>(4)?,
                "cached_input_tokens":r.get::<_,Option<i64>>(5)?, "output_tokens":r.get::<_,Option<i64>>(6)?,
                "estimated_usd_nanos":r.get::<_,Option<i64>>(7)?, "actual_usd_nanos":r.get::<_,Option<i64>>(8)?,
                "unknown_cost_requests":r.get::<_,i64>(9)?, "budget_usage_nanos":budget_usage,
                "spend_nanos":r.get::<_,Option<i64>>(11)?,
                "budget_reached":budget.zip(budget_usage).is_some_and(|(budget,used)|used>=budget),
            }))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({"start":start,"end":end,"day_start":day,"reset_at":day+SECONDS_PER_DAY,"timezone":"UTC","enforcement":"advisory","rows":rows}),
        )
    }
}
