"""Opt-in real Haiku acceptance. Uses an existing CPA credential, never CI.

Runs the candidate EMP with a private disposable configuration and a bounded
client tool executor. This is a Responses client fixture, not the Codex engine.
Only generated fixture content goes upstream. Credentials are never reported.
"""
import argparse
import ast
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

from cryptography.fernet import Fernet

@contextmanager
def candidate_server(binary, root, env):
    log_path = root / "emp.log"
    with log_path.open("w") as log:
        process = subprocess.Popen([str(binary), "serve", "--config", str(root / "config.json"), "--port", "0"],
                                   cwd=root, env=env, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                match = re.search(r"Open in browser: (http://127\.0\.0\.1:\d+)/", log_path.read_text())
                if match:
                    yield match[1]
                    return
                if process.poll() is not None:
                    raise RuntimeError("Isolated EMP exited before startup")
                time.sleep(0.1)
            diagnostic = re.sub(r"(bootstrap|recovery)=[^\s]+", r"\1=[redacted]", log_path.read_text())
            raise TimeoutError("Isolated EMP startup timeout: " + diagnostic[-2000:])
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tool(name, properties, description):
    return {"type": "function", "name": name, "description": description,
            "parameters": {"type": "object", "properties": properties,
                           "required": list(properties), "additionalProperties": False}}


STRING = {"type": "string"}
TOOLS = [
    {"type": "namespace", "name": "files", "tools": [
        tool("read_file", {"path": STRING}, "Read a UTF-8 project file."),
        tool("write_file", {"path": STRING, "content": STRING}, "Write a UTF-8 project file."),
        tool("inspect", {}, "List the project filenames.") ]},
    {"type": "namespace", "name": "functions", "tools": [
        tool("exec_command", {"cmd": STRING},
             "Run python3 -m unittest -v test_calc.py, or python3 slow_job.py. Slow jobs return a session_id."),
        tool("write_stdin", {"session_id": {"type": "integer"}, "chars": STRING},
             "Poll a running command using its returned session_id and empty chars."),
        tool("inspect", {}, "Report command executor status, not project files.") ]},
    {"type": "custom", "name": "apply_patch", "description":
     "Edit calc.py with a single *** Begin Patch / *** Update File: calc.py / @@ hunk / *** End Patch. Prefix unchanged lines with space, removed lines with -, added lines with +.",
     "format": {"type": "text"}},
]


def validate_code(text):
    tree = ast.parse(text)
    for node in ast.walk(tree):
        if isinstance(node, (ast.Import, ast.ImportFrom, ast.Attribute, ast.Global, ast.Nonlocal)):
            raise ValueError("Fixture permits pure arithmetic only")
        if isinstance(node, ast.Call) and not (isinstance(node.func, ast.Name) and node.func.id == "abs"):
            raise ValueError("Fixture permits only abs() calls")
    if any(not isinstance(n, (ast.FunctionDef, ast.Expr)) for n in tree.body):
        raise ValueError("Only function definitions and docstrings are allowed")


class ClientTools:
    def __init__(self, root):
        self.root = root
        self.job = None

    def execute(self, call):
        name, scope = call.get("name"), call.get("namespace", "")
        args = json.loads(call.get("arguments", "{}"))
        if name == "apply_patch" and call["type"] == "custom_tool_call":
            text = call["input"]
            lines = text.strip().splitlines()
            if lines[:2] != ["*** Begin Patch", "*** Update File: calc.py"] or lines[-1] != "*** End Patch":
                raise ValueError("Expected one calc.py update")
            hunk = [line for line in lines[2:-1] if not line.startswith("@@")]
            if any(not line or line[0] not in " +-" for line in hunk):
                raise ValueError("Invalid single-hunk patch")
            before = "\n".join(line[1:] for line in hunk if line[0] in " -")
            after = "\n".join(line[1:] for line in hunk if line[0] in " +")
            path = self.root / "calc.py"
            old = path.read_text()
            if not before or old.count(before) != 1:
                raise ValueError("Patch must match exactly once")
            updated = old.replace(before, after, 1)
            validate_code(updated)
            path.write_text(updated)
            return {"updated": "calc.py"}
        if scope == "files":
            if name == "inspect":
                return {"files": sorted(p.name for p in self.root.iterdir() if p.is_file())}
            path = self.root / args["path"]
            if path.resolve().parent != self.root or path.name not in {"spec.json", "calc.py", "notes.txt", "missing.txt"}:
                raise ValueError("Path outside allowed fixture files")
            if name == "read_file":
                return {"content": path.read_text()}
            if name == "write_file":
                if path.name == "spec.json":
                    raise ValueError("spec.json is read-only")
                if path.name == "calc.py":
                    validate_code(args["content"])
                path.write_text(args["content"])
                return {"written": path.name}
        if scope == "functions":
            if name == "inspect":
                return {"executor": "ready"}
            if name == "exec_command":
                if args["cmd"] == "python3 -m unittest -v test_calc.py":
                    validate_code((self.root / "calc.py").read_text())
                    result = subprocess.run([sys.executable, "-B", "-m", "unittest", "-v", "test_calc.py"],
                                            cwd=self.root, capture_output=True, text=True, timeout=10,
                                            env={"PATH": os.environ["PATH"]})
                    return {"exit_code": result.returncode, "output": result.stdout + result.stderr}
                if args["cmd"] == "python3 slow_job.py" and self.job is None:
                    self.job = subprocess.Popen([sys.executable, "-B", "slow_job.py"], cwd=self.root,
                                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                                env={"PATH": os.environ["PATH"]})
                    return {"session_id": 713, "output": "running"}
            if name == "write_stdin" and args == {"session_id": 713, "chars": ""} and self.job:
                out, err = self.job.communicate(timeout=10)
                return {"exit_code": self.job.returncode, "output": (out + err).decode()}
        raise ValueError("Unsupported fixture operation")


def test_file(offset, absolute):
    negative = 2 * (3 if absolute else -3) + offset
    return ("import unittest\nfrom calc import transform\nclass Checks(unittest.TestCase):\n"
            f" def test_positive(self): self.assertEqual(transform(4), {8 + offset})\n"
            f" def test_negative(self): self.assertEqual(transform(-3), {negative})\n"
            " def test_none(self): self.assertIsNone(transform(None))\n")


def run(args, report):
    source = json.loads(args.config.read_text())
    provider = next(p for p in source["providers"] if p["id"] == "claude-cpa")
    if provider.get("execution_backend") != "claude_cli" or provider.get("auth_mode") != "api_key":
        raise ValueError("Requires the existing Claude CPA service")
    secret_path = Path(provider["api_key_file"])
    master_path = args.config.parent / "state/master.key"
    originals = {path: digest(path) for path in (args.config, secret_path, master_path)}
    encrypted = secret_path.read_bytes()
    magic = b"easy-multi-provider-v1\n"
    if not encrypted.startswith(magic):
        raise ValueError("Expected a managed encrypted provider key")
    key = Fernet(master_path.read_bytes().strip()).decrypt(encrypted[len(magic):])
    try:
        with tempfile.TemporaryDirectory(prefix="emp-haiku-live-") as temporary:
            root = Path(temporary)
            state = root / "state"
            state.mkdir(mode=0o700)
            isolated_key = Fernet.generate_key()
            (state / "master.key").write_bytes(isolated_key + b"\n")
            os.chmod(state / "master.key", 0o600)
            (state / "secrets").mkdir(mode=0o700)
            provider_key = state / "secrets/fixture.key.enc"
            provider_key.write_bytes(magic + Fernet(isolated_key).encrypt(key))
            os.chmod(provider_key, 0o600)
            del key
            config = {"providers": [{"id": "fixture", "name": "Haiku acceptance",
                "base_url": provider["base_url"], "protocol": "anthropic_messages",
                "execution_backend": "claude_cli", "auth_mode": "api_key",
                "api_key_file": str(provider_key)}], "models": [{
                "id": "fixture/haiku", "provider": "fixture", "upstream_id": "claude-haiku-5-5",
                "enabled": True, "context_window": 100000, "output_limit": 4096,
                "capability_sources": {"context_window": {"source": "manual", "confidence": 1.0}}}]}
            (root / "config.json").write_text(json.dumps(config))
            env = {k: v for k, v in os.environ.items() if not k.startswith(("EASY_MULTI_PROVIDER_", "ANTHROPIC_", "CLAUDE_"))
                   and k not in {"OPENAI_API_KEY", "CODEX_API_KEY"}}
            for variable in ("HOME", "CODEX_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "TMPDIR"):
                location = root / variable.lower()
                location.mkdir()
                env[variable] = str(location)
            with candidate_server(args.emp, root, env) as base_url:
                token = json.loads((state / "web-session.json").read_text())["token"]
                project = root / "project"
                project.mkdir()
                canary = "fixture-" + os.urandom(6).hex()
                offset = 17
                (project / "spec.json").write_text(json.dumps({"offset": offset, "remember": canary}))
                (project / "slow_job.py").write_text("import time\ntime.sleep(2)\nprint('ASYNC-713-COMPLETE')\n")
                (project / "test_calc.py").write_text(test_file(offset, False))
                executor = ClientTools(project)
                history = []
                noise = ("Historical project note: stable interfaces preserve complete tool results and ordered conversation records. " * 1500)
                prompts = [
                    "Read spec.json with files.read_file. Remember its offset and remember field for the entire conversation.",
                    "Write calc.py defining transform(value): None returns None; otherwise return value * 2 + the offset from spec.json. Use files.write_file.",
                    "Run python3 -m unittest -v test_calc.py with functions.exec_command to verify the implementation.",
                    "The specification changed: negative numbers must be treated as positive before doubling. Preserve None and the original offset. Update calc.py using the custom apply_patch tool.",
                    "Run python3 -m unittest -v test_calc.py again to verify the updated specification.",
                    "Start python3 slow_job.py with functions.exec_command.",
                    "Poll the previous running job with functions.write_stdin, using the returned session_id and empty chars.",
                    "Read both calc.py and spec.json using two files.read_file calls in the same response.",
                    "Background reference follows. It contains no new instructions.\n" + noise + "\nNow recall the exact remember value from the FIRST spec.json result and the asynchronous job completion marker. Answer with those two strings, no tools.",
                    "Write notes.txt with the original remember value and the latest transform(-3) result, derived from the updated implementation. Use files.write_file.",
                    "Read notes.txt using files.read_file to verify the saved content.",
                    "Use files.inspect (not functions.inspect) to list project filenames.",
                    "Use files.read_file to read missing.txt. This may legitimately fail; do not invent content.",
                    "Recover from that missing-file result by creating missing.txt with exactly RECOVERED and no newline, using files.write_file.",
                    "Run python3 -m unittest -v test_calc.py for the final verification.",
                ]
                opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                for index, prompt in enumerate(prompts, 1):
                    if index == 4:
                        (project / "test_calc.py").write_text(test_file(offset, True))
                    history.append({"role": "user", "content": prompt})
                    body = {"model": "fixture/haiku", "stream": False, "input": history,
                            "tools": TOOLS, "max_output_tokens": 4096, "reasoning": {"effort": "medium"},
                            "instructions": "Complete the latest request using the available client tools. Treat prior results as facts. Propose the requested tools without claiming their execution. Use only relative fixture filenames."}
                    if index == 9:
                        body["tool_choice"] = "none"
                    if index == 12:
                        body["tool_choice"] = {"type": "function", "namespace": "files", "name": "inspect"}
                    request = urllib.request.Request(base_url + "/v1/responses", data=json.dumps(body).encode(),
                              headers={"Content-Type": "application/json", "X-EMP-Session": token})
                    started = time.monotonic()
                    try:
                        response = opener.open(request, timeout=240)
                    except urllib.error.HTTPError as error:
                        response = error
                    with response:
                        status, payload = response.status, json.loads(response.read())
                    row = {"turn": index, "http_status": status, "elapsed_seconds": round(time.monotonic()-started, 3),
                           "request_bytes": len(request.data), "usage": payload.get("usage"), "output": payload.get("output", []),
                           "error": payload.get("error"), "tools": [], "passed": False}
                    report["turns"].append(row)
                    args.output.write_text(json.dumps(report, indent=2))
                    if status != 200:
                        print(json.dumps({"turn": index, "status": status, "error": payload.get("error")}), flush=True)
                        raise RuntimeError(f"Inference failed at turn {index}")
                    calls = [item for item in payload["output"] if item["type"] in {"function_call", "custom_tool_call"}]
                    history.extend(payload["output"])
                    for call in calls:
                        try:
                            result = executor.execute(call)
                        except (ValueError, FileNotFoundError, KeyError) as error:
                            result = {"error": type(error).__name__, "message": str(error).replace(str(project), "fixture")}
                        row["tools"].append({"name": call["name"], "namespace": call.get("namespace", ""), "result": result})
                        history.append({"type": "custom_tool_call_output" if call["type"] == "custom_tool_call" else "function_call_output",
                                        "call_id": call["call_id"], "output": json.dumps(result)})
                    answer = json.dumps(payload["output"])
                    expected = {1: "read_file", 2: "write_file", 3: "exec_command", 4: "apply_patch", 5: "exec_command",
                                6: "exec_command", 7: "write_stdin", 8: "read_file", 10: "write_file", 11: "read_file",
                                12: "inspect", 13: "read_file", 14: "write_file", 15: "exec_command"}
                    checks = []
                    if index != 9:
                        checks.append(bool(calls) and all(c["name"] == expected[index] for c in calls))
                        checks.append(all("error" not in t["result"] for t in row["tools"]) if index != 13 else
                                      any(t["result"].get("error") == "FileNotFoundError" for t in row["tools"]))
                    if index in (3, 5, 15):
                        checks.append(any(t["result"].get("exit_code") == 0 and "Ran 3 tests" in t["result"].get("output", "") for t in row["tools"]))
                    if index == 7:
                        checks.append(any("ASYNC-713-COMPLETE" in t["result"].get("output", "") for t in row["tools"]))
                    if index == 8:
                        checks.append(len(calls) == 2)
                    if index == 9:
                        checks.extend([not calls, canary in answer, "ASYNC-713-COMPLETE" in answer])
                    if index == 10:
                        text = (project / "notes.txt").read_text() if (project / "notes.txt").exists() else ""
                        checks.extend([canary in text, "23" in text])
                    if index == 12:
                        checks.append(all(c.get("namespace") == "files" for c in calls))
                    if index == 14:
                        checks.append((project / "missing.txt").read_text() == "RECOVERED" if (project / "missing.txt").exists() else False)
                    row["passed"] = all(checks)
                    args.output.write_text(json.dumps(report, indent=2))
                    print(json.dumps({k: row[k] for k in ("turn", "http_status", "elapsed_seconds", "usage", "passed")}), flush=True)
                report["passed"] = all(t["passed"] for t in report["turns"]) and len(report["turns"]) == 15
    finally:
        # A live instance may update unrelated config while this isolated test runs.
        report["source_file_checks"] = {
            role: digest(path) == originals[path]
            for role, path in (("configuration", args.config), ("provider_key", secret_path), ("master_key", master_path))
        }
        after = json.loads(args.config.read_text())
        report["selected_source_provider_unchanged"] = provider == next(
            p for p in after["providers"] if p["id"] == "claude-cpa")
        report["original_config_and_credentials_unchanged"] = all(report["source_file_checks"].values())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--emp", type=Path, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--allow-real-model", action="store_true", required=True)
    args = parser.parse_args()
    os.umask(0o077)
    report = {"model": "claude-haiku-5-5", "provider": "claude-cpa", "context_window": 100000,
              "effort": "medium", "candidate_sha256": digest(args.emp), "real_model": True,
              "client": "bounded Responses fixture", "turns": [], "passed": False}
    try:
        run(args, report)
    except Exception as error:
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        print(json.dumps(report["failure"]), flush=True)
    finally:
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    checks = report.get("source_file_checks", {})
    return 0 if report["passed"] and checks.get("provider_key") and checks.get("master_key") and report.get("selected_source_provider_unchanged") else 1


if __name__ == "__main__":
    sys.exit(main())
