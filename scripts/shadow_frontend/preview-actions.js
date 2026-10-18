// Concrete synthetic scenarios. Navigation and isolation are owned by their adapters.
async function openRoutingDemo() {
  if (!(state.accounts || []).some(account => account.id === 'routing-demo')) {
    await api('/api/accounts/import', {method:'POST', body:JSON.stringify({
      id:'routing-demo', prefix:'routing-demo', name:tr('图片示例账号','Image sample account'),
      auth_json:{tokens:{account_id:'routing-demo-identity',access_token:'synthetic-routing-preview-only'}}
    })});
    state = await api('/api/config'); fill();
  }
  routingSettings.open({back:openSettings});
}
function openCollaborationDemo() {
  openSettings();
  document.querySelector('[data-shadow-help-template=collaboration-comparison]').focus();
}
async function openHistoricalDemo() {
  closeModal();
  await api('/api/accounts/import',{method:'POST',body:JSON.stringify({id:'review-history',prefix:'review-history',name:'Earlier imported settings',auth_json:{tokens:{account_id:'demo-fixture-account',access_token:'synthetic-history-preview-only'}}})});
  const candidate=await api('/api/config');
  candidate.native_hidden_models=[];candidate.native_model_context_windows={'demo-model':400000};
  for(const account of candidate.accounts.filter(a=>a.id==='review-history')){
    account.hidden_models=['demo-model'];account.model_context_windows={'demo-model':467368};
  }
  await api('/api/config',{method:'POST',body:JSON.stringify(candidate)});
  state=await api('/api/config');fill();editAccount('@native');
}
async function openRemovalDemo() {
  closeModal();
  await api('/api/accounts/import',{method:'POST',body:JSON.stringify({id:'review-removal',prefix:'review-removal',name:'Removal sample',auth_json:{tokens:{account_id:'review-removal-identity',access_token:'synthetic-preview-only'}}})});
  state=await api('/api/config');fill();await openAccountDetails('review-removal');
  notice(tr('示例账号可移除；演示增加 0.7 秒等待以查看进度。','Remove this sample account. A 0.7 second demo delay makes progress visible.'));
}
