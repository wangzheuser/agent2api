import assert from 'node:assert/strict'
import { test } from 'node:test'
import { build } from 'esbuild'
import { fileURLToPath } from 'node:url'

const bundled = await build({
  entryPoints: [fileURLToPath(new URL('../src/islands/accounts-data.ts', import.meta.url))],
  bundle: true, format: 'esm', platform: 'node', write: false,
})
const source = bundled.outputFiles[0].text
let moduleId = 0
async function fixture(accounts = [{ id: 'A', uid: 'uA', provider: 'workbuddy', addedAt: 1 }]) {
  const state = { accounts: { accounts } }
  const calls = []
  const pending = []
  globalThis.window = {
    wbApp: { getState: () => state, toast() {} },
    workbuddyDesktop: {
      getAllBalances(id) {
        calls.push(id)
        return new Promise((resolve, reject) => pending.push({ resolve, reject }))
      },
    },
  }
  globalThis.localStorage = { getItem: () => null }
  const api = await import(`data:text/javascript;base64,${Buffer.from(source + `\n// fixture ${++moduleId}`).toString('base64')}`)
  const tick = () => new Promise(resolve => setImmediate(resolve))
  const usage = (remaining, at = Date.now()) => ({ creditDetails: {
    version: 1, kind: 'personal', fetchedAt: at, complete: true, remaining,
    unlimited: false, unattributedRemaining: 0, issues: [], segments: [],
  } })
  const result = (id, value) => ({ results: [{ id, usage: value }] })
  return { api, state, calls, pending, tick, usage, result }
}

test('fresh details reopen without a request; force bypasses freshness and preserves old data', async () => {
  const f = await fixture()
  const previous = f.usage(12)
  f.api.applyBalances(f.result('A', previous))
  await f.api.refreshCreditDetails('A')
  assert.equal(f.calls.length, 0)
  f.api.openCreditsDialog('A')
  assert.equal(f.api.getStore().dialog.id, 'A')
  await f.tick()
  assert.equal(f.calls.length, 0)
  const first = f.api.refreshCreditDetails('A', true)
  const second = f.api.refreshCreditDetails('A', true)
  await f.tick()
  assert.deepEqual(f.calls, ['A'])
  assert.equal(f.api.usageEntries().get('A'), previous)
  assert.ok(f.api.getStore().usageInflight.has('A'))
  f.pending[0].resolve(f.result('A', f.usage(9)))
  await Promise.all([first, second])
  assert.equal(f.api.lastSuccessfulUsage('A').creditDetails.remaining, 9)
  assert.ok(!f.api.getStore().usageInflight.has('A'))
})

test('missing details and expired details query only the requested account', async () => {
  const f = await fixture([{ id: 'A' }, { id: 'B' }])
  f.api.applyBalances(f.result('A', { totalLeft: 50 }))
  const first = f.api.refreshCreditDetails('A')
  await f.tick()
  assert.deepEqual(f.calls, ['A'])
  f.pending[0].resolve(f.result('A', f.usage(5)))
  await first
  f.api.applyBalances({ at: Date.now(), ...f.result('B', f.usage(8, Date.now() - 61_000)) })
  const second = f.api.refreshCreditDetails('B')
  await f.tick()
  assert.deepEqual(f.calls, ['A', 'B'])
  f.pending[1].resolve(f.result('B', f.usage(7)))
  await second
})

test('transport and business failures retain prior successful details with current errors', async () => {
  const f = await fixture()
  const previous = f.usage(12)
  f.api.applyBalances(f.result('A', previous))
  const first = f.api.refreshCreditDetails('A', true)
  await f.tick()
  f.pending[0].reject(new Error('offline'))
  await first
  assert.match(f.api.usageFailureOf(f.api.usageEntries().get('A')).message, /offline/)
  assert.equal(f.api.lastSuccessfulUsage('A'), previous)
  const second = f.api.refreshCreditDetails('A', true)
  await f.tick()
  f.pending[1].resolve({ results: [{ id: 'A', error: 'denied' }] })
  await second
  assert.equal(f.api.usageFailureOf(f.api.usageEntries().get('A')).message, 'denied')
  assert.equal(f.api.lastSuccessfulUsage('A'), previous)
})

test('partial and malformed fresh details are refreshed rather than treated as complete cache', async () => {
  const f = await fixture([{ id: 'A' }, { id: 'B' }])
  const partial = f.usage(null)
  partial.creditDetails.complete = false
  f.api.applyBalances(f.result('A', partial))
  f.api.applyBalances(f.result('B', { creditDetails: { fetchedAt: Date.now(), version: 2 } }))
  const first = f.api.refreshCreditDetails('A')
  const second = f.api.refreshCreditDetails('B')
  await f.tick()
  assert.deepEqual(f.calls, ['A', 'B'])
  f.pending[0].resolve(f.result('A', f.usage(0)))
  f.pending[1].resolve(f.result('B', f.usage(0)))
  await Promise.all([first, second])
  assert.equal(f.api.lastSuccessfulUsage('A').creditDetails.remaining, 0)
})

test('detail refresh waits for an existing batch and does not issue a single request', async () => {
  const f = await fixture([{ id: 'A' }, { id: 'B' }])
  const batch = f.api.queryUsageFor()
  const details = f.api.refreshCreditDetails('A', true)
  await f.tick()
  assert.deepEqual(f.calls, [undefined])
  f.pending[0].resolve({ results: [{ id: 'A', usage: f.usage(1) }, { id: 'B', usage: f.usage(2) }] })
  await Promise.all([batch, details])
  assert.equal(f.api.lastSuccessfulUsage('A').creditDetails.remaining, 1)
  assert.equal(f.api.lastSuccessfulUsage('B').creditDetails.remaining, 2)
})

test('late A response never modifies the open B dialog or B data', async () => {
  const f = await fixture([{ id: 'A' }, { id: 'B' }])
  f.api.openCreditsDialog('A')
  f.api.openCreditsDialog('B')
  await f.tick()
  f.pending[1].resolve(f.result('B', f.usage(22)))
  await f.tick()
  f.pending[0].resolve(f.result('A', f.usage(11)))
  await f.tick()
  assert.equal(f.api.getStore().dialog.id, 'B')
  assert.equal(f.api.lastSuccessfulUsage('B').creditDetails.remaining, 22)
})

test('deleted account and missing batch rows cannot recreate deleted cache entries', async () => {
  const f = await fixture([{ id: 'A' }, { id: 'B' }])
  const request = f.api.queryUsageFor()
  await f.tick()
  f.state.accounts.accounts = [{ id: 'B' }]
  f.api.refreshCaches(new Set(['B']))
  f.pending[0].resolve(f.result('A', f.usage(99)))
  await request
  assert.ok(!f.api.usageEntries().has('A'))
  assert.equal(f.api.lastSuccessfulUsage('A'), undefined)
  assert.equal(f.api.usageEntries().get('B'), '未返回余额数据')
})

test('replaced identity rejects late responses and stale snapshots, including old failures', async () => {
  const f = await fixture()
  f.api.openCreditsDialog('A')
  await f.tick()
  f.state.accounts.accounts = [{ id: 'A', uid: 'replacement', provider: 'workbuddy', addedAt: 2 }]
  f.api.refreshCaches(new Set(['A']))
  assert.equal(f.api.getStore().dialog, null)
  const replacement = f.api.refreshCreditDetails('A', true)
  await f.tick()
  const newValue = f.usage(2)
  f.pending[1].resolve(f.result('A', newValue))
  await replacement
  f.pending[0].resolve(f.result('A', f.usage(99)))
  await f.tick()
  assert.equal(f.api.usageEntries().get('A'), newValue)
  assert.equal(f.api.applyBalances(f.result('A', f.usage(100, Date.now() - 60_000))), 0)
  assert.equal(f.api.applyBalances({ at: Date.now() - 60_000, results: [{ id: 'A', error: 'old failure' }] }), 0)
  assert.equal(f.api.usageEntries().get('A'), newValue)
})

test('other providers retain pending null and not-configured error semantics', async () => {
  const f = await fixture([{ id: 'A', provider: 'catpaw' }])
  f.api.applyBalances(f.result('A', { balance: 2 }))
  const request = f.api.queryUsageFor('A')
  assert.equal(f.api.usageEntries().get('A'), null)
  await f.tick()
  f.pending[0].resolve({ results: [{ id: 'A', error: 'missing credential', code: 'usage_not_configured' }] })
  await request
  assert.deepEqual(f.api.usageFailureOf(f.api.usageEntries().get('A')), { message: 'missing credential', notConfigured: true })
})

test('latest manual response applies when server clock is five minutes behind or ahead', async () => {
  for (const offset of [-300_000, 300_000]) {
    const f = await fixture()
    const serverNow = Date.now() + offset
    f.api.applyBalances(f.result('A', f.usage(10, serverNow - 1_000)))
    const request = f.api.refreshCreditDetails('A', true)
    await f.tick()
    const latest = f.usage(4, serverNow)
    f.pending[0].resolve(f.result('A', latest))
    await request
    assert.equal(f.api.usageEntries().get('A'), latest)
    assert.equal(f.api.creditDetailsFresh('A'), true)
    assert.equal(f.api.creditDetailsFresh('A', Date.now() + 61_000), false)
    f.api.openCreditsDialog('A')
    await f.api.refreshCreditDetails('A')
    await f.tick()
    assert.deepEqual(f.calls, ['A'])
    assert.equal(f.api.applyBalances(f.result('A', f.usage(99, serverNow - 100))), 0)
    assert.equal(f.api.applyBalances({ at: serverNow - 100, results: [{ id: 'A', error: 'old failure' }] }), 0)
    assert.equal(f.api.applyBalances(f.result('A', f.usage(3, serverNow + 100))), 1)
    f.state.accounts.accounts = [{ id: 'A', uid: 'new identity', addedAt: 2 }]
    f.api.refreshCaches(new Set(['A']))
    const replacement = f.api.refreshCreditDetails('A', true)
    await f.tick()
    const replacedValue = f.usage(2, serverNow + 200)
    f.pending[1].resolve(f.result('A', replacedValue))
    await replacement
    assert.equal(f.api.usageEntries().get('A'), replacedValue)
  }
})

test('replacement blocks an old snapshot until current identity query succeeds', async () => {
  const f = await fixture()
  const oldValue = f.usage(99, Date.now() + 300_000)
  f.api.applyBalances(f.result('A', oldValue))
  f.state.accounts.accounts = [{ id: 'A', uid: 'replacement', addedAt: 2 }]
  f.api.refreshCaches(new Set(['A']))
  assert.equal(f.api.applyBalances(f.result('A', oldValue)), 0)
  assert.equal(f.api.lastSuccessfulUsage('A'), undefined)
  const failure = f.api.refreshCreditDetails('A', true)
  await f.tick()
  f.pending[0].reject(new Error('offline'))
  await failure
  assert.equal(f.api.applyBalances(f.result('A', oldValue)), 0)
  assert.match(f.api.usageFailureOf(f.api.usageEntries().get('A')).message, /offline/)
  const success = f.api.refreshCreditDetails('A', true)
  await f.tick()
  const current = f.usage(2, oldValue.creditDetails.fetchedAt + 1_000)
  f.pending[1].resolve(f.result('A', current))
  await success
  assert.equal(f.api.applyBalances(f.result('A', oldValue)), 0)
  assert.equal(f.api.applyBalances(f.result('A', f.usage(3, current.creditDetails.fetchedAt + 1_000))), 1)
})

test('delete and re-add the same id and identity cannot resurrect prior snapshot', async () => {
  const account = { id: 'A', uid: 'same uid', addedAt: 1 }
  const f = await fixture([account])
  f.api.refreshCaches(new Set(['A']))
  const oldValue = f.usage(99)
  f.api.applyBalances(f.result('A', oldValue))
  f.state.accounts.accounts = []
  f.api.refreshCaches(new Set())
  f.state.accounts.accounts = [account]
  f.api.refreshCaches(new Set(['A']))
  assert.equal(f.api.applyBalances(f.result('A', oldValue)), 0)
  assert.equal(f.api.usageEntries().get('A'), undefined)
  const request = f.api.refreshCreditDetails('A')
  await f.tick()
  f.pending[0].resolve(f.result('A', f.usage(2, oldValue.creditDetails.fetchedAt + 1_000)))
  await request
  assert.equal(f.api.lastSuccessfulUsage('A').creditDetails.remaining, 2)
})

test('snapshot freshness uses current server-to-row age regardless of clock offset', async () => {
  for (const offset of [-300_000, 300_000]) {
    const f = await fixture()
    const serverAt = Date.now() + offset
    f.api.applyBalances({ at: serverAt - 60_000, serverNow: serverAt, ...f.result('A', f.usage(99, serverAt - 61_000)) })
    assert.equal(f.api.creditDetailsFresh('A'), false)
    f.api.applyBalances({ at: serverAt, serverNow: serverAt, ...f.result('A', f.usage(98, serverAt - 1_000)) })
    assert.equal(f.api.creditDetailsFresh('A'), true)
    await f.api.refreshCreditDetails('A')
    assert.deepEqual(f.calls, [])
  }
})

test('legacy persisted snapshot without current server time stays stale until live refresh', async () => {
  const f = await fixture()
  const yesterday = Date.now() - 86_400_000
  f.api.applyBalances({ at: yesterday, ...f.result('A', f.usage(99, yesterday)) })
  assert.equal(f.api.creditDetailsFresh('A'), false)
  const request = f.api.refreshCreditDetails('A')
  await f.tick()
  assert.deepEqual(f.calls, ['A'])
  f.pending[0].resolve(f.result('A', f.usage(2, Date.now() - 300_000)))
  await request
  assert.equal(f.api.creditDetailsFresh('A'), true)
})

test('snapshot started before a manual query cannot override that query after it completes', async () => {
  const f = await fixture()
  let resolveSnapshot
  window.workbuddyDesktop.getBalancesSnapshot = () => new Promise(resolve => { resolveSnapshot = resolve })
  const snapshot = f.api.syncBalancesSnapshot()
  const request = f.api.refreshCreditDetails('A', true)
  await f.tick()
  const latest = f.usage(2, Date.now() - 300_000)
  f.pending[0].resolve(f.result('A', latest))
  await request
  // 轮次 at 比行内 fetchedAt 晚，也不能绕过捕获的请求代次。
  resolveSnapshot({ at: latest.creditDetails.fetchedAt + 1_000, results: [{ id: 'A', error: 'old failure' }] })
  assert.equal(await snapshot, false)
  assert.equal(f.api.usageEntries().get('A'), latest)
})

test('old equal-time successful snapshot cannot conceal a current manual failure', async () => {
  const f = await fixture()
  const previous = f.usage(8, Date.now() - 300_000)
  f.api.applyBalances(f.result('A', previous))
  const request = f.api.refreshCreditDetails('A', true)
  await f.tick()
  f.pending[0].reject(new Error('offline'))
  await request
  assert.equal(f.api.applyBalances(f.result('A', previous)), 0)
  assert.match(f.api.usageFailureOf(f.api.usageEntries().get('A')).message, /offline/)
  assert.equal(f.api.lastSuccessfulUsage('A'), previous)
})

test('slow B cannot promote an old A failure over newer manual A success', async () => {
  const f = await fixture([{ id: 'A' }, { id: 'B' }])
  const serverAt = Date.now() - 300_000
  const request = f.api.refreshCreditDetails('A', true)
  await f.tick()
  const latest = f.usage(2, serverAt + 100)
  f.pending[0].resolve({ results: [{ id: 'A', usage: latest, queriedAt: serverAt + 200 }] })
  await request
  const oldFailure = { id: 'A', error: 'old A failure', queriedAt: serverAt }
  assert.equal(f.api.applyBalances({ at: serverAt + 1_000, results: [oldFailure] }), 0)
  assert.equal(f.api.usageEntries().get('A'), latest)
  // 旧服务端没有行完成时间时同样不能靠较新的 batch.at 覆盖已有明细。
  assert.equal(f.api.applyBalances({ at: serverAt + 2_000, results: [{ id: 'A', error: 'legacy A failure' }] }), 0)
  // 真正晚于手动查询的新失败仍正常应用；同一失败重复读取不会再覆盖。
  const newFailure = { id: 'A', error: 'new A failure', queriedAt: serverAt + 300 }
  assert.equal(f.api.applyBalances({ at: serverAt + 3_000, results: [newFailure] }), 1)
  assert.equal(f.api.usageFailureOf(f.api.usageEntries().get('A')).message, 'new A failure')
  assert.equal(f.api.applyBalances({ at: serverAt + 4_000, results: [newFailure, oldFailure] }), 0)
  assert.equal(f.api.lastSuccessfulUsage('A'), latest)
})

test('manual business failure records queriedAt and legacy failure without prior detail still displays', async () => {
  const f = await fixture([{ id: 'A' }, { id: 'B' }, { id: 'C', provider: 'catpaw' }])
  const serverAt = Date.now() + 300_000
  f.api.applyBalances(f.result('A', f.usage(8, serverAt)))
  const request = f.api.refreshCreditDetails('A', true)
  await f.tick()
  f.pending[0].resolve({ results: [{ id: 'A', error: 'current manual failure', queriedAt: serverAt + 200 }] })
  await request
  assert.equal(f.api.applyBalances({ at: serverAt + 3_000, results: [{ id: 'A', error: 'older', queriedAt: serverAt + 100 }] }), 0)
  assert.equal(f.api.usageFailureOf(f.api.usageEntries().get('A')).message, 'current manual failure')
  assert.equal(f.api.applyBalances({ at: serverAt + 4_000, results: [{ id: 'B', error: 'legacy first failure' }] }), 1)
  assert.equal(f.api.usageFailureOf(f.api.usageEntries().get('B')).message, 'legacy first failure')
  assert.equal(f.api.applyBalances({ at: serverAt + 4_000, results: [{ id: 'C', error: 'other provider failure', queriedAt: serverAt - 1_000 }] }), 1)
  assert.equal(f.api.usageFailureOf(f.api.usageEntries().get('C')).message, 'other provider failure')
})
