const { chromium } = require('playwright');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { pathToFileURL } = require('node:url');

const root = path.resolve(__dirname, '../../..');
const evidence = path.join(root, '.verify/zcode-plans');
const ui = path.join(root, 'desktop-tauri/ui');
const href = name => pathToFileURL(path.join(ui, name)).href;
const styles = ['css/tokens.css', 'css/layout.css', 'css/components.css', 'css/page-accounts.css', 'islands/ui.css'];
fs.mkdirSync(evidence, { recursive: true });
const fixture = path.join(evidence, 'fixture.html');
fs.writeFileSync(fixture, `<!doctype html><html lang="zh-CN"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
${styles.map(name => `<link rel="stylesheet" href="${href(name)}">`).join('')}
</head><body><section class="page active" data-page="accounts"></section>
<section class="page" data-page="checkin" style="display:none"></section><script>
window.fixture = { calls: [], result: { plans: [] }, error: null };
window.wbApp = { getState: () => ({ accounts: { accounts: [
  { id: 'cn', name: 'CN fixture', provider: 'zcode', enabled: true },
  { id: 'intl', name: 'Intl fixture', provider: 'zcode-intl', enabled: true }
] } }), toast() {} };
window.wbZcodeClaim = {
  async preview(id) { fixture.calls.push(id); if (fixture.error) throw new Error(fixture.error); return fixture.result; },
  async claim() { throw new Error('Claim must not run during preview'); }
};
window.workbuddyDesktop = {
  async getAllBalances() { return { results: [] }; },
  async getCheckinCenter() {
    return {
      daily: { providers: [], outOfScope: [], todayDone: 0, todayEligible: 0 },
      extras: { onboarding: [], plans: [
        { id: 'cn', name: 'CN fixture', lastClaimResult: '本期无可领套餐（上游活动投放未下发领取动作）' },
        { id: 'intl', name: 'Intl fixture', claimPlans: { 'claimed-plan': Date.now() } }
      ] }, auto: { enabled: false, schedules: [] }, history: []
    };
  }
};
</script><script src="${href('islands/ui.js')}"></script></body></html>`);

async function run() {
  const browser = await chromium.launch({
    ...(process.platform === 'win32' ? { channel: 'msedge' } : {}),
    headless: true,
  });
  try {
    const page = await browser.newPage({ viewport: { width: 720, height: 600 } });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(pathToFileURL(fixture).href);
    await page.waitForFunction(() => typeof window.wbZcodePlans?.open === 'function');
    for (const id of ['cn', 'intl']) {
      await page.setViewportSize({ width: id === 'intl' ? 320 : 720, height: 600 });
      await page.evaluate(id => window.wbZcodePlans.open({ id, name: id }), id);
      const dialog = page.getByRole('dialog');
      await dialog.getByText('可领取的套餐：0', { exact: true }).waitFor();
      assert.doesNotMatch(await dialog.innerText(), /探测失败|明天可再领|次日可再领/);
      await page.waitForTimeout(350);
      assert.ok(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth + 1));
      await page.screenshot({ path: path.join(evidence, `${id}-empty.png`) });
      await dialog.getByRole('button', { name: '关闭', exact: true }).first().click();
      await dialog.waitFor({ state: 'detached' });
    }
    assert.deepEqual(await page.evaluate(() => fixture.calls), ['cn', 'intl']);
    await page.evaluate(() => {
      fixture.result = { plans: [{ planId: 'marketing-plan', name: 'Marketing offer', description: 'Current campaign' }] };
      window.wbZcodePlans.open({ id: 'cn', name: 'Current campaign' });
    });
    const dialog = page.getByRole('dialog');
    await dialog.getByText('Marketing offer', { exact: true }).waitFor();
    await dialog.getByRole('button', { name: '领取', exact: true }).waitFor();
    await dialog.getByRole('button', { name: '关闭', exact: true }).first().click();
    await dialog.waitFor({ state: 'detached' });
    await page.evaluate(() => {
      fixture.error = '套餐活动投放探测失败（HTTP 404）';
      window.wbZcodePlans.open({ id: 'intl', name: 'Failed activity query' });
    });
    await dialog.getByText('探测失败：套餐活动投放探测失败（HTTP 404）').waitFor();
    assert.doesNotMatch(await dialog.innerText(), /可领取的套餐：0|活动尚未开始/);
    await dialog.getByRole('button', { name: '关闭', exact: true }).first().click();
    await dialog.waitFor({ state: 'detached' });
    await page.evaluate(() => { document.querySelector('[data-page="checkin"]').style.display = 'block'; });
    const center = page.locator('[data-page="checkin"]');
    await center.getByText('上游活动投放未下发领取动作', { exact: false }).waitFor();
    await center.getByText('今天已领 1 份', { exact: true }).waitFor();
    assert.doesNotMatch(await center.innerText(), /每天发一份|每天可再领|每天一份|明天可再领/);
    assert.match(await center.getByText('今天未领', { exact: true }).getAttribute('title'), /当前活动投放/);
    assert.deepEqual(errors, []);
    console.log('PASS: CN/Intl marketing empty and populated states render; errors remain errors; checkin center reflects actual delivery without daily promises; no claims or page errors.');
  } finally {
    await browser.close();
  }
}

run().catch(error => { console.error(error); process.exitCode = 1; });
