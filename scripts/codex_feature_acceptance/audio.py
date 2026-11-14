"""Recordings survive provider projection, follow-up turns and process resume."""
import base64
import json
import urllib.request
import wave

from .fixture import Fixture, turn


def run(codex, emp, root, evidence):
    results = []
    native = {'models':[{'slug':'gpt-6.1-sol', 'supported_in_api':True,
        'context_window':100000, 'input_modalities':['text','image','audio'],
        'shell_type':'shell_command', 'base_instructions':'Reply to the synthetic recording.'}]}
    config = {'models':[{'id':source+'/probe', 'provider':source, 'upstream_id':'probe',
        'enabled':True, 'input_modalities':['text','audio']} for source in ['responses','chat']]}
    for target in ['gpt-6.1-sol', 'responses/probe', 'chat/probe']:
        label = target.replace('/', '-')
        with Fixture(codex, emp, root/label, evidence/label,
                     configuration=config, native_catalog=native) as fixture:
            model = next(row for row in fixture.catalog['models'] if row['slug'] == target)
            assert 'audio' in model['input_modalities'], model
            if target.startswith('responses/'):
                etags = set()
                for version in ['0.165.0','0.166.0','0.167.1','0.168.0','0.169.0']:
                    with urllib.request.urlopen(urllib.request.Request(fixture.base+'/v1/models?client_version='+version, headers=fixture.headers)) as response:
                        assert response.status == 200
                        catalog = json.load(response)
                        etags.add(response.headers['ETag'])
                    audio_model = next(row for row in catalog['models'] if row['slug'] == target)
                    assert ('audio' in audio_model['input_modalities']) == (int(version.split('.')[1]) >= 168), (version, audio_model)
                assert len(etags) == 1, 'catalog revisions must remain consistent across client projections'
            recording = fixture.home/'fixture.wav'
            with wave.open(str(recording), 'wb') as audio:
                audio.setnchannels(1)
                audio.setsampwidth(2)
                audio.setframerate(16000)
                audio.writeframes(b'\0\0' * 1600)
            encoded = base64.b64encode(recording.read_bytes()).decode()
            client = fixture.client()
            thread = client.rpc('thread/start', {'model':target, 'cwd':str(fixture.home),
                'approvalPolicy':'never', 'sandbox':'read-only'})['thread']['id']
            turn(client, thread, 'RECORDING_FIXTURE', input=[
                {'type':'text','text':'RECORDING_FIXTURE'},
                {'type':'localAudio','path':str(recording)}])
            turn(client, thread, 'Continue with the same recording.')
            client.close()
            client = fixture.client()
            client.rpc('thread/resume', {'threadId':thread})
            turn(client, thread, 'Continue after process restart.')
            records = fixture.upstream.requests
            assert len(records) == 3, records
            for record in records:
                body = record['body']
                messages = body.get('messages', body.get('input', []))
                parts = [part for message in messages if isinstance(message, dict)
                         for part in message.get('content', []) if isinstance(part, dict)]
                sounds = [part for part in parts if part.get('type') == 'input_audio']
                assert len(sounds) == 1, sounds
                expected = {'type':'input_audio', 'input_audio':{'data':encoded,'format':'wav'}} if target.startswith('chat/') else {
                    'type':'input_audio', 'audio_url':'data:audio/wav;base64,'+encoded}
                assert sounds[0] == expected, sounds[0]
            if '/' in target:
                audio_part = {'type':'input_audio', 'audio_url':'data:audio/wav;base64,'+encoded}
                history = [{'type':'message', 'role':'user', 'content':[{'type':'input_text','text':'Read the recording.'}]},
                    {'type':'function_call','name':'record','call_id':'fixture-recording','arguments':'{}'},
                    {'type':'function_call_output','call_id':'fixture-recording','output':[audio_part]}]
                histories = [history]
                if target.startswith('responses/'):
                    histories.append([{'role':'user','content':[{'type':'input_audio','file_id':'file-fixture-recording'}]}])
                for history in histories:
                    payload = json.dumps({'model':target,'input':history,'stream':True}).encode()
                    request_headers = {**fixture.headers, 'Content-Type':'application/json'}
                    with urllib.request.urlopen(urllib.request.Request(fixture.base+'/v1/responses', payload, request_headers), timeout=15) as response:
                        assert response.status == 200
                        assert 'response.completed' in response.read().decode()
                    forwarded = fixture.upstream.requests[-1]['body']
                    if target.startswith('chat/'):
                        sounds = [part for part in forwarded['messages'][-1]['content'] if part['type'] == 'input_audio']
                        assert sounds == [{'type':'input_audio', 'input_audio':{'data':encoded,'format':'wav'}}]
                        assert forwarded['messages'][-2]['tool_call_id'] == 'fixture-recording'
                    elif len(history) > 1:
                        assert forwarded['input'][-1]['output'] == [audio_part], forwarded
                    else:
                        assert forwarded['input'][0]['content'] == history[0]['content']
            else:
                with urllib.request.urlopen(urllib.request.Request(fixture.base+'/v1/models?client_version=0.169.0', headers=fixture.headers)) as response:
                    catalog_etag = response.headers['ETag']
                    response.read()
                payload = json.dumps({'model':target,'input':'Catalog revision fixture','stream':True}).encode()
                headers = {**fixture.headers,'Content-Type':'application/json'}
                with urllib.request.urlopen(urllib.request.Request(fixture.base+'/v1/responses',payload,headers),timeout=15) as response:
                    assert response.headers['X-Models-Etag'] == catalog_etag
                    assert 'response.completed' in response.read().decode()
            result = {'target':target, 'requests':len(records), 'passed':True,
                      'recording_bytes':recording.stat().st_size, 'process_resume':True}
            (evidence/label/'result.json').write_text(json.dumps(result, indent=2)+'\n')
            results.append(result)
            print(json.dumps(result), flush=True)
    return results
