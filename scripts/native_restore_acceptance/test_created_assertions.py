from __future__ import annotations

import copy
import unittest
from typing import Any

from .common import sha256
from .constants import CALL_AFTER_COMPACTION, CALL_BEFORE_COMPACTION, TOOL_ARGUMENTS
from .created_assertions import created_assertions


OUTPUT_HASHES = {
    CALL_BEFORE_COMPACTION: "a" * 64,
    CALL_AFTER_COMPACTION: "b" * 64,
}


def request(
    prompt: str,
    call_id: str,
    *,
    phase: str,
    source: str,
) -> dict[str, Any]:
    value = {
        "prompt": prompt,
        "phase": phase,
        "source": source,
        "http_status": 200,
        "function_calls": [
            {
                "call_id": call_id,
                "name": "mcp__fixture__fixture_tool",
                "arguments_sha256": sha256(TOOL_ARGUMENTS.encode()),
            }
        ],
        "function_outputs": [
            {"call_id": call_id, "name": None, "output_sha256": OUTPUT_HASHES[call_id]}
        ],
    }
    if source == "emp_upstream":
        value["emp_request_id_present"] = True
    if phase == "external_compaction":
        value["compaction_prompt_present"] = True
    if source == "codex_direct":
        value.update(
            provider_error_code=None,
            has_emp1=False,
            summary_seen=True,
        )
    return value


def fixture() -> tuple[dict[str, Any], dict[str, str], list[dict[str, Any]]]:
    ids = {"parent": "thread-parent"}
    requests = [
        request(
            "tool_before_compaction",
            CALL_BEFORE_COMPACTION,
            phase="pre_restore_created_flow",
            source="emp_upstream",
        ),
        request(
            "tool_after_compaction",
            CALL_AFTER_COMPACTION,
            phase="pre_restore_created_flow",
            source="emp_upstream",
        ),
        request(
            "external_compaction",
            CALL_BEFORE_COMPACTION,
            phase="external_compaction",
            source="emp_upstream",
        ),
        request("parent", CALL_AFTER_COMPACTION, phase="post_restore_probe", source="codex_direct"),
    ]
    report = {
        "resume_thread_ids": ids,
        "resume_turn_outcomes": {"parent": "completed"},
        "restore": {"peak_rss_bytes": 1},
        "process_limits": {"address_space_bytes": 1},
    }
    return report, ids, requests


class CreatedAssertionsPromptKeyRegression(unittest.TestCase):
    def test_noop_restore_requires_verified_shutdown_restoration(self) -> None:
        report, ids, requests = fixture()
        report["restore"].update(exit_code=0, action="noop", state="restored",
            native_base_url_restored=True, native_config_matches_saved=True,
            provider_config_preserved=True)
        self.assertFalse(created_assertions(report, ids, requests, [])["restore_succeeded"])
        report["shutdown_restored_config"] = True
        self.assertTrue(created_assertions(report, ids, requests, [])["restore_succeeded"])

    def test_caller_selects_output_oracle_and_rejects_duplicate_or_changed_output(self) -> None:
        report, ids, requests = fixture()
        valid = created_assertions(report, ids, requests, [])
        self.assertTrue(valid["first_real_mcp_tool_outputs_observed"])
        self.assertTrue(valid["compaction_input_retained_first_tool_pair"])
        self.assertTrue(valid["parent_post_compaction_tool_pair_preserved"])

        changed_requests = copy.deepcopy(requests)
        parent = next(item for item in changed_requests if item["prompt"] == "parent")
        parent["function_outputs"][0]["output_sha256"] = "c" * 64
        changed = created_assertions(dict(report), ids, changed_requests, [])
        self.assertFalse(changed["parent_post_compaction_tool_pair_preserved"])

        duplicate_requests = copy.deepcopy(requests)
        parent = next(item for item in duplicate_requests if item["prompt"] == "parent")
        parent["function_outputs"].append(copy.deepcopy(parent["function_outputs"][0]))
        duplicate = created_assertions(dict(report), ids, duplicate_requests, [])
        self.assertFalse(duplicate["parent_post_compaction_tool_pair_preserved"])


if __name__ == "__main__":
    unittest.main()
