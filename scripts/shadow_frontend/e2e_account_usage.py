"""Account lifetime-cache navigation and refresh acceptance with delayed HTTP fixtures."""
import argparse
import contextlib
import json
from pathlib import Path
import tempfile

from e2e_environment import start_backend, request, wait_for
from e2e_refinements import Browser


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-account-usage-') as directory, contextlib.ExitStack() as stack:
        root = Path(directory)
        _, url, _, _, upstream = start_backend(stack, args.emp.resolve(), root,
            {'auto_enable_on_start': False}, {'PATH': '/usr/bin:/bin'})
        browser = Browser(stack, root)
        browser.navigate(url)
        browser.execute('''
            managementEvents.stop(); clearInterval(quotaStateTimer); dismissNotice();
            window.fixtureReads=[]; window.fixtureClock=0;
            const now=Date.now; Date.now=()=>now()+fixtureClock;
            state.accounts=['first','second'].map(id=>({id,name:id,credential_set:true,
                hidden_models:[],model_context_windows:{},quota:{account_label:id+'@example.invalid'}}));
            const original=window.fetch;
            window.fetch=(input,options={})=>{
                const path=String(input);
                if(path.startsWith('/api/usage?account_id='))return new Promise(resolve=>{
                    fixtureReads.push({path,signal:options.signal,resolve});
                });
                if(path.startsWith('/api/accounts/')&&path.endsWith('/models'))
                    return Promise.resolve(new Response(JSON.stringify({models:[]})));
                return original(input,options);
            };
            window.releaseUsage=(index,tokens,status=200)=>{
                const row=fixtureReads[index];
                const data=status===200?{groups:Array.isArray(tokens)?tokens:[{category:'codex',owner:'fixture',model:'fixture-model',
                    requests:1,input_reports:1,output_reports:1,input_tokens:tokens,output_tokens:0}],
                    first_record_at:1,periods:[{unused:true}]}:{error:{message:'Fixture read failed'}};
                row.resolve(new Response(JSON.stringify(data),{status,headers:{'Content-Type':'application/json'}}));
            };
            renderServices();
        ''')

        def click(selector):
            element = browser.execute('return document.querySelector(arguments[0]);', selector)
            assert element, selector
            status, result = request(browser.base + '/element/' + element['element-6066-11e4-a52e-4f735466cecf'] + '/click', {})
            assert status == 200, result

        def count(expected):
            wait_for(lambda: browser.execute('return fixtureReads.length;') >= expected)
            assert browser.execute('return fixtureReads.length;') == expected

        def tokens(expected):
            wait_for(lambda: browser.execute('return document.querySelector("#account_usage .usage-total strong")?.textContent;') == str(expected))

        click('[data-ui-action=account-details][data-id=first]')
        count(1)
        click('#service_detail_nav [data-ui-action=account-edit]')
        wait_for(lambda: browser.execute('return !!document.getElementById("subscription_model_list");'))
        click('#service_detail_nav [data-ui-action=account-details]')
        count(1)
        assert not browser.execute('return fixtureReads[0].signal.aborted;')
        browser.execute('releaseUsage(0,123);')
        tokens(123)
        click('#service_detail_nav [data-ui-action=account-edit]')
        click('#service_detail_nav [data-ui-action=account-details]')
        tokens(123)
        click('#modal_close')
        click('[data-ui-action=account-details][data-id=first]')
        tokens(123)
        count(1)
        checks.append('Tab return shares a pending read; completed usage survives model tabs and closing/reopening')

        browser.execute('processManagementEvent("usage-updated", "{}");')
        count(2)
        tokens(123)
        browser.execute('processManagementEvent("usage-updated", "{}");processManagementEvent("usage-updated", "{}");')
        count(2)
        browser.execute('releaseUsage(1,234);')
        count(3)
        browser.execute('releaseUsage(2,345);')
        tokens(345)
        count(3)
        checks.append('New usage updates in the background; events during a request coalesce into one follow-up')

        click('#modal_close')
        browser.execute('processManagementEvent("usage-updated", "{}");')
        count(3)
        click('[data-ui-action=account-details][data-id=first]')
        tokens(345)
        count(4)
        browser.execute('releaseUsage(3,0,503);')
        wait_for(lambda: browser.execute('return !!document.querySelector("#account_usage [role=status]");'))
        tokens(345)
        click('#modal_close')
        click('[data-ui-action=account-details][data-id=first]')
        count(5)
        browser.execute('releaseUsage(4,456);')
        tokens(456)
        checks.append('Hidden accounts defer refresh; a failed refresh retains totals and reopening retries')

        click('#modal_close')
        click('[data-ui-action=account-details][data-id=second]')
        count(6)
        click('#modal_close')
        click('[data-ui-action=account-details][data-id=first]')
        browser.execute('releaseUsage(5,987);')
        tokens(456)
        click('#modal_close')
        click('[data-ui-action=account-details][data-id=second]')
        tokens(987)
        count(6)
        checks.append('Late results stay with their account and can be reused when that account reopens')

        click('#modal_close')
        browser.execute('fixtureClock+=61_000;')
        click('[data-ui-action=account-details][data-id=second]')
        tokens(987)
        count(7)
        browser.execute('releaseUsage(6,876);')
        tokens(876)
        browser.execute('state.accounts[1].quota.account_label="replacement@example.invalid";void accountDetails.sync();')
        count(8)
        assert not browser.execute('return !!document.querySelector("#account_usage .usage-total strong");')
        browser.execute('releaseUsage(7,12);')
        tokens(12)
        checks.append('Expired data remains visible while refreshing; changed account identity discards the old totals')

        browser.execute('void accountDetails.refresh();')
        count(9)
        browser.execute('accountDetails.reset();void accountDetails.sync();')
        count(10)
        assert browser.execute('return fixtureReads[8].signal.aborted;')
        browser.execute('releaseUsage(8,999);releaseUsage(9,34);')
        tokens(34)
        checks.append('Import/sign-in reset rejects a stale in-flight result')

        browser.execute('void accountDetails.refresh();')
        count(11)
        browser.execute('releaseUsage(10,0,401);')
        wait_for(lambda: browser.execute('return !!document.querySelector("#account_usage [role=status]");'))
        assert not browser.execute('return !!document.querySelector("#account_usage .usage-total strong");')
        checks.append('Lost authorization clears the displayed cached totals')
        browser.execute('closeModal();state.accounts=[{id:"summary",name:"Summary",credential_set:true}];renderServices();void openAccountDetails("summary");')
        count(12)
        browser.execute('releaseUsage(11,arguments[0]);', [
            {'model':'same-model','category':'native','service_tier':'default','requests':1,'input_reports':1,'output_reports':1,'input_tokens':10,'output_tokens':2,'priced_requests':1,'cost_nanos':5_000_000_000},
            {'model':'same-model','category':'subscription','service_tier':'fast','requests':1,'input_reports':1,'output_reports':1,'input_tokens':20,'output_tokens':3,'priced_requests':1,'cost_nanos':7_000_000_000},
            {'model':'same-model','requests':1},
            {'model':'free-model','requests':1,'input_reports':1,'output_reports':1,'input_tokens':4,'output_tokens':0,'priced_requests':1,'cost_nanos':0},
            {'model':'<unknown>','requests':1},
        ])
        tokens(39)
        for language in ['en','zh-CN']:
            browser.execute('setLanguage(arguments[0]);closeModal();void openAccountDetails("summary");', language)
            rows = browser.execute('return [...document.querySelectorAll("#account_usage tbody tr")].map(row=>[...row.cells].map((cell,index)=>index===3?cell.textContent.replace(/[^0-9.\u2014-]/g,""):cell.textContent));')
            assert len(rows) == 3, rows
            assert {row[0]:row[1:] for row in rows} == {'same-model':['30','5','12.00'],'free-model':['4','0','0.00'],'<unknown>':['—','—','—']}, rows
            assert browser.execute('return document.querySelector("#account_usage footer").textContent.includes("3 / 5") && document.querySelectorAll("#modal_backdrop [data-ui-action=account-remove]").length===1;')
        count(12)
        checks.append('Shipped EN/ZH summaries merge repeated models across tiers, preserve missing versus zero prices and escaped names, reuse cached totals and expose one removal action')
        browser.execute('closeModal();state.native_account={id:"@native",native:true,auth_mode:"subscription",credential_set:true,quota:{rate_limits:{primary:{used_percent:24,window_minutes:300},secondary:{used_percent:90,window_minutes:10080}}}};quotaDisplay.set("remaining");renderServices();')
        browser.execute('openSettings();document.querySelector("[data-presentation-style=bar]").click();closeModal();')
        meter = '[data-service="account:@native"]'
        assert browser.execute('return document.querySelector(arguments[0]+" [aria-valuenow]").getAttribute("aria-valuenow");', meter) == '76'
        browser.execute('openSettings();document.querySelector("[data-ui-action=quota-display][data-mode=used]").click();closeModal();')
        assert browser.execute('return localStorage.getItem("emp.quotaDisplay");') == 'used'
        assert browser.execute('return document.querySelector(arguments[0]+" [aria-valuenow]").getAttribute("aria-valuenow");', meter) == '24'
        assert browser.execute('return !!document.querySelector(arguments[0]+" .quota-meter.is-low") && state.native_account.quota.rate_limits.primary.used_percent===24;', meter)
        for language, label in [('en','EMP models loaded'),('zh-CN','EMP 模型已加载')]:
            browser.execute('setLanguage(arguments[0]);renderIntegration({configuration:{state:"emp_applied"},runtime:{state:"catalog_loaded"},codex_compatibility:{installed:"0.166.0"}});applyTranslations();', language)
            assert browser.execute('return document.getElementById("integration_badge").textContent;') == label
            assert browser.execute('return document.getElementById("codex_version").textContent;') == 'Codex 0.166.0'
        browser.execute('state.native_account.quota=null;state.native_account.quota_pending=true;renderServices();')
        assert browser.execute('return document.querySelectorAll(arguments[0]+" [aria-valuenow]").length;', meter) == 0
        browser.execute('state.native_account.quota_pending=false;state.native_account.quota={rate_limits:{primary:{usedPercent:60,windowDurationMins:10080,resetsAt:2000000000}}};renderServices();')
        assert browser.execute('return [...document.querySelectorAll(arguments[0]+" [aria-valuenow]")].map(node=>node.getAttribute("aria-valuenow"));', meter) == ['60']
        assert browser.execute('return document.querySelectorAll(arguments[0]+" [data-reset-countdown]").length;', meter) == 1
        checks.append('Display settings change visible quota without mutating usage; pending and weekly-only quotas stay distinct, and EN/ZH refresh retains the integration status')
        errors = [line for line in browser.diagnostics().splitlines() if 'chrome://browser/' not in line]
        assert not errors, errors
        assert upstream.calls == 0
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {'checks':checks,'real_model_calls':0,'usage_reads':12}
    args.output.write_text(json.dumps(report, indent=2))
    print(json.dumps(report))


if __name__ == '__main__':
    main()
