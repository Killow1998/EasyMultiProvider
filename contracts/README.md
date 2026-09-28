# EMP frozen behavior contracts

These fixtures freeze observable EMP behavior for the Rust backend. Each case
identifies the Rust test or documented boundary that enforces it, the baseline
revision, the transport and dialect, permitted normalization, and expected side
effects. The Python backend is frozen and its behavior lives on only in these
fixture files, which use the historical `python-` file-name prefix.

Contract levels are independent:

- `bytes`: exact assets, pass-through payloads, framing and hash inputs;
- `events`: ordered response, reasoning, tool, usage and terminal events;
- `api`: HTTP/WebSocket methods, status, headers, JSON and state transitions;
- `state`: compatible files, databases, locks, permissions and rollback.

Rust tests load the frozen fixtures directly; nothing at test time spawns a
Python process or reads a live Python checkout. A `consumer` role still
validates observable results independently with the unchanged browser, a raw
protocol client, or the pinned Codex CLI. Fixtures use local deterministic
upstreams and synthetic credentials. They must not call real generation,
refresh credentials, consume a quota reset, or install an update.

Exact equality is not required for TCP/TLS segmentation, fresh ciphertext,
compressed bytes, timestamps, or random identifiers. A case that normalizes a
field must declare the rule and still verify relationships, decoded content,
limits and side effects.

`manifest.json` is the initial inventory. New fixture directories may extend it
without changing the schema. A fixture is accepted only after Point reviews the
source behavior and any normalization.
