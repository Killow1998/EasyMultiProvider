function createCpaQuotas({getState,api,esc,tr,notice,render,meters,resetText,quotaDisplay}) {
  const busy=new Set(),record=id=>getState()?.provider_quotas?.[id];
  let timer=null;
  function markup(id) {
    const entry=record(id);
    if(!entry?.refreshable)return '';
    const observed=entry.observation,error=observed?.error_code;
    if(error==='cpa_quota_unsupported')return '';
    if(error)return `<div class="service-quota muted">${esc(tr('services.'+error))}</div>`;
    const quota=observed?.quota;
    if(!quota)return '';
    const accounts=quota?.accounts || [];
    const names=['five_hour','seven_day'];
    const cell=(account,name)=>{
      if(!account.enabled)return tr('services.cpa_quota_disabled');
      const window=account.windows[name];
      if(!window || window.remaining_percent===null)return tr('services.cpa_quota_unknown');
      const value=quotaDisplay.value(window.remaining_percent)+'%';
      return value+(resetText(window.resets_at)?' · '+resetText(window.resets_at):'')+(account.stale?' · '+tr('services.cpa_quota_stale'):'');
    };
    const template='cpa-quota-'+encodeURIComponent(id);
    const rows=accounts.map(account=>`<tr><td><code>${esc(account.id)}</code></td>${names.map(name=>`<td>${esc(cell(account,name))}</td>`).join('')}</tr>`).join('');
    const headings=names.map(name=>`<th>${esc(name.replace(/^five_hour/,'5h').replace(/^seven_day/,'7d').replaceAll('_',' '))}</th>`).join('');
    return `<div class="service-quota" tabindex="0" data-shadow-help="" data-shadow-help-template="${esc(template)}" aria-label="${esc(tr('services.cpa_quota_accounts'))}">${meters({quota,quota_pending:true},busy.has(id))}<template id="${esc(template)}"><table><thead><tr><th>${tr('services.account')}</th>${headings}</tr></thead><tbody>${rows || `<tr><td colspan="${names.length+1}">${tr('services.cpa_quota_unknown')}</td></tr>`}</tbody></table>${quota?.checked_at?`<p class="muted">${tr('services.updated')}${esc(new Date(quota.checked_at).toLocaleString())}</p>`:''}</template></div>`;
  }
  async function refresh(id,announce=true,force=true) {
    if(busy.has(id) || !record(id)?.refreshable)return false;
    busy.add(id);render();
    try {
      const result=await api('/api/providers/'+encodeURIComponent(id)+'/quota',{method:'POST',body:JSON.stringify({force})});
      if(record(id)?.identity!==result.identity)return false;
      record(id).observation=result.observation;
      const error=result.observation.error_code;
      if(announce)notice(error?tr('services.'+error):tr('services.quota_updated'),Boolean(error));
      return !error;
    } catch(error) {
      const entry=record(id);
      if(entry)entry.observation={observed_at:Date.now()/1000,quota:null,error_code:'cpa_quota_connection_failed'};
      if(announce)notice(error.message,true);
      return false;
    } finally {busy.delete(id);render();}
  }
  function sync() {
    clearTimeout(timer);
    if(document.hidden)return;
    for(const [id,entry] of Object.entries(getState()?.provider_quotas || {})) {
      if(entry.refreshable && !busy.has(id) && Date.now()/1000-(entry.observation?.observed_at || 0)>=300)void refresh(id,false,false);
    }
    timer=setTimeout(sync,60000);
  }
  document.addEventListener('visibilitychange',sync);
  window.addEventListener('pagehide',()=>clearTimeout(timer));
  return {markup,refresh,sync,busy,has:id=>Boolean(record(id)),canRefresh:id=>Boolean(record(id)?.refreshable),canViewHistory:id=>Boolean(record(id)?.observation) && record(id).observation.error_code!=='cpa_quota_unsupported'};
}
