import assert from 'node:assert/strict'
import { test } from 'node:test'
import { build } from 'esbuild'
import { fileURLToPath } from 'node:url'

const bundle = await build({ entryPoints: [fileURLToPath(new URL('../src/islands/accounts-growth-data.ts', import.meta.url))], bundle: true, format: 'esm', platform: 'node', write: false })
const { amountText, rewardsText, uniqueGrowthAccounts, growthAccountKey, growthActionKey, growthTaskProgressed, checkedState, runGrowthBatch } = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`)

test('reward and actual-credit display preserve zero, fractions and missing reports', () => {
  assert.equal(amountText(0, '未上报'), '0')
  assert.equal(amountText(.012345), '0.012345')
  for (const value of [null, undefined, NaN, Infinity, -1, '0']) assert.equal(amountText(value, '未上报'), '未上报')
  assert.equal(rewardsText({ credits: 0, energy: 0, buddy: null, lotteryChances: 1 }), '积分 0 · 能量 0 · 抽奖次数 1')
  assert.equal(rewardsText({ credits: null, energy: 2 }), '积分 未知 · 能量 2')
})

test('domestic batch isolates enterprise, international and duplicate identities without losing unknown UID records', () => {
  const accounts = [{ id: 'A', uid: 'same' }, { id: 'B', uid: 'same' }, { id: 'C', uid: 'same', edition: 'intl' }, { id: 'D', uid: 'same', tenantId: 'org' }, { id: 'E' }, { id: 'F' }, { id: 'G', provider: 'catpaw' }, { id: 'H', enterpriseId: 'org' }, { id: 'I', type: 'enterprise' }, { id: 'J', type: 'ultimate' }, { id: 'K', tenant: 0 }, { id: 'L', edition: 'unknown' }]
  assert.deepEqual(uniqueGrowthAccounts(accounts).map(a => a.id), ['A', 'E', 'F'])
  assert.notEqual(growthAccountKey(accounts[0]), growthAccountKey({ ...accounts[0], uid: 'changed' }))
  assert.notEqual(growthAccountKey(accounts[0]), growthAccountKey({ ...accounts[0], tenantId: 'other' }))
  assert.notEqual(growthAccountKey(accounts[0]), growthAccountKey({ ...accounts[0], enterpriseId: 'other' }))
  assert.throws(() => checkedState({ schemaVersion: 1, id: 'B', identity: 'old', tasks: [], errors: [] }, 'A'))
  assert.equal(growthActionKey({ id: 'A', action: 'makeup', date: '2026-10-01', clientToken: 'original' }), growthActionKey({ date: '2026-10-01', action: 'makeup', id: 'A', clientToken: 'replayed' }))
  assert.notEqual(growthActionKey({ id: 'A', action: 'makeup', date: '2026-10-01' }), growthActionKey({ id: 'A', action: 'makeup', date: '2026-10-02' }))
})

test('batch never exceeds three active accounts and cancellation stops undispatched work', async () => {
  const controller = new AbortController(), started = [], release = []
  let active = 0, maximum = 0, completed = 0
  const running = runGrowthBatch([1, 2, 3, 4, 5, 6], controller.signal, async item => {
    started.push(item); active++; maximum = Math.max(active, maximum)
    await new Promise(resolve => release.push(resolve))
    active--; completed++
  })
  assert.deepEqual(started, [1, 2, 3]); assert.equal(maximum, 3)
  controller.abort(); release.forEach(resolve => resolve())
  await running
  assert.equal(completed, 3); assert.deepEqual(started, [1, 2, 3])
})

test('a new task execution requires observed progress and no unresolved server operation', () => {
  const before = { code: 'chat_5', source: 'growth', current: 0 }
  assert.equal(growthTaskProgressed(before, { tasks: [{ ...before, current: 1 }] }), true)
  for (const next of [{ ...before }, { ...before, current: null }, { ...before, current: 1, source: 'mini_program' }]) assert.equal(growthTaskProgressed(before, { tasks: [next] }), false)
  assert.equal(growthTaskProgressed(before, { tasks: [{ ...before, current: 1 }], pending: { clientToken: 'still-pending' } }), false)
  assert.equal(growthTaskProgressed(undefined, { tasks: [{ ...before, current: 1 }] }), false)
})
