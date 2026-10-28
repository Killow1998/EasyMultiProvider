"""Verify pinned official Terra metadata on Native and imported subscriptions."""
import json

from scripts.shadow_frontend.local_backend import request
from .fixture import Fixture, turn


def run(codex, emp, root, evidence, catalog):
    official = json.loads(catalog.read_text())
    expected = next(model for model in official['models'] if model['slug'] == 'gpt-6.1-terra')
    with Fixture(codex, emp, root, evidence, native_catalog=official) as fixture:
        fixture.import_account()
        status, merged = request(fixture.base+'/v1/models?client_version=0.165.0', headers=fixture.headers)
        assert status == 200, merged
        fields = ['context_window', 'max_context_window', 'default_reasoning_level',
                  'supported_reasoning_levels', 'multi_agent_reasoning_effort', 'tool_mode',
                  'use_responses_lite', 'supports_reasoning_effort_updates', 'prefer_websockets', 'input_modalities']
        names = ['gpt-6.1-terra', 'imported/gpt-6.1-terra']
        for name in names:
            model = next(model for model in merged['models'] if model['slug'] == name)
            assert {key:model.get(key) for key in fields} == {key:expected.get(key) for key in fields}, name
        merged['models'] = [model for model in merged['models'] if model['slug'] in names]
        (fixture.home/'models.json').write_text(json.dumps(merged))
        client = fixture.client()
        for name in names:
            thread = client.rpc('thread/start', {'model':name, 'cwd':str(fixture.home)})['thread']['id']
            for level in expected['supported_reasoning_levels']:
                effort = level['effort']
                turn(client, thread, 'TERRA_METADATA_FIXTURE: reply once.', effort=effort)
                body = fixture.upstream.requests[-1]['body']
                assert body['model'] == 'gpt-6.1-terra', body['model']
                updates = [item for item in body['input'] if item.get('type') == 'configuration_update']
                actual = updates[-1]['reasoning']['effort'] if updates else body['reasoning']['effort']
                assert actual == (expected['multi_agent_reasoning_effort'] if effort == 'ultra' else effort), (effort, actual)
        status, ledger = request(fixture.base+'/api/calls', headers=fixture.headers)
        levels = [level['effort'] for level in expected['supported_reasoning_levels']]
        assert status == 200 and [row['requested_effort'] for row in reversed(ledger['records'])] == levels*2, ledger
        result = {'passed':True, 'source_ref':'rust-v0.165.0', 'model':'gpt-6.1-terra',
                  'metadata':{key:expected.get(key) for key in fields}, 'accounts':['native', 'imported'],
                  'requests':len(fixture.upstream.requests), 'real_provider_calls':0}
        (evidence/'result.json').write_text(json.dumps(result, indent=2))
        print(json.dumps(result), flush=True)
        return result
