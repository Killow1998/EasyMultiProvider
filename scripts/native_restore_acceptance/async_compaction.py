"""Isolated real-engine async-compaction probe with a gated native upstream."""
import argparse
from compression import zstd
import contextlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import os
import subprocess
import threading
import urllib.request

from .app_server import AppServer
from .common import sha256_file
from .emp_server import EmpServer
from .fake_provider import FakeResponsesProvider
from .fixture import make_env, run_turn


class GatedNativeUpstream:
    def __init__(self):
        self.requests = []
        self.compacting = threading.Event()
        self.release = threading.Event()
        self.sent = threading.Event()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                raw = self.rfile.read(int(self.headers['Content-Length']))
                if self.headers.get('Content-Encoding') == 'zstd':
                    raw = zstd.decompress(raw)
                body = json.loads(raw)
                compact = self.path.endswith('/compact') or any(item.get('type') == 'compaction_trigger' for item in body.get('input', []) if isinstance(item, dict))
                record = {'path': self.path, 'compact': compact, 'body': body, 'turn_state': self.headers.get('x-codex-turn-state')}
                owner.requests.append(record)
                sequence = len(owner.requests)
                if compact:
                    owner.compacting.set()
                    if not owner.release.wait(30):
                        record['gate_timeout'] = True
                    response = {'id': f'resp_compact_{sequence}', 'object': 'response', 'created_at': 1,
                        'model': body.get('model', 'gpt-5'), 'status': 'completed',
                        'output': [{'id': f'cmp_{sequence}', 'type': 'compaction',
                            'encrypted_content': f'OPAQUE_ASYNC_FIXTURE_{sequence}'}],
                        'usage': {'input_tokens': 7, 'output_tokens': 3, 'total_tokens': 10}}
                    payload = FakeResponsesProvider.sse(response) if body.get('stream') else json.dumps(response).encode()
                    content_type = 'text/event-stream' if body.get('stream') else 'application/json'
                else:
                    response = FakeResponsesProvider.response(body, sequence, 'setup')
                    total = 70000 if sequence == 1 else 2
                    response['usage'] = {'input_tokens': total - 1, 'output_tokens': 1, 'total_tokens': total}
                    payload = FakeResponsesProvider.sse(response)
                    content_type = 'text/event-stream'
                self.send_response(200)
                self.send_header('Content-Type', content_type)
                self.send_header('Content-Length', str(len(payload)))
                self.send_header('X-Codex-Turn-State', 'async-fixture-pin')
                self.end_headers()
                try:
                    self.wfile.write(payload)
                    self.wfile.flush()
                    record['sent'] = True
                except (BrokenPipeError, ConnectionResetError):
                    record['disconnected'] = True
                if compact:
                    owner.sent.set()

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f'http://127.0.0.1:{self.server.server_port}/v1'

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def run_case(root, codex, emp, mode):
    root.mkdir(parents=True)
    for name in ['home', 'codex', 'tmp', 'workspace']:
        (root / name).mkdir()
    env = make_env(root / 'home', root / 'codex', root / 'tmp', 'synthetic-async-key')
    (root / 'codex/auth.json').write_text(json.dumps({'tokens': {'access_token': 'synthetic-async-key', 'account_id': 'fixture-account'}}))
    (root / 'codex/auth.json').chmod(0o600)
    report = {'mode': mode, 'passed': False}
    with contextlib.ExitStack() as stack:
        upstream = GatedNativeUpstream()
        stack.callback(upstream.close)
        with socket.socket() as reservation:
            reservation.bind(('127.0.0.1', 0))
            port = reservation.getsockname()[1]
        config = root / 'emp-config.json'
        config.write_text(json.dumps({'auto_enable_on_start': False,
            'providers': [{'id': 'native', 'base_url': upstream.url, 'protocol': 'responses', 'auth_mode': 'forward'}],
            'models': [{'id': 'gpt-5', 'provider': 'native', 'upstream_id': 'gpt-5', 'context_window': 100000, 'enabled': True}]}))
        service = stack.enter_context(EmpServer(emp, config, root, env, root / 'emp.log', 1536 * 1024 ** 2, port))
        request = urllib.request.Request(service.api_base_url + '/models?client_version=0.163.0',
            headers={'Authorization': 'Bearer synthetic-async-key'})
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=10) as response:
            (root / 'models.json').write_bytes(response.read())
        (root / 'codex/config.toml').write_text(
            'model_catalog_json=' + json.dumps(str(root / 'models.json')) + '\n'
            + 'model="gpt-5"\nmodel_provider="fixture"\napproval_policy="never"\nsandbox_mode="read-only"\n'
            'model_context_window=100000\nmodel_auto_compact_token_limit=90000\n'
            '[features]\nplugins=false\ntoken_budget=false\nasync_compaction=' + str(mode != 'disabled').lower() + '\n'
            '[model_providers.fixture]\nname="OpenAI"\nbase_url=' + json.dumps(service.api_base_url) + '\n'
            'wire_api="responses"\nenv_key="OPENAI_API_KEY"\nsupports_websockets=false\nrequest_max_retries=0\nstream_max_retries=0\n'
            '[model_providers.fixture.capabilities]\nremote_compaction="v2"\n')
        try:
            with AppServer(codex, env, root / 'codex.log') as server:
                thread = server.rpc('thread/start', {'cwd': str(root / 'workspace'), 'model': 'gpt-5',
                    'modelProvider': 'fixture', 'approvalPolicy': 'never', 'sandbox': 'read-only'})['thread']['id']
                outcome = run_turn(server, thread, 'ASYNC_BASE', 'gpt-5')
                assert outcome == 'completed', json.dumps({'outcome': outcome, 'errors': [event for event in server.events if event.get('method') == 'error']})
                if mode == 'disabled':
                    assert run_turn(server, thread, 'ASYNC_CONTROL', 'gpt-5') == 'completed'
                else:
                    assert upstream.compacting.wait(15), 'No background request reached the native upstream'
                    if mode == 'adopted':
                        server.rpc('thread/compact/start', {'threadId': thread})
                        upstream.release.set()
                        assert upstream.sent.wait(15)
                        assert server.wait_turn(thread) == 'completed', 'Explicit compaction did not finish'
                        assert run_turn(server, thread, 'ASYNC_AFTER', 'gpt-5') == 'completed'
                    elif mode == 'suffix':
                        assert run_turn(server, thread, 'LIVE_SUFFIX_AFTER_CAPTURE', 'gpt-5') == 'completed'
                        upstream.release.set()
                        assert upstream.sent.wait(15)
                        assert run_turn(server, thread, 'ASYNC_AFTER_SUFFIX', 'gpt-5') == 'completed'
                    elif mode != 'shutdown':
                        raise AssertionError(mode)
            compactions = [row for row in upstream.requests if row['compact']]
            responses = [row for row in upstream.requests if not row['compact']]
            report.update(compactions=len(compactions), responses=len(responses),
                process_exit_code=server.process.returncode, requests=upstream.requests)
            assert server.process.returncode == 0, 'Codex required forced termination'
            if mode == 'disabled':
                assert not compactions
            elif mode == 'adopted':
                assert len(compactions) == 1, 'Foreground repeated a prepared background request'
                assert 'OPAQUE_ASYNC_FIXTURE_' in json.dumps(responses[-1]['body'])
            elif mode == 'suffix':
                assert 'LIVE_SUFFIX_AFTER_CAPTURE' in json.dumps(responses[-1]['body'])
                assert 'OPAQUE_ASYNC_FIXTURE_2' not in json.dumps(responses[-1]['body'])
            assert not any(row.get('gate_timeout') for row in upstream.requests)
            report['passed'] = True
        except Exception as error:
            report.update(error=repr(error), requests=upstream.requests)
        finally:
            upstream.release.set()
    (root / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({key: value for key, value in report.items() if key != 'requests'}), flush=True)
    return report['passed']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex-bin', type=Path, required=True)
    parser.add_argument('--emp-bin', type=Path, required=True)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--modes', nargs='+', choices=['disabled', 'adopted', 'suffix', 'shutdown'],
        default=['disabled', 'adopted', 'suffix', 'shutdown'])
    args = parser.parse_args()
    os.umask(0o077)
    args.codex_bin = args.codex_bin.resolve(strict=True)
    args.emp_bin = args.emp_bin.resolve(strict=True)
    assert not args.root.exists(), 'Acceptance needs a fresh root'
    args.root.mkdir(parents=True)
    (args.root / 'version-home').mkdir()
    env = {'PATH': os.defpath, 'HOME': str(args.root), 'CODEX_HOME': str(args.root / 'version-home')}
    version = subprocess.check_output([str(args.codex_bin), '--version'], env=env, text=True).strip()
    hashes = {'codex': sha256_file(args.codex_bin), 'emp': sha256_file(args.emp_bin)}
    results = [run_case(args.root / mode, args.codex_bin, args.emp_bin, mode) for mode in args.modes]
    unchanged = hashes == {'codex': sha256_file(args.codex_bin), 'emp': sha256_file(args.emp_bin)}
    report = {'passed': all(results) and unchanged, 'codex_version': version,
        'binary_hashes': hashes, 'binaries_unchanged': unchanged, 'cases': dict(zip(args.modes, results))}
    (args.root / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
