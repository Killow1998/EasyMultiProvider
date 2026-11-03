#!/usr/bin/env python3
"""Appearance persistence, launcher updates and balance presentation in a real browser."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile
from e2e_environment import request, start_backend, wait_for
from e2e_refinements import Browser


def click(browser, selector):
    element = browser.execute('return document.querySelector(arguments[0]);', selector)
    assert element, selector
    identifier = element['element-6066-11e4-a52e-4f735466cecf']
    status, response = request(browser.base + '/element/' + identifier + '/click', {})
    assert status == 200, response


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-appearance-') as directory, contextlib.ExitStack() as stack:
        root = Path(directory)
        data = root / 'data'
        desktop = root / 'home/Desktop'
        desktop.mkdir(parents=True)
        applications = data / 'applications'
        applications.mkdir(parents=True)
        entry = '[Desktop Entry]\nType=Application\nName=EMP\nExec=/fixture/EMP\nIcon=old.svg\n'
        for path in [applications / 'easy-multi-provider-user.desktop', desktop / 'easy-multi-provider.desktop']:
            path.write_text(entry)
        (root/'backend').mkdir()
        base, url, state_dir, headers, _ = start_backend(stack, args.emp.resolve(), root/'backend',
            env_overrides={'HOME': str(root/'home'), 'XDG_DATA_HOME': str(data)})
        original_config = (state_dir.parent / 'config.json').read_bytes()
        browser = Browser(stack, root)
        request(browser.base+'/window/rect', {'width': 1280, 'height': 960})
        browser.navigate(url)
        headers = {'X-EMP-Session': json.loads((state_dir/'web-session.json').read_text())['token']}
        browser.execute('setTheme("light");setLanguage("zh-CN");settings.open("general");')
        click(browser, '[data-app-icon-open]')
        wait_for(lambda: browser.execute('return [...document.querySelectorAll(".app-icon-choice img")].every(image=>image.complete && image.naturalWidth>0);'))
        for identifier in browser.execute('return [...document.querySelectorAll("[data-app-icon]")].map(button=>button.dataset.appIcon).filter(id=>id!=="custom");'):
            click(browser, '[data-app-icon="'+identifier+'"]')
            wait_for(lambda: browser.execute('return document.getElementById("app_icon_preview").complete && document.getElementById("app_icon_preview").naturalWidth===512 || document.getElementById("app_icon_preview").naturalWidth===1024;'))
        click(browser, '[data-app-icon="emp"]')
        click(browser, '[data-icon-style="mono"]')
        click(browser, '#modal_submit')
        wait_for(lambda: browser.execute('return state.app_icon.preset==="emp" && state.app_icon.monochrome && !document.getElementById("modal_submit").disabled;'))
        browser.execute('''
          window.templatePixels=null;
          (async()=>{
            const image=new Image();image.src=state.app_icon.image_url;await image.decode();
            const canvas=document.createElement('canvas');canvas.width=canvas.height=512;
            const context=canvas.getContext('2d');context.drawImage(image,0,0);
            window.templatePixels=[[181,256],[256,309],[256,370],[0,0]].map(([x,y])=>context.getImageData(x,y,1,1).data[3]);
          })();
        ''')
        assert wait_for(lambda: browser.execute('return window.templatePixels;')) == [0,255,0,0]
        checks.append('EMP monochrome preserves the color logo proportions and its glasses, smile and background cutouts after saving')
        click(browser, '[data-app-icon="codex"]')
        click(browser, '[data-icon-style="mono"]')
        click(browser, '#modal_submit')
        wait_for(lambda: browser.execute('return state.app_icon.preset==="codex" && state.app_icon.monochrome && !document.getElementById("modal_submit").disabled;'))
        first = request(base+'/api/config', headers=headers)[1]['app_icon']
        assert first['preset'] == 'codex' and first['monochrome']
        for path in [applications / 'easy-multi-provider-user.desktop', desktop / 'easy-multi-provider.desktop']:
            assert 'Exec=/fixture/EMP\n' in path.read_text()
            assert 'Icon='+str(state_dir/'app-icons'/(first['version']+'.png')) in path.read_text()
        assert (state_dir/'appearance.json').stat().st_mode & 0o777 == 0o600
        assert (state_dir.parent/'config.json').read_bytes() == original_config
        checks.append('Preset and monochrome save through the real API; desktop and menu launchers update without changing provider configuration')

        svg = root/'square.svg'
        svg.write_text('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="12" fill="#f08020"/><circle cx="32" cy="32" r="16" fill="#365fa8"/></svg>')
        browser.execute('const transfer=new DataTransfer();transfer.items.add(new File([arguments[0]],"square.svg",{type:"image/svg+xml"}));const input=document.getElementById("app_icon_file");input.files=transfer.files;input.dispatchEvent(new Event("change",{bubbles:true}));', svg.read_text())
        wait_for(lambda: browser.execute('return document.querySelector("[data-app-icon=custom]").getAttribute("aria-pressed")==="true";'))
        click(browser, '[data-icon-style="color"]')
        click(browser, '#modal_submit')
        wait_for(lambda: browser.execute('return state.app_icon.preset==="custom" && !state.app_icon.monochrome && !document.getElementById("modal_submit").disabled;'))
        custom_color = browser.execute('return state.app_icon.version;')
        click(browser, '[data-icon-style="mono"]')
        click(browser, '#modal_submit')
        wait_for(lambda: browser.execute('return state.app_icon.monochrome && !document.getElementById("modal_submit").disabled;'))
        browser.execute('closeModal();settings.open("general");')
        click(browser, '[data-app-icon-open]')
        click(browser, '[data-icon-style="color"]')
        click(browser, '#modal_submit')
        wait_for(lambda: browser.execute('return state.app_icon.preset==="custom" && !state.app_icon.monochrome && !document.getElementById("modal_submit").disabled;'))
        browser.execute('window.iconPixels=null;(async()=>{const image=new Image();image.src=state.app_icon.image_url;await image.decode();const canvas=document.createElement("canvas");canvas.width=canvas.height=512;const context=canvas.getContext("2d");context.drawImage(image,0,0);window.iconPixels=Array.from(context.getImageData(256,256,1,1).data);})();')
        assert wait_for(lambda: browser.execute('return window.iconPixels;')) == [54,95,168,255]
        custom_color = browser.execute('return state.app_icon.version;')
        assert not (state_dir/'app-icons'/(first['version']+'.png')).exists()
        checks.append('Custom SVG import and color to monochrome to color roundtrip preserve the source; obsolete generated icons are removed')

        (root/'restart').mkdir()
        (root/'restart/state').mkdir()
        (root/'restart/state/appearance.json').write_bytes((state_dir/'appearance.json').read_bytes())
        balance_urls = {
            'deepseek': 'https://api.deepseek.com',
            'moonshot': 'https://api.moonshot.ai/v1',
            'moonshot-cn': 'https://api.moonshot.cn/v1',
            'openrouter': 'https://openrouter.ai/api/v1',
            'openai': 'https://api.openai.com/v1',
            'anthropic': 'https://api.anthropic.com/v1',
            'gemini': 'https://generativelanguage.googleapis.com/v1beta/openai',
            'xai': 'https://api.x.ai/v1',
            'zhipu': 'https://api.z.ai/api/paas/v4',
            'inception': 'https://api.inceptionlabs.ai/v1',
        }
        restored_base, _, _, restored_headers, _ = start_backend(stack, args.emp.resolve(), root/'restart',
            configuration={'providers': [{'id': identifier, 'name': identifier, 'base_url': endpoint,
                'protocol': 'chat_completions', 'auth_mode': 'api_key'} for identifier, endpoint in balance_urls.items()], 'models': []})
        restored_config = request(restored_base+'/api/config', headers=restored_headers)[1]
        assert restored_config['app_icon']['version'] == custom_color
        assert set(restored_config['provider_balances']) == {'deepseek', 'moonshot', 'moonshot-cn'}
        for identifier in balance_urls:
            status, result = request(restored_base+'/api/providers/'+identifier+'/balance', {'force': True}, headers=restored_headers)
            if identifier in restored_config['provider_balances']:
                assert status == 200 and result['observation']['error_code'] == 'balance_key_missing', result
            else:
                assert status == 404, result
        assert request(base+'/api/app-icon', {'preset': 'custom', 'monochrome': False, 'png': base64.b64encode(b'not png').decode()}, headers=headers)[0] == 400
        assert request(base+'/api/app-icon', {'preset': 'emp', 'monochrome': False})[0] == 401
        checks.append('A new process loads the saved appearance; balance metadata and refresh routes support only DeepSeek and both Kimi regions; malformed icons and unauthenticated writes are rejected')

        browser.execute('''
          closeModal();window.balanceCalls=0;
          const originalFetch=window.fetch;
          window.fetch=async function(path,options){
            if(/^\\/api\\/providers\\/(deepseek|moonshot)\\/balance$/.test(String(path))){
              window.balanceCalls++;
              const id=String(path).split('/')[3],entry=state.provider_balances[id];
              return new Response(JSON.stringify({identity:entry.identity,observation:{observed_at:Date.now()/1000,error_code:null,
                balance:{kind:'account_balance',amounts:[{currency:'USD',available:12.5}]}}}),{headers:{'Content-Type':'application/json'}});
            }
            return originalFetch(path,options);
          };
          state.providers.push(...Object.entries(officialProviders).filter(([id])=>id!=='chatgpt').map(([id,provider])=>({...provider,id,enabled:true})));
          state.provider_balances={};
          for(const id of ['deepseek','moonshot']){
            state.provider_balances[id]={identity:id,billing_url:id==='deepseek'?'https://platform.deepseek.com/usage':'https://platform.kimi.ai/console/account',refreshable:true,
              observation:{observed_at:Date.now()/1000,error_code:null,balance:{kind:'account_balance',amounts:[{currency:id==='moonshot'?'CNY':'USD',available:id==='deepseek'?0:3.25}]}}};
          }
          fill();
        ''')
        assert browser.execute('return document.querySelector("[data-service=\\"provider:deepseek\\"] .service-balance").textContent.includes("0.00");')
        assert browser.execute('return document.querySelector("[data-service=\\"provider:moonshot\\"] .service-balance").textContent.includes("3.25");')
        assert browser.execute('return Object.keys(officialProviders).filter(id=>!["chatgpt","deepseek","moonshot"].includes(id)).every(id=>{const row=document.querySelector(`[data-service="provider:${id}"]`);return row && !row.querySelector(".service-balance, [data-ui-action=provider-balance-refresh]");});')
        assert browser.execute('return document.querySelector("[data-service=\\"provider:inception\\"] .service-brand title").textContent==="Inception";')
        click(browser, '[data-ui-action="provider-balance-refresh"][data-id="deepseek"]')
        wait_for(lambda: browser.execute('return document.querySelector("[data-service=\\"provider:deepseek\\"] .service-balance").textContent.includes("12.50");'))
        assert browser.execute('return window.balanceCalls;') == 1
        click(browser, '[data-ui-action="provider-balance-refresh"][data-id="moonshot"]')
        wait_for(lambda: browser.execute('return document.querySelector("[data-service=\\"provider:moonshot\\"] .service-balance").textContent.includes("12.50");'))
        assert browser.execute('return window.balanceCalls;') == 2
        if args.output:
            args.output.mkdir(parents=True, exist_ok=True)
            for language in ['zh-CN','en']:
                browser.execute('setLanguage(arguments[0]);', language)
                data=request(browser.base+'/screenshot')[1]['value']
                (args.output/('balances-'+language+'.png')).write_bytes(base64.b64decode(data))
            browser.execute('setLanguage("zh-CN");')
        click(browser, '[data-ui-action="provider-details"][data-id="deepseek"]')
        wait_for(lambda: browser.execute('return document.querySelector(".provider-balance-details a")?.hostname==="platform.deepseek.com";'))
        browser.execute('closeModal();')
        for identifier in ['openrouter', 'openai', 'xai']:
            click(browser, '[data-ui-action="provider-details"][data-id="'+identifier+'"]')
            wait_for(lambda: browser.execute('return !document.getElementById("modal_backdrop").classList.contains("hidden");'))
            assert browser.execute('return !document.querySelector(".provider-balance-details");')
            browser.execute('closeModal();')
        checks.append('Only DeepSeek and Kimi show balances and refresh once per click; other official services omit balance controls and billing sections; the Mercury icon remains visible')
        browser.execute('closeModal();settings.open("general");')
        click(browser, '[data-app-icon-open]')
        if args.output:
            args.output.mkdir(parents=True, exist_ok=True)
            for theme in ['light','dark']:
                browser.execute('setTheme(arguments[0]);', theme)
                data=request(browser.base+'/screenshot')[1]['value']
                (args.output/('icons-'+theme+'.png')).write_bytes(base64.b64decode(data))
        browser.execute('closeModal();setLanguage("en");settings.open("general");')
        click(browser, '[data-app-icon-open]')
        assert browser.execute('return document.getElementById("modal_title").textContent;') == 'EMP icon'
        request(browser.base+'/window/rect', {'width': 640, 'height': 820})
        assert browser.execute('return document.documentElement.scrollWidth<=window.innerWidth;')
        checks.append('Chinese and English navigation, light/dark layout and narrow-screen width remain usable')
    report={'checks':checks,'real_provider_requests':0}
    if args.output:
        (args.output/'report.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps(report,ensure_ascii=False))


if __name__ == '__main__':
    main()
