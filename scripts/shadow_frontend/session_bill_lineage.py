"""Observe real request metadata becoming one conversation bill; fake upstream only."""
import json
import sqlite3
from urllib.parse import urlencode

from e2e_environment import request

REVIEW = '77777777-7777-4777-8777-777777777777'
SIDECHAT = '88888888-8888-4888-8888-888888888888'
AGENT = '99999999-9999-4999-8999-999999999999'
ORPHAN = 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa'


def check_conversation_ownership(base, state, home, headers, query, main):
    config = request(base + '/api/config', headers=headers)[1]
    config['models'].append({'id': 'demo/codex-auto-review', 'provider': 'demo',
                             'upstream_id': 'upstream-model', 'enabled': True})
    assert request(base + '/api/config', config, headers)[0] == 200
    for thread, model, parent, sidechat in [
        (REVIEW, 'demo/codex-auto-review', main, False),
        (SIDECHAT, 'demo/model', main, True),
        (AGENT, 'demo/model', SIDECHAT, False),
        (ORPHAN, 'demo/model', '', False),
    ]:
        metadata = {'thread_id': thread, 'turn_id': 'turn-' + thread}
        incoming = {**headers, 'thread-id': thread}
        if sidechat:
            metadata['forked_from_thread_id'] = parent
        elif parent:
            incoming['x-codex-parent-thread-id'] = parent
        incoming['x-codex-turn-metadata'] = json.dumps(metadata)
        status, result = request(base + '/v1/responses',
            {'model': model, 'input': 'synthetic ownership check', 'stream': False}, incoming)
        assert status == 200, result
    with sqlite3.connect(home / 'state_5.sqlite') as database:
        database.execute('ALTER TABLE threads ADD COLUMN source TEXT')
        database.execute('ALTER TABLE threads ADD COLUMN thread_source TEXT')
        database.execute('INSERT INTO threads VALUES (?,?,?,?)',
            (REVIEW, 'Guardian review', json.dumps({'subagent': {'other': 'guardian'}}), 'guardian_review'))
        database.execute('CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT)')
        database.executemany('INSERT INTO thread_spawn_edges VALUES (?,?)', [(main, REVIEW), (SIDECHAT, AGENT)])
    status, bills = request(base + '/api/usage?sessions=true&' + query, headers=headers)
    assert status == 200, bills
    assert bills['total'] == 129 and bills['totals']['requests'] == 135, {
        'bills': bills['total'], 'requests': bills['totals']['requests'],
        'unassigned': bills['unassigned']}
    assert bills['unassigned']['requests'] == 2, bills
    all_usage = request(base + '/api/usage?' + query, headers=headers)[1]
    for field in ['requests', 'input_tokens', 'output_tokens', 'cached_input_tokens', 'cost_nanos']:
        assert bills['totals'][field] == all_usage['totals'][field], field
    for term in ['Guardian review', REVIEW, SIDECHAT]:
        matching = request(base + '/api/usage?' + urlencode({'sessions': 'true', 'q': term})
                           + '&' + query, headers=headers)[1]
        assert matching['total'] == 1 and matching['records'][0]['session_id'] == main, matching
    detail = request(base + '/api/usage?series=true&conversation=true&session=' + main + '&'
                     + query, headers=headers)[1]
    assert detail['totals']['requests'] == 5, detail
    assert {row['session_kind'] for row in detail['components']} == {
        'conversation', 'review', 'sidechat', 'subagent'}, detail
    assert detail['components'][0]['thread_id'] == main, detail
    for field in ['requests', 'input_tokens', 'output_tokens', 'cached_input_tokens', 'cost_nanos']:
        assert sum(row[field] for row in detail['components']) == detail['totals'][field], field
    with sqlite3.connect(state / 'usage.sqlite3') as database:
        saved = database.execute("SELECT json_extract(facts,'$.session_kind') FROM call_records WHERE session_id=?", (SIDECHAT,)).fetchone()[0]
    assert saved == 'sidechat', saved
