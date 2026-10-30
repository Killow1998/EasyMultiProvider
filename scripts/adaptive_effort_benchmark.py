"""Opt-in live HumanEval+ Mini pilot: fixed low/xhigh versus per-step effort.

Uses an isolated EMP, the supplied Native login, and a networkless Python tool.
Never changes the running EMP or Codex configuration. Model calls consume quota.
"""
import argparse
import contextlib
import gzip
import hashlib
import json
import random
import shutil
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from datetime import datetime, timezone
from pathlib import Path

from scripts.shadow_frontend.local_backend import request, start_backend


def inference(base, headers, body):
    req = urllib.request.Request(base+'/v1/responses', json.dumps(body).encode(),
                                 {'Content-Type':'application/json', **headers})
    try:
        response = urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req, timeout=120)
    except urllib.error.HTTPError as error:
        return {'error': {'status':error.code, 'body':error.read().decode()}}
    with response:
        for line in response:
            if not line.startswith(b'data: ') or line.strip() == b'data: [DONE]':
                continue
            event = json.loads(line[6:])
            if event.get('type') in ('response.completed', 'response.incomplete'):
                return event['response']
            if event.get('type') in ('error', 'response.failed'):
                return {'error':event}
    return {'error':'stream ended without a response'}


def evaluate(task, code, folder):
    (folder/'solution.py').write_text(code)
    (folder/'task.json').write_text(json.dumps(task))
    runner = '''import copy,json
task=json.load(open('task.json'))
expected={}
exec(task['prompt']+task['canonical_solution'], expected)
actual={}
try:
 exec(open('solution.py').read(),actual)
 failures=[]
 inputs=task['base_input']+task['plus_input']
 for args in inputs:
  wanted=expected[task['entry_point']](*copy.deepcopy(args))
  got=actual[task['entry_point']](*copy.deepcopy(args))
  if wanted!=got:
   failures.append({'input':repr(args),'expected':repr(wanted),'actual':repr(got)})
 print(json.dumps({'passed':not failures,'tests':len(inputs),'failures':failures[:3]}))
except Exception as error:
 print(json.dumps({'passed':False,'error':str(error)}))
'''
    (folder/'check.py').write_text(runner)
    command = ['bwrap', '--unshare-all', '--die-with-parent', '--ro-bind', '/usr', '/usr',
               '--ro-bind', '/lib', '/lib', '--ro-bind', '/lib64', '/lib64', '--proc', '/proc',
               '--dev', '/dev', '--tmpfs', '/tmp', '--ro-bind', str(folder), '/work',
               '--chdir', '/work', '/usr/bin/python3', '-I', '-S', 'check.py']
    try:
        result = subprocess.run(command, text=True, capture_output=True, timeout=10)
    except subprocess.TimeoutExpired:
        return {'passed':False, 'error':'Tests exceeded 10 seconds'}
    if result.returncode:
        return {'passed':False, 'error':result.stderr[-1200:]}
    return json.loads(result.stdout.splitlines()[-1])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--auth', type=Path, required=True)
    parser.add_argument('--catalog', type=Path, required=True)
    parser.add_argument('--dataset', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--tasks', type=int, default=6)
    parser.add_argument('--modes', nargs='+', choices=['low', 'xhigh', 'auto'], default=['low','xhigh','auto'])
    parser.add_argument('--model', default='gpt-6-astra')
    parser.add_argument('--classifier', default='gpt-6-luna')
    parser.add_argument('--allow-live-calls', action='store_true', required=True)
    args = parser.parse_args()
    tasks = [json.loads(line) for line in gzip.open(args.dataset, 'rt')]
    tasks = random.Random(20261029).sample(tasks, args.tasks)
    # These pilot tasks have deterministic outputs and no approximate comparator.
    assert all(task['atol'] == 0 for task in tasks)
    args.output.mkdir(parents=True)
    results = {'dataset':'HumanEval+ Mini v0.1.10', 'seed':20261029,
               'started_at':datetime.now(timezone.utc).isoformat(),
               'emp_sha256':hashlib.sha256(args.emp.read_bytes()).hexdigest(),
               'dataset_sha256':hashlib.sha256(args.dataset.read_bytes()).hexdigest(),
               'model':args.model, 'classifier':args.classifier,
               'tasks':[task['task_id'] for task in tasks], 'runs':[]}
    with tempfile.TemporaryDirectory(prefix='emp-effort-live-') as temporary, contextlib.ExitStack() as stack:
        root = Path(temporary)
        native = root/'native'
        native.mkdir(mode=0o700)
        shutil.copy2(args.auth, native/'auth.json')
        catalog = json.loads(args.catalog.read_text())
        catalog['models'] = [model for model in catalog['models'] if model['slug'] in (args.model,args.classifier)]
        assert {model['slug'] for model in catalog['models']} == {args.model,args.classifier}
        (root/'models.json').write_text(json.dumps(catalog))
        config = {'auto_enable_on_start':False, 'codex_base_url':'https://chatgpt.com/backend-api/codex',
                  'native_catalog_path':str(root/'models.json')}
        base, _, _, headers = start_backend(stack, args.emp.resolve(), root, config, {'CODEX_HOME':str(native)})
        status, available = request(base+'/v1/models?client_version=0.165.0', headers=headers)
        assert status == 200 and any(model['slug'] == args.model for model in available['models']), available
        for task_index, task in enumerate(tasks):
            modes = args.modes[task_index % len(args.modes):]+args.modes[:task_index % len(args.modes)]
            for mode in modes:
                status, _ = request(base+'/api/config', {'adaptive_effort':{
                    'enabled':mode == 'auto', 'classifier_model':args.classifier, 'max_effort':'xhigh'}}, headers)
                assert status == 200
                thread = str(uuid.uuid4())
                effort = 'low' if mode == 'low' else 'xhigh'
                run = {'task':task['task_id'], 'mode':mode, 'thread_id':thread, 'passed':False,
                       'main_responses':[], 'tool_results':[]}
                folder = args.output/(task['task_id'].replace('/','-')+'-'+mode)
                folder.mkdir()
                body = {'model':args.model, 'stream':True, 'store':False,
                        'reasoning':{'effort':effort, 'summary':'auto'},
                        'include':['reasoning.encrypted_content'], 'parallel_tool_calls':False,
                        'text':{'format':{'type':'text'}}, 'tool_choice':'auto', 'prompt_cache_key':thread,
                        'instructions':'Implement the requested Python function. Submit complete executable Python source to run_tests. Fix failures using its results. Once tests pass, reply briefly that the implementation is verified. Do not use any other tools.',
                        'tools':[{'type':'function','name':'run_tests','description':'Test a complete Python implementation against the benchmark cases.',
                                  'strict':True,'parameters':{'type':'object','properties':{'code':{'type':'string'}},'required':['code'],'additionalProperties':False}}],
                        'client_metadata':{'x-codex-turn-metadata':json.dumps({'thread_id':thread,'turn_id':thread,'reasoning_effort':effort})},
                        'input':[{'role':'user','content':task['prompt']}]}
                started = time.monotonic()
                for step in range(4):
                    response = inference(base, {**headers,'thread-id':thread,
                        'originator':'codex_cli_rs','User-Agent':'codex_cli_rs/0.165.0'}, body)
                    (folder/f'response-{step}.json').write_text(json.dumps(response))
                    if response.get('error') is not None:
                        run['error'] = response['error']
                        break
                    run['main_responses'].append({'usage':response.get('usage'), 'status':response.get('status')})
                    output = response.get('output', [])
                    body['input'].extend(output)
                    calls = [item for item in output if item.get('type') == 'function_call']
                    if not calls:
                        run['passed'] = bool(run['tool_results'] and run['tool_results'][-1]['passed'])
                        break
                    for call in calls:
                        assert call['name'] == 'run_tests'
                        check = evaluate(task, json.loads(call['arguments'])['code'], folder)
                        run['tool_results'].append(check)
                        body['input'].append({'type':'function_call_output','call_id':call['call_id'],'output':json.dumps(check)})
                run['seconds'] = round(time.monotonic()-started,3)
                status, calls = request(base+'/api/calls?period=1d', headers=headers)
                assert status == 200
                run['calls'] = [row for row in calls['records'] if row.get('thread_id') == thread]
                run['input_tokens'] = sum(row.get('input_tokens') or 0 for row in run['calls'])
                run['output_tokens'] = sum(row.get('output_tokens') or 0 for row in run['calls'])
                run['unknown_usage_calls'] = sum(row.get('input_tokens') is None or
                                                 row.get('output_tokens') is None for row in run['calls'])
                results['runs'].append(run)
                results['updated_at'] = datetime.now(timezone.utc).isoformat()
                (args.output/'results.json').write_text(json.dumps(results,indent=2))
                print(json.dumps({key:run[key] for key in ['task','mode','passed','seconds','input_tokens','output_tokens']}),flush=True)
                if 'error' in run:
                    raise SystemExit('Stopped on request failure; inspect the isolated result before continuing.')


if __name__ == '__main__':
    main()
