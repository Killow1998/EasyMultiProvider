function createCodexAccountImport({$, tr, esc, api, openModal, closeModal, notice, onSaved}) {
  function parse(text) {
    let auth;
    try { auth = JSON.parse(text); } catch (_) { throw new Error(tr('auth.json 内容不是有效 JSON','auth.json contains invalid JSON')); }
    if (!auth || typeof auth !== 'object' || Array.isArray(auth)) throw new Error(tr('auth.json 内容需要是 JSON 对象','auth.json must contain a JSON object'));
    return auth;
  }
  function form(auth = null, filename = '') {
    openModal(tr('导入 Codex 订阅','Import Codex subscription'), `<div id="codex_import_form">
      ${filename ? `<p class="muted">${tr('已选择','Selected')}: ${esc(filename)}</p>` : `<label>${tr('auth.json 内容','auth.json contents')}<textarea id="modal_account_auth" rows="7" spellcheck="false" autocomplete="off" autocapitalize="off" placeholder="${tr('粘贴 auth.json 的完整内容','Paste the complete auth.json contents')}"></textarea></label>`}
      <label>${tr('账户 ID','Account ID')}<input id="modal_account_id" placeholder="primary" autocomplete="off"></label>
    </div>`, tr('导入订阅','Import subscription'), async () => {
      const id = $('modal_account_id').value.trim();
      if (!id) throw new Error(tr('请填写账户 ID','Enter an account ID'));
      const authJson = auth || parse($('modal_account_auth').value);
      await api('/api/accounts/import', {method:'POST', body:JSON.stringify({id, name:id, prefix:id, auth_json:authJson})});
      if ($('codex_import_form') === node) closeModal();
      await onSaved();
    });
    const node = $('codex_import_form');
    (auth ? $('modal_account_id') : $('modal_account_auth')).focus();
  }
  function chooseFile() {
    const input = $('a_auth_json'); input.value = ''; input.click();
  }
  async function fileChosen(input) {
    const file = input.files[0];
    if (!file) return;
    const choices = $('codex_import_choices');
    try {
      const auth = parse(await file.text());
      if (choices && $('codex_import_choices') === choices) form(auth, file.name);
    } catch (error) { if ($('codex_import_choices') === choices) notice(error.message, true); }
    finally { input.value = ''; }
  }
  return {paste:() => form(), chooseFile, fileChosen};
}
