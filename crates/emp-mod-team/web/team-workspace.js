function createTeamWorkspace({api, openModal, notice, $, esc, tr, getState, onChange, onSync}) {
  let current = {joined:false}, inviteTick, pendingTick, pendingAccount = null;
  const title = () => tr('modules.team_workspace');
  const changed = () => { watch(); onChange(); };
  let membersView = null;
  let approvalTimer = null;
  const approvals = new Map();
  // Requests answered here in the last 30 s; a poll that started before the answer must not bring them back.
  const answered = new Map();
  const recentlyAnswered = id => Date.now() - (answered.get(id) || 0) < 30000;
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
  const roleBadge = role => `<span class="team-role-icon team-role-${esc(role)}" role="img" aria-label="${esc(roleName(role))}" title="${esc(roleName(role))}">${roleSvg(role)}</span>`;
  const action = (name, body = {}) => api('/api/team/' + name, {method:'POST',body:JSON.stringify(body)});
  const failure = error => {
    const code = error.payload?.error?.code;
    return ['workspace_join_pending', 'workspace_join_code_invalid', 'workspace_join_limited',
      'workspace_member_exists', 'workspace_discovery_unavailable', 'workspace_discovery_requires_https',
      'workspace_share_version_conflict','workspace_subscription_sign_in_required',
      'workspace_only_native_subscription', 'workspace_handover_required',
      'workspace_handover_successor_required','workspace_backup_too_large',
      'workspace_login_failed','workspace_login_limited','workspace_join_rejected',
      'workspace_member_not_pending','workspace_member_not_found'].includes(code)
      ? tr('modules.' + code) : error.message;
  };
  const showError = (owner, error) => { if (owner.isConnected) $('modal_status').textContent = failure(error); };
  const locale = () => document.documentElement.lang || 'zh-CN';
  const finite = value => Number.isFinite(value) ? value : null;
  const money = nanos => finite(nanos) == null ? '—' : new Intl.NumberFormat(locale(), {
    style:'currency',currency:'USD',maximumFractionDigits:4
  }).format(nanos / 1e9);
  const budgetDisplay = nanos => finite(nanos) == null ? `<span class="team-unlimited" role="img" aria-label="${esc(tr('modules.no_daily_limit'))}" title="${esc(tr('modules.no_daily_limit'))}">∞</span>` : esc(money(nanos));
  const number = value => Number.isFinite(value) ? value.toLocaleString(locale()) : '—';
  const periods = ['today','1d','7d','30d','lifetime'];
  const periodLabel = id => id === 'lifetime' ? periodPresetLabel('all') : id === 'today' ? tr('modules.today_utc') : periodPresetLabel(id);
  const periodBar = period => `<div class="period-presets" role="group" aria-label="${esc(tr('core.period'))}">${periods.map(id =>
    `<button class="secondary ${id === period ? 'active' : ''}" type="button" data-team-period="${id}" aria-pressed="${id === period}">${esc(periodLabel(id))}</button>`).join('')}</div>`;
  const pressPeriod = (buttons, period) => buttons.forEach(button => {
    const active = button.dataset.teamPeriod === period;
    button.classList.toggle('active',active);
    button.setAttribute('aria-pressed',String(active));
  });
  const scope = () => current.identity?.member;
  const date = seconds => finite(seconds) == null ? '—' : new Date((seconds - 8 * 3600) * 1000).toISOString().slice(0,10);
  const compact = row => compactUsage(tokens(row)).html;
  const spend = row => finite(row?.spend_nanos) ?? (row?.requests === 0 ? 0 : null);
  const compactSpend = row => {
    const value = spend(row);
    return compactUsage(value == null ? null : value / 1e9,true).html;
  };
  const subscriptionHead = last => `<div class="team-subscription-head"><span>${tr('modules.subscription_column')}</span><span>${tr('modules.subscription_usage')} ${shadowHelp(tr('modules.subscription_usage_help'))}</span><span>${tr(last)}</span></div>`;
  const planTag = plan => plan ? `<span class="team-plan">${esc(subscriptionPlanLabel(plan))}</span>` : '';
  const profiles = createTeamProfiles({action,openModal,$,esc,tr,scope,number,tokens,roleName,roleBadge,date,compact,compactSpend,planTag,failure,subscriptionHead});
  const summaryCell = (label, value) => `<div><dt>${tr(label)}</dt><dd>${value}</dd></div>`;

  async function refresh() {
    current = await api('/api/team/status');
    changed();
  }

  // Background duties of a joined EMP: the EMPT wordmark, and for administrators, join requests that
  // surface in the notification island with Decline / Approve, wherever the user is in the WebUI.
  function watch() {
    if (typeof setLogoWord === 'function') setLogoWord(current.joined ? 'EMPT' : 'EMP');
    if (typeof tutorials === 'object') tutorials.setTeam?.(current.joined);
    clearTimeout(approvalTimer);
    approvalTimer = null;
    if (canApprove()) approvalTimer = setTimeout(checkApprovals, 1500);
    else clearApprovals();
  }
  function canApprove() {
    return Boolean(current.joined && ['team_admin','vps_admin'].includes(scope()?.role));
  }
  function clearApprovals() {
    for (const id of approvals.values()) notifications.withdraw(id);
    approvals.clear();
  }
  async function checkApprovals() {
    approvalTimer = null;
    if (!canApprove()) return clearApprovals();
    if (!document.hidden) {
      try {
        const {members} = await action('list_members');
        const waiting = members.filter(member => member.pending && member.can_manage !== false && !recentlyAnswered(member.id));
        for (const id of answered.keys()) if (!recentlyAnswered(id)) answered.delete(id);
        for (const [id,notice] of approvals) if (!waiting.some(member => member.id === id)) { notifications.withdraw(notice); approvals.delete(id); }
        const arrived = waiting.some(member => !approvals.has(member.id));
        const workspace = current.identity?.workspace?.name || tr('modules.team_workspace');
        for (const member of waiting) if (!approvals.has(member.id)) {
          const name = member.nickname || member.name;
          approvals.set(member.id, notifications.show(tr('modules.join_request',{name,workspace}), false, {persist:true, group:'team-join', actions:[
            {label:tr('modules.accept_member'), primary:true, attributes:{'data-island-member-approve':member.id}, run:() => decide(member,true)},
            {label:tr('modules.decline_member'), attributes:{'data-island-member-decline':member.id}, run:() => decide(member,false)}]}));
        }
        if (arrived) membersView?.({quiet:true});
      } catch {}
    }
    if (canApprove()) approvalTimer = setTimeout(checkApprovals, 10000);
  }
  async function decide(member, approve) {
    const name = member.nickname || member.name;
    try {
      if (approve) await action('member_access',{id:member.id,role:'member',publish:false,sync:true,enabled:true});
      else await action('reject_member',{id:member.id});
      answered.set(member.id, Date.now());
      notifications.withdraw(approvals.get(member.id));
      approvals.delete(member.id);
      // While other requests wait, the result goes to the inbox so it does not cover the remaining rows.
      const message = tr(approve ? 'modules.member_accepted' : 'modules.member_declined',{name});
      if (approvals.size) notifications.remember([{id:`team-${member.id}-${Date.now()}`, message, time:Date.now()}]);
      else notice(message);
      membersView?.();
    } catch (error) {
      // Someone else already decided: drop the stale request instead of keeping it in the island.
      const code = error.payload?.error?.code;
      if (code === 'workspace_member_not_pending' || code === 'workspace_member_not_found') {
        answered.set(member.id, Date.now());
        notifications.withdraw(approvals.get(member.id));
        approvals.delete(member.id);
        membersView?.();
      }
      notice(failure(error), true);
    }
  }

  function open(back) {
    stopPending();
    if (!current.joined) return pendingAccount ? pending(back, pendingAccount) : join(back);
    remember(current.base, current.identity?.workspace?.name);
    const member = scope();
    const manage = member.role !== 'member';
    const vps = member.role === 'vps_admin';
    const button = (attributes, label, hint = '') => {
      const html = `<button type="button" class="secondary" ${attributes} ${hint ? 'disabled' : ''}>${esc(tr(label))}</button>`;
      return hint ? `<span class="team-disabled-hint" title="${esc(hint)}">${html}</span>` : html;
    };
    const actions = manage ? [button('data-team-choice="manage"','modules.manage')] : [button('data-team-subscriptions','modules.workspace_subscriptions')];
    if (vps) actions.push(button('data-team-backup','modules.backup'));
    else actions.push(
      button('data-team-choice="publish"','modules.upload',member.publish ? '' : tr('modules.upload_requires_permission')),
      button('data-team-choice="sync"','modules.sync',member.sync ? '' : tr('modules.sync_requires_permission')),
      `<button type="button" class="danger team-leave" data-team-leave>${esc(tr('modules.sign_out'))}</button>`);
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
        <div class="team-overview-actions">${actions.join('')}</div>
      </div>
      <section class="team-card team-home-usage" aria-labelledby="team_usage_title">
        <div class="team-card-heading">
          <h3 id="team_usage_title">${tr(manage ? 'modules.team_usage' : 'modules.my_usage')}</h3>
          <div class="team-card-actions">
            <button type="button" class="secondary" data-icon="user" data-team-profiles>${tr('modules.member_profiles')}</button>
            <button type="button" class="secondary team-usage-toggle" data-team-usage-toggle data-icon="chevron-right" aria-expanded="false" aria-controls="team_usage_detail" title="${esc(tr('modules.expand_usage'))}" aria-label="${esc(tr('modules.expand_usage'))}"></button>
          </div>
        </div>
        <dl class="team-usage-summary" aria-busy="true">
          ${summaryCell(manage ? 'modules.team_member_count' : 'modules.today_spend_limit',manage ? '<span data-team-member-count>—</span>' : '<span data-team-today>—</span>')}
          ${summaryCell('modules.today_tokens','<span data-team-today-tokens>—</span>')}
          ${summaryCell('modules.lifetime_tokens','<span data-team-lifetime-tokens>—</span>')}
          ${summaryCell('modules.lifetime_spend','<span data-team-lifetime-usd>—</span>')}
        </dl>
        ${manage ? `<div class="team-usage-mine"><h4>${tr('modules.my_usage')}</h4><dl class="team-usage-summary" data-team-mine aria-busy="true">
          ${summaryCell('modules.today_spend_limit','<span data-team-mine-today>—</span>')}
          ${summaryCell('modules.today_tokens','<span data-team-mine-today-tokens>—</span>')}
          ${summaryCell('modules.lifetime_tokens','<span data-team-mine-lifetime-tokens>—</span>')}
          ${summaryCell('modules.lifetime_spend','<span data-team-mine-lifetime-usd>—</span>')}
        </dl></div>` : ''}
        <div id="team_usage_detail" class="team-usage-detail" hidden><div class="team-usage-toolbar">${periodBar('today')}${manage ? `<div class="period-presets team-usage-scope" role="group" aria-label="${esc(tr('modules.usage_scope'))}"><button class="secondary active" type="button" data-team-scope="team" aria-pressed="true">${esc(tr('modules.scope_team'))}</button><button class="secondary" type="button" data-team-scope="mine" aria-pressed="false">${esc(tr('modules.scope_mine'))}</button></div>` : ''}</div><div data-team-breakdown aria-live="polite"></div></div>
      </section>
    </div>`, '', null, null, {back,reopen:() => open(back)});
    const home = () => open(back);
    const view = $('team_home');
    bindUsageDetails(view,member);
    view.querySelector('[data-team-profiles]').onclick = () => profiles.directory(home);
    if (vps) {
      view.querySelector('[data-team-backup]').onclick = event => downloadBackup(event.currentTarget);
    } else {
      const leave = async () => {
        current = await action('logout');
        changed();
        await onSync();
        if ($('team_leave_confirm')) join(back);
      };
      view.querySelector('[data-team-leave]').onclick = () => confirmLeave(home,false,leave);
      view.querySelector('[data-team-choice=publish]').onclick = () => publish(home);
      view.querySelector('[data-team-choice=sync]').onclick = event => sync(event.currentTarget);
    }
    if (manage) view.querySelector('[data-team-choice=manage]').onclick = () => openManagement(home);
    else view.querySelector('[data-team-subscriptions]').onclick = () => profiles.subscriptions(home);
    action('status').then(result => {
      const prior = scope();
      current = result;
      if (!current.joined) {
        changed();
        if (view.isConnected) join(back);
        return;
      }
      if (prior.role !== scope().role || prior.publish !== scope().publish || prior.sync !== scope().sync) {
        changed();
        if (view.isConnected) home();
      }
    }).catch(error => {
      if (!view.isConnected) return;
      if (error.status === 401 || error.payload?.error?.code === 'workspace_authentication_required') {
        current = {joined:false,base:current.base};
        changed();
        join(back, true);
      } else showError(view,error);
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
      // Administrators see the team totals above and their own share here, from the same per-member ledger.
      if (member.role !== 'member') {
        const [day, all] = await Promise.all([action('usage',{period:'today'}), action('usage',{period:'lifetime'})]);
        if (!view.isConnected) return;
        const mine = report => report.rows.find(row => row.member === member.id) || idle;
        const myDay = view.querySelector('[data-team-mine-today]');
        myDay.innerHTML = `${esc(money(spend(mine(day))))} / ${budgetDisplay(budget_nanos)}`;
        if (mine(day).unknown_cost_requests) myDay.title = tr('modules.unknown_count',{count:number(mine(day).unknown_cost_requests)});
        view.querySelector('[data-team-mine-today-tokens]').innerHTML = compact(mine(day));
        view.querySelector('[data-team-mine-lifetime-tokens]').innerHTML = compact(mine(all));
        view.querySelector('[data-team-mine-lifetime-usd]').innerHTML = compactSpend(mine(all));
      }
    } catch (error) {
      showError(view,error);
    } finally {
      view.querySelectorAll('.team-usage-summary').forEach(list => list.removeAttribute('aria-busy'));
    }
  }

  const idle = {requests:0,input_tokens:0,output_tokens:0,spend_nanos:0,unknown_cost_requests:0,models:[]};
  function usageFor(rows, member, only = false) {
    if (member.role === 'member' || only) return rows.find(row => row.member === member.id) || idle;
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
    const filters = [...detail.querySelectorAll('[data-team-period]')];
    const scopes = [...detail.querySelectorAll('[data-team-scope]')];
    let selected = 'today', mineOnly = false;
    scopes.forEach(control => control.onclick = () => {
      mineOnly = control.dataset.teamScope === 'mine';
      scopes.forEach(other => { const on = other === control; other.classList.toggle('active',on); other.setAttribute('aria-pressed',String(on)); });
      void load(selected);
    });
    async function load(period) {
      selected = period;
      pressPeriod(filters,period);
      data.textContent = tr('modules.loading');
      try {
        const report = await action('usage',{period});
        if (!data.isConnected || selected !== period) return;
        const row = usageFor(report.rows,member,mineOnly);
        data.innerHTML = row?.models.length ? modelUsage(row) : esc(tr('modules.no_usage'));
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
    view.querySelector('#team_usage_title').onclick = () => button.click();
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
      showError(button,error);
    } finally {
      button.disabled = false;
    }
  }

  async function confirmLeave(home, manage, leave) {
    const select = manage ? `<div class="team-handover"><label for="team_successor">${tr('modules.workspace_successor')}</label><select id="team_successor" disabled></select></div>` : '';
    const title = manage ? 'modules.workspace_handover' : 'modules.sign_out';
    openModal(tr(title), `<div id="team_leave_confirm"><p>${tr(manage ? 'modules.workspace_handover_required' : 'modules.leave_removes_credentials')}</p>${select}</div>`, tr(title), async () => {
      const successor = manage ? $('team_successor').value : '';
      if (manage && !successor) return;
      await leave(successor);
    }, null, {back:home,tone:'danger'});
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
      showError(input,error);
    }
  }

  // Members never need to know the workspace address: nearby discovery, a remembered workspace or a pasted
  // invitation supplies it. Typing it by hand stays available behind the settings button as a last resort.
  const rememberKey = 'emp.team.workspace';
  const remembered = () => { try { return JSON.parse(localStorage.getItem(rememberKey)) || {}; } catch { return {}; } };
  const remember = (base, name) => {
    if (!base) return;
    const prior = remembered();
    try { localStorage.setItem(rememberKey, JSON.stringify({base, name:name || (prior.base === base ? prior.name : '')})); } catch {}
  };
  const parseInvitation = text => {
    const code = text.match(/(?<!\d)\d{7}(?!\d)/)?.[0];
    const base = text.match(/https?:\/\/[^\s，。、」”)]+/)?.[0];
    const name = text.match(/[「“]([^」”]{1,80})[」”]/)?.[1];
    return {code, base, name};
  };

  function join(back, signingIn = false, preset = {}) {
    stopPending();
    const saved = remembered();
    const known = preset.base || current.base || saved.base || '';
    const knownName = preset.workspace || (known === saved.base ? saved.name : '') || current.identity?.workspace?.name || '';
    const field = (id, label, input, help = '') => `<div class="team-field"><div class="team-field-label"><label for="${id}">${label}</label>${help ? shadowHelp(help) : ''}</div>${input}</div>`;
    const iconButton = (id, icon, label, extra = '') => `<button type="button" class="secondary team-icon-button" id="${id}" data-icon="${icon}" title="${esc(label)}" aria-label="${esc(label)}" ${extra}></button>`;
    openModal(tr(signingIn ? 'modules.sign_in' : 'modules.join_workspace'), `<form id="team_login" class="team-form" novalidate>
      ${current.recovery_required ? `<p class="setting-help">${tr('modules.recover_team_services')}</p>` : ''}
      <div class="team-origin">
        <div class="team-origin-head"><span>${tr('modules.workspace')}</span>
          <span class="team-origin-tools">${iconButton('team_discover','refresh',tr('modules.find_nearby_workspace'))}${iconButton('team_origin_settings','settings',tr('modules.enter_address'),'aria-expanded="false" aria-controls="team_base_field"')}</span></div>
        <div id="team_nearby" class="team-origin-choices"></div>
        <p id="team_origin_status" class="team-origin-status" role="status" aria-live="polite"></p>
        <div id="team_base_field" class="team-base-field" hidden>
          <div class="team-field-label"><label for="team_base">${tr('modules.workspace_url')}</label>${shadowHelp(tr('modules.workspace_url_help'))}</div>
          <input id="team_base" type="url" inputmode="url" placeholder="https://workspace.example.org" value="${esc(known)}" required>
        </div>
      </div>
      ${signingIn ? '' : `<div class="team-form-pair">
        ${field('team_join_code',tr('modules.join_code'),'<input id="team_join_code" class="team-code-input" inputmode="numeric" pattern="[0-9]{7}" maxlength="7" autocomplete="one-time-code" placeholder="0000000" required>',tr('modules.join_code_help'))}
        ${field('team_nickname',tr('modules.nickname'),'<input id="team_nickname" autocomplete="nickname" maxlength="48" required>',tr('modules.nickname_help'))}
      </div>`}
      <div class="team-form-pair">
        ${field('team_name',tr('modules.username'),`<input id="team_name" autocomplete="username" value="${esc(preset.name || '')}" maxlength="64" required>`)}
        ${field('team_password',tr(signingIn ? 'modules.password' : 'modules.set_password'),
          signingIn ? '<input id="team_password" type="password" autocomplete="current-password" required>' : '<input id="team_password" type="password" minlength="12" maxlength="256" autocomplete="new-password" required>',
          signingIn ? '' : tr('modules.password_help'))}
      </div>
      <p class="team-form-switch">${tr(signingIn ? 'modules.no_account_yet' : 'modules.already_member')}<button type="button" id="team_switch_entry">${tr(signingIn ? 'modules.join_workspace' : 'modules.sign_in')}</button></p>
      <button type="submit" hidden></button>
    </form>`, tr(signingIn ? 'modules.sign_in' : 'modules.request_to_join'), async () => {
      const form = $('team_login');
      const invalid = [...form.querySelectorAll('input')].find(input => !input.checkValidity());
      if (invalid) {
        if (invalid.id === 'team_base') manual(true);
        invalid.reportValidity();
        return;
      }
      const account = {base:$('team_base').value.trim(),name:$('team_name').value.trim(),password:$('team_password').value};
      const nickname = signingIn ? preset.nickname : $('team_nickname').value.trim();
      const workspace = selectedName();
      try {
        if (signingIn) {
          current = await action('login', account);
          pendingAccount = null;
          remember(account.base, current.identity?.workspace?.name || workspace);
          changed();
          if (form.isConnected) open(back);
        } else {
          await action('join', {...account,nickname,code:$('team_join_code').value});
          remember(account.base, workspace);
          if (form.isConnected) pending(back, {...account,nickname,workspace});
        }
      } catch (error) {
        if (!form.isConnected) return;
        if (signingIn && error.payload?.error?.code === 'workspace_join_pending') return pending(back, {...account,nickname,workspace});
        $('modal_status').textContent = failure(error);
      }
    }, null, {back,reopen:() => join(back, signingIn, preset)});
    const form = $('team_login');
    const input = $('team_base');
    const baseField = $('team_base_field');
    const choices = $('team_nearby');
    const status = $('team_origin_status');
    const discoverButton = $('team_discover');
    const settingsButton = $('team_origin_settings');
    form.onsubmit = event => { event.preventDefault(); $('modal_submit').click(); };
    $('team_switch_entry').onclick = () => join(back, !signingIn, {base:input.value.trim(),workspace:selectedName(),name:$('team_name').value.trim()});
    function selectedName() {
      return choices.querySelector('[data-team-origin][aria-pressed=true]')?.dataset.teamName || '';
    }
    function manual(open) {
      baseField.hidden = !open;
      settingsButton.setAttribute('aria-expanded',String(open));
      if (open) status.textContent = '';
    }
    function render(workspaces, selected) {
      choices.innerHTML = workspaces.map(({name,base,source}) => `<button type="button" class="team-origin-choice" data-team-origin="${esc(base)}" data-team-name="${esc(name || '')}" aria-pressed="${base === selected}" title="${esc(base)}">
        <strong>${esc(name || tr('modules.workspace_unnamed'))}</strong><small>${tr('modules.workspace_source_' + source)}</small></button>`).join('');
      choices.querySelectorAll('[data-team-origin]').forEach(choice => choice.onclick = () => {
        choose(choice.dataset.teamOrigin);
        (signingIn ? $('team_name') : $('team_join_code')).focus();
      });
    }
    // Nothing nearby: one short line, with the invitation and manual-address routes behind the help mark.
    function missing() {
      status.innerHTML = `<span>${tr('modules.no_nearby_workspace')}</span>${shadowHelp(tr('modules.no_nearby_workspace_auto'))}`;
    }
    function choose(origin) {
      input.value = origin;
      choices.querySelectorAll('[data-team-origin]').forEach(choice => choice.setAttribute('aria-pressed',String(choice.dataset.teamOrigin === origin)));
      status.textContent = '';
    }
    async function discover(automatic) {
      status.dataset.busy = '';
      status.textContent = tr('modules.searching_nearby');
      discoverButton.disabled = true;
      try {
        const {workspaces} = await action('discover');
        if (!form.isConnected) return;
        if (!workspaces.length) {
          if (!choices.children.length) missing();
          else if (automatic) status.textContent = ''; else missing();
          return;
        }
        const keep = [...choices.querySelectorAll('[data-team-origin]')].map(choice => ({base:choice.dataset.teamOrigin,name:choice.dataset.teamName,source:'saved'}))
          .filter(item => !workspaces.some(found => found.base === item.base));
        render([...workspaces.map(item => ({...item,source:'nearby'})),...keep], input.value.trim());
        if (!automatic || !input.value.trim()) choose(workspaces[0].base);
        status.textContent = '';
      } catch (error) {
        if (!form.isConnected) return;
        if (choices.children.length && automatic) status.textContent = ''; else if (automatic) missing(); else status.textContent = failure(error);
      } finally {
        delete status.dataset.busy;
        discoverButton.disabled = false;
      }
    }
    discoverButton.onclick = () => discover(false);
    settingsButton.onclick = () => {
      const open = baseField.hidden;
      manual(open);
      if (open) input.focus();
    };
    input.oninput = () => choices.querySelectorAll('[data-team-origin]').forEach(choice => choice.setAttribute('aria-pressed',String(choice.dataset.teamOrigin === input.value.trim())));
    const code = $('team_join_code');
    if (code) code.onpaste = event => {
      const invitation = parseInvitation(event.clipboardData?.getData('text') || '');
      if (!invitation.code && !invitation.base) return;
      event.preventDefault();
      if (invitation.code) code.value = invitation.code;
      if (invitation.base) {
        render([{base:invitation.base,name:invitation.name,source:'invite'}], invitation.base);
        choose(invitation.base);
        manual(false);
      }
      $('team_nickname').focus();
    };
    if (known) {
      render([{base:known,name:knownName,source:'saved'}], known);
      if (!signingIn) void discover(true);
    } else void discover(true);
    (signingIn ? $('team_name') : code).focus({preventScroll:true});
  }

  // After a join request the member waits here; approval signs them in without retyping the password.
  function pending(back, account) {
    stopPending();
    pendingAccount = account;
    const signIn = () => join(back, true, {base:account.base,workspace:account.workspace,name:account.name,nickname:account.nickname});
    openModal(tr('modules.join_workspace'), `<div id="team_pending" class="team-pending" role="status">
      <span class="team-pending-spinner" aria-hidden="true"></span>
      <h3>${tr('modules.waiting_for_approval')}</h3>
      <p>${tr('modules.waiting_for_approval_detail',{name:`<span class="team-pending-name">${esc(account.nickname || account.name)}</span>`})}</p>
      <p class="setting-help">${tr('modules.waiting_auto_sign_in')}</p>
      <p class="team-form-switch"><button type="button" id="team_pending_cancel">${tr('modules.cancel_waiting')}</button></p>
    </div>`, '', null, null, {back,reopen:() => pending(back, account)});
    const view = $('team_pending');
    $('team_pending_cancel').onclick = () => { pendingAccount = null; signIn(); };
    const poll = async () => {
      if (!view.isConnected) return;
      try {
        const signedIn = await action('login', account);
        if (!view.isConnected) return;
        current = signedIn;
        pendingAccount = null;
        changed();
        open(back);
        notice(tr('modules.join_approved'));
      } catch (error) {
        if (!view.isConnected) return;
        const code = error.payload?.error?.code;
        if (code === 'workspace_join_pending' || code === 'workspace_login_limited' || !error.status || error.status >= 500) {
          pendingTick = setTimeout(poll, 5000);
        } else if (code === 'workspace_join_rejected') {
          pendingAccount = null;
          join(back, false, {base:account.base,workspace:account.workspace,name:account.name,nickname:account.nickname});
          notice(failure(error), true);
        } else {
          pendingAccount = null;
          signIn();
          $('modal_status').textContent = failure(error);
        }
      }
    };
    pendingTick = setTimeout(poll, 4000);
  }

  function stopPending() {
    clearTimeout(pendingTick);
  }

  function publish(home) {
    const native = getState().native_account;
    const ready = native?.auth_mode === 'subscription' && native.credential_set;
    const plan = native?.quota?.plan_type;
    const label = native?.quota?.account_label;
    openModal(tr('modules.upload_a_service'), `<div id="team_upload_form" class="team-form">
      <div class="team-upload"><span class="team-role-icon" aria-hidden="true">${roleSvg('member')}</span>
        <div><strong>Native · Codex</strong> ${planTag(plan)} ${shadowHelp(tr('modules.authorized_teammates_can_sync_native'))}${label ? `<p class="setting-help">${esc(label)}</p>` : ''}</div></div>
      ${ready ? '' : `<p class="team-form-warning">${tr('modules.workspace_subscription_sign_in_required')}</p>`}
      </div>`, tr('modules.upload'), async () => {
        const form = $('team_upload_form');
        try {
          const result = await action('publish');
          if (form.isConnected) {
            home();
            // Re-uploading an already approved account only refreshes its credentials, so members keep syncing it.
            notice(tr(result?.review_required === false ? 'modules.share_refreshed' : 'modules.uploaded_for_review'));
          }
        } catch (error) {
          showError(form,error);
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
        const message = tr('modules.synced_count',{count:number(result.synced)}) +
          (result.skipped_native ? ' ' + tr('modules.synced_skipped_native',{count:number(result.skipped_native)}) : '');
        notice(message);
      }
    } catch (error) {
      showError(button,error);
    } finally {
      button.textContent = label;
      button.disabled = false;
    }
  }

  function tokens(row) {
    const input = finite(row?.input_tokens), output = finite(row?.output_tokens);
    return input == null && output == null ? null : (input ?? 0) + (output ?? 0);
  }

  function tokenDetails(row) {
    const total = compactUsage(tokens(row));
    const cost = ` / ${compactSpend(row)}`;
    if (!row?.models?.length) return total.html + cost;
    const id = esc('team_models_' + row.member);
    const text = `${number(row.input_tokens)} / ${number(row.output_tokens)}`;
    return `<button type="button" class="secondary team-token-detail" data-shadow-help="" data-shadow-help-template="${id}" aria-label="${esc(tr('modules.view_model_usage') + ' · ' + total.title)}" aria-haspopup="dialog" aria-expanded="false" aria-controls="shadow_feedback">${esc(total.text)}${cost}</button>
      <template id="${id}"><strong>${tr('modules.view_model_usage')}</strong><p>${tr('modules.input_output')} · ${esc(text)}</p>${modelUsage(row)}</template>`;
  }

  function modelUsage(row) {
    const models = [...row.models].sort((left, right) => (tokens(right) ?? 0) - (tokens(left) ?? 0));
    const unknown = value => value ? `<small>${esc(tr('modules.unknown_count',{count:number(value)}))}</small>` : '';
    return `<dl class="team-model-usage">
      <dt class="team-model-head">${tr('modules.model')}</dt><dd class="team-model-head">Token</dd><dd class="team-model-head">${tr('modules.spend')}</dd>
      ${models.map(model => `<dt title="${esc(model.model)}">${esc(model.model)}</dt><dd>${esc(number(tokens(model)))}</dd><dd>${esc(money(spend(model)))}${unknown(model.unknown_cost_requests)}</dd>`).join('')}
      <dt class="team-breakdown-total">${tr('modules.total')}</dt><dd class="team-breakdown-total">${esc(number(tokens(row)))}</dd><dd class="team-breakdown-total">${esc(money(spend(row)))}</dd>
    </dl>`;
  }

  // Management is one dialog with tabs, so members, subscriptions and the log are one click apart.
  const managementTabs = [['members','data-team-manage-members','modules.manage_members'],
    ['subscriptions','data-team-manage-subscriptions','modules.manage_subscriptions'],
    ['audit','data-team-audit','modules.audit']];

  function openManagement(home, tab = 'members') {
    stopInvite();
    if (tab === 'subscriptions') return reviewServices(home);
    if (tab === 'audit') return openAudit(home);
    return openMembers(home);
  }

  function management(home, tab, body, reopen) {
    openModal(tr('modules.workspace_management'), `<div class="tabs" role="tablist" aria-label="${esc(tr('modules.workspace_management'))}">${managementTabs.map(([id,attribute,label]) =>
      `<button type="button" role="tab" ${attribute} aria-selected="${id === tab}" tabindex="${id === tab ? 0 : -1}">${esc(tr(label))}</button>`).join('')}</div>
      <div role="tabpanel">${body}</div>`, '', null, null, {back:home,reopen});
    const tabs = [...$('modal_body').querySelectorAll('.tabs [role=tab]')];
    tabs.forEach((button, index) => {
      const id = managementTabs[index][0];
      button.onclick = () => { if (id !== tab) openManagement(home, id); };
      button.onkeydown = event => {
        const step = {ArrowRight:1,ArrowLeft:-1}[event.key];
        if (!step) return;
        event.preventDefault();
        const next = managementTabs[(index + step + tabs.length) % tabs.length][0];
        openManagement(home, next);
        $('modal_body').querySelector(`.tabs [${managementTabs.find(([key]) => key === next)[1]}]`)?.focus();
      };
    });
  }

  function openMembers(home, initial = 'today') {
    let selected = initial, generation = 0, members;
    const reopen = () => openMembers(home,selected);
    management(home, 'members', `<div id="team_manage">
      <div class="team-members-toolbar">${periodBar(selected)}${scope().role === 'team_admin' ? `<button type="button" class="secondary" data-team-handover>${tr('modules.workspace_handover')}</button>` : ''}<button type="button" data-member-add>${tr('modules.add_member')}</button></div>
      <div data-team-usage-table>${tr('modules.loading')}</div></div>`, reopen);
    const panel = $('team_manage');
    const handover = panel.querySelector('[data-team-handover]');
    if (handover) handover.onclick = () => confirmLeave(reopen, true, async successor => {
      current = await action('handover',{successor});
      changed();
      await onSync();
      if ($('team_leave_confirm')) home();
    });
    // An island decision refreshes this table if it is still the open view.
    // A quiet refresh (a new request arrived) waits while the administrator is typing a limit.
    membersView = ({quiet = false} = {}) => { if (panel.isConnected && !(quiet && panel.contains(document.activeElement) && document.activeElement.matches('input,textarea'))) reopen(); };
    const table = panel.querySelector('[data-team-usage-table]');
    const periodButtons = [...panel.querySelectorAll('[data-team-period]')];
    const chip = (member, id, label, editable) => `<label class="team-chip"><input type="checkbox" data-team-permission="${id}" ${member[id] ? 'checked' : ''} ${editable ? '' : 'disabled'}>${esc(label)}</label>`;
    async function load(period) {
      const request = ++generation;
      selected = period;
      pressPeriod(periodButtons,period);
      table.setAttribute('aria-busy','true');
      try {
        const [list,report] = await Promise.all([members ?? action('list_members').then(reply => reply.members),action('usage',{period})]);
        if (!table.isConnected || request !== generation) return;
        const byId = new Map(report.rows.map(row => [row.member,row]));
        if (members) {
          for (const row of table.querySelectorAll('[data-team-member]')) {
            row.lastElementChild.innerHTML = tokenDetails(byId.get(row.dataset.teamMember));
          }
          return;
        }
        members = list;
        const columns = [tr('modules.member'),tr('modules.daily_usd_limit') + ' ' + shadowHelp(tr('modules.resets_daily_in_utc_budget_alerts')),
          tr('modules.permissions') + ' ' + shadowHelp(tr('modules.member_permissions_help')),tr('modules.account_enabled') + ' ' + shadowHelp(tr('modules.account_enabled_help')),tr('modules.token_spend')];
        const rows = members.map(member => {
          const name = member.nickname || member.name;
          const editable = member.can_manage && !member.pending;
          const editRole = editable && scope().role === 'vps_admin';
          const role = member.pending ? tr('modules.pending_approval') : roleName(member.role);
          const changeRole = editRole ? tr('modules.change_role') + ' · ' + role : role;
          const rolePicker = editRole
            ? `<select class="team-role-select" data-team-member-role title="${esc(changeRole)}" aria-label="${esc(tr('modules.change_role') + ' · ' + name)}">${['member','team_admin'].map(option => `<option value="${option}" ${option === member.role ? 'selected' : ''}>${roleName(option)}</option>`).join('')}</select>` : '';
          const identity = `<div class="team-member-name"><span class="team-role-icon team-role-${esc(member.role)}" title="${esc(changeRole)}" ${editRole ? 'data-editable-role' : `role="img" aria-label="${esc(role)}"`}>${roleSvg(member.role)}${rolePicker}</span><span>${esc(name)}</span></div>`;
          if (member.pending) {
            return `<tr data-team-member="${esc(member.id)}"><th scope="row">${identity}</th><td></td>
              <td><span class="team-pending-actions"><span>${tr('modules.join_request_label')}</span><button type="button" data-member-approve="${esc(member.id)}">${tr('modules.accept_member')}</button><button type="button" class="secondary" data-member-decline="${esc(member.id)}">${tr('modules.decline_member')}</button></span></td><td></td><td>—</td></tr>`;
          }
          const budget = member.can_budget
            ? `<label class="team-budget"><span aria-hidden="true">$</span><input data-member-budget type="number" min="0" max="1000000" step="any" value="${finite(member.budget_nanos) == null ? '' : esc(member.budget_nanos/1e9)}" placeholder="∞" title="${esc(tr('modules.budget_input_help'))}" aria-label="${esc(tr('modules.daily_usd_limit') + ' · ' + name)}"></label>`
            : budgetDisplay(member.budget_nanos);
          const reset = member.role !== 'member';
          const chips = [chip(member,'fast','Fast',editable),chip(member,'ultrafast','Ultrafast',editable),
            chip(member,'publish',tr('modules.upload'),editable),chip(member,'sync',tr('modules.sync'),editable),
            `<label class="team-chip team-chip-static" title="${esc(tr('modules.reset_policy'))}"><input type="checkbox" ${reset ? 'checked' : ''} disabled aria-label="${esc(tr('modules.reset_permission') + ' · ' + tr(reset ? 'modules.reset_can_use' : 'modules.reset_view_only'))}">${tr('modules.reset_short')}</label>`].join('');
          return `<tr data-team-member="${esc(member.id)}"><th scope="row">${identity}</th>
            <td>${budget}</td>
            <td><div class="team-chips">${chips}</div></td>
            <td><label class="team-switch" title="${esc(tr('modules.account_enabled_help'))}"><input type="checkbox" role="switch" data-team-permission="enabled" ${member.enabled ? 'checked' : ''} ${editable ? '' : 'disabled'} aria-label="${esc(tr('modules.account_enabled') + ' · ' + name)}"><span></span></label></td>
            <td>${tokenDetails(byId.get(member.id))}</td></tr>`;
        }).join('');
        table.innerHTML = `<div class="model-table-wrap"><table class="model-table team-usage-table">
          <thead><tr>${columns.map(label => `<th scope="col">${label}</th>`).join('')}</tr></thead>
          <tbody>${rows}</tbody></table></div>`;
      } catch (error) {
        if (table.isConnected && request === generation) {
          if (members) showError(table,error);
          else table.textContent = failure(error);
        }
      } finally {
        if (request === generation) table.removeAttribute('aria-busy');
      }
    }
    for (const button of periodButtons) button.onclick = () => load(button.dataset.teamPeriod);
    void load(selected);
    bindInvitation(panel.querySelector('[data-member-add]'));
    table.onclick = async event => {
      const button = event.target.closest('[data-member-approve],[data-member-decline]');
      const approve = button?.hasAttribute('data-member-approve');
      const member = members?.find(item => item.id === (approve ? button.dataset.memberApprove : button?.dataset.memberDecline));
      if (!member) return;
      button.disabled = true;
      try {
        if (approve) await action('member_access',{id:member.id,role:'member',publish:false,sync:true,enabled:true});
        else await action('reject_member',{id:member.id});
        answered.set(member.id, Date.now());
        notifications.withdraw(approvals.get(member.id));
        approvals.delete(member.id);
        if (panel.isConnected) reopen();
      } catch (error) {
        showError(panel,error);
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
        if (isBudget) control.value = finite(member.budget_nanos) == null ? '' : member.budget_nanos/1e9;
        else if (isRole) control.value = member.role;
        else control.checked = member[permission];
        showError(panel,error);
      } finally {
        controls.forEach(input => input.disabled = false);
      }
    };
  }

  function openAudit(home) {
    const columns = ['time','actor','action','target'].map(key => `<th scope="col">${tr('modules.audit_' + key)}</th>`).join('');
    management(home, 'audit', `<div id="team_audit"><div class="model-table-wrap"><table class="model-table team-audit-table" hidden><thead><tr>${columns}</tr></thead><tbody></tbody></table></div>
      <p class="setting-help" data-audit-status role="status"></p><button type="button" class="secondary" data-audit-more hidden>${tr('modules.audit_more')}</button></div>`, () => openAudit(home));
    const panel = $('team_audit');
    const table = panel.querySelector('table');
    const status = panel.querySelector('[data-audit-status]');
    const more = panel.querySelector('[data-audit-more]');
    const labels = {role:tr('modules.role'),publish:tr('modules.upload'),sync:tr('modules.sync'),enabled:tr('modules.account_enabled'),fast:'Fast',ultrafast:'Ultrafast'};
    const description = ({kind,details}) => {
      if (kind === 'share_review') return tr(details.shared ? 'modules.audit_share_started' : 'modules.audit_share_stopped');
      if (kind === 'share_refreshed') return tr(details.review_required === false ? 'modules.audit_share_refreshed' : 'modules.audit_share_resubmitted');
      if (kind === 'profile_visibility') return tr(details.visible ? 'modules.audit_profiles_visible' : 'modules.audit_profiles_private');
      const label = tr('modules.audit_' + kind);
      if (kind === 'budget') return label + ' · ' + (finite(details.daily_usd_nanos) == null ? tr('modules.no_daily_limit') : money(details.daily_usd_nanos));
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

  async function reviewServices(home) {
    management(home, 'subscriptions', `<div class="team-subscription-list" id="team_shared_services">${tr('modules.loading')}</div>`, () => reviewServices(home));
    const panel = $('team_shared_services');
    try {
      const {services} = await action('shared_services');
      if (!panel.isConnected) return;
      panel.innerHTML = services.length ? subscriptionHead('modules.action_column') + services.map(service => {
        const status = esc(tr(service.valid ? 'modules.share_valid' : 'modules.share_invalid'));
        const usable = service.shared || service.available;
        return `<div class="setting-row team-subscription-row">
        <div class="team-subscription-owner"><span class="team-subscription-dot" data-valid="${service.valid === true}" role="img" aria-label="${status}" title="${status}"></span><strong>${esc(service.owner)}</strong>${planTag(service.plan)}${service.shared ? '' : `<span class="team-pending-tag">${tr('modules.share_review')}</span>`}</div>
        <span class="team-subscription-usage" title="${esc(tr('modules.token_spend'))}">${compact(service.usage)} / ${compactSpend(service.usage)}</span>
        <button type="button" class="${service.shared ? 'secondary' : ''}" data-review-service="${esc(service.id)}" ${usable ? '' : 'disabled'}>${tr(service.shared ? 'modules.stop_sharing' : 'modules.share_service')}</button></div>`;
      }).join('')
        : `<p class="setting-help">${esc(tr('modules.no_uploaded_services'))}</p>`;
      panel.querySelectorAll('[data-review-service]').forEach(button => button.onclick = async () => {
        const service = services.find(row => row.id === button.dataset.reviewService);
        button.disabled = true;
        try {
          await action('review_service',{id:service.id,version:service.version,shared:!service.shared});
          if (panel.isConnected) reviewServices(home);
        } catch (error) {
          showError(panel,error);
          button.disabled = false;
        }
      });
    } catch (error) {
      if (panel.isConnected) panel.textContent = failure(error);
    }
  }

  // Core calls this whenever the dialog closes; both the invite countdown and approval polling stop with it.
  function stopInvite() {
    clearTimeout(inviteTick);
    stopPending();
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
          await navigator.clipboard.writeText(current.base ? tr('modules.invite_text',{name:current.identity?.workspace?.name || '',code:invite.code,base:current.base}) : invite.code);
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
        showError(button,error);
      } finally {
        button.disabled = false;
      }
    };
    button.disabled = true;
    action('join_code').then(show).catch(error => showError(button,error)).finally(() => {button.disabled = false;});
  }

  function dispose() {
    clearTimeout(approvalTimer);
    approvalTimer = null;
    clearApprovals();
  }

  return {open,refresh,stopInvite,watch,dispose};
}
