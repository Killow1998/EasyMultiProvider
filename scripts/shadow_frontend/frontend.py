"""Preview isolation around the same assets shipped by EMP."""
import json
from pathlib import Path
HERE = Path(__file__).resolve().parent
WEB = HERE.parents[1] / 'crates/emp-app/web'
PREVIEW_ASSETS = {'shadow-ui.js', 'shadow-drafts.js', 'shadow-service-demo.js'}

def frontend(name, bootstrap):
    if name in PREVIEW_ASSETS:
        return (HERE / name).read_bytes()
    if name.endswith(('.png', '.wav')):
        return (WEB / name).read_bytes()
    text = (WEB / name).read_text(encoding='utf-8')
    if name == 'index.html':
        text = text.replace('<title>EMP</title>', '<title>EMP · 影子预览</title>')
        text = text.replace('</head>', '<script src="/assets/shadow-drafts.js"></script><script src="/assets/shadow-service-demo.js"></script></head>')
        text = text.replace('<body>', '<body><script>window.EMP_PREVIEW=true;window.EMP_SHADOW_BOOTSTRAP=' + json.dumps(bootstrap) + ';</script>')
        text = text.replace('establishSession().then(load)', (HERE / 'shadow-ui.js').read_text(encoding='utf-8') + '\nestablishSession().then(load)')
        text = text.replace("tr('账户余量已刷新','Account quota refreshed')", "tr('已读取当前额度','Current quota loaded')")
        text = text.replace("tr('刷新额度','Refresh quota')", "tr('读取额度','Read quota')")
    if name == 'management-client.js':
        text = text.replace('async function establishSession() {', '''async function establishSession() {
    if (window.EMP_SHADOW_BOOTSTRAP) {
      const response = await fetch('/api/session', {method:'POST', headers:{'X-EMP-Bootstrap':window.EMP_SHADOW_BOOTSTRAP}});
      if (!response.ok) throw new Error('影子页面登录失败');
      saveSessionToken((await response.json()).session);
      delete window.EMP_SHADOW_BOOTSTRAP;
      return;
    }''')
    if name == 'statistics.js':
        text = text.replace('/api/usage?series=true&', '/api/shadow/usage?')
    if name == 'model-settings.js':
        text = text.replace("tr('更新模型列表','Update model list')", "tr('读取已保存列表','Read saved list')")
    return text.encode('utf-8')
