import * as React from 'react'
import {
  Button, Dialog, DialogBody, DialogContent, DialogDescription, DialogFooter, DialogHeader,
  DialogTitle, SegmentedControl,
} from '@ui'
import {
  allAccounts, creditOverviewSample, findAccount, getStore, maskName, refreshCreditOverview,
  subscribe, visibleList,
} from './accounts-data'
import { displayNameOf, providerOf } from './accounts-domain'
import { formatCreditAmount } from './accounts-credit-details'
import { AccountsCreditDialog } from './accounts-credit-dialog'
import { AccountsGrowthOverview } from './accounts-growth-overview'
import { growthAccountKey } from './accounts-growth-data'
import {
  CREDIT_BUCKETS, creditScopeAccounts, sortCreditOverviewRows, summarizeCreditOverview,
  type CreditBucket, type CreditOverviewRow, type CreditScope,
} from './accounts-credit-overview'
import type { AccountRecord } from './accounts-shared'

const dateFormat = new Intl.DateTimeFormat(undefined, { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false })
const timestamp = (value: number): string => dateFormat.format(value)

function rowStatus(row: CreditOverviewRow): string {
  if (row.duplicate) return '同一身份，未重复计入'
  if (row.generic) {
    if (row.generic.issues.length) return '余额已读取，但部分附加信息失败'
    return '通用余额，未计入积分包分段合计'
  }
  if (!row.details) return row.failed ? '查询失败，未计入' : '尚无明细，未计入'
  if (row.details.kind === 'enterprise') return '企业周期额度，单独展示'
  if (row.details.unlimited) return '不限量，不计入积分合计'
  if (row.partial) return '明细不完整或金额待核对，未计入'
  if (row.expired > 0) return `已排除到期积分 ${formatCreditAmount(row.expired)}，待刷新`
  if (row.failed) return '刷新失败，使用上次成功数据'
  if (!row.fresh) return '缓存数据，待刷新'
  return '已计入个人积分'
}

export function AccountsCreditOverviewDialog({ onClose, returnFocus }: { onClose: () => void; returnFocus: HTMLElement }) {
  const store = React.useSyncExternalStore(subscribe, getStore, getStore)
  const [scope, setScope] = React.useState<CreditScope>('all')
  const [tab, setTab] = React.useState('credits')
  const [sort, setSort] = React.useState('expiry')
  const [filter, setFilter] = React.useState<{ provider: string; edition: string; bucket: CreditBucket } | null>(null)
  const [, setNow] = React.useState(Date.now)
  const [refreshing, setRefreshing] = React.useState(false)
  const refresh = React.useRef<AbortController | null>(null)
  const [detail, setDetail] = React.useState<{ account: AccountRecord; trigger: HTMLElement; tab?: 'growth' } | null>(null)
  const closeDetail = React.useCallback(() => setDetail(null), [])
  const all = allAccounts()
  const visibleIds = new Set(visibleList().map(account => account.id))
  const accounts = creditScopeAccounts(all, scope, visibleIds, store.selected)
  const groups = summarizeCreditOverview(accounts.map(account => creditOverviewSample(account)))
  const growthAccounts = accounts.filter(account => providerOf(account) === 'workbuddy')
  const currentDetail = detail && findAccount(detail.account.id)
  const sameIdentity = detail && currentDetail && growthAccountKey(detail.account) === growthAccountKey(currentDetail)

  React.useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 15000)
    return () => { window.clearInterval(timer); refresh.current?.abort() }
  }, [])
  React.useEffect(() => { if (detail && !sameIdentity) closeDetail() }, [detail, sameIdentity, closeDetail])

  async function refreshScope() {
    if (refresh.current) return
    const controller = new AbortController()
    refresh.current = controller
    setRefreshing(true)
    try { await refreshCreditOverview(accounts.map(account => account.id), controller.signal) }
    finally {
      refresh.current = null
      if (!controller.signal.aborted) { setNow(Date.now()); setRefreshing(false) }
    }
  }

  function renderRow(row: CreditOverviewRow) {
    const name = displayNameOf(row.account)
    const shownName = store.namesHidden ? maskName(name) : name
    const details = row.details
    const generic = row.generic
    const amount = row.amount ?? details?.remaining ?? generic?.available ?? null
    return <button key={row.account.id} type='button' className='credits-row overview-account' data-account-id={row.account.id}
      onClick={event => setDetail({ account: { ...row.account }, trigger: event.currentTarget })}>
      <span className='credits-package'><span className='credits-name'>{shownName}{row.account.enabled === false ? ' · 已禁用' : ''}</span>
        <span className='credits-note'>{rowStatus(row)}</span>
        {details && <span className='credits-note'>查询于 {timestamp(details.fetchedAt)}{row.failed ? ' · 最近刷新失败' : ''}</span>}
      </span>
        <span className='credits-amount'><strong>{details?.unlimited ? '∞ 不限量' : formatCreditAmount(amount)}</strong>
        <span className='credits-note'>{generic ? `${generic.unit} · 通用余额` : row.partial || row.duplicate ? '查询读数' : details?.kind === 'enterprise' ? '周期剩余额度' : '已知有效积分'}</span>
      </span>
      <span className='credits-expiry'>
        {details?.kind === 'enterprise' ? <span>重置时间见逐账号明细</span> : <span>{row.nearest !== null ? `最近到期／重置 ${timestamp(row.nearest)}` : '暂无可确认的到期时间'}</span>}
        {details && !details.unlimited && row.amount !== null && <span className='credits-note'>24 小时内 {formatCreditAmount(row.buckets.day)} · 7 天内 {formatCreditAmount(row.buckets.day + row.buckets.week)}</span>}
        {generic && <span className='credits-note'>订阅到期：{generic.subscription?.expireAt ? timestamp(generic.subscription.expireAt) : '未知'}</span>}
        {!!details?.unattributedRemaining && <span className='credits-note'>其中未归属 {formatCreditAmount(details.unattributedRemaining)}（已包含在未知周期段）</span>}
        {details && row.asOf === null && <span className='credits-note'>快照时钟未确认，请刷新后查看到期分布</span>}
      </span>
    </button>
  }

  return <Dialog open onOpenChange={open => { if (!open) onClose() }}>
    <DialogContent className='w-[min(860px,calc(100vw-32px))] max-h-[calc(100dvh-32px)] credits-dialog credit-overview'
      finalFocus={() => returnFocus.isConnected ? returnFocus : false}>
      <DialogHeader><DialogTitle>积分总览</DialogTitle></DialogHeader>
      <DialogBody className='credits-body'>
        <DialogDescription className='credits-description'>所有已接入余额的提供商按账号展示；WorkBuddy 与 Qoder 汇总精确积分包，其它提供商展示通用余额和到期信息。点击账号查看明细。</DialogDescription>
        <SegmentedControl aria-label='总览内容' value={tab} options={[{ value: 'credits', label: '积分到期' }, { value: 'growth', label: '福利待办' }]} onValueChange={setTab} />
        <div className='overview-controls'>
          <SegmentedControl aria-label='积分汇总范围' value={scope} disabled={refreshing} options={[
            { value: 'all', label: '全部', count: creditScopeAccounts(all, 'all', visibleIds, store.selected).length },
            { value: 'filtered', label: '当前筛选', count: creditScopeAccounts(all, 'filtered', visibleIds, store.selected).length },
            { value: 'selected', label: '已选账号', count: creditScopeAccounts(all, 'selected', visibleIds, store.selected).length },
          ]} onValueChange={value => { setScope(value as CreditScope); setFilter(null) }} />
          {tab === 'credits' && <SegmentedControl aria-label='总览列表排序' value={sort} options={[
            { value: 'expiry', label: '最近到期' }, { value: 'original', label: '账号顺序' },
          ]} onValueChange={setSort} />}
        </div>
        {tab === 'growth' ? <AccountsGrowthOverview key={growthAccounts.map(growthAccountKey).join('|')} accounts={growthAccounts} namesHidden={store.namesHidden}
          onDetail={(account, trigger) => setDetail({ account: { ...account }, trigger, tab: 'growth' })} /> : <>
        <p className='credits-note'>当前范围 {accounts.length} 个账号（含禁用账号）。排序仅影响此弹窗；7 天内包含 24 小时内，分布条各段互不重叠。</p>
        {!accounts.length && <p role='status' className='credits-empty-bar'>当前范围没有可查询余额的账号。</p>}
          {groups.map(group => {
            if (group.mode === 'generic') {
            const providerLabel = ({
              workbuddy: 'WorkBuddy', qoder: 'Qoder', raccoon: '小浣熊', catpaw: '小浣熊', autoclaw: 'AutoClaw', 'autoclaw-intl': 'AutoClaw 国际版',
              trae: 'Trae', 'cline-free': 'Cline Free', 'cline-pass': 'Cline Pass', accio: 'Accio', 'accio-cn': 'Accio',
              zcode: 'ZCode', 'zcode-intl': 'ZCode 国际版', codearts: 'CodeArts',
              } as Record<string, string>)[group.provider] || group.provider
            const rows = sortCreditOverviewRows(group.rows, sort === 'expiry')
            const nearest = group.rows.reduce((value, row) => row.nearest === null ? value : Math.min(value, row.nearest), Infinity)
            const editionLabel = group.edition === 'cn' ? '国内版' : '国际版'
            return <section key={`${group.provider}:${group.edition}:generic:${group.unit || ''}`} className='overview-edition' aria-label={`${providerLabel} ${editionLabel}通用余额总览`}>
              <h3 className='credits-section-title'>{providerLabel} {editionLabel}通用余额总览<span>{group.rows.length} 个账号 · 通用余额</span></h3>
              <div className='credits-summary overview-summary'>
                <div><span className='credits-label'>已知余额</span><strong data-overview-total>{formatCreditAmount(group.amount)}</strong><span className='credits-note'>单位：{group.unit || '余额'}</span></div>
                <div><span className='credits-label'>最近到期／重置</span><strong>{Number.isFinite(nearest) ? timestamp(nearest) : '未知'}</strong></div>
              </div>
              <p className='credits-note overview-coverage'>通用余额不计入积分包分段合计 · 缓存待刷新 {group.stale} · 无明细 {group.missing} · 部分失败 {group.partial}</p>
              <h4 className='credits-section-title'>账号明细<span>{rows.length} / {group.rows.length} 个 · 点击查看余额</span></h4>
              <div className='credits-list'>{rows.map(renderRow)}</div>
            </section>
          }
          const activeBucket = filter?.provider === group.provider && filter.edition === group.edition ? filter.bucket : null
          const rows = sortCreditOverviewRows(group.rows.filter(row => !activeBucket || (!row.duplicate && row.amount !== null && row.buckets[activeBucket] > 0)), sort === 'expiry')
          const providerLabel = group.provider === 'qoder' ? 'Qoder' : 'WorkBuddy'
          const editionLabel = group.edition === 'cn' ? '国内版' : '国际版'
          return <section key={`${group.provider}:${group.edition}`} className='overview-edition' aria-label={`${providerLabel} ${editionLabel}积分总览`}>
            <h3 className='credits-section-title'>{providerLabel} {editionLabel}<span>{group.rows.length} 个账号 · 个人已计入 {group.included} 个</span></h3>
            <div className='credits-summary overview-summary'>
              <div><span className='credits-label'>已知有效个人积分{group.stale ? '（含缓存）' : ''}</span><strong data-overview-total>{formatCreditAmount(group.amount)}</strong></div>
              <div><span className='credits-label'>24 小时内到期</span><strong>{formatCreditAmount(group.amount === null ? null : group.buckets.day)}</strong></div>
              <div><span className='credits-label'>7 天内到期（含 24 小时）</span><strong>{formatCreditAmount(group.amount === null ? null : group.buckets.day + group.buckets.week)}</strong></div>
            </div>
            <p className='credits-note overview-coverage'>缓存待刷新 {group.stale} · 无明细 {group.missing} · 不完整 {group.partial} · 重复身份 {group.duplicates} · 企业 {group.enterprise} · 不限量 {group.unlimited}</p>
            {group.amount !== null && group.amount > 0 ? <div className='credits-distribution' aria-label='个人积分到期分布'>
              {CREDIT_BUCKETS.filter(bucket => group.buckets[bucket.id] > 0).map(bucket => <button key={bucket.id} type='button'
                className='credits-segment' data-tone={bucket.tone} data-selected={activeBucket === bucket.id}
                style={{ width: `${group.buckets[bucket.id] / group.amount! * 100}%` }}
                aria-label={`${bucket.label}：${formatCreditAmount(group.buckets[bucket.id])} 积分，筛选账号`}
                title={`${bucket.label} · ${formatCreditAmount(group.buckets[bucket.id])} 积分`}
                onClick={() => setFilter(activeBucket === bucket.id ? null : { provider: group.provider, edition: group.edition, bucket: bucket.id })} />)}
            </div> : <p className='credits-empty-bar'>{group.amount === 0 ? '已查询账号暂无有效个人积分。' : '尚无可汇总的完整个人明细，请刷新当前范围。'}</p>}
            <div className='overview-buckets' aria-label='到期分布筛选'>
              <button type='button' aria-pressed={!activeBucket} onClick={() => setFilter(null)}>全部账号</button>
              {CREDIT_BUCKETS.map(bucket => <button key={bucket.id} type='button' data-tone={bucket.tone} aria-pressed={activeBucket === bucket.id}
                disabled={!group.buckets[bucket.id]} onClick={() => setFilter(activeBucket === bucket.id ? null : { provider: group.provider, edition: group.edition, bucket: bucket.id })}>
                {bucket.label} <b>{formatCreditAmount(group.amount === null ? null : group.buckets[bucket.id])}</b>
              </button>)}
            </div>
            <h4 className='credits-section-title'>账号明细<span>{activeBucket ? `${CREDIT_BUCKETS.find(bucket => bucket.id === activeBucket)?.label} · ` : ''}{rows.length} / {group.rows.length} 个 · 点击查看积分包</span></h4>
            <div className='credits-list'>{rows.map(renderRow)}</div>
            {!rows.length && <p className='credits-note'>该时间段当前没有匹配账号。</p>}
          </section>
        })}
        <p className='credits-timezone'>显示时区：{dateFormat.resolvedOptions().timeZone}。未知到期及待确认时区单独列示；不完整数据不参与合计，缓存中的已到期积分会排除。企业额度不合并为可共享余额。</p>
        </>}
      </DialogBody>
      <DialogFooter className='credits-footer'>
        <span className='credits-note mr-auto' role='status'>{tab === 'growth' ? '范围变化后重新查询；仅 WorkBuddy 国内账号参与福利。' : refreshing ? '正在刷新当前范围，最多同时查询 3 个账号…' : '显示已有查询结果，可手动刷新当前范围。'}</span>
        {tab === 'credits' && <Button variant='outline' disabled={refreshing || !accounts.length} onClick={() => void refreshScope()}>{refreshing ? '刷新中…' : '刷新当前范围'}</Button>}
        <Button variant='outline' onClick={onClose}>关闭</Button>
      </DialogFooter>
    </DialogContent>
    {detail && sameIdentity && <AccountsCreditDialog key={detail.account.id} id={detail.account.id} onClose={closeDetail} returnFocus={detail.trigger} initialTab={detail.tab} />}
  </Dialog>
}
