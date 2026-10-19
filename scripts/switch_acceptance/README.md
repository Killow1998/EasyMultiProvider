# Same-thread model-switch acceptance

This manual Linux E2E uses real configured services and subscription quota.
Run it only with authorization to make real model calls. It requires Python 3,
`cryptography`, util-linux `prlimit`, and supplied EMP and Codex executables.
The current aliases are Opus `claude-cpa/claude-opus-5-5`, Chuang Gemma
`chuang/gemma-3.1`, and Native GPT `native-test/gpt-6.1-sol`.

```sh
python3 -B -m scripts.switch_acceptance --allow-real-model \
  --emp-bin /path/to/EMP --codex-bin /path/to/codex \
  --config /path/to/easy-multi-provider/config.json \
  --native-auth /path/to/codex/auth.json \
  --root /path/to/new-private-run
```

The root must not exist. Each service gets one persistent parent conversation:
external → Native → external, external spawning a Native child, switching both
ways after completion, Native spawning an external child, and switching back.
Assertions check remembered synthetic codes, actual child model and completed
output, parent results, error events, and the EMP call ledger. A parent's claim
alone cannot pass a child check. There are 16 parent turns and four child agents.
`--external ALIAS` selects one service; `--without-subagents` runs three-turn
continuity smoke checks instead.

The harness reuses the existing native-restore app-server/process owners. It
starts disposable EMP/Codex processes on a random loopback port with separate
homes, read-only project sandbox, plugins disabled and On-demand recovery. Only
an access token is supplied to Codex; the source refresh token is never copied.
Selected provider keys are re-encrypted into a fresh private store. Production
credentials and selected provider/model records are checked for changes; unrelated
account/quota updates by a running EMP may change the source config's whole-file
hash, reported separately. The live service and user sessions are not restarted.

`ROOT/result.json` contains per-turn assertions, actual upstream model labels,
binary hashes and summarized call outcomes. A zero-attempt
`previous_response_not_found` causes Codex to replay full history after a route
change and is counted as a history replay. Other failed calls fail acceptance.
Recovery calls are counted separately. Failure details and session files stay
private under the root; do not publish that directory.

See [verified scope and ablations](../../docs/claude-switch-failures.md#real-model-switch-acceptance-and-modularization-2026-10-19-utc).
