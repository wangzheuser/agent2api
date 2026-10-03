import * as React from 'react'
import { Button } from '@ui'
import { findAccount, refreshCreditDetails } from './accounts-data'
import type { AccountRecord } from './accounts-shared'
import {
  amountText, checkedState, growthAccountKey, growthActionKey, growthApi, growthClientToken, growthTaskProgressed, resultLabels, rewardsText, supportsGrowth,
  taskLabels, travelLabels, type GrowthAction, type GrowthResult, type GrowthState, type GrowthTask, type WorkBuddyPolicy,
} from './accounts-growth-data'

const dateText = (value: number | null | undefined) => value ? new Date(value).toLocaleString('zh-CN') : '未知'
const errorText = (error: unknown) => error instanceof Error ? error.message : String(error)
function officialLink(value: string | null | undefined): string | undefined {
  if (!value) return undefined
  try { const url = new URL(value); return url.protocol === 'https:' && !url.username && !url.password ? value : undefined } catch { return undefined }
}

export function GrowthResultView({ result, namesHidden = false }: { result: GrowthResult; namesHidden?: boolean }) {
  return <section className='growth-result' data-status={result.status} aria-label='最近福利操作结果'>
    <strong>{resultLabels[result.status] || '状态待确认'}</strong>
    <span>{namesHidden ? '' : result.message}</span>
    <span className='credits-note'>本次回执：{rewardsText(result.rewards)} · 回执{result.receiptConfirmed ? '已确认' : '待确认'} · 状态{result.stateConfirmed ? '已读回' : '待核实'}</span>
    <span className='credits-note'>余额 {amountText(result.balanceBefore)} → {amountText(result.balanceAfter)} · 净变化 {typeof result.balanceDelta === 'number' && Number.isFinite(result.balanceDelta) ? `${result.balanceDelta > 0 ? '+' : ''}${result.balanceDelta.toLocaleString('zh-CN')}` : '未知'}（净变化不等同发奖金额）</span>
    {!!result.items?.length && <ul>{result.items.map((item, index) => <li key={index}>{resultLabels[item.status] || item.status} · {namesHidden ? item.action : item.message} · {rewardsText(item.rewards)}</li>)}</ul>}
  </section>
}

export function WorkBuddyPolicyPanel({ account }: { account: AccountRecord }) {
  const [policy, setPolicy] = React.useState<WorkBuddyPolicy | null>(null)
  const [floor, setFloor] = React.useState('')
  const [busy, setBusy] = React.useState(false)
  const [message, setMessage] = React.useState('')
  const mounted = React.useRef(true)
  const key = growthAccountKey(account)
  const current = () => mounted.current && !!findAccount(account.id) && growthAccountKey(findAccount(account.id)!) === key
  React.useEffect(() => { mounted.current = true; return () => { mounted.current = false } }, [])
  async function load() {
    if (busy || policy) return
    setBusy(true); setMessage('')
    try {
      const result = await growthApi().getWorkBuddyPolicy(account.id)
      if (current()) { setPolicy(result); setFloor(result.creditFloor === null ? '' : String(result.creditFloor)) }
    } catch (error) { if (current()) setMessage(errorText(error)) }
    finally { if (current()) setBusy(false) }
  }
  async function save(event: React.FormEvent) {
    event.preventDefault()
    if (!policy || busy) return
    const value = floor.trim() === '' ? null : Number(floor)
    if (value !== null && (!Number.isFinite(value) || value < 0)) { setMessage('保底积分应为空或非负数。'); return }
    setBusy(true); setMessage('')
    try {
      const result = await growthApi().updateWorkBuddyPolicy(account.id, { autoGrowth: policy.autoGrowth, autoTravel: policy.autoTravel, creditFloor: value, selection: policy.selection, expectedIdentity: policy.identity })
      if (current()) { setPolicy(result); setMessage('设置已保存。自动福利还需在定时任务页开启福利巡检。') }
    } catch (error) { if (current()) setMessage(errorText(error)) }
    finally { if (current()) setBusy(false) }
  }
  return <details className='growth-policy' onToggle={event => { if (event.currentTarget.open) void load() }}>
    <summary>积分保底、自动福利与选号策略</summary>
    {busy && !policy && <p role='status' className='credits-note'>正在读取策略…</p>}
    {message && <p role='status' className='credits-note'>{message}</p>}
    {!policy && !busy && message && <Button variant='outline' onClick={() => void load()}>重试策略查询</Button>}
    {policy && <form className='growth-policy-form' onSubmit={event => void save(event)}>
      {supportsGrowth(account) && <div className='growth-actions'>
        <label><input type='checkbox' checked={policy.autoGrowth} disabled={busy} onChange={event => setPolicy({ ...policy, autoGrowth: event.target.checked })} /> 自动领取已完成奖励</label>
        <label><input type='checkbox' checked={policy.autoTravel} disabled={busy} onChange={event => setPolicy({ ...policy, autoTravel: event.target.checked })} /> 自动猫猫旅行</label>
      </div>}
      <label>账号保底积分 <input type='number' min='0' step='any' placeholder='留空关闭' value={floor} disabled={busy} onChange={event => setFloor(event.target.value)} /></label>
      <p className='credits-note'>余额达到或低于保底、余额未知、明细不完整或超过 {Math.round(policy.balanceMaxAgeSeconds / 60)} 分钟未刷新时，暂停该账号的新转发。并发请求与查询间隔仍可能使余额低于保底值；福利领取不受影响。</p>
      <label>WorkBuddy 全局选号 <select value={policy.selection} disabled={busy} onChange={event => setPolicy({ ...policy, selection: event.target.value as WorkBuddyPolicy['selection'] })}>
        <option value='priority'>原账号优先级（默认）</option><option value='expiry'>临期积分优先</option><option value='cost'>同模型实扣成本优先</option>
      </select></label>
      <p className='credits-note'>选号设置影响所有 WorkBuddy 账号；仅在同地区可用账号内排序。积分包或成本样本不足时沿用账号优先级。</p>
      <p className='credits-note'>最近实扣积分：{amountText(policy.cost?.lastCredits, '未上报')} · 样本 {policy.cost?.samples ?? 0} · {dateText(policy.cost?.lastAt)}</p>
      {!!policy.cost?.models.length && <ul className='credits-issues'>{policy.cost.models.map(model => <li key={model.model}>{model.model} · 每千 token {amountText(model.creditsPer1kTokens, '未上报')} 积分 · {model.samples} 个样本</li>)}</ul>}
      <Button type='submit' variant='outline' disabled={busy}>{busy ? '保存中…' : '保存设置'}</Button>
    </form>}
  </details>
}

export function AccountsGrowthPanel({ account, namesHidden = false }: { account: AccountRecord; namesHidden?: boolean }) {
  const [state, setState] = React.useState<GrowthState | null>(null)
  const [busy, setBusy] = React.useState(false)
  const [error, setError] = React.useState('')
  const [uncertain, setUncertain] = React.useState(false)
  const [result, setResult] = React.useState<GrowthResult | null>(null)
  const [location, setLocation] = React.useState('')
  const [makeupDate, setMakeupDate] = React.useState('')
  const [agreement, setAgreement] = React.useState(false)
  const lock = React.useRef(false)
  const mounted = React.useRef(true)
  const actionRequest = React.useRef<{ key: string; token: string; task?: GrowthTask } | null>(null)
  const key = growthAccountKey(account)
  const current = () => mounted.current && !!findAccount(account.id) && growthAccountKey(findAccount(account.id)!) === key
  React.useEffect(() => { mounted.current = true; void refresh(); return () => { mounted.current = false } }, [])

  async function refresh() {
    if (lock.current) return
    lock.current = true; setBusy(true); setError('')
    try {
      const value = checkedState(await growthApi().getWorkBuddyGrowth(account.id), account.id)
      if (current()) {
        setState(value); setUncertain(false); setResult(value.lastRun); setAgreement(false)
        if (growthTaskProgressed(actionRequest.current?.task, value)) actionRequest.current = null
        if (value.pending?.request.id === account.id) actionRequest.current = { key: growthActionKey(value.pending.request), token: value.pending.clientToken }
      }
    } catch (error) { if (current()) { setError(namesHidden ? '成长福利查询失败，请重试。' : errorText(error)); setUncertain(true) } }
    finally { lock.current = false; if (current()) setBusy(false) }
  }
  async function act(action: string, options: Omit<GrowthAction, 'id' | 'action'> = {}) {
    if (lock.current || !state || uncertain) return
    const identity = state.identity
    const request = { ...options, id: account.id, action, expectedIdentity: identity }
    const requestKey = growthActionKey(request)
    if (state.pending && (state.pending.request.id !== account.id || growthActionKey(state.pending.request) !== requestKey)) { setError('此前操作待核实，请先继续原操作。'); return }
    lock.current = true; setBusy(true); setError('')
    try {
      if (actionRequest.current?.key !== requestKey) actionRequest.current = { key: requestKey, token: growthClientToken(), task: action === 'execute_task' ? state.tasks.find(task => task.code === request.taskCode && task.source === (request.source || 'growth')) : undefined }
      const value = await growthApi().workBuddyGrowthAction({ ...request, clientToken: actionRequest.current.token })
      if (!current()) return
      if (value.state?.identity && value.state.identity !== identity) { setState(null); setUncertain(true); setError('账号身份已变化，请重新查询。'); return }
      setResult(value)
      if (value.state) setState(checkedState(value.state, account.id))
      const needsRefresh = ['uncertain', 'pending', 'busy'].includes(value.status) || !value.state
      setUncertain(needsRefresh)
      if (!['uncertain', 'pending', 'busy'].includes(value.status)) actionRequest.current = null
      setAgreement(false)
      void refreshCreditDetails(account.id, true)
    } catch (error) {
      if (current()) { setError(namesHidden ? '操作结果待核实，请先刷新状态。' : `操作结果待核实：${errorText(error)}。请先刷新状态后再操作。`); setUncertain(true) }
    } finally { lock.current = false; if (current()) setBusy(false) }
  }
  const disabled = busy || uncertain || !!state?.running || !!state?.pending
  const travel = state?.travel
  const selectedLocation = travel?.locations.find(item => item.id === location && item.enabled)
  return <div className='growth-panel' aria-busy={busy}>
    <div className='growth-actions'>
      <Button variant='outline' disabled={busy} onClick={() => void refresh()}>{busy ? '处理中…' : '刷新福利状态'}</Button>
      {state?.supported && <><Button variant='outline' disabled={disabled} onClick={() => void act('claim_available')}>领取已完成奖励</Button>
        <Button variant='outline' disabled={disabled || !state.tasks.some(task => task.canExecute)} onClick={() => void act('run_supported')}>执行可用任务</Button></>}
    </div>
    <p className='credits-note'>领取仅处理已达标任务及可兑换连登档位；执行可用任务会发送真实请求，可能消耗积分。猫猫旅行、领养、补签、抽奖分别操作。</p>
    {error && <div className='credits-alert' role='alert'>{error}</div>}
    {state && <>
      <p className='credits-note'>查询于 {dateText(state.fetchedAt)}{state.running ? ' · 账号有动作处理中，请稍后刷新' : ''}</p>
      {state.pending && <div className='credits-alert'>此前操作结果待核实，新动作暂缓。
        {state.pending.request.id === account.id ? <Button variant='outline' disabled={busy || uncertain || state.running} onClick={() => void act(state.pending!.request.action, state.pending!.request)}>继续核实上次操作</Button> : <span>请回到发起操作的账号核实。</span>}
      </div>}
      {!state.supported && <p className='credits-alert'>{state.reason || '当前账号不适用国内成长福利。'}</p>}
      {!!state.errors.length && <div className='credits-alert' role='status'>部分状态查询失败：{state.errors.map(item => namesHidden ? item.section : `${item.section}：${item.message}`).join('；')}</div>}
      {state.supported && <>
      {travel && <section className='growth-card' aria-label='猫猫日常'>
        <h3 className='credits-section-title'>猫猫日常 <span>{travelLabels[travel.state] || '状态待确认'}</span></h3>
        {travel.arrivesAt !== null && <p className='credits-note'>预计归来：{dateText(travel.arrivesAt)}</p>}
        <p className='credits-note'>当前旅行奖励：{rewardsText(travel.rewards)}</p>
        {travel.canAdopt && <div className='growth-adopt'>
          <p className='credits-note'>首次领养是独立操作；请先在官方客户端阅读领养说明和协议，再在此确认。</p>
          {officialLink(travel.agreementUrl) && <a href={officialLink(travel.agreementUrl)} target='_blank' rel='noreferrer'>前往官方成长中心查看领养协议</a>}
          <label><input type='checkbox' checked={agreement} disabled={disabled} onChange={event => setAgreement(event.target.checked)} /> 我已阅读并同意官方领养协议</label>
          <Button variant='outline' disabled={disabled || !agreement} onClick={() => void act('buddy_adopt', { agreementAccepted: true })}>领养猫猫</Button>
        </div>}
        {travel.canDepart && <div className='growth-actions'>
          <label>旅行目的地 <select value={location} disabled={disabled} onChange={event => setLocation(event.target.value)}><option value=''>选择目的地</option>{travel.locations.filter(item => item.enabled).map(item => <option key={item.id} value={item.id}>{item.name}</option>)}</select></label>
          <Button variant='outline' disabled={disabled || !selectedLocation} onClick={() => void act('travel_depart', { locationId: location })}>出发旅行</Button>
        </div>}
        {selectedLocation && <p className='credits-note'>预计 {amountText(selectedLocation.durationSecondsMin == null ? null : selectedLocation.durationSecondsMin / 3600)}～{amountText(selectedLocation.durationSecondsMax == null ? null : selectedLocation.durationSecondsMax / 3600)} 小时 · 奖励范围 {amountText(selectedLocation.rewardCreditsMin)}～{amountText(selectedLocation.rewardCreditsMax)} 积分；最终以归来回执为准。</p>}
        {travel.canClaim && <Button variant='outline' disabled={disabled} onClick={() => void act('travel_claim')}>领取旅行奖励</Button>}
      </section>}
      <section aria-label='成长任务'><h3 className='credits-section-title'>成长任务 <span>{state.tasks.length} 项</span></h3>
        <div className='growth-tasks'>{state.tasks.map(task => <div key={`${task.source}:${task.code}`} className='growth-card'>
          <h4>{task.title} <span className='credits-note'>{taskLabels[task.state] || '状态待确认'}{task.source === 'mini_program' ? ' · 小程序' : ''}</span></h4>
          <p className='credits-note'>进度 {amountText(task.current)} / {amountText(task.target)} · {rewardsText(task.rewards)}{task.expiresAt ? ` · 截止 ${dateText(task.expiresAt)}` : ''}</p>
          {task.reason && <p className='credits-note'>{task.reason}</p>}
          <div className='growth-actions'>
            {task.canAccept && <Button variant='outline' disabled={disabled} onClick={() => void act('accept_task', { taskCode: task.code, source: task.source })}>接取任务</Button>}
            {task.canExecute && <Button variant='outline' disabled={disabled} onClick={() => void act('execute_task', { taskCode: task.code, source: task.source })}>执行任务</Button>}
            {task.canClaim && <Button variant='outline' disabled={disabled} onClick={() => void act('claim_task', { taskCode: task.code, source: task.source })}>领取奖励</Button>}
            {officialLink(task.actionUrl) && <a href={officialLink(task.actionUrl)} target='_blank' rel='noreferrer'>前往官方完成</a>}
          </div>
        </div>)}</div>
        {!state.tasks.length && <p className='credits-note'>当前未返回成长任务；请结合上方查询状态判断。</p>}
      </section>
      {state.streak && <section className='growth-card' aria-label='连续活跃'>
        <h3 className='credits-section-title'>连续活跃 <span>{amountText(state.streak.days)} 天 · 时区 {state.streak.timezone || '未知'}</span></h3>
        {state.streak.tiers.map(tier => <div key={tier.id} className='growth-tier'><span>{amountText(tier.requiredDays)} 天 · {rewardsText(tier.rewards)}</span><Button variant='outline' disabled={disabled || !tier.claimable} onClick={() => void act('streak_redeem', { tier: tier.id })}>{tier.claimed ? '已兑换' : tier.claimable ? '兑换奖励' : '未达条件'}</Button></div>)}
        <div className='growth-actions'><label>补签（卡片 {amountText(state.streak.makeupCards)} 张）<select value={makeupDate} disabled={disabled} onChange={event => setMakeupDate(event.target.value)}><option value=''>选择缺签日期</option>{state.streak.missedDates.map(date => <option key={date} value={date}>{date}</option>)}</select></label>
          <Button variant='outline' disabled={disabled || !makeupDate || !state.streak.missedDates.includes(makeupDate) || !(state.streak.makeupCards! > 0)} onClick={() => void act('makeup', { date: makeupDate })}>消耗 1 张卡补签</Button></div>
      </section>}
      <details className='growth-policy'><summary>抽奖与其他活动</summary>
        {state.lottery && <div className='growth-tier'><span>抽奖次数 {amountText(state.lottery.chances)}</span><Button variant='outline' disabled={disabled || !state.lottery.canDraw} onClick={() => void act('lottery_draw')}>消耗 1 次机会抽奖</Button></div>}
        {state.activities.map(activity => <div key={activity.code} className='growth-card'><h4>{activity.title}</h4><p className='credits-note'>{activity.reason || activity.state}</p>
          {['gift', 'compensation'].includes(activity.code) && (activity.canClaim || activity.canAttempt) && <Button variant='outline' disabled={disabled} onClick={() => void act(activity.code === 'gift' ? 'claim_gift' : 'claim_compensation')}>{activity.canClaim ? '领取奖励' : '检查并尝试领取'}</Button>}
          {officialLink(activity.actionUrl) && <a href={officialLink(activity.actionUrl)} target='_blank' rel='noreferrer'>查看官方活动</a>}
        </div>)}
      </details>
      </>}
    </>}
    {result && <GrowthResultView result={result} namesHidden={namesHidden} />}
    <WorkBuddyPolicyPanel account={account} />
  </div>
}
