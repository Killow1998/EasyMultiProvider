"""Exercise v2 extraction, consolidation, persistent files and subsequent memory reads."""
import json
import sqlite3
import time

from scripts.shadow_frontend.local_backend import request
from .fixture import Fixture, message, turn


def run(codex, emp, root, evidence, source='native'):
    with Fixture(codex, emp, root, evidence, configuration={
            'memory_extraction_model':'gpt-6-luna', 'memory_model':'gpt-5.6-luna'}) as fixture:
        seed = fixture.client()
        thread = seed.rpc('thread/start', {'cwd':str(fixture.home), 'approvalPolicy':'never',
                          'sandbox':'workspace-write'})['thread']['id']
        turn(seed, thread, 'Remember this synthetic preference: use focused acceptance tests.')
        launcher_model = 'gpt-6.1-sol'
        if source == 'imported':
            fixture.import_account()
            launcher_model = 'imported/gpt-6.1-sol'
            template = next(entry for entry in fixture.catalog['models'] if entry['slug'] == 'gpt-6.1-sol')
            fixture.catalog['models'].append({**template, 'slug':launcher_model, 'display_name':launcher_model})
            (fixture.home/'models.json').write_text(json.dumps(fixture.catalog))
        elif source == 'external':
            launcher_model = 'responses/probe'
        launcher = seed.rpc('thread/start', {'cwd':str(fixture.home), 'model':launcher_model,
                            'approvalPolicy':'never', 'sandbox':'workspace-write'})['thread']['id']
        turn(seed, launcher, 'MEMORY_SOURCE_ACCOUNT: reply once.')
        seed.close()
        database, = fixture.home.glob('state_*.sqlite')
        with sqlite3.connect(database) as db:
            rows = db.execute('SELECT id, source, memory_mode, updated_at FROM threads').fetchall()
            print(json.dumps({'seed_threads':rows}), flush=True)
            db.execute('UPDATE threads SET updated_at=? WHERE id=?', (int(time.time())-86400, thread))
        config = fixture.config.replace('memories=false', 'memories=true')
        config += ('[memories]\ngenerate_memories=true\nmin_rollout_idle_hours=0\n'
                   'max_rollouts_per_startup=1\nmin_rate_limit_remaining_percent=0\n'
                   'extract_model="gpt-6.1-sol"\nconsolidation_model="gpt-6.1-sol"\n')
        (fixture.home/'config.toml').write_text(config)
        memory_root = fixture.home/'memories_v2'
        summary = ('v1\n\n## User Profile\n\nSynthetic fixture.\n\n## User preferences\n\n'
                   'MEMORY_PIPELINE_COMPLETE: use focused acceptance tests.\n\n'
                   '## General Tips\n\nUse isolated homes.\n\n## What\'s in Memory\n\n'
                   '- Focused acceptance fixture, supported by rollout_summaries.\n')
        wrote = False
        def respond(record):
            nonlocal wrote
            body = record['body']
            if 'MEMORY_STARTUP_TRIGGER' in json.dumps(body) and 'rollout_summary' not in body.get('text', {}).get('format', {}).get('schema', {}).get('properties', {}):
                return message('Memory startup trigger complete.')
            schema = body.get('text', {}).get('format', {}).get('schema', {})
            if 'rollout_summary' in schema.get('properties', {}):
                assert 'raw_memory' not in schema['properties'], 'v1 extraction contract used'
                return message(json.dumps({'rollout_summary':'Fixture preference: use focused acceptance tests.',
                                           'rollout_slug':'focused-acceptance'}))
            if not wrote:
                declarations = body.get('tools', []) + [tool for item in body.get('input', [])
                    if isinstance(item, dict) and item.get('type') == 'additional_tools' for tool in item['tools']]
                tools = [child.get('name') for tool in declarations for child in tool.get('tools', [tool])]
                assert 'apply_patch' in tools, ('missing consolidation writer', tools)
                assert not (memory_root/'memory_summary.md').exists()
                patch = '*** Begin Patch\n*** Add File: '+str(memory_root/'memory_summary.md')+'\n'
                patch += ''.join('+'+line+'\n' for line in summary.splitlines())+'*** End Patch\n'
                wrote = True
                return {'id':'write-memory', 'type':'custom_tool_call', 'name':'apply_patch',
                        'call_id':'write-memory', 'namespace':'functions', 'input':patch, 'status':'completed'}
            assert (memory_root/'memory_summary.md').read_text() == summary
            return message('Memory consolidation complete.')
        fixture.upstream.respond = respond
        client = fixture.client()
        environment = fixture.executor(client)
        client.rpc('thread/resume', {'threadId':launcher})
        turn(client, launcher, 'MEMORY_STARTUP_TRIGGER: reply once.', environments=[environment])
        deadline = time.monotonic()+30
        state = {}
        while time.monotonic() < deadline:
            memory_database = fixture.home/'memories_v2_1.sqlite'
            if memory_database.exists():
                with sqlite3.connect(memory_database) as db:
                    tables = [row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")]
                    state = {name:db.execute('SELECT * FROM "'+name+'"').fetchall()
                             for name in tables if not name.startswith('_')}
                assert not any(row[2] == 'error' for row in state.get('jobs', [])), state
                if len(state.get('jobs', [])) == 2 and all(row[2] == 'done' for row in state['jobs']):
                    break
            time.sleep(.1)
        assert (memory_root/'memory_summary.md').exists(), 'v2 memory consolidation did not produce a summary'
        assert len(state['jobs']) == 2 and all(row[2] == 'done' for row in state['jobs']), state
        assert list((memory_root/'rollout_summaries').glob('*.md')), 'v2 extraction summary missing'
        assert not (memory_root/'raw_memories.md').exists(), 'v1 raw memory leaked into v2'
        assert not (memory_root/'MEMORY.md').exists(), 'v1 registry leaked into v2'
        assert not (fixture.home/'memories').exists(), 'fresh v2 profile created v1 memories'
        fixture.upstream.respond = lambda _record: message('MEMORY_READ_COMPLETE')
        reader = client.rpc('thread/start', {'cwd':str(fixture.home), 'ephemeral':True})['thread']['id']
        turn(client, reader, 'Use the saved synthetic preference and reply once.')
        assert 'MEMORY_PIPELINE_COMPLETE' in json.dumps(fixture.upstream.requests[-1]['body'])
        status, ledger = request(fixture.base+'/api/calls', headers=fixture.headers)
        assert status == 200, ledger
        extraction = [row for row in ledger['records'] if row['route'] == 'memory_extraction']
        consolidation = [row for row in ledger['records'] if row['route'] == 'memory_consolidation']
        assert len(extraction) == 1 and extraction[0]['upstream_model'] == 'gpt-6-luna', ledger
        assert extraction[0]['account_id'] == ('fixture-imported' if source == 'imported' else '@native'), ledger
        assert len(consolidation) == 2 and all(row['upstream_model'] == 'gpt-5.6-luna' for row in consolidation), ledger
        assert all(row['account_id'] == '@native' for row in consolidation), ledger
        assert (fixture.home/'config.toml').read_text() == config
        result = {'passed':True, 'source':source, 'seed_thread':thread, 'memory_root':'memories_v2',
                  'state':state, 'ledger':ledger, 'requests':len(fixture.upstream.requests)}
        (evidence/'result.json').write_text(json.dumps(result, indent=2))
        print(json.dumps({'passed':True, 'requests':result['requests'], 'memory_tables':list(state)}), flush=True)
        return result
