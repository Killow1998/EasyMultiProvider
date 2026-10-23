function createTutorials({openModal, $, tr, esc}) {
  const groups = [
    ['gpt', [['native_context','00-native-context'], ['native_name','00-native-name'], ['remove_subscription','00-remove-subscription']]],
    ['external', [['add','01-add-service'], ['context','05-change-context'], ['rename','06-rename'], ['remove','04-remove-service']]],
    ['shared', [['labels','07-context-label'], ['export','03-export-service'], ['import','02-import-service']]]
  ];
  const entries = new Map(groups.flatMap(([, items]) => items));
  let selected = 'native_context';
  function select(id) {
    if (!entries.has(id)) return;
    selected = id;
    const name = tr(`tutorials.${id}`), url = `/assets/tutorial-${entries.get(id)}.gif`;
    $('tutorial_title').textContent = name;
    $('tutorial_full_size').href = url;
    $('tutorial_list').querySelectorAll('[data-tutorial]').forEach(button => {
      button.setAttribute('aria-pressed', String(button.dataset.tutorial === id));
      if (button.dataset.tutorial === id && $('tutorial_list').scrollHeight > $('tutorial_list').clientHeight) {
        $('tutorial_list').scrollTop = button.offsetTop - ($('tutorial_list').clientHeight - button.offsetHeight) / 2;
      }
    });
    const media = $('tutorial_media');
    // Replace the whole image owner so late load/error events cannot change a new selection.
    media.innerHTML = `<div class="tutorial-placeholder"><p role="status">${esc(tr('tutorials.loading'))}</p><button type="button" class="secondary" hidden></button></div>`;
    const placeholder = media.firstElementChild, status = placeholder.querySelector('p'), action = placeholder.querySelector('button');
    function play() {
      const image = new Image();
      image.alt = name; image.width = 1280; image.height = 920; image.hidden = true;
      status.textContent = tr('tutorials.loading'); status.hidden = false; action.hidden = true;
      media.setAttribute('aria-busy', 'true');
      image.onload = () => {
        if (!placeholder.isConnected) return;
        placeholder.hidden = true; image.hidden = false; media.setAttribute('aria-busy', 'false');
      };
      image.onerror = () => {
        if (!placeholder.isConnected) return;
        image.remove(); media.setAttribute('aria-busy', 'false');
        status.textContent = tr('tutorials.failed'); action.textContent = tr('tutorials.retry'); action.hidden = false;
      };
      media.append(image); image.src = url;
    }
    action.onclick = play;
    if (matchMedia('(prefers-reduced-motion: reduce)').matches) {
      media.setAttribute('aria-busy', 'false'); status.hidden = true;
      action.textContent = tr('tutorials.play'); action.hidden = false;
    } else play();
  }
  function open() {
    const list = groups.map(([group, items]) => `<div class="tutorial-group"><h3>${esc(tr(`tutorials.${group}`))}</h3>${items.map(([id]) => `<button type="button" data-tutorial="${id}" aria-controls="tutorial_demo" aria-pressed="false">${esc(tr(`tutorials.${id}`))}</button>`).join('')}</div>`).join('');
    openModal(tr('tutorials.title'), `<div id="tutorials"><nav id="tutorial_list" aria-label="${esc(tr('tutorials.title'))}">${list}</nav><div id="tutorial_demo"><div class="tutorial-heading"><h3 id="tutorial_title"></h3><a id="tutorial_full_size" target="_blank" rel="noopener">${esc(tr('tutorials.full_size'))}</a></div><div id="tutorial_media" aria-busy="true"></div></div></div>`, '', null);
    $('tutorial_list').onclick = event => {
      const button = event.target.closest('[data-tutorial]');
      if (button) select(button.dataset.tutorial);
    };
    $('tutorial_list').onkeydown = event => {
      const buttons = [...$('tutorial_list').querySelectorAll('[data-tutorial]')];
      const index = buttons.indexOf(event.target);
      if (index < 0 || !['ArrowUp','ArrowDown','Home','End'].includes(event.key)) return;
      event.preventDefault();
      const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : Math.max(0, Math.min(buttons.length - 1, index + (event.key === 'ArrowDown' ? 1 : -1)));
      buttons[next].focus({preventScroll:true});
      buttons[next].scrollIntoView({block:'nearest'});
    };
    select(selected);
  }
  return {open};
}
