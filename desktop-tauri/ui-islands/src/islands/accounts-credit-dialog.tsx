import * as React from 'react'
import {
  Button, Dialog, DialogBody, DialogContent, DialogDescription, DialogFooter,
  DialogHeader, DialogTitle, SegmentedControl, Tooltip, TooltipContent, TooltipProvider, TooltipTrigger,
} from '@ui'
import {
  creditDetailsFresh, findAccount, getStore, lastSuccessfulUsage, maskName, refreshCreditDetails,
  subscribe, usageEntries, usageFailureOf,
} from './accounts-data'
import { displayNameOf, editionSuffix, providerOf } from './accounts-domain'
import { creditDetailsOf, formatCreditAmount, type WorkBuddyCreditSegment } from './accounts-credit-details'
import { growthAccountKey, supportsGrowth } from './accounts-growth-data'
import { AccountsGrowthPanel, WorkBuddyPolicyPanel } from './accounts-growth-panel'

const HOUR = 3600000
const issueLabels: Record<string, string> = {
  truncated: '仅取得部分资源包，明细尚未完整',
  invalid_amount: '部分积分数量缺失或无效',
  balance_mismatch: '资源包金额与汇总不一致，需刷新核对',
  unknown_expiry: '部分积分的到期时间未知',
  timezone_unverified: '部分上游日期未提供可确认的时区',
}
const dateFormatter = new Intl.DateTimeFormat(undefined, {
  year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit',
  second: '2-digit', hour12: false,
})
const displayTimezone = dateFormatter.resolvedOptions().timeZone
const formatDate = (value: number): string => dateFormatter.format(value)

function expiryText(segment: WorkBuddyCreditSegment, enterprise: boolean): string {
  if (segment.expiryStatus === 'timezone_unverified') return `${segment.expiresAtText || '日期未知'} · 时区待确认`
  if (segment.expiryStatus === 'never') return '无到期限制'
  if (segment.expiryStatus !== 'known' || segment.expiresAt === null) return enterprise ? '重置时间未知' : '到期时间未知'
  return `${enterprise ? '重置于' : '到期于'} ${formatDate(segment.expiresAt)}`
}

function remainingTime(segment: WorkBuddyCreditSegment, now: number, enterprise = false): string {
  if (segment.state === 'not_started') return '尚未生效'
  if (segment.state === 'expired') return '已到期'
  if (segment.state === 'exhausted') return '已耗尽'
  if (segment.expiryStatus !== 'known' || segment.expiresAt === null) return ''
  const difference = segment.expiresAt - now
  if (difference <= 0) return enterprise ? '已到重置时间，待刷新' : '已到期，待刷新'
  if (difference < HOUR) return `剩余 ${Math.max(1, Math.ceil(difference / 60000))} 分钟`
  if (difference < 24 * HOUR) return `剩余 ${Math.ceil(difference / HOUR)} 小时 · 24 小时内${enterprise ? '重置' : '到期'}`
  return `剩余 ${Math.floor(difference / (24 * HOUR))} 天${difference <= 7 * 24 * HOUR ? ` · 7 天内${enterprise ? '重置' : '到期'}` : ''}`
}

function toneOf(segment: WorkBuddyCreditSegment, now: number): string {
  if (segment.expiryStatus !== 'known' || segment.expiresAt === null) return 'unknown'
  if (segment.expiresAt - now <= 24 * HOUR) return 'urgent'
  if (segment.expiresAt - now <= 7 * 24 * HOUR) return 'soon'
  return 'normal'
}

export function AccountsCreditDialog({ id, onClose, returnFocus, initialTab = 'credits' }: {
  id: string
  onClose: () => void
  returnFocus?: HTMLElement | null
  initialTab?: 'credits' | 'growth'
}) {
  const store = React.useSyncExternalStore(subscribe, getStore, getStore)
  const account = findAccount(id)
  const entry = usageEntries().get(id)
  const failure = usageFailureOf(entry)
  const previous = lastSuccessfulUsage(id)
  const details = creditDetailsOf(entry) || ((failure || !entry) ? creditDetailsOf(previous) : null)
  const busy = store.usageInflight.has(id)
  const [now, setNow] = React.useState(Date.now)
  const [selected, setSelected] = React.useState<string | null>(null)
  const [tab, setTab] = React.useState(initialTab)
  const rows = React.useRef(new Map<string, HTMLButtonElement>())
  const originalFocus = React.useRef(returnFocus || (document.activeElement instanceof HTMLElement ? document.activeElement : null))
  const name = displayNameOf(account)
  const shownName = store.namesHidden ? maskName(name) : name
  const enterprise = details?.kind === 'enterprise'
  const growthEnabled = !!account && supportsGrowth(account) && !enterprise
  const providerLabel = providerOf(account) === 'qoder' ? 'Qoder' : 'WorkBuddy'

  React.useEffect(() => { void refreshCreditDetails(id) }, [id])
  React.useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 15000)
    return () => window.clearInterval(timer)
  }, [])
  React.useEffect(() => { if (!account) onClose() }, [account, onClose])

  const ordered = (details?.segments || []).slice().sort((a, b) => {
    const first = a.expiryStatus === 'known' && a.expiresAt !== null ? a.expiresAt : Infinity
    const second = b.expiryStatus === 'known' && b.expiresAt !== null ? b.expiresAt : Infinity
    return first - second || a.id.localeCompare(b.id)
  })
  const expiredInCache = ordered.some(segment => ['active', 'unknown'].includes(segment.state) && segment.expiryStatus === 'known' &&
    segment.expiresAt !== null && segment.expiresAt <= now && (segment.remaining || 0) > 0)
  const stale = Boolean(failure || (details && !creditDetailsFresh(id)) || expiredInCache)
  const inactive = (segment: WorkBuddyCreditSegment): boolean => segment.state === 'exhausted' ||
    segment.state === 'expired' || segment.state === 'not_started' || segment.remaining === 0 ||
    (segment.expiryStatus === 'known' && segment.expiresAt !== null && segment.expiresAt <= now)
  const active = ordered.filter(segment => !inactive(segment))
  const historical = ordered.filter(inactive)
  const positive = active.filter(segment => segment.remaining !== null && segment.remaining > 0)
  const sum = positive.reduce((total, segment) => total + segment.remaining!, 0)
  const unresolved = !details?.complete || details.issues.some(issue => ['truncated', 'invalid_amount', 'balance_mismatch'].includes(issue)) ||
    ordered.some(segment => segment.remaining !== null && segment.total !== null && segment.remaining > segment.total)
  const canShowDistribution = Boolean(details && !details.unlimited && !unresolved && !expiredInCache &&
    !(details.unattributedRemaining && details.unattributedRemaining > 0) &&
    active.every(segment => segment.remaining !== null) && sum > 0 && details.remaining !== null &&
    Math.abs(sum - details.remaining) <= 0.01)
  const expiring = positive.filter(segment => segment.expiryStatus === 'known' && segment.expiresAt !== null &&
    segment.expiresAt > now && segment.expiresAt <= now + 24 * HOUR)
  const nearest = positive.find(segment => segment.expiryStatus === 'known' && segment.expiresAt !== null && segment.expiresAt > now)
  const knownResourceIds = new Set(ordered.map(segment => segment.resourceId).filter(Boolean))
  const unnamedResources = ordered.some(segment => segment.resourceId === null)
  const shownSegmentName = (segment: WorkBuddyCreditSegment): string => store.namesHidden ? maskName(segment.name) : segment.name || '未命名积分包'

  function selectRow(segment: WorkBuddyCreditSegment, scroll = false) {
    setSelected(segment.id)
    if (scroll) {
      const row = rows.current.get(segment.id)
      row?.scrollIntoView({ block: 'nearest', behavior: 'auto' })
      row?.focus({ preventScroll: true })
    }
  }

  function renderRow(segment: WorkBuddyCreditSegment) {
    const ratio = segment.remaining !== null && segment.total !== null && segment.total > 0 && segment.remaining <= segment.total
      ? segment.remaining / segment.total * 100 : null
    const identity = segment.resourceId || segment.id
    return <button key={segment.id} type='button' className='credits-row'
      ref={node => { if (node) rows.current.set(segment.id, node); else rows.current.delete(segment.id) }}
      data-selected={selected === segment.id} data-tone={toneOf(segment, now)}
      aria-pressed={selected === segment.id} onClick={() => selectRow(segment)}>
      <span className='credits-package'>
        <span className='credits-name'>{shownSegmentName(segment)}</span>
        <span className='credits-note'>资源 {store.namesHidden ? maskName(identity) : identity}{segment.packageCode ? ` · ${segment.packageCode}` : ''}</span>
      </span>
      <span className='credits-amount' title={`原始精度：${segment.remaining ?? '未知'} / ${segment.total ?? '未知'}`}>
        <strong>{details?.unlimited ? '∞ 不限量' : formatCreditAmount(segment.remaining)}</strong>{!details?.unlimited && <span className='credits-note'> / {segment.total === null ? '总量未知' : formatCreditAmount(segment.total)}</span>}
        {!details?.unlimited && ratio !== null && <span className='credits-row-meter' aria-hidden='true'><span style={{ width: `${ratio}%` }} /></span>}
        {segment.remaining !== null && segment.total !== null && segment.remaining > segment.total && <span className='credits-warning'>剩余量超过总量，待核对</span>}
      </span>
      <span className='credits-expiry'>
        <span>{expiryText(segment, Boolean(enterprise))}</span>
        <span className='credits-note'>{remainingTime(segment, now, enterprise)}{segment.state === 'unknown' ? ' · 状态待确认' : ''}</span>
        {segment.entitlementEndsAt !== null && segment.entitlementEndsAt !== segment.expiresAt &&
          <span className='credits-note'>权益有效至 {formatDate(segment.entitlementEndsAt)}</span>}
      </span>
    </button>
  }

  const legacy = entry && typeof entry === 'object' && !failure ? entry :
    previous && typeof previous === 'object' ? previous : null

  return <Dialog open onOpenChange={open => { if (!open) onClose() }}>
    <DialogContent overlayForceRender className='w-[min(680px,calc(100vw-32px))] max-h-[calc(100dvh-32px)] credits-dialog'
      finalFocus={() => originalFocus.current?.isConnected ? originalFocus.current : false}>
      <DialogHeader><DialogTitle>{tab === 'growth' && growthEnabled ? '成长福利' : '积分包明细'}{account ? ` · ${shownName}` : ''}</DialogTitle></DialogHeader>
      <DialogBody className='credits-body' aria-busy={busy}>
        <DialogDescription className='credits-description'>{providerLabel} {editionSuffix(account)} · {enterprise ? '企业周期额度' : '个人积分包'}</DialogDescription>
        {growthEnabled && <SegmentedControl aria-label='账号积分内容' value={tab} options={[{ value: 'credits', label: '积分包' }, { value: 'growth', label: '成长福利' }]} onValueChange={value => setTab(value as 'credits' | 'growth')} />}
        {tab === 'growth' && growthEnabled && account ? <AccountsGrowthPanel key={growthAccountKey(account)} account={account} namesHidden={store.namesHidden} /> : !account ? <p role='status'>账号已不存在。</p> : <>
          {failure && <div className='credits-alert' role='alert'>刷新失败：{store.namesHidden ? '当前账号余额查询失败，请重试。' : failure.message}
            {details && <span>以下为上次成功查询的数据。</span>}</div>}
          {busy && <p className='credits-note' role='status'>{details ? '正在刷新当前账号，保留上次成功查询的数据…' : '正在查询当前账号积分明细…'}</p>}
          {details ? <>
            <div className='credits-summary'>
              <div><span className='credits-label'>{unresolved ? '已读取积分（明细不完整）' : stale ? '上次查询余额' : enterprise ? '本周期剩余额度' : '可用积分'}</span>
                <strong className='credits-balance'>{details.unlimited ? '∞ 不限量' : formatCreditAmount(details.remaining)}</strong>
                <span className='credits-note'>{stale ? '上次成功查询 · ' : '查询于 '}{formatDate(details.fetchedAt)}</span>
              </div>
              {!details.unlimited && !unresolved && <div><span className='credits-label'>{stale ? '缓存中 ' : ''}24 小时内{enterprise ? '重置' : '到期'}</span>
                <strong className='credits-expiring'>{formatCreditAmount(expiring.reduce((total, segment) => total + segment.remaining!, 0))}</strong>
                <span className='credits-note'>{nearest ? `最近${enterprise ? '重置' : '到期'}：${formatDate(nearest.expiresAt!)}` : '暂无已知的近期到期时间'}</span>
              </div>}
            </div>
            {expiredInCache && <div className='credits-alert' role='status'>缓存中有积分已到期，余额尚待刷新；旧余额不代表当前可用量。</div>}
            {details.issues.length > 0 && <ul className='credits-issues'>{details.issues.map(issue => <li key={issue}>{issueLabels[issue] || '部分明细的数据质量待确认'}</li>)}</ul>}
            {!details.complete && details.issues.length === 0 && <p className='credits-warning'>明细尚未完整，请刷新后核对。</p>}
            {details.unattributedRemaining !== null && details.unattributedRemaining > 0 && <p className='credits-warning'>未归属到具体资源包／周期：{formatCreditAmount(details.unattributedRemaining)} 积分，到期时间未知。</p>}
            {!details.unlimited && <section aria-label='积分到期分布'>
              <h3 className='credits-section-title'>积分到期分布 <span>按剩余积分占比</span></h3>
              {canShowDistribution ? <TooltipProvider delay={100}><div className='credits-distribution'>
                {positive.map(segment => <Tooltip key={segment.id}>
                  <TooltipTrigger render={<button type='button' className='credits-segment' />}
                    style={{ width: `${segment.remaining! / sum * 100}%` }} data-tone={toneOf(segment, now)}
                    data-selected={selected === segment.id} aria-label={`${shownSegmentName(segment)}，${formatCreditAmount(segment.remaining)} 积分，${expiryText(segment, Boolean(enterprise))}，显示时区 ${displayTimezone}`}
                    onClick={() => selectRow(segment, true)} />
                  <TooltipContent><strong>{shownSegmentName(segment)}</strong><br />剩余 {formatCreditAmount(segment.remaining)} / {segment.total === null ? '总量未知' : `总量 ${formatCreditAmount(segment.total)}`}<br />
                    {expiryText(segment, Boolean(enterprise))}<br />{remainingTime(segment, now, enterprise)}<br />显示时区：{displayTimezone}</TooltipContent>
                </Tooltip>)}
              </div></TooltipProvider> : <div className='credits-empty-bar'>{details.complete && details.remaining === 0 ? '暂无可用积分' : '分布暂不可用，请查看逐包明细'}</div>}
              <div className='credits-legend' aria-label='到期颜色图例'>
                <span data-tone='urgent'>24 小时内</span><span data-tone='soon'>7 天内</span>
                <span data-tone='normal'>其余有效积分</span><span data-tone='unknown'>时间未知</span>
              </div>
              <p className='credits-note'>近到期 → 远到期 · 时间未知放末尾 · 极小积分段可通过下方明细访问</p>
            </section>}
            <section aria-label={enterprise ? '企业额度明细' : '积分包明细列表'}>
              <h3 className='credits-section-title'>{enterprise ? '周期额度明细' : '逐包明细'}<span>{knownResourceIds.size} 个可识别资源包{unnamedResources ? '，另含未提供资源标识的记录' : ''} · {ordered.length} 个积分段</span></h3>
              <div className='credits-list-heading' aria-hidden='true'><span>积分包／资源</span><span>剩余 / 总量</span><span>{enterprise ? '重置时间' : '到期时间'}</span></div>
              <div className='credits-list'>{active.map(renderRow)}</div>
              {active.length === 0 && <p className='credits-note'>暂无当前可用的积分段。</p>}
              {historical.length > 0 && <details className='credits-history'><summary>已耗尽／已到期／未生效（{historical.length} 个积分段）</summary><div className='credits-list'>{historical.map(renderRow)}</div></details>}
            </section>
            <p className='credits-timezone'>显示时区：{displayTimezone}（浏览器本地时区）。时区待确认的上游文本不参与倒计时和 24 小时到期统计。{enterprise ? '日期表示当前额度重置时间。' : '日期表示当前积分截止时间，长期权益另列。'}</p>
          </> : !busy && <div className='credits-unavailable'>
            <h3>积分包明细暂不可用</h3>
            {legacy && <p>原余额摘要：{legacy.unlimited ? '不限量' : typeof legacy.totalLeft === 'number' ? formatCreditAmount(legacy.totalLeft) : '未知'}{failure ? '（上次成功查询）' : ''}</p>}
            <p>{failure ? '请重试当前账号。' : '当前服务器或缓存未返回新版积分明细，可刷新当前账号后重试。'}</p>
          </div>}
          {!enterprise && !account.enterpriseId && account.type !== 'enterprise' && !growthEnabled && <WorkBuddyPolicyPanel key={growthAccountKey(account)} account={account} />}
        </>}
      </DialogBody>
      <DialogFooter className='credits-footer'>
        <span className='credits-note mr-auto' aria-live='polite'>{tab === 'growth' && growthEnabled ? '关闭后已发出的操作仍会完成。' : busy ? '刷新中' : failure ? '刷新失败' : stale ? '上次成功查询' : details ? '当前查询结果' : '暂无明细'}</span>
        {!(tab === 'growth' && growthEnabled) && <Button variant='outline' disabled={busy || !account} onClick={() => { void refreshCreditDetails(id, true) }}>{failure ? '重试当前账号' : '刷新当前账号'}</Button>}
        <Button variant='outline' onClick={onClose}>关闭</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
}
