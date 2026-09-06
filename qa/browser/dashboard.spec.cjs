const { test, expect } = require('@playwright/test');
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

test('real server exchanges a session, renders the library, and revokes sign-out', async ({ page, context }, testInfo) => {
  const runtime = testInfo.outputPath('runtime');
  fs.mkdirSync(runtime, { recursive: true });
  const binary = process.env.BOOKFORGE_BIN;
  if (!binary || !path.isAbsolute(binary)) throw new Error('Run scripts/verify.sh browser to build this checkout');
  const server = spawn(binary, ['serve', '--bind', '127.0.0.1:0'], {
    cwd: runtime, env: { ...process.env, RUST_LOG: 'bookforge=debug' }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  let logs = '';
  const stopped = new Promise(resolve => server.once('close', resolve));
  let timer;
  const ready = new Promise((resolve, reject) => {
    timer = setTimeout(() => reject(new Error('Dashboard startup timed out')), 15000);
    server.once('error', reject);
    server.once('exit', code => reject(new Error(`Dashboard exited: ${code}`)));
    server.stdout.on('data', chunk => {
      logs += chunk;
      const match = logs.match(/http:\/\/127\.0\.0\.1:\d+\/\?token=[^\s]+/);
      if (match) resolve(match[0]);
    });
    server.stderr.on('data', chunk => { logs += chunk; });
  });
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    const bootstrap = await ready;
    clearTimeout(timer);
    const origin = new URL(bootstrap).origin;
    expect((await page.request.get(`${origin}/api/jobs`)).status()).toBe(401);
    await page.goto(bootstrap);
    await expect(page.getByRole('heading', { name: 'Your library' })).toBeVisible();
    await expect(page).toHaveURL(`${origin}/`);
    expect((await page.request.get(`${origin}/api/jobs`)).status()).toBe(200);
    const cookies = await context.cookies();
    expect(cookies.some(cookie => cookie.httpOnly && cookie.sameSite === 'Strict')).toBe(true);
    await page.getByRole('button', { name: '+ New translation', exact: true }).first().click();
    await expect(page.locator('#stage h2')).toBeVisible();
    await page.getByRole('button', { name: 'Sign out', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Sign out', exact: true })).toHaveCount(0);
    expect((await page.request.get(`${origin}/api/jobs`)).status()).toBe(401);
    // Replaying the pre-logout cookie must also fail: clearing it alone is insufficient.
    const staleCookie = cookies.map(cookie => `${cookie.name}=${cookie.value}`).join('; ');
    expect((await page.request.get(`${origin}/api/jobs`, { headers: { Cookie: staleCookie } })).status()).toBe(401);
    expect(errors).toEqual([]);
  } finally {
    clearTimeout(timer);
    server.kill();
    const killTimer = setTimeout(() => server.kill('SIGKILL'), 3000);
    await stopped;
    clearTimeout(killTimer);
    fs.writeFileSync(testInfo.outputPath('server.log'), logs.replace(/token=[^\s]+/g, 'token=[redacted]'));
  }
});
