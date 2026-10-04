// Feature state is private; the page supplies current data and UI operations.
function createManagementClient({tr, fetch, storage, window}) {
  const SESSION_STORAGE_KEY = 'emp_management_session_v1';
  let sessionToken = '';
  const SESSION_HEADER = 'X-EMP-Session';
  let legacyCookieAuth = false;
  function readStoredSessionToken() { try { return storage().getItem(SESSION_STORAGE_KEY) || ''; } catch (_) { return ''; } }
  function saveSessionToken(token) { sessionToken = token; try { storage().setItem(SESSION_STORAGE_KEY, token); } catch (_) {} }
  function clearSessionToken() { sessionToken = ''; try { storage().removeItem(SESSION_STORAGE_KEY); } catch (_) {} }
  function managementFetch(path, options = {}) {
    const headers = new Headers(options.headers || {});
    if (legacyCookieAuth) headers.delete(SESSION_HEADER);
    else if (sessionToken) headers.set(SESSION_HEADER, sessionToken);
    else headers.delete(SESSION_HEADER);
    return fetch(path, {...options, headers, credentials:legacyCookieAuth ? 'same-origin' : 'omit', redirect:'error'});
  }
  const SESSION_ERRORS = ['management session is required', 'proxy caller authentication is required'];
  const UPSTREAM_ERROR_GROUPS = [
    [['auth'], ['API Key 无效或已过期，请检查后重新填写。','The API key is invalid or expired. Check it and enter it again.']],
    [['payment_required'], ['上游账户余额不足或需要付费。','The provider account is out of credit or requires payment.']],
    [['rate_limit'], ['请求过于频繁，请稍后重试。','Too many requests. Try again later.']],
    [['network','dns_failure','tls_failure','connect_timeout','proxy_unavailable','proxy_reset'], ['连不上模型服务，请检查网络、代理和 Base URL。','Cannot reach the model service. Check the network, proxy and Base URL.']],
    [['timeout','upstream_504','first_event_timeout','first_output_timeout','idle_after_output','local_deadline'], ['模型服务响应超时，请稍后重试。','The model service timed out. Try again later.']],
    [['upstream_5xx','upstream_capacity'], ['模型服务暂时不可用，请稍后重试。','The model service is temporarily unavailable. Try again later.']],
    [['protocol_rejection'], ['模型服务拒绝了这个请求，可能不支持该模型或参数。','The model service rejected the request; it may not support this model or its parameters.']],
    [['protocol_error','malformed_terminal','stream_error','stream_incomplete'], ['模型服务返回了无法识别的内容。','The model service returned something EMP could not read.']],
    [['output_limit'], ['模型输出超过了上限。','The model output hit its limit.']],
    [['context_length_exceeded'], ['内容超过了模型的上下文上限。','The input exceeds the model context window.']],
    [['content_filter'], ['内容被模型服务的安全策略拦截。','The model service blocked the content.']],
  ];
  // Turns an api() failure into a sentence that says whether the key, the network or the service is at fault.
  function upstreamErrorText(error, fallback = '') {
    if (error?.session) return error.message;
    const detail = error?.payload?.error || {};
    if (detail.code === 'active_codex_writer') return tr('Codex 仍在使用对话历史。请先完成活动对话，再关闭 ChatGPT App、Codex CLI 和 IDE 中的 Codex 会话，然后重试。','Codex is still using conversation history. Finish active conversations, close ChatGPT App, Codex CLI and Codex sessions in your IDE, then retry.');
    if (detail.message === 'provider API key is not configured') return tr('还没有填写 API Key。','No API key has been entered yet.');
    const group = UPSTREAM_ERROR_GROUPS.find(([types]) => types.includes(detail.type));
    return group ? tr(...group[1]) : error?.message || fallback;
  }
  async function api(path, options = {}) {
    const headers = new Headers(options.headers || {});
    if (!headers.has('Content-Type')) headers.set('Content-Type','application/json');
    const response = await managementFetch(path, {...options, headers});
    const text = await response.text();
    let data = {};
    try { data = text ? JSON.parse(text) : {}; } catch (_) { data = {error:{message:text || tr('服务器返回了无效响应', 'The server returned an invalid response')}}; }
    if (!response.ok) {
      // Upstream failures carry error.type; EMP's own 401s mean this page lost its session.
      const session = response.status === 401 && SESSION_ERRORS.includes(data.error?.message);
      if (session) clearSessionToken();
      const error = new Error(session ? tr('管理页登录已失效（EMP 可能已重启）。请从 EMP 重新打开管理页。','This page is no longer signed in (EMP may have restarted). Reopen the management page from EMP.') : data.error?.message || response.statusText || `HTTP ${response.status}`);
      error.session = session;
      error.payload = data;
      error.status = response.status;
      // Every caller shows error.message, so give upstream failures their readable text here.
      error.message = upstreamErrorText(error, error.message);
      throw error;
    }
    return data;
  }
  async function establishSession() {
    const pageUrl = new URL(window.location.href);
    const bootstrapTokens = pageUrl.searchParams.getAll('bootstrap');
    if (bootstrapTokens.length) {
      pageUrl.searchParams.delete('bootstrap');
      window.history.replaceState(window.history.state, '', pageUrl.pathname + pageUrl.search + pageUrl.hash);
    }
    if (bootstrapTokens.length > 1) throw new Error(tr('登录链接无效，请从 EMP 重新打开管理页。','This sign-in link is invalid. Reopen the management page from EMP.'));
    if (bootstrapTokens.length === 1) {
      const response = await fetch('/api/session', {
        method:'POST',
        // Same JSON shape as the probe below: cookie-session Python servers
        // answer it with 404 instead of rejecting the body.
        headers:{'X-EMP-Bootstrap':bootstrapTokens[0],'Content-Type':'application/json'}, body:'{}',
        credentials:'same-origin',
        cache:'no-store',
        redirect:'error',
      });
      if (response.status === 404) {
        legacyCookieAuth = true;
        return;
      }
      let data = {};
      try { data = await response.json(); } catch (_) {}
      if (!response.ok) {
        sessionToken = readStoredSessionToken();
        if (!sessionToken) throw new Error(data.error?.message || response.statusText || `HTTP ${response.status}`);
      } else {
        if (typeof data.session !== 'string' || !data.session) throw new Error(tr('服务器未返回登录会话。','The server did not return a management session.'));
        saveSessionToken(data.session);
      }
    } else {
      // Probe even with a stored token: a token left by a header-session EMP on
      // this origin must not hide a cookie-session server. The probe carries no
      // bootstrap token and has no side effects.
      sessionToken = readStoredSessionToken();
      const response = await fetch('/api/session', {
        method:'POST', headers:{'Content-Type':'application/json'}, body:'{}',
        credentials:'same-origin', cache:'no-store', redirect:'error',
      });
      if (response.status === 404) {
        legacyCookieAuth = true;
        return;
      }
    }
    if (!legacyCookieAuth && !sessionToken) throw new Error(tr('请从 EMP 启动时提供的链接打开管理页。','Open the management page from the link printed by EMP.'));
  }
  function receiveStorageEvent(event) {
    if (event.key !== SESSION_STORAGE_KEY) return false;
    sessionToken = event.newValue || '';
    return Boolean(sessionToken);
  }

  return {request:api, fetch:managementFetch, establish:establishSession, errorText:upstreamErrorText, clear:clearSessionToken, usesLegacyCookies:() => legacyCookieAuth, isSignedIn:() => Boolean(sessionToken || legacyCookieAuth), receiveStorageEvent};
}
