"""Tutorial selection and recovery in a real browser with disposable EMP data."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile
import time
import urllib.request
from unittest.mock import patch

import candidate_web
from e2e_refinements import Browser
from local_backend import request, wait_for
from onboarding_preview import start

HERE = Path(__file__).resolve().parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    checks, measurements = [], []
    with tempfile.TemporaryDirectory(prefix='emp-tutorials-') as directory, contextlib.ExitStack() as stack:
        root = Path(directory)
        url = start(stack, args.emp.resolve(), root, 'http://127.0.0.1:4201/?review=1')
        before = [(path, path.read_bytes()) for path in root.glob('*/config.json')]
        assert len(before) == 2
        browser = Browser(stack, root, reduced_motion=0)
        browser.navigate(url)
        browser.execute('document.getElementById("tutorials_open").click();')
        ids = browser.execute('return [...document.querySelectorAll("[data-tutorial]")].map(button=>button.dataset.tutorial);')
        # Exercise a cold asset; already decoded GIFs can be served from the browser's image cache.
        original = candidate_web.asset_file
        def missing(web, name):
            return root / 'missing.gif' if name == 'tutorial-00-native-name.gif' else original(web, name)
        with patch.object(candidate_web, 'asset_file', missing):
            browser.execute('document.querySelector("[data-tutorial=native_name]").click();')
            wait_for(lambda: browser.execute('return document.querySelector(".tutorial-placeholder button")?.hidden===false;'))
        browser.execute('document.querySelector(".tutorial-placeholder button").click();')
        wait_for(lambda: browser.execute('return document.querySelector("#tutorial_media img")?.naturalWidth>0;'))
        checks.append('A real asset 404 can be retried without closing the tutorial')
        local_assets = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        for identifier in ids:
            browser.execute('document.querySelector(`[data-tutorial="${arguments[0]}"]`).click();', identifier)
            wait_for(lambda: browser.execute('return document.querySelector("#tutorial_media img")?.naturalWidth>0 && document.getElementById("tutorial_media").getAttribute("aria-busy")==="false";'))
            source = browser.execute('return document.querySelector("#tutorial_media img").src;')
            with local_assets.open(source, timeout=15) as response:
                assert response.headers.get_content_type() == 'image/gif'
                expected = HERE.parents[1] / 'docs/assets/tutorials' / source.split('/tutorial-')[1]
                assert response.read() == expected.read_bytes()
        assert 'tunnel' in ids
        checks.append('Every tutorial, including LAN transfer, serves the original local GIF bytes')
        browser.execute('closeModal();document.getElementById("tutorials_open").click();')
        assert browser.execute('return document.querySelector("[data-tutorial][aria-pressed=true]").dataset.tutorial;') == ids[-1]
        browser.execute('const last=document.querySelector("[data-tutorial][aria-pressed=true]");last.focus();last.dispatchEvent(new KeyboardEvent("keydown",{key:"Home",bubbles:true}));')
        assert browser.execute('return document.activeElement.dataset.tutorial;') == ids[0]
        checks.append('Reopening retains the selected tutorial; keyboard navigation reaches the list start')
        assert all(path.read_bytes() == content for path, content in before)
        checks.append('Viewing tutorials does not change stored configuration')

        for lang, width, height, theme in [('zh-CN',1440,1000,'light'),('en',1280,900,'dark'),('zh-CN',560,800,'light')]:
            request(browser.base+'/window/rect', {'width':width,'height':height})
            browser.execute('closeModal();setLanguage(arguments[0]);setTheme(arguments[1]);tutorials.open();', lang, theme)
            wait_for(lambda: browser.execute('return document.querySelector("#tutorial_media img")?.naturalWidth>0;'))
            time.sleep(.6)
            screenshot(browser, args.output/f'tutorials-{lang}-{width}.png')

        # Lists with the same size as the reported migration screenshot, without real credentials.
        browser.execute('closeModal();state.providers=Array.from({length:8},(_,i)=>({...state.providers[0],id:`sample-${i}`,name:`Sample service ${i+1}`}));state.models=[];')
        for width, height in [(1280,900),(1280,600),(560,800)]:
            request(browser.base+'/window/rect', {'width':width,'height':height})
            browser.execute('setLanguage("zh-CN");setTheme("light");')
            for mode in ['import','export','tunnel']:
                browser.execute('migration.open();document.querySelector(`[data-migration-mode="${arguments[0]}"]`).click();',mode)
                time.sleep(.3)
                measurements.append(browser.execute('const dialog=document.querySelector(".modal"),body=document.getElementById("modal_body");return {mode:arguments[0],viewport:[innerWidth,innerHeight],dialogHeight:dialog.getBoundingClientRect().height,bodyHeight:body.clientHeight,contentHeight:body.scrollHeight,overflow:document.documentElement.scrollWidth-innerWidth};',mode))
                screenshot(browser,args.output/f'migration-{mode}-{width}-{height}.png')
                browser.execute('document.getElementById("modal_back").click();')
                assert browser.execute('return !!document.getElementById("migration_chooser");')
        checks.append('All migration workflows return to their chooser with eight synthetic services')

        reduced = root/'reduced'; reduced.mkdir()
        quiet = Browser(stack, reduced, reduced_motion=1)
        quiet.navigate(url)
        quiet.execute('document.getElementById("tutorials_open").click();')
        assert quiet.execute('return !document.querySelector("#tutorial_media img") && !document.querySelector(".tutorial-placeholder button").hidden;')
        quiet.execute('document.querySelector(".tutorial-placeholder button").click();')
        wait_for(lambda: quiet.execute('return document.querySelector("#tutorial_media img")?.naturalWidth>0;'))
        checks.append('Reduced motion waits for explicit playback')
    result = {'checks':checks,'measurements':measurements}
    (args.output/'result.json').write_text(json.dumps(result,indent=2))
    print(json.dumps({'checks':checks,'output':str(args.output)}))


def screenshot(browser, path):
    status, shot = request(browser.base+'/screenshot')
    assert status == 200
    path.write_bytes(base64.b64decode(shot['value']))


if __name__ == '__main__':
    main()
