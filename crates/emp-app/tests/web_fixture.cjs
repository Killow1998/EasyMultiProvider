const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const web = path.join(__dirname, '../web');
const tr = (_zh, en) => en;
const esc = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return {promise, resolve, reject}; };
function feature(file, factory, globals = {}) {
  const context = vm.createContext({Headers, URL, URLSearchParams, Blob, AbortController, structuredClone, ...globals});
  if(file === 'call-reports.js') vm.runInContext(fs.readFileSync(path.join(web,'call-outcomes.js'),'utf8'),context,{filename:'call-outcomes.js'});
  vm.runInContext(fs.readFileSync(path.join(web, file), 'utf8'), context, {filename:file});
  return context[factory];
}
module.exports = {assert, fs, path, vm, web, tr, esc, deferred, feature};
