import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import easy_multi_provider.router as router
from easy_multi_provider.catalog import build_catalog, write_catalog
from easy_multi_provider.config import load, normalize, save
from easy_multi_provider.router import discover_models, forward_responses, responses_to_chat
from easy_multi_provider.server import AppState


class _JsonResponse:
    status = 200
    headers = {"Content-Type": "application/json"}

    def __init__(self, value):
        self._raw = json.dumps(value).encode("utf-8")
        self._read = False

    def read(self, size=-1):
        if self._read:
            return b""
        self._read = True
        return self._raw

    def close(self):
        pass

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


class MultimodalRegressionTests(unittest.TestCase):
    def _provider(self):
        return {
            "id": "openrouter",
            "base_url": "https://openrouter.example/api/v1",
            "protocol": "responses",
            "auth_mode": "api_key",
        }

    def _raw_config(self, root, models):
        return {
            "native_catalog_path": str(Path(root) / "native-models.json"),
            "providers": [self._provider()],
            "models": models,
        }

    def _discover(self, value):
        with patch.object(
            router, "urlopen", return_value=_JsonResponse(value)
        ), patch.object(router, "_discovery_headers", return_value={}):
            return discover_models(self._provider())

    def _state(self, root, models=None):
        root = Path(root)
        path = root / "config.json"
        save(normalize(self._raw_config(root, models or [])), path)
        return AppState(path, catalog_path=root / "catalog.json"), path

    def test_openrouter_architecture_modalities_are_discovered(self):
        models = self._discover(
            {
                "data": [
                    {
                        "id": "vision-model",
                        "name": "Vision model",
                        "architecture": {
                            "input_modalities": ["text", "image"],
                            "output_modalities": ["text"],
                        },
                        "context_length": 128000,
                    }
                ]
            }
        )

        self.assertEqual(models[0].get("input_modalities"), ["text", "image"])

    def test_discovered_modalities_are_imported_and_persisted(self):
        with tempfile.TemporaryDirectory() as directory:
            state, path = self._state(Path(directory))
            discovered = [
                {
                    "upstream_id": "vision-model",
                    "display_name": "Vision model",
                    "input_modalities": ["text", "image"],
                    "context_window": 128000,
                }
            ]
            with patch(
                "easy_multi_provider.server.discover_models",
                return_value=discovered,
            ), patch(
                "easy_multi_provider.server.generated_catalog_path",
                return_value=Path(directory) / "catalog.json",
            ):
                result = state.discover_provider_models("openrouter", ["vision-model"])

            self.assertEqual(result["added"], 1)
            persisted = load(path)
            self.assertEqual(
                persisted["models"][0].get("input_modalities"), ["text", "image"]
            )

    def test_catalog_projects_text_and_image_modalities(self):
        with tempfile.TemporaryDirectory() as directory:
            config = normalize(
                self._raw_config(
                    directory,
                    [
                        {
                            "id": "openrouter/vision-model",
                            "provider": "openrouter",
                            "input_modalities": ["text", "image"],
                        }
                    ],
                )
            )

            entry = build_catalog(config)["models"][0]
            self.assertEqual(entry["input_modalities"], ["text", "image"])

    def test_absent_modality_metadata_remains_text_only(self):
        with tempfile.TemporaryDirectory() as directory:
            state, path = self._state(Path(directory))
            discovered = [{"upstream_id": "text-model", "display_name": "Text model"}]
            with patch(
                "easy_multi_provider.server.discover_models",
                return_value=discovered,
            ), patch(
                "easy_multi_provider.server.generated_catalog_path",
                return_value=Path(directory) / "catalog.json",
            ):
                state.discover_provider_models("openrouter", ["text-model"])

            persisted = load(path)
            self.assertEqual(persisted["models"][0].get("input_modalities"), ["text"])
            self.assertEqual(
                build_catalog(persisted)["models"][0]["input_modalities"], ["text"]
            )

    def test_responses_to_chat_preserves_mixed_text_and_image_urls(self):
        data_url = "data:image/png;base64,ZmFrZS1pbWFnZQ=="
        payload = responses_to_chat(
            {
                "model": "openrouter/vision-model",
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_text", "text": "before"},
                            {
                                "type": "input_image",
                                "image_url": "https://images.example.test/asset.png",
                            },
                            {"type": "input_text", "text": "middle"},
                            {"type": "input_image", "image_url": data_url},
                            {"type": "input_text", "text": "after"},
                        ],
                    }
                ],
            },
            "vision-model",
        )

        self.assertEqual(
            payload["messages"][0]["content"],
            [
                {"type": "text", "text": "before"},
                {
                    "type": "image_url",
                    "image_url": {"url": "https://images.example.test/asset.png"},
                },
                {"type": "text", "text": "middle"},
                {"type": "image_url", "image_url": {"url": data_url}},
                {"type": "text", "text": "after"},
            ],
        )

    def test_direct_responses_forwarding_preserves_input_image(self):
        data_url = "data:image/png;base64,ZmFrZS1pbWFnZQ=="
        body = {
            "model": "openrouter/vision-model",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_image", "image_url": data_url}],
                }
            ],
        }
        model = {"id": "openrouter/vision-model", "upstream_id": "vision-model"}

        with patch.object(
            router,
            "_request",
            return_value=_JsonResponse({"status": "completed", "output": []}),
        ) as request:
            forward_responses(self._provider(), body, model, {})

        sent = request.call_args.args[1]
        self.assertEqual(sent["input"][0]["content"][0]["type"], "input_image")
        self.assertEqual(sent["input"][0]["content"][0]["image_url"], data_url)

    def test_save_load_and_catalog_refresh_keep_multimodal_models(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    self._raw_config(
                        root,
                        [
                            {
                                "id": "openrouter/vision-model",
                                "provider": "openrouter",
                                "input_modalities": ["text", "image"],
                            }
                        ],
                    )
                ),
                config_path,
            )

            loaded = load(config_path)
            catalog_path = root / "catalog.json"
            write_catalog(loaded, catalog_path)
            catalog = json.loads(catalog_path.read_text(encoding="utf-8"))

            self.assertEqual(
                loaded["models"][0].get("input_modalities"), ["text", "image"]
            )
            self.assertEqual(catalog["models"][0]["input_modalities"], ["text", "image"])

    def test_internal_video_modality_is_retained_but_filtered_from_catalog(self):
        with tempfile.TemporaryDirectory() as directory:
            config = normalize(
                self._raw_config(
                    directory,
                    [
                        {
                            "id": "openrouter/video-model",
                            "provider": "openrouter",
                            "input_modalities": ["text", "image", "video"],
                        }
                    ],
                )
            )

            self.assertEqual(
                config["models"][0].get("input_modalities"), ["text", "image", "video"]
            )
            self.assertEqual(
                build_catalog(config)["models"][0]["input_modalities"], ["text", "image"]
            )

    def test_image_detail_original_is_independent_from_input_modalities(self):
        with tempfile.TemporaryDirectory() as directory:
            config = normalize(
                self._raw_config(
                    directory,
                    [
                        {
                            "id": "openrouter/vision-default",
                            "provider": "openrouter",
                            "input_modalities": ["text", "image"],
                            "supports_image_detail_original": False,
                        },
                        {
                            "id": "openrouter/vision-detailed",
                            "provider": "openrouter",
                            "input_modalities": ["text", "image"],
                            "supports_image_detail_original": True,
                        },
                    ],
                )
            )

            entries = build_catalog(config)["models"]
            self.assertEqual(
                [entry["supports_image_detail_original"] for entry in entries],
                [False, True],
            )

    def test_manual_modalities_context_and_visibility_survive_provider_refresh(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state, path = self._state(
                root,
                [
                    {
                        "id": "openrouter/vision-model",
                        "provider": "openrouter",
                        "input_modalities": ["text", "image"],
                        "context_window": 77777,
                        "visibility": "hide",
                        "capability_sources": {
                            "context_window": {
                                "source": "manual",
                                "confidence": 1.0,
                                "observed_at": "2026-08-22T00:00:00+00:00",
                            },
                            "input_modalities": {
                                "source": "manual",
                                "confidence": 1.0,
                                "observed_at": "2026-08-22T00:00:00+00:00",
                            },
                        },
                    }
                ],
            )
            discovered = [
                {
                    "upstream_id": "vision-model",
                    "input_modalities": ["text"],
                    "context_window": 4096,
                }
            ]
            with patch(
                "easy_multi_provider.server.discover_models",
                return_value=discovered,
            ), patch(
                "easy_multi_provider.server.generated_catalog_path",
                return_value=root / "catalog.json",
            ):
                state.discover_provider_models("openrouter", ["vision-model"])

            model = load(path)["models"][0]
            self.assertEqual(model.get("input_modalities"), ["text", "image"])
            self.assertEqual(model["context_window"], 77777)
            self.assertEqual(model.get("visibility"), "hide")

    def test_malformed_or_oversized_modalities_fall_back_to_text_only(self):
        models = self._discover(
            {
                "data": [
                    {
                        "id": "malformed-model",
                        "architecture": {"input_modalities": "text+image"},
                    },
                    {
                        "id": "oversized-model",
                        "architecture": {
                            "input_modalities": ["text", "x" * 5000]
                        },
                    },
                ]
            }
        )

        self.assertEqual(
            [item.get("input_modalities") for item in models], [["text"], ["text"]]
        )


if __name__ == "__main__":
    unittest.main()
