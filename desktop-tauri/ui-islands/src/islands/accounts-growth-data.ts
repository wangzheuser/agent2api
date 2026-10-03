import type { AccountRecord } from './accounts-shared'

export type Rewards = { credits: number | null; energy: number | null; buddy: number | null; lotteryChances: number | null; makeupCards: number | null }
export type GrowthTask = {
  code: string; title: string; source: string; state: string; current: number | null; target: number | null
  accepted: boolean; claimed: boolean; expiresAt: number | null; rewards: Rewards
  canAccept: boolean; canClaim: boolean; canExecute: boolean; execution: string; reason: string | null; actionUrl: string | null
}
export type GrowthLocation = { id: string; name: string; enabled: boolean; durationSeconds: number | null; durationSecondsMin?: number | null; durationSecondsMax?: number | null; rewardCreditsMin?: number | null; rewardCreditsMax?: number | null }
export type GrowthState = {
  schemaVersion: number; id: string; identity: string; fetchedAt: number; supported: boolean; reason: string | null; edition: string
  capabilities: string[]; tasks: GrowthTask[]
  travel: null | { state: string; recordId: string | null; buddyId: string | null; arrivesAt: number | null; completedToday: number | null; dailyLimit: number | null; canAdopt: boolean; agreementRequired: boolean; canDepart: boolean; canClaim: boolean; locations: GrowthLocation[]; rewards: Rewards; agreementUrl?: string | null }
  streak: null | { days: number | null; makeupCards: number | null; missedDates: string[]; timezone: string; tiers: { id: string; requiredDays: number; claimable: boolean; claimed: boolean; rewards: Rewards }[] }
  lottery: null | { chances: number | null; canDraw: boolean }
  activities: { code: string; title: string; state: string; canClaim: boolean; canAttempt?: boolean; reason: string | null; actionUrl?: string | null }[]
  errors: { section: string; code: string; message: string }[]; running: boolean; lastRun: GrowthResult | null
  pending?: { clientToken: string; request: GrowthAction } | null
}
export type GrowthResult = {
  id: string; action: string; at: number; status: string; message: string; rewards: Rewards
  balanceBefore: number | null; balanceAfter: number | null; balanceDelta: number | null
  receiptConfirmed: boolean; stateConfirmed: boolean; items: GrowthResult[]; state?: GrowthState | null
}
export type GrowthAction = { id: string; action: string; expectedIdentity?: string; taskCode?: string; source?: string; tier?: string; date?: string; locationId?: string; agreementAccepted?: boolean; clientToken?: string }
export function growthActionKey(request: GrowthAction): string {
  return JSON.stringify(Object.fromEntries(Object.entries(request).filter(([key]) => key !== 'clientToken').sort(([a], [b]) => a.localeCompare(b))))
}
export function growthClientToken(): string {
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, '0')).join('')
}
export type WorkBuddyPolicy = {
  schemaVersion: number; id: string; identity: string; autoGrowth: boolean; autoTravel: boolean; creditFloor: number | null
  selection: 'priority' | 'expiry' | 'cost'; selectionScope: 'provider'; balanceMaxAgeSeconds: number
  cost: { lastCredits: number | null; lastAt: number | null; samples: number; models: { model: string; creditsPer1kTokens: number | null; samples: number; lastAt: number | null }[] }
}
export type PolicyPatch = Pick<WorkBuddyPolicy, 'autoGrowth' | 'autoTravel' | 'creditFloor' | 'selection'> & { expectedIdentity: string }
type GrowthBridge = {
  getWorkBuddyGrowth(id: string): Promise<GrowthState>
  workBuddyGrowthAction(action: GrowthAction): Promise<GrowthResult>
  getWorkBuddyPolicy(id: string): Promise<WorkBuddyPolicy>
  updateWorkBuddyPolicy(id: string, patch: PolicyPatch): Promise<WorkBuddyPolicy>
}
export function growthApi(): GrowthBridge {
  const bridge = (window as unknown as { workbuddyDesktop?: GrowthBridge }).workbuddyDesktop
  if (!bridge?.getWorkBuddyGrowth) throw new Error('当前服务未提供成长福利接口，请更新后重试。')
  return bridge
}

export function growthAccountKey(account: AccountRecord): string {
  return JSON.stringify([account.id, account.provider || 'workbuddy', account.edition || 'cn', account.uid,
    account.tenantId, account.tenant_id, account.tenant, account.enterpriseId, account.type, account.accountType, account.addedAt, account.tokenTail])
}
export function supportsGrowth(account: AccountRecord): boolean {
  return (account.provider ?? 'workbuddy') === 'workbuddy' && (account.edition ?? 'cn') === 'cn' &&
    ['tenantId', 'tenant_id', 'tenant', 'enterpriseId'].every(key => account[key] == null || account[key] === '') &&
    ['type', 'accountType'].every(key => account[key] == null || account[key] === '' || account[key] === 'personal')
}
// 未提供 UID 的记录分别保留；有 UID 时按地区和租户去重，服务端仍会再次互斥。
export function uniqueGrowthAccounts(accounts: AccountRecord[]): AccountRecord[] {
  const seen = new Set<string>()
  return accounts.filter(account => {
    if (!supportsGrowth(account)) return false
    const key = JSON.stringify([account.edition || 'cn', account.uid || `local:${account.id}`, account.tenantId || account.tenant_id || account.tenant || ''])
    if (seen.has(key)) return false
    seen.add(key); return true
  })
}

export function finiteAmount(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null
}
export function amountText(value: unknown, missing = '未知'): string {
  const number = finiteAmount(value)
  return number === null ? missing : number.toLocaleString('zh-CN', { maximumFractionDigits: 6 })
}
export function rewardsText(rewards: Rewards | null | undefined): string {
  const fields: [keyof Rewards, string][] = [['credits', '积分'], ['energy', '能量'], ['buddy', '猫猫'], ['lotteryChances', '抽奖次数'], ['makeupCards', '补签卡']]
  return fields.filter(([key]) => key === 'credits' || finiteAmount(rewards?.[key]) !== null)
    .map(([key, label]) => `${label} ${amountText(rewards?.[key])}`).join(' · ')
}
export const resultLabels: Record<string, string> = { claimed: '已领取', already_claimed: '此前已领取', completed: '动作已完成', pending: '等待计入', manual_required: '需官方操作', not_applicable: '当前不适用', failed: '失败', uncertain: '结果待核实', busy: '账号处理中' }
export const taskLabels: Record<string, string> = { available: '待接取', active: '进行中', claimable: '可领取', claimed: '已领取', locked: '未解锁', expired: '已结束', unknown: '状态待确认' }
export const travelLabels: Record<string, string> = { no_buddy: '尚未领养', idle: '等待出发', traveling: '旅行中', arrived: '已归来，可领奖', daily_limit: '今日旅行已完成', unknown: '状态待确认' }
export function growthTodo(state: GrowthState): number {
  return state.tasks.filter(task => task.canClaim).length + (state.streak?.tiers.filter(tier => tier.claimable).length || 0) + (state.travel?.canClaim ? 1 : 0)
}
export function growthTaskProgressed(before: GrowthTask | undefined, state: GrowthState): boolean {
  return !state.pending && finiteAmount(before?.current) !== null && state.tasks.some(task => task.code === before?.code && task.source === before?.source && finiteAmount(task.current) !== null && task.current! > before!.current!)
}
export function checkedState(value: GrowthState, id: string): GrowthState {
  if (!value || value.schemaVersion !== 1 || value.id !== id || !value.identity || !Array.isArray(value.tasks) || !Array.isArray(value.errors)) throw new Error('成长福利响应不完整，请刷新核对。')
  return value
}
// 停止只影响未派发账号；已发动作继续完成，不能把取消前端等待说成撤销领奖。
export async function runGrowthBatch<T>(items: T[], signal: AbortSignal, work: (item: T) => Promise<void>): Promise<void> {
  let index = 0
  async function worker() {
    while (!signal.aborted && index < items.length) {
      const item = items[index++]
      await work(item)
    }
  }
  await Promise.all(Array.from({ length: Math.min(3, items.length) }, worker))
}
