function createTeamProfiles({action, openModal, $, esc, tr, scope, number, tokens, roleName, date, compact, compactSpend}) {
  const error = (panel, reason) => {
    if (panel.isConnected) $('modal_status').textContent = reason.message;
  };

  async function directory(back) {
    const reopen = () => directory(back);
    openModal(tr('modules.member_profiles'), '<div id="team_profiles" class="settings-list" aria-live="polite"></div>', '', null, null, {back,reopen});
    const panel = $('team_profiles');
    try {
      const data = await action('profiles');
      if (!panel.isConnected) return;
      panel.innerHTML = `<div class="setting-row"><span>${tr('modules.workspace_created')}</span><strong>${date(data.created_at)}</strong></div>
        ${data.can_manage ? `<label class="setting-row"><span>${tr('modules.profiles_visible')}</span><input type="checkbox" data-profiles-visible ${data.profiles_visible ? 'checked' : ''}></label>` : ''}
        <div class="list-rows">${data.members.map(member => `<button type="button" class="choice team-profile-link" data-team-profile-member="${esc(member.member)}">
          <strong>${esc(member.name)}${member.member === scope().id ? ` · ${tr('modules.my_profile')}` : ''}</strong>
          <small>${roleName(member.role)} · ${tr('modules.joined_at')} ${date(member.joined_at)}</small></button>`).join('')}</div>`;
      const toggle = panel.querySelector('[data-profiles-visible]');
      if (toggle) toggle.onchange = async () => {
        const visible = toggle.checked;
        toggle.disabled = true;
        try { await action('profile_visibility',{visible}); }
        catch (reason) { toggle.checked = !visible; error(panel,reason); }
        finally { toggle.disabled = false; }
      };
      panel.querySelectorAll('[data-team-profile-member]').forEach(button => button.onclick = () => profile(button.dataset.teamProfileMember,reopen));
    } catch (reason) { error(panel,reason); }
  }

  async function profile(member, back) {
    const reopen = () => profile(member,back);
    openModal(tr('modules.member_profile'), '<div id="team_profile" class="settings-list" aria-live="polite"></div>', '', null, null, {back,reopen});
    const panel = $('team_profile');
    try {
      const {name,joined_at,created_at,lifetime,activity,day} = await action('profile',{member});
      if (!panel.isConnected) return;
      const rows = new Map(activity.map(row => [row.day,row]));
      const values = Array.from({length:30},(_,index) => day-(29-index)*86400);
      const peak = Math.max(1,...activity.map(row => tokens(row) || 0));
      const html = values.map(value => {
        const row = rows.get(value), used = tokens(row);
        const label = date(value) + ' · ' + (row ? number(used ?? 0) : tr('modules.activity_offline'));
        return `<div role="listitem" class="${row ? 'seen' : ''}" title="${esc(label)}" aria-label="${esc(label)}"><i style="--bar-height:${Math.max(3,(used || 0)*100/peak)}%"></i></div>`;
      }).join('');
      panel.innerHTML = `<div class="setting-row"><strong>${esc(name)}</strong>
        <span class="setting-help">${tr('modules.joined_at')} ${date(joined_at)} · ${tr('modules.workspace_created')} ${date(created_at)}</span></div>
        <div class="team-profile-metrics">
          <div><small>${tr('modules.lifetime_tokens')}</small><strong title="${esc(number(tokens(lifetime)))}">${compactUsage(tokens(lifetime)).html}</strong></div>
          <div><small>${tr('modules.lifetime_spend')}</small><strong>${compactSpend(lifetime)}</strong></div>
        </div>
        ${lifetime.unknown_cost_requests ? `<div class="setting-help">${tr('modules.unknown_count',{count:lifetime.unknown_cost_requests})}</div>` : ''}
        <div><strong>${tr('modules.token_activity')}</strong>
        <div class="team-token-activity" role="list" aria-label="${esc(tr('modules.token_activity'))}">${html}</div>
        <div class="team-activity-axis setting-help"><span>${date(values[0])}</span><span>${date(day)}</span></div></div>`;
    } catch (reason) { error(panel,reason); }
  }

  async function subscriptions(back) {
    const reopen = () => subscriptions(back);
    openModal(tr('modules.workspace_subscriptions'), '<div id="team_subscriptions" class="settings-list" aria-live="polite"></div>', '', null, null, {back,reopen});
    const panel = $('team_subscriptions');
    try {
      const data = await action('subscriptions');
      if (!panel.isConnected) return;
      panel.innerHTML = data.subscriptions.length ? data.subscriptions.map(row => `<div class="setting-row team-subscription-row" data-workspace-subscription="${esc(row.share)}">
        <div class="team-subscription-owner"><strong>${esc(row.owner)}</strong>${row.plan ? `<span class="pill">${esc(subscriptionPlanLabel(row.plan))}</span>` : ''}</div>
        <span class="setting-help">${tr(row.status === 'review' ? 'modules.service_pending_review' : 'modules.service_shared')}</span>
        <span class="team-subscription-usage">${compact(row.usage)} / ${compactSpend(row.usage)}</span>
        <span class="team-subscription-status" data-valid="${row.status === 'available'}" data-status="${esc(row.status)}">${tr(row.status === 'available' ? 'modules.share_valid' : row.status === 'review' ? 'modules.share_review' : 'modules.share_invalid')}</span></div>`).join('')
        : tr('modules.no_workspace_subscriptions');
    } catch (reason) { error(panel,reason); }
  }

  return {directory, subscriptions};
}
