import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { test, expect } from './fixtures.mjs';
import { addAPIService, dialog } from './ui.mjs';

const updateButton = page => page.getByRole('button', {
  name: /^(Check updates|Update available:|检查更新|有可用更新)/,
});

async function enableEMP(page) {
  await page.getByRole('button', { name: 'Enable EMP', exact: true }).click();
  await dialog(page).getByRole('button', { name: 'Apply EMP to Codex', exact: true }).click();
  await expect(dialog(page)).toBeHidden();
  await expect(page.getByRole('button', { name: 'Restore Native', exact: true })).toBeEnabled();
}

async function quitEMP(page, emp) {
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await dialog(page).getByRole('button', { name: /^General/ }).click();
  await dialog(page).getByRole('button', { name: 'Quit EMP', exact: true }).click();
  const quit = page.waitForResponse(response => new URL(response.url()).pathname === '/api/quit'
    && response.request().method() === 'POST');
  await dialog(page).getByRole('button', { name: 'Quit EMP', exact: true }).click();
  expect((await quit).status()).toBe(200);
  expect(await emp.exited).toEqual({ code: 0, signal: null });
}

test('U05 不可达发布源检查失败后提供中英文可读提示', async ({ page, emp }) => {
  await emp.login(page);
  for (const chinese of [false, true]) {
    if (chinese) await page.getByRole('button', { name: '切换为简体中文', exact: true }).click();
    await updateButton(page).click();
    await expect(dialog(page).getByRole('status').filter({
      hasText: chinese ? /检查版本时失败/ : /Update failed while checking for a release/,
    })).toContainText(chinese ? '网络' : 'network');
    expect((await emp.json('/api/updates')).state).toBe('error');
    await expect(dialog(page).getByRole('heading', { name: chinese ? '更新内容' : 'What changed', exact: true })).toBeVisible();
    await dialog(page).getByRole('button', { name: chinese ? '关闭' : 'Close', exact: true }).click();
  }
});

test('U05 本地 Release 新版本说明在检查失败后仍可重读，不执行安装', async ({ page, empFactory, release }) => {
  const emp = await empFactory({ release });
  await emp.login(page);
  const currentVersion = (await emp.json('/api/updates')).current_version;
  const checked = page.waitForResponse(response => new URL(response.url()).pathname === '/api/updates/check'
    && response.request().method() === 'POST');
  await updateButton(page).click();
  expect((await checked).status()).toBe(202);
  await expect(dialog(page).getByText(release.notes.en, { exact: false })).toBeVisible();
  await expect.poll(() => emp.json('/api/updates')).toMatchObject({
    state: 'available', latest_version: release.version, current_version: currentVersion,
  });
  expect(release.requests).toContainEqual({ method: 'GET', path: '/api/latest' });
  await expect(dialog(page).getByRole('status')).toContainText(release.version);
  await dialog(page).getByRole('button', { name: 'Close', exact: true }).click();

  release.fail = true;
  await updateButton(page).click();
  await expect(dialog(page).getByRole('status').filter({ hasText: /Update failed while checking for a release/ }))
    .toContainText('HTTP 503');
  await expect(dialog(page).getByText(release.notes.en, { exact: false })).toBeVisible();
  await dialog(page).getByRole('button', { name: 'Close', exact: true }).click();
  await page.getByRole('button', { name: '切换为简体中文', exact: true }).click();
  await updateButton(page).click();
  await expect(dialog(page).getByText(release.notes.zh, { exact: false })).toBeVisible();
  await expect(dialog(page).getByRole('status').filter({ hasText: /检查版本时失败/ })).toContainText('HTTP 503');
  expect(await emp.json('/api/updates')).toMatchObject({
    state: 'error', latest_version: release.version, current_version: currentVersion,
  });
  expect(release.requests.every(request => ['/latest', '/api/latest'].includes(request.path))).toBe(true);
});

test('U05 恢复 Native 还原 Codex 文件；通用设置退出真实 EMP 进程', async ({ page, empFactory, upstream }) => {
  const emp = await empFactory({
    nativeAuth: { tokens: { account_id: 'native-recovery-owner', access_token: 'synthetic-native-token' } },
    nativeConfig: 'model = "gpt-fixture"\nsandbox_mode = "read-only"\n',
    config: {
      providers: [{ id: 'restore-api', name: 'Restore API', base_url: upstream.url + '/v1',
        protocol: 'responses', auth_mode: 'api_key', api_key: 'fixture-restore-key' }],
      models: [{ id: 'restore-api/fixture-model', provider: 'restore-api',
        upstream_id: 'fixture-model', enabled: true }],
    },
  });
  await emp.login(page);
  const configPath = join(emp.codexHome, 'config.toml'), authPath = join(emp.codexHome, 'auth.json');
  const originalConfig = await readFile(configPath), originalAuth = await readFile(authPath);
  await enableEMP(page);
  expect((await emp.json('/api/integration')).configuration.state).toBe('emp_applied');
  expect(await readFile(configPath)).not.toEqual(originalConfig);
  await page.getByRole('button', { name: 'Restore Native', exact: true }).click();
  const restored = page.waitForResponse(response => new URL(response.url()).pathname === '/api/integration/restore'
    && response.request().method() === 'POST');
  await dialog(page).getByRole('button', { name: 'Restore native Codex', exact: true }).click();
  const response = await restored;
  expect(response.status()).toBe(200);
  expect((await response.json()).configuration.state).toBe('native');
  await expect(dialog(page)).toBeHidden();
  expect(await readFile(configPath)).toEqual(originalConfig);
  expect(await readFile(authPath)).toEqual(originalAuth);
  expect((await emp.json('/api/integration')).configuration.state).toBe('native');
  expect((await emp.request('/healthz')).status).toBe(200);

  await enableEMP(page);
  expect(await readFile(configPath)).not.toEqual(originalConfig);
  await quitEMP(page, emp);
  expect(await readFile(configPath)).toEqual(originalConfig);
  expect(await readFile(authPath)).toEqual(originalAuth);
});

test('U05 中英文教程与演示离线可读，返回目录保留管理状态', async ({ page, emp, upstream }) => {
  await emp.login(page);
  await addAPIService(page, 'Tutorial API', upstream.url + '/v1');
  for (const chinese of [false, true]) {
    if (chinese) await page.getByRole('button', { name: '切换为简体中文', exact: true }).click();
    const before = await emp.storedConfig();
    await page.getByRole('button', { name: chinese ? '使用教程' : 'Tutorials', exact: true }).click();
    const lesson = chinese ? '添加外部服务' : 'Add an external service';
    await dialog(page).getByRole('button', { name: lesson, exact: true }).click();
    const demonstration = dialog(page).getByRole('img', { name: lesson, exact: true });
    await expect(demonstration).toBeVisible();
    await expect.poll(() => demonstration.evaluate(image => image.complete && image.naturalWidth > 0)).toBe(true);
    const popupReady = page.waitForEvent('popup');
    await dialog(page).getByRole('link', { name: chinese ? '项目说明' : 'README', exact: true }).click();
    const guide = await popupReady;
    await expect(guide).toHaveURL(emp.baseURL + `/guide/${chinese ? 'README.zh-CN.md' : 'README.md'}`);
    await expect(guide.getByRole('heading', { name: chinese ? '快速开始' : 'Quick Start', exact: true })).toBeVisible();
    await guide.getByRole('link', { name: chinese ? '查看逐步操作 GIF' : 'Watch the step-by-step GIF tutorials', exact: true }).click();
    await expect(guide).toHaveURL(emp.baseURL + '/guide/docs/tutorials.md');
    await expect(guide.getByRole('heading', { name: 'Quick Tutorials / 快速教程', exact: true })).toBeVisible();
    await expect.poll(() => guide.getByRole('img', { name: 'Change Native GPT context / 修改 Native GPT 上下文', exact: true })
      .evaluate(image => image.complete && image.naturalWidth > 0)).toBe(true);
    await guide.goBack();
    await expect(guide.getByRole('heading', { name: chinese ? '快速开始' : 'Quick Start', exact: true })).toBeVisible();
    await guide.close();
    await expect(dialog(page).getByRole('navigation', { name: chinese ? '使用教程' : 'Tutorials', exact: true })).toBeVisible();
    await expect(dialog(page).getByRole('button', { name: lesson, exact: true })).toHaveAttribute('aria-pressed', 'true');
    await dialog(page).getByRole('button', { name: chinese ? '关闭' : 'Close', exact: true }).click();
    await expect(page.getByRole('heading', { name: chinese ? '服务' : 'Services', exact: true })).toBeVisible();
    expect(await emp.storedConfig()).toEqual(before);
    expect((await emp.config()).providers.find(provider => provider.name === 'Tutorial API')).toBeTruthy();
  }
});

for (const keepBackup of [false, true]) {
  test(`U05 启动恢复页取消保留旧数据，${keepBackup ? '备份后重置' : '直接重置'}进入正式管理页`, async ({ page, empFactory }) => {
    const emp = await empFactory({ invalidMasterKey: true,
      nativeConfig: 'sandbox_mode = "read-only"\n',
      nativeAuth: { tokens: { account_id: 'preserved-owner', access_token: 'synthetic-preserved-token' } },
    });
    const configBefore = await readFile(emp.configPath);
    const keyPath = join(emp.root, 'state/master.key');
    const nativePath = join(emp.codexHome, 'config.toml'), authPath = join(emp.codexHome, 'auth.json');
    const nativeBefore = await readFile(nativePath), authBefore = await readFile(authPath);
    await page.goto(emp.recoveryURL);
    await page.getByRole('combobox', { name: /^(Language|语言)$/ }).selectOption('en');
    await expect(page.getByRole('heading', { name: 'EMP cannot use the old settings', exact: true })).toBeVisible();
    await expect(page.getByText('The encryption key for the old settings is invalid.', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: 'Clear old settings and set up again', exact: true }).click();
    await expect(dialog(page).getByRole('checkbox', { name: 'Keep a backup of old settings and data (optional)', exact: true })).not.toBeChecked();
    await dialog(page).getByRole('button', { name: 'Cancel', exact: true }).click();
    await expect(dialog(page)).toBeHidden();
    expect(await readFile(emp.configPath)).toEqual(configBefore);
    expect(await readFile(keyPath, 'utf8')).toBe('invalid-fixture-key');
    await page.getByRole('combobox', { name: 'Language', exact: true }).selectOption('zh');
    await expect(page.getByRole('heading', { name: 'EMP 无法使用旧配置', exact: true })).toBeVisible();
    await page.getByRole('button', { name: '清除旧配置并重新配置', exact: true }).click();
    if (keepBackup) await dialog(page).getByRole('checkbox', { name: '保留旧配置和数据备份（可选）', exact: true }).check();
    const resetting = page.waitForResponse(response => new URL(response.url()).pathname === '/api/recovery/reset');
    await dialog(page).getByRole('button', { name: '确认清除并重新配置', exact: true }).click();
    const response = await resetting;
    expect(response.status()).toBe(200);
    expect(await response.json()).toMatchObject({ restarting: true, cleanup_pending: false });
    await expect(page.getByRole('status')).toContainText('EMP');
    await emp.waitForManagement();
    const backups = (await readdir(emp.root)).filter(name => name.startsWith('recovery-backup-'));
    if (keepBackup) {
      expect(backups).toHaveLength(1);
      expect(await readFile(join(emp.root, backups[0], 'config.json'))).toEqual(configBefore);
      expect(await readFile(join(emp.root, backups[0], 'state/master.key'), 'utf8')).toBe('invalid-fixture-key');
    } else {
      expect(backups).toHaveLength(0);
      expect((await readdir(emp.root)).filter(name => name.startsWith('recovery-staging-'))).toHaveLength(0);
    }
    expect(await readFile(keyPath, 'utf8')).not.toBe('invalid-fixture-key');
    expect(await readFile(nativePath)).toEqual(nativeBefore);
    expect(await readFile(authPath)).toEqual(authBefore);
    await emp.login(page);
    expect((await emp.config()).providers).toHaveLength(0);
    expect((await emp.json('/api/integration')).configuration.state).toBe('native');
    await quitEMP(page, emp);
    expect(await readFile(nativePath)).toEqual(nativeBefore);
    expect(await readFile(authPath)).toEqual(authBefore);
  });
}

test('U05 启动恢复页退出真实进程并保留损坏数据', async ({ page, empFactory }) => {
  const emp = await empFactory({ invalidMasterKey: true, nativeConfig: 'sandbox_mode = "read-only"\n' });
  const before = await readFile(emp.configPath), nativePath = join(emp.codexHome, 'config.toml');
  const nativeBefore = await readFile(nativePath);
  await page.goto(emp.recoveryURL);
  await page.getByRole('combobox', { name: /^(Language|语言)$/ }).selectOption('en');
  await expect(page.getByRole('button', { name: 'Quit EMP', exact: true })).toBeEnabled();
  const quitting = page.waitForResponse(response => new URL(response.url()).pathname === '/api/recovery/exit');
  await page.getByRole('button', { name: 'Quit EMP', exact: true }).click();
  const response = await quitting;
  expect(response.status()).toBe(200);
  expect(await response.json()).toEqual({ exiting: true });
  expect(await emp.exited).toEqual({ code: 0, signal: null });
  expect(await readFile(emp.configPath)).toEqual(before);
  expect(await readFile(join(emp.root, 'state/master.key'), 'utf8')).toBe('invalid-fixture-key');
  expect(await readFile(nativePath)).toEqual(nativeBefore);
});
