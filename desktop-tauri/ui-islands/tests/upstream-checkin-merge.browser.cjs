const { chromium } = require('playwright')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { createHash } = require('node:crypto')
const { pathToFileURL } = require('node:url')

const ui = path.resolve(__dirname, '../../ui')
const href = file => pathToFileURL(path.join(ui, file)).href
const bundle = path.join(ui, 'islands/ui.js')
const hashBundle = () => createHash('sha256').update(fs.readFileSync(bundle)).digest('hex')
const styles = ['css/tokens.css', 'css/layout.css', 'css/components.css', 'css/page-checkin.css', 'islands/ui.css']

// 沿用现有浏览器用例的 file fixture + 真实 ui.js；仅替换后端桥，不重编译或导出内部组件。
const html = `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">
<base href="${href('')}/"><meta name="viewport" content="width=device-width,initial-scale=1">
${styles.map(file => `<link rel="stylesheet" href="${href(file)}">`).join('')}
<style>body{padding:20px}.page{display:block!important}</style></head><body>
<section class="page active" data-page="checkin"></section><div id="fixture-toast"></div><script>
const row = (id, name) => ({id,name,available:true,checkedInToday:false,checkinAt:null});
window.fixture = {
  calls:{center:0,refresh:0,activity:[],checkin:[],batch:0,query:[],claim:[]},
  snapshot:{
    daily:{providers:[
      {id:'workbuddy-intl',label:'WorkBuddy 国际版',totalCount:2,doneCount:0,accounts:[row('intl-a','国际账号 A'),row('intl-b','国际账号 B')]},
      {id:'loomy',label:'Loomy',totalCount:2,doneCount:0,accounts:[row('L','已完成候选'),row('N','未确认候选')]},
      {id:'raccoon',label:'小浣熊',totalCount:1,doneCount:0,accounts:[row('R','未执行候选')]}
    ],outOfScope:[],todayDone:0,todayEligible:5},
    extras:{onboarding:[{id:'L',name:'已完成候选',provider:'loomy'},{id:'N',name:'未确认候选',provider:'loomy'},{id:'R',name:'未执行候选',provider:'raccoon'}],welfare:[],plans:[]},
    auto:{enabled:true,time:'09:00',providers:['workbuddy-intl','loomy','raccoon'],providerOptions:[{id:'workbuddy-intl',label:'WorkBuddy 国际版'},{id:'loomy',label:'Loomy'},{id:'raccoon',label:'小浣熊'}]},
    keepalive:{models:['fixture-free-model'],defaultModels:['fixture-free-model']},history:[]
  },
  activityRows:{
    keepalive:{id:'intl-a',activity:{pokeSucceeded:true}},
    claim:{id:'intl-a',claim:{success:true,creditVerification:'unverified'}},
    full:{id:'intl-a',claim:{status:'auth_expired',msg:'登录态已过期'},activity:{pokeSucceeded:true}}
  },
  singleRow:{id:'L',claim:{status:'auth_expired',msg:'登录态已过期'},activity:{pokeSucceeded:true}},
  batchResult:{succeeded:1,total:3,failedCount:0,completedAccountIds:['L']}
};
window.wbApp = {
  currentPage:'checkin',getState:()=>({accounts:{accounts:[]}}),
  toast(message){document.getElementById('fixture-toast').textContent=message},
  refresh(){fixture.calls.refresh++}
};
window.workbuddyDesktop = {
  async getCheckinCenter(){fixture.calls.center++;return structuredClone(fixture.snapshot)},
  async runCheckinActivity(id,mode){fixture.calls.activity.push([id,mode]);return structuredClone(fixture.activityRows[mode])},
  async checkinAllAccounts(id){fixture.calls.checkin.push(id);return {results:[structuredClone(fixture.singleRow)]}},
  async runAutoCheckinNow(){fixture.calls.batch++;return structuredClone(fixture.batchResult)},
  async getOnboardingTasks(id){fixture.calls.query.push(id);return {tasks:[{key:'first-login',title:'首次登录',done:false}],unclaimed:1}},
  async claimOnboardingTasks(id){fixture.calls.claim.push(id);return {results:[{key:'first-login',ok:true}],tasks:[{key:'first-login',title:'首次登录',done:true}],earned:10}},
  async getAllBalances(){return {results:[]}}
};
</script><script src="${href('islands/ui.js')}"></script></body></html>`

async function run() {
  const sourceHash = hashBundle()
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'upstream-checkin-browser-'))
  const fixtureFile = path.join(directory, 'fixture.html')
  fs.writeFileSync(fixtureFile, html)
  const checks = [], errors = [], nonLocalRequests = []
  let browser
  try {
    browser = await chromium.launch({ ...(process.platform === 'win32' ? { channel: 'msedge' } : {}), headless: true })
    const context = await browser.newContext({ viewport: { width: 1280, height: 1000 }, locale: 'zh-CN', timezoneId: 'Asia/Shanghai', offline: true, serviceWorkers: 'block' })
    await context.route('**/*', route => {
      if (/^(file|data):/.test(route.request().url())) return route.continue()
      nonLocalRequests.push(route.request().url())
      return route.abort()
    })
    await context.routeWebSocket('**/*', socket => {
      nonLocalRequests.push(socket.url())
      socket.close()
    })
    const page = await context.newPage()
    page.setDefaultTimeout(10_000)
    page.on('pageerror', error => errors.push(error.message))

    async function reset() {
      await page.goto(pathToFileURL(fixtureFile).href)
      await page.getByRole('heading', { name: '每日签到', exact: true }).waitFor()
      assert.deepEqual(await page.evaluate(() => fixture.calls.query), [])
      assert.deepEqual(await page.evaluate(() => fixture.calls.claim), [])
    }

    async function expand(provider) {
      const group = page.locator('.ck-prov-row').filter({ has: page.locator('.ck-prov-name', { hasText: provider }) })
      const toggle = group.locator('.ck-prov-main')
      assert.equal(await toggle.getAttribute('aria-expanded'), 'false')
      await toggle.click()
      await group.locator('tbody').waitFor()
      assert.equal(await toggle.getAttribute('aria-expanded'), 'true')
      return group
    }

    async function clickAction(button) {
      const before = await page.evaluate(() => ({ center: fixture.calls.center, refresh: fixture.calls.refresh }))
      await button.click()
      // 主刷新发生在动作和快照重读之后；fixture 桥无定时器，后续 Promise 微任务会自然排空。
      await page.waitForFunction(previous => fixture.calls.refresh > previous.refresh && fixture.calls.center > previous.center, before)
    }

    async function refreshWithoutToast() {
      const before = await page.evaluate(() => fixture.calls.center)
      // app.js 的切页钩子正是这个公开入口；快照里刻意没有上次单签结果。
      const eligible = await page.evaluate(async () => {
        document.getElementById('fixture-toast').textContent = ''
        fixture.snapshot.daily.todayEligible++
        await wbCheckinPanel.load()
        return fixture.snapshot.daily.todayEligible
      })
      await page.waitForFunction(expected => document.querySelector('.ck-stat-value small')?.textContent.trim() === `/ ${expected}`, eligible)
      assert.equal(await page.evaluate(() => fixture.calls.center), before + 1)
      assert.equal(await page.locator('#fixture-toast').innerText(), '')
    }

    async function assertNoOnboarding() {
      assert.deepEqual(await page.evaluate(() => ({ query: fixture.calls.query, claim: fixture.calls.claim })), { query: [], claim: [] })
    }

    await reset()
    const intl = await expand('WorkBuddy 国际版')
    const account = intl.getByRole('row').filter({ hasText: '国际账号 A' })
    const other = intl.getByRole('row').filter({ hasText: '国际账号 B' })
    assert.deepEqual(await account.getByRole('button').allTextContents(), ['保活', '领取', '保活+领取'])
    assert.equal(await account.getByRole('status').count(), 0)

    await clickAction(account.getByRole('button', { name: '保活+领取', exact: true }))
    await refreshWithoutToast()
    await account.getByRole('status').filter({ hasText: '登录态已过期' }).waitFor()
    assert.match(await account.getByRole('status').getAttribute('class'), /text-destructive/)
    assert.doesNotMatch(await account.getByRole('status').innerText(), /保活完成|签到成功|已领取/)
    assert.equal(await other.getByRole('status').count(), 0)
    await assertNoOnboarding()
    checks.push('authentication failure survives snapshot refresh despite successful keepalive')

    await clickAction(account.getByRole('button', { name: '保活', exact: true }))
    await refreshWithoutToast()
    await account.getByRole('status').filter({ hasText: '未领取奖励' }).waitFor()
    assert.doesNotMatch(await account.getByRole('status').getAttribute('class'), /text-destructive/)
    assert.doesNotMatch(await account.getByRole('status').innerText(), /登录态已过期|签到成功|已领取/)
    assert.equal(await account.getByText('待签', { exact: true }).count(), 1)
    await assertNoOnboarding()
    checks.push('keepalive remains visibly unclaimed after snapshot refresh')

    await clickAction(account.getByRole('button', { name: '领取', exact: true }))
    await refreshWithoutToast()
    await account.getByRole('status').filter({ hasText: '额度到账待核验' }).waitFor()
    assert.doesNotMatch(await account.getByRole('status').getAttribute('class'), /text-destructive/)
    assert.doesNotMatch(await account.getByRole('status').innerText(), /签到成功|已到账|未领取奖励/)
    assert.equal(await other.getByRole('status').count(), 0)
    await assertNoOnboarding()
    checks.push('pending credit remains visible after snapshot refresh without claiming arrival')
    assert.deepEqual(await page.evaluate(() => fixture.calls.activity), [['intl-a', 'full'], ['intl-a', 'keepalive'], ['intl-a', 'claim']])
    assert.deepEqual(await page.evaluate(() => fixture.calls.checkin), [])
    assert.equal(await page.evaluate(() => fixture.calls.batch), 0)
    checks.push('mounted checkin center dispatches three international buttons to their explicit modes')

    await reset()
    const loomy = await expand('Loomy')
    const single = loomy.getByRole('row').filter({ hasText: '已完成候选' })
    await clickAction(single.getByRole('button', { name: '签到', exact: true }))
    await single.getByRole('status').filter({ hasText: '登录态已过期' }).waitFor()
    assert.deepEqual(await page.evaluate(() => fixture.calls.checkin), ['L'])
    await assertNoOnboarding()
    checks.push('failed single checkin never queries or claims onboarding')

    await reset()
    await clickAction(page.getByRole('button', { name: '立即全部签到', exact: true }))
    await page.waitForFunction(() => fixture.calls.claim.length === 1)
    assert.deepEqual(await page.evaluate(() => ({ batch: fixture.calls.batch, query: fixture.calls.query, claim: fixture.calls.claim })), { batch: 1, query: ['L'], claim: ['L'] })
    checks.push('batch onboarding follows only completed IDs and excludes selected neutral or unexecuted accounts')

    await reset()
    await page.evaluate(() => { fixture.batchResult = { succeeded: 1, total: 3, failedCount: 0 } })
    await clickAction(page.getByRole('button', { name: '立即全部签到', exact: true }))
    assert.equal(await page.evaluate(() => fixture.calls.batch), 1)
    await assertNoOnboarding()
    checks.push('batch without completed IDs never queries or claims onboarding')

    await reset()
    await page.evaluate(() => { fixture.batchResult = { succeeded: 1, total: 3, failedCount: 1, completedAccountIds: ['L'] } })
    await clickAction(page.getByRole('button', { name: '立即全部签到', exact: true }))
    assert.equal(await page.evaluate(() => fixture.calls.batch), 1)
    assert.match(await page.locator('#fixture-toast').innerText(), /1 个失败/)
    await assertNoOnboarding()
    checks.push('partially failed batch never automatically claims onboarding even with completed IDs')

    assert.deepEqual(errors, [], '真实 ui.js 不得产生页面异常')
    assert.deepEqual(nonLocalRequests, [], 'fixture 只允许本地 file 资源')
    assert.equal(hashBundle(), sourceHash, '测试期间 ui.js 发生变化，需对新产物重跑')
    for (const name of checks) console.log(`PASS ${name}`)
    console.log(JSON.stringify({ passed: checks.length, uiJsSha256: sourceHash, pageErrors: errors, nonLocalRequests }, null, 2))
  } finally {
    if (browser) await browser.close()
    fs.rmSync(directory, { recursive: true, force: true })
  }
}

run().catch(error => { console.error(error); process.exitCode = 1 })
