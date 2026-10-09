//! 后台任务的**持久化排期**与执行占位（跨重启、跨实例）。
//!
//! ── 这个模块解决什么问题 ─────────────────────────────────────
//! 改造前后台任务的排期只活在内存里（`scheduled_tasks` 的 `RUNS`）：进程一重启
//! 就清零，首轮又被排到「现在」，于是**每次打开软件都会把上游打一遍** ——
//! 一分钟内重开几次，GitHub 的匿名限额（60 次/小时，按出口 IP 计）就被同一台
//! 机器重复消耗；余额查询、模型刷新、凭证维护同理，账号越多代价越大。
//!
//! 本模块把「上次尝试 / 上次成功 / 下次执行 / 失败冷却」落进统一库的 `kv`
//! （键 `backgroundTaskState`，见 `db::schema` 的保留键清单），于是：
//!   - **重启不重置节奏**：10:00 查过更新（间隔 20 分钟），10:05、10:10 反复
//!     重启都不再查，仍等到 10:20；
//!   - **停机期间只补一次**：休眠 / 关机错过的那一轮在启动后跑一次，不补跑
//!     期间的每一轮；
//!   - **失败有冷却**：连续失败按间隔指数退避（上限一天），并且**重启不清零**
//!     —— 否则「重启即可绕过冷却」等于没有冷却。手动触发要不要越过它由调用点
//!     逐处选（见 [`ManualBackoff`]）。
//!
//! ── 硬约束：请求发出**之前**就记一笔尝试 ─────────────────────
//! `claim` 在返回执行权之前就把 `lastAttemptAt` / `nextRunAt` 写进库。理由：
//! 只在成功后记录的话，「请求途中被杀 / 失败 / 上游超时」这几条路径都会留下
//! 一个「没跑过」的状态，下次启动立刻重试 —— 那正是本模块要消灭的行为。
//! 代价是「失败的那一次也占用了本轮排期」，由失败冷却（`retry_at`）另行表达。
//!
//! ── 执行占位（lease）：进程被杀也不会卡死 ────────────────────
//! 同一个任务同时只允许一轮真实请求（占位 + 心跳续租 90 秒）。占位**带过期
//! 时间**而不是一个布尔：进程崩溃 / 强杀后没有人来清标志，只有租约到期才
//! 会释放 —— 布尔形态会让任务永远卡在「执行中」。心跳任务随 `RunGuard` 一起
//! 结束，正常收尾（`finish`）与中途取消（`Drop`）都会释放占位。
//! 余额查询循环完成整轮后再等待 10 秒，跨实例余额缓存同步会随慢查询延迟；
//! 本模块的租约心跳独立续租，不依赖余额查询循环完成。
//!
//! ── 为什么键名不带任务前缀、值整份存一个对象 ────────────────
//! `kv` 的固定键必须登记进 `db::schema::RESERVED_KV_KEYS`（配置写入靠它排除
//! 「不归我管」的键），一条任务一个键要登记四五项，撞名核对面随之扩大；整份
//! 存一个对象只加一项，且新增任务不必再动 schema 那份清单。写入是「读改写
//! 整份」，但整段在一把写事务内完成（`Db::with_mut` + `IMMEDIATE`），不会丢
//! 别的任务的更新 —— 写入本身也是低频动作（每轮一次，加上 30 秒一次的心跳）。
//!
//! ── 双实例共用同一个库 ──────────────────────────────────────
//! 桌面壳允许开发版与正式版并存（端口与 identifier 各自独立），两者共用同一个
//! 配置目录与数据库。因此「判定到期 → 占用」必须在一个写事务里完成
//! （`TransactionBehavior::Immediate` 先拿写锁再读），否则两个进程会在同一
//! 毫秒各发一次请求。
//!
//! ── 硬约束：持锁期间不打日志 ────────────────────────────────
//! 与 `config::sql` / `providers::catalog_cache` 同一条：`Db::with*` 拿的是
//! 全局唯一的连接锁，而 `logging::log` / `verbose` 要往同一个库写 `logs` 表
//! —— `std::sync::Mutex` 不可重入，在闭包里打日志会当场死锁。本模块的日志
//! 一律在闭包之外打（`now_ms` 只读时钟，不算 IO）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::server::db::Db;
use crate::server::logging;

/// `kv` 表里的键名（保留键，见 `db::schema::RESERVED_KV_KEYS`）
const KV_KEY: &str = "backgroundTaskState";

/// 执行占位的租约时长：心跳每 30 秒续一次，留三倍余量。
///
/// 为什么不是「任务预计最长多久」：一轮凭证维护可能跨十几个账号的网络请求，
/// 几分钟是正常的。租约只需满足「活着的那一轮不会被人抢走」，过期时间由心跳
/// 不断推后；真正的兜底场景是**进程被杀**，那时 90 秒后自动放行。
const LEASE_MS: i64 = 90_000;

/// 失败退避的上限（一天）：上游长期不可用时也要保证每天试一次，
/// 否则「上游恢复了我却还在等」会变成需要重启才能恢复的故障。
const MAX_BACKOFF_MS: i64 = 24 * 60 * 60_000;
static DB: OnceLock<Option<Db>> = OnceLock::new();
static SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TaskState {
    pub last_attempt_at: i64,
    pub last_run_at: i64,
    pub last_success_at: i64,
    pub last_result: Option<String>,
    pub last_error: Option<String>,
    pub next_run_at: i64,
    pub retry_at: i64,
    pub failures: u32,
    pub value: Option<Value>,
    interval_ms: i64,
    lease_until: i64,
    owner: String,
}

impl TaskState {
    pub fn running(&self) -> bool {
        !self.owner.is_empty() && self.lease_until > logging::now_ms()
    }

    pub fn due_at(&self) -> i64 {
        self.next_run_at.max(self.retry_at).max(self.lease_until)
    }

    pub fn interval_ms(&self) -> i64 { self.interval_ms }

    pub fn clock_needs_adjustment(&self) -> bool { self.last_attempt_at > logging::now_ms() + 60_000 }

    pub fn waiting_message(&self) -> String {
        if self.running() {
            return "任务正在执行中，请稍候".to_string();
        }
        let seconds = ((self.retry_at - logging::now_ms()).max(0) + 999) / 1000;
        if seconds > 0 {
            format!("请求处于冷却期，请在 {seconds} 秒后重试")
        } else {
            "刚刚已执行，请稍后重试".to_string()
        }
    }

    fn adjust_clock(&mut self, now: i64) {
        if self.last_attempt_at <= now + 60_000 {
            return;
        }
        // 系统时钟回拨时保留剩余等待量，不能把未来的旧时间当作无限期冷却。
        let shift = self.last_attempt_at - now;
        self.last_attempt_at = now;
        self.last_run_at = (self.last_run_at - shift).max(0);
        self.last_success_at = (self.last_success_at - shift).max(0);
        self.next_run_at = (self.next_run_at - shift).max(now);
        self.retry_at = (self.retry_at - shift).max(0);
        self.lease_until = (self.lease_until - shift).max(0);
    }

    fn renew_lease_at(&mut self, now: i64) {
        // 先把旧排期移到当前时钟，再写新租约，避免后续校时再次平移。
        self.adjust_clock(now);
        self.lease_until = now.saturating_add(LEASE_MS);
    }

    fn finish_at(
        &mut self,
        now: i64,
        success: bool,
        summary: String,
        value: Option<Value>,
        retry_at: i64,
        interval_ms: i64,
    ) {
        // 查询期间也可能回拨；旧时间必须在写入本次完成时间前一起校正。
        self.adjust_clock(now);
        self.last_run_at = now;
        self.last_result = Some(summary.clone());
        self.interval_ms = interval_ms;
        if success {
            self.last_success_at = now;
            self.last_error = None;
            self.failures = 0;
            self.retry_at = retry_at;
        } else {
            self.last_error = Some(summary);
            self.failures = self.failures.saturating_add(1);
            let multiplier = 1_i64 << self.failures.saturating_sub(1).min(10);
            let delay = interval_ms.max(60_000).saturating_mul(multiplier).min(MAX_BACKOFF_MS);
            self.retry_at = retry_at.max(now.saturating_add(delay));
        }
        if let Some(value) = value {
            self.value = Some(value);
        }
        self.next_run_at = now.saturating_add(interval_ms).max(self.retry_at);
        self.owner.clear();
        self.lease_until = 0;
    }
}

pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
}

fn database() -> Result<&'static Db, String> {
    DB.get()
        .and_then(Option::as_ref)
        .ok_or_else(|| "任务状态数据库不可用，未发送后台请求".to_string())
}

fn read_states(conn: &rusqlite::Connection) -> Result<HashMap<String, TaskState>, String> {
    let text: Option<String> = conn
        .query_row("SELECT value FROM kv WHERE key = ?1", [KV_KEY], |row| row.get(0))
        .optional()
        .map_err(|error| format!("读取任务状态失败: {error}"))?;
    match text {
        Some(text) => serde_json::from_str(&text)
            .map_err(|error| format!("任务状态格式错误: {error}")),
        None => Ok(HashMap::new()),
    }
}

/// 在调用方的写事务中核对执行所有权，拒绝租约替换后的迟到结果。
pub(crate) fn owns_run(conn: &rusqlite::Connection, key: &str, owner: &str) -> bool {
    !owner.is_empty() && read_states(conn).ok().and_then(|states| states.get(key).cloned()).is_some_and(|state| state.owner == owner)
}

pub fn read(key: &str) -> Result<TaskState, String> {
    Ok(read_all()?.get(key).cloned().unwrap_or_default())
}

pub fn read_all() -> Result<HashMap<String, TaskState>, String> {
    database()?.with(read_states).ok_or_else(|| "任务状态数据库不可用".to_string())?
}

fn change(
    key: &str,
    mutate: impl FnOnce(&mut TaskState),
) -> Result<TaskState, String> {
    database()?
        .with_mut(|conn| {
            // 开发版和正式版可共用数据库，先取得写事务再判定到期，避免双份执行。
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| format!("锁定任务状态失败: {error}"))?;
            let mut states = read_states(&tx)?;
            let state = states.entry(key.to_string()).or_default();
            mutate(state);
            let result = state.clone();
            let text = serde_json::to_string(&states)
                .map_err(|error| format!("编码任务状态失败: {error}"))?;
            tx.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![KV_KEY, text],
            )
            .map_err(|error| format!("保存任务状态失败: {error}"))?;
            tx.commit().map_err(|error| format!("提交任务状态失败: {error}"))?;
            Ok(result)
        })
        .ok_or_else(|| "任务状态数据库不可用".to_string())?
}

pub fn store_value(key: &str, value: Value) -> Result<(), String> {
    change(key, |state| state.value = Some(value)).map(|_| ())
}

/// 记录一次**外部发起**的真实请求（用户手动点的那一轮）：把定时排期顺延一个
/// 间隔，避免「刚点完、到点或重启后又立刻再跑一遍」。
///
/// 正在执行的那一轮（占位仍有效）不动 —— 那正是它自己的排期，由 `RunGuard`
/// 在收尾时统一写。失败也不改冷却：手动动作不该把上游的限流冷却提前解除。
pub fn note_external_run(key: &str, interval_ms: i64) -> Result<(), String> {
    change(key, |state| {
        if state.running() {
            return;
        }
        let now = logging::now_ms();
        state.adjust_clock(now);
        state.last_attempt_at = now;
        state.next_run_at = now.saturating_add(interval_ms).max(state.retry_at);
    })
    .map(|_| ())
}

/// 把排期置为「现在就到期」（用户在界面上刚把任务从关闭拨到开启时调用）。
///
/// 与 `note_external_run` 的区别是**要不要顺延**：那一条是「刚真跑过一轮」，
/// 这一条是「用户明确要求这条任务开始工作」—— 后者保留改造前「开启即跑一次」
/// 的语义，不受上次尝试时间影响（只有失败冷却仍然优先：`due_at` 取两者的较大值）。
pub fn schedule_now(key: &str) -> Result<(), String> {
    change(key, |state| {
        let now = logging::now_ms();
        state.adjust_clock(now);
        state.next_run_at = now;
    })
    .map(|_| ())
}

/// 清除一条任务的**失败冷却**（`retry_at` 归零），不动排期与最近尝试时间。
///
/// 用途：调用方确认「上次失败的前提条件已经变了」—— 对更新检查就是换出网线路 /
/// 换 GitHub 令牌（冷却记的是某一个配额桶的恢复时刻，匿名按出口 IP 计、带令牌
/// 按用户计，换了桶之后旧冷却不再适用）。清掉它让用户立刻能重试一次，而不是
/// 对着界面上好端端的「检查更新」按钮空等几十分钟。
///
/// 排期（`next_run_at`）**保持不动**：定时任务仍按自己的间隔跑，这条函数只解开
/// 「现在不让试」的那把锁。`adjust_clock` 与其它入口一样先跑一遍，防系统时钟
/// 回拨把旧时间当成无限期冷却。
pub fn clear_cooldown(key: &str) -> Result<(), String> {
    change(key, |state| {
        let now = logging::now_ms();
        state.adjust_clock(now);
        state.retry_at = 0;
    })
    .map(|_| ())
}

/// 改间隔以最近一次尝试/完成为起点，不因重启或开关切换重新计时。
pub fn reschedule(key: &str, interval_ms: i64) -> Result<(), String> {
    change(key, |state| {
        let now = logging::now_ms();
        state.adjust_clock(now);
        state.interval_ms = interval_ms;
        let anchor = state.last_run_at.max(state.last_attempt_at);
        state.next_run_at = if anchor > 0 { anchor.saturating_add(interval_ms) } else { now };
        state.next_run_at = state.next_run_at.max(state.retry_at);
    })
    .map(|_| ())
}

/// 逐账号排期首次继承旧全局/余额记录；改间隔仍保留失败冷却与在途租约。
pub fn initialize_schedule(key: &str, initial: &TaskState, interval_ms: i64) -> Result<(), String> {
    change(key, |state| {
        if state.interval_ms == 0 && state.last_attempt_at == 0 {
            state.last_attempt_at = initial.last_attempt_at;
            state.last_run_at = initial.last_run_at;
            state.last_success_at = initial.last_success_at;
            state.retry_at = initial.retry_at;
            state.failures = initial.failures;
        }
        state.adjust_clock(logging::now_ms());
        if state.interval_ms != interval_ms && !state.running() {
            state.interval_ms = interval_ms;
            let anchor = state.last_run_at.max(state.last_attempt_at);
            state.next_run_at = anchor.saturating_add(interval_ms).max(state.retry_at);
        }
    }).map(|_| ())
}

pub enum Claim {
    Acquired(RunGuard),
    Deferred(TaskState),
}

/// 手动触发**越不越过失败冷却**（[`claim`] 的显式入参）。
///
/// 两种语义在仓库里同时存在，而且各有理由 —— 所以它不藏在 `manual` 里顺带决定：
///   - [`ManualBackoff::Respect`]：冷却记的是**上游配额桶什么时候恢复**
///     （检查更新的 GitHub 限额匿名按出口 IP 计、带令牌按用户计）。提前打一次
///     只会再吃一次 403，还把恢复时刻重新顶到未来，不如如实告诉用户还要等多久；
///   - [`ManualBackoff::Bypass`]：冷却记的是**上一轮为什么没成功**（模型目录按
///     上游逐个 401 / 5xx 退避；凭证维护按账号失败退避）。用户按下
///     「获取模型」/「立即执行」的预期就是「现在真打一次」，而且按按钮往往正是
///     因为刚把那个原因修好（重新导入登录态、换账号、把坏账号删了）—— 继续拿旧
///     结论挡着，界面上只会留着上一次的错误文案，看起来就是按钮坏了。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManualBackoff {
    Respect,
    Bypass,
}

/// 判定到期并占位（两种结果都是 `Ok`：Deferred 不是错误，只是这轮不该跑）。
///
/// `manual`（用户主动触发）跳过普通排期；**是否连失败冷却一起跳过**由 `backoff`
/// 决定（见 [`ManualBackoff`]，逐个调用点自选）。
///
/// 在途占位与最短请求间隔**手动也不越过**：前者防同一时刻两个入口（或两个进程）
/// 并排打上游，后者是「同一家一秒内不重复打」的底线 —— 冷却可以商量，并发与连点
/// 不行：用户连点按钮时，真正保护上游的是这两条，而不是那个动辄几小时的冷却。
pub fn claim(
    key: &str,
    interval_ms: i64,
    manual: bool,
    backoff: ManualBackoff,
    min_gap_ms: i64,
) -> Result<Claim, String> {
    let now = logging::now_ms();
    let owner = format!(
        "{}-{now}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let mut acquired = false;
    let state = change(key, |state| {
        state.adjust_clock(now);
        let earliest = state.last_attempt_at.saturating_add(min_gap_ms);
        let cooling = state.retry_at > now && !(manual && backoff == ManualBackoff::Bypass);
        if state.running() || now < earliest || cooling || (!manual && now < state.next_run_at) {
            return;
        }
        acquired = true;
        state.last_attempt_at = now;
        state.interval_ms = interval_ms;
        state.next_run_at = now.saturating_add(interval_ms);
        state.lease_until = now.saturating_add(LEASE_MS);
        state.owner = owner.clone();
    })?;
    if !acquired {
        return Ok(Claim::Deferred(state));
    }
    let heartbeat_key = key.to_string();
    let heartbeat_owner = owner.clone();
    let heartbeat = crate::spawn_task(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let result = change(&heartbeat_key, |state| {
                if state.owner == heartbeat_owner {
                    state.renew_lease_at(logging::now_ms());
                }
            });
            if !matches!(result, Ok(state) if state.owner == heartbeat_owner) {
                break;
            }
        }
    });
    Ok(Claim::Acquired(RunGuard {
        key: key.to_string(),
        owner,
        heartbeat,
        finished: false,
    }))
}

pub struct RunGuard {
    key: String,
    owner: String,
    heartbeat: tokio::task::JoinHandle<()>,
    finished: bool,
}

impl RunGuard {
    pub(crate) fn ownership(&self) -> (&str, &str) { (&self.key, &self.owner) }

    pub fn finish(
        mut self,
        success: bool,
        summary: String,
        value: Option<Value>,
        retry_at: i64,
        interval_ms: i64,
    ) -> Result<TaskState, String> {
        self.heartbeat.abort();
        let result = change(&self.key, |state| {
            if state.owner != self.owner {
                return;
            }
            state.finish_at(logging::now_ms(), success, summary, value, retry_at, interval_ms);
        });
        self.finished = result.is_ok();
        result
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if self.finished {
            return;
        }
        let _ = change(&self.key, |state| {
            if state.owner == self.owner {
                state.owner.clear();
                state.lease_until = 0;
                state.last_result = Some("上次执行中断，保留原有排期".to_string());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OLD_NOW: i64 = 4_000_000;
    const ROLLED_BACK_NOW: i64 = 405_000;
    const INTERVAL_MS: i64 = 600_000;

    fn active_run() -> TaskState {
        TaskState {
            last_attempt_at: OLD_NOW,
            last_run_at: OLD_NOW - 10_000,
            last_success_at: OLD_NOW - 20_000,
            last_result: Some("旧结果".into()),
            last_error: Some("旧失败".into()),
            next_run_at: OLD_NOW + INTERVAL_MS,
            retry_at: OLD_NOW + 500_000,
            failures: 2,
            value: Some(json!({"saved": true})),
            interval_ms: INTERVAL_MS,
            lease_until: OLD_NOW + LEASE_MS,
            owner: "fixture-owner".into(),
        }
    }

    fn assert_next_tick_preserves_state(state: &mut TaskState, now: i64) {
        let before = serde_json::to_value(&*state).unwrap();
        state.adjust_clock(now + 10_000);
        assert_eq!(serde_json::to_value(&*state).unwrap(), before);
    }

    #[test]
    fn finish_success_after_clock_rollback_preserves_interval() {
        for now in [ROLLED_BACK_NOW, OLD_NOW + 5_000] {
            let mut state = active_run();
            state.finish_at(now, true, "查询完成".into(), Some(json!({"saved": "new"})), 0, INTERVAL_MS);
            assert_eq!(state.last_attempt_at, now.min(OLD_NOW));
            assert_eq!(state.last_run_at, now);
            assert_eq!(state.last_success_at, now);
            assert_eq!(state.last_result.as_deref(), Some("查询完成"));
            assert_eq!(state.last_error, None);
            assert_eq!(state.failures, 0);
            assert_eq!(state.retry_at, 0);
            assert_eq!(state.next_run_at, now + INTERVAL_MS);
            assert_eq!(state.value, Some(json!({"saved": "new"})));
            assert!(state.owner.is_empty());
            assert_eq!(state.lease_until, 0);
            assert_next_tick_preserves_state(&mut state, now);
            assert_eq!(state.due_at(), now + INTERVAL_MS);
        }
    }

    #[test]
    fn finish_failure_after_clock_rollback_preserves_backoff() {
        for now in [ROLLED_BACK_NOW, OLD_NOW + 5_000] {
            let mut state = active_run();
            state.finish_at(now, false, "查询失败".into(), None, 0, INTERVAL_MS);
            assert_eq!(state.last_attempt_at, now.min(OLD_NOW));
            assert_eq!(state.last_run_at, now);
            assert_eq!(state.last_success_at, now.min(OLD_NOW) - 20_000);
            assert_eq!(state.last_result.as_deref(), Some("查询失败"));
            assert_eq!(state.last_error.as_deref(), Some("查询失败"));
            assert_eq!(state.failures, 3);
            assert_eq!(state.retry_at, now + 4 * INTERVAL_MS);
            assert_eq!(state.next_run_at, state.retry_at);
            assert_eq!(state.value, Some(json!({"saved": true})));
            assert!(state.owner.is_empty());
            assert_eq!(state.lease_until, 0);
            assert_next_tick_preserves_state(&mut state, now);
            assert_eq!(state.due_at(), now + 4 * INTERVAL_MS);
        }
    }

    #[test]
    fn heartbeat_after_clock_rollback_preserves_active_lease() {
        for now in [ROLLED_BACK_NOW, OLD_NOW + 5_000] {
            let mut state = active_run();
            state.renew_lease_at(now);
            assert_eq!(state.last_attempt_at, now.min(OLD_NOW));
            assert_eq!(state.last_run_at, now.min(OLD_NOW) - 10_000);
            assert_eq!(state.last_success_at, now.min(OLD_NOW) - 20_000);
            assert_eq!(state.next_run_at, now.min(OLD_NOW) + INTERVAL_MS);
            assert_eq!(state.retry_at, now.min(OLD_NOW) + 500_000);
            assert_eq!(state.lease_until, now + LEASE_MS);
            assert_eq!(state.owner, "fixture-owner");
            assert_eq!(state.failures, 2);
            assert_eq!(state.last_result.as_deref(), Some("旧结果"));
            assert_eq!(state.last_error.as_deref(), Some("旧失败"));
            assert_eq!(state.value, Some(json!({"saved": true})));
            assert_next_tick_preserves_state(&mut state, now);
            assert!(state.lease_until > now + 10_000);
            state.renew_lease_at(now + 30_000);
            assert_eq!(state.lease_until, now + 30_000 + LEASE_MS);
            assert_eq!(state.last_attempt_at, now.min(OLD_NOW));
        }
    }
}
