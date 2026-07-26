import { test, expect } from '@playwright/test';
import fs from 'node:fs';

test('button completes without browser errors', async ({ page }) => {
  const events = [];
  page.on('console', message => {
    if (message.type() === 'error') {
      events.push({ kind: 'console_error', message: message.text() });
    }
  });
  page.on('pageerror', error => {
    events.push({ kind: 'page_exception', message: error.message });
  });
  page.on('crash', () => {
    events.push({ kind: 'page_crash', message: 'page crashed' });
  });
  page.on('requestfailed', request => {
    events.push({
      kind: 'failed_request',
      message: request.failure()?.errorText || 'request failed',
      url: request.url()
    });
  });

  await page.goto('/');
  await page.getByRole('button', { name: 'Run action' }).click();
  try {
    await expect(page.locator('#result')).toHaveText('complete');
  } finally {
    fs.mkdirSync('test-results', { recursive: true });
    fs.writeFileSync(
      'test-results/browser-events.jsonl',
      events.map(event => JSON.stringify(event)).join('\n')
    );
  }
  expect(events.filter(event => event.kind === 'console_error')).toEqual([]);
});
