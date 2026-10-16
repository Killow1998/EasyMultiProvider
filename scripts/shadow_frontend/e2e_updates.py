"""Update page reuse and native launch behavior with disposable local fixtures."""
import argparse
import contextlib
import json
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import tempfile

from candidate_web import candidate_frontend
from e2e_environment import request, start_backend, stop, wait_for
from e2e_refinements import Browser


def launch_checks(binary, root):
    executable = root / 'EMP'
    shutil.copy2(binary, executable)
    version = subprocess.check_output([str(executable), '--version'], text=True).strip().split()[-1]
    opened = root / 'browser-opened'
    browser = root / 'browser-fixture'
    browser.write_text('#!/bin/sh\nprintf "opened\\n" >> ' + shlex.quote(str(opened)) + '\n')
    browser.chmod(0o700)
    job = root / '.emp-update-fixture'
    job.mkdir()
    (job / 'plan.json').write_text(json.dumps({'target': str(executable), 'candidate': str(job / 'candidate'),
        'relative_binary': '', 'parents': [], 'args': [], 'version': version, 'nonce': 'fixture-nonce'}))
    for mode in ['normal', 'update', 'rollback']:
        with contextlib.ExitStack() as stack:
            home = root / mode
            home.mkdir()
            config = home / 'config.json'
            config.write_text('{"auto_enable_on_start":false}')
            env = {'PATH': '/usr/bin:/bin', 'HOME': str(home), 'CODEX_HOME': str(home / 'codex'),
                'XDG_CONFIG_HOME': str(home), 'BROWSER': str(browser),
                'HTTP_PROXY': 'http://127.0.0.1:9', 'HTTPS_PROXY': 'http://127.0.0.1:9', 'NO_PROXY': 'localhost,127.0.0.1'}
            if mode == 'update':
                env['EMP_UPDATE_READY'] = str(job / 'ready.json')
            if mode == 'rollback':
                env['EMP_UPDATE_RESULT'] = 'rolled_back'
            log_path = home / 'log'
            log = stack.enter_context(log_path.open('w'))
            process = subprocess.Popen([str(executable), 'serve', '--config', str(config), '--port', '0', '--open-browser'],
                env=env, stdout=log, stderr=log)
            stack.callback(stop, process)
            match = wait_for(lambda: re.search(r'EMP listening on (http://127.0.0.1:\d+)', log_path.read_text()))
            if mode == 'normal':
                wait_for(opened.exists)
            if mode == 'update':
                wait_for(lambda: (job / 'ready.json').exists())
                assert json.loads((job / 'ready.json').read_text()) == {'version': version, 'nonce': 'fixture-nonce'}
            assert request(match[1] + '/healthz')[0] == 200
            assert process.poll() is None
        assert opened.read_text().splitlines() == ['opened'], mode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-update-ui-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        launches = root / 'launches'
        launches.mkdir()
        launch_checks(args.emp.resolve(), launches)
        checks.append('Native normal launch opens once; acknowledged update and rollback launch zero additional browser windows')
        backend_root = root / 'backend'
        backend_root.mkdir()
        base, url, _, _, upstream = start_backend(stack, args.emp.resolve(), backend_root)
        shim = r'''<script>
          sessionStorage.fixtureLoads=String(Number(sessionStorage.fixtureLoads||0)+1);
          const updateFixtureFetch=window.fetch.bind(window);let failures=0;
          window.fetch=async(path,options)=>{
            if(!String(path).startsWith('/api/updates'))return updateFixtureFetch(path,options);
            if(String(path).endsWith('/install'))sessionStorage.fixtureInstall='pending';
            const pending=sessionStorage.fixtureInstall==='pending';
            if(pending&&String(path)==='/api/updates'){
              if(failures++<2)throw new TypeError('Synthetic restart disconnect');
              sessionStorage.fixtureInstall='done';
            }
            const done=sessionStorage.fixtureInstall==='done';
            return new Response(JSON.stringify({state:done?'current':pending?'installing':'available',
              current_version:done?'0.13.5':'0.13.4',latest_version:'0.13.5',supported:true,release_notes:'Fixture notes'}),
              {headers:{'Content-Type':'application/json'}});
          };
        </script>'''
        frontend, _ = candidate_frontend(stack, base, Path(__file__).resolve().parents[2] / 'crates/emp-app/web', index_head=shim)
        browser = Browser(stack, root)
        browser.navigate(url.replace(base, frontend))
        original = request(browser.base + '/window/handles')[1]['value']
        for language in ['zh-CN', 'en']:
            browser.execute('setLanguage(arguments[0]);openUpdate();', language)
            wait_for(lambda: browser.execute('return document.getElementById("update_version")?.textContent==="v0.13.4 → v0.13.5";'))
            browser.execute('closeModal();')
        browser.execute('openUpdate();')
        wait_for(lambda: browser.execute('return !document.getElementById("update_install").hidden;'))
        browser.execute('document.getElementById("update_install").click();')
        wait_for(lambda: browser.execute('return document.getElementById("update_status")?.textContent.includes("Waiting for EMP");'))
        # During reload, the document can exist before its application state is initialized.
        wait_for(lambda: browser.execute('try{return sessionStorage.fixtureLoads==="2" && !!state?.providers?.length;}catch(_){return false;}'))
        assert request(browser.base + '/window/handles')[1]['value'] == original
        browser.execute('openUpdate();')
        wait_for(lambda: browser.execute('return document.getElementById("update_version")?.textContent==="Current version: v0.13.5";'))
        assert upstream.calls == 0
        checks.append('ZH/EN source-to-target versions; two simulated disconnects; one reload in the same tab with management session retained')
    args.output.write_text(json.dumps({'passed': True, 'checks': checks, 'real_downloads': 0, 'real_model_requests': 0}, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': checks}))


if __name__ == '__main__':
    main()
