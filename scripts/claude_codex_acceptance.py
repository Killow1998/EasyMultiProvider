"""Drive the installed Codex runtime through EMP's real Claude TUI fixture."""
import json
import os
from pathlib import Path
import subprocess
import sys

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from scripts.native_restore_acceptance.app_server import AppServer
from scripts.shadow_frontend.local_backend import request


def run(codex,base,headers,root,evidence,on_text=None):
    home=root/'real-codex';home.mkdir()
    (home/'fixture.txt').write_text('PUBLIC_TOOL_RESULT')
    version=subprocess.check_output([str(codex),'--version'],env={'PATH':os.environ['PATH'],'HOME':str(home),'CODEX_HOME':str(home)},text=True).strip()
    status,catalog=request(base+'/v1/models?client_version='+version.split()[-1],headers=headers)
    assert status==200,catalog
    (home/'models.json').write_text(json.dumps(catalog))
    (home/'config.toml').write_text(
        'model="cpa/sonnet"\nmodel_provider="fixture"\n'
        f'model_catalog_json={json.dumps(str(home/"models.json"))}\n'
        '[features]\nmemories=false\n'
        '[model_providers.fixture]\nname="TUI acceptance"\nwire_api="responses"\n'
        f'base_url={json.dumps(base+"/v1")}\n'
        'requires_openai_auth=false\nenv_key="EMP_TEST_KEY"\nsupports_websockets=false\n'
        'request_max_retries=0\nstream_max_retries=0\n')
    env={'PATH':os.environ['PATH'],'HOME':str(home),'CODEX_HOME':str(home),'XDG_CONFIG_HOME':str(home),
        'EMP_TEST_KEY':headers['Authorization'].removeprefix('Bearer '),
        'HTTP_PROXY':'http://127.0.0.1:9','HTTPS_PROXY':'http://127.0.0.1:9',
        'ALL_PROXY':'http://127.0.0.1:9','NO_PROXY':'127.0.0.1,localhost'}
    version=subprocess.check_output([str(codex),'--version'],env=env,text=True).strip()
    evidence.mkdir(parents=True,exist_ok=True)
    tool_calls,answers=0,[]
    client=AppServer(codex,env,evidence/'first.stderr',address_space_limit_bytes=2*1024**4)
    try:
        thread=client.rpc('thread/start',{'model':'cpa/sonnet','cwd':str(home),
            'approvalPolicy':'never','sandbox':'read-only','dynamicTools':[{'name':'read_public',
            'description':'Read the public fixture','inputSchema':{'type':'object','properties':{'path':{'type':'string'}}}}]})['thread']['id']
        for index in range(3):
            if index==2:
                client.close()
                client=AppServer(codex,env,evidence/'resumed.stderr',address_space_limit_bytes=2*1024**4)
                client.rpc('thread/resume',{'threadId':thread})
            client.rpc('turn/start',{'threadId':thread,'input':[{'type':'text',
                'text':f'PUBLIC_TASK_MARKER turn {index}: read the fixture and answer.'}]})
            answer=''
            while True:
                event=client.read_message(timeout=60)
                client.events.append(event)
                if event.get('method')=='item/agentMessage/delta' and on_text:
                    on_text()
                if event.get('method')=='item/tool/call':
                    assert event['params']['tool']=='read_public',event
                    tool_calls+=1
                    client.send({'id':event['id'],'result':{'success':True,'contentItems':[
                        {'type':'inputText','text':(home/'fixture.txt').read_text()}]}})
                elif event.get('method')=='item/completed':
                    item=event['params']['item']
                    if item['type']=='agentMessage':answer+=item['text']
                elif event.get('method')=='turn/completed':
                    assert event['params']['turn']['status']=='completed',event
                    break
                elif 'id' in event and 'method' in event:
                    raise AssertionError(event)
            expected=('PUBLIC_PROGRESS' if index==0 and on_text else '')+'PUBLIC_FINAL_ANSWER'
            assert answer==expected,(answer,client.events[-5:])
            answers.append(answer)
    finally:
        client.close()
    assert tool_calls==1,tool_calls
    return {'version':version,'turns':len(answers),'tool_calls':tool_calls,'resumed':True,'passed':True}
