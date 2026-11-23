import { expect } from './fixtures.mjs';

export const dialog = page => page.getByRole('dialog', { includeHidden: false });

export function serviceCard(page, name) {
  return page.getByRole('article').filter({ has: page.getByRole('button', {
    name: new RegExp('^' + name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + ' · '),
  }) });
}

export function field(page, label, currentSelector) {
  return dialog(page).getByLabel(label, { exact: true }).or(dialog(page).locator(currentSelector)).first();
}

export async function tabTo(page, control) {
  const controls = await page.locator('button,input,select,textarea,a[href],[tabindex]').count();
  for (let step = 0; step <= controls; step++) {
    if (await control.evaluate(node => node === document.activeElement)) return;
    await page.keyboard.press('Tab');
  }
  await expect(control).toBeFocused();
}

export async function addAPIService(page, name, baseURL) {
  await page.getByRole('button', { name: 'Add service', exact: true }).focus();
  await page.keyboard.press('Enter');
  await tabTo(page, dialog(page).getByRole('button', { name: /^Custom API/ }));
  await page.keyboard.press('Enter');
  await tabTo(page, field(page, 'Name', '#modal_provider_name'));
  await page.keyboard.type(name);
  await tabTo(page, field(page, 'Base URL', '#modal_provider_base'));
  await page.keyboard.type(baseURL);
  await tabTo(page, field(page, 'Protocol', '#modal_provider_protocol'));
  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('ArrowDown');
  await expect(field(page, 'Protocol', '#modal_provider_protocol')).toHaveValue('responses');
  await tabTo(page, dialog(page).getByLabel('API Key', { exact: true }));
  await page.keyboard.type('fixture-upstream-key');
  await tabTo(page, dialog(page).getByRole('button', { name: 'Save service', exact: true }));
  await page.keyboard.press('Enter');
  await expect(dialog(page)).toBeHidden();
  await expect(serviceCard(page, name)).toBeVisible();
}

export async function importSubscription(page, name, auth, asFile = false) {
  await page.getByRole('button', { name: 'Add service', exact: true }).click();
  await dialog(page).getByRole('button', { name: /^auth\.json/ }).click();
  if (asFile) {
    const chooser = page.waitForEvent('filechooser');
    await dialog(page).getByRole('button', { name: 'Choose a file', exact: true }).click();
    await (await chooser).setFiles({ name: 'auth.json', mimeType: 'application/json', buffer: Buffer.from(JSON.stringify(auth)) });
    await expect(dialog(page).getByLabel('auth.json contents', { exact: true })).toHaveValue(/access_token/);
  } else {
    await dialog(page).getByLabel('auth.json contents', { exact: true }).fill(JSON.stringify(auth));
  }
  await dialog(page).getByLabel('Name', { exact: true }).fill(name);
  await dialog(page).getByRole('button', { name: 'Import subscription', exact: true }).click();
  await expect(dialog(page)).toBeHidden();
}
