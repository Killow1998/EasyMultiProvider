# Native restore acceptance harness

This Linux manual acceptance harness uses Python 3, an explicitly supplied Codex CLI and EMP binary, fresh disposable roots on disk, and loopback fake Responses providers. It requires `/usr/bin/prlimit` from util-linux and `/proc/<pid>/status` for process memory limits and RSS sampling. It uses synthetic auth only; it does not validate production-provider ciphertext or contact external services. Every Codex and EMP process is started under a 1536 MiB address space limit. Reports contain executable hashes, peak RSS samples, summarized HTTP observations, and assertion results in `ROOT/result.json`. RSS fields use `null` when no positive `/proc` sample was captured; memory-bound assertions require a positive measurement and do not treat missing data as a pass.

Run the history-created-through-EMP contract:

```sh
python3 -B -m scripts.native_restore_acceptance \
  --contract created \
  --codex-bin /path/to/codex \
  --emp-bin /path/to/EMP \
  --root /mnt/test-data/emp-acceptance/created-run-unique
```

The created-history flow runs a disposable EMP service and real Codex app-server clients. Codex invokes a real MCP fixture tool, completes an external compaction through EMP, creates a fork before compaction and a fork/grandchild after compaction, then closes before native settings are restored. The harness appends an opaque reasoning probe to each original rollout while leaving the Codex database paths unchanged. EMP resolves the timestamped rollout paths directly; the harness does not create alternate UUID-named lineage files. Provider IDs, account labels, keys, and endpoints are fixture-only; the `OpenAI` display label is the Codex 0.158 compatibility selector for Responses remote-compaction V2, while the configured endpoint remains loopback. A graceful EMP shutdown may already restore native settings; the following restore command may report a no-op only if the exact saved native configuration was verified after shutdown. Backup and rollout-content assertions remain required. It resumes the same thread IDs against a local fake native endpoint. Assertions check the upstream fixture requests, actual tool input/result pair, portable summary, opaque reasoning fixture, fork cutoffs, exact native config restoration, pre-restore backups and byte prefixes, and that a second unchanged restore performs no writes.

Run the old-history restore contract:

```sh
python3 -B -m scripts.native_restore_acceptance \
  --codex-bin /path/to/codex \
  --emp-bin /path/to/EMP \
  --root /mnt/test-data/emp-acceptance/existing-run-unique \
  --large-history
```

`--large-history` incrementally appends more than 128 MiB of historical `event_msg/token_count` rows to a disposable paginated rollout after its bounded forks are created. Point `--root` at a new directory on a disk filesystem with enough free space; do not use tmpfs for this scenario. These rows do not enlarge model input. The restore and Codex resume processes run under the same memory limit; backup verification streams the files instead of loading them into memory. Omit `--large-history` for the smaller three-thread differential scenario.

The supplied versions and executable hashes are recorded rather than pinned to one historical release. Disposable app-server clients disable plugins. The created contract passed with one official Linux Codex patch from each series 0.153–0.162 (listed in [compatibility results](../../docs/codex-compatibility.md)) against the v0.12.14 development binary on 2026-10-09; this validates these fixture paths, not every platform or real provider.

Use a new or empty root for every run. Never point the harness at a live Codex home or the user's EMP service. The fake upstream is not evidence that real OpenAI accepts or emits EMP-specific ciphertext.
