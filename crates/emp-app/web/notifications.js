function createNotifications({$, tr, getLanguage}) {
  const storageKey = 'emp.notifications.v1', limit = 50;
  const island = $('notification'), inbox = $('notification_inbox'), mailbox = $('notification_mailbox');
  let items = [], currentId = null, countdown = null, entrance = null;
  function remember(entries) {
    items = read();
    const seen = new Set([...items.map(item => item.id), ...readSeen()]);
    const fresh = entries.filter(item => !seen.has(item.id));
    if (!fresh.length) return;
    items = [...items, ...fresh].sort((a,b) => a.time-b.time).slice(-limit);
    try {
      localStorage.setItem(storageKey + '.seen', JSON.stringify([...seen, ...fresh.map(item => item.id)].slice(-2 * limit)));
    } catch (_) {}
    save();
    refresh();
  }
  function readSeen() {
    try {
      const ids = JSON.parse(localStorage.getItem(storageKey + '.seen') || '[]');
      return Array.isArray(ids) ? ids : [];
    } catch (_) { return []; }
  }
  // Lucide info / circle-alert icons (ISC).
  function icon(error) {
    return `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="10"/>${error ? '<path d="M12 8v4M12 16h.01"/>' : '<path d="M12 16v-4M12 8h.01"/>'}</svg>`;
  }
  function read() {
    try {
      const stored = JSON.parse(localStorage.getItem(storageKey) || '[]');
      if (!Array.isArray(stored)) return [];
      return stored.filter(item => item && typeof item.id === 'string' && typeof item.message === 'string'
        && Number.isFinite(item.time) && item.time >= 0 && item.time <= 8640000000000000)
        .slice(-limit).map(item => ({id:item.id, message:item.message.slice(0,10000), time:item.time, error:item.error === true, read:item.read === true}));
    } catch (_) { return items; }
  }
  function save() {
    try { localStorage.setItem(storageKey, JSON.stringify(items)); } catch (_) {}
  }
  const isOpen = () => inbox.matches(':popover-open');
  function refresh() {
    const unread = items.filter(item => !item.read).length;
    $('notification_unread').textContent = String(unread);
    $('notification_unread').hidden = !unread;
    mailbox.ariaLabel = mailbox.title = unread ? tr('notifications.unread', {count:unread}) : tr('notifications.inbox');
    $('notification_clear').title = tr('notifications.clear');
    $('notification_inbox_title').title = tr('notifications.storage');
    if (isOpen()) renderHistory();
  }
  function renderHistory() {
    const list = $('notification_history');
    list.replaceChildren();
    $('notification_empty').hidden = items.length > 0;
    $('notification_clear').disabled = !items.length;
    $('notification_count').textContent = String(items.length);
    for (const item of [...items].reverse()) {
      const row = document.createElement('li'), symbol = document.createElement('span');
      row.dataset.kind = item.error ? 'error' : 'message';
      symbol.className = 'notification-symbol'; symbol.innerHTML = icon(item.error);
      symbol.setAttribute('role','img'); symbol.ariaLabel = tr(item.error ? 'notifications.error' : 'notifications.message');
      const body = document.createElement('div'), text = document.createElement('p'), time = document.createElement('time');
      text.textContent = item.message;
      time.dateTime = new Date(item.time).toISOString();
      time.textContent = new Intl.DateTimeFormat(getLanguage(), {dateStyle:'short', timeStyle:'medium'}).format(item.time);
      body.append(text,time); row.append(symbol,body); list.append(row);
    }
  }
  let stickies = [];
  const top = () => stickies.at(-1) || null;
  const showingSticky = () => stickies.some(item => item.id === currentId);
  const STACK_LIMIT = 3;
  const actionButtons = actions => (actions || []).map(action => {
    const button = document.createElement('button');
    button.type = 'button'; button.textContent = action.label;
    button.className = action.primary ? 'notification-action primary' : 'notification-action';
    for (const [name,value] of Object.entries(action.attributes || {})) button.setAttribute(name, value);
    button.onclick = () => action.run?.();
    return button;
  });
  const stacked = () => stickies.length > 1 || stickies.some(item => item.group);
  function displayStack() {
    const fresh = island.dataset.stack !== 'true' || island.hidden;
    if (fresh) hide();
    const shown = stickies.slice(0, STACK_LIMIT), rest = stickies.length - shown.length;
    const list = $('notification_stack'), before = new Set([...list.children].map(row => row.dataset.id));
    currentId = shown[0].id;
    island.dataset.kind = 'message'; island.dataset.persist = 'true'; island.dataset.stack = 'true';
    $('notification_symbol').innerHTML = icon(false);
    $('notification_close').hidden = true;
    $('notification_actions').replaceChildren();
    $('status').setAttribute('role','status'); $('status').setAttribute('aria-live','polite');
    $('status').textContent = tr('notifications.pending', {count:stickies.length});
    list.replaceChildren(...shown.map(item => {
      const row = document.createElement('li'), text = document.createElement('p'), actions = document.createElement('span');
      row.dataset.id = item.id; text.textContent = item.message; actions.className = 'notification-row-actions';
      actions.append(...actionButtons(item.actions));
      row.append(text, actions);
      if (!before.has(item.id) && !fresh && !matchMedia('(prefers-reduced-motion: reduce)').matches) {
        row.animate([{opacity:0,transform:'translateY(-6px)'},{opacity:1,transform:'none'}],{duration:220,easing:'ease-out'});
      }
      return row;
    }));
    if (rest > 0) {
      const more = document.createElement('li'); more.className = 'notification-stack-more';
      more.textContent = tr('notifications.more_waiting', {count:rest}); list.append(more);
    }
    list.hidden = false;
    if (fresh) {
      island.hidden = false; island.showPopover();
      if (!matchMedia('(prefers-reduced-motion: reduce)').matches) {
        entrance = island.animate([{opacity:0,transform:'translate(-50%, -8px) scale(.97)'},{opacity:1,transform:'translate(-50%, 0) scale(1)'}],{duration:250,easing:'ease-out'});
      }
    }
  }
  function hide() {
    countdown?.cancel(); entrance?.cancel(); countdown = entrance = null;
    island.dataset.stack = 'false'; $('notification_stack').replaceChildren(); $('notification_stack').hidden = true;
    if (island.contains(document.activeElement)) mailbox.focus({preventScroll:true});
    if (island.matches(':popover-open')) island.hidePopover();
    island.hidden = true; $('status').textContent = ''; $('notification_actions').replaceChildren(); currentId = null;
  }
  function restore() { if (top() && !currentId) display(top()); }
  function dismiss() {
    if (showingSticky()) return;
    items = read();
    const item = items.find(item => item.id === currentId);
    if (item) { item.read = true; save(); refresh(); }
    hide(); restore();
  }
  function pauseWhenReading() {
    if (!countdown) return;
    const paused = document.hidden || !document.hasFocus() || island.matches(':hover,:focus-within');
    island.dataset.paused = String(paused);
    if (paused) countdown.pause(); else countdown.play();
  }
  function display(item) {
    if (item.persist && stacked()) return displayStack();
    hide();
    currentId = item.id;
    island.dataset.kind = item.error ? 'error' : 'message';
    island.dataset.persist = String(Boolean(item.persist));
    $('notification_symbol').innerHTML = icon(item.error);
    $('status').setAttribute('role', item.quiet ? 'none' : item.error ? 'alert' : 'status');
    $('status').setAttribute('aria-live', item.quiet ? 'off' : item.error ? 'assertive' : 'polite');
    $('notification_close').hidden = Boolean(item.persist);
    $('notification_actions').append(...actionButtons(item.actions));
    island.hidden = false; island.showPopover(); $('status').textContent = item.message;
    if (!matchMedia('(prefers-reduced-motion: reduce)').matches) {
      entrance = island.animate([{opacity:0,transform:'translate(-50%, -8px) scale(.97)'},{opacity:1,transform:'translate(-50%, 0) scale(1)'}],{duration:250,easing:'ease-out'});
    }
    if (item.persist) return;
    countdown = $('notification_progress').animate([{transform:'scaleX(1)'},{transform:'scaleX(0)'}],{duration:item.error ? 10000 : 5000,fill:'forwards'});
    countdown.onfinish = () => { hide(); restore(); };
    pauseWhenReading();
  }
  function show(message, error = false, {quiet = false, actions = null, persist = false, group = null} = {}) {
    if (!message) return null;
    const item = {id:`${Date.now()}-${Math.random().toString(36).slice(2)}`, message:String(message).slice(0,10000), time:Date.now(), error:Boolean(error), read:isOpen()};
    if (persist) {
      const entry = {...item, quiet, actions, persist:true, group};
      stickies.push(entry);
      if (currentId && !showingSticky()) return item.id;
      display(entry);
      return item.id;
    }
    items = [...read(),item].slice(-limit); save(); refresh();
    display({...item, quiet, actions});
    return item.id;
  }
  function update(id, message) {
    const entry = stickies.find(item => item.id === id);
    if (!entry) return;
    entry.message = String(message);
    if (island.dataset.stack === 'true') { const row = $('notification_stack').querySelector(`[data-id="${CSS.escape(id)}"] p`); if (row) row.textContent = entry.message; }
    else if (currentId === id) $('status').textContent = entry.message;
  }
  function withdraw(id) {
    if (!id) return;
    const known = stickies.some(item => item.id === id);
    stickies = stickies.filter(item => item.id !== id);
    if (island.dataset.stack === 'true' && known) { if (stickies.length && stacked()) displayStack(); else { hide(); restore(); } }
    else if (currentId === id) { hide(); restore(); }
  }
  $('notification_close').onclick = dismiss;
  $('notification_clear').onclick = () => { items = []; save(); if (!showingSticky()) { hide(); restore(); } refresh(); };
  inbox.addEventListener('beforetoggle', event => {
    if (event.newState === 'open') {
      items = read().map(item => ({...item,read:true})); save(); if (!showingSticky()) hide(); refresh(); renderHistory();
    }
  });
  inbox.addEventListener('toggle', () => mailbox.setAttribute('aria-expanded',String(isOpen())));
  island.addEventListener('keydown', event => {
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); dismiss(); }
  });
  for (const event of ['pointerenter','pointerleave','focusin','focusout']) island.addEventListener(event, () => queueMicrotask(pauseWhenReading));
  document.addEventListener('visibilitychange',pauseWhenReading);
  window.addEventListener('blur',pauseWhenReading); window.addEventListener('focus',pauseWhenReading);
  window.addEventListener('pagehide',() => { stickies = []; hide(); });
  window.addEventListener('storage', event => { if (event.key === storageKey) { items = read(); refresh(); } });
  items = read(); refresh();
  return {show,update,withdraw,dismiss,refresh,isOpen,remember};
}
