import { test as base, expect } from '@playwright/test';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { setTimeout as delay } from 'node:timers/promises';

function deferred() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}

async function listen(server) {
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  return server.address().port;
}

async function freePort() {
  const server = net.createServer();
  const port = await listen(server);
  await new Promise(resolve => server.close(resolve));
  if (port < 4300) throw new Error('EMP browser fixtures require an ephemeral port above 4300');
  return port;
}

async function fakeUpstream() {
  const requests = [];
  const gates = [];
  const upstream = {
    requests,
    models: ['fixture-model', 'spare-model'],
    rejectResponses: false,
    gate(method, path) {
      const entered = deferred(), released = deferred();
      const gate = { method, path, entered: entered.promise, release: released.resolve,
        started: entered.resolve, released: released.promise, used: false };
      gates.push(gate);
      return gate;
    },
  };
  const server = http.createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const text = Buffer.concat(chunks).toString();
    const body = text ? JSON.parse(text) : {};
    const path = new URL(request.url, 'http://127.0.0.1').pathname;
    const record = { number: requests.length + 1, method: request.method, path: request.url,
      authorization: request.headers.authorization || '', model: body.model ?? null, body,
      cancelled: false };
    requests.push(record);
    response.on('close', () => { if (!response.writableEnded) record.cancelled = true; });
    const gate = gates.find(item => !item.used && item.method === request.method && item.path === path);
    if (gate) {
      gate.used = true;
      gate.started(record);
      await gate.released;
      if (response.destroyed) return;
    }
    let status = 200, result;
    if (request.method === 'GET' && path.endsWith('/models')) {
      result = { object: 'list', data: upstream.models.map(id => ({ id, object: 'model', owned_by: 'fixture' })) };
    } else if (request.method === 'POST' && path.endsWith('/responses')) {
      if (upstream.rejectResponses) {
        status = 401;
        result = { error: { type: 'authentication_error', code: 'invalid_api_key', message: 'Fixture rejected key' } };
      } else {
        result = { id: 'resp-fixture', object: 'response', model: body.model, status: 'completed',
          output: [{ id: 'msg-fixture', type: 'message', role: 'assistant', status: 'completed',
            content: [{ type: 'output_text', text: 'fixture complete', annotations: [] }] }],
          usage: { input_tokens: 10, output_tokens: 2, total_tokens: 12 } };
        if (body.stream) {
          response.writeHead(200, { 'Content-Type': 'text/event-stream' });
          const events = [
            { type: 'response.created', response: { ...result, status: 'in_progress', output: [] } },
            { type: 'response.output_text.delta', item_id: 'msg-fixture', output_index: 0, content_index: 0, delta: 'fixture complete' },
            { type: 'response.completed', response: result },
          ];
          response.end(events.map(event => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`).join(''));
          return;
        }
      }
    } else {
      status = 404;
      result = { error: { message: 'Unknown fixture endpoint' } };
    }
    response.writeHead(status, { 'Content-Type': 'application/json' });
    response.end(JSON.stringify(result));
  });
  const port = await listen(server);
  upstream.url = `http://127.0.0.1:${port}`;
  upstream.stop = async () => {
    gates.forEach(gate => gate.release());
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  };
  return upstream;
}

function isolatedEnvironment(root) {
  const environment = Object.fromEntries(['PATH', 'SystemRoot', 'LANG', 'LC_ALL', 'TZ']
    .filter(key => process.env[key] !== undefined).map(key => [key, process.env[key]]));
  for (const key of ['HOME', 'USERPROFILE', 'CODEX_HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME',
    'XDG_CACHE_HOME', 'XDG_RUNTIME_DIR', 'APPDATA', 'LOCALAPPDATA', 'TMPDIR', 'TMP', 'TEMP']) {
    environment[key] = join(root, key.toLowerCase());
  }
  for (const key of ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy']) {
    environment[key] = 'http://127.0.0.1:9';
  }
  environment.NO_PROXY = environment.no_proxy = '127.0.0.1,localhost';
  environment.EMP_UPDATE_TEST_REPOSITORY_URL = 'http://127.0.0.1:9/releases';
  environment.EMP_UPDATE_TEST_API_URL = 'http://127.0.0.1:9/releases';
  return environment;
}

function signalTree(child, signal) {
  if (!child.pid) return;
  try { process.kill(-child.pid, signal); }
  catch (error) { if (error.code !== 'ESRCH') throw error; }
}

function redact(text) {
  return text.replace(/((?:bootstrap|recovery|session|token)[="':\s]+)[^&\s",}]+/gi, '$1[redacted]');
}

async function startEmp(binary, root, options) {
  const environment = isolatedEnvironment(root);
  await Promise.all(Object.entries(environment).filter(([key]) => ['HOME', 'USERPROFILE', 'CODEX_HOME',
    'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'XDG_RUNTIME_DIR', 'APPDATA',
    'LOCALAPPDATA', 'TMPDIR', 'TMP', 'TEMP'].includes(key))
    .map(([, path]) => mkdir(path, { recursive: true, mode: 0o700 })));
  if (options.nativeAuth) await writeFile(join(environment.CODEX_HOME, 'auth.json'), JSON.stringify(options.nativeAuth), { mode: 0o600 });
  const catalogPath = join(root, 'native-models.json');
  await writeFile(catalogPath, JSON.stringify({ models: options.catalog || [] }));
  const configPath = join(root, 'config.json');
  await writeFile(configPath, JSON.stringify({ auto_enable_on_start: false, language: 'en',
    native_catalog_path: catalogPath, codex_base_url: 'http://127.0.0.1:9/v1',
    ...options.config }), { mode: 0o600 });
  const port = await freePort();
  const child = spawn(binary, ['serve', '--config', configPath, '--host', '127.0.0.1', '--port', String(port)], {
    cwd: root, env: environment, detached: true, stdio: ['ignore', 'pipe', 'pipe'],
  });
  const exited = deferred();
  child.once('exit', exited.resolve);
  child.once('error', exited.resolve);
  let output = '', stopped = false;
  const app = {
    root, configPath, codexHome: environment.CODEX_HOME, baseURL: `http://127.0.0.1:${port}`,
    bootstrapURL: '', session: '',
    log: () => redact(output),
    async stop() {
      if (stopped) return;
      stopped = true;
      signalTree(child, 'SIGTERM');
      const timer = new AbortController();
      try { await Promise.race([exited.promise, delay(8000, undefined, { signal: timer.signal })]); }
      finally { timer.abort(); signalTree(child, 'SIGKILL'); }
      await exited.promise;
    },
    async request(path, body, { method = body === undefined ? 'GET' : 'POST', headers = {} } = {}) {
      return fetch(app.baseURL + path, { method, headers: { 'Content-Type': 'application/json',
        'X-EMP-Session': app.session, Origin: app.baseURL, ...headers },
      body: body === undefined ? undefined : JSON.stringify(body) });
    },
    async json(path, body) {
      const response = await app.request(path, body);
      expect(response.status, `${path}: ${await response.clone().text()}`).toBe(200);
      return response.json();
    },
    config: () => app.json('/api/config'),
    storedConfig: async () => JSON.parse(await readFile(configPath, 'utf8')),
    async authenticate() {
      const token = new URL(app.bootstrapURL).searchParams.get('bootstrap');
      const response = await app.request('/api/session', {}, { headers: { 'X-EMP-Bootstrap': token } });
      expect(response.status).toBe(200);
      app.session = (await response.json()).session;
    },
    async login(page) {
      const sessionResponse = page.waitForResponse(response => new URL(response.url()).pathname === '/api/session' && response.request().method() === 'POST');
      await page.goto(app.bootstrapURL);
      const response = await sessionResponse;
      expect(response.status()).toBe(200);
      app.session = (await response.json()).session;
      await expect(page.getByRole('button', { name: /^(切换登录方式|Change sign-in method)$/ })).toBeVisible();
      const english = page.getByRole('button', { name: 'Switch to English', exact: true });
      if (await english.isVisible()) {
        const savedLanguage = page.waitForResponse(response => new URL(response.url()).pathname === '/api/language'
          && response.request().postDataJSON()?.language === 'en');
        await english.click();
        expect((await savedLanguage).status()).toBe(200);
      }
      await expect(page.getByRole('heading', { name: 'Services', exact: true })).toBeVisible();
      await expect(page).toHaveURL(app.baseURL + '/');
    },
  };
  const ready = deferred();
  let listening = false;
  const lines = createInterface({ input: child.stdout });
  const append = chunk => { output = (output + chunk.toString()).slice(-32_000); };
  child.stdout.on('data', append);
  child.stderr.on('data', append);
  lines.on('line', line => {
    if (line === `EMP listening on ${app.baseURL}`) listening = true;
    if (line.startsWith('Open in browser: ') && listening) {
      app.bootstrapURL = line.slice('Open in browser: '.length).trim();
      ready.resolve();
    }
  });
  const startupTimer = new AbortController();
  try {
    await Promise.race([
      ready.promise,
      exited.promise.then(() => { throw new Error(`EMP exited during startup\n${app.log()}`); }),
      delay(30_000, undefined, { signal: startupTimer.signal }).then(() => { throw new Error(`EMP readiness timed out\n${app.log()}`); }),
    ]);
    return app;
  } catch (error) {
    await app.stop();
    throw error;
  } finally { startupTimer.abort(); }
}

export const test = base.extend({
  empOptions: [{}, { option: true }],
  binary: [async ({}, use) => {
    if (!process.env.EMP_BIN) throw new Error('EMP_BIN is required. First run cargo build -p emp-app, then set EMP_BIN to the absolute path of target/debug/EMP');
    const path = resolve(process.env.EMP_BIN);
    await access(path, constants.X_OK);
    if (process.platform === 'win32') throw new Error('EMP browser fixtures require POSIX process groups; run them on Linux');
    const hash = createHash('sha256');
    for await (const chunk of createReadStream(path)) hash.update(chunk);
    await use({ path, sha256: hash.digest('hex') });
  }, { scope: 'worker' }],
  upstream: async ({}, use) => {
    const upstream = await fakeUpstream();
    try { await use(upstream); }
    finally { await upstream.stop(); }
  },
  empFactory: async ({ binary, browser }, use, testInfo) => {
    const instances = [], roots = [];
    await testInfo.attach('runtime', { body: JSON.stringify({ binary: binary.path,
      sha256: binary.sha256, browser: browser.version(), node: process.version }), contentType: 'application/json' });
    try {
      await use(async (options = {}) => {
        const root = await mkdtemp(join(tmpdir(), 'emp-web-'));
        roots.push(root);
        const app = await startEmp(binary.path, root, options);
        instances.push(app);
        return app;
      });
    } finally {
      try {
        for (const app of instances.reverse()) {
          await app.stop();
          if (testInfo.status !== testInfo.expectedStatus) {
            await testInfo.attach(`emp-${instances.indexOf(app)}.log`, { body: app.log(), contentType: 'text/plain' });
          }
        }
      } finally {
        for (const root of roots) await rm(root, { recursive: true, force: true });
      }
    }
  },
  emp: async ({ empFactory, empOptions }, use) => { await use(await empFactory(empOptions)); },
  responseGates: async ({ page }, use) => {
    const gates = [];
    await use(async (path, method = 'GET') => {
      const arrived = deferred(), released = deferred(), finished = deferred(), cancelled = deferred();
      let request, used = false;
      const gate = { arrived: arrived.promise, finished: finished.promise, cancelled: cancelled.promise,
        release: released.resolve };
      gates.push(gate);
      const onFailure = failed => { if (failed === request) cancelled.resolve(failed.failure()); };
      page.on('requestfailed', onFailure);
      gate.dispose = () => page.off('requestfailed', onFailure);
      await page.route(url => url.pathname === path, async route => {
        if (used || route.request().method() !== method) return route.fallback();
        used = true;
        request = route.request();
        const response = await route.fetch();
        arrived.resolve({ request, response });
        await released.promise;
        try { await route.fulfill({ response }); }
        catch (error) {
          if (!request.failure()) throw error;
        } finally { finished.resolve(); }
      });
      return gate;
    });
    for (const gate of gates) gate.release();
    await page.unrouteAll({ behavior: 'wait' });
    gates.forEach(gate => gate.dispose());
  },
});

export { expect };
