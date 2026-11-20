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
  // Lucide info / circle-alert icons (ISC); the message itself is always plain text.
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
    try { localStorage.setItem(storageKey, JSON.stringify(items)); } catch (_) { /* Still usable without browser storage. */ }
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
  // A persistent notice carries actions (copy, approve, decline) and stays until its owner withdraws it;
  // ordinary notices pass over it briefly and then give the island back.
  let sticky = null;
  function hide() {
    countdown?.cancel(); entrance?.cancel(); countdown = entrance = null;
    if (island.contains(document.activeElement)) mailbox.focus({preventScroll:true});
    if (island.matches(':popover-open')) island.hidePopover();
    island.hidden = true; $('status').textContent = ''; $('notification_actions').replaceChildren(); currentId = null;
  }
  function restore() { if (sticky && !currentId) display(sticky); }
  function dismiss() {
    if (sticky && currentId === sticky.id) return;
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
    hide();
    currentId = item.id;
    island.dataset.kind = item.error ? 'error' : 'message';
    island.dataset.persist = String(Boolean(item.persist));
    $('notification_symbol').innerHTML = icon(item.error);
    $('status').setAttribute('role', item.quiet ? 'none' : item.error ? 'alert' : 'status');
    $('status').setAttribute('aria-live', item.quiet ? 'off' : item.error ? 'assertive' : 'polite');
    $('notification_close').hidden = Boolean(item.persist);
    for (const action of item.actions || []) {
      const button = document.createElement('button');
      button.type = 'button'; button.textContent = action.label;
      button.className = action.primary ? 'notification-action primary' : 'notification-action';
      for (const [name,value] of Object.entries(action.attributes || {})) button.setAttribute(name, value);
      button.onclick = () => action.run?.();
      $('notification_actions').append(button);
    }
    island.hidden = false; island.showPopover(); $('status').textContent = item.message;
    if (!matchMedia('(prefers-reduced-motion: reduce)').matches) {
      entrance = island.animate([{opacity:0,transform:'translate(-50%, -8px) scale(.97)'},{opacity:1,transform:'translate(-50%, 0) scale(1)'}],{duration:250,easing:'ease-out'});
    }
    if (item.persist) return;
    // The bar is the timer: pausing it also pauses dismissal, without a second clock.
    countdown = $('notification_progress').animate([{transform:'scaleX(1)'},{transform:'scaleX(0)'}],{duration:item.error ? 10000 : 5000,fill:'forwards'});
    countdown.onfinish = () => { hide(); restore(); };
    pauseWhenReading();
  }
  // `quiet` is for text another live region (the dialog status) already announces.
  function show(message, error = false, {quiet = false, actions = null, persist = false} = {}) {
    if (!message) return null;
    const item = {id:`${Date.now()}-${Math.random().toString(36).slice(2)}`, message:String(message).slice(0,10000), time:Date.now(), error:Boolean(error), read:isOpen()};
    if (persist) {
      sticky = {...item, quiet, actions, persist:true};
      display(sticky);
      return item.id;
    }
    items = [...read(),item].slice(-limit); save(); refresh();
    display({...item, quiet, actions});
    return item.id;
  }
  function update(id, message) {
    if (!sticky || sticky.id !== id) return;
    sticky.message = String(message);
    if (currentId === id) $('status').textContent = sticky.message;
  }
  function withdraw(id) {
    if (!id) return;
    if (sticky?.id === id) sticky = null;
    if (currentId === id) { hide(); restore(); }
  }
  $('notification_close').onclick = dismiss;
  $('notification_clear').onclick = () => { items = []; save(); if (!sticky || currentId !== sticky.id) { hide(); restore(); } refresh(); };
  inbox.addEventListener('beforetoggle', event => {
    if (event.newState === 'open') {
      items = read().map(item => ({...item,read:true})); save(); if (!sticky || currentId !== sticky.id) hide(); refresh(); renderHistory();
    }
  });
  inbox.addEventListener('toggle', () => mailbox.setAttribute('aria-expanded',String(isOpen())));
  island.addEventListener('keydown', event => {
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); dismiss(); }
  });
  for (const event of ['pointerenter','pointerleave','focusin','focusout']) island.addEventListener(event, () => queueMicrotask(pauseWhenReading));
  document.addEventListener('visibilitychange',pauseWhenReading);
  window.addEventListener('blur',pauseWhenReading); window.addEventListener('focus',pauseWhenReading);
  window.addEventListener('pagehide',() => { sticky = null; hide(); });
  window.addEventListener('storage', event => { if (event.key === storageKey) { items = read(); refresh(); } });
  items = read(); refresh();
  return {show,update,withdraw,dismiss,refresh,isOpen,remember};
}
