use super::{Session, policy};
use crate::app::ServerState;
use emp_modules::team::{BudgetNotice, BudgetOccurrence, BudgetStatus, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Mutex};

#[derive(Deserialize, Serialize)]
struct Notice {
    workspace: String,
    member: String,
    day: i64,
    at: i64,
    reached: bool,
    used: i64,
    limit: i64,
}

pub(super) struct Notices {
    path: PathBuf,
    history: Mutex<Vec<Notice>>,
    #[cfg(windows)]
    pending_desktop: Mutex<Option<(String, String)>>,
}

impl Notices {
    pub(super) fn open(path: PathBuf) -> Self {
        let history = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            history: Mutex::new(history),
            #[cfg(windows)]
            pending_desktop: Mutex::new(None),
        }
    }

    pub(super) fn history(&self, session: &Session) -> Value {
        let Ok(history) = self.history.lock() else {
            return json!([]);
        };
        json!(history.iter().filter(|entry| entry.belongs_to(session)).map(|entry| json!({
            "id": format!("team-budget-{}-{}-{}-{}", entry.workspace,entry.member,entry.day,entry.reached),
            "at":entry.at,"reached":entry.reached,"used":entry.used,"limit":entry.limit
        })).collect::<Vec<_>>())
    }

    pub(super) fn observe(
        &self,
        state: &ServerState,
        english: bool,
        session: &Session,
        status: BudgetStatus,
    ) {
        let Ok(mut history) = self.history.lock() else {
            return;
        };
        let previous = history
            .iter()
            .filter(|entry| entry.belongs_to(session))
            .map(|entry| BudgetOccurrence {
                day: entry.day,
                reached: entry.reached,
            })
            .collect();
        let Ok(Some(BudgetNotice {
            day,
            reached,
            used,
            limit,
            title,
            message,
        })) = policy::<Option<BudgetNotice>>(
            state,
            Request::BudgetNotice {
                status,
                previous,
                english,
            },
        )
        else {
            return;
        };
        history.push(Notice {
            workspace: session.workspace().to_owned(),
            member: session.member().to_owned(),
            day,
            at: crate::util::system_now() as i64,
            reached,
            used,
            limit,
        });
        let expired = history.len().saturating_sub(50);
        let raw = serde_json::to_vec(&history[expired..]).expect("budget notices serialize");
        if emp_state::filesystem::atomic_write_private_state(&self.path, &raw).is_err() {
            history.pop();
            return;
        }
        history.drain(..expired);
        drop(history);
        #[cfg(not(windows))]
        crate::cli::desktop::notify_budget(&title, message);
        #[cfg(windows)]
        if let Ok(mut pending) = self.pending_desktop.lock() {
            *pending = Some((title, message));
        }
    }

    #[cfg(windows)]
    pub(super) fn take_desktop(&self) -> Option<(String, String)> {
        self.pending_desktop.lock().ok()?.take()
    }
}

impl Notice {
    fn belongs_to(&self, session: &Session) -> bool {
        self.workspace == session.workspace() && self.member == session.member()
    }
}
