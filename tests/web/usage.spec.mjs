import { test, expect } from './fixtures.mjs';
import { addAPIService, dialog, field, importSubscription } from './ui.mjs';

async function sendUsage(emp, upstream, model, session, path, omitUsage = false) {
  upstream.omitUsage = omitUsage;
  const response = await emp.request('/v1/responses', {
    model, input: 'Record browser billing usage', metadata: { session_id: session },
  });
  expect(response.status).toBe(200);
  const result = await response.json();
  expect(result.status).toBe('completed');
  if (omitUsage) expect(result).not.toHaveProperty('usage');
  else expect(result.usage).toMatchObject({ input_tokens: 10, output_tokens: 2 });
  expect(upstream.requests.at(-1)).toMatchObject({ method: 'POST', path, model: 'billing-model' });
  upstream.omitUsage = false;
}

function serviceRow(page, name) {
  return dialog(page).getByRole('row').filter({
    has: page.getByRole('cell', { name, exact: true }),
  });
}

function sessionBill(page, id) {
  return dialog(page).locator('details').filter({
    has: page.getByText(`Session ${id}`, { exact: true }),
  });
}

test('U04 真实请求的账号、服务和会话账单；未知费用、时间范围与刷新', async ({ page, empFactory, upstream }) => {
  const emp = await empFactory({
    catalog: [{ slug: 'billing-model', display_name: 'Billing model', context_window: 64000,
      visibility: 'list', supported_in_api: true }],
    config: {
      codex_base_url: upstream.url + '/v1',
      providers: ['first', 'second'].map(id => ({ id, name: `${id === 'first' ? 'First' : 'Second'} API`,
        base_url: `${upstream.url}/${id}/v1`, protocol: 'responses', auth_mode: 'api_key', api_key: `fixture-${id}-key` })),
      models: ['first', 'second'].map(id => ({ id: `${id}/billing-model`, provider: id,
        upstream_id: 'billing-model', context_window: 64000, enabled: true })),
    },
  });
  await emp.login(page);
  await importSubscription(page, 'Billing Account', {
    tokens: { account_id: 'billing-owner', access_token: 'synthetic-billing-token' },
  });
  const account = (await emp.config()).accounts.find(account => account.name === 'Billing Account');
  expect(account).toBeTruthy();
  await sendUsage(emp, upstream, `${account.id}/billing-model`, 'bill-account', '/v1/responses');
  await sendUsage(emp, upstream, 'first/billing-model', 'bill-first', '/first/v1/responses');
  await sendUsage(emp, upstream, 'first/billing-model', 'bill-first', '/first/v1/responses');
  await sendUsage(emp, upstream, 'second/billing-model', 'bill-missing', '/second/v1/responses', true);
  expect(upstream.requests.filter(request => request.method === 'POST')).toEqual([
    expect.objectContaining({ authorization: 'Bearer synthetic-billing-token' }),
    expect.objectContaining({ authorization: 'Bearer fixture-first-key' }),
    expect.objectContaining({ authorization: 'Bearer fixture-first-key' }),
    expect.objectContaining({ authorization: 'Bearer fixture-second-key' }),
  ]);
  await expect.poll(async () => (await emp.json('/api/usage?series=true&start=0&end=4102444800')).totals)
    .toMatchObject({ requests: 4, reported_requests: 3, input_tokens: 30, output_tokens: 6 });

  await page.getByRole('button', { name: /^Billing Account ·/ }).click();
  await expect(dialog(page).getByRole('heading', { name: 'Lifetime usage', exact: true })).toBeVisible();
  await expect(serviceRow(page, 'billing-model').getByRole('cell').nth(1)).toHaveText('10');
  await expect(serviceRow(page, 'billing-model').getByRole('cell').nth(2)).toHaveText('2');
  await dialog(page).getByRole('button', { name: 'Close', exact: true }).click();

  await page.getByRole('button', { name: 'Statistics', exact: true }).click();
  await dialog(page).getByRole('group', { name: 'View', exact: true })
    .getByRole('button', { name: 'Services', exact: true }).click();
  await expect(serviceRow(page, 'Billing Account').getByRole('cell').nth(1)).toHaveText('1');
  await expect(serviceRow(page, 'First API').getByRole('cell').nth(1)).toHaveText('2');
  await expect(serviceRow(page, 'First API').getByRole('cell').nth(2)).toHaveText('20');
  await expect(serviceRow(page, 'First API').getByRole('cell').nth(3)).toHaveText('4');
  const unknown = serviceRow(page, 'Second API').getByRole('cell');
  await expect(unknown.nth(1)).toHaveText('1');
  await expect(unknown.nth(2)).toHaveText('—');
  await expect(unknown.nth(3)).toHaveText('—');
  await expect(unknown.nth(5)).toHaveText('—');

  const localDate = value => new Date(value - new Date(value).getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
  await dialog(page).getByLabel('From', { exact: true }).fill(localDate(Date.now() - 2 * 86_400_000));
  await dialog(page).getByLabel('Until', { exact: true }).fill(localDate(Date.now() - 86_400_000));
  await dialog(page).getByRole('button', { name: 'Apply', exact: true }).click();
  await expect(dialog(page).getByText('No usage in this period', { exact: true })).toBeVisible();
  await dialog(page).getByRole('button', { name: 'Last 7 days', exact: true }).click();
  await expect(serviceRow(page, 'First API').getByRole('cell').nth(1)).toHaveText('2');
  await sendUsage(emp, upstream, 'first/billing-model', 'bill-new', '/first/v1/responses');
  const refreshed = page.waitForResponse(response => {
    const url = new URL(response.url());
    return url.pathname === '/api/usage' && url.searchParams.get('series') === 'true';
  });
  await dialog(page).getByRole('button', { name: 'Refresh', exact: true }).click();
  expect((await refreshed).status()).toBe(200);
  await expect(serviceRow(page, 'First API').getByRole('cell').nth(1)).toHaveText('3');
  await expect(serviceRow(page, 'First API').getByRole('cell').nth(2)).toHaveText('30');

  await dialog(page).getByRole('combobox', { name: 'Service', exact: true }).selectOption({ label: 'Second API' });
  await expect(serviceRow(page, 'First API')).toHaveCount(0);
  await expect(unknown.nth(5)).toHaveText('—');
  await dialog(page).getByRole('button', { name: 'Session bills', exact: true }).click();
  const bill = sessionBill(page, 'bill-missing');
  await expect(bill.locator('summary')).toContainText('Records: 1');
  await expect(dialog(page).getByText('Session bill-first', { exact: true })).toHaveCount(0);
  await bill.locator('summary').click();
  await expect(bill.getByRole('code').filter({ hasText: 'bill-missing' })).toBeVisible();
  const missingModel = bill.getByRole('row').filter({ has: page.getByRole('cell', { name: 'billing-model', exact: true }) });
  await expect(missingModel.getByRole('cell').nth(1)).toHaveText('—');
  await expect(missingModel.getByRole('cell').last()).toHaveText('—');
  const missingReport = await emp.json('/api/usage?series=true&start=0&end=4102444800&provider=second&session=bill-missing');
  expect(missingReport.totals).toMatchObject({ requests: 1, reported_requests: 0, priced_requests: 0, unpriced_requests: 1 });

  await dialog(page).getByRole('combobox', { name: 'Service', exact: true }).selectOption({ label: 'Billing Account' });
  await expect(dialog(page).getByText('Session bill-account', { exact: true })).toBeVisible();
  await expect(sessionBill(page, 'bill-account').locator('summary')).toContainText('Records: 1');
  await expect(dialog(page).getByText('Session bill-missing', { exact: true })).toHaveCount(0);
  await dialog(page).getByRole('combobox', { name: 'Service', exact: true }).selectOption({ label: 'First API' });
  await expect(dialog(page).getByText('Session bill-first', { exact: true })).toBeVisible();
  await expect(dialog(page).getByText('Session bill-new', { exact: true })).toBeVisible();
  await expect(sessionBill(page, 'bill-first').locator('summary')).toContainText('Records: 2');
  await expect(sessionBill(page, 'bill-new').locator('summary')).toContainText('Records: 1');
  await sessionBill(page, 'bill-first').locator('summary').click();
  await expect(serviceRow(page, 'billing-model').getByRole('cell').nth(1)).toHaveText('20');
  await expect(serviceRow(page, 'billing-model').getByRole('cell').nth(2)).toHaveText('4');
  await dialog(page).getByLabel('Search session name or ID', { exact: true }).fill('bill-new');
  await expect(dialog(page).getByText('Session bill-first', { exact: true })).toHaveCount(0);
  await expect(dialog(page).getByText('Session bill-new', { exact: true })).toBeVisible();
});

test('U04 通知未读、历史重读与清空来自真实保存操作', async ({ page, emp, upstream }) => {
  await emp.login(page);
  await addAPIService(page, 'Notification API', upstream.url + '/v1');
  await expect(page.getByRole('button', { name: 'Notification inbox, 1 unread', exact: true })).toBeVisible();
  await page.reload();
  await page.getByRole('button', { name: 'Notification inbox, 1 unread', exact: true }).click();
  const inbox = page.getByRole('dialog', { name: 'Notifications', exact: true });
  await expect(inbox.getByRole('listitem')).toHaveCount(1);
  await expect(inbox.getByText('Service saved', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Notification inbox', exact: true })).toBeVisible();
  await inbox.getByRole('button', { name: 'Close notification inbox', exact: true }).click();
  await page.reload();
  await expect(page.getByRole('button', { name: 'Notification inbox', exact: true })).toBeVisible();
  await addAPIService(page, 'Another notification API', upstream.url + '/v1');
  await page.getByRole('button', { name: 'Notification inbox, 1 unread', exact: true }).click();
  await expect(inbox.getByRole('listitem')).toHaveCount(2);
  await inbox.getByRole('button', { name: 'Clear notification history', exact: true }).click();
  await expect(inbox.getByText('No notifications yet', { exact: true })).toBeVisible();
  await expect(inbox.getByRole('listitem')).toHaveCount(0);
  await expect(inbox.getByRole('button', { name: 'Clear notification history', exact: true })).toBeDisabled();
  await inbox.getByRole('button', { name: 'Close notification inbox', exact: true }).click();
  await page.reload();
  await page.getByRole('button', { name: 'Notification inbox', exact: true }).click();
  await expect(inbox.getByText('No notifications yet', { exact: true })).toBeVisible();
  expect((await emp.config()).providers.map(provider => provider.name)).toEqual(['Notification API', 'Another notification API']);
  expect((await emp.storedConfig()).providers).toHaveLength(2);
});

test('U04 在编辑中关闭通知信箱保留草稿', async ({ page, emp, upstream }) => {
  test.fail(true, '已知产品问题：弹窗遮罩挡住通知信箱按钮，见 feature-map.md');
  await emp.login(page);
  await addAPIService(page, 'Draft notification API', upstream.url + '/v1');
  await page.getByRole('button', { name: 'Service migration', exact: true }).click();
  await dialog(page).getByRole('button', { name: 'Import', exact: true }).click();
  const password = field(page, 'Migration password', '#shadow_import_password');
  await password.fill('unfinished-notification-draft');
  await page.getByRole('button', { name: /^Notification inbox/ }).click();
  const inbox = page.getByRole('dialog', { name: 'Notifications', exact: true });
  await expect(inbox.getByText('Service saved', { exact: true })).toBeVisible();
  await inbox.getByRole('button', { name: 'Close notification inbox', exact: true }).click();
  await expect(password).toHaveValue('unfinished-notification-draft');
  await page.getByRole('button', { name: /^Notification inbox/ }).click();
  await page.keyboard.press('Escape');
  await expect(inbox).toBeHidden();
  await expect(password).toHaveValue('unfinished-notification-draft');
});
