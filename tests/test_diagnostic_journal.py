import contextlib
import io
import json
import ntpath
import os
import posixpath
import re
import stat
import tempfile
import threading
import unittest

from easy_multi_provider.diagnostic_journal import (
    DiagnosticJournal,
    NullJournal,
    _absolute_path_prefixes,
    _is_forbidden_field_key,
    create_journal,
)


def read_records(paths):
    records = []
    for path in paths:
        with open(path, "r", encoding="utf-8") as handle:
            for line in handle:
                line = line.strip()
                if line:
                    records.append(json.loads(line))
    return records


def write_matching_part(logs_dir, name, size, mtime_ns):
    path = os.path.join(logs_dir, name)
    with open(path, "wb") as handle:
        handle.write(b"x" * size)
    os.utime(path, ns=(mtime_ns, mtime_ns))
    return path


class DiagnosticJournalTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = tmp.name
        self.config_dir = os.path.join(self.tmp, "config")

    def make_journal(self, part_bytes=4096, dir_bytes=16384, config_dir=None):
        journal = create_journal(
            config_dir or self.config_dir,
            max_part_bytes=part_bytes,
            max_dir_bytes=dir_bytes,
        )
        self.addCleanup(journal.close)
        return journal

    def log_paths(self, config_dir=None):
        root = os.path.join(config_dir or self.config_dir, "state", "logs")
        return sorted(os.path.join(root, name) for name in os.listdir(root))

    def test_null_journal_is_safe_noop(self):
        journal = NullJournal(run_id="fixed")
        journal.event("info", "noop", request="body")
        journal.exception_event("error", "err", "stage", ValueError("x"))
        journal.close()
        self.assertFalse(journal.enabled)
        self.assertIsNone(journal.current_path)
        self.assertEqual(journal.run_id, "fixed")
        with journal as ctx:
            self.assertIs(ctx, journal)

    def test_path_prefixes_preserve_windows_drive_and_posix_root(self):
        self.assertEqual(
            _absolute_path_prefixes(
                r"C:\Users\EMP\state\logs", path_module=ntpath
            ),
            (
                "C:\\",
                r"C:\Users",
                r"C:\Users\EMP",
                r"C:\Users\EMP\state",
                r"C:\Users\EMP\state\logs",
            ),
        )
        self.assertEqual(
            _absolute_path_prefixes("/var/lib/emp/logs", path_module=posixpath),
            ("/", "/var", "/var/lib", "/var/lib/emp", "/var/lib/emp/logs"),
        )

    def test_forbidden_key_rules_distinguish_secrets_from_metrics(self):
        forbidden = (
            "x-api-key",
            "proxy-authorization",
            "client-secret",
            "oauth-access-token",
            "refreshToken",
            "setCookie",
        )
        allowed = (
            "estimated_tokens",
            "max_output_tokens",
            "duration_ms",
            "model_count",
            "token_count",
        )
        for key in forbidden:
            with self.subTest(key=key):
                self.assertTrue(_is_forbidden_field_key(key))
        for key in allowed:
            with self.subTest(key=key):
                self.assertFalse(_is_forbidden_field_key(key))

    def test_jsonl_validity_and_required_fields(self):
        journal = self.make_journal(part_bytes=65536, dir_bytes=1048576)
        self.assertTrue(journal.enabled)
        journal.event("debug", "alpha", count=3, note="hello")
        journal.event("warning", "beta", nested={"ok": [1, 2]})
        records = read_records(self.log_paths())
        self.assertEqual(len(records), 2)
        for index, record in enumerate(records):
            self.assertEqual(record["run_id"], journal.run_id)
            self.assertTrue(record["timestamp"].endswith("Z"))
            self.assertIn(record["level"], {"debug", "info", "warning", "error"})
            self.assertEqual(record["sequence"], index + 1)

    def test_concurrent_writes_are_valid_and_ordered(self):
        journal = self.make_journal(part_bytes=1048576, dir_bytes=8388608)
        errors = []

        def worker(worker_id):
            try:
                for i in range(25):
                    journal.event("info", "concurrent", worker=worker_id, i=i)
            except Exception as exc:
                errors.append(exc)

        threads = [threading.Thread(target=worker, args=(n,)) for n in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertFalse(errors)
        records = read_records(self.log_paths())
        self.assertEqual(len(records), 200)
        self.assertEqual([r["sequence"] for r in records], list(range(1, 201)))

    def test_rotation_and_aggregate_prune_keep_active_data(self):
        journal = self.make_journal(part_bytes=300, dir_bytes=1200)
        for i in range(80):
            journal.event("info", "rotate", index=i, filler="x" * 40)
        paths = self.log_paths()
        self.assertGreaterEqual(len(paths), 2)
        total_size = sum(os.path.getsize(path) for path in paths)
        self.assertLessEqual(total_size, 1200)
        newest = read_records([journal.current_path])
        self.assertTrue(newest)
        sequences = [r["sequence"] for r in read_records(paths)]
        self.assertEqual(max(sequences), max(r["sequence"] for r in newest))

    def test_post_write_budget_crossing_prunes_oldest_across_runs(self):
        logs_dir = os.path.join(self.config_dir, "state", "logs")
        os.makedirs(logs_dir)
        now_ns = 2_000_000_000_000_000_000
        older = write_matching_part(
            logs_dir,
            "emp-20260101T000000Z-1-1111111111111111-p001.jsonl",
            300,
            now_ns - 20,
        )
        newer = write_matching_part(
            logs_dir,
            "emp-20260101T000001Z-1-2222222222222222-p999.jsonl",
            300,
            now_ns - 10,
        )
        journal = self.make_journal(part_bytes=4096, dir_bytes=900)
        self.assertTrue(os.path.exists(older))
        self.assertTrue(os.path.exists(newer))

        journal.event("info", "cross-budget", filler="z" * 300)

        self.assertFalse(os.path.exists(older))
        self.assertTrue(os.path.exists(newer))
        managed = [
            path for path in self.log_paths()
            if os.path.isfile(path) and not os.path.islink(path)
        ]
        self.assertLessEqual(sum(os.path.getsize(path) for path in managed), 900)

    def test_prune_age_ties_use_stable_filename_order(self):
        logs_dir = os.path.join(self.config_dir, "state", "logs")
        os.makedirs(logs_dir)
        tied_ns = 1_000_000_000
        first = write_matching_part(
            logs_dir,
            "emp-20260101T000000Z-1-1111111111111111-p001.jsonl",
            300,
            tied_ns,
        )
        second = write_matching_part(
            logs_dir,
            "emp-20260101T000000Z-1-2222222222222222-p001.jsonl",
            300,
            tied_ns,
        )
        journal = self.make_journal(part_bytes=4096, dir_bytes=900)
        journal.event("info", "cross-budget", filler="z" * 300)
        self.assertFalse(os.path.exists(first))
        self.assertTrue(os.path.exists(second))

    def test_active_part_is_only_allowed_budget_overage(self):
        journal = self.make_journal(part_bytes=65536, dir_bytes=128)
        journal.event("info", "larger-than-directory-budget", filler="x" * 300)
        paths = self.log_paths()
        self.assertEqual(paths, [journal.current_path])
        self.assertGreater(os.path.getsize(journal.current_path), 128)

    def test_under_budget_writes_do_not_rescan_directory(self):
        journal = self.make_journal(part_bytes=65536, dir_bytes=1048576)
        original_prune = journal._prune_locked
        calls = []

        def counted_prune():
            calls.append(True)
            return original_prune()

        journal._prune_locked = counted_prune
        for i in range(20):
            journal.event("info", "under-budget", index=i)
        self.assertEqual(calls, [])

    def test_prune_never_touches_unrelated_files_or_symlinks(self):
        journal = self.make_journal(part_bytes=250, dir_bytes=1000)
        logs_dir = os.path.join(self.config_dir, "state", "logs")
        unrelated = os.path.join(logs_dir, "keep-me.jsonl")
        with open(unrelated, "w", encoding="utf-8") as handle:
            handle.write("{}\n")
        outside = os.path.join(self.tmp, "outside.jsonl")
        with open(outside, "w", encoding="utf-8") as handle:
            handle.write("outside\n")
        linked = os.path.join(
            logs_dir,
            "emp-20200101T000000Z-1-aaaaaaaaaaaaaaaa-p001.jsonl",
        )
        os.symlink(outside, linked)
        matching_dir = os.path.join(
            logs_dir,
            "emp-20200101T000000Z-1-bbbbbbbbbbbbbbbb-p001.jsonl",
        )
        os.mkdir(matching_dir)
        for i in range(70):
            journal.event("info", "prune", index=i, filler="y" * 50)
        self.assertTrue(os.path.exists(unrelated))
        self.assertTrue(os.path.islink(linked))
        self.assertTrue(os.path.isdir(matching_dir))
        with open(outside, "r", encoding="utf-8") as handle:
            self.assertEqual(handle.read(), "outside\n")

    def test_posix_modes_directory_and_file(self):
        journal = self.make_journal()
        journal.event("info", "mode-check")
        logs_dir = os.path.dirname(journal.current_path)
        self.assertEqual(stat.S_IMODE(os.stat(logs_dir).st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(os.stat(journal.current_path).st_mode), 0o600)

    def test_symlinked_log_directory_disables_journal(self):
        state_dir = os.path.join(self.config_dir, "state")
        os.makedirs(state_dir)
        outside = os.path.join(self.tmp, "outside-dir")
        os.makedirs(outside)
        os.symlink(outside, os.path.join(state_dir, "logs"))
        journal = create_journal(self.config_dir)
        self.addCleanup(journal.close)
        self.assertFalse(journal.enabled)

    def test_symlinked_parent_directory_disables_journal(self):
        outside = os.path.join(self.tmp, "outside-state")
        os.makedirs(outside)
        os.makedirs(self.config_dir)
        os.symlink(outside, os.path.join(self.config_dir, "state"))
        journal = create_journal(self.config_dir)
        self.addCleanup(journal.close)
        self.assertFalse(journal.enabled)
        self.assertFalse(os.path.exists(os.path.join(outside, "logs")))

    def test_forbidden_fields_dropped_and_strings_redacted(self):
        journal = self.make_journal(part_bytes=65536, dir_bytes=1048576)
        journal.event(
            "info",
            "safe-event",
            authorization="Bearer abc.def.ghi",
            api_key="sk-abcdefghijklmnop1234",
            cookie="session=supersecretvalue",
            password="hunter2222",
            bootstrap="boot-secret-token-value",
            headers={"x": 1},
            body=b"raw-bytes",
            prompt="user prompt text",
            content="assistant text",
            input="tool input text",
            output="tool output text",
            request="http request body",
            response="http response body",
            token="tok-1234567890abcdef",
            session="sess-1234567890",
            estimated_tokens=123,
            max_output_tokens=456,
            duration_ms=45,
            model_count=7,
            token_count=9,
            note=(
                "Bearer zzz111yyy222xxx333www444 "
                "sk-proj-OpenAISecret1234567890 "
                "AIzaSyGoogleSecretKey123456789012345 "
                "ghp_GitHubSecretToken123456789012345 "
                "access_token=value-secret-1234567890"
            ),
            nested={"Authorization": "Bearer qqq111www222eee333rrr444", "count": 2},
            **{
                "api-key": "hyphen-api-secret",
                "apiKey": "camel-api-secret",
                "APIKey": "acronym-api-secret",
                "access_token": "access-secret",
                "refresh-token": "refresh-secret",
                "set-cookie": "set-cookie-secret",
                "authPayload": {"value": "auth-payload-secret"},
                "authenticationPayload": "authentication-payload-secret",
                "credential-data": "credential-data-secret",
                "x-api-key": "prefixed-api-secret",
                "proxy-authorization": "proxy-auth-secret",
                "client-secret": "client-secret-value",
                "oauth-access-token": "oauth-token-secret",
            },
        )
        raw_parts = []
        for path in self.log_paths():
            with open(path, "rb") as handle:
                raw_parts.append(handle.read())
        raw = b"".join(raw_parts).lower()
        for needle in [
            b"abc.def.ghi",
            b"sk-abcdefghijklmnop1234",
            b"supersecretvalue",
            b"hunter2222",
            b"user prompt text",
            b"http request body",
            b"tok-1234567890abcdef",
            b"zzz111yyy222xxx333www444",
            b"sk-shortenough12345",
            b"qqq111www222eee333rrr444",
            b"openaisecret1234567890",
            b"aizasygooglesecretkey123456789012345",
            b"githubsecrettoken123456789012345",
            b"value-secret-1234567890",
            b"hyphen-api-secret",
            b"camel-api-secret",
            b"acronym-api-secret",
            b"access-secret",
            b"refresh-secret",
            b"set-cookie-secret",
            b"auth-payload-secret",
            b"authentication-payload-secret",
            b"credential-data-secret",
            b"prefixed-api-secret",
            b"proxy-auth-secret",
            b"client-secret-value",
            b"oauth-token-secret",
        ]:
            self.assertNotIn(needle, raw)
        fields = read_records(self.log_paths())[0]["fields"]
        for forbidden in [
            "authorization", "api_key", "cookie", "password", "bootstrap",
            "headers", "body", "prompt", "content", "input", "output",
            "request", "response", "token", "session",
            "api-key", "apiKey", "APIKey", "access_token", "refresh-token",
            "set-cookie", "authPayload", "authenticationPayload",
            "credential-data", "x-api-key", "proxy-authorization",
            "client-secret", "oauth-access-token",
        ]:
            self.assertNotIn(forbidden, fields)
        self.assertEqual(fields["estimated_tokens"], 123)
        self.assertEqual(fields["max_output_tokens"], 456)
        self.assertEqual(fields["duration_ms"], 45)
        self.assertEqual(fields["model_count"], 7)
        self.assertEqual(fields["token_count"], 9)

    def test_oversize_fields_and_records_stay_bounded_jsonl(self):
        journal = self.make_journal(part_bytes=1048576, dir_bytes=8388608)
        huge_list = [{"data": "z" * 500} for _ in range(400)]
        journal.event("info", "oversize", blob="q" * 60000, rows=huge_list)
        with open(journal.current_path, "rb") as handle:
            lines = handle.read().splitlines()
        self.assertEqual(len(lines), 1)
        self.assertLessEqual(len(lines[0]), 16384)
        record = json.loads(lines[0].decode("utf-8"))
        self.assertEqual(record["event"], "oversize")
        self.assertTrue(record["fields"].get("_dropped"))

    def test_sequence_injection_keeps_final_json_line_within_limit(self):
        journal = self.make_journal(part_bytes=1048576, dir_bytes=8388608)
        boundary_record = {
            "timestamp": "2026-01-01T00:00:00Z",
            "sequence": None,
            "run_id": journal.run_id,
            "level": "info",
            "event": "sequence-boundary",
            "fields": {"padding": ""},
        }
        encoded = json.dumps(
            boundary_record, ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")
        boundary_record["fields"]["padding"] = "x" * (16384 - len(encoded))
        encoded = json.dumps(
            boundary_record, ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")
        self.assertEqual(len(encoded), 16384)
        journal._sequence = 10 ** 30
        journal._fit_record = lambda *args, **kwargs: encoded

        journal.event("info", "ignored-by-test")

        with open(journal.current_path, "rb") as handle:
            lines = handle.read().splitlines()
        self.assertEqual(len(lines), 1)
        self.assertLessEqual(len(lines[0]), 16384)
        record = json.loads(lines[0].decode("utf-8"))
        self.assertEqual(record["sequence"], 10 ** 30 + 1)
        self.assertEqual(record["fields"], {"_dropped": True})

    def test_pseudonym_salted_by_run_and_stable_within_run(self):
        first = self.make_journal(config_dir=os.path.join(self.tmp, "c1"))
        second = self.make_journal(config_dir=os.path.join(self.tmp, "c2"))
        self.assertNotEqual(first.run_id, second.run_id)
        self.assertNotEqual(
            first.pseudonym("account@example.com"),
            second.pseudonym("account@example.com"),
        )
        self.assertEqual(
            first.pseudonym("account@example.com"),
            first.pseudonym("account@example.com"),
        )

    def test_exception_event_only_class_and_frames(self):
        journal = self.make_journal(part_bytes=65536, dir_bytes=1048576)
        secret_local = "do-not-store"
        try:
            raise ValueError("Bearer exception-message-secret-1234567890")
        except ValueError as exc:
            journal.exception_event("error", "boom", "routing", exc)
        del secret_local
        fields = read_records(self.log_paths())[0]["fields"]
        self.assertEqual(fields["exception_class"], "ValueError")
        self.assertEqual(fields["stage"], "routing")
        self.assertTrue(fields["frames"])
        serialized = json.dumps(fields)
        self.assertNotIn("do-not-store", serialized)
        self.assertNotIn("secret_local", serialized)
        self.assertNotIn("exception-message-secret", serialized)
        for frame in fields["frames"]:
            self.assertNotIn("/", frame["file"])
            self.assertFalse(re.search(r"[A-Za-z]", str(frame.get("line"))))

    def test_formatting_failure_drops_record_without_raising(self):
        journal = self.make_journal(part_bytes=65536, dir_bytes=1048576)
        original_fit = journal._fit_record

        def broken_fit(*args, **kwargs):
            raise RuntimeError("formatting exploded")

        journal._fit_record = broken_fit
        journal.event("info", "will-fail", x=1)
        self.assertTrue(journal.enabled)
        self.assertEqual(read_records(self.log_paths()), [])
        journal._fit_record = original_fit
        journal.event("info", "after-failure", x=2)
        self.assertEqual(read_records(self.log_paths())[-1]["event"], "after-failure")

    def test_io_write_failure_disables_without_raising(self):
        journal = DiagnosticJournal(
            os.path.join(self.tmp, "io-state", "logs"),
            run_id="0123456789abcdef",
        )
        self.assertTrue(journal.open())

        class FailingWriter:
            def write(self, data):
                raise OSError("disk gone with secret-value")

            def flush(self):
                pass

            def close(self):
                pass

        journal._handle.close()
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            journal._handle = FailingWriter()
            journal.event("info", "io-fail", x=1)
            journal.event("info", "io-fail-again", x=2)
        self.assertFalse(journal.enabled)
        warning = stderr.getvalue()
        self.assertEqual(len(warning.strip().splitlines()), 1)
        self.assertNotIn(self.tmp, warning)
        self.assertNotIn("secret-value", warning)
        journal.close()

    def test_setup_failure_returns_null_journal(self):
        blocker = os.path.join(self.tmp, "blocker")
        os.makedirs(blocker)
        with open(os.path.join(blocker, "state"), "w", encoding="utf-8") as handle:
            handle.write("not a directory\n")
        journal = create_journal(blocker)
        self.addCleanup(journal.close)
        self.assertFalse(journal.enabled)
        self.assertIsInstance(journal, NullJournal)
        journal.event("info", "never-written")

    def test_context_manager_closes_cleanly(self):
        journal = self.make_journal(part_bytes=65536, dir_bytes=1048576)
        with journal as ctx:
            self.assertIs(ctx, journal)
            ctx.event("info", "inside-context")
        self.assertFalse(journal.enabled)


if __name__ == "__main__":
    unittest.main()
