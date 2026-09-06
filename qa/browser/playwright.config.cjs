const { defineConfig } = require('@playwright/test');
const path = require('node:path');
module.exports = defineConfig({
  testDir: '.', testMatch: '*.spec.cjs', workers: 1,
  forbidOnly: !!process.env.CI, retries: 0, timeout: 30000,
  outputDir: path.join(process.env.BOOKFORGE_QA_ARTIFACTS || '../../.qa/browser', 'results'),
  reporter: [['list']],
  use: { browserName: 'chromium', trace: 'retain-on-failure', screenshot: 'only-on-failure' },
});
