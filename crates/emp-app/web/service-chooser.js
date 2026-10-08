function createServiceChooser({tr, esc, presets, icon, openModal, getState}) {
  function card(action, name, brand, description, attributes = '', cpa = false) {
    return `<button type="button" class="choice service-choice" data-ui-action="${action}" ${attributes}>${icon(brand, cpa)}<span><strong>${name}</strong><small>${description}</small></span></button>`;
  }
  function group(name, cards) {
    return `<div class="service-choice-group"><h3>${name}</h3><div class="choice-grid">${cards}</div></div>`;
  }
  function open() {
    const native = getState().native_account;
    const existingClaude = (getState().providers || []).find(p => p.execution_backend === 'claude_cli' && p.auth_mode === 'claude_login');
    const codex = card('service-native', tr('当前登录','Current sign-in'), 'codex', native?.credential_set ? tr('查看 Native 账号','View Native account') : tr('连接本机 Codex','Connect local Codex'))
      + card('service-import-account', tr('导入订阅','Import subscription'), 'codex', tr('添加 Codex 订阅账号','Add a Codex subscription'));
    const claude = card('claude-local-choose', tr('本机订阅','Local subscription'), 'claudecode', tr('连接本机 Claude Code','Connect local Claude Code'), `data-id="${esc(existingClaude?.id || '')}"`)
      + card('claude-cpa-choose', 'CPA', 'claudecode', tr('填写地址和 API Key','Enter address and API key'), '', true);
    const apiKeys = ['openai', 'anthropic', ...Object.keys(presets).filter(key => !['chatgpt','openai','anthropic'].includes(key))];
    const api = apiKeys.filter(key => presets[key]).map(key => card('official-provider-choose', esc(presets[key].name), key, tr('连接 API 服务','Connect an API service'), `data-provider="${esc(key)}"`)).join('')
      + card('service-custom-api', tr('自定义 API','Custom API'), 'custom', tr('填写地址和 API Key','Enter address and API key'));
    openModal(tr('添加服务','Add service'), group(tr('Codex 订阅','Codex subscriptions'), codex) + group('Claude Code', claude) + group('API', api), '', null);
  }
  function importCodex() {
    openModal(tr('导入 Codex 订阅','Import Codex subscription'), `<div id="codex_import_choices" class="choice-grid">`
      + card('account-import-paste', tr('粘贴内容','Paste contents'), 'codex', tr('粘贴 auth.json 的完整内容','Paste the complete auth.json contents'))
      + card('account-import-file', tr('选择文件','Choose a file'), 'codex', tr('选择 auth.json 文件','Choose an auth.json file')) + '</div>', '', null);
  }
  return {open, importCodex};
}
