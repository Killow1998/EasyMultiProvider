"""Assertions and pure helpers for the live EMP created-history acceptance."""

from __future__ import annotations

from typing import Any

from .common import sha256
from .constants import (
    CALL_AFTER_COMPACTION,
    CALL_BEFORE_COMPACTION,
    CHILD_SUFFIX,
    CREATED_FLOW_PROMPTS,
    EARLY_CHILD_SUFFIX,
    GRANDCHILD_SUFFIX,
    OPAQUE,
    OPAQUE_ID,
    PARENT_SUFFIX,
    PROBE_PROMPTS,
    SUMMARY,
    TOOL_ARGUMENTS,
)

CREATED_LOCAL_MARKERS = {
    "parent": {PARENT_SUFFIX},
    "early_child": {EARLY_CHILD_SUFFIX, CREATED_FLOW_PROMPTS["early_child"]},
    "child": {PARENT_SUFFIX, CHILD_SUFFIX, CREATED_FLOW_PROMPTS["late_child"]},
    "grandchild": {
        PARENT_SUFFIX,
        CHILD_SUFFIX,
        GRANDCHILD_SUFFIX,
        CREATED_FLOW_PROMPTS["late_child"],
        CREATED_FLOW_PROMPTS["grandchild"],
    },
}
CREATED_LOCAL_MARKER_PROJECTION = {
    "parent": [PARENT_SUFFIX],
    "early_child": [EARLY_CHILD_SUFFIX, CREATED_FLOW_PROMPTS["early_child"]],
    "child": [PARENT_SUFFIX, CHILD_SUFFIX, CREATED_FLOW_PROMPTS["late_child"]],
    "grandchild": [
        PARENT_SUFFIX,
        CHILD_SUFFIX,
        CREATED_FLOW_PROMPTS["late_child"],
        GRANDCHILD_SUFFIX,
        CREATED_FLOW_PROMPTS["grandchild"],
    ],
}


def tool_pair_present(
    request: dict[str, Any],
    call_id: str,
    output_sha256: str | None,
    expected_name: str = "mcp__fixture__fixture_tool",
) -> tuple[bool, bool]:
    raw_calls = request.get("function_calls")
    raw_outputs = request.get("function_outputs")
    if (
        not isinstance(raw_calls, list)
        or not isinstance(raw_outputs, list)
        or any(not isinstance(item, dict) for item in raw_calls + raw_outputs)
    ):
        return False, False
    calls = [item for item in raw_calls if item.get("call_id") == call_id]
    outputs = [item for item in raw_outputs if item.get("call_id") == call_id]
    call_ok = (
        len(calls) == 1
        and calls[0].get("name") == expected_name
        and calls[0].get("arguments_sha256") == sha256(TOOL_ARGUMENTS.encode())
    )
    output_ok = len(outputs) == 1 and isinstance(output_sha256, str) and (
        outputs[0].get("output_sha256") == output_sha256
        and (
            outputs[0].get("name") is None
            or outputs[0].get("name") == "fixture_tool"
        )
    )
    return call_ok, output_ok


def observed_tool_name(requests: list[dict[str, Any]], call_id: str) -> str | None:
    names = {
        call.get("name")
        for request in requests
        for call in request.get("function_calls", [])
        if isinstance(call, dict) and call.get("call_id") == call_id and isinstance(call.get("name"), str)
    }
    return next(iter(names)) if len(names) == 1 else None


def first_observed_tool_output_sha256(
    requests: list[dict[str, Any]], *, prompt_key: str, call_id: str
) -> str | None:
    for request in requests:
        if (
            request.get("phase") != "pre_restore_created_flow"
            or request.get("prompt") != prompt_key
            or request.get("source") != "emp_upstream"
            or request.get("emp_request_id_present") is not True
            or request.get("http_status") != 200
        ):
            continue
        outputs = [
            item
            for item in request.get("function_outputs", [])
            if isinstance(item, dict) and item.get("call_id") == call_id
        ]
        if not outputs:
            continue
        calls = [
            item
            for item in request.get("function_calls", [])
            if isinstance(item, dict) and item.get("call_id") == call_id
        ]
        if len(outputs) != 1 or len(calls) != 1:
            return None
        if (
            not isinstance(calls[0].get("name"), str)
            or calls[0].get("arguments_sha256") != sha256(TOOL_ARGUMENTS.encode())
        ):
            return None
        output = outputs[0]
        digest = output.get("output_sha256")
        output_name = output.get("name")
        if (
            not (output_name is None or output_name == "fixture_tool")
            or not isinstance(digest, str)
            or len(digest) != 64
            or any(character not in "0123456789abcdef" for character in digest)
        ):
            return None
        return digest
    return None


def tool_call_id_absent(request: dict[str, Any], call_id: str) -> bool:
    for field in ("function_calls", "function_outputs"):
        items = request.get(field)
        if not isinstance(items, list):
            return False
        if any(isinstance(item, dict) and item.get("call_id") == call_id for item in items):
            return False
    return True

def index_requests(requests: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    indexed: dict[str, list[dict[str, Any]]] = {}
    for request in requests:
        indexed.setdefault(str(request.get("prompt")), []).append(request)
    return indexed


def created_assertions(
    report: dict[str, Any],
    ids: dict[str, str],
    requests: list[dict[str, Any]],
    mcp_calls: list[dict[str, Any]],
) -> dict[str, bool]:
    by_prompt = index_requests(requests)
    assertions: dict[str, bool] = {}
    tool_output_oracles = {
        CALL_BEFORE_COMPACTION: first_observed_tool_output_sha256(
            requests,
            prompt_key="tool_before_compaction",
            call_id=CALL_BEFORE_COMPACTION,
        ),
        CALL_AFTER_COMPACTION: first_observed_tool_output_sha256(
            requests,
            prompt_key="tool_after_compaction",
            call_id=CALL_AFTER_COMPACTION,
        ),
    }
    report["first_observed_tool_output_sha256"] = tool_output_oracles
    assertions["first_real_mcp_tool_outputs_observed"] = all(
        isinstance(digest, str) for digest in tool_output_oracles.values()
    )
    creation = report.get("build_turn_outcomes", {})
    expected_created = {
        "tool_before_compaction": "completed",
        "early_child": "completed",
        "compaction_threshold": "completed",
        "compaction_followup": "completed",
        "tool_after_compaction": "completed",
        "build_parent": "completed",
        "build_child": "completed",
        "build_grandchild": "completed",
    }
    assertions["all_created_codex_turns_completed"] = all(
        creation.get(name) == state for name, state in expected_created.items()
    )

    upstream_created = [
        item for item in requests if item.get("phase") == "pre_restore_created_flow"
    ]
    assertions["codex_traffic_reached_live_emp"] = bool(upstream_created) and all(
        item.get("source") == "emp_upstream" and item.get("emp_request_id_present") is True
        for item in upstream_created
    )
    compact_requests = by_prompt.get("external_compaction", [])
    first_tool_name = observed_tool_name(
        by_prompt.get("tool_before_compaction", []), CALL_BEFORE_COMPACTION
    )
    assertions["external_model_compaction_completed_through_emp"] = (
        len(compact_requests) == 1
        and compact_requests[0].get("phase") == "external_compaction"
        and compact_requests[0].get("source") == "emp_upstream"
        and compact_requests[0].get("http_status") == 200
        and compact_requests[0].get("compaction_prompt_present") is True
    )
    assertions["compaction_input_retained_first_tool_pair"] = (
        len(compact_requests) == 1
        and first_tool_name is not None
        and all(
            tool_pair_present(
                compact_requests[0],
                CALL_BEFORE_COMPACTION,
                tool_output_oracles[CALL_BEFORE_COMPACTION],
                first_tool_name,
            )
        )
    )
    assertions["real_mcp_tool_calls_completed"] = (
        len(mcp_calls) == 2
        and all(item.get("name") == "fixture_tool" for item in mcp_calls)
        and all(item.get("arguments") == {"fixture": True} for item in mcp_calls)
    )

    pre_restore_emp1 = set(report.get("pre_restore_emp1_threads", []))
    pre_restore_emp1_thread_ids = {
        ids[key]
        for key in pre_restore_emp1
        if key in ids
    }
    all_emp1_roles_mapped = len(pre_restore_emp1_thread_ids) == len(pre_restore_emp1)
    assertions["emp1_thread_roles_mapped_to_ids"] = all_emp1_roles_mapped
    assertions["generated_emp1_marker_observed_before_restore"] = bool(pre_restore_emp1)
    assertions["restore_created_exact_backups_for_emp1_histories"] = (
        all_emp1_roles_mapped
        and pre_restore_emp1_thread_ids.issubset(
            set(report.get("backup_verification", {}).get("backup_threads_verified", []))
        )
    )
    assertions["restore_preserved_exact_pre_restore_prefixes"] = (
        all_emp1_roles_mapped
        and pre_restore_emp1_thread_ids.issubset(
            set(report.get("backup_verification", {}).get("target_prefix_threads_verified", []))
        )
    )
    assertions["all_created_thread_ids_are_distinct"] = len(set(ids.values())) == len(ids)
    assertions["native_config_matches_saved"] = (
        report.get("restore", {}).get("native_config_matches_saved") is True
    )
    assertions["second_restore_config_matches_saved"] = (
        report.get("second_restore", {}).get("config_matches_saved_native_config") is True
    )
    assertions["restore_succeeded"] = (
        report.get("restore", {}).get("exit_code") == 0
        and (
            report.get("restore", {}).get("action") == "restored"
            or (
                report.get("restore", {}).get("action") == "noop"
                and report.get("shutdown_restored_config") is True
            )
        )
        and report.get("restore", {}).get("state") == "restored"
        and report.get("restore", {}).get("native_base_url_restored") is True
        and assertions["native_config_matches_saved"]
        and report.get("restore", {}).get("provider_config_preserved") is True
    )
    assertions["second_unchanged_restore_is_noop"] = report.get("second_restore", {}).get("no_op") is True

    after = index_requests([item for item in requests if item.get("source") == "codex_direct"])
    outcomes = report.get("resume_turn_outcomes", {})
    for key, thread_id in ids.items():
        probe_name = PROBE_PROMPTS[key]
        candidates = after.get(key, [])
        if not candidates:
            candidates = after.get(probe_name, [])
        request = candidates[-1] if candidates else None
        assertions[f"{key}_single_native_resume_request"] = len(candidates) == 1
        assertions[f"{key}_same_id_resumed"] = report.get("resume_thread_ids", {}).get(key) == thread_id
        assertions[f"{key}_native_request_accepted"] = (
            request is not None
            and request.get("phase") == "post_restore_probe"
            and request.get("http_status") == 200
            and request.get("provider_error_code") is None
            and request.get("has_emp1") is False
            and outcomes.get(key) == "completed"
        )
        if request is not None:
            assertions[f"{key}_opaque_fixture_preserved"] = (
                request.get("opaque_items") == [
                    {"item_id": OPAQUE_ID, "encrypted_content_sha256": sha256(OPAQUE.encode())}
                ]
            )
            assertions[f"{key}_known_branch_marker_set_matches"] = (
                set(request.get("local_suffixes_seen", [])) == CREATED_LOCAL_MARKERS[key]
            )
            assertions[f"{key}_known_branch_marker_projection_matches"] = (
                request.get("local_marker_projection")
                == CREATED_LOCAL_MARKER_PROJECTION[key]
            )
            if key == "early_child":
                assertions[f"{key}_post_compaction_call_absent_from_calls_and_outputs"] = (
                    tool_call_id_absent(request, CALL_AFTER_COMPACTION)
                )
                assertions[f"{key}_pre_compaction_tool_pair_preserved"] = all(
                    tool_pair_present(
                        request,
                        CALL_BEFORE_COMPACTION,
                        tool_output_oracles[CALL_BEFORE_COMPACTION],
                    )
                )
            else:
                assertions[f"{key}_post_compaction_tool_pair_preserved"] = all(
                    tool_pair_present(
                        request,
                        CALL_AFTER_COMPACTION,
                        tool_output_oracles[CALL_AFTER_COMPACTION],
                    )
                )
            expected_summary = key != "early_child"
            assertions[f"{key}_portable_summary_expected"] = request.get("summary_seen") is expected_summary

    restore_peak = report.get("restore", {}).get("peak_rss_bytes")
    limit = report.get("process_limits", {}).get("address_space_bytes", 0)
    assertions["restore_peak_rss_within_limit"] = (
        type(restore_peak) is int and restore_peak > 0 and restore_peak <= limit
    )
    return assertions


def expected_baseline_failure(
    report: dict[str, Any], ids: dict[str, str], requests: list[dict[str, Any]]
) -> bool:
    if not (
        report.get("setup_turn_outcome") == "thread_created"
        and all(state == "completed" for state in report.get("build_turn_outcomes", {}).values())
        and report.get("restore", {}).get("exit_code") == 0
        and report.get("restore", {}).get("action") == "restored"
        and report.get("second_restore", {}).get("no_op") is True
        and report.get("resume_thread_ids", {}) == ids
    ):
        return False
    by_prompt = index_requests(requests)
    rejected = 0
    for key in ids:
        probe = by_prompt.get(key, []) or by_prompt.get(PROBE_PROMPTS[key], [])
        if (
            len(probe) == 1
            and probe[0].get("source") == "codex_direct"
            and probe[0].get("http_status") == 400
            and probe[0].get("provider_error_code") == "emp1_rejected"
            and probe[0].get("has_emp1") is True
            and report.get("resume_turn_outcomes", {}).get(key) == "failed"
        ):
            rejected += 1
    return rejected > 0
