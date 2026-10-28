"""Exercise Codex's external memory import across fresh, legacy and explicit-v2 profiles."""
from datetime import datetime, timezone
import json

from .fixture import Fixture


def run(codex, emp, root, evidence):
    results = []
    for label, legacy, explicit in [('fresh-v2', False, False), ('legacy-v1', True, False),
                                    ('legacy-explicit-v2', True, True)]:
        with Fixture(codex, emp, root/label, evidence/label) as fixture:
            legacy_file = fixture.home/'memories/memory_summary.md'
            if legacy:
                legacy_file.parent.mkdir()
                legacy_file.write_text('v1\n\nLEGACY_IMPORT_PRESERVED\n')
            source = fixture.home/'.claude/projects/project-a'
            (source/'memory').mkdir(parents=True)
            (source/'memory/MEMORY.md').write_text('SYNTHETIC_IMPORTED_MEMORY\n')
            (source/'memory/release.md').write_text('Synthetic release guidance.\n')
            project = fixture.home/'project-a'
            project.mkdir()
            (source/'session.jsonl').write_text(json.dumps({'type':'user', 'cwd':str(project),
                'timestamp':datetime.now(timezone.utc).isoformat(), 'message':{'content':'synthetic memory import'}})+'\n')
            config = fixture.config.replace('memories=false', 'memories=true\nexternal_agent_memory_import=true')
            config += '[memories]\ngenerate_memories=false\n'+('version="v2"\n' if explicit else '')
            (fixture.home/'config.toml').write_text(config)
            client = fixture.client()
            items = client.rpc('externalAgentConfig/detect', {'includeHome':True})['items']
            selected = [item for item in items if item['itemType'] == 'MEMORY']
            assert len(selected) == 1, items
            assert selected[0]['details']['memory'] == ['project-a'], selected
            imported = client.rpc('externalAgentConfig/import', {'migrationItems':selected})
            while True:
                completed = next((event for event in client.events
                    if event.get('method') == 'externalAgentConfig/import/completed'), None)
                if completed is not None:
                    break
                client.events.append(client.read_message(30))
            folder = 'memories' if legacy and not explicit else 'memories_v2'
            resources = fixture.home/folder/'extensions/external_agent_import/resources/project-a'
            assert (resources/'MEMORY.md').read_bytes() == (source/'memory/MEMORY.md').read_bytes()
            assert (resources/'release.md').read_bytes() == (source/'memory/release.md').read_bytes()
            assert json.loads((resources/'scope.json').read_text()) == {'cwd':str(project)}
            if legacy:
                assert legacy_file.read_text() == 'v1\n\nLEGACY_IMPORT_PRESERVED\n'
            detected = client.rpc('externalAgentConfig/detect', {'includeHome':True})['items']
            assert not any(item['itemType'] == 'MEMORY' for item in detected), detected
            assert not fixture.upstream.requests, 'Import unexpectedly started model inference'
            result = {'profile':label, 'folder':folder, 'passed':True, 'import':imported,
                      'completion':completed, 'model_requests':0}
            (evidence/label/'result.json').write_text(json.dumps(result, indent=2))
            print(json.dumps({'profile':label, 'folder':folder, 'passed':True}), flush=True)
            results.append(result)
    return results
