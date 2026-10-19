"""Private candidate state; production files are read-only inputs."""
import base64
import hashlib
import json
import os
from pathlib import Path

from cryptography.fernet import Fernet

NATIVE = "native-test/gpt-6.1-sol"
EXTERNALS = ("claude-cpa/claude-opus-5-5", "chuang/gemma-3.1")
MAGIC = b"easy-multi-provider-v1\n"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Environment:
    def __init__(self, root, source, auth):
        root.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.root = root
        self.originals = {auth: digest(auth)}
        self.source = source
        self.source_digest = digest(source)
        self.env = {"PATH": os.environ["PATH"], "RUST_LOG": "error"}
        for key in ("HOME", "CODEX_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "TMPDIR"):
            path = root / key.lower()
            path.mkdir(mode=0o700)
            self.env[key] = str(path)
        self.env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"] = "1"
        self.project = root / "project"
        self.project.mkdir(mode=0o700)
        state = root / "state"
        state.mkdir(mode=0o700)
        (state / "secrets").mkdir(mode=0o700)
        destination_key = Fernet.generate_key()
        (state / "master.key").write_bytes(destination_key + b"\n")
        os.chmod(state / "master.key", 0o600)
        configuration = json.loads(source.read_text())
        self.source_selection = self.selection(configuration)
        providers = []
        ids = {model.split("/", 1)[0] for model in EXTERNALS}
        master = source.parent / "state/master.key"
        self.originals[master] = digest(master)
        source_key = master.read_bytes().strip()
        for original in configuration["providers"]:
            if original["id"] not in ids:
                continue
            provider = dict(original)
            key_path = Path(provider["api_key_file"])
            self.originals[key_path] = digest(key_path)
            encrypted = key_path.read_bytes()
            if not encrypted.startswith(MAGIC):
                raise RuntimeError("Expected an encrypted managed provider key")
            secret = Fernet(source_key).decrypt(encrypted[len(MAGIC):])
            destination = state / "secrets" / (provider["id"] + ".key.enc")
            destination.write_bytes(MAGIC + Fernet(destination_key).encrypt(secret))
            os.chmod(destination, 0o600)
            provider["api_key_file"] = str(destination)
            providers.append(provider)
        providers.append({"id": "native-test", "protocol": "responses", "auth_mode": "forward",
                          "base_url": "https://chatgpt.com/backend-api/codex"})
        models = [dict(model, enabled=True, output_limit=4096) for model in configuration["models"]
                  if model["id"] in EXTERNALS]
        if {model["id"] for model in models} != set(EXTERNALS):
            raise RuntimeError("Requested external models are not configured")
        models.append({"id": NATIVE, "provider": "native-test", "upstream_id": "gpt-6.1-sol",
                       "enabled": True, "context_window": 200000, "output_limit": 4096})
        self.config = root / "config.json"
        self.config.write_text(json.dumps({"providers": providers, "models": models,
                                          "auto_enable_on_start": False,
                                          "external_collaboration": "on_demand",
                                          "subscription_search": {"enabled": False}}))
        tokens = json.loads(auth.read_text())["tokens"]
        claims = json.loads(base64.urlsafe_b64decode(tokens["access_token"].split(".")[1] + "==="))
        identity = claims.get("https://api.openai.com/auth", {})
        self.login = {"type": "chatgptAuthTokens", "accessToken": tokens["access_token"],
                      "chatgptAccountId": tokens.get("account_id") or identity["chatgpt_account_id"],
                      "chatgptPlanType": identity.get("chatgpt_plan_type")}
        # No refresh token is copied or supplied to the disposable client.
        (root / "input-hashes.json").write_text(json.dumps({str(p): h for p, h in self.originals.items()}))

    def configure_codex(self, base_url):
        session = json.loads((self.root / "state/web-session.json").read_text())["token"]
        text = (f"model = {json.dumps(NATIVE)}\nmodel_provider = \"emp-switch\"\n"
                "approval_policy = \"never\"\nsandbox_mode = \"read-only\"\n"
                "model_reasoning_effort = \"low\"\nweb_search = \"disabled\"\n"
                "[features]\nplugins = false\nmemories = false\nmulti_agent = true\n"
                "multi_agent_v2 = true\n"
                "[model_providers.emp-switch]\nname = \"OpenAI\"\nwire_api = \"responses\"\n"
                f"base_url = {json.dumps(base_url + '/v1')}\nrequires_openai_auth = true\n"
                "supports_websockets = true\nrequest_max_retries = 0\nstream_max_retries = 0\n"
                f"http_headers = {{\"X-EMP-Session\" = {json.dumps(session)}}}\n")
        (Path(self.env["CODEX_HOME"]) / "config.toml").write_text(text)

    def unchanged(self):
        # The running EMP may update unrelated account/quota fields. Guard the
        # exact credential bytes and provider/model records actually consumed.
        return (all(digest(path) == expected for path, expected in self.originals.items())
                and self.selection(json.loads(self.source.read_text())) == self.source_selection)

    @staticmethod
    def selection(configuration):
        ids = {model.split("/", 1)[0] for model in EXTERNALS}
        return {"providers": [x for x in configuration["providers"] if x["id"] in ids],
                "models": [x for x in configuration["models"] if x["id"] in EXTERNALS]}
