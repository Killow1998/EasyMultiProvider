const button = document.querySelector('#theme');
const systemDark = () => matchMedia('(prefers-color-scheme: dark)').matches;
let savedTheme;
try { savedTheme = localStorage.getItem('emp-site-theme'); } catch (_) {}
if (['light','dark'].includes(savedTheme)) document.documentElement.dataset.theme = savedTheme;
button.addEventListener('click', () => {
  const dark = document.documentElement.dataset.theme ? document.documentElement.dataset.theme === 'dark' : systemDark();
  const theme = dark ? 'light' : 'dark';
  document.documentElement.dataset.theme = theme;
  try { localStorage.setItem('emp-site-theme', theme); } catch (_) {}
});
const examples = {work:['工作账号 · GPT','work / gpt-…'],personal:['个人账号 · GPT','personal / gpt-…'],service:['研究服务 · Claude','research / claude-…']};
document.querySelectorAll('[data-demo]').forEach(source => {
  source.setAttribute('aria-pressed', String(source.classList.contains('selected')));
  source.addEventListener('click', () => {
    document.querySelectorAll('[data-demo]').forEach(other => { other.classList.toggle('selected', other === source); other.setAttribute('aria-pressed', String(other === source)); });
    const [label, route] = examples[source.dataset.demo];
    document.querySelector('#demo-model').textContent = label;
    document.querySelector('#demo-route').textContent = route;
  });
});
