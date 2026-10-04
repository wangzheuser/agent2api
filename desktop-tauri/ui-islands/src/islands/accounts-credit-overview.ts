import type { AccountRecord } from './accounts-shared'
import type { GenericBalanceDetails, WorkBuddyCreditDetails } from './accounts-credit-details'
import { accountEdition, byPriorityOrder, providerOf, supportsUsage } from './accounts-domain'

const DAY = 86400000
export const CREDIT_BUCKETS = [
  { id: 'day', label: '24 小时内', tone: 'urgent' },
  { id: 'week', label: '1–7 天', tone: 'soon' },
  { id: 'month', label: '7–30 天', tone: 'normal' },
  { id: 'later', label: '30 天以上', tone: 'normal' },
  { id: 'never', label: '无到期限制', tone: 'normal' },
  { id: 'unknown', label: '到期未知', tone: 'unknown' },
  { id: 'timezone', label: '时区待确认', tone: 'unknown' },
] as const
export type CreditBucket = typeof CREDIT_BUCKETS[number]['id']
export type CreditScope = 'all' | 'filtered' | 'selected'
export type CreditSample = {
  account: AccountRecord
  details: WorkBuddyCreditDetails | null
  generic: GenericBalanceDetails | null
  fresh: boolean
  failed: boolean
  /** 同源服务端时钟；旧快照缺少时间锚点时为 null。 */
  asOf: number | null
}
export type CreditOverviewRow = CreditSample & {
  buckets: Record<CreditBucket, number>
  amount: number | null
  nearest: number | null
  expired: number
  partial: boolean
  duplicate: boolean
  mode: 'precise' | 'generic'
  unit: string | null
}

export function supportsCreditOverview(account: AccountRecord | null | undefined): boolean {
  return supportsUsage(account)
}

const emptyBuckets = (): Record<CreditBucket, number> => ({ day: 0, week: 0, month: 0, later: 0, never: 0, unknown: 0, timezone: 0 })

export function creditScopeAccounts(accounts: AccountRecord[], scope: CreditScope, visible: ReadonlySet<string>, selected: ReadonlySet<string>): AccountRecord[] {
  return accounts.filter(account => supportsCreditOverview(account) &&
    (scope === 'all' || (scope === 'filtered' ? visible : selected).has(account.id)))
}

function overviewRow(sample: CreditSample): CreditOverviewRow {
  const row: CreditOverviewRow = {
    ...sample, buckets: emptyBuckets(), amount: null, nearest: null, expired: 0,
    partial: false, duplicate: false, mode: sample.generic || !sample.details ? 'generic' : 'precise',
    unit: sample.generic?.unit || null,
  }
  if (sample.generic) {
    row.amount = sample.generic.available
    row.partial = !sample.generic.complete
    row.nearest = [sample.generic.subscription?.expireAt, sample.generic.subscription?.resetAt,
      ...sample.generic.wallets.map(wallet => wallet.expiresAt)]
      .filter((value): value is number => typeof value === 'number' && Number.isFinite(value) && value > 0)
      .reduce((nearest, value) => Math.min(nearest, value), Infinity)
    if (!Number.isFinite(row.nearest)) row.nearest = null
    return row
  }
  const details = sample.details
  // 企业可能共用额度池；只逐账号展示，不加入个人积分总数或到期分布。
  if (!details || details.kind === 'enterprise' || details.unlimited) return row
  const active = details.segments.filter(segment => !['expired', 'not_started'].includes(segment.state))
  const sum = active.reduce((total, segment) => total + (segment.remaining ?? 0), 0)
  row.partial = !details.complete || details.remaining === null || !Number.isFinite(sum) ||
    details.issues.some(issue => ['truncated', 'invalid_amount', 'balance_mismatch'].includes(issue)) ||
    active.some(segment => segment.remaining === null ||
      (segment.state !== 'active' && segment.remaining > 0) ||
      (segment.total !== null && segment.remaining !== null && segment.remaining > segment.total + 0.01)) ||
    (details.remaining !== null && Math.abs(sum - details.remaining) > 0.01)
  if (row.partial) return row
  for (const segment of active) {
    const amount = segment.remaining || 0
    if (!amount) continue
    let bucket: CreditBucket = 'unknown'
    if (segment.expiryStatus === 'never') bucket = 'never'
    else if (segment.expiryStatus === 'timezone_unverified') bucket = 'timezone'
    else if (segment.expiryStatus === 'known' && segment.expiresAt !== null && sample.asOf !== null) {
      const left = segment.expiresAt - sample.asOf
      if (left <= 0) { row.expired += amount; continue }
      bucket = left <= DAY ? 'day' : left <= 7 * DAY ? 'week' : left <= 30 * DAY ? 'month' : 'later'
      row.nearest = Math.min(row.nearest ?? Infinity, segment.expiresAt)
    }
    row.buckets[bucket] += amount
  }
  // unattributedRemaining 已包含在后端生成的未知周期段中，不能再次相加。
  row.amount = Object.values(row.buckets).reduce((total, amount) => total + amount, 0)
  return row
}

export function summarizeCreditOverview(samples: CreditSample[]) {
  const rows = samples.filter(sample => supportsCreditOverview(sample.account)).map(overviewRow)
  const identities = new Map<string, CreditOverviewRow>()
  for (const row of rows) {
    const provider = providerOf(row.account)
    const uid = [row.account.uid, row.account.userId].find(value => typeof value === 'string' && value) as string | undefined
    const key = JSON.stringify([provider, accountEdition(row.account), uid ? 'uid' : 'id', uid || row.account.id])
    const previous = identities.get(key)
    if (!previous) { identities.set(key, row); continue }
    const newer = (row.details?.fetchedAt ?? row.generic?.fetchedAt ?? 0) -
      (previous.details?.fetchedAt ?? previous.generic?.fetchedAt ?? 0)
    if (newer > 0 || (newer === 0 && row.fresh && !previous.fresh)) {
      previous.duplicate = true
      identities.set(key, row)
    } else row.duplicate = true
  }
  const groups = new Map<string, CreditOverviewRow[]>()
  for (const row of rows) {
    const key = `${providerOf(row.account)}:${accountEdition(row.account)}:${row.mode}:${row.unit || ''}`
    const group = groups.get(key)
    if (group) group.push(row)
    else groups.set(key, [row])
  }
  return [...groups.entries()].sort(([a], [b]) => {
    const [providerA, editionA] = a.split(':')
    const [providerB, editionB] = b.split(':')
    return (providerA === providerB ? 0 : providerA === 'workbuddy' ? -1 : 1) || (editionA === editionB ? 0 : editionA === 'cn' ? -1 : 1)
  }).map(([key, groupRows]) => {
    const [provider, edition, mode, unit] = key.split(':') as [string, 'cn' | 'intl', 'precise' | 'generic', string]
    const included = groupRows.filter(row => !row.duplicate && row.amount !== null)
    const buckets = emptyBuckets()
    for (const row of included) for (const { id } of CREDIT_BUCKETS) buckets[id] += row.buckets[id]
    const amount = included.reduce((total, row) => total + row.amount!, 0)
    return {
      provider, edition, mode, unit: unit || null, rows: groupRows, buckets,
      amount: included.length && Number.isFinite(amount) ? amount : null,
      included: included.length,
      stale: included.filter(row => !row.fresh || row.failed || row.expired > 0).length,
      missing: groupRows.filter(row => !row.details && !row.generic && !row.duplicate).length,
      partial: groupRows.filter(row => row.partial && !row.duplicate).length,
      duplicates: groupRows.filter(row => row.duplicate).length,
      enterprise: groupRows.filter(row => row.details?.kind === 'enterprise' && !row.duplicate).length,
      unlimited: groupRows.filter(row => row.details?.unlimited && !row.duplicate).length,
    }
  }).filter(group => group.rows.length > 0)
}

/** 仅供总览列表展示排序，不修改账号原始数组或转发 priority。 */
export function sortCreditOverviewRows(rows: CreditOverviewRow[], byExpiry: boolean): CreditOverviewRow[] {
  return rows.slice().sort((a, b) =>
    (byExpiry ? (a.nearest ?? Infinity) - (b.nearest ?? Infinity) : 0) || byPriorityOrder(a.account, b.account))
}
