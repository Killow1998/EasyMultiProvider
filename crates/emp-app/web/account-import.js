function createCodexAccountImport({$, tr, api, openModal, closeModal, notice, onSaved, modalReturn}) {
  function parse(text) {
    let auth;
    try { auth = JSON.parse(text); } catch (_) { throw new Error(tr("services.auth_json_contains_invalid_json")); }
    if (!auth || typeof auth !== 'object' || Array.isArray(auth)) throw new Error(tr("services.auth_json_must_contain_a_json_object"));
    return auth;
  }
  function form(back = modalReturn()) {
    openModal(tr("services.import_codex_subscription"), `<div id="codex_import_form">
      <div class="toolbar"><label for="modal_account_auth">${tr("services.auth_json_contents")}</label><button type="button" class="secondary" data-icon="upload" data-ui-action="account-import-file">${tr("services.choose_a_file")}</button></div>
      <p id="account_import_filename" class="muted" hidden></p>
      <textarea id="modal_account_auth" rows="7" spellcheck="false" autocomplete="off" autocapitalize="off" placeholder="${tr("services.auth_import_help")}"></textarea>
      <label>${tr("services.name")}<input id="modal_account_name" placeholder="${tr("services.for_example_work_account")}" autocomplete="off"></label>
    </div>`, tr("services.import_subscription"), async () => {
      const name = $('modal_account_name').value.trim();
      if (!name) throw new Error(tr("services.enter_a_name_2"));
      const authJson = parse($('modal_account_auth').value);
      await api('/api/accounts/import', {method:'POST', body:JSON.stringify({name, auth_json:authJson})});
      if ($('codex_import_form') === node) closeModal();
      await onSaved();
    }, null, {back});
    const node = $('codex_import_form');
    $('modal_account_auth').focus();
  }
  function chooseFile() {
    const input = $('a_auth_json'); input.value = ''; input.click();
  }
  async function fileChosen(input) {
    const file = input.files[0];
    if (!file) return;
    const owner = $('codex_import_form');
    try {
      const auth = parse(await file.text());
      if (owner && $('codex_import_form') === owner) {
        $('modal_account_auth').value=JSON.stringify(auth,null,2);
        $('account_import_filename').textContent=file.name;$('account_import_filename').hidden=false;
        $('modal_account_name').focus();
      }
    } catch (error) { if ($('codex_import_form') === owner) notice(error.message, true); }
    finally { input.value = ''; }
  }
  return {open:() => form(), chooseFile, fileChosen};
}
