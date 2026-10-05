const { chromium } = require('playwright')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { pathToFileURL } = require('node:url')

const root = path.resolve(__dirname, '../../..')
const evidence = path.join(root, '.verify/workbuddy-growth')
fs.mkdirSync(evidence, { recursive: true })
const href = file => pathToFileURL(path.join(root, 'desktop-tauri/ui', file)).href
const accounts = Array.from({ length: 7 }, (_, i) => ({ id: `A${i}`, uid: `demo-${i}`, name: `国内示例账号 ${i + 1}`, provider: 'workbuddy', edition: 'cn', enabled: true, addedAt: 1, priority: i }))
accounts.push({ ...accounts[0], id: 'intl', edition: 'intl', name: '国际示例账号' }, { ...accounts[0], id: 'duplicate', name: '重复身份示例' }, { ...accounts[0], id: 'enterprise', enterpriseId: 'demo-tenant', name: '企业示例' })
const rewards = { credits: 0, energy: 0, buddy: null, lotteryChances: null, makeupCards: null }
const state = {
  schemaVersion: 1, id: 'A0', identity: 'identity-A0', fetchedAt: Date.now(), supported: true, reason: null, edition: 'cn', capabilities: ['tasks', 'travel', 'streak', 'lottery'],
  tasks: [{ code: 'zero', title: '零积分示例任务', source: 'growth', state: 'claimable', current: 1, target: 1, accepted: true, claimed: false, expiresAt: null, rewards, canAccept: false, canClaim: true, canExecute: false, execution: 'none', reason: null, actionUrl: null },
    { code: 'manual', title: '官方操作示例任务', source: 'mini_program', state: 'active', current: 0, target: 1, accepted: true, claimed: false, expiresAt: null, rewards: { ...rewards, credits: null }, canAccept: false, canClaim: false, canExecute: false, execution: 'manual', reason: '请在官方客户端完成真实操作', actionUrl: null }],
  travel: { state: 'no_buddy', recordId: null, buddyId: null, arrivesAt: null, completedToday: 0, dailyLimit: 1, canAdopt: true, agreementRequired: true, canDepart: false, canClaim: false, locations: [], rewards },
  streak: { days: 7, makeupCards: 1, missedDates: ['2026-10-01'], timezone: 'Asia/Shanghai', tiers: [{ id: '7d', requiredDays: 7, claimable: true, claimed: false, rewards: { ...rewards, energy: 2 } }] },
  lottery: { chances: 1, canDraw: true }, activities: [{ code: 'gift', title: '新手礼包', state: 'unknown', canClaim: false, canAttempt: true, reason: '资格由上游确认，仅手动尝试' }], errors: [], running: false, lastRun: null,
}
const policy = { schemaVersion: 1, id: 'A0', identity: 'identity-A0', autoGrowth: false, autoTravel: false, creditFloor: null, selection: 'priority', selectionScope: 'provider', balanceMaxAgeSeconds: 900, cost: { lastCredits: 0, lastAt: Date.now(), samples: 1, models: [] } }
const styles = ['css/tokens.css', 'css/layout.css', 'css/components.css', 'css/page-accounts.css', 'css/page-accounts-providers.css', 'css/page-accounts-table.css', 'islands/ui.css']
const html = `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">${styles.map(file => `<link rel="stylesheet" href="${href(file)}">`).join('')}<style>body{padding:20px}.page{display:block!important}</style></head><body><section class="page active" data-page="accounts"></section><script>
window.fixture={accounts:${JSON.stringify(accounts)},base:${JSON.stringify(state)},policy:${JSON.stringify(policy)},growthCalls:[],actions:[],pending:[],defer:false,active:0,maximum:0,policyWrites:[]};
fixture.growth=id=>({...structuredClone(fixture.base),id,identity:'identity-'+id});
fixture.usage=id=>({id,usage:{kind:'personal',totalLeft:100,creditDetails:{version:1,kind:'personal',fetchedAt:Date.now(),complete:true,unlimited:false,remaining:100,unattributedRemaining:0,issues:[],segments:[{id:'pack',resourceId:'pack',packageCode:'monthly',name:'示例积分包',remaining:100,total:100,expiresAt:Date.now()+86400000,expiresAtText:null,expiryStatus:'known',entitlementEndsAt:null,state:'active'}]}}});
window.wbApp={getState:()=>({accounts:{accounts:fixture.accounts}}),toast(){}};
window.workbuddyDesktop={
 getAllBalances(id){return Promise.resolve({results:[fixture.usage(id)]})},
 getWorkBuddyGrowth(id){fixture.growthCalls.push(id);if(!fixture.defer)return Promise.resolve(fixture.growth(id));fixture.active++;fixture.maximum=Math.max(fixture.maximum,fixture.active);return new Promise(resolve=>fixture.pending.push({id,resolve:()=>{fixture.active--;resolve(fixture.growth(id))}}))},
 workBuddyGrowthAction(payload){fixture.actions.push(payload);fixture.base.pending=null;return Promise.resolve({id:payload.id,action:payload.action,at:Date.now(),status:fixture.actionStatus||'failed',message:fixture.actionStatus==='pending'?'真实动作已完成，待官方计入':'示例业务拒绝，未发奖',rewards:${JSON.stringify(rewards)},balanceBefore:100,balanceAfter:100,balanceDelta:0,receiptConfirmed:false,stateConfirmed:true,items:[],state:fixture.growth(payload.id)})},
 getWorkBuddyPolicy(id){return Promise.resolve({...fixture.policy,id})},updateWorkBuddyPolicy(id,patch){fixture.policyWrites.push(patch);fixture.policy={...fixture.policy,...patch};return Promise.resolve({...fixture.policy,id})}
};
</script><script src="${href('islands/ui.js')}"></script></body></html>`
const fixtureFile = path.join(evidence, 'growth-browser-fixture.html')
fs.writeFileSync(fixtureFile, html)

async function run() {
  const browser = await chromium.launch({ channel: 'msedge', headless: true })
  const checks = [], errors = []
  try {
    const page = await browser.newPage({ viewport: { width: 1120, height: 980 }, locale: 'zh-CN', timezoneId: 'Asia/Shanghai' })
    page.on('pageerror', error => errors.push(error.message))
    await page.goto(pathToFileURL(fixtureFile).href)
    await page.evaluate(() => Object.defineProperty(crypto, 'randomUUID', { value: undefined }))
    await page.locator('.credit-balance-trigger').first().waitFor()
    await page.evaluate(() => wbAccountsView.applyBalances({ results: fixture.accounts.map(account => fixture.usage(account.id)) }))
    await page.locator('.credit-balance-trigger').first().dblclick()
    let dialog = page.getByRole('dialog')
    await dialog.waitFor()
    assert.equal(await page.evaluate(() => fixture.growthCalls.length), 0)
    assert.equal(await dialog.getByRole('radio', { name: '积分包', exact: true }).getAttribute('aria-checked'), 'true')
    checks.push('balance double-click keeps credit packages default without growth fetch')
    await dialog.getByRole('radio', { name: '成长福利', exact: true }).click()
    await dialog.getByRole('region', { name: '成长任务' }).waitFor()
    assert.equal(await page.evaluate(() => fixture.growthCalls.length), 1)
    assert.match(await dialog.innerText(), /积分 0 · 能量 0/)
    assert.match(await dialog.innerText(), /积分 未知/)
    assert.equal(await dialog.getByRole('button', { name: '执行可用任务', exact: true }).isDisabled(), true)
    assert.equal(await dialog.getByRole('button', { name: '领养猫猫', exact: true }).isDisabled(), true)
    await dialog.getByRole('checkbox', { name: '我已阅读并同意官方领养协议' }).check()
    assert.equal(await dialog.getByRole('button', { name: '领养猫猫', exact: true }).isEnabled(), true)
    assert.equal(await page.evaluate(() => fixture.actions.length), 0)
    await dialog.getByRole('button', { name: '领取已完成奖励', exact: true }).click()
    await dialog.locator('.growth-result[data-status="failed"]').waitFor()
    const firstAction = await page.evaluate(() => fixture.actions[0])
    assert.equal(firstAction.id, 'A0'); assert.equal(firstAction.action, 'claim_available')
    assert.equal(firstAction.expectedIdentity, 'identity-A0')
    assert.match(firstAction.clientToken, /^[0-9a-f]{32}$/)
    assert.equal(firstAction.agreementAccepted, undefined)
    checks.push('action idempotency token is generated when crypto.randomUUID is unavailable')
    assert.match(await dialog.locator('.growth-result').innerText(), /失败.*示例业务拒绝/s)
    assert.equal(await dialog.getByRole('checkbox', { name: '我已阅读并同意官方领养协议' }).isChecked(), false)
    checks.push('lazy growth query preserves zero/unknown; claim failure stays failed; adoption and real execution remain separate')
    for (const [status, action, confirmed, expected] of [
      ['not_applicable', 'travel_cycle', false, '未执行，无需确认'],
      ['completed', 'claim_available', false, '未执行，无需确认'],
      ['uncertain', 'travel_cycle', false, '回执待确认 · 状态待核实'],
      ['claimed', 'travel_cycle', true, '回执已确认 · 状态已读回'],
    ]) {
      await page.evaluate(({ status, action, confirmed }) => {
        fixture.base.lastRun = { id: 'A0', action, at: Date.now(), status, message: '示例操作结果', rewards: { credits: null }, receiptConfirmed: confirmed, stateConfirmed: confirmed, items: [], balanceBefore: 100, balanceAfter: 100, balanceDelta: 0 }
      }, { status, action, confirmed })
      await dialog.getByRole('button', { name: '刷新福利状态', exact: true }).click()
      await dialog.locator(`.growth-result[data-status="${status}"]`).waitFor()
      const text = await dialog.locator('.growth-result').innerText()
      assert.ok(text.includes(expected), text)
      if (expected === '未执行，无需确认') assert.doesNotMatch(text, /回执待确认|状态待核实|积分 未知/)
    }
    await page.evaluate(() => { fixture.base.lastRun = null })
    checks.push('skipped travel and empty claims require no receipt; uncertain and confirmed actions keep their receipt status')
    await dialog.locator('summary', { hasText: '抽奖与其他活动' }).click()
    assert.equal(await dialog.getByRole('button', { name: '消耗 1 次机会抽奖', exact: true }).isEnabled(), true)
    assert.equal(await dialog.getByRole('button', { name: '检查并尝试领取', exact: true }).isEnabled(), true)
    assert.equal(await dialog.getByRole('button', { name: '消耗 1 张卡补签', exact: true }).isDisabled(), true)
    await dialog.locator('summary', { hasText: '积分保底、自动福利与选号策略' }).click()
    await dialog.getByRole('button', { name: '保存设置', exact: true }).waitFor()
    assert.equal(await dialog.getByRole('checkbox', { name: '自动领取已完成奖励', exact: true }).isChecked(), false)
    await dialog.getByRole('spinbutton', { name: '账号保底积分' }).fill('0')
    await dialog.getByRole('button', { name: '保存设置', exact: true }).click()
    await page.waitForFunction(() => fixture.policyWrites.length === 1)
    assert.equal(await page.evaluate(() => fixture.policyWrites[0].creditFloor), 0)
    assert.equal(await page.evaluate(() => fixture.policyWrites[0].expectedIdentity), 'identity-A0')
    checks.push('resource-consuming actions use separate buttons and policy preserves disabled defaults and explicit zero floor')
    await dialog.screenshot({ path: path.join(evidence, 'growth-ui-settings.png') })
    await page.setViewportSize({ width: 320, height: 800 })
    const dimensions = await dialog.evaluate(el => ({ width: el.clientWidth, scroll: el.scrollWidth, body: el.querySelector('.credits-body').clientWidth, bodyScroll: el.querySelector('.credits-body').scrollWidth }))
    assert.ok(dimensions.scroll <= dimensions.width + 1 && dimensions.bodyScroll <= dimensions.body + 1, JSON.stringify(dimensions))
    await dialog.locator('.credits-body').evaluate(element => { element.scrollTop = 0 })
    await dialog.screenshot({ path: path.join(evidence, 'growth-ui-narrow.png') })
    checks.push('320px growth dialog has no horizontal overflow')
    await page.setViewportSize({ width: 1120, height: 980 })
    await dialog.screenshot({ path: path.join(evidence, 'growth-ui-desktop.png') })
    await dialog.getByRole('button', { name: '关闭', exact: true }).first().click()
    await page.evaluate(() => { fixture.base.pending = { clientToken: 'persistent-operation-token', request: { id: 'A0', action: 'makeup', expectedIdentity: 'identity-A0', date: '2026-10-01', clientToken: 'persistent-operation-token' } } })
    await page.locator('.credit-balance-trigger').first().dblclick()
    dialog = page.getByRole('dialog')
    await dialog.getByRole('radio', { name: '成长福利', exact: true }).click()
    await dialog.getByRole('button', { name: '继续核实上次操作', exact: true }).waitFor()
    assert.equal(await dialog.getByRole('button', { name: '领取已完成奖励', exact: true }).isDisabled(), true)
    await dialog.getByRole('button', { name: '继续核实上次操作', exact: true }).click()
    await page.waitForFunction(() => fixture.actions.length === 2)
    assert.deepEqual(await page.evaluate(() => fixture.actions[1]), { id: 'A0', action: 'makeup', expectedIdentity: 'identity-A0', date: '2026-10-01', clientToken: 'persistent-operation-token' })
    checks.push('reopened pending action blocks new work and preserves original account, parameters and clientToken')
    await dialog.getByRole('button', { name: '关闭', exact: true }).first().click()
    await page.evaluate(() => { Object.assign(fixture.base.tasks[1], { source: 'growth', canExecute: true, execution: 'automatic', current: 0, target: 3 }); fixture.actionStatus = 'pending' })
    await page.locator('.credit-balance-trigger').first().dblclick()
    dialog = page.getByRole('dialog')
    await dialog.getByRole('radio', { name: '成长福利', exact: true }).click()
    await dialog.getByRole('button', { name: '执行任务', exact: true }).click()
    await dialog.locator('.growth-result[data-status="pending"]').waitFor()
    await dialog.getByRole('button', { name: '刷新福利状态', exact: true }).click()
    await dialog.getByRole('button', { name: '执行任务', exact: true }).click()
    await dialog.locator('.growth-result[data-status="pending"]').waitFor()
    assert.equal(await page.evaluate(() => fixture.actions[2].clientToken === fixture.actions[3].clientToken), true)
    await page.evaluate(() => { fixture.base.tasks[1].current = 1 })
    await dialog.getByRole('button', { name: '刷新福利状态', exact: true }).click()
    assert.equal(await page.evaluate(() => fixture.actions.length), 4)
    await dialog.getByRole('button', { name: '执行任务', exact: true }).click()
    await dialog.locator('.growth-result[data-status="pending"]').waitFor()
    assert.equal(await page.evaluate(() => fixture.actions[3].clientToken !== fixture.actions[4].clientToken), true)
    checks.push('task execution keeps its token until refresh observes real progress, then only a new user click starts the next operation')
    await dialog.getByRole('button', { name: '关闭', exact: true }).first().click()
    await page.locator('#btn-credit-overview').click()
    dialog = page.getByRole('dialog', { name: '积分总览', exact: true })
    await page.evaluate(() => { fixture.defer = true; fixture.growthCalls = [] })
    await dialog.getByRole('radio', { name: '福利待办', exact: true }).click()
    await page.waitForFunction(() => fixture.pending.length === 3)
    assert.equal(await page.evaluate(() => fixture.maximum), 3)
    assert.deepEqual(await page.evaluate(() => fixture.growthCalls), ['A0', 'A1', 'A2'])
    await dialog.getByRole('button', { name: '停止未派发', exact: true }).click()
    await page.evaluate(() => fixture.pending.splice(0).forEach(item => item.resolve()))
    await dialog.getByRole('button', { name: '停止未派发', exact: true }).waitFor({ state: 'detached' })
    assert.equal(await page.evaluate(() => fixture.growthCalls.length), 3)
    assert.match(await dialog.innerText(), /国际、企业及重复身份 3 条/)
    checks.push('overview limits concurrency to three and stops undispatched accounts; international, enterprise and duplicate identities excluded')
    await dialog.getByRole('button', { name: '刷新范围福利', exact: true }).click()
    await page.waitForFunction(() => fixture.pending.length === 3)
    await dialog.getByRole('button', { name: '关闭', exact: true }).first().click()
    await page.evaluate(() => fixture.pending.splice(0).forEach(item => item.resolve()))
    await page.waitForTimeout(100)
    assert.equal(await page.evaluate(() => fixture.growthCalls.length), 6)
    checks.push('closing overview leaves dispatched reads to finish and starts no further account')
    const requestsFixture = path.join(evidence, 'growth-credits-requests-fixture.html')
    const requestsScript = `<script>fixture.requests=[{id:'zero',status:200,provider:'workbuddy',upstreamCredits:0},{id:'missing',status:200,provider:'workbuddy',upstreamCredits:null},{id:'failed',status:502,error:'示例上游错误',provider:'workbuddy',upstreamCredits:1.25}];wbApp.currentPage='requests';Object.assign(workbuddyDesktop,{getStatsRequests:async()=>({entries:fixture.requests,total:3,matched:3,running:0}),getStatsRequestFilters:async()=>({providers:[],models:[]}),getScheduledTasks:async()=>({tasks:[]}),getStatsRequestRaw:async()=>({}),getDebugTraffic:async()=>({})});</script>`
    fs.writeFileSync(requestsFixture, html.replace('data-page="accounts"', 'data-page="requests"').replace('<script src=', `${requestsScript}<script src=`))
    await page.goto(pathToFileURL(requestsFixture).href)
    await page.locator('#req-list').waitFor()
    await page.evaluate(() => wbRequestsPanel.load())
    for (const text of ['实扣积分: 0', '实扣积分: 未上报', '实扣积分: 1.25']) await page.getByText(text, { exact: true }).waitFor()
    for (const [id, expected] of [['zero', '0'], ['missing', '未上报'], ['failed', '1.25']]) {
      await page.evaluate(id => wbRequestDetail.open(id, fixture.requests.find(row => row.id === id)), id)
      const detail = page.getByRole('dialog', { name: '请求详情', exact: true })
      await detail.waitFor()
      assert.match(await detail.innerText(), new RegExp('实扣积分\\s+' + expected.replace('.', '\\.')))
      await page.evaluate(() => wbRequestDetail.close())
      await detail.waitFor({ state: 'detached' })
    }
    checks.push('request list and details display explicit zero, missing report and credits charged on a failed request')
    assert.deepEqual(errors, [])
    const result = { passed: checks.length, checks, pageErrors: errors }
    fs.writeFileSync(path.join(evidence, 'growth-browser-result.json'), JSON.stringify(result, null, 2))
    console.log(JSON.stringify(result, null, 2))
  } finally { await browser.close() }
}
run().catch(error => { console.error(error); process.exitCode = 1 })
