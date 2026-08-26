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
let catalogAliases = [];
let catalogContexts = [];
let catalogSummaries = [];

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
    if (this.id === "catalog_display_models") parseCatalogDisplay(this._innerHTML);
  }
  get innerHTML() { return this._innerHTML; }
  querySelector(selector) {
    if (selector === 'input[name="discovered_model"]') return this.input || null;
    return null;
  }
  click() { if (this.onclick) return this.onclick(); }
  remove() {}
}

function parseCatalogDisplay(html) {
  catalogAliases = [];
  catalogContexts = [];
  catalogSummaries = [];
  for (const match of html.matchAll(/<input data-catalog-alias data-route="([^"]*)" value="([^"]*)"/g)) {
    const input = new Element(); input.dataset.route = unescapeHtml(match[1]); input.value = unescapeHtml(match[2]); catalogAliases.push(input);
  }
  for (const match of html.matchAll(/<input type="checkbox" data-catalog-context data-route="([^"]*)"([^>]*)>/g)) {
    const input = new Element(); input.dataset.route = unescapeHtml(match[1]); input.checked = /\schecked(?:\s|$)/.test(match[2]); catalogContexts.push(input);
  }
  for (const match of html.matchAll(/<select data-catalog-summary data-route="([^"]*)"[^>]*>([\s\S]*?)<\/select>/g)) {
    const select = new Element(); select.dataset.route = unescapeHtml(match[1]); const selected = match[2].match(/<option value="([^"]*)" selected>/); select.value = selected ? selected[1] : "auto"; catalogSummaries.push(select);
  }
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
    if (selector === "[data-catalog-alias]") return catalogAliases;
    if (selector === "[data-catalog-context]") return catalogContexts;
    if (selector === "[data-catalog-summary]") return catalogSummaries;
    return [];
  },
  documentElement: {lang: "", dataset: {}},
  addEventListener() {},
  createElement: () => new Element(),
  body: {appendChild() {}},
};

for (const id of [
  "status", "modal_backdrop", "modal_title", "modal_body", "modal_status",
  "modal_submit", "integration", "integration_badge", "integration_title",
  "integration_summary", "integration_enable", "integration_restore",
  "integration_reload", "language_select", "theme_select",
  "catalog_display_search", "catalog_display_toggle", "catalog_display_models",
  "diagnostics_summary", "diagnostics_records", "accounts", "providers", "models",
]) getElement(id);

const html = fs.readFileSync(process.argv[2], "utf8");
const match = html.match(/<script>([\s\S]*)<\/script>/);
assert(match, "page script not found");
const script = match[1].replace(/\nload\(\);\s*$/, "\n");
const context = vm.createContext({
  console,
  document,
  window: {location: {origin: "http://127.0.0.1:4200"}},
  localStorage: {values:new Map(), getItem(key) { return this.values.get(key) || null; }, setItem(key, value) { this.values.set(key, String(value)); }},
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
  assert.match(getElement("integration_badge").textContent, /Unavailable|不可用/);
  assert.match(getElement("integration_summary").textContent, /integration request failed/);
  assert.match(getElement("integration_summary").textContent, /stale|过期/i);
  assert.doesNotMatch(getElement("integration_badge").textContent, /^Conflict$|^配置冲突$/);

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
  assert.match(getElement("integration_badge").textContent, /EMP (?:applied|已启用)/);
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
  assert.match(html, /可见性设置作用于原生列表/);
}

function modelGroupBehavior() {
  run("state = {providers:[{id:'provider-b',name:'Provider B'},{id:'provider-a',name:'Provider A'}],models:[{id:'provider-a/new',provider:'provider-a',enabled:true,created_at:30},{id:'provider-b/old',provider:'provider-b',enabled:true,created_at:10},{id:'provider-b/new',provider:'provider-b',enabled:true,created_at:20},{id:'provider-b/hidden',provider:'provider-b',enabled:false,created_at:40}]}; renderModels()");
  const html = getElement("models").innerHTML;
  assert(html.indexOf("Provider B") < html.indexOf("Provider A"), "provider config order must be preserved");
  assert(html.indexOf("provider-b/new") < html.indexOf("provider-b/old"), "newer visible models must sort first");
  assert(html.indexOf("provider-b/old") < html.indexOf("provider-b/hidden"), "hidden models must sort last");
}

function officialPresetBehavior() {
  const presets = run("officialProviders");
  assert.strictEqual(presets.openrouter.base_url, "https://openrouter.ai/api/v1");
  assert.strictEqual(presets.openrouter.protocol, "responses");
  assert.strictEqual(presets.openrouter.auth_mode, "api_key");
  assert.strictEqual(presets.xai.base_url, "https://api.x.ai/v1");
  assert.strictEqual(presets.xai.protocol, "responses");
  assert.strictEqual(presets.xai.auth_mode, "api_key");
  assert.strictEqual(presets.moonshot.base_url, "https://api.moonshot.ai/v1");
  assert.strictEqual(presets.moonshot.protocol, "chat_completions");
  assert.strictEqual(presets.moonshot.auth_mode, "api_key");
  assert.strictEqual(presets.zhipu.base_url, "https://api.z.ai/api/paas/v4");
  assert.strictEqual(presets.zhipu.protocol, "chat_completions");
  assert.strictEqual(presets.zhipu.auth_mode, "api_key");
  assert.strictEqual(presets.deepseek.base_url, "https://api.deepseek.com");
  assert(!("meta" in presets), "Meta must not be an official preset");
}

function capabilityMetadataBehavior() {
  context.__testMultimodal = [{id:'provider-a/multimodal',provider:'provider-a',upstream_id:'multimodal',enabled:true,input_modalities:['text','image'],output_modalities:['text','audio'],supported_protocols:['responses','chat_completions'],capability_sources:{input_modalities:{source:'official'},output_modalities:{source:'advertised'},supported_protocols:{source:'observed'}}}];
  context.__testUnconfirmed = [{id:'provider-a/unconfirmed',provider:'provider-a',upstream_id:'unconfirmed',enabled:true,input_modalities:['text','image'],capability_sources:{input_modalities:{source:'unknown'}}}];
  run("__testState = {providers:[{id:'provider-a',name:'Provider A'}],models:[]}");
  run("state.models.push(...__testMultimodal); state.models.push(...__testUnconfirmed); renderModels()");
  const html = getElement("models").innerHTML;
  assert.match(html, /输入 文本\/图像/, "confirmed input modalities must display");
  assert.match(html, /输出 文本\/音频/, "confirmed output modalities must display");
  assert.match(html, /Responses\/Chat Completions/, "confirmed protocols must display");
  const unconfirmedStart = html.indexOf("unconfirmed");
  const unconfirmedEnd = html.indexOf("</tr>", unconfirmedStart);
  const unconfirmedCell = unconfirmedEnd > unconfirmedStart ? html.substring(unconfirmedStart, unconfirmedEnd) : "";
  assert.doesNotMatch(unconfirmedCell, /输入 文本/, "unknown provenance must not display as confirmed support");

  context.__testPickerModels = [
    {upstream_id:'multimodal',display_name:'Multimodal Model',input_modalities:['text','image'],output_modalities:['text','audio'],supported_protocols:['responses','chat_completions'],capability_sources:{input_modalities:{source:'official'},output_modalities:{source:'advertised'},supported_protocols:{source:'observed'}}},
    {upstream_id:'unconfirmed',display_name:'Unconfirmed Model',input_modalities:['text','image'],capability_sources:{input_modalities:{source:'unknown'}}}
  ];
  run("__testState2 = {providers:[{id:'provider-a',name:'Provider A'}],models:[]}; state = __testState2; openModelImportModal('provider-a', __testPickerModels)");
  const pickerHtml = getElement("modal_body").innerHTML;
  assert.match(pickerHtml, /输入 文本\/图像/, "confirmed input modalities must show in picker");
  assert.match(pickerHtml, /输出 文本\/音频/, "confirmed output modalities must show in picker");
  assert.match(pickerHtml, /Responses\/Chat Completions/, "confirmed protocols must show in picker");
  const pickerSplit = getElement("modal_body").innerHTML.split("Unconfirmed Model")[1];
  const pickerUnconfirmedHtml = pickerSplit ? pickerSplit.split("</label>")[0] : "";
  assert.doesNotMatch(pickerUnconfirmedHtml, /输入 文本/, "unknown provenance must not display as confirmed support in picker");
}

async function presentationBehavior() {
  run("state = {catalog_presentations:{'provider-a/other':{catalog_alias:'General',show_context:true,reasoning_summary:'auto'}},catalog_models:[{id:'native-model',default_display_name:'Native Model',display_name:'[ 258K]  Native Model',context_window:258000,source_type:'native',source_id:'',supports_reasoning_summaries:true},{id:'provider-a/model',default_display_name:'provider-a/model',display_name:'[ 258K]  provider-a/model',context_window:258000,source_type:'provider',source_id:'provider-a',supports_reasoning_summaries:true}],subscription_models:[{id:'native-model',display_name:'Native Model',context_window:258000}],providers:[{id:'provider-a',name:'Provider A'}],accounts:[],models:[{id:'provider-a/model',provider:'provider-a',upstream_id:'model',display_name:'Model',context_window:258000,enabled:true}]} ");
  run("renderCatalogDisplay()");
  assert.match(getElement("catalog_display_models").innerHTML, /data-catalog-alias/);
  assert.match(getElement("catalog_display_models").innerHTML, /provider-a\/model/);
  const alias = catalogAliases.find(input => input.dataset.route === "provider-a/model");
  const contextInput = catalogContexts.find(input => input.dataset.route === "provider-a/model");
  const summary = catalogSummaries.find(input => input.dataset.route === "provider-a/model");
  assert(alias && contextInput && summary, "display controls must be rendered for every catalog route");
  alias.value = "General";
  contextInput.checked = false;
  summary.value = "hide";
  context.__persistStateStub = async (_message, candidate) => { context.__savedCandidate = candidate; context.state = candidate; };
  run("__realPersistState = persistState; persistState = __persistStateStub");
  await run("saveCatalogDisplay()");
  run("persistState = __realPersistState");
  const saved = run("__savedCandidate.catalog_presentations['provider-a/model']");
  assert.strictEqual(saved.catalog_alias, "General");
  assert.strictEqual(saved.show_context, false);
  assert.strictEqual(saved.reasoning_summary, "hide");
  run("openManualModelModal('provider-a/model')");
  assert.doesNotMatch(getElement("modal_body").innerHTML, /Codex 显示名称|Reasoning summary/);
  assert.match(getElement("modal_body").innerHTML, /模型列表显示/);
}

function presentationMigrationBehavior() {
  run("state = {catalog_presentations:{'old/model-a':{catalog_alias:'General',show_context:false,reasoning_summary:'hide'},'old/model-b':{catalog_alias:'Builder',show_context:true,reasoning_summary:'auto'},'native-model':{catalog_alias:'Native',show_context:true,reasoning_summary:'show'}}}");
  run("movePresentationPrefix('old', 'new')");
  assert.strictEqual(run("state.catalog_presentations['old/model-a']"), undefined);
  assert.strictEqual(run("state.catalog_presentations['new/model-a'].catalog_alias"), "General");
  assert.strictEqual(run("state.catalog_presentations['new/model-a'].show_context"), false);
  assert.strictEqual(run("state.catalog_presentations['new/model-b'].catalog_alias"), "Builder");
  assert.strictEqual(run("state.catalog_presentations['native-model'].catalog_alias"), "Native");

  run("movePresentation('new/model-a', 'provider/model-a')");
  assert.strictEqual(run("state.catalog_presentations['new/model-a']"), undefined);
  assert.strictEqual(run("state.catalog_presentations['provider/model-a'].reasoning_summary"), "hide");

  run("state.emp_version = '0.8.0'");
  assert.strictEqual(run("migrationFilename()"), "easy-multi-provider-0.8.0.emp");
  run("state.emp_version = '../../unsafe'");
  assert.strictEqual(run("migrationFilename()"), "easy-multi-provider-0.8.0.emp");
}

function modalDismissalBehavior() {
  run("openModal('Editor', '<p>unsaved marker</p>', 'Save', () => {})");
  assert(!getElement("modal_backdrop").classList.contains("hidden"));
  assert.strictEqual(run("typeof backdropClose"), "undefined", "backdrop clicks must not dismiss the editor");
  run("handleModalKeydown({key:'Enter'})");
  assert(!getElement("modal_backdrop").classList.contains("hidden"), "unrelated keys must not dismiss the editor");
  run("handleModalKeydown({key:'Escape'})");
  assert(getElement("modal_backdrop").classList.contains("hidden"), "Escape must dismiss the editor");
}

async function atomicStateBehavior() {
  run("state = {native_catalog_path:'',catalog_presentations:{},accounts:[{id:'account-a',prefix:'old'}],providers:[],models:[]}; __candidate = cloneState(); __candidate.accounts[0].prefix = 'new'");
  context.__apiStub = async path => { if (path === "/api/config") throw new Error("save rejected"); throw new Error("unexpected API " + path); };
  run("api = __apiStub");
  await assert.rejects(run("persistState('save', __candidate)"), /save rejected/);
  assert.strictEqual(run("state.accounts[0].prefix"), "old", "failed save must not mutate live UI state");

  run("state = {native_catalog_path:'',catalog_presentations:{},accounts:[],providers:[{id:'provider-a',name:'Provider A'}],models:[{id:'provider-a/model',provider:'provider-a',enabled:true}]}");
  context.__apiStub = async path => { if (path === "/api/config") throw new Error("save rejected"); throw new Error("unexpected API " + path); };
  run("api = __apiStub");
  await run("toggleProviderModels('provider-a', false)");
  assert.strictEqual(run("state.models[0].enabled"), true, "failed provider visibility save must not mutate live UI state");

  run("state = {native_catalog_path:'',catalog_presentations:{},accounts:[{id:'account-a',prefix:'old'}],providers:[],models:[]}; __candidate = cloneState(); __candidate.accounts[0].prefix = 'saved'");
  context.__apiStub = async path => {
    if (path === "/api/config") return context.__candidate;
    if (path === "/api/catalog/refresh") throw new Error("refresh rejected");
    if (path === "/api/integration") return {};
    throw new Error("unexpected API " + path);
  };
  run("api = __apiStub");
  await assert.rejects(run("persistState('save', __candidate)"), /refresh rejected/);
  assert.strictEqual(run("state.accounts[0].prefix"), "saved", "a persisted save remains authoritative when catalog refresh fails");

  run("state = {catalog_presentations:{},accounts:[],providers:[{id:'provider-a',name:'Provider A'}],models:[{id:'provider-a/model',provider:'provider-a',upstream_id:'model',supports_reasoning_summaries:false}]}; openManualModelModal('provider-a/model')");
  getElement("modal_model_provider").value = "provider-a";
  getElement("modal_model_upstream").value = "model";
  context.__apiStub = async path => {
    if (path === "/api/models/metadata") return {context_window:1000,input_token_limit:1000,output_token_limit:100,reasoning_levels:["high"],supports_reasoning:true,supports_reasoning_summaries:true};
    throw new Error("unexpected API " + path);
  };
  run("api = __apiStub");
  await run("inspectModalModel()");
  assert.strictEqual(run("modalReasoningSummarySupport"), true, "metadata inspection must retain summary capability");

  run("state = {native_catalog_path:'',catalog_presentations:{'provider-a/a':{catalog_alias:'A'},'provider-a/b':{catalog_alias:'B'}},accounts:[],providers:[{id:'provider-a',name:'Provider A'}],models:[{id:'provider-a/a',provider:'provider-a',upstream_id:'a',enabled:true},{id:'provider-a/b',provider:'provider-a',upstream_id:'b',enabled:true}]}; openManualModelModal('provider-a/a')");
  getElement("modal_model_provider").value = "provider-a";
  getElement("modal_model_upstream").value = "b";
  await assert.rejects(run("saveManualModel()"), /模型路由已存在/);
  assert.deepStrictEqual(
    Array.from(run("state.models.map(model => model.id)")),
    ["provider-a/a", "provider-a/b"],
    "a colliding model rename must preserve both routes",
  );
  assert.strictEqual(run("state.catalog_presentations['provider-a/b'].catalog_alias"), "B");
}

(async () => {
  await integrationBehavior();
  pickerBehavior();
  duplicateAccountBehavior();
  modelGroupBehavior();
  officialPresetBehavior();
  capabilityMetadataBehavior();
  await presentationBehavior();
  presentationMigrationBehavior();
  modalDismissalBehavior();
  await atomicStateBehavior();
  process.stdout.write("web DOM behavior: ok\n");
})().catch(error => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
