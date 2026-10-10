"""Preview asset rendering, separate from HTTP permissions and backend reads."""
import json
import re
from pathlib import Path
HERE = Path(__file__).resolve().parent
WEB = HERE.parents[1] / 'crates/emp-app/web'

def frontend(name, bootstrap):
    if name == 'index.html':
        text = (WEB / name).read_text()
        text = text.replace('<title>EMP</title>', '<title>EMP · 影子预览</title>')
        text = text.replace('</head>', '<link rel="stylesheet" href="/assets/shadow.css"></head>')
        text = text.replace('<script src="/assets/call-reports.js"></script>', '<script src="/assets/shadow-performance-chart.js"></script><script src="/assets/shadow-performance.js"></script><script src="/assets/call-reports.js"></script>')
        text = text.replace('<script src="/assets/service-icons.js"></script>', '<script src="/assets/service-icons.js"></script><script src="/assets/shadow-brand-colors.js"></script>')
        text = text.replace('</head>', '<script src="/assets/shadow-drafts.js"></script><script src="/assets/shadow-service-errors.js"></script><script src="/assets/shadow-interactions.js"></script><script src="/assets/shadow-motion.js"></script><script src="/assets/shadow-service-demo.js"></script><script src="/assets/shadow-request-errors.js"></script></head>')
        text = text.replace('</head>', '<script src="/assets/shadow-usage-data.js"></script><script src="/assets/shadow-stats.js"></script></head>')
        text = text.replace('<body>', '<body><script>window.EMP_SHADOW_BOOTSTRAP=' + json.dumps(bootstrap) + ';</script>')
        text = text.replace('const {open:openSettings, toggle:toggleSetting}', 'let {open:openSettings, toggle:toggleSetting}')
        text = text.replace('quotaMeters:quotaMetersHtml', 'quotaMeters:shadowQuotaMeters')
        text = text.replace("tr('账户余量已刷新','Account quota refreshed')", "tr('已读取当前额度','Current quota loaded')")
        text = text.replace("tr('刷新额度','Refresh quota')", "tr('读取额度','Read quota')")
        text = text.replace('onclick="openDiagnostics()" data-i18n="diagnostics">性能', 'onclick="shadowStats.open()" data-i18n="statistics">统计')
        text = re.sub(r'<button[^>]+onclick="openUsage\(\)"[^>]*>[^<]*</button>', '', text, count=1)
        text = text.replace('function stopModalFeatures() {', 'function stopModalFeatures() {\n  shadowStats.stop();')
        text = text.replace('establishSession().then(load)', (HERE / 'shadow-ui.js').read_text() + '\nestablishSession().then(load)')
        return text.encode()
    if name in {'shadow-drafts.js', 'shadow-service-errors.js', 'shadow.css', 'shadow-performance-chart.js', 'shadow-performance.js', 'shadow-brand-colors.js', 'shadow-interactions.js', 'shadow-motion.js', 'shadow-service-demo.js', 'shadow-request-errors.js', 'shadow-usage-data.js', 'shadow-stats.js'}:
        return (HERE / name).read_bytes()
    text = (WEB / name).read_bytes()
    if name == 'style.css':
        def hover_rule(match):
            selectors, declarations = match.groups()
            if ':hover' not in selectors:
                return match.group(0)
            return selectors+'{'+re.sub(r'(?:^|;)(?:background(?:-color)?|color|border-color):[^;]+', '', declarations)+'}'
        return re.sub(r'([^{}]+)\{([^{}]*)\}', hover_rule, text.decode()).encode()
    if name == 'call-reports.js':
        text = text.decode()
        start = text.index('  function trendHtml(report) {')
        end = text.index('  function modelHtml(report) {', start)
        text = text[:start] + '''  const shadowPerformance = createShadowPerformanceView({esc,tr,getLanguage,number,seconds,tps,rate,outcomes,bindOutcomes,definition});
  function summaryHtml(report) { return shadowPerformance.html(report,current?.metric || 'calls'); }
''' + text[end:]
        text = text.replace('    target.innerHTML = ', '    shadowPerformance.stop();\n    target.innerHTML = ', 1)
        text = text.replace("    disposeOutcomes = bindOutcomes(target,report.summary || {},report);", "    disposeOutcomes = () => {};")
        anchor = "    target.querySelectorAll('details[data-call-id]').forEach"
        text = text.replace(anchor, "    if (current.view === 'overview') shadowPerformance.bind(target,report,current.metric || 'calls');\n" + anchor, 1)
        text = text.replace('function stop() { disposeOutcomes();', 'function stop() { shadowPerformance.stop(); disposeOutcomes();', 1).encode()
    if name == 'management-client.js':
        text = text.decode().replace('async function establishSession() {', '''async function establishSession() {
    if (window.EMP_SHADOW_BOOTSTRAP) {
      const response = await fetch('/api/session', {method:'POST', headers:{'X-EMP-Bootstrap':window.EMP_SHADOW_BOOTSTRAP}});
      if (!response.ok) throw new Error('影子页面登录失败');
      saveSessionToken((await response.json()).session);
      delete window.EMP_SHADOW_BOOTSTRAP;
      return;
    }''').encode()
    if name == 'service-list.js':
        text = text.decode().replace("${activity('account', account.native ? '@native' : account.id, label)}${icon('codex')}", "${shadowServiceIcon('account', account.native ? '@native' : account.id, label, 'codex', false, refreshErrors[account.id] ? {title:tr('额度刷新失败','Quota refresh failed'), message:refreshErrors[account.id]} : status ? {title:tr('登录需要处理','Sign-in needs attention'), message:status} : null)}")
        text = text.replace("${activity('provider', provider.id, label)}${icon(brand, cpa)}", "${shadowServiceIcon('provider', provider.id, label, brand, cpa, errors.has(provider.id) ? {title:tr('额度刷新失败','Quota refresh failed'),message:errors.get(provider.id)} : null)}")
        text = text.replace("${refreshErrors[account.id] ? `<div class=\"refresh-error\">${esc(refreshErrors[account.id])}</div>` : ''}", '')
        text = text.replace("${errors.has(provider.id) ? `<div class=\"refresh-error\">${esc(errors.get(provider.id))}</div>` : ''}", '')
        text = text.replace('const detail = [status, duplicateText]', 'const detail = [duplicateText]')
        text = text.replace('    updateActivityDots();', '    updateActivityDots(); shadowUpdateServiceRequestErrors(); shadowUpdateServiceErrorLabels();').encode()
    if name == 'model-settings.js':
        text = text.decode().replace("tr('更新模型列表','Update model list')", "tr('读取已保存列表','Read saved list')").encode()
    return text
