import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { test, expect } from './fixtures.mjs';
import { addAPIService, dialog, importSubscription, serviceCard, tabTo } from './ui.mjs';

test('U01 bootstrap、键盘添加服务、模型请求、上游 401 与移除', async ({ page, emp, upstream }) => {
  upstream.models = ['fixture-model'];
  await emp.login(page);
  const token = new URL(emp.bootstrapURL).searchParams.get('bootstrap');
  const replay = await page.request.post(emp.baseURL + '/api/session', { headers: { 'X-EMP-Bootstrap': token }, data: {} });
  expect(replay.status()).toBe(401);
  await page.reload();
  await expect(page.getByRole('heading', { name: 'Services', exact: true })).toBeVisible();
  await page.getByRole('button', { name: '切换为简体中文', exact: true }).click();
  await expect(page.getByRole('heading', { name: '服务', exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Switch to English', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Services', exact: true })).toBeVisible();

  await addAPIService(page, 'Fixture API', upstream.url + '/v1');
  let config = await emp.config();
  const provider = config.providers.find(provider => provider.name === 'Fixture API');
  expect(provider).toMatchObject({ protocol: 'responses', auth_mode: 'api_key', api_key_set: true });
  const card = serviceCard(page, 'Fixture API');
  await card.getByRole('button', { name: 'Manage models', exact: true }).click();
  await dialog(page).getByRole('button', { name: 'Update model list', exact: true }).click();
  await expect(dialog(page).getByText('fixture-model', { exact: true }).first()).toBeVisible();
  expect(upstream.requests.filter(request => request.method === 'GET' && request.path === '/v1/models'))
    .toEqual([expect.objectContaining({ authorization: 'Bearer fixture-upstream-key' })]);
  await dialog(page).getByRole('button', { name: 'Show model', exact: true }).click();
  await expect(dialog(page).getByRole('button', { name: 'Hide model', exact: true })).toBeVisible();
  await expect(page.getByRole('complementary')).toContainText('fixture-model');
  config = await emp.config();
  const model = config.models.find(model => model.provider === provider.id);
  expect(model).toMatchObject({ upstream_id: 'fixture-model', enabled: true });

  const upstreamGate = upstream.gate('POST', '/v1/responses');
  const responseReady = page.waitForResponse(response => new URL(response.url()).pathname === '/v1/responses');
  await dialog(page).getByRole('button', { name: 'Test model', exact: true }).click();
  expect(await upstreamGate.entered).toMatchObject({ method: 'POST', path: '/v1/responses',
    authorization: 'Bearer fixture-upstream-key', model: 'fixture-model' });
  upstreamGate.release();
  const response = await responseReady;
  expect(response.status()).toBe(200);
  expect(await response.json()).toMatchObject({ status: 'completed' });
  expect(upstream.requests.filter(request => request.method === 'POST')).toEqual([
    expect.objectContaining({ path: '/v1/responses', authorization: 'Bearer fixture-upstream-key', model: 'fixture-model' }),
  ]);

  upstream.rejectResponses = true;
  const rejected = page.waitForResponse(response => new URL(response.url()).pathname === '/v1/responses');
  await dialog(page).getByRole('button', { name: 'Test model', exact: true }).click();
  expect((await rejected).status()).toBe(401);
  await page.reload();
  await expect(card).toBeVisible();
  expect((await emp.config()).models.find(item => item.id === model.id).enabled).toBe(true);
  await card.getByRole('button', { name: 'Manage models', exact: true }).focus();
  await page.keyboard.press('Enter');
  await tabTo(page, dialog(page).getByRole('button', { name: 'Hide model', exact: true }));
  await page.keyboard.press('Enter');
  await expect(dialog(page).getByRole('button', { name: 'Show model', exact: true })).toBeVisible();
  expect((await emp.config()).models.find(item => item.id === model.id).enabled).toBe(false);
  await page.keyboard.press('Escape');
  await expect(dialog(page)).toBeHidden();
  await expect(page.getByRole('complementary')).not.toContainText('fixture-model');

  await card.getByRole('button', { name: /^Fixture API · API · Model:/ }).click();
  const confirmation = page.waitForEvent('dialog');
  const removing = dialog(page).getByRole('button', { name: 'Remove', exact: true }).click();
  const prompt = await confirmation;
  expect(prompt.message()).toContain('Fixture API');
  await prompt.accept();
  await removing;
  await expect(card).toBeHidden();
  await page.reload();
  await expect(card).toBeHidden();
  config = await emp.config();
  expect(config.providers.some(item => item.id === provider.id)).toBe(false);
  expect(config.models.some(item => item.provider === provider.id)).toBe(false);
  const stored = await emp.storedConfig();
  expect(stored.providers.some(item => item.id === provider.id)).toBe(false);
  expect(stored.models.some(item => item.provider === provider.id)).toBe(false);
});

test.describe('U01 订阅导入与偏好', () => {
  test.use({ empOptions: {
    nativeAuth: { tokens: { account_id: 'native-owner', access_token: 'synthetic-native-token' } },
    catalog: [
      { slug: 'gpt-fixture', display_name: 'Fixture GPT', context_window: 272000,
        max_context_window: 872000, visibility: 'list', supported_in_api: true, base_instructions: 'fixture' },
      { slug: 'gpt-retained', display_name: 'Retained GPT', context_window: 272000,
        visibility: 'list', supported_in_api: true, _emp_catalog_retained: true },
    ],
    config: { native_model_context_windows: { 'gpt-retained': 180000 } },
  } });

  test('U01 auth.json 文件与粘贴合并到 Other；偏好取消、同步、保存并重读', async ({ page, emp }) => {
    await emp.login(page);
    const authPath = join(emp.codexHome, 'auth.json');
    const nativeBefore = await readFile(authPath, 'utf8');
    const other = { tokens: { account_id: 'other-owner', access_token: 'synthetic-other-token' } };
    await importSubscription(page, 'File subscription', other, true);
    expect((await emp.config()).accounts).toHaveLength(1);
    await importSubscription(page, 'Pasted subscription', other);
    expect((await emp.config()).accounts).toHaveLength(1);
    expect(await readFile(authPath, 'utf8')).toBe(nativeBefore);

    const native = serviceCard(page, 'Native');
    await native.getByRole('button', { name: 'Manage models', exact: true }).click();
    const context = dialog(page).getByRole('spinbutton', { name: 'Maximum context window (tokens)', exact: true });
    await expect(context).toHaveCount(1);
    await context.fill('200000');
    await dialog(page).getByRole('button', { name: 'Sync visibility and context to other subscriptions', exact: true }).click();
    await page.keyboard.press('Escape');
    await expect(dialog(page)).toBeHidden();
    expect((await emp.config()).native_model_context_windows).toEqual({ 'gpt-retained': 180000 });
    expect((await emp.config()).accounts[0].model_context_windows || {}).not.toHaveProperty('gpt-fixture');

    await native.getByRole('button', { name: 'Manage models', exact: true }).click();
    await expect(context).toBeVisible();
    await context.fill('100000');
    await dialog(page).getByRole('button', { name: 'Sync visibility and context to other subscriptions', exact: true }).click();
    await dialog(page).getByRole('button', { name: 'Hide model', exact: true }).click();
    await dialog(page).getByRole('button', { name: 'Save changes', exact: true }).click();
    await expect(dialog(page)).toBeHidden();
    const config = await emp.config();
    expect(config.native_hidden_models).toContain('gpt-fixture');
    expect(config.native_model_context_windows).toEqual({ 'gpt-retained': 180000, 'gpt-fixture': 100000 });
    expect(config.accounts[0].hidden_models).toContain('gpt-fixture');
    expect(config.accounts[0].model_context_windows['gpt-fixture']).toBe(100000);
    expect((await emp.storedConfig()).native_model_context_windows).toEqual(config.native_model_context_windows);
    expect(await readFile(authPath, 'utf8')).toBe(nativeBefore);
    await page.reload();
    await native.getByRole('button', { name: 'Manage models', exact: true }).click();
    await expect(context).toHaveValue('100000');
    await expect(dialog(page).getByRole('button', { name: 'Show model', exact: true })).toBeVisible();
  });
});
