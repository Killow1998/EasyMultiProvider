// Route preferences share one dialog; changes apply only after Save succeeds.
function createRoutingSettings({getState, cloneState, persistState, openModal, closeModal, $, esc, tr}) {
  function accounts(state) {
    return [...(state.native_account ? [{...state.native_account, id:'@native', name:'Native'}] : []), ...(state.accounts || [])]
      .filter(account => account.enabled !== false && account.credential_set && account.credential_status !== 'invalid' && !account.duplicate && account.quota?.plan_type?.toLowerCase() !== 'free');
  }
  function open({back} = {}, draft) {
    const state = getState(), eligible = accounts(state);
    draft ||= {review:state.auto_review_fallback !== false, mode:state.image_routing?.mode || 'follow', account:state.image_routing?.account_id || ''};
    const selected = draft.account || eligible[0]?.id || '';
    const options = eligible.map(account => `<option value="${esc(account.id)}" ${account.id === selected ? 'selected' : ''}>${esc(account.name || account.id)}</option>`).join('');
    const missing = selected && !eligible.some(account => account.id === selected);
    const title = tr('路由设置','Routing settings');
    openModal(title, `<div class="routing-settings">
      <div class="setting-row"><div><div class="setting-title" id="routing_review_label">${tr('自动审查','Auto Review')}</div><p class="setting-help" id="routing_review_help">${tr('Native 无额度时，使用其他订阅账号。','Use another subscription when Native runs out of quota.')}</p></div><button type="button" class="setting-switch" id="routing_review" role="switch" aria-checked="${draft.review}" aria-labelledby="routing_review_label" aria-describedby="routing_review_help"></button></div>
      <div class="setting-row routing-mode"><label class="setting-title" for="routing_image_mode">${tr('图片生成与编辑','Image generation and editing')}</label><select id="routing_image_mode">${[['follow',tr('跟随对话','Follow conversation')],['account',tr('指定账号','Specific account')],['auto',tr('自动选择','Automatic')]].map(([mode,label]) => `<option value="${mode}" ${mode === draft.mode ? 'selected' : ''}>${label}</option>`).join('')}</select></div>
      <div class="setting-row routing-account" id="routing_image_account_row" ${draft.mode === 'account' ? '' : 'hidden'}><label class="setting-title" for="routing_image_account">${tr('图片账号','Image account')}</label><select id="routing_image_account"><option value="" ${selected ? '' : 'selected'}>${tr('选择账号','Choose an account')}</option>${missing ? `<option value="${esc(selected)}" selected disabled>${esc(selected)} · ${tr('不可用','Unavailable')}</option>` : ''}${options}</select></div>
    </div>`, tr('保存','Save'), async () => {
      const candidate = cloneState(), form = $('routing_image_mode').closest('.routing-settings'), controls = [...form.querySelectorAll('select,button')];
      candidate.auto_review_fallback = $('routing_review').getAttribute('aria-checked') === 'true';
      const mode = $('routing_image_mode').value, account = $('routing_image_account').value;
      if (mode === 'account' && !accounts(getState()).some(item => item.id === account)) throw new Error(tr('请选择可用的订阅账号','Choose an available subscription account'));
      candidate.image_routing = {mode, account_id:mode === 'account' ? account : ''};
      controls.forEach(control => control.disabled = true);
      try {
        await persistState(tr('路由设置已保存','Routing settings saved'), candidate);
        if (form.isConnected) (back || closeModal)();
      } finally { controls.forEach(control => control.disabled = false); }
    }, null, {back, reopen:() => open({back}, read())});
    function read() { return {review:$('routing_review').getAttribute('aria-checked') === 'true', mode:$('routing_image_mode').value, account:$('routing_image_account').value}; }
    $('routing_review').onclick = () => $('routing_review').setAttribute('aria-checked', String($('routing_review').getAttribute('aria-checked') !== 'true'));
    $('routing_image_mode').onchange = () => {
      $('routing_image_account_row').hidden = $('routing_image_mode').value !== 'account';
    };
  }
  return {open};
}
