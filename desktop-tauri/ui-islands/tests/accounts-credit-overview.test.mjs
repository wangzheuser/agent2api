import assert from 'node:assert/strict'
import { test } from 'node:test'
import { build } from 'esbuild'
import { fileURLToPath } from 'node:url'

const bundled = await build({ entryPoints: [fileURLToPath(new URL('../src/islands/accounts-credit-overview.ts', import.meta.url))], bundle: true, format: 'esm', platform: 'node', write: false })
const { summarizeCreditOverview, creditScopeAccounts, sortCreditOverviewRows } = await import(`data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString('base64')}`)
const now = 1791000000000, day = 86400000
const segment = (id, remaining, delay = day, extra = {}) => ({ id, resourceId: id, packageCode: 'monthly', name: id, remaining, total: remaining,
  expiresAt: now + delay, expiresAtText: null, expiryStatus: 'known', entitlementEndsAt: null, state: 'active', ...extra })
const sample = (id, segments = [segment('pack', 10)], extra = {}) => ({
  account: { id, uid: id, provider: 'workbuddy', edition: 'cn' }, fresh: true, failed: false, asOf: now,
  details: { version: 1, kind: 'personal', fetchedAt: now, complete: true, unlimited: false, remaining: segments.reduce((n, s) => n + (s.remaining || 0), 0), unattributedRemaining: 0, issues: [], segments }, ...extra,
})
const first = rows => summarizeCreditOverview(rows)[0]

test('domestic and international totals remain separate even for the same UID', () => {
  const cn = sample('A'), intl = sample('B', [segment('pack', 25)])
  intl.account.edition = 'intl'; intl.account.uid = cn.account.uid
  const groups = summarizeCreditOverview([cn, intl])
  assert.deepEqual(groups.map(g => [g.edition, g.amount, g.duplicates]), [['cn', 10, 0], ['intl', 25, 0]])
})
test('buckets are disjoint at exact 24h, 7d and 30d boundaries and preserve fractions', () => {
  const result = first([sample('A', [segment('a', .1), segment('b', .2, day + 1), segment('c', .3, 7 * day), segment('d', .4, 7 * day + 1), segment('e', .5, 30 * day), segment('f', .6, 30 * day + 1)])])
  assert.equal(result.buckets.day, .1); assert.equal(result.buckets.week, .5)
  assert.equal(result.buckets.month, .9); assert.equal(result.buckets.later, .6)
  assert.ok(Math.abs(result.amount - 2.1) < 1e-8)
})
test('unattributed amount is already in segments and must never be added twice', () => {
  const row = sample('A', [segment('known', 70), segment('unattributed', 30, 0, { expiryStatus: 'unknown', expiresAt: null })])
  row.details.unattributedRemaining = 30
  const result = first([row])
  assert.equal(result.amount, 100); assert.equal(result.buckets.unknown, 30); assert.equal(result.buckets.day, 70)
})
test('unknown, unverified timezone and explicitly never expire have distinct buckets', () => {
  const result = first([sample('A', [segment('a', 10, 0, { expiryStatus: 'unknown', expiresAt: null }), segment('b', 20, 0, { expiryStatus: 'timezone_unverified', expiresAt: null }), segment('c', 30, 0, { expiryStatus: 'never', expiresAt: null })])])
  assert.equal(result.buckets.unknown, 10); assert.equal(result.buckets.timezone, 20); assert.equal(result.buckets.never, 30)
  assert.equal(result.buckets.day, 0); assert.equal(result.rows[0].nearest, null)
})
test('cached active segments reaching expiry are excluded including the exact boundary', () => {
  const result = first([sample('A', [segment('expired', 20, 0), segment('valid', 30, day)])])
  assert.equal(result.amount, 30); assert.equal(result.rows[0].expired, 20); assert.equal(result.stale, 1)
})
test('expired, future and exhausted segments never inflate the current total', () => {
  const row = sample('A', [segment('expired', 40, -day, { state: 'expired' }), segment('future', 50, day, { state: 'not_started' }), segment('empty', 0, day, { state: 'exhausted' }), segment('current', 10)])
  row.details.remaining = 10
  assert.equal(first([row]).amount, 10)
})
test('missing data is unknown while a verified zero remains zero', () => {
  assert.equal(first([sample('A', [], { details: null })]).amount, null)
  assert.equal(first([sample('A', [])]).amount, 0)
})
test('partial data and balance mismatch remain visible but are excluded from aggregation', () => {
  const partial = sample('A'), mismatch = sample('B'), unknown = sample('C')
  partial.details.complete = false; mismatch.details.remaining = 100
  unknown.details.segments[0].remaining = null
  const result = first([partial, mismatch, unknown, sample('valid')])
  assert.equal(result.amount, 10); assert.equal(result.partial, 3); assert.equal(result.rows.length, 4)
})
test('unknown positive states and amounts above package total require review', () => {
  for (const change of [{ state: 'unknown' }, { total: 1 }]) {
    const result = first([sample('A', [segment('x', 10, day, change)])])
    assert.equal(result.amount, null); assert.equal(result.partial, 1)
  }
})
test('enterprise shared limits and unlimited accounts never enter personal totals', () => {
  const a = sample('enterprise-a'), b = sample('enterprise-b'), unlimited = sample('unlimited')
  a.details.kind = b.details.kind = 'enterprise'; unlimited.details.unlimited = true
  const result = first([a, b, unlimited, sample('personal')])
  assert.equal(result.amount, 10); assert.equal(result.enterprise, 2); assert.equal(result.unlimited, 1)
})
test('duplicate identity chooses the newest snapshot once, independent of row order', () => {
  const older = sample('A'), newer = sample('B', [segment('x', 25)])
  newer.account.uid = 'A'; newer.details.fetchedAt += 10
  for (const rows of [[older, newer], [newer, older]]) {
    const result = first(rows)
    assert.equal(result.amount, 25); assert.equal(result.duplicates, 1)
    assert.equal(result.rows.find(r => r.account.id === 'A').duplicate, true)
  }
})
test('newer incomplete identity prevents an older success from appearing authoritative', () => {
  const older = sample('A'), newer = sample('B'); newer.account.uid = 'A'
  newer.details.fetchedAt++; newer.details.complete = false
  const result = first([older, newer])
  assert.equal(result.amount, null); assert.equal(result.partial, 1)
})
test('missing UID keeps distinct local records separate', () => {
  const a = sample('A'), b = sample('B'); delete a.account.uid; delete b.account.uid
  assert.equal(first([a, b]).amount, 20)
})
test('stale and failed snapshots are counted with explicit stale coverage', () => {
  const result = first([sample('A', undefined, { fresh: false, failed: true })])
  assert.equal(result.amount, 10); assert.equal(result.stale, 1)
})
test('a legacy snapshot without a server clock cannot manufacture an expiry forecast', () => {
  const result = first([sample('A', undefined, { asOf: null, fresh: false })])
  assert.equal(result.amount, 10); assert.equal(result.buckets.unknown, 10); assert.equal(result.buckets.day, 0)
})
test('scope filters include selected hidden and disabled accounts but no other providers', () => {
  const accounts = [{ id: 'A' }, { id: 'B', enabled: false }, { id: 'C', provider: 'catpaw' }]
  const visible = new Set(['A']), selected = new Set(['B', 'C'])
  assert.deepEqual(creditScopeAccounts(accounts, 'all', visible, selected).map(a => a.id), ['A', 'B'])
  assert.deepEqual(creditScopeAccounts(accounts, 'filtered', visible, selected).map(a => a.id), ['A'])
  assert.deepEqual(creditScopeAccounts(accounts, 'selected', visible, selected).map(a => a.id), ['B'])
})
test('display sorting does not mutate the source order or account priority', () => {
  const a = sample('A', [segment('a', 10, 3 * day)]), b = sample('B')
  a.account.priority = 1; b.account.priority = 2
  const rows = first([a, b]).rows
  assert.deepEqual(sortCreditOverviewRows(rows, true).map(r => r.account.id), ['B', 'A'])
  assert.deepEqual(rows.map(r => [r.account.id, r.account.priority]), [['A', 1], ['B', 2]])
  assert.deepEqual(sortCreditOverviewRows(rows, false).map(r => r.account.id), ['A', 'B'])
  assert.deepEqual(sortCreditOverviewRows(rows.slice().reverse(), false).map(r => r.account.id), ['A', 'B'])
})
