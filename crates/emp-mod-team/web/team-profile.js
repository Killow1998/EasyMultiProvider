function createTeamProfiles({action, openModal, $, esc, tr, scope, number, tokens, roleName, roleBadge, date, compact, compactSpend, planTag, failure, subscriptionHead}) {
  const error = (panel, reason) => {
    if (panel.isConnected) $('modal_status').textContent = failure(reason);
  };

  async function directory(back) {
    const reopen = () => directory(back);
    openModal(tr('modules.member_profiles'), `<div id="team_profiles" class="team-profiles" aria-live="polite">${tr('modules.loading')}</div>`, '', null, null, {back,reopen});
    const panel = $('team_profiles');
    try {
      const data = await action('profiles');
      if (!panel.isConnected) return;
      const self = scope().id;
      const members = [...data.members].sort((left, right) => (right.member === self) - (left.member === self));
      panel.innerHTML = `${data.can_manage ? `<div class="setting-row team-profile-visibility">
          <div><strong>${tr('modules.profiles_visible')}</strong></div>
          <label class="team-switch"><input type="checkbox" role="switch" data-profiles-visible ${data.profiles_visible ? 'checked' : ''} aria-label="${esc(tr('modules.profiles_visible'))}"><span></span></label></div>` : ''}
        <div class="team-profile-list">${members.map(member => `<button type="button" class="team-profile-link" data-team-profile-member="${esc(member.member)}">
          ${roleBadge(member.role)}
          <span><strong>${esc(member.name)}</strong>${member.member === self ? `<span class="team-self-tag">${tr('modules.my_profile')}</span>` : ''}
          <small>${roleName(member.role)} · ${tr('modules.joined_at')} ${date(member.joined_at)}</small></span></button>`).join('')}</div>`;
      const toggle = panel.querySelector('[data-profiles-visible]');
      if (toggle) toggle.onchange = async () => {
        const visible = toggle.checked;
        toggle.disabled = true;
        try { await action('profile_visibility',{visible}); }
        catch (reason) { toggle.checked = !visible; error(panel,reason); }
        finally { toggle.disabled = false; }
      };
      panel.querySelectorAll('[data-team-profile-member]').forEach(button => button.onclick = () => profile(button.dataset.teamProfileMember,reopen,members.find(row => row.member === button.dataset.teamProfileMember)?.role));
    } catch (reason) {
      if (panel.isConnected) panel.textContent = failure(reason);
    }
  }

  async function profile(member, back, role) {
    const reopen = () => profile(member,back,role);
    openModal(tr('modules.member_profile'), `<div id="team_profile" class="team-profile" aria-live="polite">${tr('modules.loading')}</div>`, '', null, null, {back,reopen});
    const panel = $('team_profile');
    try {
      const {name,joined_at,lifetime,activity,day} = await action('profile',{member});
      if (!panel.isConnected) return;
      const rows = new Map(activity.map(row => [row.day,row]));
      const values = Array.from({length:30},(_,index) => day-(29-index)*86400);
      const peak = Math.max(1,...activity.map(row => tokens(row) || 0));
      const html = values.map(value => {
        const row = rows.get(value), used = tokens(row);
        const label = date(value) + ' · ' + (row ? number(used ?? 0) : tr('modules.activity_offline'));
        return `<div role="listitem" class="${row ? 'seen' : ''}" title="${esc(label)}" aria-label="${esc(label)}"><i style="--bar-height:${Math.max(3,(used || 0)*100/peak)}%"></i></div>`;
      }).join('');
      panel.innerHTML = `<div class="team-identity">
          ${role ? roleBadge(role) : ''}
          <div><h3>${esc(name)}</h3><div class="setting-help">${role ? esc(roleName(role)) + ' · ' : ''}${tr('modules.joined_at')} ${date(joined_at)}</div></div>
        </div>
        <dl class="team-profile-metrics">
          <div><dt>${tr('modules.lifetime_tokens')}</dt><dd title="${esc(number(tokens(lifetime)))}">${compact(lifetime)}</dd></div>
          <div><dt>${tr('modules.lifetime_spend')}</dt><dd>${compactSpend(lifetime)}</dd></div>
        </dl>
        ${lifetime.unknown_cost_requests ? `<p class="setting-help">${tr('modules.unknown_count',{count:number(lifetime.unknown_cost_requests)})}</p>` : ''}
        <section class="team-card"><div class="team-card-heading"><h3>${tr('modules.token_activity')}</h3></div>
          <div class="team-token-activity" role="list" aria-label="${esc(tr('modules.token_activity'))}">${html}</div>
          <div class="team-activity-axis setting-help"><span>${date(values[0])}</span><span>${date(day)}</span></div></section>`;
    } catch (reason) {
      if (panel.isConnected) panel.textContent = failure(reason);
    }
  }

  async function subscriptions(back) {
    const reopen = () => subscriptions(back);
    openModal(tr('modules.workspace_subscriptions'), `<div id="team_subscriptions" class="team-subscription-list" aria-live="polite">${tr('modules.loading')}</div>`, '', null, null, {back,reopen});
    const panel = $('team_subscriptions');
    try {
      const data = await action('subscriptions');
      if (!panel.isConnected) return;
      const state = row => row.status === 'available' ? 'share_valid' : row.status === 'review' ? 'share_review' : 'share_invalid';
      panel.innerHTML = data.subscriptions.length ? subscriptionHead('modules.status_column') + data.subscriptions.map(row => `<div class="setting-row team-subscription-row" data-workspace-subscription="${esc(row.share)}">
        <div class="team-subscription-owner"><span class="team-subscription-dot" data-valid="${row.status === 'available'}" aria-hidden="true"></span><strong>${esc(row.owner)}</strong>${planTag(row.plan)}</div>
        <span class="team-subscription-usage" title="${esc(tr('modules.token_spend'))}">${compact(row.usage)} / ${compactSpend(row.usage)}</span>
        <span class="team-subscription-status" data-valid="${row.status === 'available'}" data-status="${esc(row.status)}">${tr('modules.' + state(row))}</span></div>`).join('')
        : `<p class="setting-help">${tr('modules.no_workspace_subscriptions')}</p>`;
    } catch (reason) {
      if (panel.isConnected) panel.textContent = failure(reason);
    }
  }

  return {directory, subscriptions};
}
