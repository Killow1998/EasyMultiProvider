"""Separate production UI instances with synthetic auth and loopback-only migration."""
import json
import shutil
from pathlib import Path
from candidate_web import candidate_frontend
from local_backend import request, start_backend

HERE=Path(__file__).resolve().parent

def start(stack,binary,root,return_url):
    bin_dir=root/'bin';bin_dir.mkdir()
    shutil.copy(HERE/'fixtures/device-codex.py',bin_dir/'codex');(bin_dir/'codex').chmod(0o700)
    (bin_dir/'login-control').write_text('auto')
    endpoints=[]
    for name in ['sender','receiver']:
        directory=root/name;directory.mkdir()
        config={'auto_enable_on_start':False,'providers':[{'id':'example','name':'Example API',
            'base_url':'http://127.0.0.1:9/v1','protocol':'responses','auth_mode':'api_key','api_key':'example-only'}],
            'models':[{'id':'example/model','provider':'example','upstream_id':'example-model','context_window':100000}]}
        if name=='receiver':config={'auto_enable_on_start':False}
        base,_,_,headers=start_backend(stack,binary,directory,config,{
            'PATH':str(bin_dir)+':/usr/bin:/bin','HTTP_PROXY':'http://127.0.0.1:9',
            'HTTPS_PROXY':'http://127.0.0.1:9','ALL_PROXY':'http://127.0.0.1:9','NO_PROXY':'localhost,127.0.0.1'})
        if name=='sender':
            status,_=request(base+'/api/integration/enable',{'confirm_reload':True},headers)
            if status!=200:raise ValueError('Isolated onboarding could not start')
        endpoints.append((base,headers['X-EMP-Session']))
    urls=[]
    def script(index):
        settings=json.dumps({'peer':urls[1-index],'back':return_url})
        return '<script>window.EMP_ONBOARDING_FIXTURE='+settings+';</script><script>'+ (HERE/'onboarding-preview.js').read_text()+'</script>'
    for index,(base,token) in enumerate(endpoints):
        head='<script>localStorage.setItem("emp_management_session_v1",'+json.dumps(token)+');</script>'
        url,_=candidate_frontend(stack,base,HERE.parents[1]/'crates/emp-app/web',lambda i=index:script(i),index_head=head)
        urls.append(url)
    return urls[0]
