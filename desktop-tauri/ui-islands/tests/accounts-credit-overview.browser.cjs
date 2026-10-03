const { chromium } = require('playwright')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { pathToFileURL } = require('node:url')

const root = path.resolve(__dirname, '../../..')
const evidence = path.join(root, '.verify/credit-overview')
fs.mkdirSync(evidence, { recursive: true })
const href = file => pathToFileURL(path.join(root, 'desktop-tauri/ui', file)).href
const styles = ['css/tokens.css', 'css/layout.css', 'css/components.css', 'css/page-accounts.css', 'css/page-accounts-providers.css', 'css/page-accounts-table.css', 'islands/ui.css']
const accounts = [
  { id: 'A', uid: 'same', name: '国内长期账号', edition: 'cn', priority: 1 },
  { id: 'B', uid: 'urgent', name: '国内临期账号', edition: 'cn', priority: 2 },
  { id: 'C', uid: 'same', name: '国际示例账号', edition: 'intl', priority: 3 },
  { id: 'E', uid: 'enterprise', name: '企业额度账号', priority: 4 },
  { id: 'F', uid: 'partial', name: '明细不完整账号', priority: 5 },
  { id: 'G', uid: 'disabled', name: '禁用待查询账号', enabled: false, priority: 6 },
  { id: 'H', uid: 'same', name: '同身份重复账号', priority: 7 },
  { id: 'other', name: '其他提供商', provider: 'catpaw', priority: 8 },
].map(a => ({ provider: 'workbuddy', enabled: true, addedAt: 1, ...a }))
const html = `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">${styles.map(file => `<link rel="stylesheet" href="${href(file)}">`).join('')}<style>body{padding:20px}.page{display:block!important}</style></head><body><section class="page active" data-page="accounts"></section><script>
window.fixture={accounts:${JSON.stringify(accounts)},calls:[],pending:[]};
window.wbApp={getState:()=>({accounts:{accounts:fixture.accounts}}),toast(){}};
window.workbuddyDesktop={getAllBalances(id){fixture.calls.push(id);return new Promise((resolve,reject)=>fixture.pending.push({id,resolve,reject}));}};
</script><script src="${href('islands/ui.js')}"></script></body></html>`
const fixtureFile = path.join(evidence, 'overview-fixture.html')
fs.writeFileSync(fixtureFile, html)
const day = 86400000
let serial = 0
function details(amount, delay = day, extra = {}) {
  const now = Date.now()
  return { version: 1, kind: 'personal', fetchedAt: now + serial++, complete: true, remaining: amount, unlimited: false, unattributedRemaining: 0, issues: [],
    segments: [{ id: 'pack', resourceId: 'pack', packageCode: 'monthly', name: '示例积分包', remaining: amount, total: 1000,
      expiresAt: now + delay, expiresAtText: null, expiryStatus: 'known', entitlementEndsAt: null, state: 'active' }], ...extra }
}
const usage = value => ({ totalLeft: value.remaining, kind: value.kind, unlimited: value.unlimited, creditDetails: value })
async function apply(page, rows) {
  await page.evaluate(rows => window.wbAccountsView.applyBalances({ results: rows.map(([id, value]) => ({ id, usage: value })) }), rows.map(([id, value]) => [id, usage(value)]))
}
async function resolvePending(page, id, value, failure = false) {
  await page.evaluate(({ id, value, failure }) => {
    const at = fixture.pending.findIndex(p => p.id === id)
    if (at < 0) throw new Error('Missing pending request ' + id)
    const pending = fixture.pending.splice(at, 1)[0]
    if (failure) pending.reject(new Error('synthetic network failure'))
    else pending.resolve({ results: [{ id, usage: value }] })
  }, { id, value: value && usage(value), failure })
}
async function run() {
  const browser = await chromium.launch({ channel: 'msedge', headless: true })
  const checks = [], errors = [], metrics = []
  try {
    const page = await browser.newPage({ viewport: { width: 1200, height: 1000 }, locale: 'zh-CN', timezoneId: 'Asia/Shanghai' })
    page.on('pageerror', error => errors.push(error.message))
    await page.goto(pathToFileURL(fixtureFile).href)
    const trigger = page.locator('#btn-credit-overview')
    await trigger.waitFor()
    const urgent = details(25, day / 2)
    urgent.segments[0].remaining = 20
    urgent.segments.push({ ...urgent.segments[0], id: 'unknown', resourceId: 'unknown', remaining: 5, expiresAt: null, expiryStatus: 'unknown' })
    urgent.unattributedRemaining = 5
    await apply(page, [['A', details(50, 40 * day)], ['B', urgent], ['C', details(30, 2 * day)], ['E', details(500, day, { kind: 'enterprise' })], ['F', details(99, day, { complete: false, issues: ['truncated'] })], ['H', details(999, day, { fetchedAt: Date.now() - 1000 })]])
    await page.locator('[data-pick="B"]').click()
    await page.locator('[data-pick="C"]').click()
    await trigger.click()
    const overview = page.locator('.credit-overview')
    const cn = overview.getByRole('region', { name: '国内版积分总览' })
    const intl = overview.getByRole('region', { name: '国际版积分总览' })
    await overview.waitFor()
    assert.equal(await cn.locator('[data-overview-total]').innerText(), '75.00')
    assert.equal(await intl.locator('[data-overview-total]').innerText(), '30.00')
    assert.match(await cn.locator('.overview-coverage').innerText(), /不完整 1.*重复身份 1.*企业 1/)
    assert.equal(await page.evaluate(() => fixture.calls.length), 0)
    checks.push('cached overview separates editions, deduplicates identity, excludes enterprise and partial totals without fetching')
    await cn.getByRole('button', { name: /24 小时内：20.00/ }).click()
    assert.equal(await cn.locator('.overview-account').count(), 1)
    assert.equal(await cn.locator('.overview-account').getAttribute('data-account-id'), 'B')
    checks.push('expiry segment filters matching accounts and retains other edition')
    const rowB = cn.locator('[data-account-id="B"]')
    await rowB.click()
    await page.getByRole('dialog', { name: /积分包明细/ }).waitFor()
    assert.equal(await page.locator('[role=dialog]').count(), 2)
    await page.keyboard.press('Escape')
    await page.getByRole('dialog', { name: '积分总览', exact: true }).waitFor()
    assert.equal(await cn.locator('.overview-account').count(), 1)
    assert.equal(await rowB.evaluate(el => el === document.activeElement), true)
    checks.push('nested details returns to the preserved bucket and restores row focus')
    await rowB.click()
    await page.getByRole('dialog', { name: /积分包明细/ }).waitFor()
    await page.mouse.click(5, 5)
    await page.getByRole('dialog', { name: '积分总览', exact: true }).waitFor()
    assert.equal(await rowB.evaluate(el => el === document.activeElement), true)
    checks.push('nested backdrop closes only details and returns focus to overview')
    await cn.getByRole('button', { name: '全部账号', exact: true }).click()
    await overview.getByRole('radiogroup', { name: '总览列表排序' }).getByRole('radio', { name: '账号顺序' }).click()
    assert.equal(await cn.locator('.overview-account').first().getAttribute('data-account-id'), 'A')
    await overview.getByRole('radiogroup', { name: '总览列表排序' }).getByRole('radio', { name: '最近到期' }).click()
    assert.equal(await cn.locator('.overview-account').first().getAttribute('data-account-id'), 'B')
    assert.deepEqual(await page.evaluate(() => fixture.accounts.map(a => a.priority)), accounts.map(a => a.priority))
    checks.push('display sorting changes only overview order and preserves routing priorities')
    await overview.getByRole('radiogroup', { name: '积分汇总范围' }).getByRole('radio', { name: /已选账号/ }).click()
    assert.equal(await cn.locator('[data-overview-total]').innerText(), '25.00')
    assert.equal(await overview.locator('.overview-account').count(), 2)
    await overview.getByRole('button', { name: '刷新当前范围', exact: true }).click()
    await page.waitForFunction(() => fixture.calls.length === 2)
    assert.deepEqual(await page.evaluate(() => fixture.calls), ['B', 'C'])
    await resolvePending(page, 'B', null, true)
    await resolvePending(page, 'C', details(29, 2 * day))
    await overview.getByRole('button', { name: '刷新当前范围', exact: true }).waitFor()
    assert.equal(await cn.locator('[data-overview-total]').innerText(), '25.00')
    assert.match(await rowB.innerText(), /刷新失败/)
    assert.equal(await intl.locator('[data-overview-total]').innerText(), '29.00')
    checks.push('selected scope refreshes only selected accounts and preserves failed snapshot with status')
    await page.keyboard.press('Escape')
    await overview.waitFor({ state: 'detached' })
    assert.equal(await trigger.evaluate(el => el === document.activeElement), true)
    await page.getByRole('radiogroup', { name: '启用状态', exact: true }).getByRole('radio', { name: /禁用/ }).click()
    await trigger.click()
    await overview.getByRole('radiogroup', { name: '积分汇总范围' }).getByRole('radio', { name: /当前筛选/ }).click()
    assert.equal(await overview.locator('.overview-account').count(), 1)
    assert.equal(await overview.locator('.overview-account').getAttribute('data-account-id'), 'G')
    assert.equal(await cn.locator('[data-overview-total]').innerText(), '未知')
    await overview.getByRole('button', { name: '刷新当前范围', exact: true }).click()
    await page.waitForFunction(() => fixture.pending.some(p => p.id === 'G'))
    await resolvePending(page, 'G', details(12, 2 * day))
    await overview.getByRole('button', { name: '刷新当前范围', exact: true }).waitFor()
    assert.equal(await cn.locator('[data-overview-total]').innerText(), '12.00')
    checks.push('filtered scope includes disabled accounts and distinguishes missing from zero')
    await overview.getByRole('radiogroup', { name: '积分汇总范围' }).getByRole('radio', { name: /已选账号/ }).click()
    assert.equal(await overview.locator('.overview-account').count(), 2)
    checks.push('selected accounts stay in scope when hidden by the account page filter')
    await overview.getByRole('radiogroup', { name: '积分汇总范围' }).getByRole('radio', { name: /^全部/ }).click()
    await page.screenshot({ path: path.join(evidence, 'overview-desktop.png') })
    assert.equal((await cn.locator('.overview-summary').evaluate(el => getComputedStyle(el).gridTemplateColumns)).split(' ').length, 3)
    for (const width of [320, 768]) {
      await page.setViewportSize({ width, height: 900 })
      const size = await overview.evaluate(el => ({ width: el.clientWidth, scroll: el.scrollWidth, bodyWidth: el.querySelector('.credits-body').clientWidth, bodyScroll: el.querySelector('.credits-body').scrollWidth }))
      metrics.push({ viewport: width, ...size })
      assert.ok(size.scroll <= size.width + 1 && size.bodyScroll <= size.bodyWidth + 1, JSON.stringify(size))
      if (width === 320) await page.screenshot({ path: path.join(evidence, 'overview-narrow.png') })
    }
    await page.evaluate(() => document.documentElement.setAttribute('data-theme', 'dark'))
    await page.screenshot({ path: path.join(evidence, 'overview-dark.png') })
    checks.push('320px and tablet layouts have no horizontal overflow; light and dark screenshots captured')
    for (let n = 0; n < 15; n++) await page.keyboard.press('Tab')
    assert.equal(await page.evaluate(() => !!document.activeElement.closest('[role=dialog]')), true)
    checks.push('overview traps keyboard focus and Escape returns to toolbar')
    await page.keyboard.press('Escape')
    await page.reload()
    await trigger.waitFor()
    await trigger.click()
    await overview.getByRole('button', { name: '刷新当前范围', exact: true }).click()
    await page.waitForFunction(() => fixture.calls.length === 3)
    await page.keyboard.press('Escape')
    await overview.waitFor({ state: 'detached' })
    await page.evaluate(() => { for (const request of fixture.pending.splice(0)) request.resolve({ results: [] }) })
    await page.waitForTimeout(100)
    assert.equal(await page.evaluate(() => fixture.calls.length), 3)
    checks.push('closing overview cancels queued accounts without interrupting current shared requests')
    await trigger.click()
    await overview.getByRole('radiogroup', { name: '积分汇总范围' }).getByRole('radio', { name: /已选账号/ }).click()
    assert.match(await overview.innerText(), /当前范围没有 WorkBuddy 账号/)
    assert.equal(await overview.getByRole('button', { name: '刷新当前范围', exact: true }).isDisabled(), true)
    checks.push('empty selected scope has a clear empty state and no refresh request')
    assert.deepEqual(errors, [])
    checks.push('no browser runtime errors')
    const result = { result: 'PASS', checks, metrics, exitStatus: 0 }
    fs.writeFileSync(path.join(evidence, 'overview-browser.json'), JSON.stringify(result, null, 2) + '\n')
    console.log(JSON.stringify(result, null, 2))
  } finally { await browser.close() }
}
run().catch(error => { console.error(error); process.exitCode = 1 })
