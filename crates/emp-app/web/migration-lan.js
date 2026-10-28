// Tunnel transport shares selection and import ownership with file migration.
function lanMigrationControls(rows, flags) {
  return `<p class="migration-tunnel-intro">${tr('lan.intro')}${shadowHelp(tr('lan.help'))}</p>
    <div class="migration-tunnel">
      <section class="tunnel-send"><h3>${tr('lan.send_title')}</h3>
        <details id="lan_selection" open><summary>${tr('lan.selection')} <span id="lan_selected_count" class="muted"></span></summary>
          <div class="migration-services">${rows}</div>${flags}</details>
      </section>
      <section class="tunnel-receive"><h3>${tr('lan.receive_title')}</h3><p class="muted">${tr('lan.receive_help')}</p>
        <label for="lan_code">${tr('lan.code')}</label><textarea id="lan_code" rows="3" maxlength="49" autocomplete="off" spellcheck="false" placeholder="${esc(tr('lan.code_placeholder'))}"></textarea>
        <div class="tunnel-actions"><button type="button" data-lan-receive disabled>${tr('lan.receive')}</button><button type="button" class="secondary" data-lan-stop hidden>${tr('lan.stop_receive')}</button></div>
        <p id="lan_receive_status" role="status"></p>
      </section>
      <div class="tunnel-transfer">
          <div>
            <div id="lan_create_block" class="tunnel-create"><button type="button" data-lan-create>${tr('lan.create')}</button><span class="muted">${tr('lan.lifetime')}</span></div>
            <div id="lan_share_block" hidden><label for="lan_share_code">${tr('lan.code')}</label>
              <div class="tunnel-code"><textarea id="lan_share_code" readonly rows="2" spellcheck="false"></textarea><button type="button" class="secondary" data-lan-copy title="${esc(tr('lan.copy'))}" aria-label="${esc(tr('lan.copy'))}">${uiIcon('copy')}</button></div>
            </div>
            <div id="lan_transfer_meta" class="tunnel-meta" hidden><span id="lan_remaining" class="muted"></span><button type="button" class="secondary" data-lan-cancel>${tr('lan.cancel')}</button></div>
          </div>
          <div class="tunnel-confirm">
            <div id="lan_receiver_confirm" hidden><p id="lan_receiver"></p><button type="button" data-lan-approve>${tr('lan.approve')}</button></div>
            <p id="lan_send_status" role="status"></p>
          </div>
      </div>
    </div>`;
}
function createLanMigration({active,selections,options,openPreview}) {
  let sendTimer=null, receiveTimer=null, receiving=null;
  const pendingStates=['waiting','confirmation','sending'];
  function stop(){clearTimeout(sendTimer);clearTimeout(receiveTimer);receiving?.abort();receiving=null;}
  function errorText(error){const key='lan.'+error.message;return tr(key)===key?error.message:tr(key);}
  async function post(operation,body={},signal){return api('/api/migration/lan/'+operation,{method:'POST',body:JSON.stringify(body),signal});}
  function live(candidate){return active(candidate)&&candidate.mode==='tunnel';}
  function update(candidate){
    if(!live(candidate))return;
    const pending=pendingStates.includes(candidate.transferStatus);
    $('lan_create_block').hidden=pending;
    $('lan_share_block').hidden=!pending;
    $('lan_receiver_confirm').hidden=candidate.transferStatus!=='confirmation';
    $('lan_transfer_meta').hidden=!pending;
    const selected=selections(candidate).length;
    $('lan_selected_count').textContent=tr('transfers.services_selected',{count:selected});
    document.querySelector('[data-lan-create]').disabled=Boolean(candidate.sending||pending||!selected);
    for(const node of document.querySelectorAll('#lan_selection input'))node.disabled=Boolean(candidate.sending||pending)||(node.dataset.migrationSelect!==undefined&&candidate.rows[Number(node.dataset.migrationSelect)].exportable===false);
    document.querySelector('[data-lan-receive]').disabled=Boolean(receiving)||$('lan_code').value.trim().length!==49;
    document.querySelector('[data-lan-stop]').hidden=!receiving;
    $('lan_code').disabled=Boolean(receiving);
  }
  function mount(candidate){
    stop();
    document.querySelector('[data-lan-create]').onclick=()=>send(candidate);
    document.querySelector('[data-lan-receive]').onclick=()=>receive(candidate);
    document.querySelector('[data-lan-stop]').onclick=()=>{
      clearTimeout(receiveTimer);receiving?.abort();receiving=null;
      $('lan_receive_status').textContent=tr('lan.receive_stopped');update(candidate);
    };
    document.querySelector('[data-lan-copy]').onclick=()=>navigator.clipboard.writeText($('lan_share_code').value).then(()=>notice(tr('lan.copied')),error=>notice(error.message,true));
    document.querySelector('[data-lan-cancel]').onclick=()=>status(candidate,'cancel');
    document.querySelector('[data-lan-approve]').onclick=()=>status(candidate,'approve',{code:$('lan_share_code').value});
    update(candidate);void status(candidate);
  }
  async function status(candidate,operation='status',body={}) {
    clearTimeout(sendTimer);
    try {
      const result=await post(operation,body);
      if(!live(candidate))return;
      const pending=pendingStates.includes(result.status);
      const currentTransfer=pending||operation!=='status'||result.code===$('lan_share_code').value;
      candidate.transferStatus=result.status;
      $('lan_send_status').textContent=result.status==='idle'||!currentTransfer?(candidate.sending?tr('lan.preparing'):''):tr('lan.'+result.status);
      if(pending){
        $('lan_share_code').value=result.code;
        $('lan_remaining').textContent=tr('lan.remaining',{seconds:result.remaining_seconds});
      }
      if(result.status==='confirmation')$('lan_receiver').textContent=tr('lan.confirm_receiver',{address:result.receiver});
      update(candidate);
      if(pending)sendTimer=setTimeout(()=>status(candidate),1000);
    } catch(error){if(live(candidate))$('lan_send_status').textContent=errorText(error);}
  }
  async function send(candidate){
    if(candidate.sending||pendingStates.includes(candidate.transferStatus)||!selections(candidate).length)return;
    candidate.sending=true;update(candidate);
    $('lan_send_status').textContent=tr('lan.preparing');
    try{
      const authorization=await api('/api/migration/export/confirm',{method:'POST',body:'{}'});
      if(!live(candidate))return;
      await status(candidate,'start',{confirmation:authorization.confirmation,selections:selections(candidate),...options(candidate)});
    }catch(error){if(live(candidate))$('lan_send_status').textContent=errorText(error);}
    finally{candidate.sending=false;update(candidate);}
  }
  async function receive(candidate){
    const code=$('lan_code').value.trim();
    if(receiving||code.length!==49)return;
    if(candidate.receiveCode!==code){
      candidate.receiveCode=code;
      candidate.received=null;
      candidate.receiver=Array.from(crypto.getRandomValues(new Uint8Array(32)),n=>n.toString(16).padStart(2,'0')).join('');
    }
    // Back returns to this form; the one-use code has already been consumed.
    if(candidate.received){openPreview(candidate,candidate.received);return;}
    const controller=new AbortController();receiving=controller;update(candidate);
    $('lan_receive_status').textContent=tr('lan.connecting');
    const deadline=Date.now()+5*60*1000;
    async function poll(){
      if(!live(candidate)||controller.signal.aborted)return;
      try{
        const result=await post('receive',{code,receiver:candidate.receiver},controller.signal);
        if(!live(candidate)||controller.signal.aborted)return;
        if(result.token){receiving=null;candidate.received=result;openPreview(candidate,result);return;}
        if(Date.now()>deadline)throw new Error('lan_expired');
        $('lan_receive_status').textContent=tr('lan.wait_source');receiveTimer=setTimeout(poll,1000);
      }catch(error){if(live(candidate)&&!controller.signal.aborted){receiving=null;$('lan_receive_status').textContent=errorText(error);update(candidate);}}
    }
    await poll();
  }
  return {mount,update,stop};
}
