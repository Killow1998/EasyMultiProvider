"""Observe default real-data Shadow, isolated accounting and opt-in demos."""
import argparse
import base64
import contextlib
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile
import time

from e2e_environment import request, start_backend, stop, wait_for
from e2e_refinements import Browser

MAIN = '11111111-1111-4111-8111-111111111111'
REVIEW = '22222222-2222-4222-8222-222222222222'
SIDE = '33333333-3333-4333-8333-333333333333'
ORPHAN = '44444444-4444-4444-8444-444444444444'
OWNER = 'account:' + hashlib.sha256(b'emp-usage-account\0fixture').hexdigest()


def seed(state, home):
    with sqlite3.connect(home / 'state_5.sqlite') as database:
        database.executescript('CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT,source TEXT,thread_source TEXT);'
            'CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT);')
        database.executemany('INSERT INTO threads VALUES (?,?,?,?)',[
            (MAIN,'Raw source conversation','"cli"','user'),
            (REVIEW,'Guardian review','{"subagent":{"other":"guardian"}}','guardian_review'),
            (SIDE,'Sidechat','"cli"','user')])
        database.execute('INSERT INTO thread_spawn_edges VALUES (?,?)',(MAIN,REVIEW))
    for index,(session,model,amount,parent,kind) in enumerate([
        (MAIN,'gpt-6.1-sol',100,'',''),(REVIEW,'codex-auto-review',20,'',''),
        (SIDE,'gpt-6-luna',30,MAIN,'sidechat'),(ORPHAN,'unknown-model',40,'','')]):
        add(state,'source-'+str(index),session,model,amount,parent,kind)


def add(state, identifier, session, model, amount, parent='', kind=''):
    now=time.time()
    facts={'request_id':identifier,'thread_id':session,'session_id':session,'model_id':model,
        'route':'responses','success':True,'status':200,'input_tokens':amount,'output_tokens':1,
        'usage_event_id':identifier,'parent_thread_id':parent,'session_kind':kind}
    with sqlite3.connect(state / 'usage.sqlite3') as database:
        database.execute('INSERT INTO usage_events(id,observed_at,category,owner,model,service_tier,operation,input_tokens,output_tokens,cost_nanos,rates) VALUES (?,?,?,?,?,?,?,?,?,?,?)',
            (identifier,now,'native',OWNER,model,'default','responses',amount,1,amount,'{}'))
        database.execute('INSERT INTO usage_sessions VALUES (?,?)',(identifier,session))
        database.execute('INSERT INTO call_records VALUES (?,?,?,?,?,?,?,?,?,?,?)',
            (identifier,'responses',now-1,now,'source-provider','',model,session,'completed',json.dumps(facts),None))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    checks=[]
    with tempfile.TemporaryDirectory(prefix='emp-real-shadow-test-') as temporary,contextlib.ExitStack() as stack:
        root=Path(temporary)
        source=root/'source';source.mkdir()
        home=root/'native';home.mkdir()
        (home/'auth.json').write_text('{"tokens":{"access_token":"source-only-secret","account_id":"fixture"}}')
        (home/'config.toml').write_text('model_context_window = 100000\n[features.async_compaction]\nenabled = false\nmodel_async_compact_threshold_percent = 71\n')
        base,_,state,headers,upstream=start_backend(stack,args.emp.resolve(),source,
            {'auto_enable_on_start':False,'providers':[],'models':[], 'codex_base_url':'http://127.0.0.1:9/fixture'},
            {'CODEX_HOME':str(home),'PATH':'/usr/bin:/bin'})
        assert request(base+'/api/usage?sessions=true',headers=headers)[0]==200
        seed(state,home)
        source_config=(source/'config.json').read_bytes()
        source_auth=(home/'auth.json').read_bytes()
        raw_metadata=(home/'state_5.sqlite').read_bytes()
        log=stack.enter_context((root/'shadow.log').open('w'))
        child=subprocess.Popen([sys.executable,str(Path(__file__).with_name('shadow-web.py')),
            '--emp',str(args.emp.resolve()),'--port','0','--backend-port',base.rsplit(':',1)[1],
            '--state-dir',str(state),'--codex-home',str(home),'--scratch-dir',str(root/'scratch')],
            stdout=log,stderr=log)
        stack.callback(stop,child)
        def ready():
            content=(root/'shadow.log').read_text()
            match=re.search(r'EMP real-data Shadow: (http://[^\s]+)',content)
            if child.poll() is not None:
                raise AssertionError(content)
            return match[1].rstrip('/') if match else None
        shadow=wait_for(ready,seconds=40)
        browser=Browser(stack,root)
        browser.navigate(shadow)
        wait_for(lambda:browser.execute('return shadowSnapshot?.usage_events===4;'))
        assert browser.execute('return window.EMP_REAL_SHADOW===true && !window.EMP_IMPORT_DEMO && !document.getElementById("demo_import_banner") && document.getElementById("integration_toggle").disabled;')
        browser.execute('window.EMP_REVIEW_ACTION("background");')
        wait_for(lambda:browser.execute('return !!document.getElementById("settings-native-title");'))
        assert browser.execute('return location.origin===arguments[0] && window.EMP_REAL_SHADOW===true && document.querySelectorAll("#modal_body [role=region]").length===4;',shadow)
        browser.execute('closeModal();')
        own={'X-EMP-Session':browser.execute('return localStorage.getItem("emp_management_session_v1");')}
        query='/api/usage?sessions=true&start=0&end='+str(time.time()+3600)
        status,bills=request(shadow+query,headers=own)
        assert status==200 and bills['total']==1 and bills['totals']['requests']==4 and bills['unassigned']['requests']==1,bills
        assert bills['records'][0]['session_name']=='Raw source conversation'
        scoped=request(shadow+query+'&account=%40native',headers=own)[1]
        assert scoped['total']==1 and scoped['totals']['requests']==4 and scoped['records'][0]['session_name']=='Raw source conversation'
        assert request(shadow+query+'&owner=another-owner',headers=own)[1]['totals']['requests']==0
        assert request(shadow+query+'&owner='+('x'*257),headers=own)[0]==400
        assert request(shadow+query+'&account=%40native&owner=ambiguous',headers=own)[0]==400
        detail=request(shadow+'/api/usage?series=true&conversation=true&session='+MAIN+'&start=0&end='+str(time.time()+3600),headers=own)[1]
        assert detail['totals']['requests']==3 and {row['session_kind'] for row in detail['components']}=={'conversation','review','sidechat'}
        assert not any((root/'scratch').rglob('auth.json'))
        for path in (root/'scratch').rglob('config.json'):
            config=json.loads(path.read_text())
            assert config['providers']==[] and config['accounts']==[]
        checks.append('Default launcher uses source ledger/metadata and candidate attribution, without seeding examples or copying credentials')
        checks.append('Account-scoped bills use source-resolved identity with candidate accounting; explicit owner selection is bounded and ambiguous filters are rejected')
        for language in ['zh-CN','en']:
            browser.execute('setLanguage(arguments[0]);document.getElementById("shadow_review_button").click();',language)
            assert browser.execute('return [...document.querySelectorAll("#modal_body [data-shadow-demo]")].map(node=>node.dataset.shadowDemo);') == ['lifecycle']
            browser.execute('closeModal();document.querySelector("[data-i18n=statistics]").click();document.querySelector("[data-stats-view=sessions]").click();')
            wait_for(lambda:browser.execute('return document.querySelectorAll("[data-session-bill]").length===1;'))
            assert browser.execute('return document.querySelector("[data-session-bill]").textContent.includes("Raw source conversation");')
            _,png=request(browser.base+'/screenshot')
            args.output.with_name(args.output.stem+'-'+language+'.png').write_bytes(base64.b64decode(png['value']))
            browser.execute('closeModal();')
        checks.append('Both languages show the source conversation and unresolved usage in the real review path')
        add(state,'later-source',MAIN,'gpt-6.1-sol',50)
        assert request(shadow+query,headers=own)[1]['totals']['requests']==4
        browser.execute('statistics.open();')
        wait_for(lambda:browser.execute('return !document.querySelector("#presentation_stats [data-report-action=refresh]").disabled;'))
        browser.execute('document.querySelector("#presentation_stats [data-report-action=refresh]").click();')
        wait_for(lambda:browser.execute('return shadowSnapshot.usage_events===5 && !document.querySelector("#presentation_stats [data-report-action=refresh]").disabled;'))
        assert request(shadow+query,headers=own)[1]['totals']['requests']==5
        for route in ['/api/quit','/api/integration/restore','/api/config','/v1/responses']:
            assert request(shadow+route,{},own)[0]==403,route
        assert request(shadow+'/api/shadow/snapshot',{})[0]==401
        assert request(base+'/healthz')[0]==200
        checks.append('Snapshot refresh reads new source rows; direct lifecycle/config/inference writes are rejected and the source stays healthy')
        browser.execute('closeModal();openSettings();document.querySelector("[data-id=background_compaction]").click();')
        wait_for(lambda:browser.execute('return state.background_compaction===true;'))
        assert (source/'config.json').read_bytes()==source_config
        assert (home/'auth.json').read_bytes()==source_auth
        assert (home/'state_5.sqlite').read_bytes()==raw_metadata
        browser.execute('closeModal();document.getElementById("shadow_demo_button").click();document.querySelector("[data-shadow-demo=recovery]").click();')
        wait_for(lambda:browser.execute('return !!document.getElementById("reset");'),seconds=90)
        assert browser.execute('return location.origin;')!=shadow
        browser.navigate(browser.execute('return location.origin;'))
        assert browser.execute('return !window.EMP_REAL_SHADOW && !!window.EMP_REAL_SHADOW_URL && !!window.EMP_IMPORT_DEMO;')
        browser.execute('document.getElementById("shadow_demo_button").click();document.querySelector("[data-shadow-demo=real_shadow]").click();')
        wait_for(lambda:browser.execute('return location.origin===arguments[0] && window.EMP_REAL_SHADOW===true;',shadow))
        assert request(shadow+query,headers=own)[1]['totals']['requests']==5
        assert upstream.calls==0
        checks.append('Saves stay in the preview draft; demos require explicit selection, use a different disposable origin and return to real data without contamination')
    args.output.write_text(json.dumps({'passed':True,'checks':checks,'real_model_calls':0,'production_writes':0},indent=2)+'\n')
    print(json.dumps({'passed':True,'checks':checks}))


if __name__=='__main__':
    main()
