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
let subscriptionModelInputs = [];

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
    if (this.id === "modal_body") { parseDiscoveredOptions(this._innerHTML); parseSubscriptionOptions(this._innerHTML); }
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

function parseSubscriptionOptions(html) {
  subscriptionModelInputs = [];
  for (const match of html.matchAll(/<input type="checkbox" name="subscription_model" value="([^"]*)"([^>]*)>/g)) {
    const input = new Element(); input.value = unescapeHtml(match[1]); input.checked = /\schecked(?:\s|$)/.test(match[2]); subscriptionModelInputs.push(input);
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
    if (selector === 'input[name="subscription_model"]') return subscriptionModelInputs;
    if (selector === 'input[name="subscription_model"]:checked') return subscriptionModelInputs.filter(input => input.checked);
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
  "integration_reload", "codex_compatibility", "language_select", "theme_select",
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
        runtime: {state: "stopped_waiting_for_start", target: "emp", verified: false, action_required: false, detail: "shared backend unavailable"},
        service_health: "ready",
        next_action: "wait for shared backend owner start",
      };
      return integration;
    }
    if (path === "/api/integration") return integration;
    throw new Error("unexpected API " + path);
  };
  run("api = __apiStub; state = {native_catalog_path:'', accounts:[], providers:[], models:[]}");
  run("confirmIntegrationAction('enable')");
  assert(!getElement("modal_backdrop").classList.contains("hidden"), "confirmation modal must open");
  assert.match(getElement("modal_body").innerHTML, /不会停止|will not stop/i);
  assert.doesNotMatch(getElement("modal_body").innerHTML, /disconnect|中断|断开/i);
  await getElement("modal_submit").click();
  const enable = calls.find(call => call.path === "/api/integration/enable");
  assert(enable, "enable endpoint was not called");
  assert.deepStrictEqual(JSON.parse(enable.options.body), {confirm_reload: true});
  assert(!calls.some(call => call.path === "/api/integration/sync"), "obsolete second sync was called");
  assert.match(getElement("status").textContent, /owner|所有者/i);
  assert(getElement("modal_backdrop").classList.contains("hidden"), "successful modal must close");

  run("renderIntegration({codex_compatibility:{installed:'0.151.2',status:'recommended',source:'managed',path_cli:{installed:'0.146.0',status:'unsupported'},supported_range:'0.149.x–0.151.x',recommended:'0.151.x'},configuration:{state:'emp_applied',relation:'applied',conflicts:[]},runtime:{state:'emp_loaded',target:'emp',verified:true,action_required:false,detail:''},service_health:'ready',next_action:'none'})");
  assert.match(getElement("integration_summary").textContent, /exposes|已暴露/i);
  assert.match(getElement("codex_compatibility").textContent, /托管 Codex runtime 0\.151\.2.*独立命令行 0\.146\.0.*0\.149\.x.*0\.151\.x/);
  assert.strictEqual(getElement("codex_compatibility").dataset.state, "recommended");
  run("renderIntegration({configuration:{state:'emp_applied',relation:'applied',conflicts:[]},runtime:{state:'stopped_waiting_for_start',target:'emp',verified:false,action_required:false,detail:''},service_health:'ready',next_action:'none'})");
  assert.match(getElement("integration_summary").textContent, /owner|所有者/i);

  let passiveVerifyCalls = 0;
  context.__apiStub = async (path) => {
    if (path === "/api/integration") return {
      configuration: {state: "emp_applied", relation: "applied", conflicts: []},
      runtime: {state: "stop_failed", target: "emp", verified: false, action_required: true, detail: "old stop failure"},
      service_health: "ready",
      next_action: "reconnect Codex",
    };
    if (path === "/api/integration/verify") {
      passiveVerifyCalls += 1;
      return {
        configuration: {state: "emp_applied", relation: "applied", conflicts: []},
        runtime: {state: "emp_loaded", target: "emp", verified: true, action_required: false, detail: "complete catalog"},
        service_health: "ready",
        next_action: "none",
      };
    }
    throw new Error("unexpected API " + path);
  };
  run("api = __apiStub");
  await run("loadIntegration()");
  assert.strictEqual(passiveVerifyCalls, 1);
  assert.match(getElement("integration_summary").textContent, /exposes|已暴露/i);
  assert.strictEqual(getElement("integration_reload").hidden, true);

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
  assert.match(getElement("integration_summary").textContent, /只读目录检查失败|read-only shared backend catalog check failed/i);
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
  run("state = {native_account:{id:'@native',name:'当前 Codex 登录',prefix:'',native:true,credential_set:true,hidden_models:[]},accounts:[{id:'same-login-account',prefix:'same-login-account',duplicate:true,duplicate_of:'当前 Codex 登录',credential_set:true},{id:'usable-account',prefix:'usable-account',duplicate:false,credential_set:true}]}; renderAccounts()");
  const html = getElement("accounts").innerHTML;
  assert.match(html, /当前 Codex 登录/);
  assert.match(html, /Native/);
  assert.match(html, /same-login-account/);
  assert.match(html, /usable-account/);
  assert.match(html, /模型显示由原生账户管理/);
  const nativeRow = html.split("当前 Codex 登录")[1].split("</tr>")[0];
  assert.doesNotMatch(nativeRow, /removeAccount\('@native'\)/);
  const duplicateRow = html.split("</tr>").find(row => row.includes("same-login-account"));
  assert(duplicateRow, "duplicate account row must render");
  assert.match(duplicateRow, /account-duplicate/);
  assert.doesNotMatch(duplicateRow, /editAccount\('same-login-account'\)/);
  assert.match(duplicateRow, /refreshAccount\('same-login-account'\)/);
  assert.match(duplicateRow, /openQuotaHistory\('same-login-account'\)/);
}

function quotaHistoryBehavior() {
  run("renderQuotaHistory({series:[]}, '1d')");
  assert.match(getElement("quota_history_content").innerHTML, /暂无额度历史/);
  context.__quotaPayload = {series:[{limit_id:'codex',window_kind:'primary',window_minutes:10080,points:[{observed_at:1000,remaining_percent:80},{observed_at:1300,remaining_percent:75}]}]};
  run("renderQuotaHistory(__quotaPayload, '1h')");
  const html = getElement("quota_history_content").innerHTML;
  assert.match(html, /<svg/);
  assert.match(html, /主窗口/);
  assert.match(html, /75%/);
  assert.match(html, /每 5 分钟自动采样/);

  for (const [seriesValues, expectedMin, expectedMax] of [
    [[[72,74],[80]], 71, 81],
    [[[74.1,74.2]], 73, 76],
    [[[50,50]], 49, 51],
    [[[0]], 0, 1],
    [[[100]], 99, 100],
    [[[0,100]], 0, 100],
  ]) {
    context.__axisSeries = seriesValues.map(values => ({points:values.map((value, index) => ({observed_at:1000 + index * 300, remaining_percent:value}))}));
    const svg = run("quotaChartSvg(__axisSeries)");
    const ticks = [...svg.matchAll(/>([\d.]+)%<\/text>/g)].map(match => Number(match[1]));
    assert.strictEqual(ticks.length, 5);
    assert.strictEqual(ticks[0], expectedMin);
    assert.strictEqual(ticks[4], expectedMax);
    assert.doesNotMatch(svg, /NaN|Infinity/);
  }
  assert.strictEqual(run("quotaChartSvg([])"), "");
}

async function nativeAccountBehavior() {
  run("state = {native_account:{id:'@native',name:'Current Codex login',prefix:'',native:true,credential_set:true,hidden_models:['model-b']},native_hidden_models:['model-b'],catalog_presentations:{},catalog_family_presentations:{},catalog_families:[],subscription_models:[{id:'model-a',display_name:'Model A'},{id:'model-b',display_name:'Model B'}],accounts:[{id:'legacy',prefix:'legacy',duplicate:true,duplicate_of:'当前 Codex 登录',hidden_models:['model-b']}],providers:[],models:[]}; editAccount('@native')");
  assert.doesNotMatch(getElement("modal_body").innerHTML, /modal_account_alias/);
  assert.deepStrictEqual(subscriptionModelInputs.map(input => input.checked), [true, false]);
  subscriptionModelInputs[0].checked = false;
  subscriptionModelInputs[1].checked = true;
  context.__persistStateStub = async (_message, candidate) => { context.__savedNativeCandidate = candidate; context.state = candidate; };
  run("__realPersistState = persistState; persistState = __persistStateStub");
  await getElement("modal_submit").click();
  run("persistState = __realPersistState");
  assert.deepStrictEqual(Array.from(run("__savedNativeCandidate.native_hidden_models")), ["model-a"]);
  assert.deepStrictEqual(Array.from(run("__savedNativeCandidate.accounts[0].hidden_models")), []);
}

async function accountEmojiBehavior() {
  run("state = {accounts:[{id:'ship',name:'ship',prefix:'ship',hidden_models:[]}],subscription_models:[{id:'model-a',display_name:'Model A'}],catalog_presentations:{'ship/model-a':{catalog_alias:'Keep me'}},providers:[],models:[]}; editAccount('ship')");
  getElement("modal_account_alias").value = "🚢";
  context.__persistStateStub = async (_message, candidate) => { context.__savedEmojiCandidate = candidate; };
  run("__realPersistState = persistState; persistState = __persistStateStub");
  await getElement("modal_submit").click();
  run("persistState = __realPersistState");
  assert.strictEqual(run("__savedEmojiCandidate.accounts[0].name"), "🚢");
  assert.strictEqual(run("__savedEmojiCandidate.accounts[0].prefix"), "ship");
  assert.strictEqual(run("__savedEmojiCandidate.catalog_presentations['ship/model-a'].catalog_alias"), "Keep me");
  run("state = __savedEmojiCandidate; renderAccounts()");
  assert.match(getElement("accounts").innerHTML, /class="pill">🚢<\/span>/);
}

async function quotaErrorBehavior() {
  context.__quotaError = Object.assign(new Error("safe fallback"), {payload:{error:{code:'quota_auth_required'}}});
  context.__apiQuotaError = async () => { throw context.__quotaError; };
  run("__realQuotaApi = api; api = __apiQuotaError");
  assert.strictEqual(await run("refreshAccount('ship', false)"), false);
  assert.match(getElement("accounts").innerHTML, /相同账户 ID 导入最新 auth.json/);
  assert.doesNotMatch(getElement("accounts").innerHTML, /safe fallback/);
  run("api = __realQuotaApi");
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
  run("state = {catalog_presentations:{'provider-a/model':{catalog_alias:'Legacy',show_context:true,reasoning_summary:'auto'}},catalog_family_presentations:{},catalog_families:[{id:'native-model',default_display_name:'Native Model',display_name:'Native Model',context_window:258000,supports_reasoning_summaries:true,routes:[{id:'native-model',source_type:'native',source_id:''}]},{id:'model',default_display_name:'Model',display_name:'Model',context_window:258000,supports_reasoning_summaries:true,routes:[{id:'provider-a/model',source_type:'provider',source_id:'provider-a'}]}],subscription_models:[{id:'native-model',display_name:'Native Model',context_window:258000}],providers:[{id:'provider-a',name:'Provider A'}],accounts:[],models:[{id:'provider-a/model',provider:'provider-a',upstream_id:'model',display_name:'Model',context_window:258000,enabled:true}]} ");
  run("renderCatalogDisplay()");
  assert.match(getElement("catalog_display_models").innerHTML, /data-catalog-alias/);
  assert.match(getElement("catalog_display_models").innerHTML, /provider-a\/model/);
  const alias = catalogAliases.find(input => input.dataset.route === "model");
  const contextInput = catalogContexts.find(input => input.dataset.route === "model");
  const summary = catalogSummaries.find(input => input.dataset.route === "model");
  assert(alias && contextInput && summary, "display controls must be rendered once per model family");
  alias.value = "General";
  contextInput.checked = false;
  summary.value = "hide";
  context.__persistStateStub = async (_message, candidate) => { context.__savedCandidate = candidate; context.state = candidate; };
  run("__realPersistState = persistState; persistState = __persistStateStub");
  await run("saveCatalogDisplay()");
  run("persistState = __realPersistState");
  const saved = run("__savedCandidate.catalog_family_presentations['model']");
  assert.strictEqual(saved.catalog_alias, "General");
  assert.strictEqual(saved.show_context, false);
  assert.strictEqual(saved.reasoning_summary, "hide");
  assert.strictEqual(run("__savedCandidate.catalog_presentations['provider-a/model']"), undefined);
  run("openManualModelModal('provider-a/model')");
  assert.doesNotMatch(getElement("modal_body").innerHTML, /Codex 显示名称|Reasoning summary/);
  assert.match(getElement("modal_body").innerHTML, /模型列表显示/);
}

function presentationMigrationBehavior() {
  run("state = {catalog_presentations:{'old/model-a':{catalog_alias:'General',show_context:false,reasoning_summary:'hide'},'old/model-b':{catalog_alias:'Builder',show_context:true,reasoning_summary:'auto'},'native-model':{catalog_alias:'Native',show_context:true,reasoning_summary:'show'}}}");
  run("movePresentation('old/model-a', 'provider/model-a')");
  assert.strictEqual(run("state.catalog_presentations['old/model-a']"), undefined);
  assert.strictEqual(run("state.catalog_presentations['provider/model-a'].catalog_alias"), "General");
  assert.strictEqual(run("state.catalog_presentations['provider/model-a'].show_context"), false);
  assert.strictEqual(run("state.catalog_presentations['old/model-b'].catalog_alias"), "Builder");
  assert.strictEqual(run("state.catalog_presentations['native-model'].catalog_alias"), "Native");
  assert.strictEqual(run("state.catalog_presentations['provider/model-a'].reasoning_summary"), "hide");

  run("state.emp_version = '0.9.4'");
  assert.strictEqual(run("migrationFilename()"), "easy-multi-provider-0.9.4.emp");
  run("state.emp_version = '../../unsafe'");
  assert.strictEqual(run("migrationFilename()"), "easy-multi-provider-0.9.4.emp");
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
  quotaHistoryBehavior();
  await nativeAccountBehavior();
  await accountEmojiBehavior();
  await quotaErrorBehavior();
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
