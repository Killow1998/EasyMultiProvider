"""Default Shadow: real management reads and isolated candidate accounting."""
import contextlib
import json
import os
from pathlib import Path
import secrets
import signal
import tempfile
import threading
import webbrowser
from http.server import ThreadingHTTPServer

from local_backend import request
from real_snapshot import AccountingSnapshot
from shadow_migration import MigrationPreview
from shadow_release_notes import ReleaseNotes


def serve(handler, args):
    source = f'http://127.0.0.1:{args.backend_port}'
    state = args.state_dir.expanduser().resolve()
    home = (args.codex_home or Path(os.environ.get('CODEX_HOME', Path.home() / '.codex'))).expanduser().resolve()
    token = json.loads((state / 'web-session.json').read_text())['token']
    status, public = request(source + '/api/config', headers={'X-EMP-Session':token,'Origin':source})
    if status != 200:
        raise ValueError('Could not read the selected EMP instance; synthetic fallback is disabled')
    with contextlib.ExitStack() as stack:
        snapshot = AccountingSnapshot(stack, args.emp.resolve(), state, home, public, args.scratch_dir)
        server = ThreadingHTTPServer(('127.0.0.1', args.port), handler)
        server.daemon_threads = True
        stack.callback(server.server_close)
        url = f'http://127.0.0.1:{server.server_port}'
        server.hosts = {f'127.0.0.1:{server.server_port}',f'localhost:{server.server_port}'}
        server.backend_port, server.state_dir = args.backend_port, state
        server.bootstrap, server.session = secrets.token_urlsafe(32), secrets.token_urlsafe(32)
        server.snapshot = snapshot
        server.real_context = {'url':url,'draft_key':os.environ.get('EMP_SHADOW_DRAFT_KEY') or 'emp.shadow.real.' + server.bootstrap}
        # Real-page migration is a local preview; it never forwards a production write.
        config = args.config or Path(os.environ.get('EASY_MULTI_PROVIDER_CONFIG', state.parent / 'config.json'))
        server.migration = MigrationPreview(args.migration_helper, config, home / 'auth.json')
        server.release_notes = ReleaseNotes(args.release_api) if args.release_api else ReleaseNotes()
        demo_urls = {}
        demo_lock = threading.Lock()
        def open_demo(scenario=''):
            with demo_lock:
                kind='onboarding' if scenario in {'gateway','onboarding','lan'} else 'general'
                if kind not in demo_urls:
                    from preview import fixture, preview, seed_session_bills
                    with contextlib.ExitStack() as pending:
                        root = Path(pending.enter_context(tempfile.TemporaryDirectory(prefix='emp-shadow-demo-',dir=snapshot.scratch)))
                        if kind=='onboarding':
                            from onboarding_preview import start
                            demo_urls[kind]=start(pending,args.emp.resolve(),root,url)
                        else:
                            endpoint, _, bundle = fixture(pending, args.emp.resolve(), root)
                            seed_session_bills(endpoint[1])
                            demo_urls[kind], _ = preview(pending, endpoint[0], bundle, endpoint[2]['X-EMP-Session'], return_url=url)
                        stack.enter_context(pending.pop_all())
                return demo_urls[kind]
        server.open_demo = open_demo
        done = threading.Event()
        signal.signal(signal.SIGTERM, lambda *_:done.set())
        signal.signal(signal.SIGINT, lambda *_:done.set())
        threading.Thread(target=server.serve_forever, daemon=True).start()
        stack.callback(server.shutdown)
        if args.open:
            webbrowser.open(url)
        print(f'EMP real-data Shadow: {url}/', flush=True)
        done.wait()
