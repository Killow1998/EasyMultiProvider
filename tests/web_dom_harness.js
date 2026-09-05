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
let catalogPreviews = [];
let subscriptionModelInputs = [];
let runtimeInputs = [];

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
    if (this.id === "codex_runtimes") parseRuntimeInputs(this._innerHTML);
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
  catalogPreviews = [];
  for (const match of html.matchAll(/<strong data-catalog-preview data-route="([^"]*)">([^<]*)<\/strong>/g)) {
    const preview = new Element(); preview.dataset.route = unescapeHtml(match[1]); preview.textContent = unescapeHtml(match[2]); catalogPreviews.push(preview);
  }
  for (const match of html.matchAll(/<input data-catalog-alias data-route="([^"]*)" value="([^"]*)"/g)) {
    const input = new Element(); input.dataset.route = unescapeHtml(match[1]); input.value = unescapeHtml(match[2]); catalogAliases.push(input);
  }
  for (const match of html.matchAll(/<input type="checkbox" data-catalog-context data-route="([^"]*)"([^>]*)>/g)) {
    const input = new Element(); input.dataset.route = unescapeHtml(match[1]); input.checked = /\schecked(?:\s|$)/.test(match[2]); catalogContexts.push(input);
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

function parseRuntimeInputs(html) {
  runtimeInputs = [];
  for (const match of html.matchAll(/<input type="checkbox" data-runtime-source value="([^"]*)"([^>]*)>/g)) {
    const input = new Element();
    input.value = unescapeHtml(match[1]);
    input.checked = /\schecked(?:\s|$)/.test(match[2]);
    input.disabled = /\sdisabled(?:\s|$)/.test(match[2]);
    runtimeInputs.push(input);
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
    if (selector === "[data-catalog-preview]") return catalogPreviews;
    if (selector === "[data-runtime-source]") return runtimeInputs;
    if (selector === "[data-runtime-source]:checked") return runtimeInputs.filter(input => input.checked);
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
  "integration_reload", "codex_compatibility", "codex_runtime_save",
  "codex_runtime_scan", "codex_runtimes", "language_select", "theme_select",
  "catalog_display_search", "catalog_display_toggle", "catalog_display_models",
  "diagnostics_summary", "performance_records", "diagnostics_records", "accounts", "providers", "models",
]) getElement(id);

const html = fs.readFileSync(process.argv[2], "utf8");
assert.doesNotMatch(html, /实际调用时自动使用其中兼容性最可靠的一个/);
assert.match(html, /class="workspace-layout"/);
assert.match(html, /<aside class="workspace-side">/);
assert.match(html, /onclick="openUpdate\(\)"/);
assert.match(html, /href="https:\/\/github.com\/Killow1998\/EasyMultiProvider" target="_blank" rel="noopener noreferrer"/);
assert.doesNotMatch(html, /id="subscription_search_account"/);
assert.doesNotMatch(html, /data-catalog-summary/);
assert.match(html, /@phosphor-icons\/core 2\.1\.1, Regular weight, MIT/);
assert.strictEqual(
  Array.from(html.matchAll(/button\[data-icon="[^"]+"\]\{--button-icon:url\("data:image\/svg\+xml,%3Csvg%20/g)).length,
  13,
  "all action icons must come from the embedded Phosphor set",
);
for (const unwantedDefaultTip of [
  /长请求自动扩容/,
  /选中的客户端用于兼容性提示/,
  /不会返回到浏览器或写入/,
  /只保留额度查询/,
  /余量每分钟同步/,
  /每 5 分钟自动采样/,
  /隐藏模型排在最后/,
  /仅凭模型 ID/,
]) {
  assert.doesNotMatch(html, unwantedDefaultTip, `developer-facing tip leaked into the default UI: ${unwantedDefaultTip}`);
}
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
  setInterval: () => 1,
  clearInterval: () => {},
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
    if (path === "/api/runtime/select") return {};
    if (path === "/api/runtime/scan") return {};
    throw new Error("unexpected API " + path);
  };
  run("api = __apiStub; state = {native_catalog_path:'', accounts:[], providers:[], models:[]}");
  run("confirmIntegrationAction('enable')");
  assert(!getElement("modal_backdrop").classList.contains("hidden"), "confirmation modal must open");
  assert.match(getElement("modal_body").innerHTML, /应用当前 EMP 设置.*重启 Codex/);
  assert.doesNotMatch(getElement("modal_body").innerHTML, /只读|共享后端|所有者|无法确认/);
  await getElement("modal_submit").click();
  const enable = calls.find(call => call.path === "/api/integration/enable");
  assert(enable, "enable endpoint was not called");
  assert.deepStrictEqual(JSON.parse(enable.options.body), {confirm_reload: true});
  assert(!calls.some(call => call.path === "/api/integration/sync"), "obsolete second sync was called");
  assert.match(getElement("status").textContent, /EMP已启动，请重启Codex/);
  assert(getElement("modal_backdrop").classList.contains("hidden"), "successful modal must close");

  run("renderIntegration({codex_compatibility:{installed:'0.152.1',status:'recommended',source:'managed',helper_source:'managed',preferences:['auto'],runtimes:[{source:'managed',path:'/managed/codex',installed:'0.152.1',status:'recommended',selectable:true,targeted:true,helper:true},{source:'cursor',path:'/cursor/codex',installed:'0.150.1',status:'supported',selectable:true,targeted:true,helper:false},{source:'path_cli',path:'/old/codex',installed:'0.146.0',status:'unsupported',selectable:false,targeted:false,helper:false}],supported_range:'0.149.x–0.152.x',recommended:'0.152.x'},configuration:{state:'emp_applied',relation:'applied',conflicts:[]},runtime:{state:'emp_loaded',target:'emp',verified:true,action_required:false,detail:''},service_health:'ready',next_action:'none'})");
  assert.strictEqual(getElement("integration_summary").textContent, "EMP已启动，请重启Codex");
  assert.strictEqual(getElement("codex_compatibility").textContent, "");
  assert.strictEqual(getElement("codex_compatibility").hidden, true);
  assert.strictEqual(getElement("codex_compatibility").dataset.state, "recommended");
  assert.match(getElement("codex_runtimes").innerHTML, /title="\/managed\/codex"/);
  assert.match(getElement("codex_runtimes").innerHTML, /Codex CLI<\/strong> v0\.146\.0/);
  assert.doesNotMatch(getElement("codex_runtimes").innerHTML, /<code>/);
  assert.match(getElement("codex_runtimes").innerHTML, /data-runtime-source/, "compatible runtimes must be independently selectable");
  assert.deepStrictEqual(runtimeInputs.map(input => input.checked), [true, true, false]);
  assert.deepStrictEqual(runtimeInputs.map(input => input.disabled), [false, false, true]);
  await run("saveCodexRuntimeSelection()");
  const manualRuntimeSelection = calls.find(call => call.path === "/api/runtime/select");
  assert.deepStrictEqual(JSON.parse(manualRuntimeSelection.options.body), {sources:['managed','cursor']});
  await run("useAutomaticCodexRuntimes()");
  await run("scanCodexRuntimes()");
  assert(calls.some(call => call.path === "/api/runtime/select" && call.options.body.includes('auto')));
  assert(calls.some(call => call.path === "/api/runtime/scan"));

  run("renderIntegration({codex_compatibility:{installed:'0.152.0-alpha.7.2',status:'unverified',source:'codex_app',helper_source:'codex_app',preferences:['codex_app'],runtimes:[{source:'codex_app',path:'C:/OpenAI/Codex/codex.exe',installed:'0.152.0-alpha.7.2',status:'unverified',selectable:true,targeted:true,helper:true}],supported_range:'0.149.x–0.152.x',recommended:'0.152.x'},configuration:{state:'native',relation:'original',conflicts:[]},runtime:{state:'not_checked',target:'native',verified:false,action_required:false,detail:''},service_health:'ready',next_action:'none'})");
  assert.match(getElement("codex_compatibility").textContent, /0\.152\.0-alpha\.7\.2.*尚未验证.*0\.152\.x/);
  assert.strictEqual(getElement("codex_compatibility").hidden, false);
  run("renderIntegration({configuration:{state:'emp_applied',relation:'applied',conflicts:[]},runtime:{state:'stopped_waiting_for_start',target:'emp',verified:false,action_required:false,detail:''},service_health:'ready',next_action:'none'})");
  assert.strictEqual(getElement("integration_summary").textContent, "EMP已启动，请重启Codex");

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
  assert.strictEqual(getElement("integration_summary").textContent, "EMP已启动，请重启Codex");
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
  assert.strictEqual(getElement("integration_summary").textContent, "暂时无法读取 EMP 状态，请重试。");
  assert.doesNotMatch(getElement("integration_summary").textContent, /integration request failed|stale|过期/i);
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
  assert.strictEqual(getElement("integration_badge").textContent, "EMP");
  assert.strictEqual(getElement("integration_summary").textContent, "EMP已启动，请重启Codex");
  assert.doesNotMatch(getElement("integration_summary").textContent, /只读|未验证|无法确认|共享后端/);
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
  assert.match(html, /7d/);
  assert.match(html, /75%/);
  assert.match(html, /data-quota-point/);
  assert.match(html, /quota-hover-target/);
  assert.match(html, /quota-chart-tooltip/);
  assert.doesNotMatch(html, /每 5 分钟|自动采样|保留 15 天/);

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
  context.__resetSeries = [{limit_id:'codex',window_minutes:300,points:[
    {observed_at:1000,remaining_percent:5,resets_at:1200},
    {observed_at:1300,remaining_percent:100,resets_at:19200},
    {observed_at:1600,remaining_percent:95,resets_at:19200},
    {observed_at:5000,remaining_percent:90,resets_at:19200},
  ]}];
  const resetSvg = run('quotaChartSvg(__resetSeries)');
  const resetPath = resetSvg.match(/<path d="([^"]+)"/)[1];
  assert.strictEqual((resetPath.match(/M/g) || []).length, 3);
  assert.strictEqual((resetPath.match(/L/g) || []).length, 1);
  assert.match(resetSvg, /data-label="5h"/);
  assert.match(resetSvg, /data-break="新的额度周期"/);
  assert.match(resetSvg, /data-reset="19200"/);
  assert.strictEqual(run('quotaChartSvg([{points:[{observed_at:10,remaining_percent:null}]}])'), '');

  const first = {dataset:{x:'90',y:'80',time:'1000',value:'80',label:'主窗口'},radius:'',setAttribute(name,value) { if (name === 'r') this.radius = value; }};
  const second = {dataset:{x:'90.1',y:'100',time:'1000',value:'60',label:'次窗口'},radius:'',setAttribute(name,value) { if (name === 'r') this.radius = value; }};
  const distant = {dataset:{x:'300',y:'120',time:'1300',value:'50',label:'主窗口'},radius:'',setAttribute(name,value) { if (name === 'r') this.radius = value; }};
  const guide = {hidden:true,values:{},setAttribute(name,value) { this.values[name] = value; }};
  const tooltip = {hidden:true,style:{},innerHTML:''};
  context.__quotaHoverSvg = {
    getBoundingClientRect: () => ({left:0,width:628}),
    querySelectorAll: selector => selector === '[data-quota-point]' ? [first,second,distant] : [],
    querySelector: selector => selector === '.quota-chart-guide' ? guide : null,
    parentElement: {querySelector: selector => selector === '.quota-chart-tooltip' ? tooltip : null},
  };
  context.__quotaHoverTarget = {ownerSVGElement:context.__quotaHoverSvg};
  run("quotaChartHover({currentTarget:__quotaHoverTarget,clientX:95})");
  assert.strictEqual(guide.hidden, false);
  assert.strictEqual(first.radius, '4');
  assert.strictEqual(second.radius, '4');
  assert.strictEqual(distant.radius, '3');
  assert.strictEqual(tooltip.hidden, false);
  assert.match(tooltip.innerHTML, /主窗口 · 80%/);
  assert.match(tooltip.innerHTML, /次窗口 · 60%/);
  run("quotaChartLeave({currentTarget:__quotaHoverTarget})");
  assert.strictEqual(guide.hidden, true);
  assert.strictEqual(tooltip.hidden, true);
}

async function quotaHistoryRaceBehavior() {
  const waiting = [];
  context.__historyApi = () => new Promise(resolve => waiting.push(resolve));
  run('__savedHistoryApi = api; __savedQuotaSync = refreshQuotaState; api = __historyApi; refreshQuotaState = async () => {}');
  try {
    const oldRequest = run("loadQuotaHistory('first','1h')");
    const newRequest = run("loadQuotaHistory('second','1d')");
    waiting[1]({series:[{limit_id:'codex',window_minutes:10080,points:[{observed_at:1000,remaining_percent:73}]}]});
    await newRequest;
    const latest = getElement('quota_history_content').innerHTML;
    waiting[0]({series:[]});
    await oldRequest;
    assert.strictEqual(getElement('quota_history_content').innerHTML, latest);
    assert.match(latest, /73%/);
    const closingRequest = run("loadQuotaHistory('second','1h')");
    run('clearQuotaHistoryTimer()');
    getElement('quota_history_content').innerHTML = 'closed';
    waiting[2]({series:[]});
    await closingRequest;
    assert.strictEqual(getElement('quota_history_content').innerHTML, 'closed');
  } finally { run('api = __savedHistoryApi; refreshQuotaState = __savedQuotaSync'); }
}

function performanceDiagnosticsBehavior() {
  context.__performancePayload = {performance_window:{calls:20,days:7},health:{sample_count:12,success_count:10,success_rate:83.3,status_429_count:1,status_429_rate:8.3,status_502_count:1,status_502_rate:8.3,local_capacity_count:0,local_capacity_rate:0,failure_classes:[{error_class:'upstream_close_pre_output',count:1,rate:8.3},{error_class:'rate_limit',count:1,rate:8.3}]},models:[
    {model_id:'gpt-5.6-sol',speed_mode:'standard',call_count:20,ttft_ms:5000,ttft_samples:20,ttft_change_percent:10,tokens_per_second:55,tps_samples:18,tps_change_percent:22.5},
    {model_id:'gpt-5.6-sol',speed_mode:'fast',call_count:3,ttft_ms:3200,ttft_samples:3,tokens_per_second:82,tps_samples:2},
    {model_id:'gemini-3.7-flash',speed_mode:'unknown',call_count:2,ttft_ms:1200,ttft_samples:2,tokens_per_second:90,tps_samples:2},
  ],records:[
    {observed_at:'2026-09-02T11:59:59Z',route:'responses',model_id:'codex-auto-review',status:200,error_class:'none',ttft_ms:null,tokens_per_second:null,local_prepare_ms:15,duration_ms:20,protocol:'responses',transport:'websocket',context_decision:'allowed'},
    {observed_at:'2026-09-02T12:00:00Z',route:'responses',model_id:'sol/native',status:200,error_class:'none',ttft_ms:5000,tokens_per_second:55,local_prepare_ms:120,upstream_first_token_ms:4880,duration_ms:7000,protocol:'responses',transport:'websocket',context_decision:'allowed'},
    {observed_at:'2026-09-02T12:01:00Z',route:'responses',model_id:'sol/slow',status:200,error_class:'none',ttft_ms:9000,tokens_per_second:30,local_prepare_ms:100,upstream_first_token_ms:8900,duration_ms:13000,protocol:'responses',transport:'websocket',context_decision:'allowed'},
  ]};
  run('renderDiagnostics(__performancePayload)');
  assert.match(getElement('diagnostics_summary').textContent, /最近 12 次请求/);
  assert.match(getElement('health_summary').innerHTML, /83\.3%/);
  assert.match(getElement('health_summary').innerHTML, />502</);
  assert.doesNotMatch(getElement('health_summary').innerHTML, /失败原因|输出前断线|上游限流/);
  const rendered = getElement('performance_records').innerHTML;
  assert.match(rendered, /gpt-5\.6-sol/);
  assert.match(rendered, /最近 20 次有效调用/);
  assert.match(rendered, /↓10\.0%/);
  assert.match(rendered, /↑22\.5%/);
  assert.match(rendered, /5\.00 s/);
  assert.match(rendered, /55\.0 token\/s/);
  assert.match(rendered, />Fast</);
  assert.match(rendered, /82\.0 token\/s/);
  assert.match(rendered, /gemini-3\.7-flash/);
  assert.doesNotMatch(rendered, /未标记/);
  assert.doesNotMatch(rendered, /codex-auto-review/);
  assert.doesNotMatch(rendered, /判断|参考|原生 A\/B/);
  run('openDiagnostics()');
  assert.match(getElement('modal_title').textContent, /性能与健康/);
  assert.match(getElement('modal_body').innerHTML, /到收到首段正文或工具参数的时间/);
  assert.match(getElement('modal_body').innerHTML, /输出期间每秒接收的 token 数估计/);
  assert.doesNotMatch(getElement('modal_body').innerHTML, /SOL 原生参考|原生 A\/B/);
  assert.doesNotMatch(getElement('modal_body').innerHTML, /最近请求|失败原因/);
  run('closeModal()');
  context.__performancePayload.models = [
    {model_id:'gemini-3.7-flash',speed_mode:'unknown',call_count:2,ttft_ms:1200,ttft_samples:2,tokens_per_second:90,tps_samples:2},
  ];
  run('renderDiagnostics(__performancePayload)');
  assert.doesNotMatch(getElement('performance_records').innerHTML, />模式<|>Mode<|未标记|Unmarked/);
}

function providerDiscoveryErrorBehavior() {
  context.__badKey = Object.assign(new Error('upstream 401'), {status:401});
  context.__badRequestKey = Object.assign(new Error('upstream 400'), {status:400});
  context.__busyProvider = Object.assign(new Error('upstream 429'), {status:429});
  assert.match(run('providerDiscoveryError(__badKey)'), /API Key 无效/);
  assert.match(run('providerDiscoveryError(__badRequestKey)'), /API Key 无效/);
  assert.match(run('providerDiscoveryError(__busyProvider)'), /请求过于频繁/);
}

function quotaMeterBehavior() {
  context.__quotaMeterState = {
    native_account: null,
    accounts: [{id:'meter',name:'meter',prefix:'meter',credential_set:true,quota:{rate_limits:{primary:{usedPercent:20,windowDurationMins:10080,resetsAt:1900000000},secondary:{usedPercent:65.5,windowDurationMins:300,resetsAt:1900000300}}}}],
  };
  run("state = __quotaMeterState; renderAccounts()");
  let rendered = getElement("accounts").innerHTML;
  assert.match(rendered, /class="quota-battery" role="progressbar"/);
  assert.match(rendered, /aria-valuenow="80"/);
  assert.match(rendered, /aria-valuenow="34\.5"/);
  assert(rendered.indexOf("7d") < rendered.indexOf("5h"), "long quota window must render first");
  assert.match(rendered, /is-medium/);
  assert.doesNotMatch(rendered, /is-updated/, "ordinary rerenders must not replay quota animation");

  run("quotaAnimationAccounts.add('meter'); renderAccounts()");
  assert.match(getElement("accounts").innerHTML, /is-updated/, "a real quota update should animate once");
  run("renderAccounts()");
  assert.doesNotMatch(getElement("accounts").innerHTML, /is-updated/, "quota animation marker must be consumed after one render");

  context.__quotaMeterState.accounts[0].quota = {rate_limits_by_limit_id:{codex:{primary:{usedPercent:4,windowDurationMins:300}},bonus:{secondary:{usedPercent:92,windowDurationMins:60}}}};
  run("renderAccounts()");
  rendered = getElement("accounts").innerHTML;
  assert.match(rendered, /aria-valuenow="96"/);
  assert.match(rendered, /bonus · 1h/);
  assert.match(rendered, /aria-valuenow="8"/);
  assert.match(rendered, /is-low/);

  run("refreshingAccounts.add('meter'); renderAccounts()");
  assert.match(getElement("accounts").innerHTML, /is-refreshing/);
  run("refreshingAccounts.delete('meter')");
  assert.match(html, /@media\(prefers-reduced-motion:reduce\)/);
}

function invalidCredentialAccountBehavior() {
  run("state = {native_account:null,accounts:[{id:'expired',name:'expired',prefix:'expired',credential_set:true,credential_status:'invalid',quota:null}],providers:[],models:[]}; renderAccounts()");
  assert.match(getElement("accounts").innerHTML, /登录已失效/);
}

async function quotaStateSyncBehavior() {
  const calls = [];
  context.__quotaSyncState = {native_account:null,accounts:[{id:'sync',name:'sync',prefix:'sync',credential_set:true,quota:{rate_limits:{primary:{usedPercent:40,windowDurationMins:300}}}}]};
  context.__quotaSyncApi = async path => {
    calls.push(path);
    if (path === '/api/accounts') return {native_account:null,accounts:[{id:'sync',name:'sync',prefix:'sync',credential_set:true,quota:{rate_limits:{primary:{usedPercent:25,windowDurationMins:300}}}}]};
    throw new Error('unexpected quota sync request: ' + path);
  };
  run("state = __quotaSyncState; api = __quotaSyncApi; renderAccounts()");
  assert.strictEqual(await run("refreshQuotaState()"), true);
  assert.deepStrictEqual(calls, ['/api/accounts'], 'quota display sync must only read EMP local account state');
  assert.match(getElement("accounts").innerHTML, /aria-valuenow="75"/);
  assert.strictEqual(await run("refreshQuotaState()"), false);
}

async function nativeOnlyIntegrationBehavior() {
  const nativeIntegration = {
    configuration: {state: 'emp_applied'},
    runtime: {state: 'catalog_unverified', target: 'emp', verified: false, action_required: true},
  };
  context.__nativeIntegrationStub = async path => {
    if (path === '/api/integration/enable' || path === '/api/integration') return nativeIntegration;
    throw new Error('unexpected native integration request: ' + path);
  };
  run("api = __nativeIntegrationStub; state = {accounts:[],providers:[],models:[]}; confirmIntegrationAction('enable')");
  assert.match(getElement('modal_title').textContent, /将 EMP 应用于 Codex/);
  assert.match(getElement('modal_body').innerHTML, /应用当前 EMP 设置.*重启 Codex/);
  await getElement('modal_submit').click();
  assert(getElement('modal_backdrop').classList.contains('hidden'));
  assert.strictEqual(getElement('integration_summary').textContent, 'EMP已启动，请重启Codex');
  assert.doesNotMatch(getElement('integration_summary').textContent, /无法确认|仅凭|未验证|共享后端/);
  assert.strictEqual(getElement('integration_reload').hidden, false);
  assert.doesNotMatch(getElement('integration_summary').textContent, /检查失败|仍加载旧目录/);

  context.__nativeIntegrationStub = async path => {
    if (path === '/api/integration/enable') {
      const error = new Error('Keep at least one model visible');
      error.payload = {...nativeIntegration, error: {code: 'empty_emp_catalog'}};
      throw error;
    }
    throw new Error('unexpected native integration request: ' + path);
  };
  run("api = __nativeIntegrationStub; confirmIntegrationAction('enable')");
  await getElement('modal_submit').click();
  assert.match(getElement('modal_status').textContent, /原生模型也可以/);
  run('closeModal()');
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
  run("state = {catalog_presentations:{'provider-a/model':{catalog_alias:'Legacy',show_context:true,reasoning_summary:'auto'}},catalog_family_presentations:{model:{catalog_alias:'',show_context:true,reasoning_summary:'hide'}},catalog_families:[{id:'native-model',default_display_name:'Native Model',display_name:'Native Model',context_window:258000,supports_reasoning_summaries:true,routes:[{id:'native-model',source_type:'native',source_id:''}]},{id:'model',default_display_name:'Model',display_name:'Model',context_window:258000,supports_reasoning_summaries:true,routes:[{id:'provider-a/model',source_type:'provider',source_id:'provider-a'}]}],subscription_models:[{id:'native-model',display_name:'Native Model',context_window:258000}],providers:[{id:'provider-a',name:'Provider A'}],accounts:[],models:[{id:'provider-a/model',provider:'provider-a',upstream_id:'model',display_name:'Model',context_window:258000,enabled:true}]} ");
  run("renderCatalogDisplay()");
  assert.match(getElement("catalog_display_models").innerHTML, /data-catalog-alias/);
  assert.match(getElement("catalog_display_models").innerHTML, /provider-a\/model/);
  const alias = catalogAliases.find(input => input.dataset.route === "model");
  const contextInput = catalogContexts.find(input => input.dataset.route === "model");
  const preview = catalogPreviews.find(input => input.dataset.route === "model");
  assert(alias && contextInput && preview, "display controls must be rendered once per model family");
  alias.value = "General";
  contextInput.checked = false;
  run("updateCatalogDisplayPreview('model')");
  assert.strictEqual(preview.textContent, "General", "context visibility must update the preview before saving");
  context.__persistStateStub = async (_message, candidate) => { context.__savedCandidate = candidate; context.state = candidate; };
  run("__realPersistState = persistState; persistState = __persistStateStub");
  await run("saveCatalogDisplay()");
  run("persistState = __realPersistState");
  const saved = run("__savedCandidate.catalog_family_presentations['model']");
  assert.strictEqual(saved.catalog_alias, "General");
  assert.strictEqual(saved.show_context, false);
  assert.strictEqual(saved.reasoning_summary, "hide", "hidden reasoning policy must survive a display-only save");
  assert.strictEqual(run("__savedCandidate.catalog_presentations['provider-a/model']"), undefined);
  run("openManualModelModal('provider-a/model')");
  assert.doesNotMatch(getElement("modal_body").innerHTML, /Codex 显示名称|Reasoning summary|推理摘要/);
  assert.match(getElement("modal_body").innerHTML, /模型显示/);
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
  assert.strictEqual(run("migrationFilename()"), "EMP.emp");
  run("state.emp_version = '../../unsafe'");
  assert.strictEqual(run("migrationFilename()"), "EMP.emp");
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

async function runtimeSettingsIsolationBehavior() {
  let saved = {accounts:[], providers:[], models:[], codex_runtime_sources:['auto'], subscription_search:{enabled:false, account_id:''}};
  let rejectSelection = false;
  const configWrites = [];
  const compatibility = () => ({
    status:'recommended', preferences:[...saved.codex_runtime_sources],
    runtimes:[
      {source:'codex_app', selectable:true, installed:'0.152.1', status:'recommended'},
      {source:'cursor', selectable:true, installed:'0.150.0', status:'supported'},
      {source:'path_cli', selectable:false, installed:'0.146.0', status:'unsupported'},
    ].map(item => ({...item, targeted:item.selectable && (saved.codex_runtime_sources.includes('auto') || saved.codex_runtime_sources.includes(item.source))})),
  });
  context.__runtimeSettingsApi = async (path, options = {}) => {
    if (path === '/api/runtime/select') {
      if (rejectSelection) throw new Error('selection rejected');
      saved.codex_runtime_sources = JSON.parse(options.body).sources;
      return compatibility();
    }
    if (path === '/api/runtime/scan') return compatibility();
    if (path === '/api/integration') return {codex_compatibility:compatibility(), configuration:{state:'native'}, runtime:{state:'not_checked'}};
    if (path === '/api/config') {
      if (options.method === 'POST') {
        configWrites.push(JSON.parse(options.body));
        saved = JSON.parse(options.body);
      }
      return JSON.parse(JSON.stringify(saved));
    }
    if (path === '/api/catalog/refresh') return {};
    throw new Error('unexpected settings API '+path);
  };
  context.__runtimeConfig = JSON.parse(JSON.stringify(saved));
  run('state = __runtimeConfig; runtimeSelectionDraft = null; api = __runtimeSettingsApi');
  await run('loadIntegration()');
  runtimeInputs[1].checked = false;
  run('stageCodexRuntimeSelection()');
  await run('saveCodexRuntimeSelection()');
  assert.deepStrictEqual(Array.from(run('state.codex_runtime_sources')), ['codex_app'], 'saving selection must update the general form snapshot');

  getElement('subscription_search_enabled').checked = true;
  await run('saveSubscriptionSearch()');
  assert.deepStrictEqual(configWrites.at(-1).codex_runtime_sources, ['codex_app']);
  assert.strictEqual(saved.subscription_search.enabled, true);
  assert.strictEqual(saved.subscription_search.account_id, '', 'web search account selection must remain automatic');
  assert.deepStrictEqual(runtimeInputs.map(input => input.checked), [true,false,false], 'search save must keep saved client selection');

  runtimeInputs[0].checked = false;
  runtimeInputs[1].checked = true;
  run('stageCodexRuntimeSelection()');
  await run('saveSubscriptionSearch()');
  await run('scanCodexRuntimes()');
  assert.deepStrictEqual(saved.codex_runtime_sources, ['codex_app'], 'search save must not silently save draft checkboxes');
  assert.deepStrictEqual(runtimeInputs.map(input => input.checked), [false,true,false], 'draft must survive search save and rescan');
  assert.strictEqual(getElement('codex_runtime_dirty').hidden, false);

  rejectSelection = true;
  assert.strictEqual(await run('saveCodexRuntimeSelection()'), false);
  assert.deepStrictEqual(runtimeInputs.map(input => input.checked), [false,true,false], 'failed save must preserve draft for retry');
  rejectSelection = false;
  await run('saveCodexRuntimeSelection()');
  assert.deepStrictEqual(saved.codex_runtime_sources, ['cursor']);
  assert.strictEqual(getElement('codex_runtime_dirty').hidden, true);

  runtimeInputs[1].checked = false;
  run('stageCodexRuntimeSelection()');
  await run('saveSubscriptionSearch()');
  assert(runtimeInputs.every(input => !input.checked), 'empty draft must not be treated as automatic selection');
  await run('useAutomaticCodexRuntimes()');
  assert.deepStrictEqual(saved.codex_runtime_sources, ['auto']);
  assert.deepStrictEqual(runtimeInputs.map(input => input.checked), [true,true,false]);
  assert.strictEqual(getElement('codex_runtime_dirty').hidden, true);
}

async function initialRenderIsolationBehavior() {
  const calls = [];
  context.__initialLoadApi = async path => {
    calls.push(path);
    if (path === "/api/config") return {accounts:[], providers:[], models:[]};
    if (path === "/api/integration") return {
      configuration:{state:"native"},
      runtime:{state:"not_checked"},
      codex_compatibility:{status:"unavailable", runtimes:[]},
    };
    throw new Error("unexpected initial load API " + path);
  };
  run("api = __initialLoadApi; fill = () => { throw new Error('panel render failed'); }");
  await run("load()");
  assert.deepStrictEqual(calls, ["/api/config", "/api/integration"], "Codex status must load even when another panel cannot render");
  assert.strictEqual(getElement("integration_badge").textContent, "Native");
  assert.strictEqual(getElement("status").textContent, "panel render failed");
}

function updateBehavior() {
  run("setLanguage('zh-CN'); renderUpdate({state:'available',current_version:'0.9.8',latest_version:'0.9.9',supported:true})");
  assert.match(getElement('update_status').textContent, /0\.9\.9/);
  assert.strictEqual(getElement('update_install').hidden, false);
  run("renderUpdate({state:'available',current_version:'0.9.8',latest_version:'0.9.9',supported:false})");
  assert.strictEqual(getElement('update_install').hidden, true);
  run("renderUpdate({state:'waiting',current_version:'0.9.8',supported:true})");
  assert.match(getElement('update_status').textContent, /等待当前请求结束/);
  run("renderUpdate({state:'error',error:'checksum_mismatch',current_version:'0.9.8',supported:true})");
  assert.match(getElement('update_status').textContent, /校验失败/);
  run("setLanguage('zh-CN')");
}

(async () => {
  updateBehavior();
  await integrationBehavior();
  pickerBehavior();
  duplicateAccountBehavior();
  quotaHistoryBehavior();
  await quotaHistoryRaceBehavior();
  performanceDiagnosticsBehavior();
  providerDiscoveryErrorBehavior();
  quotaMeterBehavior();
  invalidCredentialAccountBehavior();
  await quotaStateSyncBehavior();
  await nativeAccountBehavior();
  await nativeOnlyIntegrationBehavior();
  await accountEmojiBehavior();
  await quotaErrorBehavior();
  modelGroupBehavior();
  officialPresetBehavior();
  capabilityMetadataBehavior();
  await presentationBehavior();
  presentationMigrationBehavior();
  modalDismissalBehavior();
  await atomicStateBehavior();
  await runtimeSettingsIsolationBehavior();
  await initialRenderIsolationBehavior();
  process.stdout.write("web DOM behavior: ok\n");
})().catch(error => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
