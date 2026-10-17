const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const web = path.join(__dirname, '../web');
const copyContext = vm.createContext({});
for (const file of ['i18n.js', ...fs.readdirSync(web).filter(name => /^copy-.*\.js$/.test(name))]) {
  vm.runInContext(fs.readFileSync(path.join(web,file),'utf8'), copyContext, {filename:file});
}
const translate = language => (key, values) => typeof values === 'string' ? (language === 'en' ? values : key) : copyContext.translateUiCopy(key,language,values);
const tr = translate('en');
const esc = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return {promise, resolve, reject}; };
function feature(file, factory, globals = {}) {
  const context = vm.createContext({Headers, URL, URLSearchParams, Blob, AbortController, structuredClone, ...globals});
  if(file === 'call-reports.js') for(const dependency of ['call-outcomes.js','performance-chart.js','performance.js']) vm.runInContext(fs.readFileSync(path.join(web,dependency),'utf8'),context,{filename:dependency});
  vm.runInContext(fs.readFileSync(path.join(web, file), 'utf8'), context, {filename:file});
  return context[factory];
}
module.exports = {assert, fs, path, vm, web, tr, translate, esc, deferred, feature};
