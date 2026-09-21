"use strict";

const assert = require("assert");
const fs = require("fs");
const vm = require("vm");

class ClassList {
  constructor(initial = []) { this.values = new Set(initial); }
  add(value) { this.values.add(value); }
  remove(value) { this.values.delete(value); }
  contains(value) { return this.values.has(value); }
  toggle(value, force) { if (force) this.add(value); else this.remove(value); }
}

const elements = new Map();
let discoveredOptions = [];
let catalogAliases = [];
let catalogContexts = [];
let catalogPreviews = [];
let subscriptionModelInputs = [];
let runtimeInputs = [];
let quotaControls = [];
let quotaRangeButtons = [];

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
    this.innerHTMLWrites = 0;
    this.attributes = {};
  }
  set innerHTML(value) {
    this._innerHTML = String(value);
    this.innerHTMLWrites++;
    if (this.id === "modal_body") { parseDiscoveredOptions(this._innerHTML); parseSubscriptionOptions(this._innerHTML); quotaRangeButtons = parseQuotaButtons(this._innerHTML); }
    if (this.id === "subscription_model_list") parseSubscriptionOptions(this._innerHTML);
    if (this.id === "catalog_display_models") parseCatalogDisplay(this._innerHTML);
    if (this.id === "codex_runtimes") parseRuntimeInputs(this._innerHTML);
    if (this.id === "quota_history_controls") quotaControls = parseQuotaButtons(this._innerHTML);
    if (this.id === "quota_history_content") for (const match of this._innerHTML.matchAll(/\bid="([^"]+)"/g)) elements.set(match[1], new Element(match[1]));
  }
  get innerHTML() { return this._innerHTML; }
  querySelector(selector) {
    if (selector === 'input[name="discovered_model"]') return this.input || null;
    if (selector.startsWith('#') && this._innerHTML.includes(`id="${selector.slice(1)}"`)) return getElement(selector.slice(1));
    return null;
  }
  querySelectorAll(selector) { return selector === '[data-limit-id], [data-window]' ? quotaControls : []; }
  setAttribute(name, value) { this.attributes[name] = value; }
  getAttribute(name) { return this.attributes[name]; }
  click() { if (this.onclick) return this.onclick(); }
  remove() {}
}

function parseQuotaButtons(html) {
  return [...html.matchAll(/<button[^>]*data-(limit-id|window|range)="([^"]+)"[^>]*>/g)].map(match => {
    const button = new Element();
    button.dataset[match[1] === 'limit-id' ? 'limitId' : match[1]] = match[2];
    if (/class="[^"]* active/.test(match[0])) button.classList.add('active');
    return button;
  });
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
    if (selector === '.quota-ranges button[data-range]') return quotaRangeButtons;
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
  "integration_summary", "integration_toggle", "codex_compatibility", "codex_runtime_save",
  "codex_runtime_scan", "codex_runtimes", "language_select", "theme_select",
  "catalog_display_search", "catalog_display_toggle", "catalog_display_models",
  "diagnostics_summary", "performance_records", "diagnostics_records", "accounts", "providers", "models",
]) getElement(id);

const html = fs.readFileSync(process.argv[2], "utf8");
assert.doesNotMatch(html, /实际调用时自动使用其中兼容性最可靠的一个/);
assert.match(html, /class="workspace-layout"/);
assert.match(html, /<aside class="workspace-side">/);
assert.match(html, /onclick="openUpdate\(\)"/);
assert.doesNotMatch(html, /<details class="(?:header-menu|action-menu)">/, "primary actions must stay visible");
for (const visibleControl of [
  /onclick="selectMigrationFile\(\)"/,
  /onclick="exportMigration\(\)"/,
  /id="language_select"/,
  /id="theme_select"/,
  /onclick="quitEmp\(\)"/,
]) assert.match(html, visibleControl, `page control must stay visible: ${visibleControl}`);
assert.match(html, /button,\.repo-link\{[^}]*height:36px[^}]*margin:0[^}]*white-space:nowrap/);
assert.match(html, /@media\(max-width:760px\)\{[\s\S]*?\.page-header\{flex-direction:column;align-items:stretch\}/);
assert.match(html, /\.model-card\{grid-template-columns:minmax\(220px,1fr\) auto auto;/, "desktop model cards must keep details and actions on one compact row");
assert.match(html, /\.model-card \.entity-card-actions\{grid-column:3;grid-row:1;flex-wrap:nowrap;/, "desktop model actions must remain aligned and visible");
assert.match(html, /\.model-card \.entity-card-meta\{grid-column:1\/-1;grid-row:2;[^}]*white-space:normal;/, "model metadata must remain fully readable across the card");
assert.match(html, /\.quota-credit span\{display:block;/, "credit details must use separate visual lines");
assert.match(html, /\.display-row\{grid-template-columns:minmax\(0,1fr\) auto auto;/, "display cards must reserve one top row for the model and controls");
assert.match(html, /\.display-row \.inline-check\{grid-column:2;grid-row:1;/, "context checkbox must stay at the upper right");
assert.match(html, /\.display-row>button\{grid-column:3;grid-row:1;/, "advanced options must stay at the upper right");
assert.match(html, /href="https:\/\/github.com\/Killow1998\/EasyMultiProvider" target="_blank" rel="noopener noreferrer"/);
assert.doesNotMatch(html, /id="subscription_search_account"/);
assert.doesNotMatch(html, /data-catalog-summary/);
assert.doesNotMatch(html, /不会自动补|not added automatically/, "users must not manage provider API path suffixes");
assert.match(html, /Icon paths derived from Lucide \(ISC\)/);
assert.strictEqual(
  Array.from(html.matchAll(/button\[data-icon="[^"]+"\](?:,\.quota-reset)?\{--button-icon:url\("data:image\/svg\+xml,%3Csvg%20/g)).length,
  14,
  "all action icons must come from the embedded Lucide set",
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
  assert.strictEqual(getElement("integration_toggle").dataset.action, "restore");

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
  assert.strictEqual(getElement("integration_toggle").dataset.action, "restore");
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
  const cards = [...html.matchAll(/<article class="entity-card account-card[^\"]*"[\s\S]*?<\/article>/g)].map(match => match[0]);
  assert.strictEqual(cards.length, 3, "each account should render as one aligned card");
  const nativeCard = cards.find(card => card.includes("refreshAccount('@native')"));
  assert.doesNotMatch(nativeCard, /removeAccount\('@native'\)/);
  const duplicateCard = cards.find(card => card.includes("refreshAccount('same-login-account')"));
  assert(duplicateCard, "duplicate account card must render");
  assert.match(duplicateCard, /account-duplicate/);
  assert.doesNotMatch(duplicateCard, /editAccount\('same-login-account'\)/);
  assert.doesNotMatch(duplicateCard, /<details class="action-menu">/);
  assert.match(duplicateCard, /openQuotaHistory\('same-login-account'\)/);
}

function quotaHistoryHtml() {
  const content = getElement('quota_history_content').innerHTML;
  return content.includes('id="quota_history_plot"') ? content + ['quota_history_controls','quota_history_legend','quota_history_plot'].map(id => getElement(id).innerHTML).join('') : content;
}

function quotaHistoryBehavior() {
  run("renderQuotaHistory({series:[]}, '1d')");
  assert.match(quotaHistoryHtml(), /暂无额度记录/);
  context.__quotaPayload = {series:[{limit_id:'codex',window_kind:'primary',window_minutes:10080,points:[{observed_at:1000,remaining_percent:80},{observed_at:1300,remaining_percent:75}]}]};
  run("renderQuotaHistory(__quotaPayload, '1h')");
  const html = quotaHistoryHtml();
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
  const distant = {dataset:{x:'300',y:'120',time:'1300',value:'50',label:'主窗口'},radius:'1.8',setAttribute(name,value) { if (name === 'r') this.radius = value; }};
  const guide = {values:{hidden:''},setAttribute(name,value) { this.values[name] = value; },removeAttribute(name) { delete this.values[name]; }};
  const tooltip = {hidden:true,style:{},innerHTML:''};
  context.__quotaHoverSvg = {
    getBoundingClientRect: () => ({left:0,width:628}),
    querySelectorAll: selector => selector === '[data-quota-point]' ? [first,second,distant] : [],
    querySelector: selector => selector === '.quota-chart-guide' ? guide : null,
    parentElement: {querySelector: selector => selector === '.quota-chart-tooltip' ? tooltip : null},
  };
  context.__quotaHoverTarget = {ownerSVGElement:context.__quotaHoverSvg};
  run("quotaChartHover({currentTarget:__quotaHoverTarget,clientX:95})");
  assert.strictEqual('hidden' in guide.values, false);
  assert.strictEqual(first.radius, '4');
  assert.strictEqual(second.radius, '4');
  assert.strictEqual(distant.radius, '1.8');
  assert.strictEqual(tooltip.hidden, false);
  assert.match(tooltip.innerHTML, /主窗口 · 80%/);
  assert.match(tooltip.innerHTML, /次窗口 · 60%/);
  run("quotaChartLeave({currentTarget:__quotaHoverTarget})");
  assert.strictEqual('hidden' in guide.values, true);
  assert.strictEqual(tooltip.hidden, true);

  context.__groupedQuota = {series: ['codex','codex_bengalfox'].flatMap(limit_id => [300,10080].map(window_minutes => ({limit_id,window_minutes,points:[{observed_at:1000,remaining_percent:window_minutes === 300 ? 42 : 88}]})))};
  run("activeQuotaLimit='codex'; activeQuotaWindow='300'; renderQuotaHistory(__groupedQuota,'1d')");
  let grouped = quotaHistoryHtml();
  assert.match(grouped, />Codex Spark<\/button>/);
  assert.match(grouped, /data-label="5h"/);
  assert.doesNotMatch(grouped, /data-label="7d"|data-label="Codex Spark/);
  run("selectQuotaHistoryWindow('10080')");
  grouped = quotaHistoryHtml();
  assert.match(grouped, /data-label="7d"/);
  assert.doesNotMatch(grouped, /data-label="5h"/);
  run("selectQuotaHistoryGroup('codex_bengalfox')");
  grouped = quotaHistoryHtml();
  assert.match(grouped, /data-label="Codex Spark · 7d"/);
  assert.doesNotMatch(grouped, />codex_bengalfox|data-label="7d"/);
  assert.strictEqual(run("quotaLimitLabel('unrecognized')"), 'unrecognized');
}

function quotaHistoryPeriodsBehavior() {
  context.__periodSeries = [{window_minutes:300,points:[
    {observed_at:1000,remaining_percent:5,resets_at:1200},
    {observed_at:1300,remaining_percent:100,resets_at:19200},
    {observed_at:1600,remaining_percent:100,resets_at:18000},
    {observed_at:19000,remaining_percent:100,resets_at:36000},
    {observed_at:20000,remaining_percent:90,resets_at:36030},
  ]}];
  const boundaries = JSON.parse(JSON.stringify(run('quotaResetBoundaries(__periodSeries,1100,40000)')));
  assert.deepStrictEqual(boundaries, [{at:1200,kind:'reset'},{at:1600,kind:'observed'},{at:18000,kind:'reset'}], 'use the recorded reset deadline, first observation for early resets, and ignore small deadline jitter');
  assert.match(run('quotaPointBreak(__periodSeries[0].points[1],__periodSeries[0].points[2])'), /新的额度周期/);
  assert.strictEqual(run('quotaResetBoundaries([{points:[{observed_at:1000,remaining_percent:90},{observed_at:20000,remaining_percent:80}]}],0,40000).length'), 0, 'missing reset times must not produce invented 5-hour periods');
  assert.strictEqual(run('quotaResetBoundaries(__periodSeries,1200,18000).length'), 1, 'viewport edges must not produce duplicate or empty sections');

  const previousTimezone = process.env.TZ;
  process.env.TZ = 'America/Los_Angeles';
  try {
    for (const [start,end,dayLength] of [
      ['2026-10-30T12:00:00-07:00','2026-11-04T12:00:00-08:00',25],
      ['2026-03-06T12:00:00-08:00','2026-03-11T12:00:00-07:00',23],
    ]) {
      context.__dayView = {start:Date.parse(start)/1000,end:Date.parse(end)/1000,mode:'day'};
      const days = run('quotaChartSections([],__dayView)');
      assert(days.boundaries.every(item => new Date(item.at*1000).getHours() === 0));
      assert(days.sections.some(item => item.end-item.start === dayLength*3600), 'week divisions must follow local calendar days across DST');
    }
  } finally { if (previousTimezone === undefined) delete process.env.TZ; else process.env.TZ = previousTimezone; }
}

async function quotaHistorySwitchingBehavior() {
  const end = Date.parse('2026-09-12T12:00:00Z')/1000, start = end-86400;
  context.__switchPayload = {end_at:end,series:[
    {limit_id:'codex',window_minutes:300,points:[
      {observed_at:start-300,remaining_percent:80,resets_at:start+7200},
      {observed_at:start+7500,remaining_percent:100,resets_at:start+25200},
      {observed_at:start+18000,remaining_percent:100,resets_at:start+36000},
      {observed_at:end-300,remaining_percent:20,resets_at:end+600},
    ]},
    {limit_id:'codex',window_minutes:10080,points:[
      {observed_at:start-300,remaining_percent:10,resets_at:start+30000},
      {observed_at:start+31000,remaining_percent:100,resets_at:start+634800},
      {observed_at:end-300,remaining_percent:90,resets_at:start+634800},
    ]},
  ]};
  run("quotaHistoryZoom=[]; activeQuotaLimit='codex'; activeQuotaWindow='300'; renderQuotaHistory(__switchPayload,'1d')");
  const frame = getElement('quota_history_content'), controls = getElement('quota_history_controls'), plot = getElement('quota_history_plot');
  const frameWrites = frame.innerHTMLWrites, controlsWrites = controls.innerHTMLWrites, plotWrites = plot.innerHTMLWrites;
  run("renderQuotaHistory(__switchPayload,'1d')");
  assert.strictEqual(plot.innerHTMLWrites, plotWrites, 'an unchanged refresh must not replace the SVG or hover state');
  assert.match(plot.innerHTML, new RegExp(`data-end="${start+7200}"`));
  run("selectQuotaHistoryWindow('10080')");
  assert.match(plot.innerHTML, new RegExp(`data-end="${start+30000}"`));
  assert.doesNotMatch(plot.innerHTML, new RegExp(`data-end="${start+7200}"`));
  assert.strictEqual(controls.innerHTMLWrites, controlsWrites, 'window switches must preserve the focused controls');
  assert(quotaControls.find(button => button.dataset.window === '10080').classList.contains('active'));

  const waiting = [], calls = [];
  context.__switchApi = path => { calls.push(path); return new Promise((resolve,reject) => waiting.push({resolve,reject})); };
  run('__savedSwitchApi=api; __savedSwitchSync=refreshQuotaState; api=__switchApi; refreshQuotaState=async () => {}');
  try {
    const beforeRefresh = plot.innerHTML;
    const pending = run("loadQuotaHistory('test','1d')");
    assert.strictEqual(plot.innerHTML, beforeRefresh, 'a pending fetch must leave the chart visible');
    for (const range of ['1h','1d','1w','all']) run(`selectQuotaHistoryRange('${range}')`);
    assert.strictEqual(calls.length, 1, 'range switches must reuse the loaded 15-day snapshot');
    assert(calls[0].endsWith('range=all'));
    waiting[0].resolve(context.__switchPayload); await pending;
    assert.strictEqual(run('activeQuotaRange'), 'all', 'a late fetch must not undo the latest selected range');
    run("selectQuotaHistoryRange('1d'); selectQuotaHistoryWindow('300')");
    const segment = plot.innerHTML.match(/data-start="([^"]+)" data-end="([^"]+)" data-mode="([^"]+)"[^>]*role="button"/);
    assert(segment, 'reset regions must offer click/keyboard drill-down');
    context.__zoomTarget = {dataset:{start:segment[1],end:segment[2],mode:segment[3]}};
    run('zoomQuotaHistory(__zoomTarget)');
    assert.strictEqual(getElement('quota_history_back').disabled, false);
    const zoomed = plot.innerHTML;
    const zoomRefresh = run("loadQuotaHistory('test','1d')");
    waiting[1].resolve({...context.__switchPayload,end_at:end+300}); await zoomRefresh;
    assert.strictEqual(plot.innerHTML, zoomed, 'auto refresh must retain the drilled-down period');
    const zoomRange = getElement('quota_history_range').textContent;
    run("selectQuotaHistoryWindow('10080')");
    assert.strictEqual(getElement('quota_history_range').textContent, zoomRange, 'changing the quota window must preserve the time interval under inspection');
    run('backQuotaHistory()');
    assert.strictEqual(run('quotaHistoryZoom.length'), 0);
    assert.strictEqual(getElement('quota_history_back').disabled, true);
    const beforeFailure = plot.innerHTML;
    const failed = run("loadQuotaHistory('test','1d')");
    waiting[2].reject(new Error('offline')); await failed;
    assert.strictEqual(plot.innerHTML, beforeFailure, 'a failed refresh must not erase existing data');
    assert.strictEqual(getElement('modal_status').textContent, 'offline');
    assert.strictEqual(getElement('quota_history_loading').hidden, true);
    assert.strictEqual(frame.innerHTMLWrites, frameWrites, 'switches and refreshes must preserve the chart frame');
  } finally { run('api=__savedSwitchApi; refreshQuotaState=__savedSwitchSync'); }
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
    const latest = quotaHistoryHtml();
    waiting[0]({series:[]});
    await oldRequest;
    assert.strictEqual(quotaHistoryHtml(), latest);
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
  run('__performancePayload.health.fallback_attempt_count = 2; renderDiagnostics(__performancePayload)');
  assert.match(getElement('diagnostics_summary').textContent, /另有 2 次连接回退尝试/);
  assert.match(run('diagnosticFailureHtml({recovery_mode:"native_http_fallback",status:502,error_class:"tls_failure"})'), /回退前尝试/);
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

async function cacheUsageBehavior() {
  const period = {start:1789214400,end:1789215000,complete:false,rate:80,call_count:3,sample_count:2,hit_count:1};
  const model = {model_id:'external/gemini-3.8-flash',provider_id:'NA2H',speed_mode:'unknown',rate:80,input_tokens:1000,cached_input_tokens:800,call_count:3,sample_count:2,hit_count:1,periods:[period]};
  context.__cachePayload = {capacity:512,cache:{models:[model, {...model, model_id:'<img onerror=bad>',rate:null,sample_count:0,periods:[{...period,rate:null,sample_count:0}]}, {...model,model_id:'deepseek',rate:0,hit_count:0,periods:[{...period,rate:0,hit_count:0,complete:true}]}]}};
  run('renderDiagnostics(__cachePayload)');
  const html = getElement('cache_records').innerHTML;
  assert.match(html, /external\/gemini-3.8-flash · NA2H/);
  assert.match(html, /80\.0%/);
  assert.match(html, />0\.0%</);
  assert.match(html, /未提供/);
  assert.match(html, /统计中/);
  assert.match(html, /有效记录 2 \/ 3/);
  assert.match(html, /命中请求 1 \/ 2/);
  assert.match(html, /800 \/ 1,000 token/);
  assert.match(html, /每 10 分钟汇总，空闲时段不显示/);
  assert.match(html, /不代表与原生调用的差异/);
  assert.doesNotMatch(html, /<img|NaN|Infinity|未标记/);
  const target = getElement('cache_records');
  const query = target.querySelectorAll;
  target.querySelectorAll = () => [{dataset:{cacheKey:JSON.stringify([model.model_id,model.provider_id,model.speed_mode,model.endpoint_fingerprint])}}];
  run('renderCacheUsage(__cachePayload)');
  assert.match(target.innerHTML, /data-cache-key="[^"]*" open/);
  target.querySelectorAll = query;
  run("renderCacheUsage({cache:{models:[]}})");
  assert.match(target.innerHTML, /还没有模型调用记录/);

  let resolveRequest;
  context.__cacheApi = () => new Promise(resolve => { resolveRequest = resolve; });
  run('var __savedCacheApi = api; api = __cacheApi');
  const pending = run('loadDiagnostics()');
  run('closeModal()');
  target.innerHTML = 'closed';
  resolveRequest(context.__cachePayload);
  await pending;
  assert.strictEqual(target.innerHTML, 'closed', 'late metrics must not redraw a closed modal');
  run('api = __savedCacheApi');
  assert.strictEqual(run('diagnosticsTimer'), null);
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

function creditLayoutBehavior() {
  context.__creditLayoutState = {
    native_account: null,
    accounts: [{id:'credit-lines',name:'credit-lines',prefix:'credit-lines',credential_set:true,quota:{credits:{balance:1200,individual_limit:{remaining_percent:73},reset_credits:{available_count:2,credits:[{expires_at:1893553445},{expires_at:1896321906}]}}}}],
  };
  run("state = __creditLayoutState; renderAccounts()");
  const rendered = getElement("accounts").innerHTML;
  const block = rendered.match(/<div class="muted quota-credit">([\s\S]*?)<\/div>/);
  assert(block, "credit details must render in a dedicated block");
  assert.strictEqual((block[1].match(/<span>/g) || []).length, 5, "credit, monthly, reset, and each expiry must have their own line");
  assert.match(block[1], /<span>credit 1200<\/span><span>monthly 73% left<\/span><span>reset 2<\/span><span>expire /);
  assert.doesNotMatch(block[1], / · /, "credit details must not be joined into one line");
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

async function quotaNotificationBehavior() {
  const instances = [];
  context.EventSource = class {
    constructor(url) { this.url = url; this.listeners = {}; this.closed = false; instances.push(this); }
    addEventListener(name, listener) { this.listeners[name] = listener; }
    close() { this.closed = true; }
  };
  let resolveFirst;
  let reads = 0;
  const snapshot = remaining => ({native_account:null, accounts:[{id:'live',prefix:'live',credential_set:true,quota:{rate_limits:{primary:{usedPercent:100-remaining,windowDurationMins:300}}}}], refresh_errors:{}});
  context.__liveQuotaApi = async path => {
    assert.strictEqual(path, '/api/accounts');
    reads++;
    return reads === 1 ? new Promise(resolve => { resolveFirst = resolve; }) : snapshot(70);
  };
  run("closeModal(); state={accounts:[],providers:[],models:[]}; api=__liveQuotaApi; startQuotaEvents(); startQuotaEvents()");
  assert.strictEqual(instances.length, 1, 'reloading the page state must not add subscribers');
  const source = instances[0];
  assert.strictEqual(source.url, '/api/accounts/events');
  source.onopen();
  assert.strictEqual(run('quotaEventsConnected'), true);
  const pending = run('requestQuotaSync()');
  source.listeners['quota-updated']();
  source.listeners['quota-updated']();
  resolveFirst(snapshot(20));
  await pending;
  assert.strictEqual(reads, 2, 'events during a read must coalesce into a fresh read');
  assert.match(getElement('accounts').innerHTML, /aria-valuenow="70"/);
  source.onerror();
  assert.strictEqual(run('quotaEventsConnected'), false, 'disconnect enables polling fallback');
  run('stopQuotaEvents()');
  assert.strictEqual(source.closed, true);
  source.onopen();
  assert.strictEqual(run('quotaEventsConnected'), false, 'closed subscriptions cannot change state');
  document.visibilityState = 'hidden';
  run('startQuotaEvents()');
  assert.strictEqual(instances.length, 1, 'hidden pages must release their subscription');
  document.visibilityState = 'visible';
  run('startQuotaEvents()');
  assert.strictEqual(instances.length, 2);
  run('stopQuotaEvents()');
  delete context.EventSource;

  context.__liveQuotaApi = async () => ({...snapshot(70),refresh_errors:{live:'quota_auth_required'}});
  run('api=__liveQuotaApi');
  await run('requestQuotaSync()');
  assert.match(getElement('accounts').innerHTML, /刷新失败/);
  context.__liveQuotaApi = async () => snapshot(70);
  run('api=__liveQuotaApi; quotaHistoryLoading=true; quotaUpdatePending=true; closeModal()');
  await run('drainQuotaUpdates()');
  assert.strictEqual(run('quotaHistoryLoading'), false, 'closing a loading chart cannot block account updates');
  assert.doesNotMatch(getElement('accounts').innerHTML, /刷新失败/);
}

function failureDetailsBehavior() {
  context.__failurePayload = {records:[
    {model_id:'model-ok',status:200,error_class:'none'},
    {model_id:'cancelled-model',error_class:'client_cancelled'},
    {model_id:'<img src=x onerror=alert(1)>',provider_id:'test-provider',status:502,error_class:'tls_failure',protocol:'responses',transport:'websocket',request_id:'0123456789abcdef',observed_at:'2026-09-12T12:00:00Z',duration_ms:2300,prompt:'private-test-prompt',response_text:'private-test-answer'},
    {model_id:'model-with-tool',status:502,error_class:'upstream_close_after_tool',tool_activity:true},
  ]};
  run('renderDiagnostics(__failurePayload)');
  const details = getElement('diagnostics_records').innerHTML;
  assert.match(details, /test-provider/);
  assert.match(details, /0123456789abcdef/);
  assert.match(details, /0912-/);
  assert.match(details, /&lt;img/);
  assert.match(details, /请先检查任务进度/);
  assert.doesNotMatch(details, /<img|private-test-prompt|private-test-answer|cancelled-model|model-ok/);
  assert.match(html, /<details class="failure-list"><summary>/);
  assert.doesNotMatch(html, /<details class="failure-list" open/);
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
  assert.strictEqual(getElement('integration_toggle').dataset.action, 'restore');
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
  assert.match(getElement("accounts").innerHTML, /<strong>🚢<\/strong><span class="pill">ship<\/span>/);
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
  assert.strictEqual((html.match(/class="entity-card model-card/g) || []).length, 4);
  assert.strictEqual((html.match(/<details class="action-menu">/g) || []).length, 0);
  assert.strictEqual((html.match(/testModelVision\('/g) || []).length, 4);
  assert.strictEqual((html.match(/removeModel\('/g) || []).length, 4);
  assert.doesNotMatch(html, /<table>/, "model actions should not be squeezed into table cells");
  assert.doesNotMatch(html, /<br>/, "model metadata should wrap naturally instead of forcing extra lines");
  assert.match(html, /<\/div>\s*<div class="entity-card-meta">/, "model metadata must use a full-width row outside the narrow title column");
}

function providerCardBehavior() {
  run("state = {providers:[{id:'provider-a',name:'Provider A',base_url:'https://example.test/v1',protocol:'chat_completions',auth_mode:'api_key'}],models:[{provider:'provider-a',enabled:true}]}; renderProviders()");
  const html = getElement("providers").innerHTML;
  assert.match(html, /class="entity-card provider-card"/);
  assert.match(html, /discoverProvider\('provider-a'\)/);
  assert.match(html, /editProvider\('provider-a'\)/);
  assert.doesNotMatch(html, /<details class="action-menu">/);
  assert.match(html, /toggleProviderModels\('provider-a', false\)/);
  assert.match(html, /removeProvider\('provider-a'\)/);
  assert.doesNotMatch(html, /<table>/);
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
  const unconfirmedCard = [...html.matchAll(/<article class="entity-card model-card[^\"]*"[\s\S]*?<\/article>/g)].map(match => match[0]).find(card => card.includes('provider-a/unconfirmed'));
  assert(unconfirmedCard, "unconfirmed model card must render");
  assert.match(unconfirmedCard, /图像未知/);
  assert.doesNotMatch(unconfirmedCard, /输入 文本/, "unknown provenance must not display as confirmed support");

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
  await run("persistState('save', __candidate)");
  await run("catalogSync");
  assert.strictEqual(run("state.accounts[0].prefix"), "saved", "a persisted save remains authoritative when catalog refresh fails");
  assert.match(getElement("status").textContent, /设置已保存.*同步失败.*refresh rejected/);

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

  run("state = {catalog_presentations:{},accounts:[],providers:[{id:'provider-a',name:'Provider A'}],models:[{id:'provider-a/model',provider:'provider-a',upstream_id:'model',input_modalities:['text'],output_modalities:['text','audio'],output_limit:4000,capability_sources:{input_modalities:{source:'advertised'},output_modalities:{source:'advertised'}}}]}; openManualModelModal('provider-a/model')");
  assert.match(getElement("modal_body").innerHTML, /id="modal_model_input_modalities" value="text"/);
  assert.match(getElement("modal_body").innerHTML, /id="modal_model_vision"/);
  assert.strictEqual(getElement("modal_model_vision").value, "unsupported");
  getElement("modal_model_provider").value = "provider-a";
  getElement("modal_model_upstream").value = "model";
  getElement("modal_model_input_modalities").value = "text";
  getElement("modal_model_vision").value = "supported";
  context.__persistStateStub = async (_message, candidate) => { context.__savedCandidate = candidate; context.state = candidate; };
  run("__realPersistState = persistState; persistState = __persistStateStub");
  await run("saveManualModel()");
  run("persistState = __realPersistState");
  run("state = __savedCandidate");
  const savedModel = run("__savedCandidate.models[0]");
  assert.deepStrictEqual(Array.from(savedModel.input_modalities), ["text", "image"]);
  assert.strictEqual(savedModel.capability_sources.input_modalities.source, "manual");
  assert.deepStrictEqual(Array.from(savedModel.output_modalities), ["text", "audio"], "editing input must preserve output modalities");
  assert.strictEqual(savedModel.output_limit, 4000, "editing input must preserve discovered limits");
  assert.strictEqual(savedModel.capability_sources.output_modalities.source, "advertised");
  run("openManualModelModal('provider-a/model')");
  getElement("modal_model_provider").value = "provider-a";
  getElement("modal_model_upstream").value = "model";
  getElement("modal_model_input_modalities").value = "text, invalid!";
  await assert.rejects(run("saveManualModel()"), /请输入有效的其他输入模态/);
  assert.deepStrictEqual(Array.from(run("state.models[0].input_modalities")), ["text", "image"]);

  getElement("modal_model_input_modalities").value = "text";
  getElement("modal_model_vision").value = "unknown";
  run("persistState = __persistStateStub");
  await run("saveManualModel()");
  run("persistState = __realPersistState");
  run("state = __savedCandidate");
  assert.deepStrictEqual(Array.from(run("state.models[0].input_modalities")), ["text"]);
  assert.strictEqual(run("state.models[0].capability_sources.input_modalities.source"), "unknown");

  const visionCalls = [];
  context.__apiStub = async (path, options) => {
    visionCalls.push([path, options]);
    if (path === "/api/models/vision-test-image") return {data_url:"data:image/png;base64,fixture"};
    if (path === "/v1/responses") return {output_text:"A gold face icon."};
    throw new Error("unexpected API " + path);
  };
  run("api = __apiStub");
  assert.strictEqual(visionCalls.length, 0, "vision must not be probed automatically");
  await run("testModelVision('provider-a/model')");
  assert.deepStrictEqual(visionCalls.map(call => call[0]), ["/api/models/vision-test-image", "/v1/responses"]);
  const probe = JSON.parse(visionCalls[1][1].body);
  assert.strictEqual(probe.model, "provider-a/model");
  assert.strictEqual(probe.input[0].content[1].type, "input_image");
  assert.strictEqual(probe.input[0].content[1].image_url, "data:image/png;base64,fixture");
  assert.strictEqual(probe.max_output_tokens, 512);
  assert.match(getElement("status").textContent, /仅为本次兼容性观察/);
  assert.strictEqual(run("state.models[0].capability_sources.input_modalities.source"), "unknown", "probe must not silently change a manual override");

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

async function accountSaveDoesNotWaitForCatalog() {
  run("state = {accounts:[{id:'account-a',name:'Original',prefix:'a',hidden_models:[]}],subscription_models:[],providers:[],models:[]}; editAccount('account-a')");
  getElement('modal_account_alias').value = 'Updated';
  let finishSave, finishCatalog;
  let catalogCalls = 0;
  const saving = new Promise(resolve => { finishSave = resolve; });
  const refreshing = new Promise(resolve => { finishCatalog = resolve; });
  context.__apiStub = async (path, options) => {
    if (path === '/api/config') { await saving; return JSON.parse(options.body); }
    if (path === '/api/catalog/refresh') { catalogCalls++; return refreshing; }
    if (path === '/api/integration') return {};
    throw new Error('unexpected API ' + path);
  };
  run('api = __apiStub');
  const submitted = getElement('modal_submit').click();
  assert.strictEqual(getElement('modal_submit').disabled, true);
  assert.strictEqual(run('state.accounts[0].name'), 'Original', 'do not announce an unsaved change');
  finishSave();
  let saved = false;
  submitted.then(() => { saved = true; });
  await new Promise(resolve => setImmediate(resolve));
  try {
    assert.strictEqual(saved, true, 'a slow catalog sync must not keep the save modal busy');
    assert.strictEqual(getElement('modal_submit').disabled, false);
    assert(getElement('modal_backdrop').classList.contains('hidden'));
    assert.strictEqual(run('state.accounts[0].name'), 'Updated');
    assert.strictEqual(catalogCalls, 1);
    assert.match(getElement('status').textContent, /账户信息已保存/);
    await run("persistState('', cloneState())");
    assert.strictEqual(catalogCalls, 1, 'background catalog writes must stay ordered');
  } finally {
    finishCatalog({});
    await submitted;
    await run('catalogSync');
  }
  assert.strictEqual(catalogCalls, 2);
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
  quotaHistoryPeriodsBehavior();
  await quotaHistorySwitchingBehavior();
  await quotaHistoryRaceBehavior();
  performanceDiagnosticsBehavior();
  await cacheUsageBehavior();
  providerDiscoveryErrorBehavior();
  quotaMeterBehavior();
  creditLayoutBehavior();
  invalidCredentialAccountBehavior();
  await quotaStateSyncBehavior();
  await quotaNotificationBehavior();
  failureDetailsBehavior();
  await nativeAccountBehavior();
  await nativeOnlyIntegrationBehavior();
  await accountEmojiBehavior();
  await quotaErrorBehavior();
  modelGroupBehavior();
  providerCardBehavior();
  officialPresetBehavior();
  capabilityMetadataBehavior();
  await presentationBehavior();
  presentationMigrationBehavior();
  modalDismissalBehavior();
  await atomicStateBehavior();
  await accountSaveDoesNotWaitForCatalog();
  await runtimeSettingsIsolationBehavior();
  await initialRenderIsolationBehavior();
  process.stdout.write("web DOM behavior: ok\n");
})().catch(error => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
