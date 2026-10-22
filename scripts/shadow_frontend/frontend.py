"""Preview isolation around the same assets shipped by EMP."""
import json
from pathlib import Path
HERE = Path(__file__).resolve().parent
WEB = HERE.parents[1] / 'crates/emp-app/web'
PREVIEW_ASSETS = {'shadow-ui.js', 'shadow-drafts.js', 'shadow-service-demo.js'}

def frontend(name, bootstrap, real_context=None):
    if name in PREVIEW_ASSETS:
        return (HERE / name).read_bytes()
    if name.endswith(('.png', '.wav')):
        return (WEB / name).read_bytes()
    text = (WEB / name).read_text(encoding='utf-8')
    if name == 'index.html':
        text = text.replace('<title>EMP</title>', '<title>EMP · 影子预览</title>')
        if real_context:
            import base64
            setup = 'window.EMP_REAL_SHADOW=true;window.EMP_SHADOW_DRAFT_KEY=' + json.dumps(real_context['draft_key']) + ';let shadowServicesDemoActive=false;let shadowServiceDemoDraft=null;'
            text = text.replace('</head>', '<script>' + setup + '</script><script src="/assets/shadow-drafts.js"></script></head>')
            icon = 'data:image/png;base64,' + base64.b64encode((WEB.parents[2] / 'assets/branding/native/easy-multi-provider-tray.png').read_bytes()).decode()
            script = (HERE / 'shadow-real.js').read_text() + '\n' + (HERE / 'preview-review.js').read_text().replace('__TRAY_TEMPLATE_ICON__',icon)
        else:
            text = text.replace('</head>', '<script src="/assets/shadow-drafts.js"></script><script src="/assets/shadow-service-demo.js"></script></head>')
            script = (HERE / 'shadow-ui.js').read_text(encoding='utf-8')
        text = text.replace('<body>', '<body><script>window.EMP_PREVIEW=true;window.EMP_SHADOW_BOOTSTRAP=' + json.dumps(bootstrap) + ';</script>')
        text = text.replace('establishSession().then(load)', script + '\nestablishSession().then(load)')
        text = text.replace('tr("core.account_quota_refreshed")', "tr('已读取当前额度','Current quota loaded')")
        text = text.replace('tr("refresh_quotas")', "tr('读取额度','Read quota')")
    if name == 'management-client.js':
        text = text.replace('async function establishSession() {', '''async function establishSession() {
    if (window.EMP_SHADOW_BOOTSTRAP) {
      const response = await fetch('/api/session', {method:'POST', headers:{'X-EMP-Bootstrap':window.EMP_SHADOW_BOOTSTRAP}});
      if (!response.ok) throw new Error('影子页面登录失败');
      saveSessionToken((await response.json()).session);
      delete window.EMP_SHADOW_BOOTSTRAP;
      return;
    }''')
    if name == 'service-list.js':
        text = text.replace('tr("services.model_settings")', "tr('模型管理','Manage models')")
    return text.encode('utf-8')
