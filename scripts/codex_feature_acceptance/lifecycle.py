"""Retained failed/usage-less output must still trigger real-engine compaction."""
import json

from scripts.code_mode_collaboration_acceptance import response_events
from scripts.shadow_frontend.local_backend import request
from .fixture import Fixture, message, turn


def run(codex, emp, root, evidence):
    results = []
    for outcome in ('failed', 'unreported'):
        with Fixture(codex, emp, root/outcome, evidence/outcome) as fixture:
            retained = 'RETAINED_OUTPUT_'+outcome+' '+('context evidence ' * 4000)

            def respond(record):
                record['compaction'] = any(item.get('type') == 'compaction_trigger'
                                           for item in record['body']['input'])
                if record['compaction']:
                    return {'id':'compaction-'+record['response_id'], 'type':'compaction',
                            'encrypted_content':'synthetic-compaction-'+outcome}
                records = [entry for entry in fixture.upstream.requests if not entry['compaction']]
                return message(retained if len(records) == 2 else 'FIXTURE_COMPLETE')

            def events(item, identity):
                values = list(response_events(item, identity))
                for event in values:
                    if 'response' in event:
                        event['response'].pop('usage', None)
                if item['type'] == 'compaction':
                    return values
                if item['content'][0]['text'] == retained:
                    earlier = message('SUPERSEDED_OUTPUT_'+outcome)
                    earlier.update(id='earlier-'+identity, phase='commentary')
                    for event in values[1:-1]:
                        event['output_index'] = 1
                    values[1:1] = list(response_events(earlier, identity))[1:-1]
                    values[-1]['response']['output'].insert(0, earlier)
                    if outcome == 'failed':
                        values[-1]['type'] = 'response.failed'
                        values[-1]['response']['status'] = 'failed'
                        values[-1]['response']['error'] = {
                            'code':'invalid_request_error', 'message':'Synthetic retained-output failure'}
                else:
                    values[-1]['response']['usage'] = {
                        'input_tokens':1000, 'output_tokens':3, 'total_tokens':1003}
                return values

            fixture.upstream.respond = respond
            fixture.upstream.response_events = events
            fixture.config = 'model_auto_compact_token_limit=2000\n'+fixture.config
            (fixture.home/'config.toml').write_text(fixture.config)
            client = fixture.client()
            started = client.rpc('thread/start', {'model':'gpt-6.1-sol', 'cwd':str(fixture.home),
                                 'approvalPolicy':'never', 'sandbox':'read-only'})['thread']
            assert started['historyMode'] == 'paginated', started['historyMode']
            thread = started['id']
            turn(client, thread, 'BASELINE_WITH_REPORTED_USAGE')
            retained_turn = client.rpc('turn/start', {
                'threadId':thread, 'input':[{'type':'text', 'text':'RETAIN_OUTPUT'}]})['turn']['id']
            state = client.wait_turn(thread, timeout=30)
            assert state == ('failed' if outcome == 'failed' else 'completed'), (outcome, state)
            client.events.clear()
            turn(client, thread, 'CONTINUE_WITH_RETAINED_OUTPUT')
            compact = [entry for entry in fixture.upstream.requests if entry['compaction']]
            assert len(compact) == 1, (outcome, len(compact))
            assert retained in json.dumps(compact[0]['body']), 'Compaction lost retained output'
            assert 'SUPERSEDED_OUTPUT_'+outcome in json.dumps(compact[0]['body'])
            client.close()
            client = fixture.client()
            resumed = client.rpc('thread/resume', {'threadId':thread})
            assert retained in json.dumps(resumed), 'Persisted history lost retained output'
            assert 'SUPERSEDED_OUTPUT_'+outcome in json.dumps(resumed)
            turn(client, thread, 'CONTINUE_AFTER_PROCESS_RESUME')
            records = [entry for entry in fixture.upstream.requests if not entry['compaction']]
            assert len(records) == 4, (outcome, len(records))
            status, calls = request(fixture.base+'/api/calls', headers=fixture.headers)
            assert status == 200, calls
            unreported = [row for row in calls['records'] if row.get('turn_id') == retained_turn]
            assert len(unreported) == 1, (outcome, len(unreported), retained_turn)
            assert unreported[0].get('input_tokens') is None, unreported
            assert unreported[0].get('output_tokens') is None, unreported
            result = {'outcome':outcome, 'passed':True, 'requests':len(fixture.upstream.requests),
                      'generation_requests':len(records), 'history_mode':started['historyMode'],
                      'compactions':len(compact), 'retained_output_bytes':len(retained.encode()),
                      'unknown_usage_preserved':True, 'process_resume':True}
            (evidence/outcome/'result.json').write_text(json.dumps(result, indent=2)+'\n')
            print(json.dumps(result), flush=True)
            results.append(result)
    return results
