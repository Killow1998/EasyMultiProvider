"""Statistics period switching reuses timestamped facts without changing report results."""
import argparse
import contextlib
import json
import sqlite3
import tempfile
import time
from pathlib import Path

from candidate_web import candidate_frontend
from e2e_environment import start_backend, request, wait_for
from e2e_refinements import Browser


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    end=int(time.time()//60+1)*60
    checks=[]
    with tempfile.TemporaryDirectory(prefix='emp-range-e2e-') as temporary,contextlib.ExitStack() as stack:
        root=Path(temporary)
        base,url,state,headers,upstream=start_backend(stack,args.emp.resolve(),root,{'auto_enable_on_start':False})
        for endpoint in ['calls', 'usage?series=true', 'usage?lifetime=true&owner=demo']:
            assert request(base+'/api/'+endpoint)[0]==401
            assert request(base+'/api/'+endpoint,headers={**headers,'Origin':'https://untrusted.invalid'})[0]==403
        assert request(base+'/api/calls',headers=headers)[0]==200
        checks.append('Report cancellation routing retains management authentication and same-origin checks')
        with sqlite3.connect(state/'usage.sqlite3') as db:
            for index,age in enumerate([29*86400,6*86400,23*3600,3600,100,1]):
                at=end-age
                facts={'request_id':f'cache-{index}','route':'responses','provider_id':'demo','model_id':'demo/model','success':True,'input_tokens':100,'output_tokens':20,'first_content_ms':100,'generation_ms':1000,'generation_tokens_per_second':20,'usage_category':'external'}
                db.execute('INSERT INTO call_records VALUES (?,?,?,?,?,?,?,?,?,?,?)',(facts['request_id'],'responses',at,at+.5,'demo','','demo/model','','completed',json.dumps(facts),None))
                db.execute("INSERT INTO usage_events (id,observed_at,category,owner,model,service_tier,operation,input_tokens,output_tokens,cost_nanos,rates) VALUES (?,?,'external','demo','demo/model','default','responses',100,20,0,'{}')",(facts['request_id'],at))
        frontend,_=candidate_frontend(stack,base,Path(__file__).resolve().parents[2]/'crates/emp-app/web')
        browser=Browser(stack,root);browser.navigate(url.replace(base,frontend))
        browser.execute('''
          managementEvents.stop();clearInterval(quotaStateTimer);dismissNotice();Date.now=()=>arguments[0]*1000-1;
          window.reads=[];window.originalFetch=window.fetch;
          window.fetch=async(path,options)=>{
            const response=await originalFetch(path,options);
            if(String(path).startsWith('/api/calls?')||String(path).startsWith('/api/usage?series=true'))response.clone().text().then(text=>{
              const frames=text.split('\\n\\n'),result=frames.find(frame=>frame.startsWith('event: result'));
              reads.push({path:String(path),body:result?JSON.parse(result.split('\\ndata: ')[1]):JSON.parse(text)});
            });
            return response;
          };
          statistics.open();
        ''',end)
        wait_for(lambda:browser.execute('return reads.length>=2;'))
        for period,duration,count in [('30d',30*86400,6),('7d',7*86400,5),('1d',86400,4),('1h',3600,3)]:
            browser.execute('reads=[];document.querySelector(`[data-id="presentation_stats"][data-preset="${arguments[0]}"]`).click();',period)
            wait_for(lambda:browser.execute('return reads.length>=2 && document.getElementById("presentation_stats").getAttribute("aria-busy")==="false";'))
            results=browser.execute('return reads;')
            calls=next(row['body'] for row in results if row['path'].startswith('/api/calls'))
            usage=next(row['body'] for row in results if row['path'].startswith('/api/usage'))
            assert calls['total']==count and usage['totals']['input_tokens']==count*100
            assert calls['summary']['tokens_per_second']==20 and calls['summary']['ttft_ms']==100
            assert calls['start']==end-duration and calls['end']==end
            checks.append(period+': real statistics controls retain exact boundaries, sample averages and usage totals')
        # Simulate a late receipt inside the cached period and use the normal Refresh button.
        with sqlite3.connect(state/'usage.sqlite3') as db:
            db.execute("UPDATE call_records SET facts=json_set(facts,'$.first_content_ms',400) WHERE request_id='cache-5'")
            db.execute("UPDATE usage_events SET input_tokens=400 WHERE id='cache-5'")
        browser.execute('reads=[];document.querySelector("[data-ui-action=period-refresh][data-id=presentation_stats]").click();')
        wait_for(lambda:browser.execute('return reads.length>=2;'))
        results=browser.execute('return reads;')
        assert next(r['body'] for r in results if r['path'].startswith('/api/calls'))['summary']['ttft_ms']==200
        assert next(r['body'] for r in results if r['path'].startswith('/api/usage'))['totals']['input_tokens']==600
        checks.append('Refresh invalidates both projections after retroactive database updates')
        assert upstream.calls==0
    result={'passed':True,'checks':checks,'real_model_requests':0}
    args.output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result))


if __name__=='__main__':main()
