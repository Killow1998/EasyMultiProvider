// Consume content-free request receipts already present in activity-updated.
// No polling, new backend endpoint, upstream message or credential is needed.
let shadowRequestReceipts = new Map(), shadowRequestOrder = 0;
let shadowRequestOutcomes = new Map();
function shadowRequestServiceKey(record) {
  return record.provider_id ? 'provider:'+record.provider_id : 'account:'+(record.account_id || '@native');
}
function shadowObserveRequestErrors(snapshot) {
  const next=new Map();
  const records=[...(snapshot?.requests || [])].reverse().sort((a,b)=>(a.finished_at || a.updated_at || 0)-(b.finished_at || b.updated_at || 0));
  for(const record of records) {
    if(!record.request_id || !['failed','completed'].includes(record.state))continue;
    const signature=JSON.stringify([record.state,record.finished_at,record.http_status,record.error_class,record.failure_reason,record.error_origin,record.error_code,record.model_id,record.provider_id,record.account_id]);
    const previous=shadowRequestReceipts.get(record.request_id);
    next.set(record.request_id,{record,signature,order:previous?.signature===signature?previous.order:++shadowRequestOrder});
  }
  shadowRequestReceipts=next;shadowRequestOutcomes=new Map();
  for(const outcome of next.values()) {
    const key=shadowRequestServiceKey(outcome.record);
    if(!shadowRequestOutcomes.has(key) || shadowRequestOutcomes.get(key).order<outcome.order)shadowRequestOutcomes.set(key,outcome);
  }
}
function shadowRequestError(record) {
  const status=Number(record.http_status);
  const reason=status===429?tr('额度或请求频率受限。','Quota or request rate limited.')
    :[401,403].includes(status)?tr('认证或访问权限失败。','Authentication or access denied.')
    :status>=500?tr('服务暂时不可用。','The service is unavailable.')
    :tr('本次模型请求未完成。','The model request did not complete.');
  return {title:tr('最近一次模型请求失败','Latest model request failed'),message:[reason,record.model_id,status?'HTTP '+status:'',({emp:'EMP',upstream:tr('服务端','Upstream service'),transport:tr('连接','Connection'),client:tr('Codex 客户端','Codex client'),claude_cli:'Claude Code'})[record.error_origin],record.failure_reason || record.error_code || record.error_class].filter(Boolean).join('\n')};
}
function shadowUpdateServiceRequestErrors() {
  if(shadowServicesDemoActive)return;
  for(const button of document.querySelectorAll('#services .shadow-service-icon')) {
    // Quota/login errors retain priority and their own recovery lifecycle.
    if(button.hasAttribute('data-error-message') && !button.hasAttribute('data-request-error'))continue;
    const key=button.dataset.activityKind+':'+button.dataset.activityId;
    const record=shadowRequestOutcomes.get(key)?.record;
    const error=record?.state==='failed'?shadowRequestError(record):null;
    if(error && button.dataset.requestError===record.request_id && button.dataset.errorTitle===error.title && button.dataset.errorMessage===error.message)continue;
    if(button.hasAttribute('data-request-error')) {
      delete button.dataset.requestError;delete button.dataset.errorTitle;delete button.dataset.errorMessage;
      button.querySelector('.shadow-service-error-badge')?.remove();
      button.removeAttribute('aria-describedby');
      const description=activityDescription(button.dataset.activityLabel,activitySummaryForIndicator(button));
      button.setAttribute('aria-label',description);button.title=description;
    }
    if(!error)continue;
    button.dataset.requestError=record.request_id;button.dataset.errorTitle=error.title;button.dataset.errorMessage=error.message;
    const badge=document.createElement('span');badge.className='shadow-service-error-badge';badge.setAttribute('aria-hidden','true');badge.textContent='!';button.append(badge);
  }
  shadowUpdateServiceErrorLabels();
}
function installShadowRequestErrors() {
  const apply=applyActivitySnapshot;
  applyActivitySnapshot=function(value){const snapshot=normalizeActivitySnapshot(value);if(!snapshot)return;if(!shadowServicesDemoActive)shadowObserveRequestErrors(snapshot);apply(snapshot);};
  const update=updateActivityDots;
  updateActivityDots=function(){update();shadowUpdateServiceRequestErrors();};
}
