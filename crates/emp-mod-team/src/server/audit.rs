use super::*;

const PAGE_SIZE: usize = 50;

#[derive(Deserialize)]
pub(super) struct Page {
    before: Option<i64>,
}

pub(super) fn record(
    db: &Connection,
    who: &Identity,
    target: Option<&str>,
    kind: &str,
    details: Value,
) -> Result<()> {
    db.execute(
        "INSERT INTO workspace_audit(at,actor,target,kind,details) VALUES (?,?,?,?,?)",
        params![now(), who.member, target, kind, details.to_string()],
    )?;
    Ok(())
}

pub(super) fn detach_target(db: &Connection, id: &str, name: &str, nickname: &str) -> Result<()> {
    db.execute(
        "UPDATE workspace_audit SET target=NULL,
             details=json_set(details,'$.former_target',json_object('name',?2,'nickname',?3))
         WHERE target=?1",
        params![id, name, nickname],
    )?;
    Ok(())
}

impl Workspace {
    pub(super) fn audit(&self, who: &Identity, page: Page) -> Result<Value> {
        if !who.role.can_manage() {
            return Err(forbidden());
        }
        let mut query = self.db.prepare(
            "SELECT a.id,a.at,COALESCE(ea.nickname,ma.name),
                    COALESCE(et.nickname,mt.name,json_extract(a.details,'$.former_target.nickname')),
                    a.kind,json_remove(a.details,'$.former_target'),
                    ma.name,COALESCE(mt.name,json_extract(a.details,'$.former_target.name'))
             FROM workspace_audit a
             JOIN members ma ON ma.id=a.actor LEFT JOIN members mt ON mt.id=a.target
             LEFT JOIN enrollment ea ON ea.member=ma.id LEFT JOIN enrollment et ON et.member=mt.id
             WHERE a.id<? ORDER BY a.id DESC LIMIT ?",
        )?;
        let mut rows = query.query(params![
            page.before.unwrap_or(i64::MAX),
            PAGE_SIZE as i64 + 1
        ])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            let details: String = row.get(5)?;
            entries.push(json!({"id":row.get::<_,i64>(0)?,"at":row.get::<_,i64>(1)?,
                "actor":row.get::<_,String>(2)?,"target":row.get::<_,Option<String>>(3)?,
                "actor_login":row.get::<_,String>(6)?,"target_login":row.get::<_,Option<String>>(7)?,
                "kind":row.get::<_,String>(4)?,"details":parse::<Value>(details.as_bytes())?}));
        }
        let next = if entries.len() > PAGE_SIZE {
            entries.pop();
            entries.last().and_then(|entry| entry["id"].as_i64())
        } else {
            None
        };
        Ok(json!({"entries":entries,"before":next}))
    }
}
