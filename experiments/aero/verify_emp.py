import argparse
import base64
import contextlib
import http.client
import json
import os
import signal
import subprocess
import sys
import tarfile
import tempfile
from importlib import import_module
from pathlib import Path
from urllib.parse import urlsplit
from zipfile import ZipFile

from codex import codex_command
from verify import call, native_backend, reply, text
from verify_features import PNG, LiveText
from verify_lifecycle import ControlledUpstream, JsonResponse

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts/shadow_frontend"))
Browser = import_module("e2e").Browser
local_backend = import_module("local_backend")
request, start_backend, wait_for = (
    local_backend.request,
    local_backend.start_backend,
    local_backend.wait_for,
)


def run(binary, output, browser_enabled, without_python=False, native_codex=False):
    output.mkdir(parents=True, exist_ok=True)
    with contextlib.ExitStack() as stack:
        root = Path(
            stack.enter_context(tempfile.TemporaryDirectory(prefix="aero-emp-e2e-"))
        )
        upstream = stack.enter_context(ControlledUpstream("fixture-only"))
        configuration = {
            "providers": [
                {
                    "id": identifier,
                    "name": name,
                    "base_url": upstream.url,
                    "protocol": "responses",
                    "auth_mode": "api_key",
                    "api_key": "fixture-only",
                }
                for identifier, name in [("first", "Planner"), ("second", "Worker")]
            ],
            "models": [
                {
                    "id": source + "/alias",
                    "provider": source,
                    "upstream_id": "shared-upstream",
                    "context_window": 100000,
                    "reasoning_levels": ["low", "high"],
                    "enabled": True,
                }
                for source in ["first", "second"]
            ],
            "model_groups": [
                {
                    "id": "auto/test",
                    "family_id": "shared-upstream",
                    "name_prefix": "auto",
                    "policy": "priority",
                    "sources": ["first/alias", "second/alias"],
                }
            ],
            "auto_enable_on_start": False,
            "codex_base_url": "http://127.0.0.1:9/v1",
        }
        backend, url, state_root, management = start_backend(
            stack,
            binary,
            root,
            configuration,
            graceful=True,
            env_overrides={
                "PATH": str(root / "no-system-python"),
                "PYTHONIOENCODING": "ascii",
                "PYTHONUTF8": "0",
            }
            if without_python
            else None,
        )
        management["Origin"] = backend
        status, config = request(backend + "/api/config", headers=management)
        assert status == 200 and config["aero_enabled"] and config["experimental"], (
            config
        )
        status, result = request(backend + "/api/config", config, management)
        assert status == 200, ("Saving ordinary settings without aero profiles", result)
        profile = {
            "id": "aero/pair-id",
            "name": "统筹 → 执行",
            "planner": {"model": "first/alias", "effort": "high"},
            "executor": {"model": "second/alias", "effort": "low"},
        }
        config["aero_profiles"] = [profile]
        status, result = request(backend + "/api/config", config, management)
        assert status == 200, result
        config = request(backend + "/api/config", headers=management)[1]
        catalog_profile = next(
            model
            for model in config["catalog_models"]
            if model["source_type"] == "aero"
        )
        assert catalog_profile["id"] == "aero/统筹 → 执行", catalog_profile
        for bad in ["bad/name", "", "a" * 129]:
            invalid = {**config, "aero_profiles": [{**profile, "name": bad}]}
            assert request(backend + "/api/config", invalid, management)[0] == 400
        invalid = {
            **config,
            "aero_profiles": [profile, {**profile, "id": "aero/duplicate"}],
        }
        assert request(backend + "/api/config", invalid, management)[0] == 400
        invalid = {
            **config,
            "aero_profiles": [{**profile, "planner": {"model": "missing/alias"}}],
        }
        assert request(backend + "/api/config", invalid, management)[0] == 400
        invalid = {
            **config,
            "providers": [
                {**provider, "name": "aero"} if provider["id"] == "first" else provider
                for provider in config["providers"]
            ],
            "aero_profiles": [{**profile, "name": "alias"}],
        }
        assert request(backend + "/api/config", invalid, management)[0] == 400
        status, device = request(
            backend + "/api/gateway/clients", {"name": "aero tester"}, management
        )
        assert status == 200, device
        gateway_headers = {
            "Authorization": "Bearer " + device["api_key"],
            "session_id": "aero-emp-e2e",
        }
        body = {
            "model": catalog_profile["id"],
            "input": [{"role": "user", "content": "Implement the public fixture"}],
            "tools": [
                {
                    "type": "function",
                    "name": "shell_command",
                    "parameters": {"type": "object"},
                }
            ],
        }
        status, invalid = request(
            backend + "/v1/responses",
            body,
            {"Authorization": gateway_headers["Authorization"]},
        )
        assert status == 400 and not (root / "aero").exists(), invalid
        upstream.actions.put(
            JsonResponse(
                reply([call("exit_plan_mode", {"plan": "Implement and check"}, 1)])
            )
        )
        upstream.actions.put(JsonResponse(reply([text("Implementation verified")])))
        upstream.actions.put(JsonResponse(reply([text("AERO_EMP_REVIEWED_OK")])))
        status, result = request(
            backend + "/v1/responses", body, gateway_headers, timeout=30
        )
        assert status == 200, result
        assert "AERO_EMP_REVIEWED_OK" in json.dumps(result), result
        assert len(upstream.received) == 3, upstream.received
        efforts = [item["reasoning"]["effort"] for item in upstream.received]
        assert efforts == ["high", "low", "high"], efforts
        status, replay = request(backend + "/v1/responses", body, gateway_headers)
        assert status == 200 and replay == result and len(upstream.received) == 3
        report = wait_for(
            lambda: request(backend + "/api/gateway/usage", headers=management)[1]
        )
        assert sum(row["calls"] for row in report["rows"]) == 3, report
        assert all(row["caller_name"] == "aero tester" for row in report["rows"]), (
            report
        )
        assert all(row["client_kind"] == "aero" for row in report["rows"]), report
        children = (root / "aero/profiles.json").read_text()
        assert device["api_key"] not in children
        config["aero_profiles"][0]["name"] = "Renamed pair"
        config["aero_profiles"][0]["planner"] = {"model": "auto/test", "effort": "low"}
        assert request(backend + "/api/config", config, management)[0] == 200
        body = {**body, "model": "aero/Renamed pair"}
        upstream.actions.put(JsonResponse(reply([text("AERO_AGGREGATE_REVIEWED_OK")])))
        status, result = request(
            backend + "/v1/responses",
            body,
            {**gateway_headers, "session_id": "aero-aggregate-e2e"},
        )
        assert status == 200 and "AERO_AGGREGATE_REVIEWED_OK" in json.dumps(result), (
            result
        )
        assert upstream.received[-1]["reasoning"]["effort"] == "low"
        status, replay = request(
            backend + "/v1/responses",
            {**body, "model": "aero/统筹 → 执行"},
            gateway_headers,
        )
        assert status == 200 and "AERO_EMP_REVIEWED_OK" in json.dumps(replay), replay
        assert len(upstream.received) == 4
        upstream.actions.put(JsonResponse(reply([text("AERO_SSE_REVIEWED_OK")])))
        address = urlsplit(backend)
        connection = http.client.HTTPConnection(
            address.hostname, address.port, timeout=30
        )
        connection.request(
            "POST",
            "/v1/responses",
            json.dumps({**body, "stream": True}),
            {
                **gateway_headers,
                "session_id": "aero-sse-e2e",
                "Content-Type": "application/json",
            },
        )
        response = connection.getresponse()
        streamed = response.read().decode()
        assert (
            response.status == 200
            and "response.completed" in streamed
            and "AERO_SSE_REVIEWED_OK" in streamed
        ), streamed
        connection.close()
        live = LiveText()
        upstream.actions.put(live)
        connection = http.client.HTTPConnection(
            address.hostname, address.port, timeout=30
        )
        connection.request(
            "POST",
            "/v1/responses",
            json.dumps({**body, "stream": True}),
            {
                **gateway_headers,
                "session_id": "aero-live-e2e",
                "Content-Type": "application/json",
            },
        )
        response = connection.getresponse()
        try:
            assert response.status == 200
            while not live.release.is_set():
                line = response.readline()
                assert line, "The stream ended before the first live text delta"
                if (
                    line.startswith(b"data: ")
                    and json.loads(line[6:]).get("delta") == "Hello "
                ):
                    live.release.set()
            assert b"response.completed" in response.read()
        finally:
            live.release.set()
            connection.close()
        summary_text = (
            "Retain the public image/audio fixture and continue the agreed task"
        )
        upstream.actions.put(JsonResponse(reply([text(summary_text)])))
        status, compacted = request(
            backend + "/v1/responses/compact",
            body,
            {**gateway_headers, "session_id": "aero-compact-e2e"},
            timeout=30,
        )
        assert status == 200, compacted
        checkpoint = compacted["output"]
        assert checkpoint[0]["type"] == "compaction", compacted
        for session, marker in [
            ("aero-compact-e2e", "AERO_COMPACT_CONTINUED"),
            ("aero-foreign-checkpoint-e2e", "AERO_CHECKPOINT_RECOVERED"),
        ]:
            upstream.actions.put(JsonResponse(reply([text(marker)])))
            status, continued = request(
                backend + "/v1/responses",
                {
                    **body,
                    "input": checkpoint
                    + [{"role": "user", "content": "Continue the checkpoint task"}],
                },
                {**gateway_headers, "session_id": session},
                timeout=30,
            )
            assert status == 200 and marker in json.dumps(continued), continued
            assert summary_text in json.dumps(upstream.received[-1]), upstream.received[
                -1
            ]
            assert "emp1:" not in json.dumps(upstream.received[-1]["input"])
        wav = base64.b64encode(b"RIFF\x00\xffWAVEfixture").decode()
        upstream.actions.put(JsonResponse(reply([text("AERO_MEDIA_RECEIVED")])))
        status, media = request(
            backend + "/v1/responses",
            {
                **body,
                "input": [
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "input_text",
                                "text": "Inspect these public fixtures",
                            },
                            {
                                "type": "input_image",
                                "image_url": "data:image/png;base64," + PNG,
                            },
                            {
                                "type": "input_audio",
                                "input_audio": {"data": wav, "format": "wav"},
                            },
                        ],
                    }
                ],
            },
            {**gateway_headers, "session_id": "aero-media-e2e"},
            timeout=30,
        )
        assert status == 200 and "AERO_MEDIA_RECEIVED" in json.dumps(media), media
        assert PNG in json.dumps(upstream.received[-1]["input"]) and wav in json.dumps(
            upstream.received[-1]["input"]
        )
        native_steps = []
        if native_codex:
            scripted = native_backend(native_steps)

            class NativeReply:
                def handle(self, handler, body):
                    JsonResponse(scripted(body, None)).handle(handler, body)

            for _ in range(6):
                upstream.actions.put(NativeReply())
            status, catalog = request(
                backend + "/v1/models?client_version=0.169.0", headers=gateway_headers
            )
            assert status == 200, catalog
            assert any(model["slug"] == body["model"] for model in catalog["models"]), (
                catalog
            )
            catalog_path = root / "actual-emp-catalog.json"
            catalog_path.write_text(json.dumps(catalog))
            project = root / "project"
            project.mkdir()
            (project / "SPEC.txt").write_text("NATIVE_AERO_SPEC\n")
            command = codex_command(
                body["model"],
                address.port,
                catalog_path,
                project,
                [
                    "-c",
                    "model_providers.aero.supports_websockets=true",
                    "-c",
                    "features.apps=false",
                    "-c",
                    "features.plugins=false",
                    "-c",
                    "features.personality=false",
                    "exec",
                    "--ephemeral",
                    "--ignore-user-config",
                    "--skip-git-repo-check",
                    "--sandbox",
                    "workspace-write",
                    "--json",
                    "Follow SPEC.txt and complete the task using the aero workflow",
                ],
            )
            with subprocess.Popen(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
                env={
                    **os.environ,
                    "CODEX_HOME": str(root / "codex_home"),
                    "AERO_API_KEY": device["api_key"],
                },
            ) as process:
                try:
                    stdout, stderr = process.communicate(timeout=90)
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGTERM)
            (output / "emp-native-codex.jsonl").write_text(stdout)
            (output / "emp-native-codex.stderr").write_text(stderr)
            assert process.returncode == 0, stderr
            assert "NATIVE_AERO_REVIEWED_OK" in stdout, stdout + stderr
            assert (
                project / "verified.txt"
            ).read_text().strip() == "NATIVE_AERO_VERIFIED"
            assert len(native_steps) == 6, native_steps
        if browser_enabled:
            browser = Browser(stack, root)
            browser.navigate(url)

            def click(selector):
                wait_for(
                    lambda: browser.execute(
                        "return !!document.querySelector(arguments[0]);", selector
                    )
                )
                browser.execute(
                    "document.querySelector(arguments[0]).click();", selector
                )

            for language in ["zh-CN", "en"]:
                browser.execute("closeModal();setLanguage(arguments[0]);", language)
                assert browser.execute(
                    "return !document.querySelector('#aero_open').hidden;"
                )
                assert browser.execute(
                    "return document.querySelector('#check_update').hidden;"
                )
                icon = browser.execute(
                    "return getComputedStyle(document.querySelector('#aero_open'),'::before').maskImage;"
                )
                assert "aero.svg" in icon, icon
                click("#aero_open")
                click("[data-aero-add]")
                browser.execute(
                    "document.querySelector('#aero_name').value=arguments[0];document.querySelector('#aero_name').dispatchEvent(new Event('input'));",
                    "Pair " + language,
                )
                browser.execute(
                    "const input=document.querySelector('#aero_planner');input.value='auto/test';input.dispatchEvent(new Event('change'));"
                )
                browser.execute(
                    "const input=document.querySelector('#aero_executor');input.value='second/alias';input.dispatchEvent(new Event('change'));"
                )
                assert (
                    browser.execute(
                        "return document.querySelector('#aero_name').value;"
                    )
                    == "Pair " + language
                )
                assert "Planner" in browser.execute(
                    "return document.querySelector('#aero_executor').textContent;"
                )
                status, screenshot = request(browser.base + "/screenshot")
                assert status == 200, screenshot
                (output / ("pairing-" + language + ".png")).write_bytes(
                    base64.b64decode(screenshot["value"])
                )
                click("#modal_submit")
                wait_for(
                    lambda: browser.execute(
                        "return !!document.querySelector('[data-aero-edit]');"
                    )
                )
                saved = browser.api("/api/config")["aero_profiles"][-1]
                assert (
                    saved["name"] == "Pair " + language
                    and saved["planner"]["model"] == "auto/test"
                ), saved
                browser.execute("closeModal();")
                click(
                    '[data-ui-action="catalog-display-editor"][data-id="'
                    + saved["id"]
                    + '"]'
                )
                assert (
                    browser.execute(
                        "return document.querySelector('#aero_name').value;"
                    )
                    == saved["name"]
                )
                click("#modal_back")
                assert browser.execute(
                    "return !document.querySelector('#modal_close').hidden;"
                )
                browser.execute("closeModal();")
            assert not browser.diagnostics(), browser.diagnostics()
        upstream.assert_clean()
        summary = {
            "version": "999.14.2-exp",
            "synthetic_upstreams_only": True,
            "emp_to_aero_to_emp": "passed",
            "role_effort_and_aggregate": "passed",
            "rename_and_replay": "passed",
            "device_attribution_no_double_billing": "passed",
            "browser": "passed" if browser_enabled else "not run",
            "without_system_python": without_python,
            "sse": "passed",
            "live_first_token": "passed",
            "compact_continue_and_foreign_checkpoint_recovery": "passed",
            "image_audio_input": "passed",
            "native_codex": "passed" if native_codex else "not run",
            "upstream_calls": len(upstream.received),
        }
    (output / "emp-e2e.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--binary", type=Path)
    source.add_argument("--archive", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--browser", action="store_true")
    parser.add_argument("--without-system-python", action="store_true")
    parser.add_argument("--native-codex", action="store_true")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="aero-package-e2e-") as temporary:
        binary = args.binary
        if args.archive:
            if args.archive.suffix == ".zip":
                with ZipFile(args.archive) as archive:
                    archive.extractall(temporary)
            else:
                with tarfile.open(args.archive) as archive:
                    archive.extractall(temporary, filter="data")
            binary = Path(temporary) / "EMP" / ("EMP.exe" if os.name == "nt" else "EMP")
        run(
            binary.resolve(),
            args.output.resolve(),
            args.browser,
            args.without_system_python,
            args.native_codex,
        )
