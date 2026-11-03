function createProviderBalances({getState,api,esc,tr,notice,render}) {
  const busy=new Set();
  const record=id=>getState().provider_balances?.[id];
  const errorText=code=>tr('services.'+code);
  function description(entry) {
    const observed=entry.observation;
    if(observed?.error_code)return errorText(observed.error_code);
    return tr(entry.refreshable?'services.account_balance':'services.balance_in_console');
  }
  function value(entry) {
    const balance=entry.observation?.balance;
    return (balance?.amounts || []).map(({currency,available})=>new Intl.NumberFormat(document.documentElement.lang,{style:'currency',currency,maximumFractionDigits:2}).format(available)).join(' · ');
  }
  function segment(id) {
    const entry=record(id);
    if(!entry)return '';
    const text=value(entry),title=text?description(entry):entry.observation?description(entry):tr(entry.refreshable?'services.balance_loading':'services.balance_in_console');
    const label=entry.observation?.error_code?'services.balance_unavailable':!entry.refreshable?'services.provider_billing':'services.balance';
    return `<span class="account-segment account-credit service-balance" title="${esc(title)}"><span class="credit-mark">C</span><strong>${esc(text || tr(label))}</strong></span>`;
  }
  function details(id) {
    const entry=record(id);
    if(!entry)return '';
    const observed=entry.observation,amount=value(entry);
    return `<div class="provider-balance-details"><div><strong>${esc(description(entry))}${amount?' · '+esc(amount):''}</strong>${observed?`<small class="muted">${esc(new Date(observed.observed_at*1000).toLocaleString())}</small>`:''}</div><a class="button secondary" href="${esc(entry.billing_url)}" target="_blank" rel="noopener noreferrer">${tr('services.official_billing')} ↗</a></div>`;
  }
  async function refresh(id,announce=true,force=true) {
    if(busy.has(id))return false;
    const entry=record(id);
    if(!entry?.refreshable)return false;
    busy.add(id);render();
    try {
      const result=await api('/api/providers/'+encodeURIComponent(id)+'/balance',{method:'POST',body:JSON.stringify({force})});
      if(record(id)?.identity!==result.identity)return false;
      record(id).observation=result.observation;
      const failed=result.observation.error_code;
      if(announce)notice(failed?errorText(failed):tr('services.balance_refreshed'),Boolean(failed));
      return !failed;
    } catch(error) {
      if(announce)notice(error.message,true);
      return false;
    } finally {busy.delete(id);render();}
  }
  function sync() {
    for(const [id,entry] of Object.entries(getState().provider_balances || {})) {
      if(entry.refreshable && !busy.has(id) && Date.now()/1000-(entry.observation?.observed_at || 0)>=60)void refresh(id,false,false);
    }
  }
  return {segment,details,refresh,sync,busy,canRefresh:id=>Boolean(record(id)?.refreshable)};
}
