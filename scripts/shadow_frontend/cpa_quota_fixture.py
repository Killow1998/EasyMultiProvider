"""Disposable CPA observer for quota acceptance and the isolated Shadow example."""
import datetime as dt
import json
import sqlite3
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def seed_pool_history(state):
    """A day's synthetic five-minute observations from the fixture's actual stored rows."""
    with sqlite3.connect(state/'quota_history.sqlite3') as database:
        latest = {}
        for row in database.execute('SELECT account_key,limit_id,window_kind,window_minutes,used_percent,resets_at FROM quota_samples WHERE account_key LIKE \'cpa:%\' ORDER BY observed_at DESC'):
            latest.setdefault(row[:3], row)
        now = int(time.time())
        rows = []
        for owner, limit, kind, minutes, used, reset in latest.values():
            for index in range(288, -1, -1):
                rows.append((owner, now-index*300, limit, kind, minutes, max(0, used-index/24), reset))
        database.executemany('INSERT OR REPLACE INTO quota_samples (account_key,observed_at,limit_id,window_kind,window_minutes,used_percent,resets_at) VALUES (?,?,?,?,?,?,?)', rows)


class QuotaObserver(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        self.server.model_calls += 1
        self.send_error(405)

    def do_GET(self):
        mode = self.server.mode
        if self.path == '/cpa/v1/models':
            status, body = 200, {'data': []}
        elif self.path == '/cpa/v1/quota':
            self.server.calls.append((self.path, self.headers.get('Authorization')))
            status = {'unavailable': 503, 'unsupported': 404, 'denied': 401, 'redirect': 302}.get(mode, 200)
            body = self.server.quota
            if mode == 'invalid':
                body = {**body, 'scope': 'unrelated'}
            elif mode == 'stale':
                body = {**body, 'accounts': [{**account, 'stale': True} for account in body['accounts']]}
            elif mode == 'empty':
                body = {**body, 'accounts': []}
        else:
            status, body = 404, {}
        encoded = json.dumps(body).encode()
        self.send_response(status)
        if status == 302:
            self.send_header('Location', '/must-not-follow')
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


def start_pool(stack):
    server = ThreadingHTTPServer(('127.0.0.1', 0), QuotaObserver)
    server.mode, server.calls, server.model_calls = 'ok', [], 0
    now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)

    def window(value, seconds):
        return {'remaining_percent': value, 'resets_at': (now+dt.timedelta(seconds=seconds)).isoformat()}

    accounts = [
        {'id': 'account-alpha', 'enabled': True, 'stale': False, 'status': 'ok', 'as_of': now.isoformat(),
         'windows': {'five_hour': window(80, 7200), 'seven_day': window(40, 259200), 'seven_day_opus': window(15, 259200)}},
        {'id': 'account-beta', 'enabled': True, 'stale': False, 'status': 'ok', 'as_of': now.isoformat(),
         'windows': {'five_hour': window(40, 3600), 'seven_day': window(0, 172800)}},
        {'id': 'account-old', 'enabled': True, 'stale': True, 'status': 'error', 'as_of': now.isoformat(),
         'windows': {'five_hour': window(100, 60), 'seven_day': window(100, 60)}},
        {'id': 'account-disabled', 'enabled': False, 'stale': False, 'status': 'disabled', 'as_of': None,
         'windows': {'five_hour': window(100, 30), 'seven_day': window(100, 30)}},
        {'id': 'account-unknown', 'enabled': True, 'stale': False, 'status': 'unknown', 'as_of': None, 'windows': {}},
    ]
    server.quota = {'scope': 'shared_pool', 'visibility': 'authorized_accounts', 'provider': 'claude',
        'checked_at': now.isoformat(), 'accounts': accounts, 'private_extra': 'upstream-only-private'}
    server.started_at = int(now.timestamp())
    stack.callback(server.server_close)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    stack.callback(server.shutdown)
    return server
