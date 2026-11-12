"""Caller-scoped CPA quota reads, caching and the production service UI."""
import argparse
import base64
import contextlib
import json
import re
import sqlite3
import shutil
import time
from pathlib import Path
import tempfile

from e2e_environment import request, wait_for
from e2e_refinements import Browser
from local_backend import start_backend
from cpa_quota_fixture import start_pool, seed_pool_history


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-cpa-quota-') as directory, contextlib.ExitStack() as stack:
        root = Path(directory)
        upstream = start_pool(stack)
        (root/'backend').mkdir()
        configuration = {'providers': [{'id': 'pool', 'name': 'CPA', 'protocol': 'anthropic_messages', 'enabled': False,
            'execution_backend': 'claude_cli', 'auth_mode': 'api_key', 'api_key': 'fixture-pool',
            'base_url': f'http://127.0.0.1:{upstream.server_port}/cpa/v1/'}], 'models': []}
        base, url, state_root, headers = start_backend(stack, args.emp.resolve(), root/'backend', configuration)
        endpoint = base+'/api/providers/pool/quota'
        assert request(endpoint, {})[0] == 401
        assert request(endpoint, {'force': 'yes'}, headers)[0] == 400
        assert upstream.calls == []
        current = request(base+'/api/config', headers=headers)[1]
        current['providers'][0]['enabled'] = True
        assert request(base+'/api/config', current, headers)[0] == 200
        status, result = request(endpoint, {}, headers)
        assert status == 200 and result['observation']['error_code'] is None, result
        quota = result['observation']['quota']
        limits = quota['rate_limits_by_limit_id']['claude']
        assert limits['primary']['used_percent'] == 60
        assert limits['secondary']['used_percent'] == 80
        assert quota['accounts'][1]['windows']['five_hour']['remaining_percent'] == 40
        assert limits['primary']['resets_at'] == upstream.started_at+3600
        assert limits['secondary']['resets_at'] == upstream.started_at+172800
        assert len(quota['accounts']) == 5 and 'upstream-only-private' not in json.dumps(result)
        assert upstream.calls == [('/cpa/v1/quota', 'Bearer fixture-pool')]
        cached = request(endpoint, {}, headers)[1]
        assert cached == result and len(upstream.calls) == 1
        for route in ['/api/config', '/api/accounts']:
            metadata = request(base+route, headers=headers)[1]['provider_quotas']['pool']
            assert metadata['identity'] == result['identity'] and metadata['observation'] == result['observation']
        checks.append('Real management API reuses the CPA Key, retains the URL prefix, counts weekly-exhausted accounts as zero in 5h availability while preserving individual values; averages only enabled fresh known values, selects the nearest reset, omits extra upstream fields and caches reads')

        history_url = base+'/api/providers/pool/quota-history?'+f'start={upstream.started_at-7200}&end={upstream.started_at+60}'
        assert request(history_url)[0] == 401
        history = request(history_url, headers=headers)[1]
        assert history['scope'] == 'shared_pool' and history['sample_interval_seconds'] == 300
        assert len(history['series']) == 6 and len(history['account_ids']) == 5
        assert {series['limit_id'] for series in history['series']} == {'claude','account:account-alpha','account:account-beta'}
        assert next(series for series in history['series'] if series['limit_id']=='account:account-beta' and series['window_minutes']==10080)['points'][0]['remaining_percent'] == 0
        with sqlite3.connect(state_root/'quota_history.sqlite3') as database:
            database.execute('UPDATE quota_samples SET observed_at=observed_at-3600')
        upstream.quota['accounts'][0]['windows']['five_hour']['remaining_percent'] = 78.2
        upstream.quota['accounts'][0]['windows']['seven_day']['remaining_percent'] = 39
        fractional = request(endpoint, {'force': True}, headers)[1]['observation']['quota']
        assert fractional['rate_limits_by_limit_id']['claude']['primary']['used_percent'] == 60.9
        history = request(history_url, headers=headers)[1]
        pool = next(series for series in history['series'] if series['limit_id']=='claude' and series['window_minutes']==300)
        assert [point['remaining_percent'] for point in pool['points']] == [40,39.1]
        assert all(len(series['points'])==2 for series in history['series'])
        assert all(series['window_minutes'] in [300,10080] for series in history['series'])
        history_calls = len(upstream.calls)
        assert request(history_url, headers=headers)[1] == history and len(upstream.calls) == history_calls
        checks.append('Pool and individual 5h/7d observations persist in the shared SQLite store; zero is recorded, stale/disabled/unknown values are omitted, fractional averages are exact, and history reads make no upstream query')
        seed_pool_history(state_root)

        browser = Browser(stack, root)
        request(browser.base+'/window/rect', {'width': 1420, 'height': 960})
        browser.navigate(url)
        wait_for(lambda: browser.execute('return typeof cpaQuotas!=="undefined" && state?.provider_quotas?.pool?.observation?.quota;'))
        headers={'X-EMP-Session':browser.execute('return localStorage.getItem("emp_management_session_v1");')}
        for language in ['zh-CN', 'en']:
            request(browser.base+'/window/rect', {'width':1420 if language=='zh-CN' else 520,'height':960})
            browser.execute('setLanguage(arguments[0]);setTheme(arguments[1]);', language, 'light' if language=='zh-CN' else 'dark')
            rect = browser.execute('const box=document.querySelector("[data-service=\\"provider:pool\\"] .service-quota").getBoundingClientRect();return {x:Math.round(box.x+box.width/2),y:Math.round(box.y+box.height/2)};')
            for position in [{'x': 5, 'y': 5}, rect]:
                assert request(browser.base+'/actions', {'actions': [{'type': 'pointer', 'id': 'mouse',
                    'parameters': {'pointerType': 'mouse'}, 'actions': [{'type': 'pointerMove', 'duration': 150, **position}]}]})[0] == 200
            wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden;'))
            assert browser.execute('return [...document.querySelectorAll("[data-service=\\"provider:pool\\"] .presentation-ring-value")].map(node=>node.textContent).join(",")==="40%,20%" && document.getElementById("shadow_feedback").textContent.includes("79%");')
            (args.output/('pool-'+language+'.png')).write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
            browser.execute('document.querySelector("[data-ui-action=provider-quota-history][data-id=pool]").click();')
            wait_for(lambda: browser.execute('return document.querySelector("#quota_history_plot svg") && document.getElementById("quota_history_legend").textContent.includes("40%");'))
            assert browser.execute('return document.getElementById("quota_history_account").value==="claude" && document.querySelectorAll("#quota_history_account option").length===6;')
            (args.output/('trend-pool-'+language+'.png')).write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
            browser.execute('quotaDisplay.set("used");quotaHistory.redraw();')
            assert browser.execute('return document.getElementById("quota_history_legend").textContent.includes("61%");')
            browser.execute('quotaDisplay.set("remaining");quotaHistory.redraw();')
            browser.execute('const select=document.getElementById("quota_history_account");select.value="account:account-alpha";select.dispatchEvent(new Event("change",{bubbles:true}));')
            assert browser.execute('return document.getElementById("quota_history_legend").textContent.includes("39%");')
            (args.output/('trend-account-'+language+'.png')).write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
            browser.execute('quotaDisplay.set("used");quotaHistory.redraw();')
            assert browser.execute('return document.getElementById("quota_history_legend").textContent.includes("61%");')
            browser.execute('quotaDisplay.set("remaining");quotaHistory.redraw();')
            browser.execute('const select=document.getElementById("quota_history_account");select.value="account:account-beta";select.dispatchEvent(new Event("change",{bubbles:true}));')
            assert browser.execute('return document.getElementById("quota_history_legend").textContent.includes("0%");')
            browser.execute('const select=document.getElementById("quota_history_account");select.value="account:account-old";select.dispatchEvent(new Event("change",{bubbles:true}));')
            assert browser.execute('return !document.querySelector("#quota_history_plot svg");')
            browser.execute('closeModal();')
            checks.append(f'{language}: production trend entry switches between the pool, every account and both windows; zero stays visible and unknown accounts show no invented curve')
            browser.execute('document.querySelector("[data-ui-action=provider-details][data-id=pool]").click();')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-ui-action=provider-edit][data-id=pool]");'))
            browser.execute('document.querySelector("[data-ui-action=provider-edit][data-id=pool]").click();')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-cpa-quota-help]");'))
            browser.execute('document.querySelector("[data-cpa-quota-help]").focus();')
            wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden && !!document.querySelector("#shadow_feedback a");'))
            assert browser.execute('return new URL(document.querySelector("#shadow_feedback a").href).hash;') == '#cpa-operator-integration'
            (args.output/('integration-help-'+language+'.png')).write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
            browser.execute('closeModal();')
            checks.append(f'{language}: CPA connection form exposes quota integration help and the operator contract without changing connection settings')
        browser.execute('document.body.dataset.quotaStyle="bar";renderServices();document.querySelector("[data-service=\\"provider:pool\\"] .service-quota").focus();')
        wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden;'))
        assert browser.execute('return [...document.querySelectorAll("[data-service=\\"provider:pool\\"] .quota-value")].map(node=>node.textContent).join(",")==="40%,20%";')
        (args.output/'pool-bars.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        browser.execute('quotaDisplay.set("used");renderServices();document.querySelector("[data-service=\\"provider:pool\\"] .service-quota").focus();')
        wait_for(lambda: browser.execute('return document.getElementById("shadow_feedback").textContent.includes("22%");'))
        assert browser.execute('return [...document.querySelectorAll("[data-service=\\"provider:pool\\"] .quota-value")].map(node=>node.textContent).join(",")==="61%,81%";')
        browser.execute('quotaDisplay.set("remaining");renderServices();')
        assert len(upstream.calls) == history_calls
        checks.append('Production Chinese/English rings, bars, per-account hover and trend percentages round upward after remaining/used conversion, while raw history retains fractional precision; no extra quota queries')
        browser.execute('state.native_account=null;state.accounts=[];document.getElementById("refresh_all").click();')
        wait_for(lambda: len(upstream.calls) == history_calls+1 and browser.execute('return !document.getElementById("refresh_all").disabled;'))
        checks.append('Refresh All includes CPA pools and makes one snapshot request without a model call')

        upstream.mode = 'unsupported'
        browser.execute('document.getElementById("refresh_all").click();')
        wait_for(lambda: browser.execute('return state.provider_quotas.pool.observation.error_code==="cpa_quota_unsupported" && !cpaQuotas.busy.has("pool");'))
        assert browser.execute('return !document.querySelector("[data-ui-action=provider-quota-history][data-id=pool]") && !document.querySelector("[data-service=\\"provider:pool\\"] .service-quota") && !document.querySelector("[data-ui-action=provider-quota-refresh][data-id=pool]");')
        before_recovery = len(upstream.calls)
        upstream.mode = 'ok'
        browser.execute('document.getElementById("refresh_all").click();')
        wait_for(lambda: len(upstream.calls)==before_recovery+1 and browser.execute('return state.provider_quotas.pool.observation.error_code===null && !cpaQuotas.busy.has("pool");'))
        assert browser.execute('return !!document.querySelector("[data-ui-action=provider-quota-history][data-id=pool]") && !!document.querySelector("[data-ui-action=provider-quota-refresh][data-id=pool]") && !!document.querySelector("[data-service=\\"provider:pool\\"] .service-quota");')
        checks.append('Unsupported quota hides service Refresh, Trend and both meters; Refresh All restores them when the endpoint becomes available without waiting for cache expiry')

        for mode, error in [('unavailable', 'cpa_quota_unavailable'), ('denied', 'cpa_quota_access_denied'),
                            ('unsupported', 'cpa_quota_unsupported'), ('redirect', 'cpa_quota_connection_failed'),
                            ('invalid', 'cpa_quota_invalid_response')]:
            upstream.mode = mode
            status, failure = request(endpoint, {'force': True}, headers)
            assert status == 200 and failure['observation']['error_code'] == error, failure
        for mode in ['stale', 'empty']:
            upstream.mode = mode
            unknown = request(endpoint, {'force': True}, headers)[1]['observation']['quota']
            assert all(window['used_percent'] is None and window['resets_at'] is None
                for window in unknown['rate_limits_by_limit_id']['claude'].values())
        checks.append('Unavailable, denied, unsupported and unrelated endpoints have explicit errors; stale or empty pools remain unknown instead of zero or full')
        unavailable_history = request(history_url, headers=headers)[1]
        assert all(series['limit_id'] in ['claude','account:account-alpha','account:account-beta'] for series in unavailable_history['series'])
        assert len(upstream.calls) >= history_calls
        upstream.mode = 'ok'
        current = request(base+'/api/config', headers=headers)[1]
        current['providers'][0]['api_key'] = 'fixture-rotated'
        assert request(base+'/api/config', current, headers)[0] == 200
        rotated = request(base+'/api/config', headers=headers)[1]['provider_quotas']['pool']
        assert rotated['identity'] != result['identity'] and rotated['observation'] is None
        assert request(history_url, headers=headers)[1]['series'] == []
        assert request(endpoint, {}, headers)[1]['observation']['error_code'] is None
        assert upstream.calls[-1][1] == 'Bearer fixture-rotated'
        assert request(base+'/api/calls', headers=headers)[1]['records'] == []
        assert upstream.model_calls == 0
        checks.append('Changing the CPA Key invalidates its quota cache; quota reads create no inference records and make zero model calls')
        upstream.quota['accounts'][0]['windows']['seven_day']['remaining_percent'] = 0
        exhausted = request(endpoint, {'force': True}, headers)[1]['observation']['quota']
        assert exhausted['rate_limits_by_limit_id']['claude']['primary']['used_percent'] == 100
        assert exhausted['accounts'][0]['windows']['five_hour']['remaining_percent'] == 78.2
        upstream.quota['accounts'][0]['windows']['seven_day']['remaining_percent'] = 39
        upstream.quota['accounts'][1]['windows']['seven_day']['remaining_percent'] = 1
        recovered = request(endpoint, {'force': True}, headers)[1]['observation']['quota']
        assert recovered['rate_limits_by_limit_id']['claude']['primary']['used_percent'] == 40.9
        upstream.quota['accounts'][1]['windows']['seven_day']['remaining_percent'] = 0
        checks.append('Weekly exhaustion makes every affected 5h pool contribution zero without changing individual 5h observations; recovery restores its contribution')
        guide=(Path(__file__).resolve().parents[2]/'docs/usage-accounting.md').read_text()
        example=json.loads(re.search(r'```json\n(.*?)\n```',guide.split('### CPA operator integration',1)[1],re.S)[1])
        saved_quota=upstream.quota
        upstream.mode,upstream.quota='ok',example
        documented=request(endpoint,{'force':True},headers)[1]['observation']
        assert documented['error_code'] is None
        limits=documented['quota']['rate_limits_by_limit_id']['claude']
        assert limits['primary']['used_percent']==60 and limits['secondary']['used_percent']==80
        upstream.quota=saved_quota
        checks.append('The documented CPA JSON example is accepted by the production endpoint and produces the stated 40%/20% usable summary')
        (root/'background/state').mkdir(parents=True)
        shutil.copy2(state_root/'quota_history.sqlite3', root/'background/state/quota_history.sqlite3')
        configuration['providers'][0]['enabled'] = True
        before_start = len(upstream.calls)
        background, _, _, background_headers = start_backend(stack, args.emp.resolve(), root/'background', configuration)
        wait_for(lambda: len(upstream.calls)==before_start+1)
        restored = request(background+'/api/providers/pool/quota-history?'+f'start={upstream.started_at-86400}&end={int(time.time())+60}', headers=background_headers)[1]
        assert any(point['observed_at']<upstream.started_at-3600 for series in restored['series'] for point in series['points'])
        assert len(upstream.calls)==before_start+1
        checks.append('A fresh backend reopens persisted pool history and samples an enabled CPA service before any WebUI opens')
        time.sleep(max(0, upstream.started_at+46-time.time()))
        assert len(upstream.calls)==before_start+1
        checks.append('The 44-second shared worker reuses the fresh five-minute CPA observation without extra upstream polling')
    report = {'passed': True, 'checks': checks, 'real_model_calls': 0}
    (args.output/'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps(report, ensure_ascii=False))


if __name__ == '__main__':
    main()
