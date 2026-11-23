import { execFileSync, spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { access, mkdir, mkdtemp, readFile, rm } from 'node:fs/promises';
import { constants } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { createInterface } from 'node:readline';
import { test as base, expect, fakeUpstream, freePort, isolatedEnvironment, listen } from './fixtures.mjs';

const buildModules = 'cargo build --locked -p emp-mod-decision -p emp-mod-gateway -p emp-mod-aggregate -p emp-mod-team --bins';

function moduleProcesses(emp, id = '') {
  const prefix = join(emp.root, 'state', 'modules', id ? id + '-' : '');
  return execFileSync('ps', ['-A', '-ww', '-o', 'pid=', '-o', 'args='], { encoding: 'utf8' })
    .trim().split('\n').map(line => {
      const [, pid, command] = line.match(/^\s*(\d+)\s+(.+)$/);
      return { pid: Number(pid), command };
    }).filter(row => row.command.startsWith(prefix));
}

async function moduleStatus(emp, id) {
  const { modules } = await emp.json('/api/modules');
  const row = modules.find(row => row.id === id);
  expect(row, `Missing module ${id}`).toBeDefined();
  return row;
}

export async function expectModule(emp, module, { installed, enabled = false }) {
  await expect.poll(async () => {
    const row = await moduleStatus(emp, module.id);
    return { installed_version: row.installed_version, enabled: row.enabled, active: row.active,
      active_version: row.active_version, active_revision: row.active_revision,
      draining: row.draining, restart_required: row.restart_required, error: row.error };
  }).toEqual({ installed_version: installed ? module.version : null, enabled, active: enabled,
    active_version: enabled ? module.version : null, active_revision: enabled ? module.digest : null,
    draining: false, restart_required: false, error: null });
  const executable = join(emp.root, 'state', 'modules', `${module.id}-${module.digest}`);
  await expect.poll(() => moduleProcesses(emp, module.id).map(row => row.command))
    .toEqual(enabled ? [executable] : []);
}

async function closeServer(server) {
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
}

export const test = base.extend({
  loopbackOnly: [async ({ context }, use) => {
    await context.route(url => !['127.0.0.1', 'localhost'].includes(url.hostname)
      || url.pathname === '/api/team/discover', route => route.abort());
    await use();
  }, { auto: true }],
  moduleRelease: async ({ binary }, use, testInfo) => {
    const root = await mkdtemp(join(tmpdir(), 'emp-web-releases-'));
    const environment = isolatedEnvironment(root);
    await mkdir(environment.HOME, { mode: 0o700 });
    let server;
    try {
      const modules = new Map();
      for (const id of ['decision', 'gateway', 'aggregate', 'team']) {
        const path = join(dirname(binary.path), `empmod-${id}`);
        try { await access(path, constants.X_OK); }
        catch (error) { throw new Error(`Missing real module binary ${path}: ${error.message}; first run ${buildModules} with the same target directory and profile as EMP`); }
        const bytes = await readFile(path);
        const hello = JSON.parse(execFileSync(path, { input: '', encoding: 'utf8', env: environment,
          cwd: root, timeout: 10_000 }));
        expect(hello).toMatchObject({ module: id, protocol: 1 });
        expect(hello.module_version).toEqual(expect.any(String));
        expect(hello.host_api).toEqual(expect.any(Number));
        const platform = { linux: 'linux', darwin: 'macos' }[process.platform];
        const arch = { x64: 'x86_64', arm64: 'aarch64' }[process.arch];
        if (!platform || !arch) throw new Error(`Unsupported module fixture platform: ${process.platform}/${process.arch}`);
        modules.set(id, { id, path, bytes, version: hello.module_version, hostAPI: hello.host_api,
          digest: createHash('sha256').update(bytes).digest('hex'),
          name: `EMP-mod-${id}-${platform}-${arch}` });
      }
      const version = execFileSync(binary.path, ['--version'], { encoding: 'utf8', env: environment,
        cwd: root, timeout: 10_000 }).trim().replace(/^EMP /, '');
      const coreTag = 'v' + version;
      const entries = new Map([[coreTag, [...modules.values()]], ...[...modules.values()].map(module =>
        [`emp-mod-${module.id}-api${module.hostAPI}-v${module.version}`, [module]])]);
      const downloads = [];
      let url;
      const metadata = (tag, assets) => ({ tag_name: tag, draft: false, prerelease: tag.includes('-beta'),
        body: 'Local real module build', assets: assets.map(module => ({ name: module.name,
          size: module.bytes.length, digest: 'sha256:' + module.digest,
          browser_download_url: `${url}/releases/download/${tag}/${module.name}` })) });
      server = http.createServer((request, response) => {
        const { pathname, searchParams } = new URL(request.url, url);
        let body, binaryBody = false;
        if (pathname === '/releases') {
          const second = searchParams.get('page') === '2';
          body = [...entries].filter(([tag]) => (tag === coreTag) !== second)
            .map(([tag, assets]) => metadata(tag, assets));
          if (!second) response.setHeader('Link', `<${url}/releases?per_page=100&page=2>; rel="next"`);
        } else if (pathname.startsWith('/releases/tags/')) {
          const tag = decodeURIComponent(pathname.slice('/releases/tags/'.length));
          if (entries.has(tag)) body = metadata(tag, entries.get(tag));
        } else if (pathname.startsWith('/releases/download/')) {
          const [tag, name] = pathname.slice('/releases/download/'.length).split('/').map(decodeURIComponent);
          const module = entries.get(tag)?.find(module => module.name === name);
          if (module) { body = module.bytes; binaryBody = true; downloads.push({ id: module.id, tag, name }); }
        }
        response.writeHead(body === undefined ? 404 : 200, {
          'Content-Type': binaryBody ? 'application/octet-stream' : 'application/json',
        });
        response.end(binaryBody ? body : JSON.stringify(body ?? { error: 'Unknown release asset' }));
      });
      const port = await listen(server);
      expect(port).toBeGreaterThan(4300);
      url = `http://127.0.0.1:${port}`;
      await testInfo.attach('module-binaries', { body: JSON.stringify([...modules.values()].map(
        ({ id, path, version, digest, hostAPI }) => ({ id, path, version, digest, hostAPI }))), contentType: 'application/json' });
      await use({ url, modules, downloads });
    } finally {
      if (server?.listening) await closeServer(server);
      await rm(root, { recursive: true, force: true });
    }
  },
  moduleEmpFactory: async ({ empFactory, moduleRelease }, use) => {
    const instances = [];
    try {
      await use(async options => {
        const emp = await empFactory({ ...options, release: { repositoryURL: moduleRelease.url, apiURL: moduleRelease.url + '/releases?per_page=100' } });
        instances.push(emp);
        return emp;
      });
    } finally {
      const results = await Promise.allSettled(instances.map(async emp => {
        await emp.stop();
        for (const { pid } of moduleProcesses(emp)) {
          try { process.kill(pid, 'SIGKILL'); }
          catch (error) { if (error.code !== 'ESRCH') throw error; }
        }
        await expect.poll(() => moduleProcesses(emp)).toEqual([]);
      }));
      const failures = results.filter(result => result.status === 'rejected').map(result => result.reason);
      if (failures.length) throw new AggregateError(failures, 'Module fixture cleanup failed');
    }
  },
  decisionUpstreams: async ({ upstream }, use) => {
    const second = await fakeUpstream();
    try {
      for (const item of [upstream, second]) {
        expect(Number(new URL(item.url).port)).toBeGreaterThan(4300);
        item.modelArchitecture = { output_modalities: ['decisions'] };
      }
      await use([upstream, second]);
    }
    finally { await second.stop(); }
  },
  teamServer: async ({ moduleRelease, upstream }, use, testInfo) => {
    const root = await mkdtemp(join(tmpdir(), 'emp-web-team-'));
    const environment = isolatedEnvironment(root);
    await mkdir(environment.HOME, { mode: 0o700 });
    const port = await freePort();
    const password = 'u06-team-password';
    const child = spawn(moduleRelease.modules.get('team').path,
      ['serve', '--state', root, '--listen', `127.0.0.1:${port}`], {
        cwd: root, env: { ...environment, EMP_TEAM_SPACE_PASSWORD: password, EMP_TEAM_ADMIN: 'deployment',
          EMP_TEAM_ADMIN_PASSWORD: password, EMP_TEAM_NAME: 'U06 workspace' },
        detached: true, stdio: ['ignore', 'ignore', 'pipe'],
      });
    let log = '', relay, listening = false, startupError;
    const exited = new Promise(resolve => { child.once('exit', resolve); child.once('error', resolve); });
    child.once('error', error => { startupError = error; });
    createInterface({ input: child.stderr }).on('line', line => {
      log = (log + line + '\n').slice(-32_000);
      if (line.startsWith('EMP workspace listening on ')) listening = true;
    });
    try {
      await expect.poll(() => {
        if (startupError) throw startupError;
        expect(child.exitCode, log).toBeNull();
        return listening;
      }).toBe(true);
      const health = await fetch(`http://127.0.0.1:${port}/healthz`);
      expect(health.status).toBe(200);
      expect(await health.json()).toEqual({ ok: true, mode: 'workspace' });
      const catalog = [{ slug: 'team-fixture-model', display_name: 'U06 subscription model',
        supported_in_api: true, visibility: 'list', context_window: 256000 }];
      relay = http.createServer((request, response) => {
        if (request.method === 'GET' && new URL(request.url, 'http://127.0.0.1').pathname === '/v1/models') {
          response.writeHead(200, { 'Content-Type': 'application/json' });
          response.end(JSON.stringify({ models: catalog }));
          return;
        }
        const destination = request.url.startsWith('/v1/') ? upstream.url : `http://127.0.0.1:${port}`;
        const forwarded = http.request(destination + request.url, { method: request.method, headers: request.headers }, reply => {
          response.writeHead(reply.statusCode, reply.headers);
          reply.pipe(response);
        });
        forwarded.on('error', error => { response.writeHead(502); response.end(error.message); });
        request.pipe(forwarded);
      });
      const relayPort = await listen(relay);
      expect(relayPort).toBeGreaterThan(4300);
      const url = `http://127.0.0.1:${relayPort}`;
      const request = async (path, body, token) => {
        const response = await fetch(url + '/workspace/v1/' + path, { method: 'POST',
          headers: { 'Content-Type': 'application/json', ...(token ? { Authorization: 'Bearer ' + token } : {}) },
          body: JSON.stringify(body) });
        expect(response.status, await response.clone().text()).toBe(200);
        return response.json();
      };
      const admin = await request('login', { name: 'deployment', password, device: 'a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0' });
      await request('members', { name: 'manager', password, role: 'team_admin', publish: true, sync: true }, admin.token);
      const manager = await request('login', { name: 'manager', password, device: 'b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1' });
      const invite = await request('join-code', {}, manager.token);
      await testInfo.attach('team-server', { body: JSON.stringify({ pid: child.pid, port, relayPort }), contentType: 'application/json' });
      await use({ url, password, code: invite.code, catalog });
    } finally {
      try {
        if (relay?.listening) await closeServer(relay);
        child.kill('SIGKILL');
        await exited;
        if (testInfo.status !== testInfo.expectedStatus) {
          await testInfo.attach('team-server.log', { body: log, contentType: 'text/plain' });
        }
      } finally { await rm(root, { recursive: true, force: true }); }
    }
  },
});

export { expect };
