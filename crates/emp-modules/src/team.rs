use crate::workspace::{Role, UsageEvent};
use serde::{Deserialize, Serialize};

pub const MAX_BACKUP_WIRE_BYTES: usize = 96 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    Endpoint {
        base: String,
        operation: Operation,
    },
    Identity {
        current: Option<IdentityKey>,
        received: IdentityKey,
        role: Option<Role>,
        sync: bool,
    },
    SyncShares {
        shares: Vec<ShareMetadata>,
    },
    Report {
        base: String,
        events: Vec<UsageEvent>,
    },
    BudgetNotice {
        status: BudgetStatus,
        previous: Vec<BudgetOccurrence>,
        english: bool,
    },
    Discovery {
        candidates: Vec<DiscoveredWorkspace>,
    },
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Login,
    Join,
    JoinCode,
    IssueInvite,
    Status,
    Members,
    ListMembers,
    MemberAccess,
    RejectMember,
    RemoveMember,
    MemberTiers,
    Budget,
    WeeklyBudget,
    BudgetStatus,
    Profiles,
    Profile,
    ProfileVisibility,
    Heartbeat,
    Subscriptions,
    SubscriptionReminder,
    Shares,
    Publish,
    PublishVersion,
    SharedServices,
    ReviewService,
    RemoveService,
    ResetAuthority,
    Usage,
    UsageFiltered,
    Report,
    Overview,
    Audit,
    Handover,
    Backup,
    Logout,
}

#[derive(Deserialize, Serialize)]
pub struct Endpoint {
    pub url: String,
    pub method: Method,
    pub announcement_origin: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Get,
    Post,
}

#[derive(PartialEq, Eq, Deserialize, Serialize)]
pub struct IdentityKey {
    pub workspace: Option<String>,
    pub member: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct IdentityPlan {
    pub migration_allowed: bool,
    pub sync_allowed: bool,
}

#[derive(Deserialize, Serialize)]
pub struct ShareMetadata {
    pub id: String,
    pub version: i64,
    pub valid_payload: bool,
    pub duplicates_native: bool,
}

#[derive(Deserialize, Serialize)]
pub struct SyncPlan {
    pub selected: Vec<usize>,
    pub skipped_native: usize,
}

#[derive(Deserialize, Serialize)]
pub struct UsageReport {
    pub events: Vec<UsageEvent>,
}

#[derive(Deserialize, Serialize)]
pub struct ReportPlan {
    pub endpoint: Endpoint,
    pub report: UsageReport,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    #[default]
    DailyUsd,
    WeeklyUsd,
}

#[derive(Deserialize, Serialize)]
pub struct BudgetStatus {
    pub day: i64,
    pub used: Option<i64>,
    pub limit: Option<i64>,
    #[serde(default)]
    pub kind: BudgetKind,
}

#[derive(Deserialize, Serialize)]
pub struct BudgetStatuses {
    #[serde(flatten)]
    pub daily: BudgetStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<BudgetStatus>,
}

#[derive(Deserialize, Serialize)]
pub struct BudgetOccurrence {
    pub day: i64,
    pub reached: bool,
}

#[derive(Deserialize, Serialize)]
pub struct BudgetNotice {
    pub day: i64,
    pub reached: bool,
    pub used: i64,
    pub limit: i64,
    pub title: String,
    pub message: String,
}

#[derive(Deserialize, Serialize)]
pub struct DiscoveredWorkspace {
    pub base: String,
    pub name: String,
}
