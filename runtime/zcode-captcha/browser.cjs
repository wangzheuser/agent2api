// 普通 Chromium + 本机 CDP；不改写 UA、指纹或验证码结果。
const { chromium } = require('playwright-core');
const { spawn } = require('node:child_process');
const { mkdtempSync, rmSync } = require('node:fs');
const { tmpdir } = require('node:os');
const { join } = require('node:path');
const { createServer } = require('node:net');

const delay = ms => new Promise(resolve => setTimeout(resolve, ms));

async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  child.kill('SIGTERM');
  for (let i = 0; i < 20 && child.exitCode === null && child.signalCode === null; i++) await delay(100);
  if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
}

class BrowserHost {
  async start() {
    if (this.closed) throw new Error('browser_closed');
    if (!this.display) {
      this.display = spawn('Xvfb', ['-displayfd', '3', '-screen', '0', '1280x720x24', '-nolisten', 'tcp'],
        { stdio: ['ignore', 'ignore', 'ignore', 'pipe'] });
      const number = await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('display_timeout')), 5000);
        const finish = (error, value) => { clearTimeout(timer); error ? reject(error) : resolve(value); };
        this.display.once('error', () => finish(new Error('display_start_failed')));
        this.display.stdio[3].once('data', data => finish(null, String(data).trim()));
        this.display.once('exit', () => finish(new Error('display_exited')));
      });
      if (!/^\d+$/.test(number)) throw new Error('display_invalid');
      this.displayName = `:${number}`;
    }
    if (this.closed) throw new Error('browser_closed');
    const reservation = createServer();
    await new Promise(resolve => reservation.listen(0, '127.0.0.1', resolve));
    const port = reservation.address().port;
    await new Promise(resolve => reservation.close(resolve));
    if (this.closed) throw new Error('browser_closed');
    this.profile = mkdtempSync(join(tmpdir(), 'agent2api-captcha-'));
    this.chrome = spawn(process.env.AGENT2API_CHROMIUM_PATH || '/usr/bin/chromium', [
      '--no-sandbox', '--disable-dev-shm-usage', '--no-first-run', '--no-default-browser-check',
      `--remote-debugging-port=${port}`, '--remote-debugging-address=127.0.0.1',
      `--user-data-dir=${this.profile}`, 'about:blank',
    ], { env: { ...process.env, DISPLAY: this.displayName }, stdio: 'ignore' });
    let failed = false;
    this.chrome.once('error', () => { failed = true; });
    for (let i = 0; i < 50; i++) {
      if (failed || this.chrome.exitCode !== null) break;
      try {
        this.browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`, { timeout: 1000 });
        return this.browser;
      } catch { await delay(100); }
    }
    throw new Error('browser_start_failed');
  }

  async close() {
    this.closed = true;
    if (this.browser) await this.browser.close().catch(() => {});
    this.browser = null;
    await stop(this.chrome); this.chrome = null;
    await stop(this.display); this.display = null;
    if (this.profile) rmSync(this.profile, { recursive: true, force: true });
    this.profile = null;
  }
}

module.exports = { BrowserHost };
