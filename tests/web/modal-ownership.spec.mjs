import { test, expect } from './fixtures.mjs';
import { dialog, field, serviceCard } from './ui.mjs';

test.use({ empOptions: { config: {
  providers: ['first', 'second'].map(id => ({ id, name: `${id} API`, base_url: 'http://127.0.0.1:9/v1',
    protocol: 'responses', auth_mode: 'api_key', api_key: `fixture-${id}-key` })),
  models: ['first', 'second'].map(id => ({ id: `${id}/${id}-model`, provider: id,
    upstream_id: `${id}-model`, context_window: 64000, enabled: true })),
} } });

test('U03 关闭模型窗口取消尚未完成的读请求', async ({ page, emp, responseGates }) => {
  await emp.login(page);
  const gate = await responseGates('/api/providers/first/models');
  await serviceCard(page, 'first API').getByRole('button', { name: 'Manage models', exact: true }).click();
  await gate.arrived;
  await expect(dialog(page).getByRole('button', { name: 'Update model list', exact: true })).toBeDisabled();
  await dialog(page).getByRole('button', { name: 'Close', exact: true }).click();
  expect((await gate.cancelled).errorText).toMatch(/ABORTED|CANCELLED/i);
  gate.release();
  await expect(dialog(page)).toBeHidden();
  await serviceCard(page, 'first API').getByRole('button', { name: 'Manage models', exact: true }).click();
  await expect(dialog(page).getByText('first-model', { exact: true }).first()).toBeVisible();
  await expect(dialog(page).getByRole('button', { name: 'Update model list', exact: true })).toBeEnabled();
});

test('U03 已完成的旧模型读取交付后保留新窗口和搜索草稿', async ({ page, emp }) => {
  await emp.login(page);
  await page.evaluate(path => {
    const url = new URL(path, location.href).href;
    const readText = Response.prototype.text;
    let captured, release, delivered;
    const released = new Promise(resolve => { release = resolve; });
    window.oldModelRead = {
      captured: new Promise(resolve => { captured = resolve; }),
      delivered: new Promise(resolve => { delivered = resolve; }),
      release,
    };
    Response.prototype.text = async function () {
      if (this.url !== url) return readText.call(this);
      Response.prototype.text = readText;
      const text = await readText.call(this);
      captured({ status: this.status, text });
      await released;
      requestAnimationFrame(() => delivered());
      return text;
    };
  }, '/api/providers/first/models');
  await serviceCard(page, 'first API').getByRole('button', { name: 'Manage models', exact: true }).click();
  const oldRead = await page.evaluate(() => window.oldModelRead.captured);
  expect(oldRead.status).toBe(200);
  expect(JSON.parse(oldRead.text).models).toEqual(expect.any(Array));
  await dialog(page).getByRole('button', { name: 'Close', exact: true }).click();
  await serviceCard(page, 'second API').getByRole('button', { name: 'Manage models', exact: true }).click();
  await expect(dialog(page).getByText('second-model', { exact: true }).first()).toBeVisible();
  const search = dialog(page).getByPlaceholder('Search model name or ID', { exact: true });
  await search.fill('second');
  await page.evaluate(async () => {
    window.oldModelRead.release();
    await window.oldModelRead.delivered;
  });
  await expect(dialog(page)).toHaveAccessibleName('Manage models · second API');
  await expect(search).toHaveValue('second');
  await expect(dialog(page).getByText('second-model', { exact: true }).first()).toBeVisible();
  await expect(dialog(page).getByText('first-model', { exact: true })).toHaveCount(0);
  await expect(dialog(page).getByRole('button', { name: 'Close', exact: true })).toBeEnabled();
});

test('U03 已提交写操作在关窗后完成；结果不关闭新窗口也不丢失', async ({ page, emp, responseGates }) => {
  await emp.login(page);
  await serviceCard(page, 'first API').getByRole('button', { name: /^first API · API · Model:/ }).click();
  await dialog(page).getByRole('button', { name: 'Connection settings', exact: true }).click();
  await field(page, 'Name', '#modal_provider_name').fill('Saved API');
  const gate = await responseGates('/api/config', 'POST');
  await dialog(page).getByRole('button', { name: 'Save service', exact: true }).click();
  const { response } = await gate.arrived;
  expect(response.status()).toBe(200);
  expect((await emp.storedConfig()).providers.find(provider => provider.id === 'first').name).toBe('Saved API');
  await expect(dialog(page).getByRole('button', { name: 'Working…', exact: true })).toBeDisabled();
  await page.keyboard.press('Escape');
  await expect(dialog(page).getByRole('button', { name: 'Connection settings', exact: true })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(dialog(page)).toBeHidden();
  await page.getByRole('button', { name: 'Add service', exact: true }).click();
  await dialog(page).getByRole('button', { name: /^Custom API/ }).click();
  await field(page, 'Name', '#modal_provider_name').fill('Keep this draft');
  await field(page, 'Base URL', '#modal_provider_base').fill('http://127.0.0.1:9/unsaved');
  gate.release();
  await gate.finished;
  await expect(serviceCard(page, 'Saved API')).toBeVisible();
  await expect(field(page, 'Name', '#modal_provider_name')).toHaveValue('Keep this draft');
  await expect(field(page, 'Base URL', '#modal_provider_base')).toHaveValue('http://127.0.0.1:9/unsaved');
  await expect(dialog(page).getByRole('button', { name: 'Save service', exact: true })).toBeEnabled();
  expect((await emp.config()).providers.find(provider => provider.id === 'first').name).toBe('Saved API');
  await page.keyboard.press('Escape');
  await page.keyboard.press('Escape');
  await expect(dialog(page)).toBeHidden();
  await page.reload();
  const saved = serviceCard(page, 'Saved API');
  await expect(saved).toBeVisible();
  await saved.getByRole('button', { name: /^Saved API · API · Model:/ }).click();
  await dialog(page).getByRole('button', { name: 'Connection settings', exact: true }).click();
  await expect(field(page, 'Name', '#modal_provider_name')).toHaveValue('Saved API');
  expect((await emp.config()).providers.some(provider => provider.name === 'Keep this draft')).toBe(false);
});
