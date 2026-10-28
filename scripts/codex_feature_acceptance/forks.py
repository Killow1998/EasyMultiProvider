"""Verify attached forks retain their source, history and billing ownership."""
import json
import sqlite3

from scripts.shadow_frontend.local_backend import request
from .fixture import Fixture, turn


def run(codex, emp, root, evidence):
    with Fixture(codex, emp, root, evidence) as fixture:
        client = fixture.client()
        parent = client.rpc('thread/start', {'cwd':str(fixture.home)})['thread']['id']
        turn(client, parent, 'ATTACHED_PARENT_CONTEXT: reply once.')
        attached = client.rpc('thread/fork', {'threadId':parent, 'attachToSource':True})['thread']
        assert attached['forkedFromId'] == parent, attached
        child = attached['id']
        turn(client, child, 'ATTACHED_CHILD_CONTEXT: preserve the parent context.')
        assert 'ATTACHED_PARENT_CONTEXT' in json.dumps(fixture.upstream.requests[-1]['body'])
        ordinary = client.rpc('thread/fork', {'threadId':parent})['thread']['id']
        turn(client, ordinary, 'ORDINARY_FORK: this is a separate conversation.')
        client.close()
        client = fixture.client()
        listed = client.rpc('thread/list', {'attachedToThreadId':parent})['data']
        assert [item['id'] for item in listed] == [child], listed
        assert child not in [item['id'] for item in client.rpc('thread/list', {})['data']]
        client.rpc('thread/resume', {'threadId':child})
        turn(client, child, 'ATTACHED_RESUMED: retain the source and child contexts.')
        wire = json.dumps(fixture.upstream.requests[-1]['body'])
        assert 'ATTACHED_PARENT_CONTEXT' in wire and 'ATTACHED_CHILD_CONTEXT' in wire
        database, = fixture.home.glob('state_*.sqlite')
        with sqlite3.connect(database) as db:
            parents = db.execute('SELECT id, attached_thread_parent_id FROM threads WHERE id IN (?,?)',
                                 (child, ordinary)).fetchall()
        assert dict(parents) == {child:parent, ordinary:None}, parents
        status, bills = request(fixture.base+'/api/usage?sessions=true&start=0&end=9999999999', headers=fixture.headers)
        assert status == 200, bills
        result = {'passed':True, 'requests':len(fixture.upstream.requests), 'parent':parent,
                  'attached':child, 'ordinary':ordinary, 'bills':bills, 'database':parents}
        (evidence/'result.json').write_text(json.dumps(result, indent=2))
        assert bills['records'][0]['session_id'] == parent and bills['records'][0]['requests'] == 4, bills
        print(json.dumps({'passed':True, 'attached_parent_preserved':True,
                          'restart_history_preserved':True, 'billed_requests':4}), flush=True)
        return result
