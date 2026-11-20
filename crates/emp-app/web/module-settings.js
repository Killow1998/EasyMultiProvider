function createModuleSettings({api, openModal, notice, $, esc, tr, icon, onChange, openOptions, onTeamRemoved}) {
  let rows = null, migrationAllowed = false;
  const panels = new Map();
  const moduleKeys = {
    decision:'decision_api',
    gateway:'device_gateway',
    aggregate:'aggregate_routing',
    team:'team_workspace'
  };
  const moduleText = (id, suffix = '') => tr('modules.' + moduleKeys[id] + suffix);
  const active = id => rows?.some(row => row.id === id && row.active) === true;

  async function loadPanel(id) {
    const revision = rows?.find(row => row.id === id)?.active_revision;
    const cached = panels.get(id);
    if (cached && cached.revision === revision) return cached.loading;
    const loading = new Promise((resolve, reject) => {
      const script = document.createElement('script');
      script.src = `/assets/module-${id}.js`;
      script.onload = () => {script.remove(); resolve(revision);};
      script.onerror = () => {script.remove(); reject(new Error(tr('modules.module_unavailable')));};
      document.head.append(script);
    });
    panels.set(id, {revision, loading});
    try { return await loading; }
    catch (error) { panels.delete(id); throw error; }
  }

  async function refresh() {
    const data = await api('/api/modules');
    rows = data.modules;
    migrationAllowed = data.migration_allowed !== false;
    onChange();
  }

  function openInfo(id, back) {
    const installed = rows.some(row => row.id === id && row.installed_version);
    openModal(moduleText(id), `<p>${esc(moduleText(id, '_summary'))}</p>
      <p>${esc(moduleText(id, '_details'))}</p>`, installed ? tr('modules.update') : '', installed ? async () => {
        const owner = $('modal_body').firstElementChild;
        const data = await api('/api/modules', {method:'POST',body:JSON.stringify({id,action:'update'})});
        rows = data.modules;
        onChange();
        if (owner.isConnected) open();
      } : null, null, {back, reopen:() => openInfo(id, back)});
  }

  async function open() {
    if (!rows) await refresh();
    openModal(tr('modules.modules'), `<div class="settings-list" id="emp_modules">${rows.map(row => {
      const installed = Boolean(row.installed_version);
      const repair = installed && row.enabled && row.error;
      const installAction = !installed || repair ? 'install' : 'uninstall';
      const installLabel = !installed ? 'install' : repair ? 'reinstall' : 'uninstall';
      const toggleAction = row.enabled ? 'disable' : 'enable';
      const removeBlocked = installAction === 'uninstall' && (row.enabled || row.active);
      const status = row.restart_required ? tr('modules.restart_emp_to_apply')
        : row.error === 'module_update_rolled_back' ? tr('modules.update_rolled_back')
        : row.error ? tr('modules.module_unavailable') : '';
      const name = moduleText(row.id);
      const info = esc(tr('modules.view_info', {name}));
      const settingLabel = esc(tr('modules.configure', {name}));
      const id = esc(row.id);
      return `<div class="setting-row module-row" data-module-row="${id}">
        <div class="module-identity"><span class="module-icon" ${row.active ? `data-active role="img" aria-label="${esc(tr('modules.enabled'))}"` : 'aria-hidden="true"'}>${icon('module-' + row.id)}</span>
          <div><strong>${esc(name)}</strong><div class="module-version">${esc(row.installed_version ?? row.version)}</div></div></div>
        <div class="module-description">
          <p>${esc(moduleText(row.id, '_summary'))}</p><button type="button" class="shadow-help" data-module-info="${id}" aria-label="${info}" title="${info}">?</button>
          ${status ? `<div class="setting-help">${status}</div>` : ''}</div>
        <div class="module-actions">
          <button type="button" class="secondary" data-module-id="${id}" data-module-action="${installAction}" ${removeBlocked ? `disabled title="${esc(tr('modules.disable_to_remove'))}"` : ''}>${tr('modules.' + installLabel)}</button>
          <button type="button" class="secondary" data-module-id="${id}" data-module-action="${toggleAction}" ${installed ? '' : 'disabled'}>${tr('modules.' + toggleAction)}</button>
          <button type="button" class="secondary" data-module-options="${id}" aria-label="${settingLabel}" ${row.active ? '' : 'disabled'}>${tr('settings_menu')}</button>
        </div></div>`;
    }).join('')}</div>`, '', null, null, {reopen:open});
    const form = $('emp_modules');
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
        const data = await api('/api/modules', {method:'POST',body:JSON.stringify({
          id:button.dataset.moduleId,action:button.dataset.moduleAction
        })});
        rows = data.modules;
        migrationAllowed = data.migration_allowed !== false;
        onChange();
        if (form.isConnected) open();
      } catch (error) {
        if (form.isConnected) $('modal_status').textContent = error.payload?.error?.code || error.message;
      } finally {
        actions.forEach(control => control.disabled = false);
      }
    };
    form.querySelectorAll('[data-module-action]').forEach(button => button.onclick = () => {
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
  return {active, migrationAllowed:() => migrationAllowed, refresh, open, loadPanel};
}
