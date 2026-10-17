// Service presentation composed with the existing account, model and migration owners.
function shadowHelp(message, label = '?', tone = 'info') {
  return `<button type="button" class="shadow-help" data-shadow-tone="${tone}" data-shadow-help="${esc(message)}" aria-label="${esc(message)}">${label}</button>`;
}
function shadowCompactNumber(value, usd = false) {
  const n = Number(value), absolute = Math.abs(n);
  const unit = absolute >= 1e9 ? [1e9,'B'] : absolute >= 1e6 ? [1e6,'M'] : absolute >= 1e3 ? [1e3,'K'] : [1,''];
  return (usd ? 'US$' : '') + (n/unit[0]).toLocaleString('en-US',{maximumFractionDigits:2}) + unit[1];
}
function shadowAccountInfo(account, {alias, monthly, credit}) {
  const mail = account.quota?.account_label || tr("services.account_unavailable");
  const plan = subscriptionPlanLabel(account.quota?.plan_type);
  return `<div class="account-info-grid shadow-compact-account"><div><span class="account-info-label">${tr("services.name")}</span>${alias}</div><div><span class="account-info-label">${tr("services.account")}</span><div class="account-value-row"><span class="account-value">${esc(mail)}${plan ? ' · '+esc(plan) : ''}</span></div></div><div><span class="account-info-label">${tr("services.sign_in")}</span><span class="account-value">${account.native ? tr("services.local_sign_in") : tr("services.imported")}${!account.credential_set ? ' · '+tr("services.not_signed_in") : ''}</span></div></div>${typeof monthly === 'number' ? `<p class="muted">${tr("services.monthly_quota")} · ${esc(monthly)}%</p>` : ''}${credit?.spend_control_reached ? `<p class="refresh-error">${tr("services.spend_limit_reached")}</p>` : ''}<div id="account_usage" class="account-usage"><p class="muted">${tr("services.loading_usage")}</p></div>`;
}
function shadowServiceUsage(data, {aggregate, number, money, dateTime}) {
  const {models, totals} = aggregate(data.groups);
  const total = totals.input_tokens+totals.output_tokens;
  const usd = totals.cost_nanos/1e9;
  const rows = models.sort((a,b)=>a.model.localeCompare(b.model)).map(row=>`<tr><td>${esc(row.model)}</td><td>${row.input_reports ? number(row.input_tokens) : '—'}</td><td>${row.output_reports ? number(row.output_tokens) : '—'}</td><td>${row.priced_requests ? money(row.cost_nanos) : '—'}</td></tr>`).join('');
  return `<div class="shadow-usage-summary"><h3>${tr("services.lifetime_usage")}</h3><div class="usage-totals"><div class="usage-total"><span>${tr("common.tokens")}</span><strong tabindex="0" data-shadow-help="${esc(number(total))}">${totals.input_reports || totals.output_reports ? shadowCompactNumber(total) : '—'}</strong></div><div class="usage-total"><span>${tr("reports.api_estimate_usd")}</span><strong tabindex="0" data-shadow-help="${esc(money(totals.cost_nanos))}">${totals.priced_requests ? shadowCompactNumber(usd,true) : '—'}</strong></div></div></div><div class="usage-table"><table><thead><tr><th>${tr("reports.model")}</th><th>${tr("services.input")}</th><th>${tr("services.output")}</th><th>${tr("common.usd")}</th></tr></thead><tbody>${rows || `<tr><td colspan="4">${tr("services.no_usage_recorded")}</td></tr>`}</tbody></table></div><footer class="shadow-account-footnotes"><p>${number(totals.priced_requests)} / ${number(totals.requests)} ${tr("services.requests_priced")}${data.first_record_at ? ' · '+tr("services.recorded_since")+esc(dateTime(data.first_record_at)) : ''}</p>${totals.priced_requests < totals.requests ? `<p>${tr("services.some_model_prices_are_unavailable_the_estimate_includes_known")}</p>` : ''}</footer>`;
}
function shadowDetailActions(id, kind, account) {
  const nav = $('service_detail_nav');
  if (!nav) return;
  nav.querySelector('.shadow-detail-actions')?.remove();
  const actions=document.createElement('div');actions.className='shadow-detail-actions';
  if(kind==='account'){
    const button=document.createElement('button');button.type='button';button.className='secondary shadow-reset-action';
    button.dataset.uiAction='account-reset-credits';button.dataset.id=id;
    button.innerHTML=uiIcon('reset')+' '+tr("services.reset")+' · '+resetCreditCount(account);
    actions.append(button);
  }
  $('modal_body').querySelectorAll('[data-ui-action="account-remove"],[data-ui-action="provider-remove"]').forEach(button=>{const parent=button.parentElement;button.remove();if(parent.classList.contains('service-detail-actions')&&!parent.children.length)parent.remove();});
  if(!account.native){
    const button=document.createElement('button');button.type='button';button.className='danger';button.dataset.uiAction=kind==='account'?'account-remove':'provider-remove';button.dataset.id=id;
    button.dataset.icon='trash';button.textContent=tr("services.remove");actions.append(button);
  }
  nav.querySelector('.service-detail-tabs').after(actions);

}
function installServicePresentation() {
  document.body.dataset.shadowRefinements='true';
  const bubble=document.createElement('div');bubble.className='shadow-feedback';bubble.hidden=true;bubble.id='shadow_feedback';bubble.setAttribute('role','tooltip');document.body.append(bubble);
  let owner=null;
  function hide(){owner?.removeAttribute('aria-describedby');owner=null;bubble.hidden=true;}
  function show(node){if(!node)return;hide();owner=node;bubble.dataset.tone=node.dataset.shadowTone || 'info';bubble.textContent=node.dataset.shadowHelp;bubble.hidden=false;node.setAttribute('aria-describedby',bubble.id);const rect=node.getBoundingClientRect();const box=bubble.getBoundingClientRect();bubble.style.left=Math.max(12,Math.min(innerWidth-box.width-12,rect.left+rect.width/2-box.width/2))+'px';bubble.style.top=(rect.bottom+box.height+12<innerHeight ? rect.bottom+8 : Math.max(12,rect.top-box.height-8))+'px';}
  document.addEventListener('pointerover',event=>show(event.target.closest('[data-shadow-help]')));
  document.addEventListener('pointerout',event=>{if(owner?.contains(event.target)&&!owner.contains(event.relatedTarget))hide();});
  document.addEventListener('focusin',event=>show(event.target.closest('[data-shadow-help]')));
  document.addEventListener('focusout',hide);
  document.addEventListener('click',event=>{if(!event.target.closest('[data-shadow-help]'))hide();},true);
  document.addEventListener('keydown',event=>{if(event.key==='Escape')hide();});window.addEventListener('scroll',hide,true);
  return {hide};
}
