"""Real-browser notification timing, persistence and independent inbox navigation."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile
import time
from e2e_refinements import Browser
from local_backend import request, start_backend, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-notifications-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        _, url, _, _ = start_backend(stack,args.emp.resolve(),root,{'auto_enable_on_start':False,
            'providers':[{'id':'example','name':'Example API','base_url':'http://127.0.0.1:9/v1','protocol':'responses','auth_mode':'api_key','api_key':'fixture-only'}]})
        browser = Browser(stack,root,reduced_motion=0)
        browser.navigate(url)
        def reload_config():
            browser.execute('window.loadCheckDone=false;load().finally(()=>window.loadCheckDone=true);')
            wait_for(lambda:browser.execute('return window.loadCheckDone;'))
        reload_config()
        assert browser.execute('return document.getElementById("notification").hidden && JSON.parse(localStorage.getItem("emp.notifications.v1")||"[]").length===0;')
        browser.navigate(url.split('/?')[0])
        reload_config()
        assert browser.execute('return document.getElementById("notification").hidden && JSON.parse(localStorage.getItem("emp.notifications.v1")||"[]").length===0;')
        browser.execute('window.savedLoadFetch=window.fetch;window.fetch=(path,options)=>String(path)==="/api/config"&&(!options?.method||options.method==="GET")?Promise.resolve(new Response(JSON.stringify({error:{message:"Fixture config unavailable"}}),{status:503})):window.savedLoadFetch(path,options);')
        reload_config()
        assert browser.execute('return !document.getElementById("notification").hidden && document.getElementById("status").getAttribute("role")==="alert" && document.getElementById("status").textContent.includes("Fixture config unavailable");')
        browser.execute('window.fetch=window.savedLoadFetch;')
        reload_config()
        assert browser.execute('return JSON.parse(localStorage.getItem("emp.notifications.v1")).length===1;')
        checks.append('Initial open, page reload and successful configuration refresh are silent; loading failures still display an alert and persist in the inbox')
        browser.execute('document.getElementById("notification_mailbox").click();document.getElementById("notification_clear").click();document.querySelector("#notification_inbox [popovertargetaction=hide]").click();')
        browser.execute('notice("First result");')
        time.sleep(1)
        browser.execute('notice("Latest result");')
        time.sleep(4.2)
        assert browser.execute('return !document.getElementById("notification").hidden && document.getElementById("status").textContent==="Latest result";')
        wait_for(lambda:browser.execute('return document.getElementById("notification").hidden;'),seconds=3)
        browser.execute('document.getElementById("notification_mailbox").click();')
        assert browser.execute('return [...document.querySelectorAll("#notification_history p")].map(node=>node.textContent);') == ['Latest result','First result']
        assert browser.execute('return document.getElementById("notification_unread").hidden;')
        browser.escape()
        checks.append('Only the latest banner is shown; countdown dismissal keeps all messages in the inbox')

        browser.execute('notice("Pause while reading");')
        browser.hover('#notification')
        time.sleep(5.2)
        assert browser.execute('return !document.getElementById("notification").hidden;')
        browser.hover('#notification_mailbox')
        wait_for(lambda:browser.execute('return document.getElementById("notification").hidden;'),seconds=6)
        browser.execute('notice("Pause for keyboard focus");document.getElementById("notification_close").focus();')
        time.sleep(5.2)
        assert browser.execute('return !document.getElementById("notification").hidden;')
        browser.execute('document.getElementById("notification_mailbox").focus();')
        wait_for(lambda:browser.execute('return document.getElementById("notification").hidden;'),seconds=6)
        checks.append('Pointer hover and keyboard focus pause dismissal, then resume the remaining time')

        browser.execute('notice("Pause in a background tab");')
        _, current = request(browser.base+'/window')
        _, other = request(browser.base+'/window/new',{'type':'tab'})
        request(browser.base+'/window',{'handle':other['value']['handle']})
        time.sleep(5.2)
        request(browser.base+'/window',{'handle':current['value']})
        assert browser.execute('return !document.getElementById("notification").hidden;')
        request(browser.base+'/window',{'handle':other['value']['handle']})
        request(browser.base+'/window',method='DELETE')
        request(browser.base+'/window',{'handle':current['value']})
        wait_for(lambda:browser.execute('return document.getElementById("notification").hidden;'),seconds=6)
        checks.append('A background tab pauses the countdown until the user returns')

        browser.execute('notice("Retained error <img src=x onerror=window.inboxInjection=true>",true);')
        time.sleep(5.2)
        assert browser.execute('return !document.getElementById("notification").hidden;')
        wait_for(lambda:browser.execute('return document.getElementById("notification").hidden;'),seconds=6)
        browser.navigate(url.split('/?')[0])
        browser.execute('document.getElementById("notification_mailbox").click();')
        assert browser.execute('return document.getElementById("notification_history").textContent.includes("Retained error <img") && !document.querySelector("#notification_history img") && !window.inboxInjection;')
        browser.escape()
        checks.append('Errors allow a longer reading time, persist after reload, and render as plain text')

        browser.execute('migration.open();document.querySelector("[data-migration-mode=import]").click();document.getElementById("shadow_import_password").value="unfinished-draft";notice("Import message");document.getElementById("notification_mailbox").click();')
        browser.escape()
        assert browser.execute('return !document.getElementById("modal_backdrop").classList.contains("hidden") && document.getElementById("shadow_import_password").value==="unfinished-draft" && !document.getElementById("notification_inbox").matches(":popover-open");')
        browser.execute('document.getElementById("notification_mailbox").click();document.querySelector("#notification_inbox [popovertargetaction=hide]").click();')
        assert browser.execute('return document.getElementById("shadow_import_password").value==="unfinished-draft";')
        browser.execute('closeModal();')
        checks.append('Inbox Escape and Close preserve the underlying migration form and draft')

        browser.execute('for(let i=0;i<55;i++)notice("bounded-message-"+i);document.getElementById("notification_mailbox").click();')
        assert browser.execute('return document.querySelectorAll("#notification_history li").length===50 && document.querySelector("#notification_history p").textContent==="bounded-message-54" && ![...document.querySelectorAll("#notification_history p")].some(p=>p.textContent==="bounded-message-0");')
        browser.execute('document.getElementById("notification_clear").click();')
        assert browser.execute('return JSON.parse(localStorage.getItem("emp.notifications.v1")).length===0 && !document.getElementById("notification_empty").hidden;')
        browser.escape()
        checks.append('History is bounded to 50 records and can be cleared explicitly')

        browser.execute('window.savedStorageGet=Storage.prototype.getItem;window.savedStorageSet=Storage.prototype.setItem;Storage.prototype.getItem=function(key){if(key==="emp.notifications.v1")throw new Error("blocked fixture");return window.savedStorageGet.call(this,key);};Storage.prototype.setItem=function(key,value){if(key==="emp.notifications.v1")throw new Error("blocked fixture");return window.savedStorageSet.call(this,key,value);};notice("Memory-only result");document.getElementById("notification_mailbox").click();')
        assert browser.execute('return document.getElementById("notification_history").textContent.includes("Memory-only result");')
        browser.execute('Storage.prototype.getItem=window.savedStorageGet;Storage.prototype.setItem=window.savedStorageSet;')
        browser.escape()
        checks.append('Blocked browser storage still allows notifications and in-memory history')

        for language,theme,width,height in [('zh-CN','light',1280,900),('en','dark',1280,900),('zh-CN','light',560,800)]:
            request(browser.base+'/window/rect',{'width':width,'height':height})
            browser.execute('setLanguage(arguments[0]);setTheme(arguments[1]);notice(arguments[0]==="en"?"Connection failed. Check the service address and retry.":"连接失败，请检查服务地址后重试。",true);notice(arguments[0]==="en"?"Model settings saved.":"模型设置已保存。");',language,theme)
            time.sleep(.6)
            screenshot(browser,args.output/f'island-{language}-{width}.png')
            browser.execute('document.getElementById("notification_mailbox").click();')
            time.sleep(.2)
            screenshot(browser,args.output/f'inbox-{language}-{width}.png')
            browser.escape()
        page_errors = [line for line in browser.diagnostics().splitlines() if 'chrome://browser/' not in line]
        assert not page_errors,page_errors
    (args.output/'result.json').write_text(json.dumps({'checks':checks},indent=2))
    print(json.dumps({'checks':checks,'output':str(args.output)}))


def screenshot(browser,path):
    status,shot=request(browser.base+'/screenshot')
    assert status==200
    path.write_bytes(base64.b64decode(shot['value']))


if __name__=='__main__':
    main()
