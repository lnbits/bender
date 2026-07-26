import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: '.',
  testMatch: 'fixture.spec.mjs',
  outputDir: 'test-results',
  reporter: [['json', { outputFile: 'test-results/report.json' }]],
  use: {
    baseURL: process.env.BENDER_FIXTURE_URL || 'http://127.0.0.1:41739',
    launchOptions: process.env.BENDER_PLAYWRIGHT_CHROMIUM
      ? { executablePath: process.env.BENDER_PLAYWRIGHT_CHROMIUM }
      : {},
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure'
  }
});
