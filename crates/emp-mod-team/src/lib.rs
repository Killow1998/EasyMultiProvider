use emp_modules::{
    Command,
    team::{
        BudgetNotice, BudgetOccurrence, BudgetStatus, DiscoveredWorkspace, Endpoint, IdentityPlan,
        Method, Operation, ReportPlan, Request, SyncPlan, UsageReport,
    },
    workspace::Role,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use url::Url;

pub fn handle(command: Command) -> Result<Value, &'static str> {
    let request = match command {
        Command::Team { request } => request,
        Command::TeamEndpoint { base, operation } => {
            let origin = workspace_origin(&base)?;
            let operation: Operation = serde_json::from_value(Value::String(operation))
                .map_err(|_| "invalid_module_command")?;
            if matches!(
                operation,
                Operation::IssueInvite | Operation::ListMembers | Operation::UsageFiltered
            ) {
                return Err("invalid_module_command");
            }
            return json(endpoint(origin, operation).url);
        }
        _ => return Err("invalid_module_command"),
    };
    match request {
        Request::Endpoint { base, operation } => {
            json(endpoint(workspace_origin(&base)?, operation))
        }
        Request::Identity {
            current,
            received,
            role,
            sync,
        } => {
            if let Some(current) = current {
                if current != received {
                    return Err("workspace_login_required");
                }
            } else if [&received.workspace, &received.member]
                .into_iter()
                .any(|id| !id.as_deref().is_some_and(identifier))
            {
                return Err("invalid_workspace_identity");
            }
            json(IdentityPlan {
                migration_allowed: role.is_some_and(Role::can_manage),
                sync_allowed: sync,
            })
        }
        Request::SyncShares { shares } => {
            let mut plan = SyncPlan {
                selected: Vec::new(),
                skipped_native: 0,
            };
            for (index, share) in shares.into_iter().enumerate() {
                if share.duplicates_native {
                    plan.skipped_native += 1;
                    continue;
                }
                if !identifier(&share.id) || share.version <= 0 || !share.valid_payload {
                    return Err("invalid_workspace_share");
                }
                plan.selected.push(index);
            }
            json(plan)
        }
        Request::Report { base, events } => json(ReportPlan {
            endpoint: endpoint(workspace_origin(&base)?, Operation::Report),
            report: UsageReport { events },
        }),
        Request::BudgetNotice {
            status,
            previous,
            english,
        } => json(budget_notice(status, &previous, english)),
        Request::Discovery { candidates } => {
            let mut found = BTreeMap::new();
            for candidate in candidates {
                if let Ok(origin) = workspace_origin(&candidate.base)
                    && origin.scheme() == "https"
                {
                    found.insert(origin.to_string(), candidate.name);
                }
            }
            json(
                found
                    .into_iter()
                    .map(|(base, name)| DiscoveredWorkspace { base, name })
                    .collect::<Vec<_>>(),
            )
        }
    }
}

fn json(value: impl Serialize) -> Result<Value, &'static str> {
    serde_json::to_value(value).map_err(|_| "invalid_module_reply")
}

fn endpoint(mut origin: Url, operation: Operation) -> Endpoint {
    use Method::{Get, Post};
    use Operation::*;

    let announcement_origin = (matches!(operation, IssueInvite) && origin.scheme() == "https")
        .then(|| origin.to_string());
    let (path, method) = match operation {
        Login => ("login", Post),
        Join => ("join", Post),
        JoinCode => ("join-code", Get),
        IssueInvite => ("join-code", Post),
        Status => ("status", Get),
        Members => ("members", Post),
        ListMembers => ("members", Get),
        MemberAccess => ("member-access", Post),
        MemberTiers => ("member-tiers", Post),
        Budget => ("budget", Post),
        BudgetStatus => ("budget-status", Get),
        Profiles => ("profiles", Get),
        Profile => ("profile", Post),
        ProfileVisibility => ("profile-visibility", Post),
        Heartbeat => ("heartbeat", Get),
        Subscriptions => ("subscriptions", Get),
        SubscriptionReminder => ("subscription-reminder", Post),
        Shares => ("shares", Get),
        Publish => ("publish", Post),
        PublishVersion => ("publish-version", Get),
        SharedServices => ("shared-services", Get),
        ReviewService => ("review-service", Post),
        ResetAuthority => ("reset-authority", Post),
        Usage => ("usage", Get),
        UsageFiltered => ("usage", Post),
        Report => ("report", Post),
        Overview => ("overview", Get),
        Audit => ("audit", Post),
        Handover => ("handover", Post),
        Backup => ("backup", Get),
        Logout => ("logout", Post),
    };
    origin.set_path(&format!("/workspace/v1/{path}"));
    Endpoint {
        url: origin.into(),
        method,
        announcement_origin,
    }
}

fn workspace_origin(base: &str) -> Result<Url, &'static str> {
    let address = if base.contains("://") {
        base.to_owned()
    } else {
        format!("https://{base}")
    };
    let url = Url::parse(&address).map_err(|_| "invalid_team_url")?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("invalid_team_url");
    }
    Ok(url)
}

fn identifier(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn budget_notice(
    status: BudgetStatus,
    previous: &[BudgetOccurrence],
    english: bool,
) -> Option<BudgetNotice> {
    let BudgetStatus { day, used, limit } = status;
    let (used, limit) = (used?, limit?);
    let reached = used >= limit;
    if (!reached && i128::from(used) * 10 < i128::from(limit) * 9)
        || previous
            .iter()
            .any(|entry| entry.day == day && (entry.reached || !reached))
    {
        return None;
    }
    let remaining = limit.saturating_sub(used).max(0) as f64 / 1e9;
    let title = if english {
        "EMP · Team budget"
    } else {
        "EMP · 团队预算"
    };
    let message = match (english, reached) {
        (true, true) => "Daily team budget reached.".to_owned(),
        (true, false) => {
            format!("Approaching the daily team budget. US${remaining:.2} remaining.")
        }
        (false, true) => "已达到团队每日预算。".to_owned(),
        (false, false) => format!("接近团队每日预算，剩余 US${remaining:.2}。"),
    };
    Some(BudgetNotice {
        day,
        reached,
        used,
        limit,
        title: title.into(),
        message,
    })
}
