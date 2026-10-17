function createCodexAccountImport({$, tr, esc, api, openModal, closeModal, notice, onSaved, modalReturn}) {
  function parse(text) {
    let auth;
    try { auth = JSON.parse(text); } catch (_) { throw new Error(tr("services.auth_json_contains_invalid_json")); }
    if (!auth || typeof auth !== 'object' || Array.isArray(auth)) throw new Error(tr("services.auth_json_must_contain_a_json_object"));
    return auth;
  }
  function form(auth = null, filename = '', back = modalReturn()) {
    openModal(tr("services.import_codex_subscription"), `<div id="codex_import_form">
      ${filename ? `<p class="muted">${tr("services.selected")}: ${esc(filename)}</p>` : `<label>${tr("services.auth_json_contents")}<textarea id="modal_account_auth" rows="7" spellcheck="false" autocomplete="off" autocapitalize="off" placeholder="${tr("services.paste_the_complete_auth_json_contents")}"></textarea></label>`}
      <label>${tr("services.name")}<input id="modal_account_name" placeholder="${tr("services.for_example_work_account")}" autocomplete="off"></label>
    </div>`, tr("services.import_subscription"), async () => {
      const name = $('modal_account_name').value.trim();
      if (!name) throw new Error(tr("services.enter_a_name_2"));
      const authJson = auth || parse($('modal_account_auth').value);
      await api('/api/accounts/import', {method:'POST', body:JSON.stringify({name, auth_json:authJson})});
      if ($('codex_import_form') === node) closeModal();
      await onSaved();
    }, null, {back});
    const node = $('codex_import_form');
    (auth ? $('modal_account_name') : $('modal_account_auth')).focus();
  }
  function chooseFile() {
    const input = $('a_auth_json'); input.value = ''; input.click();
  }
  async function fileChosen(input) {
    const file = input.files[0];
    if (!file) return;
    const choices = $('codex_import_choices'), back = modalReturn();
    try {
      const auth = parse(await file.text());
      if (choices && $('codex_import_choices') === choices) form(auth, file.name, back);
    } catch (error) { if ($('codex_import_choices') === choices) notice(error.message, true); }
    finally { input.value = ''; }
  }
  return {paste:() => form(), chooseFile, fileChosen};
}
