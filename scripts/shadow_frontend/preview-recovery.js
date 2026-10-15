// Adapter for the actual recovery.html. All actions stay in this browser page.
(() => {
  const parameters=new URLSearchParams(location.search);
  let reason=parameters.get('reason')||'key_mismatch';
  const reasons={key_mismatch:['密钥不匹配','Key mismatch'],key_missing:['密钥丢失','Missing key'],key_invalid:['密钥格式错误','Invalid key'],key_permissions:['密钥权限异常','Key permissions'],config_invalid:['配置格式错误','Invalid settings'],reset_incomplete:['上次重建中断','Interrupted reset']};
  if(!reasons[reason])reason='key_mismatch';
  window.fetch=async(path,options={})=>{
    const action=String(path).split('/').pop();
    let value,status=200;
    if(action==='recovery')value={reason};
    else if(action==='reset')value={restarting:true,backup_path:options.headers?.['X-EMP-Recovery-Confirm']==='backup-and-reset'?'Shadow / example / recovery-backup-demo':null};
    else if(action==='exit')value={exiting:true};
    else {value={error:'preview_only'};status=403;}
    return new Response(JSON.stringify(value),{status,headers:{'Content-Type':'application/json'}});
  };
  document.addEventListener('DOMContentLoaded',()=>{
    const panel=document.createElement('aside');panel.id='recovery_demo_controls';
    panel.style.cssText='border-bottom:1px solid #aab3bc;padding:12px 20px;display:flex;gap:12px;flex-wrap:wrap;align-items:center';
    panel.innerHTML='<a href="/?review=1">← Shadow</a><span id="demo_label"></span><select id="demo_reason" aria-label="Scenario"></select><button id="demo_again" type="button"></button>';
    document.body.prepend(panel);
    const update=()=>{
      const en=document.getElementById('language').value==='en';
      document.getElementById('demo_label').textContent=en?'Demo only · no files changed':'仅演示 · 不修改文件';
      document.getElementById('demo_again').textContent=en?'Restart demo':'重新演示';
      const select=document.getElementById('demo_reason');select.replaceChildren();
      for(const [key,labels] of Object.entries(reasons))select.add(new Option(labels[en?1:0],key));
      select.value=reason;
    };
    document.getElementById('language').addEventListener('change',update);
    const reopen=()=>location.assign('/recovery?reason='+document.getElementById('demo_reason').value);
    document.getElementById('demo_reason').onchange=reopen;
    document.getElementById('demo_again').onclick=reopen;
    update();
  });
})();
