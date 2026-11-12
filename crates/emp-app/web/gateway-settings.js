function createGatewaySettings({api, openModal, notice, $, esc, tr, modelName, serviceName, help}) {
  let currentRead;
  async function read(path) {
    const owner=$('modal_body').firstElementChild, request=currentRead={};
    try {
      const result=await api(path);
      return currentRead===request && owner.isConnected ? result : null;
    } catch(error) {
      if(currentRead===request && owner.isConnected)notice(error.message,true);
      return null;
    }
  }
  async function open(back) {
    const connection = await read('/api/gateway');
    if(!connection)return;
    openModal(tr('core.gateway_connection'), `<div class="routing-settings">
      <div class="setting-row routing-mode"><div class="collaboration-title"><label class="setting-title" for="gateway_base_url">${tr('core.gateway_base_url')}</label>${help(tr('core.gateway_forwarding_hint') + '\n\n' + tr('gateway.clients_help') + '\n\n' + tr('core.gateway_jev_hint'))}</div><input id="gateway_base_url" readonly value="${esc(location.origin + '/v1')}"></div>
      <div class="setting-row routing-mode"><label class="setting-title" for="gateway_key">${tr('core.gateway_key')}</label><div class="input-action"><input id="gateway_key" type="password" autocomplete="off" readonly value="${esc(connection.api_key)}"><button type="button" class="secondary" data-icon="copy" data-gateway-copy title="${esc(tr('core.gateway_copy_key'))}" aria-label="${esc(tr('core.gateway_copy_key'))}"></button></div></div>
    </div><div class="choice-grid gateway-choices"><button type="button" class="choice" data-icon="chevron-right" data-gateway-devices><strong>${tr('gateway.devices')}</strong></button><button type="button" class="choice" data-icon="chevron-right" data-gateway-usage><strong>${tr('gateway.usage')}</strong></button></div>`, '', null, null, {back, reopen:() => open(back)});
    $('modal_body').querySelector('[data-gateway-copy]').onclick = () => navigator.clipboard.writeText($('gateway_key').value).then(() => notice(tr('core.gateway_key_copied')), error => notice(error.message, true));
    $('modal_body').querySelector('[data-gateway-devices]').onclick = () => devices(() => open(back));
    $('modal_body').querySelector('[data-gateway-usage]').onclick = () => usage(() => open(back));
  }
  async function devices(back) {
    const clients = await read('/api/gateway/clients');
    if(!clients)return;
    openModal(tr('gateway.devices'), `<div class="section-tools"><span class="spacer"></span><button type="button" data-icon="plus" data-gateway-add>${tr('gateway.add_client')}</button></div><div class="settings-list">${clients.clients.map(client => `<div class="setting-row"><strong>${esc(client.name)}</strong><div class="aggregate-actions"><button type="button" class="secondary" data-icon="chevron-right" data-gateway-view="${esc(client.id)}" title="${esc(tr('gateway.view_key'))}" aria-label="${esc(tr('gateway.view_key'))}"></button><button type="button" class="secondary danger" data-icon="trash" data-gateway-remove="${esc(client.id)}" title="${esc(tr('gateway.revoke'))}" aria-label="${esc(tr('gateway.revoke'))}"></button></div></div>`).join('') || `<p class="muted">${tr('gateway.no_clients')}</p>`}</div>`, '', null, null, {back, reopen:() => devices(back)});
    $('modal_body').querySelector('[data-gateway-add]').onclick = () => {
      openModal(tr('gateway.add_client'), `<div class="routing-settings"><div class="setting-row routing-mode"><label class="setting-title" for="gateway_client_name">${tr('gateway.client_name')}</label><input id="gateway_client_name" maxlength="64" autocomplete="off"></div></div>`, tr('gateway.create_key'), async () => {
        const owner=$('gateway_client_name');
        const created = await api('/api/gateway/clients', {method:'POST', body:JSON.stringify({name:owner.value})});
        if(!owner.isConnected)return;
        showKey(created,back);
      }, null, {back:() => devices(back)});
    };
    $('modal_body').querySelectorAll('[data-gateway-view]').forEach(button => button.onclick = () => showKey(clients.clients.find(client=>client.id===button.dataset.gatewayView),back));
    $('modal_body').querySelectorAll('[data-gateway-remove]').forEach(button => button.onclick = () => openModal(tr('gateway.revoke'), `<p>${tr('gateway.revoke_help')}</p>`, tr('services.remove'), async () => {
      const owner=$('modal_body').firstElementChild;
      await api('/api/gateway/clients', {method:'DELETE', body:JSON.stringify({id:button.dataset.gatewayRemove})});
      if(owner.isConnected)await devices(back);
    }, null, {back:() => devices(back)}));
  }
  function showKey(client,back) {
    const key=client.api_key;
    const body=key?`<label for="gateway_created_key">${tr('core.gateway_key')}</label><div class="input-action"><input id="gateway_created_key" readonly value="${esc(key)}"><button type="button" class="secondary" data-icon="copy" data-created-copy title="${esc(tr('core.gateway_copy_key'))}" aria-label="${esc(tr('core.gateway_copy_key'))}"></button></div>`:`<p>${tr('gateway.legacy_key')}</p>`;
    openModal(client.name,`<div id="gateway_client_details">${body}</div>`,key?'':tr('gateway.regenerate'),key?null:async()=>{
      const owner=$('gateway_client_details');
      const updated=await api('/api/gateway/clients',{method:'POST',body:JSON.stringify({id:client.id})});
      if(owner.isConnected)showKey(updated,back);
    },null,{back:()=>devices(back)});
    if(key)$('modal_body').querySelector('[data-created-copy]').onclick=()=>navigator.clipboard.writeText(key).then(()=>notice(tr('core.gateway_key_copied')),error=>notice(error.message,true));
  }
  async function usage(back, days = 1) {
    const end = Date.now() / 1000;
    const report = await read(`/api/gateway/usage?start=${end - days * 86400}&end=${end}`);
    if(!report)return;
    const rows = report.rows.map(row => `<tr><td title="${esc(row.peer_ips || '')}">${esc(row.caller_name || tr('gateway.source_' + (['local','shared'].includes(row.caller_id) ? row.caller_id : 'unknown')))}</td><td>${esc(modelName(row.model_id))}<small class="muted">${esc(serviceName(row.provider_id))} · ${row.operation === 'decisions' ? tr('core.decision_models') : esc(row.operation)}</small></td><td>${row.calls}</td><td title="${esc(tr('gateway.reported_calls',{reported:row.reported_calls,calls:row.calls}))}">${row.input_tokens === null ? '—' : compactUsage(row.input_tokens).html} / ${row.output_tokens === null ? '—' : compactUsage(row.output_tokens).html}</td><td title="${esc(tr('gateway.priced_calls', {priced:row.priced_calls, calls:row.calls}))}">${row.cost_nanos === null ? '—' : compactUsage(row.cost_nanos / 1e9,true).html}</td></tr>`).join('');
    openModal(tr('gateway.usage'), `<div class="call-toolbar"><div class="period-presets" id="gateway_usage_days" role="group" aria-label="${esc(tr('gateway.period'))}">${[1,7,15,30].map(value => `<button type="button" class="secondary${value === days ? ' active' : ''}" data-gateway-days="${value}" aria-pressed="${value === days}">${tr('gateway.days',{days:value})}</button>`).join('')}</div>${help(tr('gateway.usage_help'))}</div><div class="usage-table"><table><thead><tr>${['gateway.client_name','reports.model','reports.count','gateway.tokens','gateway.estimated_cost'].map(key => `<th>${tr(key)}</th>`).join('')}</tr></thead><tbody>${rows || `<tr><td colspan="5">${tr('gateway.no_calls')}</td></tr>`}</tbody></table></div>`, '', null, null, {back, reopen:() => usage(back,days)});
    $('gateway_usage_days').querySelectorAll('[data-gateway-days]').forEach(button => button.onclick = () => usage(back,Number(button.dataset.gatewayDays)));
  }
  return {open};
}
