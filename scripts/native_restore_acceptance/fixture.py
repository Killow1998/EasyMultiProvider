from __future__ import annotations

import json
import os
from pathlib import Path
import sys
from typing import Any

from .app_server import AppServer
from .common import fail
from .constants import MODEL

EMP_MODEL_PROVIDER = "native_test"


def make_env(home: Path, codex_home: Path, temp_dir: Path, provider_key: str) -> dict[str, str]:
    path = os.environ.get("PATH", os.defpath)
    return {
        "PATH": path,
        "HOME": str(home),
        "CODEX_HOME": str(codex_home),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "TMPDIR": str(temp_dir),
        "OPENAI_API_KEY": provider_key,
        "HTTP_PROXY": "http://127.0.0.1:1",
        "HTTPS_PROXY": "http://127.0.0.1:1",
        "ALL_PROXY": "http://127.0.0.1:1",
        "http_proxy": "http://127.0.0.1:1",
        "https_proxy": "http://127.0.0.1:1",
        "all_proxy": "http://127.0.0.1:1",
        "NO_PROXY": "127.0.0.1,localhost",
        "no_proxy": "127.0.0.1,localhost",
        "RUST_LOG": "error",
    }


def write_fixture_config(codex_home: Path, state_dir: Path, base_url: str) -> tuple[bytes, bytes]:
    config = codex_home / "config.toml"
    original_text = (
        "# disposable EMP native restore fixture\n"
        f'model = "{MODEL}"\n'
        'model_provider = "native_fake"\n'
        'approval_policy = "never"\n'
        'sandbox_mode = "read-only"\n'
        "\n"
        "[model_providers.native_fake]\n"
        'name = "native_fake"\n'
        f'base_url = "{base_url}"\n'
        'wire_api = "responses"\n'
        'env_key = "OPENAI_API_KEY"\n'
    )
    # TOML root keys must precede the first table; put managed fields there.
    first_table = original_text.index("[model_providers.native_fake]")
    head, tail = original_text[:first_table], original_text[first_table:]
    head = head.replace('sandbox_mode = "read-only"\n',
                        'sandbox_mode = "read-only"\n'
                        f'openai_base_url = "{base_url}"\n')
    applied_text = head + tail
    config.write_text(applied_text, encoding="utf-8")

    absent = {"present": False, "value": None}
    applied = {"present": True, "value": base_url}
    lease = {
        "schema": "easy-multi-provider.integration-lease",
        "version": 3,
        "config_path": str(config.resolve()),
        "config_existed": True,
        "fields": {
            "openai_base_url": {"original": absent, "applied": applied},
            "model_catalog_json": {"original": absent, "applied": absent},
            "experimental_realtime_ws_base_url": {"original": absent, "applied": absent},
        },
        "lease_id": "lease-acceptance-fixture",
        "instance_id": "instance-acceptance-fixture",
        "pid": os.getpid(),
        "status": "active",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    }
    lease_path = state_dir / "lease.json"
    lease_bytes = json.dumps(lease, indent=2).encode("utf-8") + b"\n"
    lease_path.write_bytes(lease_bytes)
    return original_text.encode("utf-8"), config.read_bytes()


def write_created_flow_config(
    codex_home: Path,
    state_dir: Path,
    native_url: str,
    emp_url: str,
    mcp_log: Path,
    emp_pid: int,
) -> tuple[bytes, bytes]:
    config = codex_home / "config.toml"
    mcp_server = Path(__file__).with_name("mcp_tool_server.py").resolve(strict=True)
    common = (
        f'model = "{MODEL}"\n'
        f'model_provider = "{EMP_MODEL_PROVIDER}"\n'
        'approval_policy = "never"\n'
        'sandbox_mode = "read-only"\n'
        'model_auto_compact_token_limit = 100\n'
        'model_context_window = 8192\n'
        '\n'
        '[mcp_servers.fixture]\n'
        f'command = {json.dumps(sys.executable)}\n'
        f'args = ["-B", {json.dumps(str(mcp_server))}, {json.dumps(str(mcp_log))}]\n'
        'startup_timeout_sec = 15\n'
        'tool_timeout_sec = 15\n'
        'default_tools_approval_mode = "approve"\n'
    )
    # Codex 0.158 uses this provider label for Responses remote-compaction V2;
    # both configured URLs and credentials remain disposable fixture values.
    provider_config = (
        f"\n[model_providers.{EMP_MODEL_PROVIDER}]\n"
        'name = "OpenAI"\n'
        f"base_url = {json.dumps(emp_url)}\n"
        'wire_api = "responses"\n'
        'env_key = "OPENAI_API_KEY"\n'
        'supports_websockets = false\n'
        'request_max_retries = 0\n'
        'stream_max_retries = 0\n'
    )
    native_provider_config = (
        "\n[model_providers.native_fake]\n"
        'name = "OpenAI"\n'
        f"base_url = {json.dumps(native_url)}\n"
        'wire_api = "responses"\n'
        'env_key = "OPENAI_API_KEY"\n'
        'supports_websockets = false\n'
        'request_max_retries = 0\n'
        'stream_max_retries = 0\n'
    )
    original_text = (
        f'openai_base_url = {json.dumps(native_url)}\n' + common + provider_config + native_provider_config
    )
    active_text = (
        f'openai_base_url = {json.dumps(emp_url)}\n' + common + provider_config + native_provider_config
    )
    config.write_text(active_text, encoding="utf-8")

    absent = {"present": False, "value": None}
    fields = {
        "openai_base_url": {
            "original": {"present": True, "value": native_url},
            "applied": {"present": True, "value": emp_url},
        },
        "model_catalog_json": {"original": absent, "applied": absent},
        "experimental_realtime_ws_base_url": {"original": absent, "applied": absent},
    }
    lease = {
        "schema": "easy-multi-provider.integration-lease",
        "version": 3,
        "config_path": str(config.resolve()),
        "config_existed": True,
        "fields": fields,
        "lease_id": "lease-created-history-acceptance",
        "instance_id": "instance-created-history-acceptance",
        "pid": emp_pid,
        "status": "active",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    }
    lease_path = state_dir / "lease.json"
    lease_path.write_text(json.dumps(lease, indent=2) + "\n", encoding="utf-8")
    return original_text.encode("utf-8"), active_text.encode("utf-8")


def extract_thread_id(result: Any) -> str:
    if not isinstance(result, dict) or not isinstance(result.get("thread"), dict):
        fail("Codex did not return thread metadata")
    thread_id = result["thread"].get("id")
    if not isinstance(thread_id, str) or not thread_id:
        fail("Codex did not return a thread id")
    return thread_id


def start_thread(server: AppServer, cwd: Path) -> str:
    result = server.rpc(
        "thread/start",
        {
            "cwd": str(cwd),
            "model": MODEL,
            "modelProvider": "native_fake",
            "historyMode": "paginated",
            "ephemeral": False,
            "approvalPolicy": "never",
            "sandbox": "read-only",
        },
    )
    thread_id = extract_thread_id(result)
    if run_turn(server, thread_id, "SETUP_BASELINE") != "completed":
        fail("Codex could not persist the initial disposable thread turn")
    return thread_id


def run_turn(server: AppServer, thread_id: str, prompt: str, model: str = MODEL) -> str:
    started = server.rpc(
        "turn/start",
        {
            "threadId": thread_id,
            "input": [{"type": "text", "text": prompt}],
            "model": model,
            "approvalPolicy": "never",
        },
    )
    if not isinstance(started, dict):
        return "invalid_start_response"
    return server.wait_turn(thread_id)


def fork_thread(server: AppServer, thread_id: str) -> str:
    result = server.rpc(
        "thread/fork",
        {
            "threadId": thread_id,
            "ephemeral": False,
            "excludeTurns": True,
            "approvalPolicy": "never",
            "sandbox": "read-only",
        },
    )
    return extract_thread_id(result)


def resume_thread(server: AppServer, thread_id: str, model: str = MODEL, provider: str = "native_fake") -> None:
    server.rpc(
        "thread/resume",
        {
            "threadId": thread_id,
            "model": model,
            "modelProvider": provider,
            "excludeTurns": True,
        },
    )
