function createTeamWorkspace({api, openModal, notice, $, esc, tr, getState, onChange, onSync}) {
  let current = {joined:false}, inviteTick;
  const title = () => tr('modules.team_workspace');
  const roleName = role => ({
    member:tr('modules.member'),
    team_admin:tr('modules.team_administrator'),
    vps_admin:tr('modules.vps_administrator')
  })[role] || '';
  const roleIcons = {
    member:'<circle cx="12" cy="8" r="3.3"/><path d="M5 20v-1.4a5 5 0 0 1 5-5h4a5 5 0 0 1 5 5V20"/>',
    team_admin:'<path d="M12 3.5 4.5 6v5c0 4.6 3.1 7.3 7.5 9.5 4.4-2.2 7.5-4.9 7.5-9.5V6L12 3.5Z"/><path d="m8.8 11.9 2.1 2.1 4.3-4.3"/>',
    vps_admin:'<rect x="4" y="5" width="16" height="5.5" rx="1.6"/><rect x="4" y="13.5" width="16" height="5.5" rx="1.6"/><path d="M7.5 7.75h.5m-.5 8.5h.5m7-8.5h2m-2 8.5h2"/>'
  };
  const roleSvg = role => `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.65" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${roleIcons[role] || ''}</svg>`;
  const action = (name, body = {}) => api('/api/team/' + name, {method:'POST',body:JSON.stringify(body)});
  const failure = error => {
    const code = error.payload?.error?.code;
    return ['workspace_join_pending', 'workspace_join_code_invalid', 'workspace_join_limited',
      'workspace_member_exists', 'workspace_discovery_unavailable', 'workspace_discovery_requires_https',
      'workspace_share_version_conflict','workspace_subscription_sign_in_required',
      'workspace_only_native_subscription', 'workspace_handover_required',
      'workspace_handover_successor_required','workspace_backup_too_large'].includes(code)
      ? tr('modules.' + code) : error.message;
  };
  const locale = () => document.documentElement.lang || 'zh-CN';
  const money = nanos => nanos == null ? '—' : new Intl.NumberFormat(locale(), {
    style:'currency',currency:'USD',maximumFractionDigits:4
  }).format(nanos / 1e9);
  const budgetDisplay = nanos => nanos == null ? `<span class="team-unlimited" role="img" aria-label="${esc(tr('modules.no_daily_limit'))}" title="${esc(tr('modules.no_daily_limit'))}">∞</span>` : esc(money(nanos));
  const number = value => Number.isFinite(value) ? value.toLocaleString(locale()) : '—';
  const periods = ['today','1h','1d','7d','30d'];
  const periodBar = (period, choices = periods) => `<div class="period-presets" role="group" aria-label="${tr('core.period')}">${choices.map(id =>
    `<button class="secondary ${id === period ? 'active' : ''}" type="button" data-team-period="${id}" aria-pressed="${id === period}">${esc(id === 'lifetime' ? tr('modules.all_time') : id === 'today' ? tr('modules.today_utc') : periodPresetLabel(id))}</button>`).join('')}</div>`;
  const scope = () => current.identity?.member;
  const date = seconds => seconds == null ? '—' : new Date((seconds - 8 * 3600) * 1000).toISOString().slice(0,10);
  const compact = row => compactUsage(tokens(row)).html;
  const spend = row => row?.spend_nanos ?? (row?.requests === 0 ? 0 : null);
  const spendDisplay = row => {
    const value = spend(row);
    return compactUsage(value == null ? null : value / 1e9,true);
  };
  const compactSpend = row => spendDisplay(row).html;
  const profiles = createTeamProfiles({action,openModal,$,esc,tr,scope,number,tokens,roleName,date,compact,compactSpend});
  const field = (id, label, options = '') => `<div class="setting-row routing-mode"><label for="${id}">${label}</label><input id="${id}" ${options}></div>`;
  const summaryCell = (label, value) => `<div><dt>${tr(label)}</dt><dd>${value}</dd></div>`;

  async function refresh() {
    current = await api('/api/team/status');
    onChange();
  }

  function open(back) {
    if (!current.joined) return join(back);
    const member = scope();
    const manage = member.role !== 'member';
    const vps = member.role === 'vps_admin';
    const toolbarButton = (attributes, text, disabled = false) => `<button type="button" class="secondary" ${attributes} ${disabled ? 'disabled' : ''}>${tr(text)}</button>`;
    openModal(title(), `<div id="team_home">
      <div class="team-overview-row">
        <div class="team-identity">
          <span class="team-role-icon team-role-${esc(member.role)}" aria-hidden="true">${roleSvg(member.role)}</span>
          <div><h3>${esc(member.nickname || member.name)}</h3><div class="setting-help">${roleName(member.role)}</div></div>
          <dl class="team-meta">
            <div><dt>${tr('modules.workspace_name')}</dt><dd>${esc(current.identity.workspace.name)}</dd></div>
            <div><dt>${tr('modules.workspace_created')}</dt><dd data-team-created>—</dd></div>
            <div><dt>${tr('modules.joined_at')}</dt><dd data-team-joined>—</dd></div>
          </dl>
        </div>
        <div class="team-overview-actions">
          ${manage ? toolbarButton('data-team-choice="manage"','modules.manage') : toolbarButton('data-team-subscriptions','modules.workspace_subscriptions')}
          ${vps ? toolbarButton('data-team-backup','modules.backup')
            : toolbarButton('data-team-choice="publish"','modules.upload',!member.publish) +
              toolbarButton('data-team-choice="sync"','modules.sync',!member.sync) +
              toolbarButton('data-team-leave','modules.sign_out')}
        </div>
      </div>
      <section class="team-home-usage" aria-labelledby="team_usage_title">
        <div class="team-usage-heading">
          <h3 id="team_usage_title">${tr(manage ? 'modules.team_usage' : 'modules.my_usage')}</h3>
          <div class="team-usage-actions">
            <button type="button" class="secondary" data-team-profiles>${tr('modules.member_profiles')}</button>
            <button type="button" class="secondary team-usage-toggle" data-team-usage-toggle data-icon="chevron-right" aria-expanded="false" aria-controls="team_usage_detail" title="${esc(tr('modules.expand_usage'))}" aria-label="${esc(tr('modules.expand_usage'))}"></button>
          </div>
        </div>
        <dl class="team-usage-summary" aria-busy="true">
          ${summaryCell(manage ? 'modules.team_member_count' : 'modules.today_spend_limit',manage ? '<span data-team-member-count>—</span>' : '<span data-team-today>—</span>')}
          ${summaryCell('modules.today_tokens','<span data-team-today-tokens>—</span>')}
          ${summaryCell('modules.lifetime_tokens','<span data-team-lifetime-tokens>—</span>')}
          ${summaryCell('modules.lifetime_spend','<span data-team-lifetime-usd>—</span>')}
        </dl>
        <div id="team_usage_detail" class="team-usage-detail" hidden>${periodBar('today',['today','1d','7d','30d','lifetime'])}<div data-team-breakdown aria-live="polite"></div></div>
      </section>
    </div>`, '', null, null, {back,reopen:() => open(back)});
    const home = () => open(back);
    const view = $('team_home');
    bindUsageDetails(view,member);
    view.querySelector('[data-team-profiles]').onclick = () => profiles.directory(home);
    if (vps) {
      view.querySelector('[data-team-backup]').onclick = event => downloadBackup(event.currentTarget);
    } else {
      const leave = async successor => {
        current = successor ? await action('handover',{successor}) : await action('logout');
        onChange();
        await onSync();
        if ($('team_leave_confirm')) join(back);
      };
      view.querySelector('[data-team-leave]').onclick = () => confirmLeave(home,manage,leave);
      view.querySelector('[data-team-choice=publish]').onclick = () => publish(home);
      view.querySelector('[data-team-choice=sync]').onclick = event => sync(event.currentTarget);
    }
    if (manage) view.querySelector('[data-team-choice=manage]').onclick = () => openManagement(home);
    else view.querySelector('[data-team-subscriptions]').onclick = () => profiles.subscriptions(home);
    action('status').then(result => {
      const prior = scope();
      current = result;
      if (!current.joined) {
        onChange();
        if (view.isConnected) join(back);
        return;
      }
      if (prior.role !== scope().role || prior.publish !== scope().publish || prior.sync !== scope().sync) {
        onChange();
        if (view.isConnected) home();
      }
    }).catch(error => {
      if (!view.isConnected) return;
      if (error.status === 401 || error.payload?.error?.code === 'workspace_authentication_required') {
        current = {joined:false,base:current.base};
        onChange();
        join(back);
      } else $('modal_status').textContent = failure(error);
    });
    void loadSummary(view,member);
  }

  async function loadSummary(view, member) {
    try {
      const {joined_at,created_at,members,budget_nanos,today,lifetime} = await action('overview');
      if (!view.isConnected) return;
      view.querySelector('[data-team-created]').textContent = date(created_at);
      view.querySelector('[data-team-joined]').textContent = date(joined_at);
      if (member.role === 'member') {
        const currentDay = view.querySelector('[data-team-today]');
        currentDay.innerHTML = `${esc(money(spend(today)))} / ${budgetDisplay(budget_nanos)}`;
        if (today.unknown_cost_requests) currentDay.title = tr('modules.unknown_count',{count:number(today.unknown_cost_requests)});
      } else view.querySelector('[data-team-member-count]').textContent = number(members);
      view.querySelector('[data-team-today-tokens]').innerHTML = compact(today);
      view.querySelector('[data-team-lifetime-tokens]').innerHTML = compact(lifetime);
      view.querySelector('[data-team-lifetime-usd]').innerHTML = compactSpend(lifetime);
    } catch (error) {
      if (view.isConnected) $('modal_status').textContent = failure(error);
    } finally {
      view.querySelector('.team-usage-summary').removeAttribute('aria-busy');
    }
  }

  function usageFor(rows, member) {
    if (member.role === 'member') return rows.find(row => row.member === member.id);
    const totals = {member:'workspace',models:[]};
    const models = new Map();
    const fields = ['input_tokens','output_tokens','spend_nanos','unknown_cost_requests'];
    const add = (target, values) => {
      for (const key of fields) if (Number.isFinite(values[key])) target[key] = (target[key] ?? 0) + values[key];
    };
    for (const row of rows) {
      add(totals,row);
      for (const model of row.models) {
        if (!models.has(model.model)) models.set(model.model,{model:model.model});
        add(models.get(model.model),model);
      }
    }
    totals.models = [...models.values()];
    return totals;
  }

  function bindUsageDetails(view, member) {
    const button = view.querySelector('[data-team-usage-toggle]');
    const detail = $('team_usage_detail');
    const data = detail.querySelector('[data-team-breakdown]');
    const filters = detail.querySelectorAll('[data-team-period]');
    let selected = 'today';
    async function load(period) {
      selected = period;
      filters.forEach(control => {
        const pressed = control.dataset.teamPeriod === period;
        control.classList.toggle('active',pressed);
        control.setAttribute('aria-pressed',String(pressed));
      });
      data.textContent = tr('modules.loading');
      try {
        const report = await action('usage',{period});
        if (!data.isConnected || selected !== period) return;
        const row = usageFor(report.rows,member);
        data.innerHTML = row?.models.length ? `<div class="team-breakdown-total">${compact(row)} / ${compactSpend(row)}</div>${modelUsage(row)}` : esc(tr('modules.no_usage'));
      } catch (error) {
        if (data.isConnected && selected === period) data.textContent = failure(error);
      }
    }
    filters.forEach(control => control.onclick = () => load(control.dataset.teamPeriod));
    button.onclick = () => {
      detail.hidden = !detail.hidden;
      button.setAttribute('aria-expanded',String(!detail.hidden));
      button.ariaLabel = button.title = tr(detail.hidden ? 'modules.expand_usage' : 'modules.collapse_usage');
      if (!detail.hidden) void load(selected);
    };
  }

  async function downloadBackup(button) {
    button.disabled = true;
    try {
      const backup = await action('backup');
      const url = URL.createObjectURL(new Blob([JSON.stringify(backup)],{type:'application/json'}));
      const link = document.createElement('a');
      link.href = url;
      link.download = 'EMP-team-' + new Date().toISOString().slice(0,10) + '.empteam';
      link.click();
      setTimeout(() => URL.revokeObjectURL(url),1000);
    } catch (error) {
      if (button.isConnected) $('modal_status').textContent = failure(error);
    } finally {
      button.disabled = false;
    }
  }

  async function confirmLeave(home, manage, leave) {
    const select = manage ? `<div class="team-handover"><label for="team_successor">${tr('modules.workspace_successor')}</label><select id="team_successor" disabled></select></div>` : '';
    openModal(tr('modules.sign_out_of_the_workspace'), `<div id="team_leave_confirm"><p>${tr(manage ? 'modules.workspace_handover_required' : 'modules.leave_removes_credentials')}</p>${select}</div>`, tr('modules.sign_out'), async () => {
      const successor = manage ? $('team_successor').value : '';
      if (manage && !successor) return;
      await leave(successor);
    }, null, {back:home});
    if (!manage) return;
    const input = $('team_successor');
    const submit = $('modal_submit');
    submit.disabled = true;
    try {
      const {members} = await action('list_members');
      if (!input.isConnected) return;
      const eligible = members.filter(member => member.role === 'member' && member.enabled && !member.pending);
      input.add(new Option(tr(eligible.length ? 'modules.choose_successor' : 'modules.no_successor'), ''));
      for (const member of eligible) input.add(new Option(member.nickname || member.name,member.id));
      input.disabled = !eligible.length;
      input.onchange = () => {submit.disabled = !input.value;};
    } catch (error) {
      if (input.isConnected) $('modal_status').textContent = failure(error);
    }
  }

  function join(back, signingIn = false, preset = {}) {
    openModal(tr(signingIn ? 'modules.sign_in' : 'modules.join_workspace'), `<div id="team_login" class="settings-list">
      ${current.recovery_required ? `<p class="muted">${tr('modules.recover_team_services')}</p>` : ''}
      <div class="team-menu-actions">
        <button type="button" class="secondary" id="team_switch_entry">${tr(signingIn ? 'modules.join_workspace' : 'modules.existing_member')}</button>
        ${signingIn ? '' : `<button type="button" class="secondary" id="team_discover">${tr('modules.find_nearby_workspace')}</button>`}
      </div>
      ${field('team_base', tr('modules.workspace_url'), `inputmode="url" placeholder="https://workspace.example.org" value="${esc(preset.base || current.base || '')}" required`)}
      <div id="team_nearby" class="settings-list" aria-live="polite"></div>
      ${signingIn ? '' : field('team_nickname', tr('modules.nickname'), 'autocomplete="nickname" maxlength="48" required')}
      ${signingIn ? '' : field('team_join_code', tr('modules.join_code'), 'inputmode="numeric" pattern="[0-9]{7}" maxlength="7" autocomplete="off" required')}
      ${field('team_name', tr('modules.username'), `autocomplete="username" value="${esc(preset.name || '')}" maxlength="64" required`)}
      ${field('team_password', tr('modules.password'), signingIn ? 'type="password" autocomplete="current-password" required' : 'type="password" minlength="12" maxlength="256" autocomplete="new-password" required')}
    </div>`, tr(signingIn ? 'modules.sign_in' : 'modules.request_to_join'), async () => {
      const form = $('team_login');
      const invalid = [...form.querySelectorAll('input')].find(input => !input.checkValidity());
      if (invalid) { invalid.reportValidity(); return; }
      const account = {base:$('team_base').value.trim(),name:$('team_name').value.trim(),password:$('team_password').value};
      try {
        if (signingIn) {
          current = await action('login', account);
          onChange();
          if (form.isConnected) open(back);
        } else {
          await action('join', {...account,nickname:$('team_nickname').value.trim(),code:$('team_join_code').value});
          if (form.isConnected) {
            join(back, true, {base:account.base,name:account.name});
            $('modal_status').textContent = tr('modules.workspace_join_pending');
          }
        }
      } catch (error) {
        if (form.isConnected) $('modal_status').textContent = failure(error);
      }
    }, null, {back,reopen:() => join(back, signingIn, preset)});
    $('team_switch_entry').onclick = () => join(back, !signingIn, {base:$('team_base').value});
    if (!signingIn) $('team_discover').onclick = async event => {
      const button = event.currentTarget;
      const panel = $('team_nearby');
      panel.textContent = tr('modules.loading');
      button.disabled = true;
      try {
        const {workspaces} = await action('discover');
        if (!panel.isConnected) return;
        panel.innerHTML = workspaces.length ? workspaces.map(({name,base}) => `<button type="button" class="secondary" data-team-origin="${esc(base)}">${esc(name)} · ${esc(base)}</button>`).join('')
          : esc(tr('modules.no_nearby_workspace'));
        panel.querySelectorAll('[data-team-origin]').forEach(workspace => workspace.onclick = () => {
          $('team_base').value = workspace.dataset.teamOrigin;
          panel.replaceChildren();
          $('team_nickname').focus();
        });
      } catch (error) {
        if (panel.isConnected) panel.textContent = failure(error);
      } finally {
        button.disabled = false;
      }
    };
  }

  function publish(home) {
    const native = getState().native_account;
    const ready = native?.auth_mode === 'subscription' && native.credential_set;
    openModal(tr('modules.upload_a_service'), `<div class="settings-list" id="team_upload_form">
      <div class="setting-row"><strong>Native · Codex</strong></div>
      <p class="muted">${tr(ready ? 'modules.authorized_teammates_can_sync_native' : 'modules.workspace_subscription_sign_in_required')}</p>
      </div>`, tr('modules.upload'), async () => {
        const form = $('team_upload_form');
        try {
          await action('publish');
          if (form.isConnected) {
            home();
            $('modal_status').textContent = tr('modules.uploaded_for_review');
          }
        } catch (error) {
          if (form.isConnected) $('modal_status').textContent = failure(error);
        }
      }, null, {back:home});
    $('modal_submit').disabled = !ready;
  }

  async function sync(button) {
    const label = button.textContent;
    button.disabled = true;
    button.textContent = tr('modules.working');
    $('modal_status').textContent = '';
    try {
      const result = await action('sync');
      await onSync();
      if (button.isConnected) {
        const message = tr('modules.synced_count',{count:result.synced}) +
          (result.skipped_native ? ' ' + tr('modules.synced_skipped_native',{count:result.skipped_native}) : '');
        notice(message);
      }
    } catch (error) {
      if (button.isConnected) $('modal_status').textContent = failure(error);
    } finally {
      button.textContent = label;
      button.disabled = false;
    }
  }

  function tokens(row) {
    return row?.input_tokens == null && row?.output_tokens == null ? null : (row.input_tokens ?? 0) + (row.output_tokens ?? 0);
  }

  function tokenDetails(row, includeSpend = false) {
    const total = compactUsage(tokens(row));
    const cost = includeSpend ? ` / ${compactSpend(row)}` : '';
    if (!row?.models?.length) return total.html + cost;
    const id = esc('team_models_' + row.member);
    const text = `${number(row.input_tokens)} / ${number(row.output_tokens)}`;
    return `<button type="button" class="secondary team-token-detail" data-shadow-help="" data-shadow-help-template="${id}" aria-label="${esc(tr('modules.view_model_usage') + ' · ' + total.title)}" aria-haspopup="dialog" aria-expanded="false" aria-controls="shadow_feedback">${esc(total.text)}${cost}</button>
      <template id="${id}"><strong>${tr('modules.view_model_usage')}</strong><p>${tr('modules.input_output')} · ${esc(text)}</p>${modelUsage(row)}</template>`;
  }

  function modelUsage(row) {
    return `<dl class="team-model-usage">${row.models.map(model => `<dt title="${esc(model.model)}">${esc(model.model)}</dt><dd>${esc(number(tokens(model)))} / ${esc(money(spend(model)))}
        ${model.unknown_cost_requests ? `<small>${tr('modules.unknown_count',{count:number(model.unknown_cost_requests)})}</small>` : ''}
      </dd>`).join('')}</dl>`;
  }

  function usageTable(columns, rows) {
    return `<div class="model-table-wrap"><table class="model-table team-usage-table">
      <thead><tr>${columns.map(label => `<th scope="col">${label}</th>`).join('')}</tr></thead>
      <tbody>${rows}</tbody></table></div><p class="setting-help">${tr('modules.resets_daily_in_utc_budget_alerts')}</p>`;
  }

  function openManagement(home) {
    const reopen = () => openManagement(home);
    openModal(tr('modules.workspace_management'), `<div class="choice-grid settings-choices team-management-choices" id="team_management">
      <button type="button" class="choice" data-team-manage-members><strong>${tr('modules.manage_members')}</strong></button>
      <button type="button" class="choice" data-team-manage-subscriptions><strong>${tr('modules.manage_subscriptions')}</strong></button>
    </div>`, '', null, null, {back:home,reopen});
    $('team_management').querySelector('[data-team-manage-members]').onclick = () => openMembers(reopen);
    $('team_management').querySelector('[data-team-manage-subscriptions]').onclick = () => reviewServices(reopen);
  }

  function openMembers(back, initial = 'today') {
    let selected = initial, generation = 0, members;
    const reopen = () => openMembers(back,selected);
    openModal(tr('modules.manage_members'), `<div id="team_manage" class="settings-list">
      <div class="team-members-toolbar">${periodBar(selected)}<div class="team-member-actions"><button type="button" class="secondary" data-team-audit>${tr('modules.audit')}</button><button type="button" data-member-add>${tr('modules.add_member')}</button></div></div>
      <div data-team-usage-table>${tr('modules.loading')}</div></div>`,'',null,null,{back,reopen});
    const panel = $('team_manage');
    const table = panel.querySelector('[data-team-usage-table]');
    const periodButtons = [...panel.querySelectorAll('[data-team-period]')];
    async function load(period) {
      const request = ++generation;
      selected = period;
      for (const button of periodButtons) {
        const active = button.dataset.teamPeriod === selected;
        button.classList.toggle('active',active);
        button.setAttribute('aria-pressed',String(active));
      }
      table.setAttribute('aria-busy','true');
      try {
        const [list,report] = await Promise.all([members ?? action('list_members').then(reply => reply.members),action('usage',{period})]);
        if (!table.isConnected || request !== generation) return;
        const byId = new Map(report.rows.map(row => [row.member,row]));
        if (members) {
          for (const row of table.querySelectorAll('[data-team-member]')) {
            row.lastElementChild.innerHTML = tokenDetails(byId.get(row.dataset.teamMember),true);
          }
          return;
        }
        members = list;
        const columns = [tr('modules.member'),tr('modules.daily_usd_limit'),
          tr('modules.permissions') + ' ' + shadowHelp(tr('modules.member_permissions_help')),tr('modules.token_spend')];
        const rows = members.map(member => {
          const usage = byId.get(member.id);
          const name = member.nickname || member.name;
          const editable = member.can_manage && !member.pending;
          const permissions = [['fast','Fast'],['ultrafast','Ultrafast'],['publish',tr('modules.upload')],['sync',tr('modules.sync')],['enabled',tr('modules.account_enabled')]];
          const settings = permissions.map(([id,label]) => `<label><input type="checkbox" data-team-permission="${id}" ${member[id] ? 'checked' : ''} ${editable ? '' : 'disabled'}><span>${label}</span></label>`).join('');
          const editRole = editable && scope().role === 'vps_admin';
          const role = member.pending ? tr('modules.pending_approval') : roleName(member.role);
          const changeRole = editRole ? tr('modules.change_role') + ' · ' + role : role;
          const rolePicker = editRole
            ? `<select class="team-role-select" data-team-member-role title="${esc(changeRole)}" aria-label="${esc(tr('modules.change_role') + ' · ' + name)}">${['member','team_admin'].map(role => `<option value="${role}" ${role === member.role ? 'selected' : ''}>${roleName(role)}</option>`).join('')}</select>` : '';
          return `<tr data-team-member="${esc(member.id)}"><th scope="row"><div class="team-member-name"><span class="team-role-icon team-role-${esc(member.role)}" title="${esc(changeRole)}" ${editRole ? 'data-editable-role' : `role="img" aria-label="${esc(role)}"`}>${roleSvg(member.role)}${rolePicker}</span><span>${esc(name)}</span></div></th>
            <td>${member.can_budget && !member.pending ? `<label class="team-budget"><span aria-hidden="true">$</span><input data-member-budget type="number" min="0" max="1000000" step="any" value="${member.budget_nanos == null ? '' : esc(member.budget_nanos/1e9)}" placeholder="∞" title="${esc(tr('modules.budget_input_help'))}" aria-label="${esc(tr('modules.daily_usd_limit') + ' · ' + name)}"></label>` : budgetDisplay(member.budget_nanos)}</td>
            <td>${member.pending ? `<button type="button" data-member-approve="${esc(member.id)}">${tr('modules.approve_member')}</button>` : `<div class="team-tier-controls">${settings}<span class="setting-help team-reset-permission" tabindex="0" title="${esc(tr('modules.reset_policy'))}">${tr('modules.reset_permission')} · ${tr(member.role === 'member' ? 'modules.reset_view_only' : 'modules.reset_can_use')}</span></div>`}</td>
            <td>${tokenDetails(usage,true)}</td></tr>`;
        }).join('');
        table.innerHTML = usageTable(columns,rows);
      } catch (error) {
        if (table.isConnected && request === generation) {
          if (members) $('modal_status').textContent = failure(error);
          else table.textContent = failure(error);
        }
      } finally {
        if (request === generation) table.removeAttribute('aria-busy');
      }
    }
    for (const button of periodButtons) button.onclick = () => load(button.dataset.teamPeriod);
    void load(selected);
    bindInvitation(panel.querySelector('[data-member-add]'));
    panel.querySelector('[data-team-audit]').onclick = () => openAudit(reopen);
    table.onclick = async event => {
      const member = members.find(item => item.id === event.target.closest('tr')?.dataset.teamMember);
      const button = event.target.closest('button');
      if (!member || !button) return;
      if (!button.hasAttribute('data-member-approve')) return;
      button.disabled = true;
      try {
        await action('member_access',{id:member.id,role:'member',publish:false,sync:true,enabled:true});
        if (panel.isConnected) reopen();
      } catch (error) {
        if (panel.isConnected) $('modal_status').textContent = failure(error);
        button.disabled = false;
      }
    };
    table.onkeydown = event => {
      if (event.key !== 'Enter' || !event.target.matches('[data-member-budget]')) return;
      event.preventDefault();
      event.target.blur();
    };
    table.onchange = async event => {
      const control = event.target;
      const isBudget = control.matches('[data-member-budget]');
      const isRole = control.matches('[data-team-member-role]');
      const permission = control.dataset.teamPermission;
      if (!isBudget && !isRole && !permission) return;
      const row = control.closest('tr');
      const member = members.find(item => item.id === row.dataset.teamMember);
      if (isBudget && !control.checkValidity()) {
        $('modal_status').textContent = tr('modules.enter_a_valid_budget');
        return;
      }
      const controls = row.querySelectorAll('input:not(:disabled),select:not(:disabled)');
      controls.forEach(input => input.disabled = true);
      $('modal_status').textContent = '';
      try {
        if (isBudget) {
          const value = control.value === '' ? null : Math.round(control.valueAsNumber*1e9);
          await action('budget',{member:member.id,daily_usd_nanos:value});
          member.budget_nanos = value;
        } else if (permission === 'fast' || permission === 'ultrafast') {
          await action('member_tiers',{id:member.id,fast:member.fast,ultrafast:member.ultrafast,[permission]:control.checked});
          member[permission] = control.checked;
        } else {
          const access = {id:member.id,role:member.role,publish:member.publish,sync:member.sync,enabled:member.enabled};
          if (isRole) access.role = control.value;
          else access[permission] = control.checked;
          await action('member_access',access);
          Object.assign(member,access);
          if (isRole && panel.isConnected) reopen();
        }
      } catch (error) {
        if (isBudget) control.value = member.budget_nanos == null ? '' : member.budget_nanos/1e9;
        else if (isRole) control.value = member.role;
        else control.checked = member[permission];
        if (panel.isConnected) $('modal_status').textContent = failure(error);
      } finally {
        controls.forEach(input => input.disabled = false);
      }
    };
  }

  function openAudit(back) {
    const reopen = () => openAudit(back);
    const columns = ['time','actor','action','target'].map(key => `<th scope="col">${tr('modules.audit_' + key)}</th>`).join('');
    openModal(tr('modules.audit'), `<div id="team_audit"><div class="model-table-wrap"><table class="model-table team-audit-table" hidden><thead><tr>${columns}</tr></thead><tbody></tbody></table></div>
      <p class="setting-help" data-audit-status role="status"></p><button type="button" class="secondary" data-audit-more hidden>${tr('modules.audit_more')}</button></div>`,'',null,null,{back,reopen});
    const panel = $('team_audit');
    const table = panel.querySelector('table');
    const status = panel.querySelector('[data-audit-status]');
    const more = panel.querySelector('[data-audit-more]');
    const labels = {role:tr('modules.role'),publish:tr('modules.upload'),sync:tr('modules.sync'),enabled:tr('modules.account_enabled'),fast:'Fast',ultrafast:'Ultrafast'};
    const description = ({kind,details}) => {
      if (kind === 'share_review') return tr(details.shared ? 'modules.audit_share_started' : 'modules.audit_share_stopped');
      if (kind === 'profile_visibility') return tr(details.visible ? 'modules.audit_profiles_visible' : 'modules.audit_profiles_private');
      const label = tr('modules.audit_' + kind);
      if (kind === 'budget') return label + ' · ' + (details.daily_usd_nanos == null ? tr('modules.no_daily_limit') : money(details.daily_usd_nanos));
      const changes = Object.entries(details).filter(([key]) => Object.hasOwn(labels,key)).map(([key,value]) =>
        key === 'role' ? roleName(value) : labels[key] + ' ' + tr(value ? 'modules.audit_on' : 'modules.audit_off'));
      return [label,...changes].join(' · ');
    };
    let before;
    async function load() {
      more.disabled = true;
      status.textContent = tr('modules.loading');
      try {
        const page = await action('audit',{before});
        if (!panel.isConnected) return;
        for (const entry of page.entries) {
          const row = table.tBodies[0].insertRow();
          const identity = (label, login) => label === login ? label : `${label} (${login})`;
          for (const value of [new Date(entry.at * 1000).toLocaleString(locale()),identity(entry.actor,entry.actor_login),
            description(entry),entry.target === null ? tr('modules.team_workspace') : identity(entry.target,entry.target_login)]) {
            row.insertCell().textContent = value;
          }
        }
        before = page.before;
        table.hidden = !table.tBodies[0].rows.length;
        status.textContent = table.hidden ? tr('modules.audit_empty') : '';
        more.hidden = before == null;
        more.textContent = tr('modules.audit_more');
      } catch (error) {
        if (panel.isConnected) {
          status.textContent = failure(error);
          more.hidden = false;
          more.textContent = tr('modules.audit_retry');
        }
      } finally {
        more.disabled = false;
      }
    }
    more.onclick = load;
    void load();
  }

  async function reviewServices(back) {
    const reopen = () => reviewServices(back);
    openModal(tr('modules.manage_subscriptions'), `<div class="settings-list" id="team_shared_services">${tr('modules.loading')}</div>`,'',null,null,{back,reopen});
    const panel = $('team_shared_services');
    try {
      const {services} = await action('shared_services');
      if (!panel.isConnected) return;
      panel.innerHTML = services.length ? services.map(service => {
        const status = esc(tr(service.valid ? 'modules.share_valid' : 'modules.share_invalid'));
        return `<div class="setting-row team-subscription-row">
        <div class="team-subscription-owner"><span class="team-subscription-dot" data-valid="${service.valid === true}" role="img" aria-label="${status}" title="${status}"></span><strong>${esc(service.owner)}</strong>${service.plan ? `<span class="pill">${esc(subscriptionPlanLabel(service.plan))}</span>` : ''}</div>
        <span class="setting-help">${tr(service.shared ? 'modules.service_shared' : 'modules.service_pending_review')}</span>
        <span class="team-subscription-usage">${compact(service.usage)} / ${compactSpend(service.usage)}</span>
        <button type="button" class="secondary" data-review-service="${esc(service.id)}" ${service.shared || service.available ? '' : 'disabled'}>${tr(service.shared ? 'modules.stop_sharing' : 'modules.share_service')}</button></div>`;
      }).join('')
        : esc(tr('modules.no_uploaded_services'));
      panel.querySelectorAll('[data-review-service]').forEach(button => button.onclick = async () => {
        const service = services.find(row => row.id === button.dataset.reviewService);
        button.disabled = true;
        try {
          await action('review_service',{id:service.id,version:service.version,shared:!service.shared});
          if (panel.isConnected) reviewServices(back);
        } catch (error) {
          if (panel.isConnected) $('modal_status').textContent = failure(error);
          button.disabled = false;
        }
      });
    } catch (error) {
      if (panel.isConnected) panel.textContent = failure(error);
    }
  }

  function stopInvite() {
    clearTimeout(inviteTick);
  }

  function bindInvitation(button) {
    let invite = null;
    const reset = () => {
      stopInvite();
      invite = null;
      button.classList.remove('team-invite-active');
      button.removeAttribute('aria-label');
      button.removeAttribute('title');
      button.textContent = tr('modules.add_member');
    };
    function show({code, expires_in:seconds, lifetime}) {
      if (!button.isConnected || !code) return;
      invite = {code, deadline:performance.now() + seconds * 1000};
      button.classList.add('team-invite-active');
      button.ariaLabel = button.title = tr('modules.copy_team_code') + ' · ' + code;
      button.innerHTML = `<code>${esc(code)}</code><span class="team-invite-timer" aria-hidden="true"><span></span></span>`;
      const timer = button.lastElementChild;
      function update() {
        if (!button.isConnected) return;
        const remaining = invite.deadline - performance.now();
        if (remaining <= 0) return reset();
        const seconds = Math.ceil(remaining / 1000);
        timer.firstElementChild.textContent = Math.floor(seconds / 60) + ':' + String(seconds % 60).padStart(2,'0');
        timer.style.setProperty('--invite-progress', Math.min(1,remaining / (lifetime * 1000)) * 360 + 'deg');
        inviteTick = setTimeout(update, 200);
      }
      stopInvite();
      update();
    }
    button.onclick = async () => {
      if (invite && performance.now() < invite.deadline) {
        try {
          await navigator.clipboard.writeText(invite.code);
          if (button.isConnected) notice(tr('modules.team_code_copied'));
        } catch (error) {
          if (button.isConnected) notice(error.message, true);
        }
        return;
      }
      button.disabled = true;
      try {
        show(await action('issue_invite'));
      } catch (error) {
        if (button.isConnected) $('modal_status').textContent = failure(error);
      } finally {
        button.disabled = false;
      }
    };
    button.disabled = true;
    action('join_code').then(show).catch(error => {
      if (button.isConnected) $('modal_status').textContent = failure(error);
    }).finally(() => {button.disabled = false;});
  }

  return {open,refresh,stopInvite};
}
