import argparse
import copy
import json
import os
import subprocess
import threading
from pathlib import Path

from gateway import create_server
from settings import DEFAULT_RUNTIME, ROOT, gateway_key, load_config

DEFAULT_TEMPLATE = (
    Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "models_cache.json"
)


def write_catalog(config, template, destination):
    source = json.loads(template.read_bytes())["models"]
    if not source:
        raise ValueError("The Codex model catalog is empty")
    base = next((x for x in source if x["slug"] == "gpt-6.1-sol"), source[0])
    models = []
    for index, name in enumerate(config["models"]):
        model = copy.deepcopy(base)
        model.update(
            slug=name,
            display_name=name,
            description="Automatic planning, execution and review through EMP",
            priority=index,
            visibility="list",
            supported_in_api=True,
            default_reasoning_level=None,
            supported_reasoning_levels=[],
            supports_reasoning_summaries=False,
            supports_reasoning_summary_parameter=False,
            supports_reasoning_effort_updates=False,
            prefer_websockets=False,
            tool_mode="direct",
            shell_type="shell_command",
            use_responses_lite=False,
            input_modalities=["text"],
            supports_search_tool=False,
            context_window=128000,
            max_context_window=128000,
            auto_compact_token_limit=None,
            availability_nux=None,
            upgrade=None,
            service_tiers=[],
            additional_speed_tiers=[],
        )
        model["experimental_supported_tools"] = []
        models.append(model)
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(
        json.dumps({"models": models}, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )


def codex_command(model, port, catalog, cwd, extra):
    overrides = {
        "model_provider": "aero",
        "model_catalog_json": str(catalog),
        "model_providers.aero.name": "aero",
        "model_providers.aero.base_url": f"http://127.0.0.1:{port}/v1",
        "model_providers.aero.wire_api": "responses",
        "model_providers.aero.requires_openai_auth": False,
        "model_providers.aero.env_key": "AERO_API_KEY",
        "model_providers.aero.supports_websockets": False,
    }
    command = ["codex", "-m", model, "-C", str(cwd)]
    for key, value in overrides.items():
        command.extend(["-c", key + "=" + json.dumps(value, ensure_ascii=False)])
    return command + extra


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", default="aero/astra-sol")
    parser.add_argument("--cwd", type=Path, default=Path.cwd())
    parser.add_argument("--config", type=Path, default=ROOT / "profiles.json")
    parser.add_argument("--runtime", type=Path, default=DEFAULT_RUNTIME)
    parser.add_argument("--catalog-template", type=Path, default=DEFAULT_TEMPLATE)
    parser.add_argument("codex_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    try:
        config = load_config(args.config)
        if args.model not in config["models"]:
            raise ValueError("model must be a configured aero/... profile")
        token = gateway_key(config)
        runtime = args.runtime.expanduser().resolve()
        catalog = runtime / "models.json"
        write_catalog(config, args.catalog_template.expanduser(), catalog)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    server, store = create_server(config, runtime, token=token)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    extra = args.codex_args[1:] if args.codex_args[:1] == ["--"] else args.codex_args
    try:
        result = subprocess.run(
            codex_command(
                args.model, server.server_port, catalog, args.cwd.resolve(), extra
            ),
            env={**os.environ, "AERO_API_KEY": token},
        )
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
        store.close()
    raise SystemExit(result.returncode)
