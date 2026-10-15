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
    const codex = card('service-native', tr('当前 Codex 登录','Current Codex sign-in'), 'codex', native?.credential_set ? tr('使用本机已登录的账号（Native）','Use the account already signed in on this computer (Native)') : tr('先在本机 Codex 登录，再连接该账号','Sign in to Codex on this computer, then connect that account'))
      + card('service-import-account', tr('导入其他订阅','Import another subscription'), 'codex', tr('用已有 auth.json 添加另一个 Codex 账号','Add another Codex account using its existing auth.json'));
    const claude = card('claude-local-choose', tr('本机 Claude 登录','Local Claude sign-in'), 'claudecode', tr('使用本机 Claude Code 已登录的订阅','Use the subscription signed in through local Claude Code'), `data-id="${esc(existingClaude?.id || '')}"`);
    const cpa = card('claude-cpa-choose', 'Claude Code CPA', 'claudecode', tr('通过 Claude Code 连接外部 API；需要服务地址和 API Key','Connect an external API through Claude Code; requires its address and API key'), '', true);
    const apiKeys = ['openai', 'anthropic', ...Object.keys(presets).filter(key => !['chatgpt','openai','anthropic'].includes(key))];
    const api = apiKeys.filter(key => presets[key]).map(key => card('official-provider-choose', esc(presets[key].name), key, tr('使用该服务的 API Key，按服务商规则计费','Use this service’s API key and billing'), `data-provider="${esc(key)}"`)).join('')
      + card('service-custom-api', tr('自定义 API','Custom API'), 'custom', tr('已有其他服务商的地址和 API Key','Use another provider’s address and API key')) + cpa;
    openModal(tr('添加服务','Add service'), group(tr('Codex 订阅','Codex subscriptions'), codex) + group(tr('Claude Code 订阅','Claude Code subscription'), claude) + group(tr('外部 API 服务','External API service'), api), '', null, null, {reopen:open});
  }
  function importCodex(navigation = {}) {
    openModal(tr('导入 Codex 订阅','Import Codex subscription'), `<div id="codex_import_choices" class="choice-grid">`
      + card('account-import-paste', tr('粘贴内容','Paste contents'), 'codex', tr('粘贴 auth.json 的完整内容','Paste the complete auth.json contents'))
      + card('account-import-file', tr('选择文件','Choose a file'), 'codex', tr('选择 auth.json 文件','Choose an auth.json file')) + '</div>', '', null, null, {...navigation, reopen:() => importCodex(navigation)});
  }
  return {open, importCodex};
}
