"""Run candidate accounting against read-only copies of existing local records."""
import contextlib
import json
from pathlib import Path
import sqlite3
import tempfile
import threading
import time
import tomllib
import secrets
from urllib.parse import parse_qs, urlencode, urlsplit

from local_backend import request, start_backend


def database_copy(source, target):
    if source.is_symlink() or not source.is_file():
        raise ValueError('Expected a regular SQLite source file')
    target.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(source.resolve().as_uri() + '?mode=ro', uri=True, timeout=10) as original:
        with sqlite3.connect(target) as copied:
            original.backup(copied)


def context_settings(home, target):
    """Copy only public context preferences, never auth or provider configuration."""
    path = home / 'config.toml'
    if not path.exists():
        return
    if path.is_symlink():
        raise ValueError('Codex configuration must be a regular file')
    settings = tomllib.loads(path.read_text())
    lines = []
    for key in ['model_context_window', 'model_auto_compact_token_limit', 'model_auto_compact_token_limit_scope']:
        if isinstance(settings.get(key), (int, str)):
            lines.append(key + ' = ' + json.dumps(settings[key]))
    feature = settings.get('features', {}).get('async_compaction')
    if isinstance(feature, bool):
        lines.extend(['[features]', 'async_compaction = ' + json.dumps(feature)])
    elif isinstance(feature, dict):
        lines.append('[features.async_compaction]')
        for key in ['enabled', 'model_async_compact_threshold_percent']:
            if isinstance(feature.get(key), (bool, int)):
                lines.append(key + ' = ' + json.dumps(feature[key]))
    (target / 'config.toml').write_text('\n'.join(lines) + '\n')


class AccountingSnapshot:
    def __init__(self, stack, binary, state, home, public_config, scratch=None):
        self.binary, self.source_state, self.source_home = binary, state, home
        self.config = public_config
        self.scratch = scratch or Path(__file__).resolve().parents[2] / '.shadow-state'
        self.scratch.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.lock = threading.RLock()
        self.current = None
        stack.callback(self.close)
        self.refresh()

    def refresh(self):
        with self.lock:
            pending = contextlib.ExitStack()
            try:
                root = Path(pending.enter_context(tempfile.TemporaryDirectory(prefix='emp-shadow-real-',dir=self.scratch)))
                state, home = root / 'state', root / 'native'
                state.mkdir(); home.mkdir()
                database_copy(self.source_state / 'usage.sqlite3', state / 'usage.sqlite3')
                databases = sorted((path for path in self.source_home.glob('state_*.sqlite')
                    if path.stem.removeprefix('state_').isdigit()),
                    key=lambda path: int(path.stem.removeprefix('state_')), reverse=True)[:4]
                for path in databases:
                    database_copy(path, home / path.name)
                index = self.source_home / 'session_index.jsonl'
                if index.exists():
                    if index.is_symlink():
                        raise ValueError('Codex session index must be a regular file')
                    # A fixed extent excludes concurrent appends; no hardlinks to active files.
                    with index.open('rb') as source, (home / index.name).open('wb') as target:
                        remaining = index.stat().st_size
                        while remaining:
                            data = source.read(min(remaining, 1024 * 1024))
                            if not data:
                                raise ValueError('Codex session index changed during snapshot')
                            target.write(data); remaining -= len(data)
                context_settings(self.source_home, home)
                prices = self.source_state / 'api_prices.json'
                value = json.loads(prices.read_text()) if prices.is_file() and not prices.is_symlink() else {'prices':{}}
                value['fetched_at'] = time.time() + 86400
                (state / 'api_prices.json').write_text(json.dumps(value))
                catalog = root / 'models.json'
                catalog.write_text('{"models":[]}')
                base, _, _, headers = start_backend(pending, self.binary, root,
                    {'providers':[], 'models':[], 'accounts':[], 'auto_enable_on_start':False,
                     'subscription_search':{'enabled':False}, 'native_catalog_path':str(catalog),
                     'pricing_aliases':self.config.get('pricing_aliases',{}),
                     'codex_base_url':'http://127.0.0.1:9/preview-offline'},
                    {'CODEX_HOME':str(home), 'PATH':'/usr/bin:/bin',
                     'HTTP_PROXY':'http://127.0.0.1:9', 'HTTPS_PROXY':'http://127.0.0.1:9',
                     'ALL_PROXY':'http://127.0.0.1:9', 'NO_PROXY':'127.0.0.1,localhost'})
                with sqlite3.connect(state / 'usage.sqlite3') as database:
                    count = database.execute('SELECT COUNT(*) FROM usage_events').fetchone()[0]
                previous = self.current
                self.current = (pending, base, headers, {'captured_at':time.time(), 'usage_events':count,
                    'codex_databases':len(databases), 'credentials_copied':False, 'synthetic_data':False})
                if previous:
                    previous[0].close()
            except BaseException:
                pending.close()
                raise
            return self.current[3].copy()

    def read(self, path):
        with self.lock:
            return request(self.current[1] + path, headers=self.current[2], timeout=30)

    def usage(self, path, source_read):
        query = parse_qs(urlsplit(path).query, keep_blank_values=True)
        account = (query.get('account_id') or query.get('account') or [None])[0]
        if account:
            if query.get('owner'):
                return 400, {'error':{'message':'Choose an account or an owner, not both'}}
            status, metadata = source_read('/api/usage?' + urlencode({'account_id':account,'start':0,'end':time.time()+3600}))
            if status != 200:
                return status, metadata
            owners = {row['owner'] for row in metadata.get('groups',[])}
            if len(owners) > 1:
                return 409, {'error':{'message':'Account usage identity is ambiguous'}}
            # No source usage means an empty selection, never a guessed historical identity.
            owner = next(iter(owners), 'account:' + secrets.token_hex(32))
            query.pop('account_id',None); query.pop('account',None)
            query['owner'] = [owner]
        return self.read('/api/usage?' + urlencode(query,doseq=True))

    def status(self):
        with self.lock:
            return self.current[3].copy()

    def close(self):
        with self.lock:
            if self.current:
                self.current[0].close()
                self.current = None
