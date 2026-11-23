import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { test, expect } from './fixtures.mjs';
import { addAPIService, dialog, field, serviceCard } from './ui.mjs';

function providers(baseURL) {
  return ['first', 'second'].map(id => ({ id, name: `${id} API`, base_url: `${baseURL}/${id}/v1`,
    protocol: 'responses', auth_mode: 'api_key', api_key: `fixture-${id}-key` }));
}

function models() {
  return ['first', 'second'].map(id => ({ id: `${id}/fixture-model`, provider: id,
    upstream_id: 'fixture-model', context_window: 64000, enabled: true }));
}

test('U02 首次设置：从未登录状态添加服务并启用 EMP 本地 Key', async ({ page, emp, upstream }) => {
  await emp.login(page);
  expect((await emp.config()).native_account.auth_mode).toBe('missing');
  await addAPIService(page, 'First API', upstream.url + '/v1');
  await serviceCard(page, 'First API').getByRole('button', { name: 'Manage models', exact: true }).click();
  await dialog(page).getByRole('button', { name: 'Add manually', exact: true }).click();
  await field(page, 'Upstream model name', '#modal_model_upstream').fill('fixture-model');
  await dialog(page).getByRole('button', { name: 'Save model', exact: true }).click();
  await expect(dialog(page).getByRole('button', { name: 'Hide model', exact: true })).toBeVisible();
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: 'Enable EMP', exact: true }).click();
  await dialog(page).getByRole('button', { name: 'Apply EMP to Codex', exact: true }).click();
  await expect(dialog(page)).toBeHidden();
  await expect(page.getByRole('button', { name: 'Restore Native', exact: true })).toBeEnabled();
  expect((await emp.json('/api/integration')).configuration.state).toBe('emp_applied');
  expect((await emp.config()).native_account.auth_mode).toBe('local_key');
  const auth = JSON.parse(await readFile(join(emp.codexHome, 'auth.json'), 'utf8'));
  expect(auth.auth_mode).toBe('apikey');
  expect(auth.OPENAI_API_KEY).toMatch(/^emp-local-/);
  await page.reload();
  await expect(page.getByRole('button', { name: 'Restore Native', exact: true })).toBeEnabled();
  await expect(page.getByRole('complementary')).toContainText('fixture-model');
});

test('U02 迁移模式返回直接父层并保留各自草稿和选择', async ({ page, empFactory, upstream }) => {
  const emp = await empFactory({ config: { providers: providers(upstream.url), models: models() } });
  await emp.login(page);
  const before = await emp.storedConfig();
  await page.getByRole('button', { name: 'Service migration', exact: true }).click();
  await dialog(page).getByRole('button', { name: 'Export', exact: true }).click();
  const second = dialog(page).getByRole('checkbox', { name: 'External API service · second API', exact: true });
  const includeContext = dialog(page).getByRole('checkbox', { name: 'Include subscription model context settings', exact: true });
  const exportPassword = field(page, 'Migration password', '#shadow_export_password');
  const confirmPassword = field(page, 'Confirm password', '#shadow_export_password_confirm');
  await second.uncheck();
  await includeContext.uncheck();
  await exportPassword.fill('fixture-migration-password');
  await confirmPassword.fill('fixture-migration-password');
  await dialog(page).getByRole('button', { name: 'Back to previous dialog', exact: true }).click();
  await expect(dialog(page).getByRole('button', { name: 'Export', exact: true })).toBeVisible();
  await expect(dialog(page).getByRole('button', { name: 'Import', exact: true })).toBeVisible();
  await dialog(page).getByRole('button', { name: 'Import', exact: true }).click();
  const file = dialog(page).locator('input[type="file"]');
  const importPassword = field(page, 'Migration password', '#shadow_import_password');
  await file.setInputFiles({
    name: 'draft.emp', mimeType: 'application/octet-stream', buffer: Buffer.from('draft-only'),
  });
  await importPassword.fill('unsent-draft-password');
  await page.keyboard.press('Escape');
  await expect(dialog(page).getByRole('button', { name: 'Export', exact: true })).toBeVisible();
  await dialog(page).getByRole('button', { name: 'Export', exact: true }).click();
  await expect(second).not.toBeChecked();
  await expect(includeContext).not.toBeChecked();
  await expect(exportPassword).toHaveValue('fixture-migration-password');
  await expect(confirmPassword).toHaveValue('fixture-migration-password');
  await page.keyboard.press('Escape');
  await dialog(page).getByRole('button', { name: 'Import', exact: true }).click();
  await expect(importPassword).toHaveValue('unsent-draft-password');
  expect(await file.evaluate(input => input.files[0].name)).toBe('draft.emp');
  await page.keyboard.press('Escape');
  await dialog(page).getByRole('button', { name: 'Close', exact: true }).click();
  await expect(dialog(page)).toBeHidden();
  expect(await emp.storedConfig()).toEqual(before);
  expect(upstream.requests.filter(request => request.method === 'POST')).toHaveLength(0);
});

test('U02 加密文件预览只导入所选服务；提交时禁用 Back 和选择', async ({ page, empFactory, upstream, responseGates }) => {
  const source = await empFactory({ config: { providers: providers(upstream.url), models: models() } });
  await source.authenticate();
  const confirmation = await source.json('/api/migration/export/confirm', {});
  const exported = await source.json('/api/migration/export', {
    confirmation: confirmation.confirmation, password: 'fixture-migration-password',
    selections: ['first', 'second'].map(id => ({ kind: 'provider', id,
      models: [{ id: `${id}/fixture-model`, selected: true }] })),
  });
  await source.stop();
  const target = await empFactory();
  await target.login(page);
  const before = await target.storedConfig();
  await page.getByRole('button', { name: 'Service migration', exact: true }).click();
  await dialog(page).getByRole('button', { name: 'Import', exact: true }).click();
  const file = dialog(page).locator('input[type="file"]');
  const password = field(page, 'Migration password', '#shadow_import_password');
  await file.setInputFiles({ name: 'fixture.emp', mimeType: 'application/octet-stream', buffer: Buffer.from(exported.bundle, 'base64') });
  await password.fill('fixture-migration-password');
  await dialog(page).getByRole('button', { name: 'Open package', exact: true }).click();
  const submit = dialog(page).getByRole('button', { name: 'Import', exact: true });
  await expect(submit).toBeEnabled();
  await expect(dialog(page).getByRole('heading', { name: 'Service list after import', exact: true })).toBeVisible();
  expect(await target.storedConfig()).toEqual(before);
  await dialog(page).getByRole('button', { name: 'Back to previous dialog', exact: true }).click();
  await expect(dialog(page).getByRole('button', { name: 'Open package', exact: true })).toBeVisible();
  await expect(password).toHaveValue('fixture-migration-password');
  expect(await file.evaluate(input => input.files[0].name)).toBe('fixture.emp');
  await dialog(page).getByRole('button', { name: 'Open package', exact: true }).click();
  await expect(submit).toBeEnabled();
  const second = dialog(page).getByRole('checkbox', { name: 'External API service · second API', exact: true });
  await second.uncheck();
  const includeContext = dialog(page).getByRole('checkbox', { name: 'Include subscription model context settings', exact: true });
  await includeContext.uncheck();
  await expect(submit).toBeEnabled();
  await expect(second).not.toBeChecked();
  await expect(dialog(page).getByRole('textbox', { name: 'Name · first API', exact: true })).toBeVisible();
  await expect(dialog(page).getByRole('textbox', { name: 'Name · second API', exact: true })).toHaveCount(0);
  expect((await target.config()).providers).toHaveLength(0);

  const gate = await responseGates('/api/migration/import', 'POST');
  await submit.click();
  const { request, response } = await gate.arrived;
  expect(response.status()).toBe(200);
  expect(request.postDataJSON()).toMatchObject({ include_context: false,
    selections: [expect.objectContaining({ kind: 'provider', id: 'first' })] });
  expect((await target.config()).providers.map(provider => provider.name)).toEqual(['first API']);
  await expect(dialog(page).getByRole('button', { name: 'Working…', exact: true })).toBeDisabled();
  await expect(second).toBeDisabled();
  await expect(includeContext).toBeDisabled();
  await expect(dialog(page).getByRole('button', { name: 'Back to previous dialog', exact: true })).toBeDisabled();
  await page.keyboard.press('Escape');
  await expect(dialog(page).getByRole('heading', { name: 'Service list after import', exact: true })).toBeVisible();
  gate.release();
  await gate.finished;
  await expect(dialog(page)).toBeHidden();
  await page.reload();
  await expect(serviceCard(page, 'first API')).toBeVisible();
  await expect(serviceCard(page, 'second API')).toHaveCount(0);
  const config = await target.config();
  const imported = config.providers.find(provider => provider.name === 'first API');
  expect(imported).toMatchObject({ protocol: 'responses', auth_mode: 'api_key', api_key_set: true });
  expect(config.models).toEqual([expect.objectContaining({ provider: imported.id, upstream_id: 'fixture-model', enabled: true })]);
  expect((await target.storedConfig()).providers.map(provider => provider.name)).toEqual(['first API']);
  expect(upstream.requests.filter(request => request.method === 'POST')).toHaveLength(0);
});
