"""Accept session bills through real history scanning, accounting and Firefox."""
import argparse
import base64
import contextlib
import json
import sqlite3
import tempfile
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlencode

from candidate_web import candidate_frontend
from e2e_environment import request, start_backend, wait_for
from e2e_refinements import Browser
from session_bill_lifecycle import check_bill_lifecycle
from session_bill_lineage import check_conversation_ownership

FIRST='11111111-1111-4111-8111-111111111111'
SECOND='22222222-2222-4222-8222-222222222222'
TITLE='修复账单 & <img src=x onerror="window.billXss=1"> / Billing'


class BillUpstream(BaseHTTPRequestHandler):
    calls=0

    def log_message(self,*args):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers['Content-Length']))
        type(self).calls+=1
        body=json.dumps({'id':f'bill-fixture-{type(self).calls}','model':'upstream-model',
            'choices':[{'index':0,'message':{'role':'assistant','content':'ok'},'finish_reason':'stop'}],
            'usage':{'prompt_tokens':10,'completion_tokens':2,
                'prompt_tokens_details':{'cached_tokens':0}}}).encode()
        self.send_response(200)
        self.send_header('Content-Type','application/json')
        self.send_header('Content-Length',str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def rollout(home, session, turns):
    path=home/'sessions'/f'bill-{session or "unassigned"}.jsonl'
    path.parent.mkdir(exist_ok=True)
    records=[{'type':'session_meta','payload':{'id':session,'model_provider':'emp'}}]
    cumulative={}
    for index,(turn,model,counts) in enumerate(turns):
        records.append({'type':'turn_context','payload':{'model':model,'turn_id':turn}})
        for key,value in counts.items():
            cumulative[key]=cumulative.get(key,0)+value
        records.append({'type':'event_msg','timestamp':datetime.fromtimestamp(time.time()-30-len(turns)+index,timezone.utc).isoformat(),
            'payload':{'type':'token_count','info':{'last_token_usage':counts,'total_token_usage':dict(cumulative)}}})
    path.write_text(''.join(json.dumps(record)+'\n' for record in records))
    return path


def scan(base,headers):
    status,result=request(base+'/api/usage/scan',{},headers)
    assert status==202,result
    command=result['command_id']
    return wait_for(lambda: request(base+'/api/usage?series=true',headers=headers)[1]['history']['completed']>=command)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    checks,viewports=[],[]
    with tempfile.TemporaryDirectory(prefix='emp-bills-e2e-') as temporary,contextlib.ExitStack() as stack:
        root=Path(temporary)
        (root/'state').mkdir()
        rates={'input_cost_per_token':'0.001','output_cost_per_token':'0.002',
            'cache_read_input_token_cost':'0.00025','cache_creation_input_token_cost':'0.002'}
        (root/'state/api_prices.json').write_text(json.dumps({'fetched_at':time.time(),
            'prices':{'upstream-model':rates,'second-model':rates,
                'zero-model':{key:'0' for key in rates}}}))
        fixture_upstream=ThreadingHTTPServer(('127.0.0.1',0),BillUpstream)
        stack.callback(fixture_upstream.server_close)
        threading.Thread(target=fixture_upstream.serve_forever,daemon=True).start()
        stack.callback(fixture_upstream.shutdown)
        base,_,state,headers,upstream=start_backend(stack,args.emp.resolve(),root,
            {'codex_base_url':'http://127.0.0.1:9/fixture-only','external_collaboration':'on','providers':[
                {'id':'demo','name':'Billing fixture','base_url':f'http://127.0.0.1:{fixture_upstream.server_port}/v1',
                 'protocol':'chat_completions','auth_mode':'api_key','api_key':'fixture-only'}]},
            {'PATH':'/usr/bin:/bin','HTTP_PROXY':'http://127.0.0.1:9',
             'HTTPS_PROXY':'http://127.0.0.1:9','ALL_PROXY':'http://127.0.0.1:9',
             'NO_PROXY':'127.0.0.1,localhost'})
        headers['Origin']=base
        home=root/'codex_home'
        config=request(base+'/api/config',headers=headers)[1]
        config['models'] += [{'id':'demo/'+name,'provider':'demo','upstream_id':name,'enabled':True}
            for name in ['second-model','unpriced-model','zero-model']]
        assert request(base+'/api/config',config,headers)[0]==200
        call_headers={**headers,'thread-id':FIRST,
            'x-codex-turn-metadata':json.dumps({'thread_id':FIRST,'turn_id':'live-turn'})}
        assert request(base+'/v1/responses',{'model':'demo/model','input':'fixture only','stream':False},call_headers)[0]==200
        live=request(base+'/api/calls',headers=headers)[1]['records'][0]
        first_file=rollout(home,FIRST,[('live-turn','demo/model',{
            'input_tokens':live['input_tokens'],'output_tokens':live['output_tokens'],
            'cached_input_tokens':live.get('cached_input_tokens') or 0}),
            ('second-turn','demo/second-model',{'input_tokens':20,'output_tokens':4,
                'cached_input_tokens':10,'cache_write_input_tokens':2})])
        rollout(home,SECOND,[('unpriced-turn','demo/unpriced-model',{
            'input_tokens':33,'output_tokens':3,'cached_input_tokens':0})])
        rollout(home,'missing-usage',[('missing-turn','demo/model',{'input_tokens':7})])
        rollout(home,'zero-price',[('zero-turn','demo/zero-model',{
            'input_tokens':5,'output_tokens':1,'cached_input_tokens':0})])
        rollout(home,'',[('unassigned-turn','demo/model',{
            'input_tokens':9,'output_tokens':1,'cached_input_tokens':0})])
        with sqlite3.connect(home/'state_5.sqlite') as database:
            database.execute('CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT)')
            database.executemany('INSERT INTO threads VALUES (?,?)',[
                (FIRST,TITLE),(SECOND,'Unknown price'),('missing-usage','Missing usage'),('zero-price','Zero price')])
            for index in range(125):
                identifier=f'page-{index:03}'
                rollout(home,identifier,[(f'page-turn-{index}','demo/model',{
                    'input_tokens':1,'output_tokens':1,'cached_input_tokens':0})])
                database.execute('INSERT INTO threads VALUES (?,?)',(identifier,f'Page session {index:03}'))
        scan(base,headers)
        query='start=0&end='+str(time.time()+300)
        status,bills=request(base+'/api/usage?sessions=true&'+query,headers=headers)
        assert status==200,bills
        assert bills['total']==129 and bills['totals']['requests']==131,bills
        assert bills['unassigned']['requests']==1
        all_usage=request(base+'/api/usage?'+query,headers=headers)[1]
        for key in ['input_tokens','output_tokens','cached_input_tokens','cost_nanos','requests']:
            assert bills['totals'][key]==all_usage['totals'][key],key
        detail=request(base+'/api/usage?series=true&session='+FIRST+'&'+query,headers=headers)[1]
        assert detail['totals']['requests']==2,detail
        assert {row['model'] for row in detail['groups']}=={'upstream-model','second-model'}
        assert detail['totals']['input_tokens']==30 and detail['totals']['output_tokens']==6
        assert detail['totals']['cached_input_tokens']==10
        assert detail['totals']['cache_write_tokens']==2
        assert detail['totals']['unpriced_requests']==0
        checks.append('Real live/history overlap is counted once; session models/cache/prices reconcile with the existing total, including unassigned usage')
        for term,expected in [('Page session 124','page-124'),('修复账单',FIRST),('zero-price','zero-price')]:
            result=request(base+'/api/usage?'+urlencode({'sessions':'true','q':term})+'&'+query,headers=headers)[1]
            assert result['total']==1 and result['records'][0]['session_id']==expected,result
        next_page=request(base+'/api/usage?sessions=true&offset=20&'+query,headers=headers)[1]
        assert len(next_page['records'])==20 and not {row['session_id'] for row in bills['records']} & {row['session_id'] for row in next_page['records']}
        assert request(base+'/api/usage?sessions=true')[0]==401
        for invalid in ['limit=0','limit=101','sort=invalid','offset=-1','start=nan']:
            assert request(base+'/api/usage?sessions=true&'+invalid,headers=headers)[0]==400
        checks.append('Title search covers older SQLite-only titles beyond 100 sessions; paging, filters and authenticated validation stay bounded')
        with sqlite3.connect(state/'usage.sqlite3') as database:
            saved=json.loads(database.execute('SELECT checkpoint FROM usage_files WHERE path=?',(str(first_file),)).fetchone()[0])
            saved.pop('session_usage_version')
            database.execute('UPDATE usage_files SET checkpoint=? WHERE path=?',(json.dumps(saved),str(first_file)))
            database.execute("DELETE FROM usage_sessions WHERE session_id=? AND event_id IN (SELECT id FROM usage_events WHERE origin='history')",(FIRST,))
        scan(base,headers)
        restored=request(base+'/api/usage?series=true&session='+FIRST+'&'+query,headers=headers)[1]
        assert restored['totals']==detail['totals'],restored
        assert request(base+'/api/usage?'+query,headers=headers)[1]['totals']==all_usage['totals']
        checks.append('An old history checkpoint rebuilds session links without duplicating or repricing previously recorded usage')
        check_conversation_ownership(base,state,home,headers,query,FIRST)
        checks.append('Real request headers and Sidechat metadata merge review, Sidechat and nested agents into the main bill; search follows child names and orphan usage remains unassigned')
        web=Path(__file__).resolve().parents[2]/'crates/emp-app/web'
        head='<script>localStorage.setItem("emp_management_session_v1",'+json.dumps(headers['X-EMP-Session'])+');</script>'
        frontend,_=candidate_frontend(stack,base,web,index_head=head)
        browser=Browser(stack,root)
        for language,width in [('zh-CN',500),('en',1280)]:
            browser.navigate(frontend)
            request(browser.base+'/window/rect',{'width':width,'height':1000})
            browser.execute('setLanguage(arguments[0]);statistics.open();document.querySelector("[data-stats-view=sessions]").click();',language)
            wait_for(lambda: browser.execute('return document.querySelectorAll("[data-session-bill]").length===20;'))
            browser.execute('const input=document.querySelector("[data-bill-search]");input.value=arguments[0];input.dispatchEvent(new Event("input",{bubbles:true}));','修复账单')
            wait_for(lambda: browser.execute('return document.querySelectorAll("[data-session-bill]").length===1;'))
            assert browser.execute('return document.querySelector("[data-session-bill]").textContent.includes(arguments[0]) && !document.querySelector("[data-bill-list] img") && !window.billXss;',TITLE)
            browser.execute('document.querySelector("[data-session-bill]").open=true;')
            wait_for(lambda: browser.execute('return document.querySelector("[data-bill-detail]").textContent.includes("second-model");'))
            assert browser.execute('return document.querySelectorAll("[data-bill-models] tbody tr").length===2 && document.querySelectorAll("[data-bill-components] tbody tr").length===4;')
            assert browser.execute('return document.querySelector("[data-bill-components]").textContent.includes("Sidechat") && document.querySelector("[data-bill-components]").textContent.includes("Guardian review");')
            assert browser.execute('const box=document.querySelector("[data-bill-components]").closest(".usage-table");return box.scrollWidth<=box.clientWidth+1;')
            assert browser.execute('return document.documentElement.scrollWidth<=innerWidth;')
            _,png=request(browser.base+'/screenshot')
            args.output.with_name(args.output.stem+'-'+language+'.png').write_bytes(base64.b64decode(png['value']))
            browser.execute('document.querySelector("[data-bill-components]").scrollIntoView({block:"center"});')
            _,png=request(browser.base+'/screenshot')
            args.output.with_name(args.output.stem+'-components-'+language+'.png').write_bytes(base64.b64decode(png['value']))
            viewports.append(browser.execute('return {language:arguments[0],width:innerWidth,height:innerHeight};',language))
            for term,expected in [('Unknown price','—'),('Zero price','US$0')]:
                browser.execute('const input=document.querySelector("[data-bill-search]");input.value=arguments[0];input.dispatchEvent(new Event("input",{bubbles:true}));',term)
                wait_for(lambda: browser.execute('return document.querySelectorAll("[data-session-bill]").length===1 && document.querySelector("[data-session-bill]").textContent.includes(arguments[0]);',term))
                assert browser.execute('return document.querySelector("[data-session-bill] summary").textContent.includes(arguments[0]);',expected)
            browser.execute('const input=document.querySelector("[data-bill-search]");input.value="does-not-exist";input.dispatchEvent(new Event("input",{bubbles:true}));')
            wait_for(lambda: browser.execute('return document.querySelectorAll("[data-session-bill]").length===0 && document.querySelector("[data-bill-list]").textContent.includes(arguments[0]);','没有符合' if language=='zh-CN' else 'No session bills'))
            browser.execute('document.querySelector("[data-stats-view=overview]").click();document.querySelector("[data-stats-view=sessions]").click();')
            wait_for(lambda: browser.execute('return document.querySelectorAll("[data-session-bill]").length===20;'))
            browser.execute('document.querySelector("[data-bill-page=next]").click();')
            wait_for(lambda: browser.execute('return document.querySelector("[data-bill-pages]").textContent.includes("21");'))
            browser.execute('const select=document.querySelector("[data-bill-sort]");select.value="cost";select.dispatchEvent(new Event("change",{bubbles:true}));')
            wait_for(lambda: browser.execute('return document.querySelector("[data-session-bill]").dataset.sessionBill===arguments[0];',FIRST))
            browser.execute('const select=document.querySelector("[data-stats-filter=service]");select.value="provider:demo";select.dispatchEvent(new Event("change",{bubbles:true}));')
            wait_for(lambda: browser.execute('return document.querySelectorAll("[data-session-bill]").length===1 && document.querySelector("[data-session-bill] summary").textContent.includes("48");'))
            browser.execute('document.querySelector("[data-stats-view=overview]").click();')
            wait_for(lambda: browser.execute('return [...document.querySelectorAll(".presentation-stat")].some(node=>node.querySelector(".presentation-stat-label")?.textContent==="Token" && node.querySelector("strong").textContent==="60");'))
            checks.append(language+': search, escaped titles, model expansion, missing versus zero prices, empty results, sorting, paging, service-filter navigation and layout pass')
            browser.execute('closeModal();')
        browser.execute('statistics.open();document.querySelector("[data-stats-view=sessions]").click();')
        wait_for(lambda: browser.execute('return document.querySelectorAll("[data-session-bill]").length===20;'))
        with sqlite3.connect(state/'usage.sqlite3',timeout=5) as database:
            database.execute('BEGIN EXCLUSIVE')
            browser.execute('document.querySelector("[data-session-bill]").open=true;')
            wait_for(lambda: browser.execute('return document.querySelector("[data-session-bill]").getAttribute("aria-busy")==="true";'))
            browser.execute('closeModal();')
            database.rollback()
        assert browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");')
        checks.append('Closing statistics cancels an in-flight model breakdown; a late result cannot reopen or replace another view')
        check_bill_lifecycle(browser)
        checks.append('Queued native toggle events from replaced rows cannot claim or cancel the current bill details during refresh or remount')
        large_session='c9356918-3354-42b1-8c7c-88409572fdb1'
        with sqlite3.connect(home/'state_5.sqlite') as database:
            database.execute('INSERT INTO threads(id,title) VALUES (?,?)',(large_session,'Compact usage'))
        large_turns=[(f'compact-{index}','demo/model',{'input_tokens':10000000 if index<123 else 4567890,
            'output_tokens':2345678 if index==123 else 0,'cached_input_tokens':0}) for index in range(124)]
        rollout(home,large_session,large_turns+[('compact-small','demo/zero-model',{'input_tokens':12345,'output_tokens':6789,'cached_input_tokens':0})])
        scan(base,headers)
        end=time.time()+300
        for span,bucket,count in [(3600,600,6),(86400,3600,24),(604800,86400,7),(2592000,86400,30)]:
            period=urlencode({'start':end-span,'end':end})
            usage=request(base+'/api/usage?series=true&'+period,headers=headers)[1]
            calls=request(base+'/api/calls?'+period,headers=headers)[1]
            assert usage['totals']['input_tokens']>1e9,usage['totals']
            assert usage['bucket']==calls['bucket']==bucket,(usage,calls)
            assert (usage['end']-usage['start'])/bucket==count
            for field in ['requests','input_tokens','output_tokens','cost_nanos']:
                assert sum(row[field] for row in usage['periods'])==usage['totals'][field]
            assert sum(row['calls'] for row in calls['periods'])==calls['summary']['calls']
            assert all(0<=round((row['start']-usage['start'])/bucket)<count for row in usage['periods']+calls['periods'])
        checks.append('Usage and call charts share six ten-minute, 24 hourly, seven daily and 30 daily buckets; regrouping preserves recorded totals')
        browser.navigate(frontend)
        for language in ['zh-CN','en']:
            browser.execute('setLanguage(arguments[0]);statistics.open();',language)
            wait_for(lambda:browser.execute('return document.querySelectorAll(".presentation-stat strong").length>0;'))
            wait_for(lambda:browser.execute('return [...document.querySelectorAll(".presentation-stat")].some(node=>node.querySelector(".presentation-stat-label")?.textContent==="Token" && node.querySelector("strong").textContent.split(" ")[0].endsWith("b"));'))
            assert browser.execute('const node=[...document.querySelectorAll(".presentation-stat")].find(node=>node.querySelector(".presentation-stat-label")?.textContent==="Token").querySelector("strong");return Number(node.title.replaceAll(",",""))>1e9 && !/[kmb]/.test(node.title);')
            assert browser.execute('const node=[...document.querySelectorAll(".presentation-stat")].find(node=>node.querySelector(".presentation-stat-label")?.textContent===tr("reports.api_estimate_usd")).querySelector("strong");return /^US\\$[0-9.]+m$/.test(node.textContent.split(" ")[0]) && node.title.startsWith("US$1,");')
            if language=='zh-CN':
                _,png=request(browser.base+'/screenshot')
                args.output.with_name(args.output.stem+'-compact.png').write_bytes(base64.b64decode(png['value']))
            for preset,interval in [('1h','10 '+browser.execute('return tr("reports.min");')),('1d','1 '+browser.execute('return tr("reports.h");')),('7d','1 '+browser.execute('return tr("reports.day");')),('30d','1 '+browser.execute('return tr("reports.day");'))]:
                browser.execute('selectPeriodPreset("presentation_stats",arguments[0]);',preset)
                wait_for(lambda:browser.execute('return !document.querySelector("#presentation_stats [data-report-action=refresh]").disabled && document.querySelector(".presentation-chart-heading p")?.textContent.startsWith(tr("reports.per")+" "+arguments[0]+" ·");',interval))
            browser.execute('document.querySelector("[data-stats-view=sessions]").click();')
            wait_for(lambda:browser.execute('return document.querySelector("[data-bill-search]");'))
            browser.execute('const input=document.querySelector("[data-bill-search]");input.value=arguments[0];input.dispatchEvent(new Event("input",{bubbles:true}));',large_session)
            wait_for(lambda:browser.execute('return document.querySelectorAll("[data-session-bill]").length===1;'))
            browser.execute('document.querySelector("[data-session-bill]").open=true;')
            wait_for(lambda:browser.execute('return document.querySelector("[data-bill-models]")?.textContent.includes("1.23b");'))
            assert browser.execute('const spans=[...document.querySelectorAll("[data-bill-models] span[title]")];return [["1.23b","1,234,567,890"],["2.35m","2,345,678"],["12.35k","12,345"]].every(([text,title])=>spans.some(node=>node.textContent===text && node.title===title));')
            browser.execute('closeModal();')
        checks.append('Chinese and English statistics and session bills display lowercase k/m/b with full exact token and USD hover values')
        browser.execute('''
          window.statsFetch=window.fetch;window.statsHeld={};
          window.fetch=function(path,options){
            const kind=String(path).match(/^\\/api\\/(calls|usage)\\?/);
            if(!kind)return statsFetch(path,options);
            return statsFetch(path,options).then(async response=>{
              const text=await response.text();
              if(kind[1]==='usage')return new Promise(resolve=>{statsHeld.usage=()=>resolve(new Response(text,{status:response.status,headers:response.headers}));});
              const separator=String.fromCharCode(10,10),frames=text.trimEnd().split(separator).map(frame=>frame+separator),encoder=new TextEncoder();
              return new Response(new ReadableStream({start(controller){
                let next=0;
                statsHeld.calls=stage=>{
                  if(stage==='fail'){controller.error(new Error('fixture read failed'));return;}
                  const end=stage==='first'?1:stage==='half'?Math.floor(frames.length/2):frames.length;
                  while(next<end){
                    const bytes=encoder.encode(frames[next++]);
                    for(let offset=0;offset<bytes.length;offset+=11)controller.enqueue(bytes.slice(offset,offset+11));
                  }
                  if(stage==='all')controller.close();
                };
                statsHeld.calls('first');
              }}),{status:response.status,headers:response.headers});
            });
          };
        ''')
        for language,usage_first in [('zh-CN',True),('en',False)]:
            browser.execute('setLanguage(arguments[0]);statsHeld={};statistics.open();',language)
            wait_for(lambda:browser.execute('return !!(statsHeld.calls && statsHeld.usage);'))
            wait_for(lambda:browser.execute('return document.querySelector("#presentation_stats progress")?.value===0;'))
            if usage_first:
                browser.execute('statsHeld.usage();')
                time.sleep(.1)
                assert browser.execute('return document.querySelector("#presentation_stats progress").value===0;')
            browser.execute('statsHeld.calls("half");')
            wait_for(lambda:browser.execute('const bar=document.querySelector("#presentation_stats progress");return bar?.value>0 && bar.value<bar.max;'))
            assert browser.execute('const bar=document.querySelector("#presentation_stats progress");return document.querySelector("#presentation_stats [data-report-status]").textContent===Math.floor(bar.value/bar.max*100)+"%";')
            _,png=request(browser.base+'/screenshot')
            args.output.with_name(args.output.stem+'-progress-'+language+'.png').write_bytes(base64.b64decode(png['value']))
            browser.execute('statsHeld.calls("all");')
            if not usage_first:
                wait_for(lambda:browser.execute('const bar=document.querySelector("#presentation_stats progress");return bar?.value===bar?.max;'))
                assert browser.execute('return document.querySelector("#presentation_stats").getAttribute("aria-busy")==="true";')
                browser.execute('statsHeld.usage();')
            wait_for(lambda:browser.execute('return document.querySelector("#presentation_stats").getAttribute("aria-busy")==="false";'))
            assert browser.execute('return !document.querySelector("#presentation_stats progress");')
            browser.execute('closeModal();')
        browser.execute('statsHeld={};statistics.open();')
        wait_for(lambda:browser.execute('return !!(statsHeld.calls && statsHeld.usage);'))
        browser.execute('statsHeld.calls("fail");')
        wait_for(lambda:browser.execute('return document.querySelector("#presentation_stats").getAttribute("aria-busy")==="false";'))
        error_status=browser.execute('return document.querySelector("#presentation_stats [data-report-status]").textContent;')
        assert error_status
        browser.execute('statsHeld.usage();')
        time.sleep(.2)
        assert browser.execute('return !document.querySelector("#presentation_stats progress") && document.querySelector("#presentation_stats [data-report-status]").textContent===arguments[0];',error_status)
        browser.execute('window.fetch=statsFetch;closeModal();')
        checks.append('Statistics show only real row-processing percentages, decode fragmented streams, wait for both reports, and preserve errors against late responses')
        assert BillUpstream.calls==5 and upstream.calls==0
    receipt={'passed':True,'checks':checks,'viewports':viewports,'real_model_calls':0,
        'fixture_model_calls':5,'production_state_used':False}
    args.output.write_text(json.dumps(receipt,indent=2,ensure_ascii=False)+'\n')
    print(json.dumps(receipt,ensure_ascii=False))


if __name__=='__main__':
    main()
