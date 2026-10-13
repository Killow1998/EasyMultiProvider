"""Official agy + isolated EMP + synthetic Gemini upstream; never loads real auth."""
import argparse, concurrent.futures, contextlib, http.server, json, os, pathlib, re
import hashlib, signal, socket, subprocess, sys, tempfile, threading, time, urllib.error, urllib.request
from agy_ws_fixture import WS, safe_code

from e2e_environment import wait_for, stop
from e2e_refinements import Browser

requests = []
cancel_started, cancel_release = threading.Event(), threading.Event()
STAGE = 'prepare'
CLIENT = None
PATCH = ''

def stage(name):
    global STAGE
    STAGE = name
    print(json.dumps({'stage': name}), flush=True)

def transcript(body):
    for content in reversed(body.get('contents', [])):
        for part in content.get('parts', []):
            text = part.get('text', '')
            if '<USER_REQUEST>' in text:
                value = text.split('<USER_REQUEST>', 1)[1].split('</USER_REQUEST>', 1)[0].strip()
                try: return json.loads(value)
                except ValueError: pass
    return {}

class Gemini(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self):
        self.send_response(200);self.end_headers();self.wfile.write(b'{"models":[]}')
    def do_POST(self):
        length = int(self.headers.get('Content-Length', '0'))
        assert length < 12*1024*1024
        body = json.loads(self.rfile.read(length))
        main = 'gemini-3.8-flash' in self.path
        value = transcript(body) if main else {}
        text = json.dumps(value)
        names = [f['name'] for group in body.get('tools', []) for f in group.get('functionDeclarations', [])]
        requests.append({'main':main,'bytes':length,'native_tools':names,'transcript':value})
        if main:
            assert names == ['finish', 'manage_task'], names
            if 'CANCEL_FIXTURE' in text:
                cancel_started.set();cancel_release.wait(20)
                return
            if 'CLIENT_RESULT_' in text:
                assert all('CLIENT_RESULT_'+str(i) in text for i in range(4))
                proposal = {'answer':'TOOL_RESULTS_RECEIVED','tool_calls':[]}
            elif 'FOUR_TOOL_FIXTURE' in text:
                defs = value['tools'];assert len(defs)==4
                arguments = [ {'path':str(CLIENT/'read.txt')}, {'path':str(CLIENT/'new.txt'),'content':'CLIENT_RESULT_1'}, {}, {'cmd':'printf CLIENT_RESULT_3'} ]
                proposal = {'answer':'','tool_calls':[{'name':t['name'],'namespace':t['namespace'],'arguments':arguments[i],'input':PATCH if i==2 else ''} for i,t in enumerate(defs)]}
            elif 'ALPHA_FIXTURE' in text: proposal = {'answer':'ALPHA_FIXTURE','tool_calls':[]}
            elif 'BETA_FIXTURE' in text: proposal = {'answer':'BETA_FIXTURE','tool_calls':[]}
            elif 'COMPACTION' in text: proposal = {'answer':'Synthetic context summary retaining the task.','tool_calls':[]}
            elif 'EASY_MULTI_PROVIDER_TEST' in text: proposal = {'answer':'EASY_MULTI_PROVIDER_TEST','tool_calls':[]}
            else: proposal = {'answer':'AGY_OK','tool_calls':[]}
            part = {'functionCall':{'name':'finish','args':dict(proposal,toolAction='Returning client result',toolSummary='Client result')}}
        else: part = {'text':'Synthetic title'}
        response = {'candidates':[{'index':0,'content':{'role':'model','parts':[part]},'finishReason':'STOP'}],
                    'usageMetadata':{'promptTokenCount':100,'cachedContentTokenCount':25,'candidatesTokenCount':10,'thoughtsTokenCount':3,'totalTokenCount':113}}
        streaming = 'stream' in self.path.lower()
        data = ('data: '+json.dumps(response)+'\n\n').encode() if streaming else json.dumps(response).encode()
        self.send_response(200);self.send_header('Content-Type','text/event-stream' if streaming else 'application/json');self.send_header('Content-Length',str(len(data)));self.end_headers()
        try:self.wfile.write(data)
        except (BrokenPipeError,ConnectionResetError):pass

def main():
    global CLIENT, PATCH
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=pathlib.Path,required=True);parser.add_argument('--agy',type=pathlib.Path,required=True);parser.add_argument('--output',type=pathlib.Path,required=True)
    args=parser.parse_args();proof={'official_cli':True,'client_executor':'synthetic client in a separate workspace','native_subscription_auth_verified':False,'paid_model_calls':0,'checks':[]}
    with tempfile.TemporaryDirectory(prefix='emp-agy-e2e-') as directory,contextlib.ExitStack() as stack:
        root=pathlib.Path(directory);home=root/'home';home.mkdir();bin_dir=root/'bin';bin_dir.mkdir()
        CLIENT=root/'client';CLIENT.mkdir()
        (CLIENT/'read.txt').write_text('CLIENT_RESULT_0');(CLIENT/'edit.txt').write_text('BEFORE\n')
        PATCH='*** Begin Patch\n*** Update File: '+str(CLIENT/'edit.txt')+'\n@@\n-BEFORE\n+CLIENT_RESULT_2\n*** End Patch'
        fixture_home=root/'official-home';settings=fixture_home/'.gemini/antigravity-cli';settings.mkdir(parents=True)
        (settings/'settings.json').write_text(json.dumps({'modelProvider':'gemini','permissions':{'deny':[x+'(*)' for x in ['read_file','write_file','command','unsandboxed','read_url','execute_url','mcp']]}}))
        upstream=http.server.ThreadingHTTPServer(('127.0.0.1',0),Gemini);stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever,daemon=True).start();stack.callback(upstream.shutdown);stack.callback(cancel_release.set)
        pids=root/'cli-pids.txt'
        wrapper = '#!/usr/bin/python3\nimport os,sys\n'
        wrapper += 'with open('+repr(str(pids))+',"a") as f:f.write(str(os.getpid())+"\\n")\n'
        wrapper += 'os.environ.update('+repr({'HOME':str(fixture_home),'USERPROFILE':str(fixture_home),'GEMINI_API_KEY':'fixture-not-real','GOOGLE_GEMINI_BASE_URL':f'http://127.0.0.1:{upstream.server_port}','HTTP_PROXY':'http://127.0.0.1:1','HTTPS_PROXY':'http://127.0.0.1:1','NO_PROXY':'127.0.0.1,localhost'})+')\n'
        wrapper += 'os.execv('+repr(str(args.agy.resolve()))+',['+repr(str(args.agy.resolve()))+']+sys.argv[1:])\n'
        (bin_dir/'agy').write_text(wrapper);(bin_dir/'agy').chmod(0o700)
        config=root/'config.json'
        config.write_text(json.dumps({'auto_enable_on_start':False,'providers':[{'id':'agy','name':'Antigravity fixture','execution_backend':'agy_cli','auth_mode':'agy_login','protocol':'responses','base_url':''}],
                                      'models':[{'id':'agy/model','provider':'agy','upstream_id':'gemini-3.8-flash-low','enabled':True}]}))
        env={'HOME':str(home),'USERPROFILE':str(home),'CODEX_HOME':str(root/'codex'),'XDG_CONFIG_HOME':str(root/'config'),'XDG_CACHE_HOME':str(root/'cache'),'PATH':str(bin_dir)+':/usr/local/bin:/usr/bin:/bin','LANG':'C.UTF-8','NO_PROXY':'127.0.0.1,localhost','HTTP_PROXY':'http://127.0.0.1:1','HTTPS_PROXY':'http://127.0.0.1:1'}
        for key in ['CODEX_HOME','XDG_CONFIG_HOME','XDG_CACHE_HOME']:pathlib.Path(env[key]).mkdir()
        log=stack.enter_context((root/'emp.log').open('w'))
        emp=subprocess.Popen([str(args.emp.resolve()),'serve','--config',str(config),'--port','0'],env=env,cwd=root,stdout=log,stderr=log,start_new_session=True);stack.callback(stop,emp)
        url=wait_for(lambda:re.search(r'Open in browser: (http://[^\s]+)',(root/'emp.log').read_text()),seconds=20)[1];base=url.split('/?')[0]
        token=wait_for(lambda:json.loads((root/'state/web-session.json').read_text())['token'] if (root/'state/web-session.json').exists() else None)
        def api(path,body=None,authorized=True):
            headers={'Origin':base,'Content-Type':'application/json'}
            if authorized:headers['X-EMP-Session']=token
            req=urllib.request.Request(base+path,data=json.dumps(body).encode() if body is not None else None,headers=headers)
            try:
                with urllib.request.urlopen(req,timeout=45) as r:return r.status,json.load(r)
            except urllib.error.HTTPError as e:return e.code,json.load(e)
        stage('availability_and_catalog')
        assert api('/api/runtime/agy-cli',authorized=False)[0]==401
        assert api('/api/runtime/agy-cli')[1]=={'available':True,'sign_in_status':'unverified'}
        assert not pids.exists();proof['checks'].append('Availability is authenticated and never starts the CLI')
        code,models=api('/api/providers/discover',{'provider':'agy'});assert code==200,(code,models)
        assert models['available']>0 and not requests
        proof['model_count']=models['available'];proof['checks'].append('Official model discovery performs no inference')
        body={'model':'agy/model','input':[{'role':'user','content':'FOUR_TOOL_FIXTURE'}],'reasoning':{'effort':'low'},'tools':[{'type':'namespace','name':'functions','tools':[
            {'type':'function','name':'read_file','parameters':{'type':'object'}}, {'type':'function','name':'write_file','parameters':{'type':'object'}},
            {'type':'custom','name':'apply_patch','format':{'type':'text'}},{'type':'function','name':'exec_command','parameters':{'type':'object'}},
            {'type':'function','name':'send_email','parameters':{'type':'object'}}]}]}
        stage('client_tool_round_trip')
        code,response=api('/v1/responses',body);assert code==200,(code,response)
        assert len(response['output'])==4 and all(c['namespace']=='functions' for c in response['output'])
        proof['first_turn_usage']=response['usage']
        # Validate the harmless files the official CLI creates after first use.
        profile=home/'.gemini/config';profile.mkdir(parents=True,exist_ok=True)
        for relative in ['mcp_config.json','projects/default-cli-project.json']:
            generated=fixture_home/'.gemini/config'/relative
            if generated.exists():
                target=profile/relative;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(generated.read_bytes())
        history=body['input'][:]
        assert not (CLIENT/'new.txt').exists() and (CLIENT/'edit.txt').read_text()=='BEFORE\n'
        for i,call in enumerate(response['output']):
            if i==0:
                assert call['name']=='read_file';params=json.loads(call['arguments']);result=pathlib.Path(params['path']).read_text()
            elif i==1:
                assert call['name']=='write_file';params=json.loads(call['arguments']);assert params['path']==str(CLIENT/'new.txt')
                pathlib.Path(params['path']).write_text(params['content']);result=pathlib.Path(params['path']).read_text()
            elif i==2:
                assert call['name']=='apply_patch' and call['input']==PATCH
                # The synthetic client implements this single exact edit. EMP
                # never receives ownership of the fixture's workspace tools.
                (CLIENT/'edit.txt').write_text('CLIENT_RESULT_2\n');result=(CLIENT/'edit.txt').read_text().strip()
            else:
                assert call['name']=='exec_command' and json.loads(call['arguments'])=={'cmd':'printf CLIENT_RESULT_3'}
                result=subprocess.check_output(['/usr/bin/printf','CLIENT_RESULT_3'],cwd=CLIENT,text=True,timeout=5)
            assert result=='CLIENT_RESULT_'+str(i)
            history += [call,{'type':'custom_tool_call_output' if call['type']=='custom_tool_call' else 'function_call_output','call_id':call['call_id'],'output':result}]
        second=dict(body,input=history)
        code,response=api('/v1/responses',second);assert code==200,(code,response)
        assert response['output'][0]['content'][0]['text']=='TOOL_RESULTS_RECEIVED'
        assert proof['first_turn_usage']==response['usage']
        proof['checks'].append('Four namespaced proposals execute in the synthetic client workspace and all results return; EMP does not change its files; per-request usage does not accumulate')
        stage('streaming_and_compaction')
        req=urllib.request.Request(base+'/v1/responses',data=json.dumps({'model':'agy/model','input':'AGY_OK','stream':True}).encode(),headers={'Origin':base,'X-EMP-Session':token,'Content-Type':'application/json'})
        with urllib.request.urlopen(req,timeout=45) as stream:
            raw=stream.read().decode()
        events=[json.loads(line[6:]) for line in raw.splitlines() if line.startswith('data: ') and line[6:]!='[DONE]']
        assert sum(e['type']=='response.completed' for e in events)==1
        assert ''.join(e.get('delta','') for e in events if e['type']=='response.output_text.delta')=='AGY_OK'
        ws=WS(base,token);stack.callback(ws.close)
        first={'type':'response.create','model':'agy/model','input':'AGY_OK','reasoning':{'effort':'low'}}
        facts,reply,events=ws.turn(first,'AGY_OK');assert facts['completed'] and facts['exact_reply']
        assert all(e.get('response',{}).get('id',reply['id'])==reply['id'] for e in events)
        before=len(requests)
        facts,_,_=ws.turn(dict(first,previous_response_id=reply['id']),'')
        assert facts['error_code']=='previous_response_not_found' and len(requests)==before
        facts,_,_=ws.turn(first,'AGY_OK');assert facts['completed'] and facts['exact_reply']
        ws.close()
        code,compacted=api('/v1/responses',{'model':'agy/model','input':[{'role':'user','content':'COMPACTION synthetic task'},{'type':'compaction_trigger'}]})
        assert code==200 and any(item['type']=='compaction' for item in compacted['output'])
        proof['checks'].append('SSE and WebSocket deliver one complete response; prior-only input is rejected before inference and full-history recovery succeeds; compaction returns its envelope')
        stage('concurrency_and_preflight')
        def simple(marker):return api('/v1/responses',{'model':'agy/model','input':marker,'reasoning':{'effort':'low'}})
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            futures=[pool.submit(simple,marker) for marker in ['ALPHA_FIXTURE','BETA_FIXTURE']]
            for marker,future in zip(['ALPHA_FIXTURE','BETA_FIXTURE'],futures):
                code,result=future.result();assert code==200 and result['output'][0]['content'][0]['text']==marker,(code,result)
        proof['checks'].append('Concurrent conversations keep their own input and output')
        before=len(requests)
        code,error=api('/v1/responses',{'model':'agy/model','input':[{'role':'user','content':[{'type':'input_image','image_url':'data:image/png;base64,synthetic'}]}]})
        assert code==400 and error['error']['code']=='agy_cli_unsupported_input' and len(requests)==before
        hooks=home/'.gemini/config';hooks.mkdir(parents=True,exist_ok=True)
        (hooks/'hooks.json').write_text(json.dumps({'unsafe':{'PreInvocation':[{'command':'touch SHOULD_NOT_EXIST'}]}}))
        code,error=simple('AGY_OK');assert code==409 and len(requests)==before
        (hooks/'hooks.json').unlink()
        settings_path=home/'.gemini/antigravity-cli/settings.json';settings_path.parent.mkdir(parents=True,exist_ok=True)
        settings_path.write_text(json.dumps({'hooks':{'PreInvocation':[{'command':'touch SHOULD_NOT_EXIST'}]}}))
        code,error=simple('AGY_OK');assert code==409 and len(requests)==before
        settings_path.unlink()
        code,error=api('/v1/responses',{'model':'agy/model','input':'AGY_OK','max_output_tokens':10})
        assert code==400 and error['error']['code']=='agy_cli_unsupported_parameter' and len(requests)==before
        proof['checks'].append('Media, unsupported generation limits and both hook locations fail before inference')
        # Real UI against the isolated backend: save retains CLI auth and no API key.
        stage('firefox_service_settings')
        browser=Browser(stack,root);browser.navigate(url)
        token=json.loads((root/'state/web-session.json').read_text())['token']
        browser.execute('openServiceChooser();')
        assert browser.execute('return !!document.querySelector("[data-ui-action=agy-local-choose]");')
        browser.execute('document.querySelector("[data-ui-action=agy-local-choose]").click();')
        wait_for(lambda:browser.execute('return document.getElementById("modal_agy_status")?.textContent.includes("agy");'))
        browser.execute('document.getElementById("modal_agy_name").value="Agy verified fixture";document.getElementById("modal_submit").click();')
        wait_for(lambda:api('/api/config')[1]['providers'][0]['name']=='Agy verified fixture')
        saved=api('/api/config')[1]['providers'][0]
        assert saved['execution_backend']=='agy_cli' and saved['auth_mode']=='agy_login' and not saved.get('api_key_set')
        proof['checks'].append('Firefox add/edit service flow saves official CLI sign-in without an API key')
        browser.execute('testModel("agy/model");')
        wait_for(lambda:browser.execute('return document.body.textContent.includes("测试成功") || document.body.textContent.includes("Test succeeded");'),seconds=30)
        proof['checks'].append('Firefox model test sends a supported low-effort request')
        # Disconnect while official CLI is waiting on the synthetic upstream.
        stage('disconnect_cancellation')
        payload=json.dumps({'model':'agy/model','input':'CANCEL_FIXTURE'}).encode()
        hostport=base.split('://')[1];host,port=hostport.split(':');sock=socket.create_connection((host,int(port)),timeout=10)
        sock.sendall(('POST /v1/responses HTTP/1.1\r\nHost: '+hostport+'\r\nOrigin: '+base+'\r\nX-EMP-Session: '+token+'\r\nContent-Type: application/json\r\nContent-Length: '+str(len(payload))+'\r\n\r\n').encode()+payload)
        if not cancel_started.wait(20):
            sock.settimeout(2)
            response=sock.recv(65536);sock.close()
            header,_,data=response.partition(b'\r\n\r\n')
            try: code=safe_code(json.loads(data).get('error',{}).get('code'))
            except ValueError: code=None
            print(json.dumps({'stage':STAGE,'http_status':header.split(b' ')[1].decode() if header else None,'error_code':code}),flush=True)
            raise AssertionError('cancel request did not reach synthetic upstream')
        started=time.monotonic();sock.close()
        def alive():
            return [int(pid) for pid in pids.read_text().splitlines() if pathlib.Path('/proc/'+pid).exists()]
        wait_for(lambda:not alive(),seconds=8);proof['cancel_elapsed_ms']=round((time.monotonic()-started)*1000)
        proof['checks'].append('Client disconnect stops the owned official CLI; no replay or remaining child')
        code,selected=api('/api/providers/discover',{'provider':'agy','cached':True,'selected':['gemini-3.8-flash-low']})
        assert code==200
        assert any(m['id']=='agy/gemini-3.8-flash-low' for m in api('/v1/models')[1]['data'])
        proof['checks'].append('Selecting a discovered model publishes its exact official ID to the client catalog')
        for label,path in [('emp',args.emp),('agy',args.agy)]:
            digest=hashlib.sha256()
            with path.open('rb') as file:
                for chunk in iter(lambda:file.read(1024*1024),b''):digest.update(chunk)
            proof[label+'_sha256']=digest.hexdigest()
        proof['main_inferences']=sum(r['main'] for r in requests)
        proof['unexpected_native_tools']=any(r['main'] and r['native_tools']!=['finish','manage_task'] for r in requests)
        proof['all_passed']=True
    args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(proof,indent=2)+'\n');print(json.dumps(proof),flush=True)

if __name__=='__main__':
    try: main()
    except Exception as error:
        # WebDriver exceptions can include the one-use bootstrap URL. Do not
        # print exception text, request bodies, headers or local login paths.
        details={'all_passed':False,'stage':STAGE,'error_type':type(error).__name__}
        if isinstance(error,urllib.error.HTTPError):
            details['http_status']=error.code
            try: details['error_code']=safe_code(json.loads(error.read(65536)).get('error',{}).get('code'))
            except ValueError: pass
        print(json.dumps(details),flush=True)
        sys.exit(1)
