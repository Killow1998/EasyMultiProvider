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

The created-history flow runs a disposable EMP service and real Codex app-server
clients. Codex invokes an MCP fixture, completes external compaction through EMP,
and creates forks before and after compaction, including a grandchild. It closes
before native settings are restored. An opaque reasoning probe is appended while
the original timestamped rollout paths stay unchanged. All provider IDs, keys,
accounts and endpoints are synthetic; the `OpenAI` display label selects Codex
0.158's Responses remote-compaction V2, while traffic remains on loopback.

The harness stops EMP by operating-system signal, retaining its fast
configuration-only exit contract. It then runs `EMP migrate-history --thread-id
ID --json` for explicitly selected closed conversations before offline Restore.
A selected active writer is rejected; unrelated writers do not block that
explicit selection. Offline Restore now also converts eligible idle history;
after these targeted migrations it should be a history no-op. The separate
[browser lifecycle acceptance](../shadow_frontend/README.md) checks automatic
conversion through Restore Native and normal Quit, including deferred writers.

This harness resumes the same thread IDs against a fake native endpoint and
checks the actual tool pair, summary, opaque reasoning, fork cutoffs, exact
native settings, byte-exact backups, retained original prefixes and an unchanged
second restore. A configuration no-op is accepted only after the saved native
configuration is verified.

Run the old-history restore contract:

```sh
python3 -B -m scripts.native_restore_acceptance \
  --codex-bin /path/to/codex \
  --emp-bin /path/to/EMP \
  --root /mnt/test-data/emp-acceptance/existing-run-unique \
  --large-history
```

`--large-history` incrementally appends more than 128 MiB of historical `event_msg/token_count` rows to a disposable paginated rollout after its bounded forks are created. Point `--root` at a new directory on a disk filesystem with enough free space; do not use tmpfs for this scenario. These rows do not enlarge model input. The selected migration, restore and Codex resume processes run under the same memory limit; backup verification streams the files instead of loading them into memory. Omit `--large-history` for the smaller three-thread differential scenario.

The supplied versions and executable hashes are recorded rather than pinned to one historical release. Disposable app-server clients disable plugins. The current created-history contract passed with official Linux Codex 0.154.0, 0.155.1, 0.156.1, 0.157.1, 0.158.0, 0.159.3, 0.160.1, 0.161.0, 0.162.1 and 0.163.0 against EMP 0.13.4 source on 2026-10-16 UTC. The earlier restore-time migration results remain dated separately in [compatibility results](../../docs/codex-compatibility.md).

## Exploratory background compaction

This optional manual harness is retained to reproduce earlier exploratory
results. It is outside routine maintenance and release acceptance. Defer
additional async-specific work until upstream marks the feature stable, then
reassess its place in the maintained suite; a stable CLI release alone is not
that milestone.

Python 3.14 or newer supplies `compression.zstd` for this additional native
upstream fixture. Run with an official engine supporting `async_compaction`:

```sh
python3 -B -m scripts.native_restore_acceptance.async_compaction \
  --codex-bin /path/to/codex \
  --emp-bin /path/to/EMP \
  --root /mnt/test-data/emp-acceptance/async-run-unique
```

The four cases cover disabled control, pending-result adoption without a second
compaction request, preservation of a later turn while an older result is
discarded, and shutdown during a pending background request. Gates synchronize
against actual upstream requests; explicit compaction finishes before the next
model turn starts. Every Codex process must exit normally. The fixture uses
synthetic native credentials and an explicit V2-capable test provider, requests
EMP's generated model catalog, and handles streamed `compaction_trigger` items
as well as the legacy compact endpoint. Reports record engine version and
binary hashes. It does not change the installed engine or enable the feature
in a user's configuration.

Keep large downloaded engines and build temporary files on disk rather than
tmpfs. Download one official patch per series, verify its published asset
digest, and reuse the existing contract with a fresh root for every engine.

Use a new or empty root for every run. Never point the harness at a live Codex home or the user's EMP service. The fake upstream is not evidence that real OpenAI accepts or emits EMP-specific ciphertext.
