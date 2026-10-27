// Feature state is private; the page supplies current data and UI operations.
function createManagementClient({tr, fetch, storage, window}) {
  const SESSION_STORAGE_KEY = 'emp_management_session_v1';
  let sessionToken = '';
  const SESSION_HEADER = 'X-EMP-Session';
  let stopped = false;
  function readStoredSessionToken() { try { return storage().getItem(SESSION_STORAGE_KEY) || ''; } catch (_) { return ''; } }
  function saveSessionToken(token) { sessionToken = token; try { storage().setItem(SESSION_STORAGE_KEY, token); } catch (_) {} }
  function clearSessionToken() { sessionToken = ''; try { storage().removeItem(SESSION_STORAGE_KEY); } catch (_) {} }
  function managementFetch(path, options = {}) {
    if (stopped) return Promise.reject(new Error(tr("core.emp_has_exited")));
    const headers = new Headers(options.headers || {});
    if (sessionToken) headers.set(SESSION_HEADER, sessionToken);
    else headers.delete(SESSION_HEADER);
    return fetch(path, {...options, headers, credentials:'omit', redirect:'error'});
  }
  const SESSION_ERRORS = ['management session is required', 'proxy caller authentication is required'];
  const UPSTREAM_ERROR_GROUPS = [
    [['auth'], "core.the_api_key_is_invalid_or_expired_check_it"],
    [['payment_required'], "core.the_provider_account_is_out_of_credit_or_requires"],
    [['rate_limit'], "core.too_many_requests_try_again_later"],
    [['network','dns_failure','tls_failure','connect_timeout','proxy_unavailable','proxy_reset'], "core.cannot_reach_the_model_service_check_the_network_proxy"],
    [['timeout','upstream_504','first_event_timeout','first_output_timeout','idle_after_output','local_deadline'], "core.the_model_service_timed_out_try_again_later"],
    [['upstream_5xx','upstream_capacity'], "core.the_model_service_is_temporarily_unavailable_try_again_later"],
    [['protocol_rejection'], "core.the_model_service_rejected_the_request_it_may_not"],
    [['protocol_error','malformed_terminal','stream_error','stream_incomplete'], "core.the_model_service_returned_something_emp_could_not_read"],
    [['output_limit'], "core.the_model_output_hit_its_limit"],
    [['context_length_exceeded'], "core.the_input_exceeds_the_model_context_window"],
    [['content_filter'], "core.the_model_service_blocked_the_content"],
  ];
  // Turns an api() failure into a sentence that says whether the key, the network or the service is at fault.
  function upstreamErrorText(error, fallback = '') {
    if (error?.session) return error.message;
    const detail = error?.payload?.error || {};
    if (detail.code === 'active_codex_writer') return tr("core.codex_is_still_using_conversation_history_finish_active_conversations");
    if (detail.message === 'provider API key is not configured') return tr("core.no_api_key_has_been_entered_yet");
    const group = UPSTREAM_ERROR_GROUPS.find(([types]) => types.includes(detail.type));
    return group ? tr(group[1]) : error?.message || fallback;
  }
  async function api(path, options = {}) {
    const headers = new Headers(options.headers || {});
    if (!headers.has('Content-Type')) headers.set('Content-Type','application/json');
    const response = await managementFetch(path, {...options, headers});
    const text = await response.text();
    let data = {};
    try { data = text ? JSON.parse(text) : {}; } catch (_) { data = {error:{message:text || tr("core.the_server_returned_an_invalid_response")}}; }
    if (!response.ok) {
      // Upstream failures carry error.type; EMP's own 401s mean this page lost its session.
      const session = response.status === 401 && SESSION_ERRORS.includes(data.error?.message);
      if (session) clearSessionToken();
      const error = new Error(session ? tr("core.this_page_is_no_longer_signed_in_emp_may") : data.error?.message || response.statusText || `HTTP ${response.status}`);
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
    if (bootstrapTokens.length > 1) throw new Error(tr("core.this_sign_in_link_is_invalid_reopen_the_management"));
    if (bootstrapTokens.length === 1) {
      const response = await fetch('/api/session', {
        method:'POST',
        headers:{'X-EMP-Bootstrap':bootstrapTokens[0],'Content-Type':'application/json'}, body:'{}',
        credentials:'same-origin',
        cache:'no-store',
        redirect:'error',
      });
      let data = {};
      try { data = await response.json(); } catch (_) {}
      if (!response.ok) {
        sessionToken = readStoredSessionToken();
        if (!sessionToken) throw new Error(data.error?.message || response.statusText || `HTTP ${response.status}`);
      } else {
        if (typeof data.session !== 'string' || !data.session) throw new Error(tr("core.the_server_did_not_return_a_management_session"));
        saveSessionToken(data.session);
      }
    } else {
      sessionToken = readStoredSessionToken();
    }
    if (!sessionToken) throw new Error(tr("core.open_the_management_page_from_the_link_printed_by"));
  }
  function receiveStorageEvent(event) {
    if (event.key !== SESSION_STORAGE_KEY) return false;
    sessionToken = event.newValue || '';
    return Boolean(sessionToken);
  }

  return {request:api, fetch:managementFetch, establish:establishSession, errorText:upstreamErrorText, clear:clearSessionToken, stop:()=>{stopped=true;}, isStopped:()=>stopped, isSignedIn:() => Boolean(sessionToken), receiveStorageEvent};
}
