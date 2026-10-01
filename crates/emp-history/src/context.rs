//! Conservative context assessment and request-local compaction.
//! Estimation, limit calibration and compaction each own their internal policy.

mod budget;
mod calibration;
mod compaction;
mod estimate;

pub use budget::{ContextAssessment, assess};
pub use calibration::{status, update as update_calibration};
pub use compaction::compact_with;
pub use estimate::estimate_json_tokens;

pub const SAFETY_RESERVE_TOKENS: u64 = 256;
