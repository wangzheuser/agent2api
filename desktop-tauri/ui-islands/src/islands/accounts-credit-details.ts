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

export function formatCreditAmount(value: number | null): string {
  if (value === null || !Number.isFinite(value)) return '未知'
  return value.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 8 })
}
