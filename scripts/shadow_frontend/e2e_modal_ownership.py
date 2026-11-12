"""Delayed reads and completed writes must preserve the user's current dialog."""
import argparse
import contextlib
import json
from pathlib import Path
import tempfile

from candidate_web import candidate_frontend
from e2e_refinements import Browser
from local_backend import start_backend, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--web', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with contextlib.ExitStack() as stack:
        root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix='emp-modal-owner-')))
        config = {'auto_enable_on_start': False,
                  'providers': [{'id': name, 'name': name, 'protocol': 'chat_completions',
                                 'base_url': 'http://127.0.0.1:9/v1', 'auth_mode': 'api_key',
                                 'api_key': 'fixture-only'} for name in ['first', 'second']],
                  'models': [{'id': name + '/model', 'provider': name, 'upstream_id': 'shared-model',
                              'context_window': 100000, 'enabled': True} for name in ['first', 'second']]}
        base, url, _, _ = start_backend(stack, args.emp.resolve(), root, config)
        if args.web:
            frontend, _ = candidate_frontend(stack, base, args.web)
            url = url.replace(base, frontend)
        browser = Browser(stack, root)
        browser.navigate(url)
        browser.execute('managementEvents.stop();clearInterval(quotaStateTimer);')

        def click(selector):
            wait_for(lambda: browser.execute('return !!document.querySelector(arguments[0]);', selector))
            browser.execute('document.querySelector(arguments[0]).click();', selector)

        def settings(category='general'):
            browser.execute('closeModal();openSettings();document.querySelector(`[data-settings-category="${arguments[0]}"]`).click();', category)

        def defer(path):
            browser.execute('''
              window.ownerFetch=window.fetch;window.ownerQueue=[];window.ownerDone=0;
              window.fetch=(path,options)=>String(path).startsWith(arguments[0])?
                new Promise(resolve=>window.ownerQueue.push(()=>window.ownerFetch(path,options).then(result=>{window.ownerDone++;resolve(result)}))):
                window.ownerFetch(path,options);
            ''', path)

        def release(index=0, completed=1):
            browser.execute('window.ownerQueue[arguments[0]]();', index)
            wait_for(lambda: browser.execute('return window.ownerDone===arguments[0];', completed))

        def restore():
            browser.execute('window.fetch=window.ownerFetch;delete window.ownerQueue;')

        def replacement():
            browser.execute('openModal("Replacement","<div id=owner_replacement></div>","",null);')

        def retained():
            assert browser.execute('return !!document.getElementById("owner_replacement") && !document.getElementById("modal_backdrop").classList.contains("hidden");')

        settings(); defer('/api/gateway')
        click('[data-gateway-open]')
        wait_for(lambda: browser.execute('return window.ownerQueue.length===1;'))
        replacement(); release(); retained(); restore()
        checks.append('A delayed gateway read cannot replace a newer dialog')

        settings(); click('[data-gateway-open]'); click('[data-gateway-usage]')
        wait_for(lambda: browser.execute('return !!document.getElementById("gateway_usage_days");'))
        defer('/api/gateway/usage')
        click('[data-gateway-days="7"]'); click('[data-gateway-days="30"]')
        wait_for(lambda: browser.execute('return window.ownerQueue.length===2;'))
        release(1)
        wait_for(lambda: browser.execute('return document.querySelector(`[data-gateway-days="30"]`)?.getAttribute("aria-pressed")==="true";'))
        release(0, 2)
        assert browser.execute('return document.querySelector(`[data-gateway-days="30"]`).getAttribute("aria-pressed")==="true";')
        restore()
        checks.append('The latest gateway period wins when responses arrive out of order')

        settings(); click('[data-gateway-open]'); click('[data-gateway-devices]'); click('[data-gateway-add]')
        browser.execute('document.getElementById("gateway_client_name").value="Delayed PC";')
        defer('/api/gateway/clients'); click('#modal_submit')
        wait_for(lambda: browser.execute('return window.ownerQueue.length===1;'))
        replacement(); release(); retained(); restore()
        settings(); click('[data-gateway-open]'); click('[data-gateway-devices]'); click('[data-gateway-view]')
        assert browser.execute('return document.getElementById("gateway_created_key").value.startsWith("emp-client-");')
        checks.append('A device key created after departure remains available from device management')

        browser.execute('closeModal();'); click('#catalog_aggregate_open'); click('[data-aggregate-add]')
        browser.execute('document.getElementById("aggregate_prefix").value="ownership-test";')
        defer('/api/config'); click('#modal_submit')
        wait_for(lambda: browser.execute('return window.ownerQueue.length===1;'))
        replacement(); release()
        wait_for(lambda: browser.execute('return state.model_groups.some(group=>group.name_prefix==="ownership-test");'))
        retained(); restore()
        checks.append('A completed aggregate save persists without reopening its departed dialog')

        browser.execute('closeModal();'); click('#catalog_aggregate_open')
        browser.execute('''
          window.ownerFetch=window.fetch;
          window.fetch=(path,options)=>String(path)==='/api/config' && options?.method==='POST'?
            Promise.resolve(new Response(JSON.stringify({error:{message:'Fixture rejected deletion'}}),{status:503,headers:{'Content-Type':'application/json'}})):
            window.ownerFetch(path,options);
        ''')
        click('[data-aggregate-remove]')
        wait_for(lambda: browser.execute('return document.getElementById("modal_status").textContent.includes("Fixture rejected deletion");'))
        assert browser.execute('return state.model_groups.length===1 && !document.querySelector("[data-aggregate-remove]").disabled;')
        restore()
        checks.append('Rejected deletion preserves the aggregate, reports the error and restores controls')

        defer('/api/config'); click('[data-aggregate-remove]')
        wait_for(lambda: browser.execute('return window.ownerQueue.length===1;'))
        assert browser.execute('return [...document.querySelectorAll("#modal_body button")].every(button=>button.disabled);')
        replacement(); release()
        wait_for(lambda: browser.execute('return state.model_groups.length===0;'))
        retained(); restore()
        checks.append('Completed deletion persists without reopening its departed dialog')
        assert not browser.diagnostics(), browser.diagnostics()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps({'passed': True, 'real_provider_calls': 0, 'checks': checks}, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': checks}))


if __name__ == '__main__':
    main()
