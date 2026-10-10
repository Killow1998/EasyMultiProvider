// Consume content-free request receipts already present in activity-updated.
// No polling, new backend endpoint, upstream message or credential is needed.
let presentationRequestReceipts = new Map(), presentationRequestOrder = 0;
let presentationRequestOutcomes = new Map();
function presentationRequestServiceKey(record) {
  return record.provider_id ? 'provider:'+record.provider_id : 'account:'+(record.account_id || '@native');
}
function presentationObserveRequestErrors(snapshot) {
  const next=new Map();
  const records=[...(snapshot?.requests || [])].reverse().sort((a,b)=>(a.finished_at || a.updated_at || 0)-(b.finished_at || b.updated_at || 0));
  for(const record of records) {
    if(!record.request_id || !['failed','completed'].includes(record.state))continue;
    const signature=JSON.stringify([record.state,record.finished_at,record.http_status,record.error_class,record.failure_reason,record.error_origin,record.error_code,record.model_id,record.provider_id,record.account_id]);
    const previous=presentationRequestReceipts.get(record.request_id);
    next.set(record.request_id,{record,signature,order:previous?.signature===signature?previous.order:++presentationRequestOrder});
  }
  presentationRequestReceipts=next;presentationRequestOutcomes=new Map();
  for(const outcome of next.values()) {
    const key=presentationRequestServiceKey(outcome.record);
    if(!presentationRequestOutcomes.has(key) || presentationRequestOutcomes.get(key).order<outcome.order)presentationRequestOutcomes.set(key,outcome);
  }
}
function presentationRequestError(record) {
  const status=Number(record.http_status);
  const reason=status===429?tr('额度或请求频率受限。','Quota or request rate limited.')
    :[401,403].includes(status)?tr('认证或访问权限失败。','Authentication or access denied.')
    :status>=500?tr('服务暂时不可用。','The service is unavailable.')
    :tr('本次模型请求未完成。','The model request did not complete.');
  return {title:tr('最近一次模型请求失败','Latest model request failed'),message:[reason,record.model_id,status?'HTTP '+status:'',({emp:'EMP',upstream:tr('服务端','Upstream service'),transport:tr('连接','Connection'),client:tr('Codex 客户端','Codex client'),claude_cli:'Claude Code'})[record.error_origin],record.failure_reason || record.error_code || record.error_class].filter(Boolean).join('\n')};
}
function presentationUpdateServiceRequestErrors() {
  for(const button of document.querySelectorAll('#services .presentation-service-icon')) {
    // Quota/login errors retain priority and their own recovery lifecycle.
    if(button.hasAttribute('data-error-message') && !button.hasAttribute('data-request-error'))continue;
    const key=button.dataset.activityKind+':'+button.dataset.activityId;
    const record=presentationRequestOutcomes.get(key)?.record;
    const error=record?.state==='failed'?presentationRequestError(record):null;
    if(error && button.dataset.requestError===record.request_id && button.dataset.errorTitle===error.title && button.dataset.errorMessage===error.message)continue;
    if(button.hasAttribute('data-request-error')) {
      delete button.dataset.requestError;delete button.dataset.errorTitle;delete button.dataset.errorMessage;
      button.querySelector('.presentation-service-error-badge')?.remove();
      button.removeAttribute('aria-describedby');
      const description=activityDescription(button.dataset.activityLabel,activitySummaryForIndicator(button));
      button.setAttribute('aria-label',description);button.title=description;
    }
    if(!error)continue;
    button.dataset.requestError=record.request_id;button.dataset.errorTitle=error.title;button.dataset.errorMessage=error.message;
    const badge=document.createElement('span');badge.className='presentation-service-error-badge';badge.setAttribute('aria-hidden','true');badge.textContent='!';button.append(badge);
  }
  presentationUpdateServiceErrorLabels();
}
function installRequestErrors() {
  const apply=applyActivitySnapshot;
  applyActivitySnapshot=function(value){const snapshot=normalizeActivitySnapshot(value);if(!snapshot)return;presentationObserveRequestErrors(snapshot);apply(snapshot);};
  const update=updateActivityDots;
  updateActivityDots=function(){update();presentationUpdateServiceRequestErrors();};
}
