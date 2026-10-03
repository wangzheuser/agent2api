import * as React from 'react'
import { Button } from '@ui'
import { findAccount, maskName, refreshCreditDetails } from './accounts-data'
import { displayNameOf } from './accounts-domain'
import type { AccountRecord } from './accounts-shared'
import { checkedState, growthAccountKey, growthApi, growthClientToken, growthTodo, resultLabels, runGrowthBatch, uniqueGrowthAccounts, type GrowthResult, type GrowthState } from './accounts-growth-data'

type Row = { state?: GrowthState; result?: GrowthResult; error?: string; pending?: boolean }
export function AccountsGrowthOverview({ accounts, namesHidden, onDetail }: {
  accounts: AccountRecord[]; namesHidden: boolean; onDetail: (account: AccountRecord, trigger: HTMLElement) => void
}) {
  const targets = uniqueGrowthAccounts(accounts)
  const [rows, setRows] = React.useState<Record<string, Row>>({})
  const [busy, setBusy] = React.useState(false)
  const [stopped, setStopped] = React.useState(false)
  const active = React.useRef<AbortController | null>(null)
  const mounted = React.useRef(true)
  React.useEffect(() => { mounted.current = true; void run(false); return () => { mounted.current = false; active.current?.abort() } }, [])
  async function run(claim: boolean) {
    if (active.current) return
    const controller = new AbortController()
    active.current = controller; setBusy(true); setStopped(false)
    const snapshot = targets.map(account => ({ ...account }))
    await runGrowthBatch(snapshot, controller.signal, async account => {
      const key = growthAccountKey(account)
      const current = () => mounted.current && !!findAccount(account.id) && growthAccountKey(findAccount(account.id)!) === key
      if (!current()) return
      setRows(previous => ({ ...previous, [account.id]: { ...previous[account.id], pending: true, error: undefined } }))
      try {
        // 每次执行前读回当前资格；重复身份由服务端租约再作最终隔离。
        let state = checkedState(await growthApi().getWorkBuddyGrowth(account.id), account.id)
        let result: GrowthResult | undefined
        if (claim && !controller.signal.aborted && current() && state.supported && !state.running) {
          result = await growthApi().workBuddyGrowthAction({ id: account.id, action: 'claim_available', expectedIdentity: state.identity, clientToken: growthClientToken() })
          if (result.state) {
            if (result.state.identity !== state.identity) throw new Error('账号身份已变化，请刷新核对。')
            state = checkedState(result.state, account.id)
          }
          void refreshCreditDetails(account.id, true)
        }
        if (current()) setRows(previous => ({ ...previous, [account.id]: { state, result } }))
      } catch (error) {
        if (current()) setRows(previous => ({ ...previous, [account.id]: { ...previous[account.id], pending: false, error: claim ? '操作结果待核实，请打开账号刷新状态。' : namesHidden ? '查询失败，请重试。' : error instanceof Error ? error.message : String(error) } }))
      }
    })
    active.current = null
    if (mounted.current) { setBusy(false); setStopped(controller.signal.aborted) }
  }
  const known = targets.filter(account => rows[account.id]?.state)
  const todo = known.reduce((sum, account) => sum + growthTodo(rows[account.id].state!), 0)
  return <div className='growth-panel' aria-label='跨账号福利待办' aria-busy={busy}>
    <div className='growth-actions'>
      <Button variant='outline' disabled={busy || !targets.length} onClick={() => void run(false)}>刷新范围福利</Button>
      <Button variant='outline' disabled={busy || !targets.length} onClick={() => void run(true)}>领取范围内已完成奖励</Button>
      {busy && <Button variant='outline' onClick={() => { active.current?.abort(); setStopped(true) }}>停止未派发</Button>}
    </div>
    <p className='credits-note' role='status'>{busy ? '最多同时处理 3 个账号。' : stopped ? '未派发账号已停止。' : `已查询 ${known.length} / ${targets.length} 个国内个人身份，已知可领取 ${todo} 项（含旅行，旅行请逐账号领取）。`}当前范围已排除国际、企业及重复身份 {accounts.length - targets.length} 条。</p>
    <p className='credits-note'>批量只领取已达标任务与连登档位；猫猫旅行单独处理。关闭或停止后，已发出的操作仍会完成，最近结果保存在账号内。</p>
    {!targets.length && <p className='credits-empty-bar'>当前范围没有适用的国内个人账号。</p>}
    {targets.map(account => {
      const row = rows[account.id]
      const name = displayNameOf(account)
      return <div key={account.id} className='growth-card growth-overview-row'>
        <div><strong>{namesHidden ? maskName(name) : name}{account.enabled === false ? ' · 转发已禁用' : ''}</strong>
          <p className='credits-note'>{row?.pending ? '处理中…' : row?.error || (row?.state ? row.state.supported ? `可领取 ${growthTodo(row.state)} 项 · 成长任务 ${row.state.tasks.length} 项${row.state.errors.length ? ' · 部分查询失败' : ''}` : row.state.reason : '尚未查询')}</p>
          {row?.result && <p className='credits-note'>{resultLabels[row.result.status] || '状态待确认'}{!namesHidden ? ` · ${row.result.message}` : ''}</p>}
        </div><Button variant='outline' onClick={event => onDetail(account, event.currentTarget)}>查看福利</Button>
      </div>
    })}
  </div>
}
