import type { UsageEntry } from './accounts-shared'

export type WorkBuddyCreditSegment = {
  id: string
  resourceId: string | null
  packageCode: string
  name: string
  remaining: number | null
  total: number | null
  expiresAt: number | null
  expiresAtText: string | null
  expiryStatus: 'known' | 'unknown' | 'timezone_unverified' | 'never'
  entitlementEndsAt: number | null
  state: 'active' | 'exhausted' | 'expired' | 'not_started' | 'unknown'
}

export type WorkBuddyCreditDetails = {
  version: 1
  kind: 'personal' | 'enterprise'
  fetchedAt: number
  complete: boolean
  remaining: number | null
  unlimited: boolean
  unattributedRemaining: number | null
  issues: string[]
  segments: WorkBuddyCreditSegment[]
}

export type GenericBalanceWallet = {
  id: string
  name: string
  balance: number | null
  total: number | null
  remainingPercent: number | null
  expiresAt: number | null
  detail: string | null
  display: string | null
}

export type GenericBalanceSubscription = {
  planName: string | null
  expireAt: number | null
  resetAt: number | null
  status: string | null
}

/**
 * 非 WorkBuddy/Qoder 的统一余额形状。它只承载上游明确返回的钱包和订阅，
 * 不把钱包猜成积分包，因此总览不会伪造逐包到期合计。
 */
export type GenericBalanceDetails = {
  kind: 'generic'
  fetchedAt: number
  complete: boolean
  available: number | null
  unit: string
  wallets: GenericBalanceWallet[]
  subscription: GenericBalanceSubscription | null
  issues: string[]
}

const amountOrNull = (value: unknown): boolean => value === null ||
  (typeof value === 'number' && Number.isFinite(value) && value >= 0)
const timeOrNull = (value: unknown): boolean => value === null ||
  (typeof value === 'number' && Number.isFinite(value) && value > 0 && value <= 8640000000000000)

/** 旧快照与旧服务器没有此字段；畸形明细不参与金额和比例计算。 */
export function creditDetailsOf(entry: UsageEntry): WorkBuddyCreditDetails | null {
  if (!entry || typeof entry !== 'object' || entry.error) return null
  const value = entry.creditDetails
  if (!value || typeof value !== 'object') return null
  const details = value as WorkBuddyCreditDetails
  if (details.version !== 1 || !['personal', 'enterprise'].includes(details.kind) ||
    !timeOrNull(details.fetchedAt) || details.fetchedAt === null ||
    typeof details.complete !== 'boolean' || typeof details.unlimited !== 'boolean' ||
    !amountOrNull(details.remaining) || !amountOrNull(details.unattributedRemaining) ||
    !Array.isArray(details.issues) || !details.issues.every(issue => typeof issue === 'string') ||
    !Array.isArray(details.segments)) return null
  const ids = new Set<string>()
  for (const segment of details.segments) {
    if (!segment || typeof segment !== 'object' || typeof segment.id !== 'string' || !segment.id || ids.has(segment.id) ||
      (segment.resourceId !== null && typeof segment.resourceId !== 'string') ||
      typeof segment.name !== 'string' || typeof segment.packageCode !== 'string' ||
      !amountOrNull(segment.remaining) || !amountOrNull(segment.total) ||
      !timeOrNull(segment.expiresAt) || !timeOrNull(segment.entitlementEndsAt) ||
      (segment.expiresAtText !== null && typeof segment.expiresAtText !== 'string') ||
      !['known', 'unknown', 'timezone_unverified', 'never'].includes(segment.expiryStatus) ||
      !['active', 'exhausted', 'expired', 'not_started', 'unknown'].includes(segment.state) ||
      (segment.expiryStatus === 'known' && segment.expiresAt === null)) return null
    ids.add(segment.id)
  }
  return details
}

function numberOrNull(value: unknown): number | null {
  if (value === null || value === undefined || value === '') return null
  const number = typeof value === 'number' ? value : Number(value)
  return Number.isFinite(number) ? number : null
}

function genericTimeOrNull(value: unknown): number | null {
  if (typeof value === 'string' && value.trim() && !/^\d+(?:\.\d+)?$/.test(value.trim())) {
    const parsed = Date.parse(value)
    return Number.isFinite(parsed) && parsed > 0 ? parsed : null
  }
  const number = numberOrNull(value)
  if (number === null || number <= 0) return null
  return number < 1e11 ? number * 1000 : number
}

function objectOf(value: unknown): Record<string, unknown> | null {
  return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : null
}

function firstValue(object: Record<string, unknown>, keys: string[]): unknown {
  return keys.map(key => object[key]).find(value => value !== undefined && value !== null && value !== '')
}

/** 将已有 provider 的 `{available, wallets, subscription}` 读数归一为弹窗/总览模型。 */
export function genericBalanceDetailsOf(entry: UsageEntry, fetchedAt = Date.now()): GenericBalanceDetails | null {
  if (!entry || typeof entry !== 'object' || entry.error || creditDetailsOf(entry)) return null
  const value = entry as Record<string, unknown>
  const walletsValue = Array.isArray(value.wallets) ? value.wallets : []
  const subscriptionValue = objectOf(value.subscription)
  if (!Object.prototype.hasOwnProperty.call(value, 'available') && !walletsValue.length && !subscriptionValue) return null
  const wallets = walletsValue.map((item, index) => {
    const wallet = objectOf(item) || {}
    const balance = numberOrNull(firstValue(wallet, ['balance', 'remaining', 'creditRemaining']))
    const total = numberOrNull(firstValue(wallet, ['total', 'allowanceTokens', 'creditTotal']))
    const usedPercent = numberOrNull(wallet.usedPercent)
    const remainingPercent = numberOrNull(wallet.remainingPercent) ??
      (usedPercent === null ? null : Math.max(0, Math.min(100, 100 - usedPercent)))
    return {
      id: String(firstValue(wallet, ['id', 'type']) || `wallet-${index + 1}`),
      name: String(firstValue(wallet, ['displayName', 'name', 'type']) || '余额'),
      balance,
      total,
      remainingPercent,
      expiresAt: genericTimeOrNull(firstValue(wallet, ['expireAt', 'expiresAt', 'resetAt', 'endTime'])),
      detail: typeof wallet.detail === 'string' ? wallet.detail : null,
      display: typeof wallet.balanceView === 'string' && wallet.balanceView ? wallet.balanceView : null,
    }
  })
  const subscription = subscriptionValue ? {
    planName: typeof firstValue(subscriptionValue, ['planName', 'name']) === 'string'
      ? String(firstValue(subscriptionValue, ['planName', 'name'])) : null,
    expireAt: genericTimeOrNull(firstValue(subscriptionValue, ['expireAt', 'expiresAt', 'endTime', 'resetDate'])),
    resetAt: genericTimeOrNull(firstValue(subscriptionValue, ['resetAt', 'resetDate'])),
    status: typeof subscriptionValue.status === 'string' ? subscriptionValue.status : null,
  } : null
  const issues = ['statisticsError', 'benefitError', 'subscriptionError']
    .filter(key => value[key])
    .map(key => `${key}: ${String(value[key])}`)
  const available = numberOrNull(value.available)
  return {
    kind: 'generic', fetchedAt: Number.isFinite(fetchedAt) && fetchedAt > 0 ? fetchedAt : Date.now(),
    complete: issues.length === 0 && (available !== null || wallets.length > 0), available,
    unit: String(value.unit || '余额'), wallets, subscription, issues,
  }
}

export function formatCreditAmount(value: number | null): string {
  if (value === null || !Number.isFinite(value)) return '未知'
  return value.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 8 })
}
