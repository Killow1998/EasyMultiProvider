function lanMigrationControls(rows, flags) {
  return `<div class="tabs migration-tabs" role="tablist" aria-label="${esc(tr('lan.tunnel'))}">
      <button type="button" role="tab" id="lan_tab_send" aria-controls="lan_send_panel" aria-selected="true">${tr('lan.send_title')}</button>
      <button type="button" role="tab" id="lan_tab_receive" aria-controls="lan_receive_panel" aria-selected="false" tabindex="-1">${tr('lan.receive_title')}</button>
    </div>
    <section id="lan_send_panel" class="tunnel-send" role="tabpanel" aria-labelledby="lan_tab_send">
      <div id="lan_selection" class="tunnel-selection"><div class="tunnel-selection-head"><span>${tr('lan.selection')}</span><span id="lan_selected_count" class="muted"></span></div>
        <div class="migration-services">${rows}</div>${flags}</div>
      <div class="tunnel-footer">
        <div id="lan_create_block" class="tunnel-create"><button type="button" class="primary" data-lan-create>${tr('lan.create')}</button></div>
        <div id="lan_share_block" class="tunnel-share" hidden>
          <textarea id="lan_share_code" readonly hidden tabindex="-1" aria-hidden="true"></textarea>
          <div id="lan_receiver_confirm" hidden></div>
          <div id="lan_transfer_meta" class="tunnel-meta" hidden><span id="lan_remaining" class="muted"></span><button type="button" class="secondary" data-lan-cancel>${tr('lan.cancel')}</button></div>
        </div>
        <p id="lan_send_status" class="tunnel-status" role="status"></p>
      </div>
    </section>
    <section id="lan_receive_panel" class="tunnel-receive" role="tabpanel" aria-labelledby="lan_tab_receive" hidden>
      <div class="tunnel-field-label"><label for="lan_code">${tr('lan.code')}</label>${shadowHelp(tr('lan.receive_help'))}</div>
      <textarea id="lan_code" rows="2" maxlength="49" autocomplete="off" spellcheck="false" placeholder="${esc(tr('lan.code_placeholder'))}"></textarea>
      <div class="tunnel-footer"><div class="tunnel-actions"><button type="button" data-lan-receive disabled>${tr('lan.receive')}</button><button type="button" class="secondary" data-lan-stop hidden>${tr('lan.stop_receive')}</button></div>
        <p id="lan_receive_status" class="tunnel-status" role="status"></p></div>
    </section>`;
}
function createLanMigration({active,selections,options,openPreview}) {
  let sendTimer=null, receiveTimer=null, receiving=null;
  const pendingStates=['waiting','confirmation','sending'];
  let island=null;
  function stop(){clearTimeout(sendTimer);clearTimeout(receiveTimer);receiving?.abort();receiving=null;withdraw();}
  function withdraw(){if(island)notifications.withdraw(island.id);island=null;}
  const clock=seconds=>Math.floor(seconds/60)+':'+String(Math.max(0,seconds)%60).padStart(2,'0');
  function copy(){navigator.clipboard.writeText($('lan_share_code').value).then(()=>notice(tr('lan.copied')),error=>notice(error.message,true));}
  function announce(candidate,result){
    const kind=['waiting','confirmation','sending'].includes(result.status)?result.status:null;
    if(!kind){
      const finished=island&&['sent','failed','expired','cancelled'].includes(result.status);
      withdraw();
      if(finished)notice(tr('lan.'+result.status),result.status!=='sent');
      return;
    }
    const message=kind==='waiting'?tr('lan.code_ready',{time:clock(result.remaining_seconds)}):kind==='confirmation'?tr('lan.receiver_request',{address:result.receiver}):tr('lan.sending');
    if(island?.kind===kind){notifications.update(island.id,message);return;}
    withdraw();
    const actions=kind==='waiting'?[{label:tr('lan.copy'),primary:true,attributes:{'data-lan-copy':''},run:copy}]
      :kind==='confirmation'?[{label:tr('lan.decline'),attributes:{'data-lan-decline':''},run:()=>status(candidate,'cancel')},
        {label:tr('lan.approve'),primary:true,attributes:{'data-lan-approve':''},run:()=>status(candidate,'approve',{code:$('lan_share_code').value})}]:[];
    island={kind,id:notifications.show(message,false,{persist:true,actions})};
  }
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
    const tabs=[['send',$('lan_tab_send'),$('lan_send_panel')],['receive',$('lan_tab_receive'),$('lan_receive_panel')]];
    const select=name=>{candidate.lanTab=name;for(const [id,tab,panel] of tabs){const on=id===name;tab.setAttribute('aria-selected',String(on));tab.tabIndex=on?0:-1;panel.hidden=!on;}};
    for(const [id,tab] of tabs){tab.onclick=()=>select(id);tab.onkeydown=event=>{if(event.key==='ArrowRight'||event.key==='ArrowLeft'){event.preventDefault();const next=id==='send'?'receive':'send';select(next);$('lan_tab_'+next).focus();}};}
    select(candidate.lanTab||'send');
    $('lan_code').onpaste=()=>setTimeout(()=>{update(candidate);if($('lan_code').value.trim().length===49)document.querySelector('[data-lan-receive]').focus();});
    document.querySelector('[data-lan-cancel]').onclick=()=>status(candidate,'cancel');
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
        $('lan_remaining').textContent=tr('lan.remaining_clock',{time:clock(result.remaining_seconds)});
      }
      if(currentTransfer)announce(candidate,result);
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
