import { test, expect, expectModule } from './modules-fixture.mjs';
import { dialog, serviceCard } from './ui.mjs';

const moduleActions = (page, name) => dialog(page)
  .getByRole('button', { name: `Configure ${name}`, exact: true }).locator('..');

async function openModules(page) {
  await page.getByRole('button', { name: /^(Modules|模组)$/ }).click();
  await expect(dialog(page).getByRole('heading', { name: 'Modules', exact: true })).toBeVisible();
}

async function changeModule(page, id, name, action) {
  const saved = page.waitForResponse(response => new URL(response.url()).pathname === '/api/modules'
    && response.request().method() === 'POST' && response.request().postDataJSON()?.id === id
    && response.request().postDataJSON()?.action === action);
  const label = { install: 'Install', enable: 'Enable', disable: 'Disable', uninstall: 'Remove' }[action];
  await moduleActions(page, name).getByRole('button', { name: label, exact: true }).click();
  if (action === 'uninstall') await dialog(page).getByRole('button', { name: 'Remove', exact: true }).click();
  expect((await saved).status()).toBe(200);
  await expect(dialog(page).getByRole('heading', { name: 'Modules', exact: true })).toBeVisible();
}

function decisionOptions(upstreams) {
  return { config: {
    decision_provider: 'first',
    providers: upstreams.map((upstream, index) => ({ id: index ? 'second' : 'first',
      name: index ? 'Second API' : 'First API', base_url: upstream.url + '/v1',
      protocol: 'responses', auth_mode: 'api_key', api_key: index ? 'second-key' : 'first-key',
      decision_api: { protocol: 'openai', model: 'fixture-model' } })),
  } };
}

async function openDecision(page) {
  await moduleActions(page, 'Decision API').getByRole('button', { name: 'Configure Decision API', exact: true }).click();
  await expect(dialog(page).getByLabel('Default service', { exact: true })).toBeVisible();
}

test('U06 模组安装、启停、设置入口与卸载对应真实状态和进程', async ({ page, moduleEmpFactory, moduleRelease }) => {
  const emp = await moduleEmpFactory();
  const module = moduleRelease.modules.get('decision');
  await emp.login(page);
  await openModules(page);
  await expectModule(emp, module, { installed: false });
  await expect(moduleActions(page, 'Decision API').getByRole('button', { name: 'Configure Decision API', exact: true })).toBeDisabled();
  await changeModule(page, 'decision', 'Decision API', 'install');
  await expectModule(emp, module, { installed: true, enabled: true });
  expect(moduleRelease.downloads.filter(row => row.id === 'decision')).toHaveLength(1);
  await changeModule(page, 'decision', 'Decision API', 'disable');
  await expectModule(emp, module, { installed: true });
  await changeModule(page, 'decision', 'Decision API', 'enable');
  await expectModule(emp, module, { installed: true, enabled: true });
  await openDecision(page);
  await expectModule(emp, module, { installed: true, enabled: true });
  await dialog(page).getByRole('button', { name: 'Back to previous dialog', exact: true }).click();
  await changeModule(page, 'decision', 'Decision API', 'disable');
  await expectModule(emp, module, { installed: true });
  await expect(moduleActions(page, 'Decision API').getByRole('button', { name: 'Configure Decision API', exact: true })).toBeDisabled();
  await changeModule(page, 'decision', 'Decision API', 'uninstall');
  await expectModule(emp, module, { installed: false });
  await expect(moduleActions(page, 'Decision API').getByRole('button', { name: 'Install', exact: true })).toBeEnabled();
  await page.reload();
  await openModules(page);
  await expectModule(emp, module, { installed: false });
});

test('U06 Decision 面板保存所选服务和模型后真实请求切换上游', async ({ page, moduleEmpFactory, moduleRelease, decisionUpstreams }) => {
  const emp = await moduleEmpFactory(decisionOptions(decisionUpstreams));
  const module = moduleRelease.modules.get('decision');
  await emp.login(page);
  await openModules(page);
  await changeModule(page, 'decision', 'Decision API', 'install');
  await expectModule(emp, module, { installed: true, enabled: true });
  const { api_key: key } = await emp.json('/api/decisions/connection');
  expect(key).toEqual(expect.any(String));
  const call = async () => {
    const response = await fetch(emp.baseURL + '/v1/decisions', { method: 'POST',
      headers: { 'Content-Type': 'application/json', Authorization: 'Bearer ' + key },
      body: JSON.stringify({ state: 'U06 fixture', questions: {} }),
    });
    expect(response.status, await response.clone().text()).toBe(200);
    expect(await response.json()).toMatchObject({ status: 'completed' });
  };
  await call();
  expect(decisionUpstreams[0].requests.filter(row => row.method === 'POST')).toEqual([
    expect.objectContaining({ path: '/v1/decisions', authorization: 'Bearer first-key', model: 'fixture-model' }),
  ]);
  expect(decisionUpstreams[1].requests.filter(row => row.method === 'POST')).toEqual([]);
  await openDecision(page);
  await dialog(page).getByLabel('Default service', { exact: true }).selectOption({ label: 'Second API' });
  await expect(dialog(page).getByLabel('Decision model', { exact: true })).toBeEnabled();
  await dialog(page).getByLabel('Decision model', { exact: true }).selectOption('spare-model');
  await dialog(page).getByRole('button', { name: 'Save', exact: true }).click();
  await expect(dialog(page).getByRole('heading', { name: 'Modules', exact: true })).toBeVisible();
  for (const config of [await emp.config(), await emp.storedConfig()]) {
    expect(config.decision_provider).toBe('second');
    expect(config.providers.find(row => row.id === 'second').decision_api)
      .toEqual({ protocol: 'openai', model: 'spare-model' });
  }
  await call();
  expect(decisionUpstreams[1].requests.filter(row => row.method === 'POST')).toEqual([
    expect.objectContaining({ path: '/v1/decisions', authorization: 'Bearer second-key', model: 'spare-model' }),
  ]);
  expect(decisionUpstreams[0].requests.filter(row => row.method === 'POST')).toHaveLength(1);
  await page.reload();
  await openModules(page);
  await openDecision(page);
  await expect(dialog(page).getByLabel('Default service', { exact: true })).toHaveValue('second');
  await expect(dialog(page).getByLabel('Decision model', { exact: true })).toHaveValue('spare-model');
});

async function retireDecisionInOtherTab({ page, context, moduleEmpFactory, moduleRelease, decisionUpstreams }, action) {
  const emp = await moduleEmpFactory(decisionOptions(decisionUpstreams));
  const module = moduleRelease.modules.get('decision');
  await emp.login(page);
  await openModules(page);
  await changeModule(page, 'decision', 'Decision API', 'install');
  await expectModule(emp, module, { installed: true, enabled: true });
  await openDecision(page);
  const refresh = dialog(page).getByRole('button', { name: 'Refresh list', exact: true });
  await expect(refresh).toBeEnabled();
  const refreshed = page.waitForResponse(response => new URL(response.url()).pathname === '/api/decisions/models');
  await refresh.click();
  expect((await refreshed).status()).toBe(200);
  await expect(refresh).toBeEnabled();
  const controller = await context.newPage();
  try {
    await controller.goto(emp.baseURL);
    await openModules(controller);
    await changeModule(controller, 'decision', 'Decision API', 'disable');
    await expectModule(emp, module, { installed: true });
    if (action === 'uninstall') {
      await changeModule(controller, 'decision', 'Decision API', 'uninstall');
      await expectModule(emp, module, { installed: false });
    }
  } finally {
    await controller.close();
  }
  return { emp, refresh };
}

for (const action of ['disable', 'uninstall']) {
  const label = action === 'disable' ? '停用' : '卸载';
  test(`U06 ${label} Decision 后独立请求返回 module_disabled 且不到上游`, async ({ page, context, moduleEmpFactory, moduleRelease, decisionUpstreams }) => {
    const { emp } = await retireDecisionInOtherTab({ page, context, moduleEmpFactory, moduleRelease, decisionUpstreams }, action);
    const sent = decisionUpstreams.map(upstream => upstream.requests.length);
    const response = await emp.request('/api/decisions/models', { provider: 'first', protocol: 'openai' });
    expect(response.status).toBe(404);
    expect((await response.json()).error.code).toBe('module_disabled');
    expect(decisionUpstreams.map((upstream, index) => upstream.requests.slice(sent[index]))).toEqual([[], []]);
  });

  test.fixme(`U06 另一标签页${label}后旧 Decision 控件不产生请求（阶段 5b）`, async ({ page, context, moduleEmpFactory, moduleRelease, decisionUpstreams }) => {
    const { refresh } = await retireDecisionInOtherTab({ page, context, moduleEmpFactory, moduleRelease, decisionUpstreams }, action);
    const requests = [], errors = [];
    page.on('request', request => {
      if (new URL(request.url()).pathname.startsWith('/api/decisions/')) requests.push(request.url());
    });
    page.on('pageerror', error => errors.push(error.message));
    if (await refresh.count()) await refresh.evaluate(button => button.click());
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => resolve())));
    expect(requests).toEqual([]);
    expect(errors).toEqual([]);
  });
}

async function signInTeam(page, server, username) {
  await dialog(page).getByRole('button', { name: 'Sign in', exact: true }).click();
  await dialog(page).getByRole('button', { name: 'Set workspace manually', exact: true }).click();
  await dialog(page).getByLabel('Workspace address', { exact: true }).fill(server.url);
  await dialog(page).getByLabel('Username', { exact: true }).fill(username);
  await dialog(page).getByLabel('Password', { exact: true }).fill(server.password);
  await dialog(page).getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(dialog(page).getByRole('button', { name: 'Manage', exact: true })).toBeVisible();
}

test('U06 Team 加入、管理员审核入口与成员同步获批共享订阅', async ({ page, context, moduleEmpFactory, moduleRelease, teamServer }) => {
  const claims = { exp: Math.floor(Date.now() / 1000) + 86400, 'https://api.openai.com/auth': {
    chatgpt_account_id: 'u06-subscription', chatgpt_plan_type: 'pro',
  } };
  const token = 'e30.' + Buffer.from(JSON.stringify(claims)).toString('base64url') + '.fixture';
  const admin = await moduleEmpFactory({ catalog: teamServer.catalog, config: { codex_base_url: teamServer.url + '/v1' },
    nativeAuth: { tokens: { account_id: 'u06-subscription', access_token: token, id_token: token } } });
  const member = await moduleEmpFactory({ catalog: teamServer.catalog, config: { codex_base_url: teamServer.url + '/v1' } });
  const module = moduleRelease.modules.get('team');
  const memberPage = await context.newPage();
  try {
    for (const [emp, browserPage] of [[admin, page], [member, memberPage]]) {
      await emp.login(browserPage);
      await openModules(browserPage);
      await changeModule(browserPage, 'team', 'Team Workspace', 'install');
      await expectModule(emp, module, { installed: true, enabled: true });
      await moduleActions(browserPage, 'Team Workspace').getByRole('button', { name: 'Configure Team Workspace', exact: true }).click();
      await expect(dialog(browserPage).getByRole('heading', { name: 'Join Workspace', exact: true })).toBeVisible();
    }
    await signInTeam(page, teamServer, 'manager');
    expect((await admin.json('/api/team/status')).identity.member.role).toBe('team_admin');
    await dialog(page).getByRole('button', { name: 'Manage', exact: true }).click();
    await expect(dialog(page).getByRole('tab', { name: 'Manage subscriptions', exact: true })).toBeVisible();

    await dialog(memberPage).getByRole('button', { name: 'Set workspace manually', exact: true }).click();
    await dialog(memberPage).getByLabel('Workspace address', { exact: true }).fill(teamServer.url);
    await dialog(memberPage).getByLabel('Seven-digit join code', { exact: true }).fill(teamServer.code);
    await dialog(memberPage).getByLabel('Nickname', { exact: true }).fill('U06 member');
    await dialog(memberPage).getByLabel('Username', { exact: true }).fill('researcher');
    await dialog(memberPage).getByLabel('Choose a password', { exact: true }).fill(teamServer.password);
    await dialog(memberPage).getByRole('button', { name: 'Request to join', exact: true }).click();
    await expect(dialog(memberPage).getByRole('heading', { name: 'Waiting for approval', exact: true })).toBeVisible();
    expect((await member.json('/api/team/status')).joined).toBe(false);
    await dialog(page).getByRole('button', { name: 'Back to previous dialog', exact: true }).click();
    await dialog(page).getByRole('button', { name: 'Manage', exact: true }).click();
    const pending = dialog(page).getByRole('row').filter({ hasText: 'U06 member' });
    await pending.getByRole('button', { name: 'Approve', exact: true }).click();
    await expect(dialog(memberPage).getByRole('button', { name: 'Sync', exact: true })).toBeVisible();
    const identity = await member.json('/api/team/status');
    expect(identity).toMatchObject({ joined: true, identity: { member: { role: 'member', sync: true } } });
    await expect(dialog(memberPage).getByRole('button', { name: 'Manage', exact: true })).toHaveCount(0);
    await expect(dialog(memberPage).getByRole('tab', { name: 'Manage subscriptions', exact: true })).toHaveCount(0);
    await expect(dialog(memberPage).getByRole('button', { name: 'Approve sharing', exact: true })).toHaveCount(0);

    await dialog(page).getByRole('button', { name: 'Back to previous dialog', exact: true }).click();
    await dialog(page).getByRole('button', { name: 'Upload', exact: true }).click();
    await expect(dialog(page).getByRole('button', { name: 'Upload', exact: true })).toBeEnabled();
    const published = page.waitForResponse(response => new URL(response.url()).pathname === '/api/team/publish');
    await dialog(page).getByRole('button', { name: 'Upload', exact: true }).click();
    const publication = await (await published).json();
    expect(publication.review_required).toBe(true);
    await expect(dialog(page).getByRole('button', { name: 'Manage', exact: true })).toBeVisible();
    await dialog(memberPage).getByRole('button', { name: 'Sync', exact: true }).click();
    await expect(dialog(memberPage).getByRole('button', { name: 'Sync', exact: true })).toBeEnabled();
    expect((await member.config()).accounts).toEqual([]);
    await dialog(page).getByRole('button', { name: 'Manage', exact: true }).click();
    await dialog(page).getByRole('tab', { name: 'Manage subscriptions', exact: true }).click();
    const approved = page.waitForResponse(response => new URL(response.url()).pathname === '/api/team/review_service');
    await dialog(page).getByRole('button', { name: 'Approve sharing', exact: true }).click();
    expect((await approved).status()).toBe(200);
    await expect(dialog(page).getByRole('button', { name: 'Stop sharing', exact: true })).toBeVisible();
    const synced = memberPage.waitForResponse(response => new URL(response.url()).pathname === '/api/team/sync');
    await dialog(memberPage).getByRole('button', { name: 'Sync', exact: true }).click();
    expect((await synced).status()).toBe(200);
    const accountID = 'team-' + publication.id;
    await expect.poll(async () => (await member.config()).accounts.map(row => row.id)).toEqual([accountID]);
    const account = (await member.config()).accounts.find(row => row.id === accountID);
    expect(account.credential_set).toBe(true);
    await expect(serviceCard(memberPage, account.name)).toBeVisible();
    await memberPage.reload();
    await expect(serviceCard(memberPage, account.name)).toBeVisible();
    expect((await member.storedConfig()).accounts.map(row => row.id)).toContain(accountID);
    await expectModule(admin, module, { installed: true, enabled: true });
    await expectModule(member, module, { installed: true, enabled: true });
  } finally {
    await Promise.all([admin.stop(), member.stop()]);
    await memberPage.close();
  }
});
