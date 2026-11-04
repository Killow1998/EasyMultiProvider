"""Caller-scoped CPA quota reads, caching and the production service UI."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile

from e2e_environment import request, wait_for
from e2e_refinements import Browser
from local_backend import start_backend
from cpa_quota_fixture import start_pool


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
        configuration = {'providers': [{'id': 'pool', 'name': 'CPA', 'protocol': 'anthropic_messages',
            'execution_backend': 'claude_cli', 'auth_mode': 'api_key', 'api_key': 'fixture-pool',
            'base_url': f'http://127.0.0.1:{upstream.server_port}/cpa/v1/'}], 'models': []}
        base, url, _, headers = start_backend(stack, args.emp.resolve(), root/'backend', configuration)
        endpoint = base+'/api/providers/pool/quota'
        assert request(endpoint, {})[0] == 401
        assert request(endpoint, {'force': 'yes'}, headers)[0] == 400
        assert upstream.calls == []
        status, result = request(endpoint, {}, headers)
        assert status == 200 and result['observation']['error_code'] is None, result
        quota = result['observation']['quota']
        limits = quota['rate_limits_by_limit_id']['claude']
        assert limits['primary']['used_percent'] == 40
        assert limits['secondary']['used_percent'] == 80
        assert limits['primary']['resets_at'] == upstream.started_at+3600
        assert limits['secondary']['resets_at'] == upstream.started_at+172800
        assert len(quota['accounts']) == 5 and 'upstream-only-private' not in json.dumps(result)
        assert upstream.calls == [('/cpa/v1/quota', 'Bearer fixture-pool')]
        cached = request(endpoint, {}, headers)[1]
        assert cached == result and len(upstream.calls) == 1
        for route in ['/api/config', '/api/accounts']:
            metadata = request(base+route, headers=headers)[1]['provider_quotas']['pool']
            assert metadata['identity'] == result['identity'] and metadata['observation'] == result['observation']
        checks.append('Real management API reuses the CPA Key, retains the URL prefix, averages only enabled fresh known values including zero, selects the nearest reset, omits extra upstream fields and caches reads')

        browser = Browser(stack, root)
        request(browser.base+'/window/rect', {'width': 1420, 'height': 960})
        browser.navigate(url)
        wait_for(lambda: browser.execute('return typeof cpaQuotas!=="undefined" && state?.provider_quotas?.pool?.observation?.quota;'))
        headers={'X-EMP-Session':browser.execute('return localStorage.getItem("emp_management_session_v1");')}
        for language in ['zh-CN', 'en']:
            browser.execute('setLanguage(arguments[0]);setTheme("light");', language)
            rect = browser.execute('const box=document.querySelector("[data-service=\\"provider:pool\\"] .service-quota").getBoundingClientRect();return {x:Math.round(box.x+box.width/2),y:Math.round(box.y+box.height/2)};')
            for position in [{'x': 5, 'y': 5}, rect]:
                assert request(browser.base+'/actions', {'actions': [{'type': 'pointer', 'id': 'mouse',
                    'parameters': {'pointerType': 'mouse'}, 'actions': [{'type': 'pointerMove', 'duration': 150, **position}]}]})[0] == 200
            wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden;'))
            (args.output/('pool-'+language+'.png')).write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        browser.execute('document.body.dataset.quotaStyle="bar";renderServices();document.querySelector("[data-service=\\"provider:pool\\"] .service-quota").focus();')
        wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden;'))
        (args.output/'pool-bars.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        assert len(upstream.calls) == 1
        checks.append('Production Chinese/English service rows reuse ring and bar quotas; pointer hover and keyboard focus open per-account details without another quota query')
        browser.execute('state.native_account=null;state.accounts=[];document.getElementById("refresh_all").click();')
        wait_for(lambda: len(upstream.calls) == 2 and browser.execute('return !document.getElementById("refresh_all").disabled;'))
        checks.append('Refresh All includes CPA pools and makes one snapshot request without a model call')

        for action in ['document.getElementById("refresh_all")',
                       'document.querySelector("[data-ui-action=provider-quota-refresh][data-id=pool]")']:
            upstream.mode = 'unsupported'
            browser.execute('document.getElementById("refresh_all").click();')
            wait_for(lambda: browser.execute('return state.provider_quotas.pool.observation.error_code==="cpa_quota_unsupported" && !cpaQuotas.busy.has("pool");'))
            before_recovery = len(upstream.calls)
            upstream.mode = 'ok'
            browser.execute(action+'.click();')
            wait_for(lambda: len(upstream.calls)==before_recovery+1 and browser.execute('return state.provider_quotas.pool.observation.error_code===null && !cpaQuotas.busy.has("pool");'))
        checks.append('Refresh All and service Refresh recover immediately when a previously unsupported quota endpoint becomes available, without waiting for cache expiry')

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
        upstream.mode = 'ok'
        current = request(base+'/api/config', headers=headers)[1]
        current['providers'][0]['api_key'] = 'fixture-rotated'
        assert request(base+'/api/config', current, headers)[0] == 200
        rotated = request(base+'/api/config', headers=headers)[1]['provider_quotas']['pool']
        assert rotated['identity'] != result['identity'] and rotated['observation'] is None
        assert request(endpoint, {}, headers)[1]['observation']['error_code'] is None
        assert upstream.calls[-1][1] == 'Bearer fixture-rotated'
        assert request(base+'/api/calls', headers=headers)[1]['records'] == []
        assert upstream.model_calls == 0
        checks.append('Changing the CPA Key invalidates its quota cache; quota reads create no inference records and make zero model calls')
    report = {'passed': True, 'checks': checks, 'real_model_calls': 0}
    (args.output/'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps(report, ensure_ascii=False))


if __name__ == '__main__':
    main()
