// Executed in the existing page scope. Production configuration never receives a write.
const shadowNativeFetch = window.fetch.bind(window);
const shadowStorageKey = 'emp.shadow.config.v1';
let shadowDraft;
try { shadowDraft = JSON.parse(localStorage.getItem(shadowStorageKey) || 'null'); } catch (_) { shadowDraft = null; }
function shadowScrub(value) {
  if (Array.isArray(value)) return value.map(shadowScrub);
  if (!value || typeof value !== 'object') return value;
  return Object.fromEntries(Object.entries(value).filter(([key]) => !['api_key','auth_json','access_token','refresh_token','id_token','password','authorization','proxy-authorization','cookie'].includes(key.toLowerCase())).map(([key,entry]) => [key,shadowScrub(entry)]));
}
function shadowSave(candidate) {
  if (shadowServicesDemoActive) return shadowServiceDemoDraft = shadowScrub(candidate);
  shadowDraft = shadowScrub(candidate);
  localStorage.setItem(shadowStorageKey, JSON.stringify(shadowDraft));
  return shadowDraft;
}
function shadowMerge(live) {
  if (shadowServicesDemoActive) return shadowServiceDemoState(live);
  if (!shadowDraft) return live;
  const result = structuredClone({...live,...shadowDraft});
  for (const key of ['emp_version','claude_quota','catalog_models','catalog_families']) result[key] = live[key];
  const liveAccounts = new Map([live.native_account,...(live.accounts || [])].filter(Boolean).map(a => [a.id,a]));
  result.accounts = (result.accounts || []).map(a => ({...liveAccounts.get(a.id),...a,quota:liveAccounts.get(a.id)?.quota || a.quota}));
  if (result.native_account && live.native_account) result.native_account = {...result.native_account,quota:live.native_account.quota};
  const modelById = new Map((result.models || []).map(m => [m.id,m]));
  const accounts = new Map([result.native_account,...result.accounts].filter(Boolean).map(a => [a.id,a]));
  result.catalog_families = (result.catalog_families || []).map(f => ({...f,routes:(f.routes || []).filter(r => {
    if (modelById.has(r.id)) return modelById.get(r.id).enabled !== false;
    const originalModel = (live.models || []).some(m => m.id === r.id);
    if (originalModel) return false;
    const originalAccount = [live.native_account,...(live.accounts || [])].filter(Boolean).find(a => (a.prefix ? a.prefix + '/' : '') + f.id === r.id);
    if (!originalAccount) return true;
    const a = accounts.get(originalAccount.id);
    return a && !(a.hidden_models || []).includes(f.id);
  })})).filter(f => f.routes.length);
  for (const model of result.models || []) {
    if (model.enabled === false || result.catalog_families.some(f => f.routes.some(r => r.id === model.id))) continue;
    const route = {id:model.id,source_id:model.provider,source_type:'external'};
    const id = model.family_id || model.upstream_id;
    const family = result.catalog_families.find(f => f.id === id);
    if (family) family.routes.push(route);
    else result.catalog_families.push({id,display_name:model.display_name || model.upstream_id,default_display_name:model.display_name || model.upstream_id,context_window:model.context_window,routes:[route],supports_reasoning_summaries:model.supports_reasoning_summaries});
  }
  return result;
}
function shadowReply(payload, status = 200) {
  return new Response(JSON.stringify(payload), {status,headers:{'Content-Type':'application/json'}});
}
async function shadowRead(path, options) {
  const response = await shadowNativeFetch(path, {...options,method:'GET',body:undefined});
  if (!response.ok) throw new Error('读取 EMP 数据失败');
  return response.json();
}
async function shadowSavedModels(id, options) {
  const configured = (state?.models || []).filter(m => m.provider === id);
  let saved;
  try { saved = await shadowRead('/api/providers/' + encodeURIComponent(id) + '/models',options); }
  catch (error) {
    if (!configured.length && !(shadowDraft?.providers || []).some(p => p.id === id)) throw error;
    saved = {models:[]};
  }
  const models = [...(saved.models || [])];
  for (const model of configured) if (!models.some(m => m.upstream_id === model.upstream_id)) models.push(model);
  return {...saved,models,cached:saved.cached || models.length > 0};
}
window.fetch = async function(path, options = {}) {
  const url = new URL(String(path), location.href);
  if (url.origin !== location.origin || !url.pathname.startsWith('/api/')) return shadowNativeFetch(path, options);
  const route = url.pathname, method = (options.method || 'GET').toUpperCase();
  if (route === '/api/session' || route === '/api/accounts/events') return shadowNativeFetch(path, options);
  if (method === 'GET') {
    if (route === '/api/config') return shadowReply(shadowMerge(await shadowRead(path, options)));
    const providerModels = route.match(/^\/api\/providers\/([^/]+)\/models$/);
    if (providerModels) return shadowReply(await shadowSavedModels(decodeURIComponent(providerModels[1]),options));
    if (route === '/api/accounts' && (shadowDraft || shadowServicesDemoActive)) {
      const snapshot = await shadowRead(path, options);
      const live = shadowMerge({...state,...snapshot});
      return shadowReply({...snapshot,accounts:live.accounts,native_account:live.native_account});
    }
    return shadowNativeFetch(path, options);
  }
  let payload = {};
  try { payload = JSON.parse(options.body || '{}'); } catch (_) { return shadowReply({error:{message:'请输入有效 JSON。'}},400); }
  if (route === '/api/config') {
    for (const provider of payload.providers || []) if (provider.api_key) provider.api_key_set = true;
    const live = await shadowRead('/api/config',options);
    shadowSave(payload);
    return shadowReply(shadowMerge(live));
  }
  if (route === '/api/catalog/context-preference') return shadowReply(shadowSave({...state,...payload}));
  if (route === '/api/catalog/refresh' || route === '/api/client-events') return shadowReply({ok:true});
  if (route === '/api/accounts/import') {
    if ((state.accounts || []).some(a => a.id === payload.id)) return shadowReply({error:{message:'此账号 ID 已存在。'}},409);
    shadowSave({...state,accounts:[...(state.accounts || []),{id:payload.id,name:payload.name,prefix:payload.prefix,credential_set:false,enabled:true,hidden_models:[]}]});
    return shadowReply({ok:true});
  }
  const accountDelete = method === 'DELETE' && route.match(/^\/api\/accounts\/([^/]+)$/);
  if (accountDelete) {
    shadowSave({...state,accounts:state.accounts.filter(a => a.id !== decodeURIComponent(accountDelete[1]))});
    return shadowReply({ok:true});
  }
  const quota = route.match(/^\/api\/(accounts|providers)\/([^/]+)\/quota$/);
  if (quota) {
    const snapshot = await shadowRead('/api/accounts', options);
    if (quota[1] === 'providers') return shadowReply({quota:snapshot.claude_quota,history_saved:true});
    const merged = shadowMerge({...state,...snapshot});
    const account = [merged.native_account,...merged.accounts].find(a => a?.id === decodeURIComponent(quota[2]));
    return shadowReply({account});
  }
  const modelsRefresh = route.match(/^\/api\/accounts\/([^/]+)\/models\/refresh$/);
  if (modelsRefresh) return shadowReply(await shadowRead(route.replace('/refresh',''), options));
  if (route === '/api/providers/discover') {
    const provider = (state.providers || []).find(p => p.id === payload.provider);
    if (!provider) return shadowReply({error:{message:'请选择服务。'}},404);
    const list = await shadowSavedModels(provider.id,options);
    if (!Array.isArray(payload.selected)) return shadowReply(list);
    const selected = new Set(payload.selected);
    const models = (state.models || []).map(m => m.provider === provider.id ? {...m,enabled:selected.has(m.upstream_id)} : m);
    for (const upstream of list.models || []) if (selected.has(upstream.upstream_id) && !models.some(m => m.provider === provider.id && m.upstream_id === upstream.upstream_id)) models.push({...upstream,id:provider.id + '/' + upstream.upstream_id,provider:provider.id,enabled:true});
    shadowSave({...state,models});
    return shadowReply({imported:selected.size,models:list.models});
  }
  return shadowReply({error:{message:tr('请在正式 EMP 页面执行此操作。','Use the main EMP page for this action.')}},409);
};
