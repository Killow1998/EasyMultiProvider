from __future__ import annotations

from typing import Any

from .common import semantic_function_output_retained, sha256
from .constants import (
    CALL_ID,
    CALL_ITEM_ID,
    CHILD_SUFFIX,
    GRANDCHILD_SUFFIX,
    OPAQUE,
    OPAQUE_ID,
    PARENT_SUFFIX,
    TOOL_ARGUMENTS,
    TOOL_NAME,
    TOOL_OUTPUT,
)
from .fake_provider import FakeResponsesProvider


EXPECTED_LOCAL_MARKERS = {
    "parent": {PARENT_SUFFIX},
    "child": {PARENT_SUFFIX, CHILD_SUFFIX},
    "grandchild": {PARENT_SUFFIX, CHILD_SUFFIX, GRANDCHILD_SUFFIX},
}
EXPECTED_LOCAL_MARKER_PROJECTION = {
    "parent": [PARENT_SUFFIX],
    "child": [PARENT_SUFFIX, CHILD_SUFFIX],
    "grandchild": [PARENT_SUFFIX, CHILD_SUFFIX, GRANDCHILD_SUFFIX],
}

EXPECTED_OPAQUE_ITEMS = [
    {
        "item_id": OPAQUE_ID,
        "encrypted_content_sha256": sha256(OPAQUE.encode("utf-8")),
    }
]
EXPECTED_FUNCTION_CALLS = [
    {
        "item_id": CALL_ITEM_ID,
        "call_id": CALL_ID,
        "name": TOOL_NAME,
        "arguments_sha256": sha256(TOOL_ARGUMENTS.encode("utf-8")),
    }
]
EXPECTED_TOOL_OUTPUT_SHA256 = sha256(TOOL_OUTPUT.encode("utf-8"))


def request_index(requests: list[dict[str, Any]]) -> tuple[dict[str, dict[str, Any]], dict[str, int]]:
    by_prompt: dict[str, dict[str, Any]] = {}
    counts: dict[str, int] = {}
    for request in requests:
        prompt = request.get("prompt")
        if not isinstance(prompt, str):
            continue
        counts[prompt] = counts.get(prompt, 0) + 1
        by_prompt[prompt] = request
    return by_prompt, counts


def build_assertions(report: dict[str, Any], ids: dict[str, str]) -> dict[str, bool]:
    requests = report.get("requests", [])
    by_prompt, counts = request_index(requests)
    assertions: dict[str, bool] = {}
    marker_probe = {
        "input": [
            {"type": "text", "text": CHILD_SUFFIX},
            {"type": "text", "text": GRANDCHILD_SUFFIX},
        ]
    }
    assertions["grandchild_prompt_uses_exact_marker"] = (
        not FakeResponsesProvider.has_exact_token(GRANDCHILD_SUFFIX, CHILD_SUFFIX)
        and FakeResponsesProvider.classify_prompt(marker_probe) == "build_grandchild"
    )

    setup = by_prompt.get("setup")
    assertions["successful_setup_request_seen"] = (
        counts.get("setup") == 1
        and setup is not None
        and setup.get("phase") == "setup"
        and setup.get("http_method") == "POST"
        and setup.get("http_status") == 200
        and setup.get("provider_error_code") is None
        and report.get("setup_turn_outcome") == "completed"
    )

    for key in ids:
        build_request = by_prompt.get(f"build_{key}")
        assertions[f"{key}_pre_restore_fixture_request_seen"] = (
            counts.get(f"build_{key}") == 1
            and build_request is not None
            and build_request.get("phase") == "pre_restore_build"
            and build_request.get("http_method") == "POST"
            and build_request.get("http_status") == 200
        )
        if build_request is not None:
            assertions[f"{key}_pre_restore_marker_present"] = build_request.get("has_emp1") is True
            assertions[f"{key}_pre_restore_portable_summary_present"] = (
                build_request.get("portable_summary_seen") is True
            )
            assertions[f"{key}_pre_restore_opaque_identity_retained"] = (
                build_request.get("opaque_items") == EXPECTED_OPAQUE_ITEMS
            )
            assertions[f"{key}_pre_restore_tool_call_retained"] = (
                build_request.get("function_calls") == EXPECTED_FUNCTION_CALLS
            )
            assertions[f"{key}_pre_restore_tool_output_retained"] = (
                semantic_function_output_retained(
                    build_request.get("function_outputs"),
                    call_id=CALL_ID,
                    name=TOOL_NAME,
                    output_sha256=EXPECTED_TOOL_OUTPUT_SHA256,
                )
            )
            assertions[f"{key}_pre_restore_known_marker_set_matches"] = (
                set(build_request.get("local_suffixes_seen", [])) == EXPECTED_LOCAL_MARKERS[key]
            )
            assertions[f"{key}_pre_restore_known_marker_projection_matches"] = (
                build_request.get("local_marker_projection")
                == EXPECTED_LOCAL_MARKER_PROJECTION[key]
            )
        assertions[f"{key}_pre_restore_turn_completed"] = (
            report.get("build_turn_outcomes", {}).get(key) == "completed"
        )

    for key in ids:
        request = by_prompt.get(key)
        assertions[f"{key}_same_thread_id_resumed"] = (
            report.get("resume_thread_ids", {}).get(key) == ids[key]
        )
        assertions[f"{key}_post_restore_request_seen"] = (
            counts.get(key) == 1
            and request is not None
            and request.get("phase") == "post_restore_probe"
            and request.get("http_method") == "POST"
        )
        if request is None:
            continue
        assertions[f"{key}_emp1_removed"] = request.get("has_emp1") is False
        assertions[f"{key}_summary_retained"] = request.get("summary_seen") is True
        assertions[f"{key}_opaque_identity_retained"] = (
            request.get("opaque_items") == EXPECTED_OPAQUE_ITEMS
        )
        assertions[f"{key}_tool_call_arguments_retained"] = (
            request.get("function_calls") == EXPECTED_FUNCTION_CALLS
        )
        assertions[f"{key}_tool_output_retained"] = (
            semantic_function_output_retained(
                request.get("function_outputs"),
                call_id=CALL_ID,
                name=TOOL_NAME,
                output_sha256=EXPECTED_TOOL_OUTPUT_SHA256,
            )
        )
        assertions[f"{key}_known_marker_set_matches"] = (
            set(request.get("local_suffixes_seen", [])) == EXPECTED_LOCAL_MARKERS[key]
        )
        assertions[f"{key}_known_marker_projection_matches"] = (
            request.get("local_marker_projection") == EXPECTED_LOCAL_MARKER_PROJECTION[key]
        )
        assertions[f"{key}_provider_accepted"] = (
            request.get("http_status") == 200
            and request.get("provider_error_code") is None
        )
        assertions[f"{key}_turn_completed"] = report.get("turn_outcomes", {}).get(key) == "completed"

    thread_ids = set(ids.values())
    verification = report.get("backup_verification", {})
    assertions["all_three_history_backups_verified"] = (
        set(verification.get("backup_threads_verified", [])) == thread_ids
    )
    assertions["all_three_original_jsonl_prefixes_verified"] = (
        set(verification.get("target_prefix_threads_verified", [])) == thread_ids
    )
    restore = report.get("restore", {})
    assertions["native_config_matches_saved"] = (
        restore.get("native_config_matches_saved") is True
    )
    assertions["native_config_restored"] = (
        restore.get("exit_code") == 0
        and restore.get("action") == "restored"
        and restore.get("state") == "restored"
        and restore.get("config_managed_fields_removed") is True
        and restore.get("provider_config_preserved") is True
        and assertions["native_config_matches_saved"]
    )
    assertions["restore_respects_process_memory_bound"] = (
        type(restore.get("peak_rss_bytes")) is int
        and restore.get("peak_rss_bytes") > 0
        and restore.get("peak_rss_bytes") <= restore.get("address_space_limit_bytes", 0)
    )
    assertions["second_restore_config_matches_saved"] = (
        report.get("second_restore", {}).get("config_matches_saved_native_config") is True
    )
    assertions["second_unchanged_restore_is_noop"] = (
        report.get("second_restore", {}).get("no_op") is True
    )
    large = report.get("fixture", {}).get("large_historical_rollout")
    if large is not None:
        assertions["historical_rollout_exceeds_128_mib"] = (
            large.get("rollout_bytes", 0) > 128 * 1024 * 1024
        )
        assertions["large_fixture_is_incremental_disk_jsonl"] = (
            large.get("storage") == "disk-backed incrementally appended JSONL"
            and large.get("record_count", 0) > 0
            and large.get("context_padding_type") == "event_msg/token_count"
        )
        assertions["large_restore_memory_bounded"] = (
            type(restore.get("peak_rss_bytes")) is int
            and restore.get("peak_rss_bytes") > 0
            and restore.get("peak_rss_bytes") <= restore.get("address_space_limit_bytes", 0)
        )
        assertions["large_history_native_resume_memory_bounded"] = (
            type(report.get("resume_app_server_peak_rss_bytes")) is int
            and report.get("resume_app_server_peak_rss_bytes") > 0
            and report.get("resume_app_server_peak_rss_bytes")
            <= report.get("process_limits", {}).get("address_space_bytes", 0)
        )
        probes = [
            item
            for item in report.get("requests", [])
            if item.get("phase") == "post_restore_probe"
        ]
        assertions["large_historical_rows_did_not_expand_current_context"] = (
            bool(probes)
            and max(item.get("request_body_bytes", 2**63) for item in probes) < 16 * 1024 * 1024
        )
    return assertions


def is_expected_baseline_failure(report: dict[str, Any], ids: dict[str, str]) -> bool:
    restore = report.get("restore", {})
    if not (
        report.get("setup_turn_outcome") == "completed"
        and report.get("build_turn_outcomes", {}) == {key: "completed" for key in ids}
        and restore.get("exit_code") == 0
        and restore.get("action") == "restored"
        and restore.get("state") == "restored"
        and restore.get("config_managed_fields_removed") is True
        and restore.get("provider_config_preserved") is True
        and restore.get("native_config_matches_saved") is True
        and report.get("resume_thread_ids", {}) == ids
    ):
        return False

    requests = report.get("requests", [])
    setup_requests = [item for item in requests if item.get("prompt") == "setup"]
    probes = [item for item in requests if item.get("phase") == "post_restore_probe"]
    if len(setup_requests) != 1 or len(probes) != len(ids):
        return False
    setup = setup_requests[0]
    if not (
        setup.get("http_method") == "POST"
        and setup.get("http_status") == 200
        and setup.get("provider_error_code") is None
    ):
        return False

    probe_by_prompt = {item.get("prompt"): item for item in probes}
    if set(probe_by_prompt) != set(ids):
        return False
    for key in ids:
        request = probe_by_prompt[key]
        if not (
            request.get("http_method") == "POST"
            and request.get("http_status") == 400
            and request.get("provider_error_code") == "emp1_rejected"
            and request.get("has_emp1") is True
            and report.get("turn_outcomes", {}).get(key) == "failed"
        ):
            return False
    return True


def classify(report: dict[str, Any], ids: dict[str, str]) -> str:
    if report.get("passed") is True:
        return "candidate_success"
    if is_expected_baseline_failure(report, ids):
        return "expected_baseline_failure"
    return "unexpected_failure"
