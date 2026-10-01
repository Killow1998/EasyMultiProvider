//! Reader-level history contracts.
//!
//! These tests own the reader semantics: the reverse-base fast path and the
//! full scan must stay observationally identical over the same durable
//! rollout, and failures must use stable reasons (`ordinal_missing`,
//! `ordinal_not_monotonic`, `compaction_identity_ambiguous`,
//! `lineage_cycle`, `lineage_prefix_truncated`).

use emp_codex::history::CodexHomeHistoryReader;
use emp_history::{HistoryAnchor, HistoryReader, HistorySnapshot};
use rusqlite::params;
use serde_json::{Value, json};
use std::io::Write;
use tempfile::tempdir;

const THREAD: &str = "01a00000-0000-7000-8000-000000000001";
const TURN: &str = "01a00000-0000-7000-8000-000000000004";
const PARENT: &str = "01a00000-0000-7000-8000-000000000002";
const MODEL: &str = "gpt-native";

#[path = "history_contract/bounds.rs"]
mod bounds;
#[path = "history_contract/fast_replay.rs"]
mod fast_replay;
#[path = "history_contract/fork.rs"]
mod fork;
#[path = "history_contract/lineage.rs"]
mod lineage;
#[path = "history_contract/support.rs"]
mod support;
#[path = "history_contract/validation.rs"]
mod validation;
