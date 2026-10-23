"""Exercise the opt-in onboarding fixture from the real-data Shadow entry."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile
from e2e_refinements import Browser
from local_backend import request, wait_for


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url',required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='emp-onboarding-review-') as directory,contextlib.ExitStack() as stack:
        browser=Browser(stack,Path(directory))
        def shot(name):
            status,image=request(browser.base+'/screenshot');assert status==200
            (args.output/name).write_bytes(base64.b64decode(image['value']))
        browser.navigate(args.url)
        assert browser.execute('return window.EMP_REAL_SHADOW===true;')
        assert browser.execute('return reviewRows;')==['ultra','onboarding','lan']
        shot('shadow-review.png')
        browser.execute('document.querySelector("[data-shadow-demo=onboarding]").click();')
        def ready():
            error=browser.execute('return document.getElementById("modal_status")?.textContent;')
            assert not error,error
            return browser.execute('return !!document.querySelector("[data-ui-action=account-import-device]");')
        try:wait_for(ready,seconds=50)
        except Exception:
            print(browser.diagnostics())
            shot('shadow-onboarding-error.png')
            raise
        assert browser.execute('return !!document.getElementById("onboarding_fixture_banner") && !window.EMP_REAL_SHADOW;')
        browser.execute('closeModal();')
        shot('shadow-local-key-row.png')
        browser.execute('migration.open();document.querySelector("[data-migration-mode=tunnel]").click();document.querySelector("[data-lan-create]").click();')
        try:
            wait_for(lambda:browser.execute('return document.getElementById("lan_share_code").value;'),seconds=90)
        except TimeoutError:
            raise AssertionError(browser.execute('return document.getElementById("lan_send_status").textContent;')) from None
        browser.execute('document.querySelector("[data-lan-cancel]").click();')
        wait_for(lambda:browser.execute('return document.getElementById("lan_share_block").hidden;'))
        browser.execute('closeModal();nativeLogin.open();')
        wait_for(lambda:browser.execute('return !document.querySelector("[data-native-login-start]").disabled;'))
        browser.execute('document.querySelector("[data-native-login-start]").click();')
        wait_for(lambda:browser.execute('return !!document.querySelector(".device-code");'))
        shot('shadow-device-login.png')
        wait_for(lambda:browser.execute('return state.native_account.auth_mode==="subscription";'),seconds=25)
        assert browser.execute('return document.getElementById("native_login_progress").textContent.includes(tr("login.completed"));')
        browser.execute('closeModal();migration.open();document.querySelector("[data-migration-mode=tunnel]").click();')
        for language in ['zh-CN','en']:
            for width in [1280,560]:
                request(browser.base+'/window/rect',{'width':width,'height':900})
                browser.execute('setLanguage(arguments[0]);migration.open();document.querySelector("[data-migration-mode=tunnel]").click();',language)
                shot(f'tunnel-{language}-{width}.png')
        request(browser.base+'/window/rect',{'width':1280,'height':900})
        # Select only the synthetic external service; the subscription belongs to this fixture too.
        browser.execute('for(const box of document.querySelectorAll("[data-migration-select]")){box.checked=box.closest("label").textContent.includes("Example API");box.dispatchEvent(new Event("input",{bubbles:true}));}')
        browser.execute('document.querySelector("[data-lan-create]").click();')
        code=wait_for(lambda:browser.execute('return document.getElementById("lan_share_code")?.value;'),seconds=90)
        assert len(code)==49
        peer=browser.execute('return fixture.peer;')
        sender=request(browser.base+'/window')[1]['value']
        receiver=request(browser.base+'/window/new',{'type':'tab'})[1]['value']['handle']
        request(browser.base+'/window',{'handle':receiver})
        browser.navigate(peer+'?reviewAction=lan')
        browser.execute('document.querySelector("[data-migration-mode=tunnel]").click();')
        browser.execute('const input=document.getElementById("lan_code");input.value=arguments[0];input.dispatchEvent(new Event("input",{bubbles:true}));document.querySelector("[data-lan-receive]").click();',code)
        request(browser.base+'/window',{'handle':sender})
        wait_for(lambda:browser.execute('return !document.getElementById("lan_receiver_confirm").hidden;'))
        shot('shadow-lan-approval.png')
        request(browser.base+'/window',{'handle':receiver})
        browser.execute('document.querySelector("[data-lan-stop]").click();document.querySelector("[data-lan-receive]").click();')
        request(browser.base+'/window',{'handle':sender})
        browser.execute('document.querySelector("[data-lan-approve]").click();')
        request(browser.base+'/window',{'handle':receiver})
        def preview_ready():
            return browser.execute('return !!document.querySelector("[data-migration-name]") && !document.getElementById("modal_submit").disabled;')
        wait_for(preview_ready,seconds=90)
        assert browser.execute('return state.providers.length===0;')
        browser.execute('document.getElementById("modal_back").click();')
        assert browser.execute('return document.getElementById("lan_code").value;',)==code
        browser.execute('document.querySelector("[data-lan-receive]").click();')
        wait_for(preview_ready)
        shot('shadow-lan-preview.png')
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda:browser.execute('return state.providers.some(provider=>provider.name==="Example API");'))
        browser.navigate(args.url)
        assert browser.execute('return window.EMP_REAL_SHADOW===true;')
    result={'checks':['Three current-review entries on real-data Shadow','Default selection creates a transfer with local-key authentication without exporting the managed key','Separate production UI and backend simulate device login','Sender approval and receiver preview/import work through browser controls','Stop and resume receiving; Back retains received preview without reusing the code','Bilingual tunnel screenshots at desktop and narrow widths','Return to real-data Shadow'],'real_model_calls':0}
    (args.output/'shadow-result.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result))


if __name__=='__main__':main()
