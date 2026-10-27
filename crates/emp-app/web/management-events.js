// One owner for the management SSE connection and reconnect lifecycle.
function createManagementEvents({management,visible,onEvent,onStart,onDisconnect}) {
  let quotaEventsController = null, quotaEventsConnected = false;
  let leading = false;
  const snapshots = new Map();
  let channel = typeof navigator !== 'undefined' && navigator.locks && typeof BroadcastChannel === 'function'
    ? new BroadcastChannel('emp-management-events') : null;
  function connection(connected) {
    quotaEventsConnected = connected;
    if (leading) channel?.postMessage(['$connected', connected ? '1' : '0']);
    if (!connected) onDisconnect();
  }
  function receive(type, data) {
    if (type && type !== 'emp-exiting') snapshots.set(type, data);
    onEvent(type, data);
  }
  if (channel) channel.onmessage = ({data:message}) => {
    if (!quotaEventsController || !visible() || !Array.isArray(message) || message.length !== 2 || !message.every(value => typeof value === 'string')) return;
    const [type, data] = message;
    if (type === '$sync') {
      if (leading) {
        channel.postMessage(['$connected', quotaEventsConnected ? '1' : '0']);
        for (const snapshot of snapshots) channel.postMessage(snapshot);
      }
    } else if (type === '$connected') {
      if (!leading && (data === '0' || data === '1')) connection(data === '1');
    } else receive(type, data);
  };
  function dispatch(frame) {
    let type = ''; const data = [];
    for (const line of frame.split('\n')) {
      if (line.startsWith('event:')) type = line.slice(6).trim();
      else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
    }
    const value = data.join('\n');
    if (type) channel?.postMessage([type, value]);
    receive(type, value);
  }
  async function consumeQuotaEventStream(response, signal) {
    if (!response.body) throw new Error('Quota event stream has no readable body');
    const reader = response.body.getReader();
    const cancel = () => { void reader.cancel().catch(() => {}); };
    signal.addEventListener('abort', cancel, {once:true});
    const decoder = new TextDecoder();
    let buffer = '';
    try {
      while (!signal.aborted) {
        const {done, value} = await reader.read();
        if (done) return;
        if (signal.aborted) return;
        buffer = (buffer + decoder.decode(value, {stream:true})).replace(/\r\n/g, '\n');
        let boundary;
        while ((boundary = buffer.indexOf('\n\n')) >= 0) {
          const frame = buffer.slice(0, boundary);
          buffer = buffer.slice(boundary + 2);
          dispatch(frame);
        }
      }
    } finally {
      signal.removeEventListener('abort', cancel);
      try { await reader.cancel(); } catch (_) {}
    }
  }
  async function runQuotaEventStream(controller) {
    let retryDelay = 1000;
    while (!controller.signal.aborted && visible()) {
      try {
        const response = await management.fetch('/api/accounts/events', {headers:{'Accept':'text/event-stream'}, signal:controller.signal, cache:'no-store'});
        if (controller.signal.aborted || quotaEventsController !== controller) {
          try { await response.body?.cancel(); } catch (_) {}
          return;
        }
        if (!response.ok) {
          if (response.status === 401) { management.clear(); controller.abort(); return; }
          throw new Error(`Quota event stream returned HTTP ${response.status}`);
        }
        connection(true);
        retryDelay = 1000;
        await consumeQuotaEventStream(response, controller.signal);
      } catch (error) {
        if (controller.signal.aborted) return;
      } finally {
        if (quotaEventsController === controller) {
          connection(false);
        }
      }
      if (!controller.signal.aborted && visible()) {
        await new Promise(resolve => {
          const done = () => { clearTimeout(timer); controller.signal.removeEventListener('abort', done); resolve(); };
          const timer = setTimeout(done, retryDelay);
          controller.signal.addEventListener('abort', done, {once:true});
        });
        retryDelay = Math.min(retryDelay * 2, 15000);
      }
    }
  }
  function startQuotaEvents() {
    if (!management.isSignedIn() || !visible()) return;
    if (quotaEventsController) {
      for (const [type, data] of snapshots) onEvent(type, data);
      return;
    }
    onStart();
    const controller = new AbortController();
    quotaEventsController = controller;
    const run = channel ? navigator.locks.request('emp-management-events', {signal:controller.signal}, async () => {
      if (controller.signal.aborted || !management.isSignedIn() || !visible()) return;
      leading = true;
      snapshots.clear();
      try { await runQuotaEventStream(controller); }
      finally { leading = false; }
    }).catch(() => controller.abort()) : runQuotaEventStream(controller);
    channel?.postMessage(['$sync', '']);
    run.finally(() => {
      if (quotaEventsController === controller) {
        quotaEventsController = null;
        connection(false);
        if (!controller.signal.aborted && management.isSignedIn() && visible()) startQuotaEvents();
      }
    });
  }
  function stopQuotaEvents() {
    if (quotaEventsController) quotaEventsController.abort();
    quotaEventsController = null;
    connection(false);
    if (!visible()) { channel?.close(); channel = null; snapshots.clear(); }
  }
  return {start:startQuotaEvents,stop:stopQuotaEvents,connected:()=>quotaEventsConnected};
}
