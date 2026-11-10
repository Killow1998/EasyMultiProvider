// Service presentation composed with the existing account, model and migration owners.
function shadowHelp(message, label = '?', tone = 'info') {
  return `<button type="button" class="shadow-help" data-shadow-tone="${tone}" data-shadow-help="${esc(message)}" aria-label="${esc(message)}">${label}</button>`;
}
function shadowCompactNumber(value, usd = false) {
  const n = Number(value), absolute = Math.abs(n);
  const unit = absolute >= 1e9 ? [1e9,'b'] : absolute >= 1e6 ? [1e6,'m'] : absolute >= 1e3 ? [1e3,'k'] : [1,''];
  return (usd ? 'US$' : '') + (n/unit[0]).toLocaleString('en-US',{maximumFractionDigits:usd && absolute<1 ? 9 : 2}) + unit[1];
}
function compactUsage(value, usd = false, hint = '') {
  if (value === null || value === undefined) return {text:'—',title:'',html:'—'};
  const text = shadowCompactNumber(value,usd);
  const title = (usd ? 'US$' : '') + value.toLocaleString('en-US',{maximumFractionDigits:9});
  return {text,title,html:`<span title="${esc(hint ? title+'\n'+hint : title)}">${text}</span>`};
}
function renderAccountInfo(account) {
  const name = account.native ? 'Native' : account.name || account.id;
  const alias = account.native ? `<div class="account-alias-edit">${tr("common.native")}</div>` : `<div class="account-alias-edit"><input id="account_alias" value="${esc(name)}" aria-label="${esc(tr("services.name"))}"><button type="button" class="secondary" id="account_alias_save" data-ui-action="account-alias-save" data-id="${esc(account.id)}" title="${esc(tr("services.save_name"))}" aria-label="${esc(tr("services.save_name"))}">${uiIcon('save')}</button></div>`;
  const credit = account.quota?.credits, monthly = credit?.individual_limit?.remaining_percent;
  const mail = account.quota?.account_label || tr(account.auth_mode==='api_key'?'login.api_key':account.auth_mode==='local_key'?'login.local_key':'services.account_unavailable');
  const plan = subscriptionPlanLabel(account.quota?.plan_type);
  return `<div class="account-info-grid shadow-compact-account"><div><span class="account-info-label">${tr("services.name")}</span>${alias}</div><div><span class="account-info-label">${tr("services.account")}</span><div class="account-value-row"><span class="account-value">${esc(mail)}${plan ? ' · '+esc(plan) : ''}</span></div></div><div class="service-detail-actions"></div></div>${typeof monthly === 'number' ? `<p class="muted">${tr("services.monthly_quota")} · ${esc(monthly)}%</p>` : ''}${credit?.spend_control_reached ? `<p class="refresh-error">${tr("services.spend_limit_reached")}</p>` : ''}<div id="account_usage" class="account-usage"><p class="muted">${tr("services.loading_usage")}</p></div>`;
}
function renderLifetimeUsage(data, {aggregate, number, dateTime}) {
  const {models, totals} = aggregate(data.groups);
  const values=createUsageValues({tr,esc,number});
  for(const row of [...models,totals])row.unpriced_requests=row.requests-row.priced_requests;
  const rows = models.sort((a,b)=>a.model.localeCompare(b.model)).map(row=>`<tr><td>${esc(row.model)}</td><td>${values.count(row,'input_tokens','input_reports')}</td><td>${values.count(row,'output_tokens','output_reports')}</td><td>${values.usd(row)}</td></tr>`).join('');
  return `<div class="shadow-usage-summary"><h3>${tr("services.lifetime_usage")}</h3><div class="usage-totals"><div class="usage-total"><span>${tr("common.tokens")}${totals.reported_requests<totals.requests?' · '+tr("services.partial_usage"):''}</span><strong>${values.tokens(totals)}</strong></div><div class="usage-total"><span>${tr("reports.api_estimate_usd")}${totals.priced_requests<totals.requests?' · '+tr("services.partial_estimate"):''}</span><strong>${values.usd(totals)}</strong></div></div></div><div class="usage-table"><table><thead><tr><th>${tr("reports.model")}</th><th>${tr("services.input")}</th><th>${tr("services.output")}</th><th>${tr("common.usd")}</th></tr></thead><tbody>${rows || `<tr><td colspan="4">${tr("services.no_usage_recorded")}</td></tr>`}</tbody></table></div><footer class="shadow-account-footnotes"><p>${number(totals.priced_requests)} / ${number(totals.requests)} ${tr("services.requests_priced")}${data.first_record_at ? ' · '+tr("services.recorded_since")+esc(dateTime(data.first_record_at)) : ''}</p>${totals.priced_requests < totals.requests ? `<p>${tr("services.some_model_prices_are_unavailable_the_estimate_includes_known")}</p>` : ''}</footer>`;
}
function mountServiceActions(id, kind, account) {
  const actions = $('modal_body').querySelector('.service-detail-actions');
  if (!actions) return;
  actions.replaceChildren();
  if(kind==='account' && (!account.native || account.auth_mode==='subscription')){
    const button=document.createElement('button');button.type='button';button.className='secondary shadow-reset-action';
    button.dataset.uiAction='account-reset-credits';button.dataset.id=id;
    button.innerHTML=uiIcon('reset')+' '+tr("services.reset")+' · '+resetCreditCount(account);
    actions.append(button);
  }
  if(!account.native){
    if(kind==='provider'){
      const edit=document.createElement('button');edit.type='button';edit.className='secondary';
      edit.dataset.uiAction='provider-edit';edit.dataset.id=id;edit.dataset.icon='settings';
      edit.ariaLabel=edit.title=tr('services.connection_settings');actions.append(edit);
    }
    const button=document.createElement('button');button.type='button';button.className='danger';button.dataset.uiAction=kind==='account'?'account-remove':'provider-remove';button.dataset.id=id;
    button.dataset.icon='trash';button.textContent=tr("services.remove");actions.append(button);
  }
}
function installServicePresentation() {
  document.body.dataset.shadowRefinements='true';
  const bubble=document.createElement('div');bubble.className='shadow-feedback';bubble.hidden=true;bubble.id='shadow_feedback';bubble.setAttribute('role','tooltip');document.body.append(bubble);
  let owner=null, leaveTimer=null;
  function hide(){clearTimeout(leaveTimer);owner?.removeAttribute('aria-describedby');if(owner?.hasAttribute('aria-expanded'))owner.setAttribute('aria-expanded','false');owner=null;bubble.hidden=true;}
  function position(){
    if(!owner||bubble.hidden)return;
    const rect=owner.getBoundingClientRect();const box=bubble.getBoundingClientRect();
    bubble.style.left=Math.max(12,Math.min(innerWidth-box.width-12,rect.left+rect.width/2-box.width/2))+'px';
    bubble.style.top=(rect.bottom+box.height+12<innerHeight ? rect.bottom+8 : Math.max(12,rect.top-box.height-8))+'px';
  }
  function show(node){
    if(!node)return;
    hide();owner=node;bubble.dataset.tone=node.dataset.shadowTone || 'info';
    const template=document.getElementById(node.dataset.shadowHelpTemplate);
    const rich=template instanceof HTMLTemplateElement;
    bubble.classList.toggle('shadow-feedback-table',rich);
    bubble.setAttribute('role',rich?'dialog':'tooltip');
    if(rich){bubble.replaceChildren(template.content.cloneNode(true));bubble.setAttribute('aria-label',node.getAttribute('aria-label'));node.setAttribute('aria-expanded','true');}
    else {bubble.textContent=node.dataset.shadowHelp;bubble.removeAttribute('aria-label');node.setAttribute('aria-describedby',bubble.id);}
    bubble.hidden=false;bubble.scrollTop=0;
    position();
  }
  bubble.addEventListener('toggle',position,true);
  document.addEventListener('pointerover',event=>show(event.target.closest('[data-shadow-help]')));
  document.addEventListener('pointerout',event=>{if(owner?.contains(event.target)||bubble.contains(event.target)){clearTimeout(leaveTimer);leaveTimer=setTimeout(()=>{if(!owner?.matches(':hover')&&!bubble.matches(':hover')&&document.activeElement!==owner&&!bubble.contains(document.activeElement))hide();},120);}});
  document.addEventListener('focusin',event=>show(event.target.closest('[data-shadow-help]')));
  document.addEventListener('focusout',event=>{if(!owner?.contains(event.relatedTarget)&&!bubble.contains(event.relatedTarget))hide();});
  document.addEventListener('click',event=>{if(!event.target.closest('[data-shadow-help]')&&!bubble.contains(event.target))hide();},true);
  document.addEventListener('keydown',event=>{if(event.key==='Escape'&&!bubble.hidden){if(bubble.contains(document.activeElement))owner?.focus();hide();event.stopImmediatePropagation();}},true);
  window.addEventListener('scroll',event=>{
    if(bubble.contains(event.target))return;
    // Focusing an off-screen help button scrolls its dialog after focusin.
    if(owner?.contains(document.activeElement)||bubble.contains(document.activeElement))position();
    else hide();
  },true);
  return {hide};
}
