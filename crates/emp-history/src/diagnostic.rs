//! Fixed, content-free history failure vocabulary shared by receipts and clients.
use crate::HistoryError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryDiagnostic {
    pub category: &'static str,
    pub reason: &'static str,
}

impl HistoryError {
    /// Only known codes may enter diagnostics. Sanitizing arbitrary text is not
    /// sufficient: even an alphanumeric string could contain private content.
    pub fn diagnostic(&self) -> HistoryDiagnostic {
        macro_rules! classify {
            ($($category:literal => [$($reason:literal),+ $(,)?]),+ $(,)?) => {
                match self.reason() {
                    $($($reason => HistoryDiagnostic { category: $category, reason: $reason },)+)+
                    _ => HistoryDiagnostic { category: "unknown", reason: "history_reason_unknown" },
                }
            };
        }
        classify! {
            "anchor" => [
                "invalid_turn_metadata", "conflicting_thread_identity", "conflicting_window_identity",
                "invalid_thread_id", "invalid_threadid", "invalid_turn_id", "invalid_turnid",
                "invalid_window_id", "invalid_windowid", "invalid_forked_from_thread_id",
                "thread_identity_missing", "turn_identity_missing", "thread_mismatch",
                "thread_identity_conflict", "turn_not_found",
            ],
            "storage" => [
                "state_database_missing", "database_missing", "database_unavailable",
                "threads_schema_unsupported", "thread_missing", "rollout_path_missing",
                "source_unavailable", "source_missing", "source_too_large", "source_changed",
                "history_unavailable", "rollout_outside_session_root", "rollout_outside_codex_home",
            ],
            "replay" => [
                "invalid_history_mode", "history_mode_mismatch", "history_record_too_large",
                "invalid_compressed_rollout", "invalid_json", "invalid_replay_range",
                "ordinal_missing", "ordinal_not_monotonic", "session_meta_missing",
            ],
            "lineage" => [
                "invalid_history_base", "lineage_cycle", "lineage_depth_exceeded",
                "lineage_prefix_truncated", "lineage_source_ambiguous", "fork_parent_invalid",
            ],
            "checkpoint" => [
                "multiple_compaction_boundaries", "compaction_boundary_missing",
                "compaction_summary_missing", "compaction_identity_missing", "compaction_identity_ambiguous",
                "invalid_replacement_history", "invalid_replacement_history_metadata",
                "opaque_replacement_unavailable", "portable_checkpoint_invalid",
                "portable_checkpoint_encoding_invalid", "portable_checkpoint_utf8_invalid",
                "portable_checkpoint_empty",
            ],
            "projection" => [
                "invalid_history_projection", "history_projection_incomplete", "tool_call_identity_missing",
            ],
            "destination_compaction" => [
                "history_compaction_failed", "summary_call_failed", "compaction_unit_too_large",
                "compaction_budget_invalid", "compaction_result_over_budget", "summary_reduce_unit_too_large",
                "summary_reduce_not_converging", "summary_result_missing", "summary_request_id_failed",
                "summary_output_missing", "destination_protocol_missing", "destination_protocol_invalid",
            ],
        }
    }
}

impl HistoryDiagnostic {
    /// Keep the reason visible even in clients that display only `message`.
    pub fn message(self) -> String {
        let guidance = match self.reason {
            "thread_identity_missing" | "turn_identity_missing" => {
                "The request is missing a task or turn identifier. Continue in the original task or send its full history."
            }
            "thread_missing" => {
                "This task was not found in the local Codex history. Check the Codex data directory or continue in the original task."
            }
            "state_database_missing" | "database_missing" => {
                "The local Codex history database was not found. Use the same Codex data directory as your client."
            }
            "compaction_identity_missing" => {
                "The requested checkpoint was not found in local history. Continue from the original task with its matching history."
            }
            "compaction_identity_ambiguous" => {
                "More than one local checkpoint matches this request. EMP cannot safely choose a history boundary."
            }
            "compaction_summary_missing" => {
                "The visible history has no recoverable compaction boundary or summary. Continue in the original task or provide its visible context in a new task."
            }
            "tool_call_identity_missing" => {
                "A historical tool call or result has no call ID, so it cannot be paired safely. Continue in the original task or provide a visible summary in a new task."
            }
            "summary_output_missing" | "summary_result_missing" => {
                "The compaction request produced no usable summary. Check the summary response outcome before retrying."
            }
            _ => match self.category {
                "anchor" => {
                    "The task or turn identifiers do not match the available history. Check the request metadata and original task."
                }
                "storage" => {
                    "The local history source could not be read safely. Check the Codex data directory, file availability and access."
                }
                "replay" => {
                    "Local history records could not be replayed consistently. Check the history format and record integrity."
                }
                "lineage" => {
                    "The fork's parent history could not be resolved safely. Continue in the original task or provide its visible context in a new task."
                }
                "checkpoint" => {
                    "The checkpoint could not be decoded or restored safely. Continue in the original task or provide its visible context in a new task."
                }
                "projection" => {
                    "The visible history could not be converted into valid model input. Check the checkpoint and tool-call records."
                }
                "destination_compaction" => {
                    "History could not be compacted for the selected model. Check the summary outcome and destination context budget."
                }
                _ => {
                    "The history failure reason is unrecognized. Inspect the request phase and report this error code."
                }
            },
        };
        format!(
            "History reconstruction failed [category={}, reason={}]. {guidance}",
            self.category, self.reason
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_reasons_never_echo_arbitrary_text() {
        let diagnostic = HistoryError::new("private_history_content").diagnostic();
        assert_eq!(diagnostic.category, "unknown");
        assert_eq!(diagnostic.reason, "history_reason_unknown");
        assert!(!diagnostic.message().contains("private"));
    }
}
