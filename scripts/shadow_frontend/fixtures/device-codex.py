#!/usr/bin/python3
"""Synthetic app-server authentication for isolated onboarding acceptance."""
import base64
import json
import os
from pathlib import Path
import sys
import threading
import time

if '--version' in sys.argv:
    print('codex-cli', os.environ.get('EMP_FIXTURE_CODEX_VERSION', '0.163.0'))
    raise SystemExit(0)

control = Path(__file__).with_name('login-control')

def emit(value):
    print(json.dumps(value), flush=True)

def complete():
    started = time.monotonic()
    while True:
        mode = control.read_text().strip() if control.exists() else 'pending'
        if mode == 'auto' and time.monotonic() - started >= 8:
            mode = 'complete'
        if mode in ('complete', 'fail'):
            if mode == 'complete':
                Path(os.environ['CODEX_HOME'], 'auth.json').write_text(json.dumps({
                    'auth_mode':'chatgpt', 'tokens':{'account_id':'fixture-login',
                    'access_token':'fixture-access', 'refresh_token':'fixture-refresh', 'id_token':'fixture-id'}}))
            emit({'method':'account/login/completed','params':{'loginId':'fixture-login','success':mode=='complete'}})
            return
        time.sleep(.1)

for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    result = {}
    if method == 'account/login/start':
        result = {'type':'chatgptDeviceCode','loginId':'fixture-login',
            'verificationUrl':'https://auth.openai.com/codex/device','userCode':'ABCD-EFGH'}
        if request['params']['type']=='chatgpt':
            result={'type':'chatgpt','loginId':'fixture-login','authUrl':'https://auth.openai.com/oauth/authorize?fixture=1'}
        emit({'id':request['id'],'result':result})
        threading.Thread(target=complete,daemon=True).start()
        continue
    if method == 'account/read':
        result = {'account':None,'requiresOpenaiAuth':True}
        auth_file = Path(os.environ['CODEX_HOME'], 'auth.json')
        if auth_file.exists():
            tokens = json.loads(auth_file.read_text()).get('tokens', {})
            token = tokens.get('access_token', '')
            if token.startswith('fixture.'):
                claims = json.loads(base64.urlsafe_b64decode(token.split('.')[1] + '==='))
                if claims['exp'] > time.time():
                    result['account'] = {'type':'chatgpt','email':'synthetic@example.invalid','planType':'pro'}
    elif method == 'model/list':
        result = {'data':[],'nextCursor':None}
    elif method == 'account/rateLimits/read':
        result = {'rateLimits':{'primary':{'usedPercent':0}}}
    elif method == 'account/rateLimitResetCredit/consume':
        result = {'outcome':'nothingToReset'}
    if 'id' in request:
        emit({'id':request['id'],'result':result})
