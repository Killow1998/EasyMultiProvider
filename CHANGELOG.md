# Changelog

## 0.2.0 (Unreleased)

- Added encrypted `.emp` migration bundles for moving configuration, model
  routes, Provider keys, and Codex subscription credentials between machines.
- Imported credentials are re-encrypted with the destination machine's local
  master key; the migration bundle never contains the local master key.

## 0.1.0

- Added local Web UI management for encrypted Codex subscription accounts,
  API providers, model discovery, routing, and quota snapshots.
- Added Codex profile generation with one EMP model catalog and isolated EMP
  sessions.
- Added Responses, Chat Completions, and Anthropic Messages upstream routing.
- Added proxy environment detection and a real Codex CLI demo model test.
- Published as a Linux-validated MVP; ChatGPT App and other platforms remain
  manual acceptance items.
