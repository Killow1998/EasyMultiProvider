"use strict";

const assert = require("assert");
const fs = require("fs");
const vm = require("vm");

class ClassList {
  constructor(initial = []) { this.values = new Set(initial); }
  add(value) { this.values.add(value); }
  remove(value) { this.values.delete(value); }
  contains(value) { return this.values.has(value); }
}

const elements = new Map();
let discoveredOptions = [];

class Element {
  constructor(id = "") {
    this.id = id;
    this.textContent = "";
    this.className = "";
    this.dataset = {};
    this.style = {};
    this.hidden = false;
    this.disabled = false;
    this.checked = false;
    this.value = "";
    this.onclick = null;
    this.classList = new ClassList(id === "modal_backdrop" ? ["hidden"] : []);
    this._innerHTML = "";
  }
  set innerHTML(value) {
    this._innerHTML = String(value);
    if (this.id === "modal_body") parseDiscoveredOptions(this._innerHTML);
  }
  get innerHTML() { return this._innerHTML; }
  querySelector(selector) {
    if (selector === 'input[name="discovered_model"]') return this.input || null;
    return null;
  }
  click() { if (this.onclick) return this.onclick(); }
  remove() {}
}

function unescapeHtml(value) {
  return value.replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, "&");
}

function parseDiscoveredOptions(html) {
  discoveredOptions = [];
  const pattern = /<label class="model-option" data-discovered-option data-search="([^"]*)"><input type="checkbox" name="discovered_model" value="([^"]*)"([^>]*)>/g;
  for (const match of html.matchAll(pattern)) {
    const option = new Element();
    option.dataset.search = unescapeHtml(match[1]);
    const input = new Element();
    input.value = unescapeHtml(match[2]);
    input.checked = /\schecked(?:\s|$)/.test(match[3]);
    input.closest = selector => selector === "[data-discovered-option]" ? option : null;
    option.input = input;
    discoveredOptions.push(option);
  }
}

function getElement(id) {
  if (!elements.has(id)) elements.set(id, new Element(id));
  return elements.get(id);
}

const document = {
  getElementById: getElement,
  querySelectorAll(selector) {
    if (selector === "[data-discovered-option]") return discoveredOptions;
    if (selector === 'input[name="discovered_model"]') {
      return discoveredOptions.map(option => option.input);
    }
    if (selector === 'input[name="discovered_model"]:checked') {
      return discoveredOptions.map(option => option.input).filter(input => input.checked);
    }
    return [];
  },
  createElement: () => new Element(),
  body: {appendChild() {}},
};

for (const id of [
  "status", "modal_backdrop", "modal_title", "modal_body", "modal_status",
  "modal_submit", "integration", "integration_badge", "integration_title",
  "integration_summary", "integration_commands", "integration_enable",
  "integration_restore", "integration_reload", "native_catalog_path",
  "diagnostics_summary", "diagnostics_records", "listen_info", "accounts",
  "providers", "models",
]) getElement(id);

const html = fs.readFileSync(process.argv[2], "utf8");
const match = html.match(/<script>([\s\S]*)<\/script>/);
assert(match, "page script not found");
const script = match[1].replace(/\nload\(\);\s*$/, "\n");
const context = vm.createContext({
  console,
  document,
  window: {location: {origin: "http://127.0.0.1:4200"}},
  URL,
  TextEncoder,
  Uint8Array,
  setTimeout,
  clearTimeout,
  confirm: () => true,
  fetch: async () => { throw new Error("unexpected fetch"); },
  btoa: value => Buffer.from(value, "binary").toString("base64"),
});
vm.runInContext(script, context, {filename: "index.html"});

function run(source) { return vm.runInContext(source, context); }

async function integrationBehavior() {
  const calls = [];
  let integration = {
    configuration: {state: "native", relation: "unleased", conflicts: []},
    runtime: {state: "not_checked", target: "native", verified: false, action_required: false, detail: ""},
    service_health: "ready",
    next_action: "enable default Codex",
  };
  context.__apiStub = async (path, options = {}) => {
    calls.push({path, options});
    if (path === "/api/config") return {native_catalog_path: "", accounts: [], providers: [], models: []};
    if (path === "/api/diagnostics") return {capacity: 64, records: []};
    if (path === "/api/integration/enable") {
      integration = {
        configuration: {state: "emp_applied", relation: "applied", conflicts: []},
        runtime: {state: "stopped_waiting_for_start", target: "emp", verified: false, action_required: false, detail: ""},
        service_health: "ready",
        next_action: "none",
      };
      return integration;
    }
    if (path === "/api/integration") return integration;
    throw new Error("unexpected API " + path);
  };
  run("api = __apiStub; state = {native_catalog_path:'', accounts:[], providers:[], models:[]}");
  run("confirmIntegrationAction('enable')");
  assert(!getElement("modal_backdrop").classList.contains("hidden"), "confirmation modal must open");
  assert.match(getElement("modal_body").innerHTML, /disconnect|中断|断开/i);
  await getElement("modal_submit").click();
  const enable = calls.find(call => call.path === "/api/integration/enable");
  assert(enable, "enable endpoint was not called");
  assert.deepStrictEqual(JSON.parse(enable.options.body), {confirm_reload: true});
  assert(!calls.some(call => call.path === "/api/integration/sync"), "obsolete second sync was called");
  assert.match(getElement("status").textContent, /next|下次/i);
  assert(getElement("modal_backdrop").classList.contains("hidden"), "successful modal must close");

  run("renderIntegration({configuration:{state:'emp_applied',relation:'applied',conflicts:[]},runtime:{state:'emp_loaded',target:'emp',verified:true,action_required:false,detail:''},service_health:'ready',next_action:'none'})");
  assert.match(getElement("integration_summary").textContent, /loaded|已加载/i);
  run("renderIntegration({configuration:{state:'emp_applied',relation:'applied',conflicts:[]},runtime:{state:'stopped_waiting_for_start',target:'emp',verified:false,action_required:false,detail:''},service_health:'ready',next_action:'none'})");
  assert.match(getElement("integration_summary").textContent, /next|下次/i);

  context.__apiStub = async (path, options = {}) => {
    if (path === "/api/integration/restore") throw new Error("restore failed safely");
    if (path === "/api/config") return {native_catalog_path: "", accounts: [], providers: [], models: []};
    if (path === "/api/integration") return integration;
    if (path === "/api/diagnostics") return {capacity: 64, records: []};
    throw new Error("unexpected API " + path);
  };
  run("api = __apiStub; confirmIntegrationAction('restore')");
  await getElement("modal_submit").click();
  assert.match(getElement("modal_status").textContent, /restore failed safely/);
  assert.match(getElement("status").textContent, /restore failed safely/);
  assert(!getElement("modal_backdrop").classList.contains("hidden"), "failed modal must stay open");

  context.__apiStub = async (path) => {
    if (path === "/api/integration") throw new Error("integration request failed");
    throw new Error("unexpected API " + path);
  };
  run("api = __apiStub");
  const loaded = await run("loadIntegration()");
  assert.strictEqual(loaded, false);
  assert.strictEqual(getElement("integration").dataset.state, "unavailable");
  assert.strictEqual(getElement("integration_badge").textContent, "Unavailable");
  assert.match(getElement("integration_summary").textContent, /integration request failed/);
  assert.match(getElement("integration_summary").textContent, /stale|过期/i);
  assert.notStrictEqual(getElement("integration_badge").textContent, "Conflict");

  context.__apiStub = async (path) => {
    if (path === "/api/config") return {native_catalog_path: "", accounts: [], providers: [], models: []};
    if (path === "/api/integration/enable") {
      const error = new Error("runtime verification warning");
      error.payload = {
        configuration: {state: "emp_applied", relation: "applied", conflicts: []},
        runtime: {state: "verification_failed", target: "emp", verified: false, action_required: true, detail: "partial catalog"},
        service_health: "ready",
        next_action: "reconnect Codex",
      };
      throw error;
    }
    throw new Error("unexpected API " + path);
  };
  run("api = __apiStub; state = {native_catalog_path:'', accounts:[], providers:[], models:[]}; confirmIntegrationAction('enable')");
  await getElement("modal_submit").click();
  assert.strictEqual(getElement("integration_badge").textContent, "EMP applied");
  assert.match(getElement("integration_summary").textContent, /没有加载完整|partial catalog/i);
  assert.strictEqual(getElement("integration_reload").hidden, false);
}

function pickerBehavior() {
  run("state = {providers:[{id:'provider-a',name:'Provider A'}],models:[{id:'provider-a/imported',provider:'provider-a',upstream_id:'imported',enabled:true}]}");
  run("openModelImportModal('provider-a', [{upstream_id:'imported',display_name:'Imported'},{upstream_id:'beta',display_name:'Beta Model'},{upstream_id:'gamma',display_name:'Gamma Model'}])");
  assert.strictEqual(discoveredOptions.length, 3);
  assert.deepStrictEqual(discoveredOptions.map(option => option.input.checked), [true, false, false]);
  assert.match(getElement("discovered_count").textContent, /1\/3/);
  assert.strictEqual(getElement("discovered_select_all").textContent, "全选");
  assert.strictEqual(getElement("discovered_clear_all").textContent, "全不选");

  run("filterDiscoveredModels('beta')");
  assert.deepStrictEqual(discoveredOptions.map(option => option.hidden), [true, false, true]);
  assert.strictEqual(getElement("discovered_select_all").textContent, "全选搜索结果");
  assert.strictEqual(getElement("discovered_clear_all").textContent, "全不选搜索结果");
  run("setDiscoveredChecks(true)");
  assert.deepStrictEqual(discoveredOptions.map(option => option.input.checked), [true, true, false]);
  run("filterDiscoveredModels('gamma')");
  run("setDiscoveredChecks(false)");
  assert.deepStrictEqual(discoveredOptions.map(option => option.input.checked), [true, true, false]);
  assert.match(getElement("discovered_count").textContent, /2\/3/);
  run("filterDiscoveredModels('')");
  assert.strictEqual(getElement("discovered_select_all").textContent, "全选");
  assert.strictEqual(getElement("discovered_clear_all").textContent, "全不选");

  run("state = {providers:[{id:'provider-a',name:'Provider A'}],models:[]}; openModelImportModal('provider-a', [{upstream_id:'one'},{upstream_id:'two'}])");
  assert.deepStrictEqual(discoveredOptions.map(option => option.input.checked), [false, false]);
}

function duplicateAccountBehavior() {
  run("state = {accounts:[{id:'same-login-account',prefix:'same-login-account',duplicate:true,duplicate_of:'当前 Codex 登录',credential_set:true},{id:'usable-account',prefix:'usable-account',duplicate:false,credential_set:true}]}; renderAccounts()");
  const html = getElement("accounts").innerHTML;
  assert.match(html, /same-login-account/);
  assert.match(html, /usable-account/);
  assert.match(html, /模型显示设置作用于原生列表/);
}

function modelGroupBehavior() {
  run("state = {providers:[{id:'provider-b',name:'Provider B'},{id:'provider-a',name:'Provider A'}],models:[{id:'provider-a/new',provider:'provider-a',enabled:true,created_at:30},{id:'provider-b/old',provider:'provider-b',enabled:true,created_at:10},{id:'provider-b/new',provider:'provider-b',enabled:true,created_at:20},{id:'provider-b/hidden',provider:'provider-b',enabled:false,created_at:40}]}; renderModels()");
  const html = getElement("models").innerHTML;
  assert(html.indexOf("Provider B") < html.indexOf("Provider A"), "provider config order must be preserved");
  assert(html.indexOf("provider-b/new") < html.indexOf("provider-b/old"), "newer visible models must sort first");
  assert(html.indexOf("provider-b/old") < html.indexOf("provider-b/hidden"), "hidden models must sort last");
}

(async () => {
  await integrationBehavior();
  pickerBehavior();
  duplicateAccountBehavior();
  modelGroupBehavior();
  process.stdout.write("web DOM behavior: ok\n");
})().catch(error => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
