"""Review the shared recovery/lifecycle UI without touching real EMP state."""
import argparse
import base64
import contextlib
import json
import tempfile
from pathlib import Path
from e2e import Browser
from e2e_environment import request, wait_for
from preview import fixture, preview


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-review-e2e-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        endpoint, target, bundle = fixture(stack, args.emp.resolve(), root)
        backend, _, headers, upstream = endpoint
        frontend, _ = preview(stack, backend, bundle, headers['X-EMP-Session'])
        browser = Browser(stack, root)
        before = (target / 'config.json').read_bytes()
        native_before = (target / 'codex_home/auth.json').read_bytes()
        for lang, width in [('zh-CN', 390), ('en', 1280)]:
            browser.navigate(frontend)
            request(browser.base + '/window/rect', {'width': width, 'height': 1000})
            browser.execute('setLanguage(arguments[0]);document.getElementById("shadow_review_button").click();', lang)
            wait_for(lambda: json.loads((target / 'config.json').read_text())['language'] == lang)
            before = (target / 'config.json').read_bytes()
            assert browser.execute('return document.querySelectorAll("#modal_body [data-shadow-demo]").length===6;')
            assert browser.execute('return document.querySelectorAll("#modal_body li").length===4;')
            assert browser.execute('return document.documentElement.scrollWidth<=innerWidth;')
            _, png = request(browser.base + '/screenshot')
            args.output.with_name(args.output.stem + f'-overview-{lang}.png').write_bytes(base64.b64decode(png['value']))
            browser.execute('document.querySelector("#modal_body [data-shadow-demo=lifecycle]").click();')
            assert browser.execute('return document.getElementById("modal_body").textContent.includes(arguments[0]);', '继续运行' if lang == 'zh-CN' else 'stays running')
            browser.execute('document.getElementById("modal_submit").click();')
            wait_for(lambda: browser.execute('return document.getElementById("integration").dataset.state==="native";'))
            assert browser.execute('return !document.getElementById("integration_toggle").disabled;')
            browser.execute('document.getElementById("integration_toggle").click();document.getElementById("modal_submit").click();')
            wait_for(lambda: browser.execute('return document.getElementById("integration").dataset.state==="emp_applied";'))
            browser.execute('window.confirm=()=>true;quitEmp();')
            wait_for(lambda: browser.execute('return document.getElementById("modal_title").textContent.includes(arguments[0]);', '正在退出' if lang == 'zh-CN' else 'is exiting'))
            assert request(backend + '/healthz')[0] == 200
            assert (target / 'config.json').read_bytes() == before
            checks.append(f'{lang}: overview, restore/re-enable/quit UI without backend mutations')
        for reason in ['key_mismatch', 'key_missing', 'key_invalid', 'key_permissions', 'config_invalid', 'reset_incomplete']:
            request(browser.base + '/url', {'url': frontend + '/recovery?reason=' + reason})
            wait_for(lambda: browser.execute('return !document.getElementById("reset").disabled;'))
            assert browser.execute('return !document.getElementById("retry") && !document.getElementById("path") && document.querySelectorAll("main button").length===2;')
            assert not browser.execute('return document.getElementById("keepBackup").checked;')
            assert browser.execute('return document.querySelector("#recovery_demo_controls a").getAttribute("href")==="/?review=1";')
        for keep, lang, width in [(False, 'zh', 390), (True, 'en', 1280)]:
            request(browser.base + '/url', {'url': frontend + '/recovery'})
            wait_for(lambda: browser.execute('return !document.getElementById("reset").disabled;'))
            request(browser.base + '/window/rect', {'width': width, 'height': 1000})
            browser.execute('const language=document.getElementById("language");language.value=arguments[0];language.dispatchEvent(new Event("change"));', lang)
            browser.execute('document.getElementById("reset").click();')
            assert browser.execute('return document.getElementById("confirm").open && document.documentElement.scrollWidth<=innerWidth;')
            _, png = request(browser.base + '/screenshot')
            args.output.with_name(args.output.stem + f'-recovery-{lang}.png').write_bytes(base64.b64decode(png['value']))
            browser.execute('document.getElementById("cancel").click();document.getElementById("reset").click();document.getElementById("keepBackup").checked=arguments[0];document.getElementById("commit").click();', keep)
            wait_for(lambda: browser.execute('return document.getElementById("reset").disabled;'))
            assert browser.execute('return document.getElementById("status").textContent.includes("recovery-backup-demo");') == keep
            browser.execute('document.getElementById("demo_again").click();')
            wait_for(lambda: browser.execute('return !document.getElementById("reset").disabled;'))
            browser.execute('document.getElementById("exit").click();')
            wait_for(lambda: browser.execute('return document.getElementById("exit").disabled;'))
        assert request(frontend + '/api/recovery/reset', {})[0] == 403
        assert (target / 'config.json').read_bytes() == before
        checks.append('6 recovery scenarios with only reset/exit, optional backup, cancel, repeat and server-side reset guard')
        browser.navigate(frontend)
        browser.execute('window.confirm=()=>true;window.EMP_REVIEW_ACTION("removal");')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-ui-action=account-remove]");'))
        browser.execute('document.querySelector("[data-ui-action=account-remove]").click();')
        assert browser.execute('return document.querySelector("[data-ui-action=account-remove]").disabled;')
        wait_for(lambda: browser.execute('return !Array.from(document.querySelectorAll("[data-service]")).some(e=>e.dataset.service==="account:review-removal") && document.getElementById("modal_backdrop").classList.contains("hidden");'))
        browser.execute('window.EMP_REVIEW_ACTION("historical");')
        wait_for(lambda: browser.execute('return document.querySelectorAll("[data-preference-kind]").length===2;'))
        assert browser.execute('return document.getElementById("modal_submit").disabled;')
        assert (target / 'codex_home/auth.json').read_bytes() == native_before
        assert upstream.calls == 0
        checks.append('Disposable account removal displays progress and updates list; zero model calls')
        checks.append('Historical model-setting conflicts remain reviewable in the same workspace')
    args.output.write_text(json.dumps({'passed': True, 'checks': checks, 'production_state_used': False}, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': checks}))


if __name__ == '__main__':
    main()
