const { before, test } = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const vm = require('node:vm')
const { createHash } = require('node:crypto')
const { build } = require('../node_modules/esbuild')
const React = require('../node_modules/react')
const { renderToStaticMarkup } = require('../node_modules/react-dom/server')

const root = path.resolve(__dirname, '../src/islands')
let source

// 与 accounts-credit-cache / models-reasoning-override 一样执行真实模块。
// 仅在内存中导出页面内部组件；UI 外壳和挂载边界替身不替换业务分派或状态函数。
before(async context => {
  const bundled = await build({
    stdin: {
      contents: `
        export * from './checkin-state'
        export { lowBalanceBlockedOf, lowBalanceOf, defaultLowBalanceMode } from './accounts-domain'
        export { BUILTIN_CONFIGS, methodsOf } from './add-account-configs'
        export { AccountRows } from './checkin-page'
      `,
      resolveDir: root,
      sourcefile: 'checkin-merge-fixture.ts',
      loader: 'ts',
    },
    bundle: true, format: 'cjs', platform: 'node', write: false,
    tsconfigRaw: { compilerOptions: { jsx: 'react' } },
    plugins: [{
      name: 'checkin-test-boundaries',
      setup(builder) {
        builder.onResolve({ filter: /^(react|react-dom\/client|@ui|\.\/add-provider-pick)$/ }, args => ({
          path: args.path, external: true,
        }))
        builder.onLoad({ filter: /[\\/]checkin-page\.tsx$/ }, args => ({
          contents: fs.readFileSync(args.path, 'utf8') + '\nexport { AccountRows }\n',
          loader: 'tsx',
        }))
      },
    }],
  })
  source = bundled.outputFiles[0].text
  context.diagnostic(`in-memory production bundle sha256=${createHash('sha256').update(source).digest('hex')}`)
})

const tick = () => new Promise(resolve => setImmediate(resolve))

async function fixture({ row = { id: 'A', claim: { success: true } }, onboarding = [], runBatch, platform = 'web' } = {}) {
  const snapshot = {
    daily: { providers: [], outOfScope: [], todayDone: 0, todayEligible: 0 },
    extras: { onboarding, welfare: [], plans: [] },
    auto: { enabled: true, providers: ['workbuddy'] },
    keepalive: { models: ['old-model'], defaultModels: ['default-model'] },
    history: [],
  }
  const calls = { checkin: [], activity: [], query: [], claim: [], save: [], usage: [], batch: 0, center: 0 }
  const messages = []
  const bridge = {
    platform,
    async getCheckinCenter() { calls.center++; return snapshot },
    async checkinAllAccounts(id) { calls.checkin.push(id); return { results: [row] } },
    async runCheckinActivity(id, mode) { calls.activity.push([id, mode]); return row },
    async runAutoCheckinNow() {
      calls.batch++
      return runBatch ? runBatch() : { succeeded: 1, total: 1, failedCount: 0 }
    },
    async getOnboardingTasks(id) {
      calls.query.push(id)
      return { tasks: [{ key: 'first-login', title: '首次登录', done: false }], unclaimed: 1 }
    },
    async claimOnboardingTasks(id) {
      calls.claim.push(id)
      return { results: [{ key: 'first-login', ok: true }], tasks: [{ key: 'first-login', done: true }], earned: 10 }
    },
    async saveCheckinKeepalive(models) {
      calls.save.push(Array.from(models))
      return { models: models.length ? Array.from(models) : ['default-model'], defaultModels: ['default-model'] }
    },
  }
  const module = { exports: {} }
  vm.runInNewContext(source, {
    module, exports: module.exports, Error,
    window: {
      workbuddyDesktop: bridge,
      wbApp: { toast: (message, kind) => messages.push({ message, kind }), refresh() {} },
      wbAccountsView: { refreshUsageAfterCheckin: id => calls.usage.push(id) },
    },
    document: { querySelector: () => null, addEventListener() {} },
    require(dependency) {
      if (dependency === 'react') return React
      if (dependency === '@ui') return {
        Badge: 'span', Button: 'button', Checkbox: 'input', Input: 'input',
        Progress: 'progress', Spinner: 'span', Switch: 'input',
      }
      if (dependency === './add-provider-pick') return { PROVIDER_ICONS: {} }
      if (dependency === 'react-dom/client') return { createRoot() { assert.fail('unexpected DOM mount') } }
      assert.fail(`unexpected dependency: ${dependency}`)
    },
  }, { filename: 'checkin-merge-fixture.cjs' })
  const api = module.exports
  await api.loadCheckinCenter()
  return { api, bridge, snapshot, calls, messages }
}

function singleMessage(f) {
  assert.equal(f.messages.length, 1, 'one result notification must be emitted')
  return f.messages[0]
}

for (const [name, claim] of [
  ['alreadyCompleted', { alreadyCompleted: true, msg: '今日已领取' }],
  ['already_claimed status', { status: 'already_claimed' }],
]) {
  test(`single checkin treats ${name} as already received, not an error`, async () => {
    const f = await fixture({ row: { id: 'A', claim } })
    await f.api.signSingleAccount('A')
    const result = singleMessage(f)
    assert.notEqual(result.kind, 'err')
    assert.match(result.message, /已领取|已签到/)
    assert.deepEqual(f.calls.checkin, ['A'])
    assert.deepEqual(f.calls.activity, [])
  })
}

for (const mode of ['checkin', 'full', 'keepalive']) {
  test(`single ${mode} preserves auth_expired despite successful keepalive`, async () => {
    const f = await fixture({ row: {
      id: 'A', claim: { status: 'auth_expired', msg: '登录态已过期' }, activity: { pokeSucceeded: true },
    } })
    await f.api.signSingleAccount('A', mode)
    const result = singleMessage(f)
    assert.equal(result.kind, 'err')
    assert.match(result.message, /登录态已过期/)
    assert.equal(f.api.getCheckinStore().signing.size, 0, 'failure must release the in-flight key')
  })
}

for (const [name, claim] of [
  ['unverified receipt without message', { success: true, creditVerification: 'unverified' }],
  ['query failure on an already received receipt', { alreadyCompleted: true, creditVerification: 'query_failed' }],
  ['server receipt explanation', { success: true, creditVerification: 'query_failed', msg: '签到已确认，额度到账待核验：余额查询失败' }],
]) {
  test(`single checkin preserves pending-credit classification: ${name}`, async () => {
    const f = await fixture({ row: { id: 'A', claim } })
    await f.api.signSingleAccount('A')
    const result = singleMessage(f)
    assert.notEqual(result.kind, 'err')
    assert.match(result.message, /额度到账待核验/)
    if (claim.msg) assert.ok(result.message.includes(claim.msg), 'preserve the server explanation')
  })
}

test('ordinary successful checkin still reports success and refreshes account usage', async () => {
  const f = await fixture()
  await f.api.signSingleAccount('A')
  assert.notEqual(singleMessage(f).kind, 'err')
  assert.match(singleMessage(f).message, /签到成功/)
  assert.deepEqual(f.calls.usage, ['A'])
  assert.equal(f.api.getCheckinStore().signing.size, 0)
})

test('keepalive-only success does not claim that a reward was received', async () => {
  const f = await fixture({ row: { id: 'A', activity: { pokeSucceeded: true } } })
  await f.api.signSingleAccount('A', 'keepalive')
  const result = singleMessage(f)
  assert.notEqual(result.kind, 'err')
  assert.match(result.message, /保活|活跃/)
  assert.doesNotMatch(result.message, /签到成功|成功领取|奖励到账/)
  assert.deepEqual(f.calls.activity, [['A', 'keepalive']])
  assert.deepEqual(f.calls.checkin, [])
})

for (const [name, runBatch] of [
  ['rejected request', () => { throw new Error('fixture checkin failure') }],
  ['failed batch result', () => ({ succeeded: 0, total: 1, failedCount: 1, failed: ['fixture checkin failure'] })],
]) {
  test(`runAllCheckin makes zero extra onboarding claims after ${name}`, async () => {
    const f = await fixture({
      runBatch, onboarding: [{ id: 'unselected', name: 'Unselected Loomy', provider: 'loomy' }],
    })
    await f.api.runAllCheckin()
    // 桥全部立即 resolve；排空 finally 内未 await 的后台跟进，防止过早断言假通过。
    await tick()
    assert.equal(f.calls.batch, 1)
    assert.deepEqual(f.calls.claim, [], 'a failed daily run must not claim unrelated onboarding rewards')
    assert.equal(singleMessage(f).kind, 'err')
    assert.equal(f.api.getCheckinStore().runningAll, false)
    assert.ok(f.calls.center >= 2, 'failure must still refresh the checkin snapshot')
  })
}

test('explicit onboarding query and claim remain available', async () => {
  const f = await fixture({ onboarding: [{ id: 'L', name: 'Loomy', provider: 'loomy' }] })
  await f.api.queryOnboarding('L')
  assert.deepEqual(f.calls.query, ['L'])
  assert.deepEqual(f.calls.claim, [], 'query must be read-only')
  assert.equal(f.api.getCheckinStore().onboarding.get('L').unclaimed, 1)
  await f.api.claimOnboarding('L')
  assert.deepEqual(f.calls.claim, ['L'])
  assert.equal(f.api.getCheckinStore().onboarding.get('L').unclaimed, 0)
})

for (const [name, value, expected] of [
  ['empty input', '', []],
  ['whitespace input', ' \t ', []],
  ['configured models', ' model-a， model-b、model-c, ', ['model-a', 'model-b', 'model-c']],
]) {
  test(`keepalive model save submits ${name} to the bridge`, async () => {
    const f = await fixture()
    f.api.setKeepaliveDraft(value)
    await f.api.submitKeepaliveModels(value)
    assert.deepEqual(f.calls.save, [expected])
    const store = f.api.getCheckinStore()
    assert.equal(store.keepaliveDraft, null)
    assert.equal(store.keepaliveSaving, false)
    assert.deepEqual(Array.from(store.snapshot.keepalive.models), expected.length ? expected : ['default-model'])
  })
}

const balanceAccount = { id: 'A', provider: 'minimax-code', lowBalance: { mode: 'skip', threshold: 1 } }
for (const [name, entry, expected] of [
  ['unknown null balance', { available: null }, false],
  ['missing balance', {}, false],
  ['invalid balance', { available: 'unknown' }, false],
  ['verified zero balance', { available: 0 }, true],
  ['balance equal to threshold', { available: 1 }, false],
  ['unlimited balance', { available: 0, unlimited: true }, false],
  ['WorkBuddy totalLeft zero', { totalLeft: 0, available: 10 }, true],
]) {
  test(`low-balance classification: ${name}`, async () => {
    const f = await fixture()
    assert.equal(f.api.lowBalanceBlockedOf(balanceAccount, entry), expected)
  })
}

test('explicitly disabled low-balance protection preserves zero-balance availability', async () => {
  const f = await fixture()
  assert.equal(f.api.lowBalanceBlockedOf({ ...balanceAccount, lowBalance: { mode: 'off', threshold: 1 } }, { available: 0 }), false)
})

function accountButtons(api, provider, edition) {
  const tree = api.AccountRows({ group: {
    id: provider, label: provider, totalCount: 1, doneCount: 0,
    accounts: [{ id: 'A', name: 'Fixture', available: true, edition, checkedInToday: false, checkinAt: null }],
  } })
  const buttons = []
  function visit(node) {
    if (Array.isArray(node)) return node.forEach(visit)
    if (!React.isValidElement(node)) return
    if (node.type === 'button') buttons.push(node.props)
    visit(node.props.children)
  }
  visit(tree)
  assert.ok(buttons.length, 'the account row must expose an actionable button')
  return buttons
}

for (const [provider, edition] of [['qoder', 'intl'], ['workbuddy', 'cn']]) {
  test(`${provider} ${edition} page buttons use ordinary checkin, never WorkBuddy activity`, async () => {
    const f = await fixture()
    const buttons = accountButtons(f.api, provider, edition)
    for (const button of buttons) {
      assert.notEqual(button.disabled, true)
      button.onClick()
      await tick()
    }
    assert.deepEqual(f.calls.activity, [], 'edition alone must not select the WorkBuddy activity bridge')
    assert.deepEqual(f.calls.checkin, ['A'])
    assert.equal(buttons.length, 1)
  })
}

test('WorkBuddy international page retains all three explicit activity modes', async () => {
  const f = await fixture()
  const buttons = accountButtons(f.api, 'workbuddy-intl', 'intl')
  assert.equal(buttons.length, 3)
  for (const button of buttons) {
    assert.notEqual(button.disabled, true)
    button.onClick()
    await tick()
  }
  assert.deepEqual(f.calls.checkin, [])
  assert.deepEqual(f.calls.activity, [['A', 'keepalive'], ['A', 'claim'], ['A', 'full']])
})

for (const [name, row, reject] of [
  ['request rejection', undefined, true],
  ['dispatcher error', { id: 'A', error: 'fixture dispatcher failure' }],
  ['auth_expired with keepalive', { id: 'A', claim: { status: 'auth_expired' }, activity: { pokeSucceeded: true } }],
  ['unsuccessful claim', { id: 'A', claim: { success: false, msg: '领取失败' } }],
  ['missing result', null],
]) {
  test(`single checkin makes zero extra onboarding claims after ${name}`, async () => {
    const f = await fixture({ row, onboarding: [{ id: 'A', name: 'Loomy', provider: 'loomy' }] })
    if (reject) f.bridge.checkinAllAccounts = async () => { throw new Error('fixture request failure') }
    await f.api.signSingleAccount('A')
    await tick()
    assert.deepEqual(f.calls.claim, [])
    assert.equal(singleMessage(f).kind, 'err')
    assert.equal(f.api.getCheckinStore().signing.size, 0)
    assert.ok(f.calls.center >= 2)
  })
}

test('successful single checkin follows up only its own onboarding tasks', async () => {
  const f = await fixture({ onboarding: [
    { id: 'A', name: 'Selected Loomy', provider: 'loomy' },
    { id: 'B', name: 'Other Loomy', provider: 'loomy' },
  ] })
  await f.api.signSingleAccount('A')
  await tick()
  assert.deepEqual(f.calls.query, ['A'])
  assert.deepEqual(f.calls.claim, ['A'])
})

test('successful batch follows up only completed IDs, not selected neutral or unexecuted accounts', async () => {
  const f = await fixture({
    runBatch: () => ({ succeeded: 1, total: 3, failedCount: 0, completedAccountIds: ['L'] }),
    onboarding: [
      { id: 'L', name: 'Completed Loomy', provider: 'loomy' },
      { id: 'N', name: 'Neutral Loomy', provider: 'loomy' },
      { id: 'R', name: 'Unexecuted Raccoon', provider: 'raccoon' },
    ],
  })
  f.snapshot.auto.providers = ['loomy', 'raccoon']
  await f.api.runAllCheckin()
  await tick()
  assert.deepEqual(f.calls.claim, ['L'])
  assert.deepEqual(f.calls.query, ['L'])
})

for (const [name, result] of [
  ['missing completed IDs', { succeeded: 1, total: 1, failedCount: 0 }],
  ['empty completed IDs', { succeeded: 1, total: 1, failedCount: 0, completedAccountIds: [] }],
  ['partial failure', { succeeded: 1, total: 2, failedCount: 1, completedAccountIds: ['L'] }],
  ['failure list without count', { succeeded: 1, total: 2, failed: ['R: auth expired'], completedAccountIds: ['L'] }],
]) {
  test(`batch makes zero extra onboarding claims with ${name}`, async () => {
    const f = await fixture({
      runBatch: () => result,
      onboarding: [{ id: 'L', name: 'Loomy', provider: 'loomy' }],
    })
    f.snapshot.auto.providers = ['loomy']
    await f.api.runAllCheckin()
    await tick()
    assert.deepEqual(f.calls.claim, [])
    assert.deepEqual(f.calls.query, [])
  })
}

test('already received IDs can follow up even without a new successful claim', async () => {
  const f = await fixture({
    runBatch: () => ({ succeeded: 0, total: 1, failedCount: 0, completedAccountIds: ['L'] }),
    onboarding: [{ id: 'L', name: 'Loomy', provider: 'loomy' }],
  })
  await f.api.runAllCheckin()
  await tick()
  assert.deepEqual(f.calls.claim, ['L'])
})

test('a failed snapshot refresh cannot reuse stale onboarding eligibility', async () => {
  const f = await fixture({ onboarding: [{ id: 'A', name: 'Loomy', provider: 'loomy' }] })
  f.bridge.getCheckinCenter = async () => { throw new Error('fixture snapshot failure') }
  await f.api.signSingleAccount('A')
  await tick()
  assert.deepEqual(f.calls.claim, [])
  assert.match(f.api.getCheckinStore().loadError, /fixture snapshot failure/)
})

for (const provider of ['workbuddy', 'workbuddy-intl', 'minimax-code', 'qoder', 'cline-free', 'kuku']) {
  test(`${provider} low-balance defaults match backend off, while explicit skip remains effective`, async () => {
    const f = await fixture()
    assert.equal(f.api.defaultLowBalanceMode(provider), 'off')
    for (const lowBalance of [undefined, {}, { mode: 'invalid', threshold: 1 }]) {
      const account = { id: 'A', provider, lowBalance }
      assert.equal(f.api.lowBalanceOf(account).mode, 'off')
      assert.equal(f.api.lowBalanceOf(account).threshold, 0)
      assert.equal(f.api.lowBalanceBlockedOf(account, { available: 0 }), false)
    }
    assert.equal(f.api.lowBalanceBlockedOf({ id: 'A', provider, lowBalance: { mode: 'skip', threshold: 1 } }, { available: 0 }), true)
  })
}

for (const platform of ['web', 'windows', 'macos', 'linux']) {
  test(`Kuku login methods respect the ${platform} platform`, async () => {
    const f = await fixture({ platform })
    const config = f.api.BUILTIN_CONFIGS.find(item => item.provider === 'kuku')
    assert.ok(config)
    const methods = Array.from(f.api.methodsOf(config, ''))
    if (platform === 'web') assert.deepEqual(methods, ['manual'])
    else {
      assert.ok(methods.includes('web'), 'desktop must retain browser login')
      assert.ok(methods.includes('manual'), 'manual credentials remain available')
    }
  })
}

test('Kuku platform gating does not remove remote login from other providers', async () => {
  const f = await fixture({ platform: 'web' })
  for (const provider of ['raccoon', 'qoder', 'minimax-code', 'lobsterai']) {
    const config = f.api.BUILTIN_CONFIGS.find(item => item.provider === provider)
    assert.ok(config)
    assert.ok(f.api.methodsOf(config, 'cn').includes('web'), `${provider} retains remote login`)
  }
})

function accountMarkup(api, id = 'A') {
  return renderToStaticMarkup(api.AccountRows({ group: {
    id: 'workbuddy-intl', label: 'WorkBuddy 国际版', totalCount: 1, doneCount: 0,
    accounts: [{ id, name: id, available: true, checkedInToday: false, checkinAt: null }],
  } }))
}

async function assertPersistentSingleResult(row, mode, expected, failed) {
  const f = await fixture({ row })
  await f.api.signSingleAccount('A', mode)
  f.messages.length = 0
  await f.api.loadCheckinCenter()
  const markup = accountMarkup(f.api)
  assert.ok(markup.includes(expected), 'the actual account row must retain the result')
  assert.ok(markup.includes('role="status"'))
  assert.equal(markup.includes('text-destructive'), failed)
  assert.ok(!accountMarkup(f.api, 'B').includes(expected), 'results belong to one account')
}

test('single result remains visible after toast and snapshot refresh: authentication failure', () =>
  assertPersistentSingleResult({ id: 'A', claim: { status: 'auth_expired', msg: '登录态已过期' }, activity: { pokeSucceeded: true } }, 'full', '登录态已过期', true))

test('single result remains visible after toast and snapshot refresh: already received', () =>
  assertPersistentSingleResult({ id: 'A', claim: { alreadyCompleted: true } }, 'checkin', '今日已领取', false))

test('single result remains visible after toast and snapshot refresh: pending credit', () =>
  assertPersistentSingleResult({ id: 'A', claim: { success: true, creditVerification: 'unverified' } }, 'checkin', '额度到账待核验', false))

test('single result remains visible after toast and snapshot refresh: keepalive only', () =>
  assertPersistentSingleResult({ id: 'A', activity: { pokeSucceeded: true } }, 'keepalive', '未领取奖励', false))

test('single request rejection stays visible until the next result replaces it', async () => {
  const f = await fixture()
  f.bridge.checkinAllAccounts = async () => { throw new Error('fixture single request failed') }
  await f.api.signSingleAccount('A')
  f.bridge.getCheckinCenter = async () => { throw new Error('fixture snapshot failed') }
  await f.api.loadCheckinCenter()
  assert.match(accountMarkup(f.api), /fixture single request failed/)
  assert.match(f.api.getCheckinStore().loadError, /fixture snapshot failed/)
  f.bridge.checkinAllAccounts = async () => ({ results: [{ id: 'A', claim: { success: true } }] })
  await f.api.signSingleAccount('A')
  const markup = accountMarkup(f.api)
  assert.match(markup, /签到成功/)
  assert.doesNotMatch(markup, /fixture single request failed/)
})

test('single result renders upstream text without treating it as HTML', async () => {
  const f = await fixture({ row: { id: 'A', error: '<img src=x onerror=alert(1)>' } })
  await f.api.signSingleAccount('A')
  const markup = accountMarkup(f.api)
  assert.match(markup, /&lt;img/)
  assert.doesNotMatch(markup, /<img/)
})
