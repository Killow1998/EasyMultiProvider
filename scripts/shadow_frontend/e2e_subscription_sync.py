"""Subscription settings synchronization using isolated browser data and fake upstreams."""
import argparse
import base64
import contextlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from e2e_refinements import Browser, HERE
from e2e_environment import request, start_backend, stop, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-subscription-sync-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        base, _, state_dir, _, upstream = start_backend(stack, args.emp.resolve(), root)
        before = (state_dir.parent/'config.json').read_bytes()
        env = dict(os.environ, HOME=str(root/'preview-home'), CODEX_HOME=str(root/'codex_home'))
        Path(env['HOME']).mkdir()
        log_path = root/'preview.log'
        log = stack.enter_context(log_path.open('w'))
        process = subprocess.Popen([sys.executable, str(HERE/'shadow-web.py'), '--port', '0',
            '--backend-port', base.rsplit(':', 1)[1], '--state-dir', str(state_dir)],
            env=env, stdout=log, stderr=log)
        stack.callback(stop, process)
        url = wait_for(lambda: log_path.read_text().split('EMP shadow: ')[-1].strip() if 'EMP shadow:' in log_path.read_text() else None)
        browser = Browser(stack, root, system_access=True)
        browser.navigate(url)
        browser.execute('''
          managementEvents.stop();clearInterval(quotaStateTimer);quotaStateTimer=null;
          state.accounts=['one','two'].map(id=>({id,name:id,prefix:id,enabled:true,hidden_models:['other'],model_context_windows:{other:12345},credential_set:false}));
          state.native_hidden_models=[];state.native_model_context_windows={'gpt-6-luna':467368,other:34567};
          state.native_account.hidden_models=[];state.native_account.model_context_windows={...state.native_model_context_windows};
          const models=[{id:'gpt-6-luna',display_name:'GPT-6 Luna',default_context_window:272000,max_context_window:872000},{id:'other',display_name:'Other',default_context_window:100000,max_context_window:500000}];
          state.subscription_models=models;const saved=window.fetch;
          window.fetch=async function(path,options){if(String(path).includes('/api/accounts/')&&String(path).endsWith('/models'))return new Response(JSON.stringify({models}),{status:200,headers:{'Content-Type':'application/json'}});return saved(path,options);};
          shadowSave(state);window.syncBefore=localStorage.getItem(shadowStorageKey);window.providersBefore=JSON.stringify(shadowScrub(state.providers));
          window.syncRow=()=>document.querySelector('input[data-model="gpt-6-luna"]').closest('.shadow-context-row');
          editAccount(state.native_account.id);
        ''')
        wait_for(lambda: browser.execute('return document.querySelectorAll("[data-shadow-subscription-sync]").length===2;'))
        assert browser.execute('return syncRow().querySelector("input").value==="467368" && localStorage.getItem(shadowStorageKey)===syncBefore;')
        checks.append('Opening settings preserves the chosen context and writes nothing')
        for theme in ['light', 'dark']:
            browser.execute('setTheme(arguments[0]);', theme)
            for zoom in [1, 1.5, 1.7]:
                request(browser.base+'/window/rect', {'width':750, 'height':700})
                request(browser.base+'/moz/context', {'context':'chrome'})
                request(browser.base+'/execute/sync', {'script':'gBrowser.selectedBrowser.browsingContext.fullZoom=arguments[0];', 'args':[zoom]})
                request(browser.base+'/moz/context', {'context':'content'})
                assert browser.execute('''
                  return [...document.querySelectorAll('.shadow-context-row')].every(row=>{
                    const eye=row.querySelector('[data-shadow-subscription-visibility]').getBoundingClientRect(),sync=row.querySelector('[data-shadow-subscription-sync]').getBoundingClientRect(),input=row.querySelector('input').getBoundingClientRect();
                    return Math.abs(eye.top+eye.height/2-sync.top-sync.height/2)<1 && sync.left>input.right && sync.right<=row.getBoundingClientRect().right;
                  });
                '''), (theme, zoom)
            status, png = request(browser.base+'/screenshot')
            assert status == 200
            args.output.with_name('shadow-subscription-sync-'+theme+'.png').write_bytes(base64.b64decode(png['value']))
        checks.append('Far-right action aligns with the eye in both themes at 100/150/170 percent zoom')
        assert browser.execute('return getComputedStyle(syncRow().querySelector("[data-shadow-subscription-sync]")).getPropertyValue("--button-icon").includes("M3%207h18");')
        assert browser.execute('return uiIcon("reset").includes("<rect") && !uiIcon("reset").includes("a7");')
        checks.append('Straight opposing arrows for sync; physical key shape for Reset')
        browser.execute('syncRow().querySelector("[data-shadow-subscription-sync]").click();closeModal();')
        assert browser.execute('return localStorage.getItem(shadowStorageKey)===syncBefore && state.accounts.every(a=>a.model_context_windows["gpt-6-luna"]===undefined);')
        checks.append('Cancel after marking synchronization leaves every subscription unchanged')

        def open_editor(account):
            browser.execute('editAccount(arguments[0]==="native"?state.native_account.id:arguments[0]);', account)
            wait_for(lambda: browser.execute('return document.querySelectorAll("[data-shadow-subscription-sync]").length===2;'))

        def save():
            browser.execute('document.getElementById("modal_submit").click();')
            wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))

        open_editor('native')
        browser.execute('syncRow().querySelector("[data-shadow-subscription-sync]").click();syncRow().querySelector("[data-shadow-subscription-visibility]").click();syncRow().querySelector("input").value="400000";')
        save()
        assert browser.execute('return state.accounts.every(a=>a.hidden_models.includes("gpt-6-luna")&&a.hidden_models.includes("other")&&a.model_context_windows["gpt-6-luna"]===400000&&a.model_context_windows.other===12345) && JSON.stringify(shadowScrub(state.providers))===providersBefore;'), browser.execute('return {accounts:state.accounts.map(({id,hidden_models,model_context_windows})=>({id,hidden_models,model_context_windows})),providersUnchanged:JSON.stringify(shadowScrub(state.providers))===providersBefore,nativeWindows:state.native_model_context_windows};')
        checks.append('Save copies the latest Native context and visibility; unrelated settings and providers stay intact')
        open_editor('one')
        browser.execute('syncRow().querySelector("input").value="900000";syncRow().querySelector("[data-shadow-subscription-sync]").click();')
        assert browser.execute('return syncRow().querySelector("[data-shadow-subscription-sync]").dataset.icon==="sync";')
        browser.execute('syncRow().querySelector("input").value="467368";syncRow().querySelector("[data-shadow-subscription-sync]").click();syncRow().querySelector("[data-shadow-subscription-visibility]").click();')
        save()
        assert browser.execute('return state.native_model_context_windows["gpt-6-luna"]===467368 && state.native_model_context_windows.other===34567 && !state.native_hidden_models.includes("gpt-6-luna") && state.accounts.every(a=>a.model_context_windows["gpt-6-luna"]===467368 && !a.hidden_models.includes("gpt-6-luna"));')
        checks.append('Invalid context is rejected; an imported subscription can synchronize Native and its peers')
        open_editor('two')
        browser.execute('syncRow().querySelector("input").value="";syncRow().querySelector("[data-shadow-subscription-sync]").click();')
        save()
        assert browser.execute('return !("gpt-6-luna" in state.native_model_context_windows) && state.accounts.every(a=>!("gpt-6-luna" in a.model_context_windows));')
        checks.append('Blank context consistently restores the catalog default across subscriptions')
        assert (state_dir.parent/'config.json').read_bytes() == before and upstream.calls == 0
        args.output.write_text(json.dumps({'passed':True, 'checks':checks,
            'backend_configuration_unchanged':True, 'model_requests':0}, indent=2)+'\n')
        print(json.dumps({'passed':True, 'checks':checks}))


if __name__ == '__main__':
    main()
