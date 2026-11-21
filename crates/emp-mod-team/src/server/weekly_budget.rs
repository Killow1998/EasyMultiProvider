use super::*;
use emp_modules::team::{BudgetKind, BudgetStatus};
use jiff::{Timestamp, ToSpan, tz::TimeZone};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(super) enum Edit {
    Initialize {
        timezone: String,
    },
    Reset {
        weekday: i8,
        hour: i8,
    },
    MemberLimit {
        member: String,
        usd_nanos: Option<i64>,
    },
    Mode {
        mode: BudgetKind,
    },
}

#[derive(Serialize)]
pub(super) struct Reset {
    weekday: i8,
    hour: i8,
    timezone: String,
}

impl Reset {
    pub(super) fn window(&self, at: i64) -> Result<(i64, i64)> {
        let timezone = TimeZone::get(&self.timezone).map_err(|_| invalid())?;
        let local = Timestamp::from_second(at)
            .map_err(|_| invalid())?
            .to_zoned(timezone.clone());
        let elapsed = (local.weekday().to_sunday_zero_offset() - self.weekday).rem_euclid(7);
        let boundary = |delta: i8| {
            local
                .date()
                .checked_add(delta.days())?
                .at(self.hour, 0, 0, 0)
                .to_zoned(timezone.clone())
                .map(|date| date.timestamp().as_second())
        };
        let current = boundary(-elapsed).map_err(|_| invalid())?;
        if current > at {
            Ok((boundary(-elapsed - 7).map_err(|_| invalid())?, current))
        } else {
            Ok((current, boundary(7 - elapsed).map_err(|_| invalid())?))
        }
    }
}

impl Workspace {
    pub(super) fn budget_mode(&self) -> Result<BudgetKind> {
        let weekly: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM budget_mode WHERE slot=1 AND weekly)",
            [],
            |row| row.get(0),
        )?;
        Ok(if weekly {
            BudgetKind::WeeklyUsd
        } else {
            BudgetKind::DailyUsd
        })
    }

    pub(super) fn weekly_reset(&self) -> Result<Option<Reset>> {
        Ok(self
            .db
            .query_row(
                "SELECT weekday,hour,timezone FROM weekly_reset WHERE slot=1",
                [],
                |row| {
                    Ok(Reset {
                        weekday: row.get(0)?,
                        hour: row.get(1)?,
                        timezone: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    pub(super) fn weekly_budget(&mut self, who: &Identity, edit: Edit) -> Result<Value> {
        if !who.role.can_manage() {
            return Err(forbidden());
        }
        if let Edit::MemberLimit { member, .. } = &edit
            && !who.may_set_budget(member, self.role_for(member)?)
        {
            return Err(forbidden());
        }
        if matches!(
            edit,
            Edit::Reset { .. }
                | Edit::Mode {
                    mode: BudgetKind::WeeklyUsd
                }
        ) && self.weekly_reset()?.is_none()
        {
            return Err(Failure(409, "workspace_weekly_reset_not_configured"));
        }
        let previous_mode = self.budget_mode()?;
        let tx = self.db.transaction()?;
        match edit {
            Edit::Initialize { timezone } => {
                let timezone = TimeZone::get(&timezone).map_err(|_| invalid())?;
                let name = timezone.iana_name().ok_or_else(invalid)?;
                tx.execute(
                    "INSERT OR IGNORE INTO weekly_reset VALUES (1,0,16,?)",
                    [name],
                )?;
            }
            Edit::Reset { weekday, hour } => {
                if !(0..=6).contains(&weekday) || !(0..=23).contains(&hour) {
                    return Err(invalid());
                }
                if tx.execute(
                    "UPDATE weekly_reset SET weekday=?1,hour=?2 WHERE slot=1 AND (weekday<>?1 OR hour<>?2)",
                    params![weekday, hour],
                )? != 0 {
                    audit::record(&tx, who, None, "weekly_reset", json!({"weekday":weekday,"hour":hour}))?;
                }
            }
            Edit::MemberLimit { member, usd_nanos } => {
                if usd_nanos.is_some_and(|value| !(0..=usage::MAX_MONEY).contains(&value)) {
                    return Err(invalid());
                }
                let changed = if let Some(usd_nanos) = usd_nanos {
                    tx.execute(
                        "INSERT INTO weekly_usd_limits VALUES (?,?) ON CONFLICT(member) DO UPDATE
                         SET usd_nanos=excluded.usd_nanos WHERE usd_nanos<>excluded.usd_nanos",
                        params![member, usd_nanos],
                    )?
                } else {
                    tx.execute("DELETE FROM weekly_usd_limits WHERE member=?", [&member])?
                };
                if changed != 0 {
                    audit::record(
                        &tx,
                        who,
                        Some(&member),
                        "weekly_usd_limit",
                        json!({"usd_nanos":usd_nanos}),
                    )?;
                }
            }
            Edit::Mode { mode } => {
                if previous_mode != mode {
                    tx.execute(
                        "INSERT INTO budget_mode VALUES (1,?) ON CONFLICT(slot) DO UPDATE SET weekly=excluded.weekly",
                        [mode == BudgetKind::WeeklyUsd],
                    )?;
                    audit::record(&tx, who, None, "budget_mode", json!({"mode":mode}))?;
                }
            }
        }
        tx.commit()?;
        Ok(json!({"ok":true,"weekly_reset":self.weekly_reset()?,"budget_mode":self.budget_mode()?}))
    }

    pub(super) fn weekly_usd_status(
        &self,
        member: &str,
        start: i64,
        now: i64,
    ) -> Result<BudgetStatus> {
        let (limit, used) = self.db.query_row(
            "SELECT w.usd_nanos, CASE WHEN COUNT(u.id)=0 THEN 0 ELSE
                 SUM(COALESCE(u.actual_nanos,u.estimate_nanos)) END
             FROM members m LEFT JOIN weekly_usd_limits w ON w.member=m.id
             LEFT JOIN usage u ON u.member=m.id AND u.finished_at>=? AND u.finished_at<=?
             WHERE m.id=? GROUP BY m.id",
            params![start, now, member],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(BudgetStatus {
            day: start,
            used,
            limit,
            kind: BudgetKind::WeeklyUsd,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_uses_local_time_at_each_boundary_through_daylight_saving_changes() {
        let reset = Reset {
            weekday: 0,
            hour: 16,
            timezone: "America/Los_Angeles".into(),
        };
        let stamp = |text: &str| text.parse::<Timestamp>().unwrap().as_second();
        let before = stamp("2026-03-02T00:00:00Z");
        let change = stamp("2026-03-08T23:00:00Z");
        let after = stamp("2026-03-15T23:00:00Z");
        assert_eq!(reset.window(change - 1).unwrap(), (before, change));
        assert_eq!(reset.window(change).unwrap(), (change, after));
        assert_eq!(change - before, 167 * 3600);

        let fall_before = stamp("2026-10-25T23:00:00Z");
        let fall_reset = stamp("2026-11-02T00:00:00Z");
        assert_eq!(
            reset.window(fall_reset - 1).unwrap(),
            (fall_before, fall_reset)
        );
        assert_eq!(fall_reset - fall_before, 169 * 3600);

        let changed = Reset {
            weekday: 3,
            hour: 9,
            ..reset
        };
        let next = stamp("2026-03-11T16:00:00Z");
        assert_eq!(changed.window(change).unwrap().1, next);
    }
}
