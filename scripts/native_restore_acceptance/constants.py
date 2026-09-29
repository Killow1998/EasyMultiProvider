from __future__ import annotations

MODEL = "gpt-5"
SUMMARY = "EMP_ACCEPTANCE_SHARED_SUMMARY"
OPAQUE = "OPAQUE_NATIVE_REASONING_FIXTURE"
OPAQUE_ID = "reasoning_opaque_acceptance_001"
CALL_ID = "call_emp_acceptance_001"
CALL_ITEM_ID = "function_call_acceptance_001"
OUTPUT_ITEM_ID = "function_output_acceptance_001"
TOOL_NAME = "fixture_tool"
TOOL_ARGUMENTS = "{\"fixture\":true}"
TOOL_OUTPUT = "fixture tool completed"
MCP_TOOL_NAME = "fixture_tool"
MCP_NAMESPACE = "mcp__fixture"
MCP_FUNCTION_NAME = f"{MCP_NAMESPACE}__{TOOL_NAME}"
CALL_BEFORE_COMPACTION = "call_emp_acceptance_before_compact"
CALL_AFTER_COMPACTION = "call_emp_acceptance_after_compact"
PARENT_SUFFIX = "PARENT_LOCAL_SUFFIX"
CHILD_SUFFIX = "CHILD_LOCAL_SUFFIX"
GRANDCHILD_SUFFIX = "GRANDCHILD_LOCAL_SUFFIX"
EARLY_CHILD_SUFFIX = "EARLY_CHILD_LOCAL_SUFFIX"
PROBE_PROMPTS = {
    "parent": "ACCEPTANCE_PROBE_PARENT",
    "early_child": "ACCEPTANCE_PROBE_EARLY_CHILD",
    "child": "ACCEPTANCE_PROBE_CHILD",
    "grandchild": "ACCEPTANCE_PROBE_GRANDCHILD",
}
CREATED_FLOW_PROMPTS = {
    "tool_before_compaction": "REAL_CODEX_TOOL_TURN_BEFORE_COMPACTION",
    "early_child": "EXISTING_FORK_LOCAL_SUFFIX",
    "compaction_threshold": "REAL_CODEX_COMPACTION_THRESHOLD_TURN",
    "compaction_followup": "REAL_CODEX_COMPACTION_FOLLOWUP_TURN",
    "tool_after_compaction": "REAL_CODEX_TOOL_TURN_AFTER_COMPACTION",
    "late_child": "NEW_FORK_LOCAL_SUFFIX",
    "grandchild": "NEW_GRANDCHILD_LOCAL_SUFFIX",
}
BUILD_PROMPTS = {
    "build_grandchild": GRANDCHILD_SUFFIX,
    "build_child": CHILD_SUFFIX,
    "build_parent": PARENT_SUFFIX,
}
