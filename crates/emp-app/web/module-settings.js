function createModuleSettings({api, openModal, notice, $, esc, tr, icon, getLanguage, onChange, openMountedPanel, panelContext, onTeamRemoved}) {
  let rows = null, migrationAllowed = false;
  let offers = new Map(), updateCheck, checkedAt = 0;
  const panels = new Map();
  const rowFor = id => rows?.find(row => row.id === id);
  const moduleText = (id, suffix = '') => tr(rowFor(id).ui.copy_key + suffix);
  const active = id => rowFor(id)?.active === true;
  const offerFor = id => {
    const offer = offers.get(id);
    return offer && offer.current_version === rowFor(id)?.installed_version ? offer : null;
  };

  function renderUpdateHints() {
    const available = rows?.some(row => offerFor(row.id));
    const entry = $('modules_open');
    entry.classList.toggle('has-update', Boolean(available));
    entry.title = entry.ariaLabel = tr(available ? 'modules.updates_available' : 'modules.modules');
    for (const button of document.querySelectorAll('[data-module-action="check_updates"]')) {
      const offer = offerFor(button.dataset.moduleId);
      button.classList.toggle('has-update', Boolean(offer));
      button.title = button.ariaLabel = offer
        ? tr('modules.update_available', {version:offer.latest_version}) : tr('modules.check_updates');
    }
  }

  function checkUpdates(force = false) {
    if (updateCheck) return updateCheck;
    if (!force && Date.now() - checkedAt < 6 * 60 * 60 * 1000) return;
    checkedAt = Date.now();
    updateCheck = api('/api/modules', {method:'POST', body:JSON.stringify({action:'check_updates'})})
      .then(data => {
        offers = new Map(data.updates.map(offer => [offer.id, offer]));
        renderUpdateHints();
      }).finally(() => {updateCheck = null;});
    return updateCheck;
  }

  async function checkHint() { await checkUpdates()?.catch(() => {}); }

  async function panelInstance(row) {
    await loadPanel(row.id);
    if (row.active_revision !== rowFor(row.id)?.active_revision || !active(row.id)) {
      throw new Error(tr('modules.module_unavailable'));
    }
    return window[row.ui.factory](panelContext());
  }

  async function openOptions(id, back) {
    const row = rowFor(id);
    if (!row?.active) throw new Error(tr('modules.module_unavailable'));
    if (!row.ui.factory) return openMountedPanel(back);
    return (await panelInstance(row)).open(back);
  }

  function editModel(id, model) {
    const row = rowFor(id);
    if (!row?.active || !row.ui.model_editor) return false;
    panelInstance(row).then(panel => panel.editProfile(model)).catch(error => notice(error.message, true));
    return true;
  }

  function renderEntries() {
    document.querySelectorAll('[data-module-slot]').forEach(slot => {
      const entries = (rows || []).filter(row => row.active && row.ui?.toolbar?.slot === slot.dataset.moduleSlot);
      const labels = entries.map(row => tr(row.ui.toolbar.label));
      const signature = JSON.stringify(entries.map((row,index) => [row.id, labels[index]]));
      if (signature === slot.dataset.moduleEntries) return;
      slot.dataset.moduleEntries = signature;
      for (const previous of slot.parentElement.querySelectorAll('[data-module-entry]')) {
        if (previous.dataset.moduleEntry === slot.dataset.moduleSlot) previous.remove();
      }
      slot.before(...entries.map((row,index) => {
        const entry = row.ui.toolbar, button = document.createElement('button');
        button.id = entry.id;
        button.type = 'button';
        button.className = 'secondary';
        button.dataset.moduleEntry = entry.slot;
        button.dataset.icon = entry.icon;
        button.textContent = labels[index];
        button.onclick = () => openOptions(row.id).catch(error => notice(error.message, true));
        return button;
      }));
    });
  }

  async function loadPanel(id) {
    const row = rowFor(id);
    if (!row?.active) throw new Error(tr('modules.module_unavailable'));
    const revision = row.active_revision;
    const cached = panels.get(id);
    if (cached && cached.revision === revision) return cached.loading;
    const loading = new Promise((resolve, reject) => {
      const script = document.createElement('script');
      script.src = `/assets/module-${encodeURIComponent(id)}.js?revision=${encodeURIComponent(revision)}`;
      script.onload = () => {script.remove(); resolve(revision);};
      script.onerror = () => {script.remove(); reject(new Error(tr('modules.module_unavailable')));};
      document.head.append(script);
    });
    panels.set(id, {revision, loading});
    try { return await loading; }
    catch (error) { if (panels.get(id)?.loading === loading) panels.delete(id); throw error; }
  }

  function applyStatus(data, id) {
    if (rows && data.modules.some(row => row.installed_version && !rowFor(row.id)?.installed_version)) checkedAt = 0;
    rows = data.modules;
    migrationAllowed = data.migration_allowed !== false;
    onChange(id);
    renderUpdateHints();
  }

  async function refresh() {
    applyStatus(await api('/api/modules'));
  }

  async function openUpdates(id) {
    const title = tr('modules.check_updates') + ' · ' + moduleText(id);
    const navigation = {back:open};
    let offer = offerFor(id);
    if (!offer) {
      openModal(title, `<p role="status">${tr('transfers.checking')}</p>`, '', null, null, navigation);
      const owner = $('modal_body').firstElementChild;
      try { await checkUpdates(true); }
      catch (_) {
        if (owner.isConnected) owner.textContent = tr('modules.update_check_failed');
        return;
      }
      if (!owner.isConnected) return;
      offer = offerFor(id);
    }
    const current = esc(rowFor(id).installed_version);
    if (!offer) {
      openModal(title, `<p class="muted">${current}</p><p>${tr('modules.up_to_date')}</p>`, '', null, null, navigation);
      return;
    }
    const notes = releaseHistoryText(offer, getLanguage(), tr) || offer.release_notes.trim() || tr('modules.no_release_notes');
    openModal(title, `<p class="muted">${current} → <strong>${esc(offer.latest_version)}</strong></p>
      <div class="update-notes"><h3>${tr('transfers.what_changed')}</h3><div class="update-notes-content" id="module_release_notes"></div></div>`,
      tr('modules.install_update'), async () => {
        const owner = $('module_release_notes');
        try {
          applyStatus(await api('/api/modules', {method:'POST', body:JSON.stringify({id,action:'update',revision:offer.revision})}), id);
        } catch (error) {
          if (error.payload?.error?.code !== 'module_update_changed') throw error;
          offers.delete(id);
          renderUpdateHints();
          throw new Error(tr('modules.update_changed'));
        }
        if (owner.isConnected) open();
      }, null, navigation);
    renderReleaseNotes($('module_release_notes'), notes);
  }

  function actionButton(attributes, glyph, label, enabled, hint = label) {
    return `<button type="button" class="secondary icon-only" ${attributes} ${enabled ? '' : 'disabled'} aria-label="${esc(label)}" title="${esc(hint)}">${icon(glyph)}</button>`;
  }

  function openInfo(id, back) {
    openModal(moduleText(id), `<p>${esc(moduleText(id, '_summary'))}</p>
      <p>${esc(moduleText(id, '_details'))}</p>`, '', null, null, {back, reopen:() => openInfo(id, back)});
  }

  async function open() {
    if (!rows) await refresh();
    openModal(tr('modules.modules'), `<div class="settings-list" id="emp_modules">${rows.map(row => {
      const installed = Boolean(row.installed_version);
      const repair = installed && row.enabled && row.error;
      const installAction = !installed || repair ? 'install' : 'uninstall';
      const installLabel = !installed ? 'install' : repair ? 'reinstall' : 'uninstall';
      const toggleAction = row.enabled ? 'disable' : 'enable';
      const removeBlocked = installAction === 'uninstall' && (row.enabled || row.active || row.draining);
      const status = row.draining ? tr('modules.draining') : row.restart_required ? tr('modules.restart_emp_to_apply')
        : row.error === 'module_update_rolled_back' ? tr('modules.update_rolled_back')
        : row.error ? tr('modules.module_unavailable') : '';
      const name = moduleText(row.id);
      const info = esc(tr('modules.view_info', {name}));
      const id = esc(row.id);
      const action = kind => `data-module-id="${id}" data-module-action="${kind}"`;
      const installText = tr('modules.' + installLabel);
      return `<div class="setting-row module-row" data-module-row="${id}">
        <div class="module-identity"><span class="module-icon" ${row.active ? `data-active role="img" aria-label="${esc(tr('modules.enabled'))}"` : 'aria-hidden="true"'}>${icon(row.ui.icon)}</span>
          <div><strong>${esc(name)}</strong><div class="module-version">${esc(row.installed_version ?? row.version)}</div></div></div>
        <div class="module-description">
          <p>${esc(moduleText(row.id, '_summary'))}</p><button type="button" class="shadow-help" data-module-info="${id}" aria-label="${info}" title="${info}">?</button>
          ${status ? `<div class="setting-help">${status}</div>` : ''}</div>
        <div class="module-actions">
          ${actionButton(action(installAction), installAction === 'uninstall' ? 'trash' : 'download', installText, !removeBlocked, removeBlocked ? tr('modules.disable_to_remove') : installText)}
          ${actionButton(action(toggleAction), row.enabled ? 'pause' : 'play', tr('modules.' + toggleAction), installed && !row.draining)}
          ${actionButton(action('check_updates'), 'refresh', tr('modules.check_updates'), installed && !row.draining)}
          ${actionButton(`data-module-options="${id}"`, 'settings', tr('modules.configure', {name}), row.active)}
        </div></div>`;
    }).join('')}</div>`, '', null, null, {reopen:open});
    const form = $('emp_modules');
    renderUpdateHints();
    void checkHint();
    if (rows.some(row => row.draining)) {
      const checkDrain = async () => {
        if (!form.isConnected) return;
        try {
          await refresh();
          if (!form.isConnected) return;
          if (rows.some(row => row.draining)) setTimeout(checkDrain, 1000);
          else open();
        } catch (error) {
          if (form.isConnected) notice(error.message, true);
        }
      };
      setTimeout(checkDrain, 1000);
    }
    form.querySelectorAll('[data-module-info]').forEach(button => {
      button.onclick = () => openInfo(button.dataset.moduleInfo, open);
    });
    form.querySelectorAll('[data-module-options]').forEach(button => {
      button.onclick = async () => {
        button.disabled = true;
        try { await openOptions(button.dataset.moduleOptions, open); }
        catch (error) { if (form.isConnected) notice(error.message, true); }
        finally { button.disabled = false; }
      };
    });
    const change = async button => {
      const actions = button.parentElement.querySelectorAll('button:not(:disabled)');
      actions.forEach(control => control.disabled = true);
      $('modal_status').textContent = tr('modules.working');
      try {
        applyStatus(await api('/api/modules', {method:'POST',body:JSON.stringify({
          id:button.dataset.moduleId,action:button.dataset.moduleAction
        })}), button.dataset.moduleId);
        if (form.isConnected) open();
      } catch (error) {
        if (form.isConnected) $('modal_status').textContent = error.payload?.error?.code || error.message;
      } finally {
        actions.forEach(control => control.disabled = false);
      }
    };
    form.querySelectorAll('[data-module-action]').forEach(button => button.onclick = () => {
      if (button.dataset.moduleAction === 'check_updates') return openUpdates(button.dataset.moduleId);
      if (button.dataset.moduleAction !== 'uninstall') return change(button);
      openModal(tr('modules.uninstall') + ' · ' + moduleText(button.dataset.moduleId),
        `<p>${tr(button.dataset.moduleId === 'team' ? 'modules.uninstall_team_removes_credentials' : 'modules.uninstall_keeps_data')}</p>`, tr('modules.uninstall'), async () => {
          const owner = $('modal_body').firstElementChild;
          await api('/api/modules', {method:'POST',body:JSON.stringify({id:button.dataset.moduleId,action:'uninstall'})});
          await refresh();
          if (button.dataset.moduleId === 'team') await onTeamRemoved();
          if (owner.isConnected) open();
        }, null, {back:open});
    });
  }
  return {active, openOptions, editModel, renderEntries, migrationAllowed:() => migrationAllowed, refresh, open, loadPanel, checkHint};
}
