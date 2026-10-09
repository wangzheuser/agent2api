//! 每账号的**余额查询记录**（`account_usage_records` 表）与选路用的**内存事实表**。
//!
//! ── 这个模块解决什么问题 ─────────────────────────────────────
//! 全局开关与默认间隔继续生效，余额查询按账号各自到期触发（`core::usage_query`
//! 的心跳循环）。每账号一条记录（一账号一行，主键即账号 id，重复写入天然是
//! UPSERT）取代旧 kv 快照成为「这个账号最近一次查到多少」的权威存放处：
//!   - **逐条更新**：某个账号到点只写它自己那一行，不再整份快照读改写；
//!   - **选路零 IO**：转发选路（余额不足跳过）每条请求都要比一次余额，
//!     内存里那份 `BalanceFact`（账号 id → 最近成功读数的数字）让它变成
//!     纳秒级的查表 —— 从 JSON 现场解析或整份快照读库都撑不住这个频率。
//!
//! ── 失败保留上次成功的数值（OmniProxy 同一取舍）──────────────
//! `remaining` 只在查询**成功**时覆写：失败行保留上次成功的数字，欠费状态
//! 不会因为「这次查询失败」被放行（避免「查询失败 → 放行 → 402」的窗口期）；
//! 恢复需要查询成功且数值回到阈值之上。失败本身的展示信息在 `error` / `code`
//! 两列，与数值互不覆盖。首次失败（此前从未成功过）没有可保留的数值，
//! `remaining` 为 NULL —— 选路判定「判不出」一律放行，不猜。
//!
//! 请求前的占位、租约与失败退避由 task_state 管理，余额记录只保存查询事实。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::server::core::account_store::store_util::js_truthy;
use crate::server::db::Db;
use crate::server::logging;

/// 旧 kv 快照键（全局「定时查询积分」时代的存放处）。
/// 保留在 `db::schema::RESERVED_KV_KEYS` 里：迁移中断（导入失败不删键）时，
/// 配置写入的「删除已不存在的键」不能把它误清 —— 那是唯一还能重试的来源。
const LEGACY_SNAPSHOT_KEY: &str = "usageQuerySnapshot";

static DB: OnceLock<Option<Db>> = OnceLock::new();

/// 余额事实：选路跳过判定读的**内存副本**（账号 id → 最近一次成功读数）。
#[derive(Clone, Debug)]
pub struct BalanceFact {
    pub identity: String,
    /// 最近一次成功查询的余额数字（与余额列同一口径）。`None` = 判不出
    /// （unlimited、无读数、形状不认）—— 跳过判定一律放行。
    pub remaining: Option<f64>,
}

fn facts_slot() -> &'static Mutex<HashMap<String, BalanceFact>> {
    static FACTS: OnceLock<Mutex<HashMap<String, BalanceFact>>> = OnceLock::new();
    FACTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 一条余额查询记录（读侧形状；行不存在时各字段取缺省值）。
#[derive(Clone, Debug)]
pub struct UsageRecord {
    pub identity: String,
    pub queried_at: i64,
    /// 最近一次**成功**查询的归一化余额 JSON（余额列的渲染原料）。
    pub usage: Option<Value>,
    /// 最近一次失败的原因（成功行清空）。
    pub error: Option<String>,
    /// 失败的机器可识别标记（如「未配置查询凭证」）。
    pub code: Option<String>,
    /// 数值投影（成功时从 usage 提出；失败保留上次成功值；判不出为 NULL）。
    pub remaining: Option<f64>,
    pub unlimited: bool,
    /// 最近一次成功 / 尝试的时刻（毫秒；0 = 从未）。
    pub last_success_at: i64,
    pub last_attempt_at: i64,
}

/// 一次查询的**结果**（成功带 usage，失败带 error；两者都不给 = 行为未定义，
/// 调用方保证二选一）。
pub struct UsageOutcome {
    pub usage: Option<Value>,
    pub error: Option<String>,
    pub code: Option<String>,
}

pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
    migrate_legacy_snapshot();
    sync_facts();
}

fn database() -> Option<&'static Db> {
    DB.get().and_then(Option::as_ref)
}

// ─── 读 ──────────────────────────────────────────────────────

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageRecord> {
    let usage_text: Option<String> = row.get("usage")?;
    Ok(UsageRecord {
        identity: row.get("identity")?,
        queried_at: row.get("queried_at")?,
        usage: usage_text.and_then(|text| serde_json::from_str(&text).ok()),
        error: row.get("error")?,
        code: row.get("code")?,
        remaining: row.get::<_, Option<f64>>("remaining")?,
        unlimited: row.get::<_, i64>("unlimited").unwrap_or(0) != 0,
        last_success_at: row.get("last_success_at").unwrap_or(0),
        last_attempt_at: row.get("last_attempt_at").unwrap_or(0),
    })
}

const RECORD_COLUMNS: &str =
    "usage, error, code, remaining, unlimited, last_success_at, last_attempt_at, identity, queried_at";

/// 某账号的最近一条记录（没有 = 全缺省：从未查过）。
pub fn load(account_id: &str) -> UsageRecord {
    let fallback = UsageRecord {
        identity: String::new(),
        queried_at: 0,
        usage: None,
        error: None,
        code: None,
        remaining: None,
        unlimited: false,
        last_success_at: 0,
        last_attempt_at: 0,
    };
    let Some(db) = database() else { return fallback };
    db.with(|conn| {
        conn.query_row(
            &format!(
                "SELECT {RECORD_COLUMNS} FROM account_usage_records WHERE account_id = ?1"
            ),
            [account_id],
            row_to_record,
        )
        .optional()
        .unwrap_or_default()
    })
    .flatten()
    .unwrap_or(fallback)
}

/// 全部记录（快照接口的组装原料）。库不可用时给空表 —— 上层按「没查过」降级。
pub fn load_all() -> HashMap<String, UsageRecord> {
    let Some(db) = database() else { return HashMap::new() };
    db.with(|conn| {
        let Ok(mut statement) = conn.prepare(&format!(
            "SELECT account_id, {RECORD_COLUMNS} FROM account_usage_records"
        )) else {
            return HashMap::new();
        };
        let rows = statement.query_map([], |row| {
            let account_id: String = row.get("account_id")?;
            Ok((account_id, row_to_record(row)?))
        });
        let Ok(rows) = rows else { return HashMap::new() };
        rows.filter_map(|item| item.ok()).collect::<HashMap<_, _>>()
    })
    .unwrap_or_default()
}

/// 选路用的余额事实快照（内存查表，调用方克隆一份小 Map）。
pub fn balance_facts() -> HashMap<String, BalanceFact> {
    facts_slot()
        .lock()
        .map(|facts| facts.clone())
        .unwrap_or_default()
}

// ─── 写 ──────────────────────────────────────────────────────

/// 写入一次查询的结果（成功覆写数值，失败只写原因 —— 见模块头的取舍）。
/// 内存事实表同步更新：成功且有数字 → 刷新；成功但判不出（unlimited / 形状不认）
/// → 摘除（不能让一个过期的旧数字把账号永久挡在选路之外）。
/// 身份不包含轮换令牌；创建时间用于拒绝同 ID 重建账号的迟到结果。
pub fn identity(account: &Value) -> String {
    let mut fields: Vec<Value> = ["id", "provider", "edition", "uid", "userId", "enterpriseId", "addedAt"]
        .iter().map(|key| account.get(*key).cloned().unwrap_or(Value::Null)).collect();
    fields.push(Value::String(account.get("account").and_then(Value::as_str).unwrap_or("").trim().to_string()));
    fields.push(account.get("usageIdentity").cloned().unwrap_or(Value::Null));
    format!("{:x}", Sha256::digest(serde_json::to_vec(&(fields, super::workbuddy_policy::identity(account))).unwrap_or_default()))
}

pub(crate) fn write_result_in(conn: &rusqlite::Connection, account: &Value, outcome: &UsageOutcome, queried_at: i64, attempted_at: i64, current_run: bool) -> rusqlite::Result<bool> {
    let Some(id) = account.get("id").and_then(Value::as_str) else { return Ok(false) };
    let identity = identity(account);
    let usage = outcome.usage.as_ref().map(Value::to_string);
    let (remaining, unlimited) = outcome.usage.as_ref().map(extract_remaining).unwrap_or((None, false));
    let written = conn.execute(
        "INSERT INTO account_usage_records
         (account_id,usage,error,code,remaining,unlimited,last_success_at,last_attempt_at,updated_at,identity,queried_at)
         VALUES (?1,?2,?3,?4,?5,?6,CASE WHEN ?2 IS NULL THEN 0 ELSE ?7 END,?9,?7,?8,?7)
         ON CONFLICT(account_id) DO UPDATE SET
         usage=CASE WHEN excluded.usage IS NOT NULL OR identity<>excluded.identity THEN excluded.usage ELSE usage END,
         remaining=CASE WHEN excluded.usage IS NOT NULL OR identity<>excluded.identity THEN excluded.remaining ELSE remaining END,
         unlimited=CASE WHEN excluded.usage IS NOT NULL OR identity<>excluded.identity THEN excluded.unlimited ELSE unlimited END,
         last_success_at=CASE WHEN excluded.usage IS NOT NULL OR identity<>excluded.identity THEN excluded.last_success_at ELSE last_success_at END,
         error=excluded.error,code=excluded.code,last_attempt_at=excluded.last_attempt_at,
         updated_at=excluded.updated_at,identity=excluded.identity,queried_at=excluded.queried_at
         WHERE ?10 OR last_attempt_at<=excluded.last_attempt_at",
        params![id,usage,outcome.error,outcome.code,remaining,unlimited as i64,queried_at,identity,attempted_at,current_run],
    )?;
    Ok(written == 1)
}

pub(crate) fn refresh_fact(id: &str) {
    if let Ok(mut facts) = facts_slot().lock() {
        let record = load(id);
        facts.insert(id.to_string(), BalanceFact { identity: record.identity, remaining: record.remaining });
    }
}

pub fn write_result(account: &Value, outcome: &UsageOutcome, queried_at: i64) -> bool {
    let Some(id) = account.get("id").and_then(Value::as_str) else { return false };
    let Some(db) = database() else { return false };
    let written = db.with_mut(|conn| write_result_in(conn, account, outcome, queried_at, queried_at, false)).is_some_and(|result| matches!(result, Ok(true)));
    if written { refresh_fact(id); }
    written
}

/// 清掉「账号已被删除」的孤儿行（心跳循环每轮顺手一次，一条 SQL）。
pub fn prune_orphans() {
    let Some(db) = database() else { return };
    db.with_mut(|conn| {
        conn.execute(
            "DELETE FROM account_usage_records
             WHERE account_id NOT IN (SELECT id FROM accounts)",
            [],
        )
        .ok()
    });
}

// ─── 余额数字的提取口径（与余额列同源）─────────────────────

/// 归一化余额 JSON → `(剩余数字, unlimited)`。
///
/// 口径与前端余额列逐字同源（`accounts-panels.tsx` 的 `usageSummary`）：
///   - workbuddy 既有形状认 `totalLeft` 键，`unlimited` 真值 = ∞（判不出数字）；
///   - 归一化形状认 `available`；
///   - 两者都判不出（字段缺失 / 非数字）→ `None`，选路按「无法判定」放行。
/// 「按字段形状探测而不是按 provider 分派」的理由与前端同一句：provider 只
/// 决定谁去查，不决定查回来长什么样。
pub fn extract_remaining(usage: &Value) -> (Option<f64>, bool) {
    let Some(fields) = usage.as_object() else { return (None, false) };
    let unlimited = fields.get("unlimited").map(js_truthy).unwrap_or(false);
    if unlimited {
        return (None, true);
    }
    if let Some(total_left) = fields.get("totalLeft") {
        return (total_left.as_f64().filter(|value| value.is_finite()), false);
    }
    if let Some(available) = fields.get("available") {
        return (available.as_f64().filter(|value| value.is_finite()), false);
    }
    (None, false)
}

// ─── 账号上的查询配置读取（调度与选路共用的判定口径）─────────

/// 每账号自动查询的间隔上下限（秒）：30 秒 ~ 1 天。写入侧
/// （`account_store::apply_patch`）与读取侧共用这一对常量 —— 两处各写一份
/// 「迟早会漂」。
pub const MIN_QUERY_INTERVAL_SECONDS: i64 = 30;
pub const MAX_QUERY_INTERVAL_SECONDS: i64 = 86_400;

/// 未设置账号策略时保留本地行为：继承全局查询配置，低余额处理关闭。
pub const DEFAULT_LOW_BALANCE_THRESHOLD: f64 = 1.0;

pub fn default_low_balance_mode(_provider: &str) -> &'static str {
    "off"
}

/// 缺省档的完整形状（mode + threshold）：公开形态（store_view）与写入侧
/// 归一化（store_crud）共用，保证三处「缺省」永远是同一个对象。
/// off 档阈值归零存放（与 `apply_patch` 的 off 形态一致）。
pub fn default_low_balance(provider: &str) -> Value {
    if default_low_balance_mode(provider) == "off" {
        serde_json::json!({ "mode": "off", "threshold": 0.0 })
    } else {
        serde_json::json!({ "mode": "skip", "threshold": DEFAULT_LOW_BALANCE_THRESHOLD })
    }
}

/// 逐账号显式字段覆盖全局继承值；秒与全局分钟在这里转换。
pub fn query_settings(value: Option<&Value>) -> Value {
    let inherited = crate::server::config::scheduled_settings().usage_query;
    let enabled = value.and_then(|value| value.get("enabled")).and_then(Value::as_bool).unwrap_or(inherited.enabled);
    let interval = value.and_then(|value| value.get("interval")).and_then(Value::as_i64)
        .filter(|value| (MIN_QUERY_INTERVAL_SECONDS..=MAX_QUERY_INTERVAL_SECONDS).contains(value))
        .unwrap_or(inherited.interval * 60);
    serde_json::json!({"enabled": enabled, "interval": interval})
}

pub fn query_interval_of(account: &Value) -> Option<i64> {
    if !crate::server::config::scheduled_settings().usage_query.enabled { return None; }
    let settings = query_settings(account.get("usageQuery"));
    (settings["enabled"] == true).then(|| settings["interval"].as_i64().unwrap_or(600))
}

/// 「余额不足自动禁用」档的阈值。仅 `lowBalance.mode == "disable"` 的账号有值
/// —— **缺省（无配置）不启用禁用**：自动禁用是不自动恢复的硬动作，缺省必须是
/// 用户显式选过才会发生；缺省档为关闭。
/// 阈值非法（非正 / 非有限数）一律 None —— 判不出就放行，不猜。
pub fn low_balance_disable_threshold(account: &Value) -> Option<f64> {
    let config = account.get("lowBalance")?;
    if config.get("mode").and_then(Value::as_str) != Some("disable") {
        return None;
    }
    valid_threshold(config.get("threshold"))
}

/// 账号是否应因「余额不足」在选路时被跳过（软跳过档）。
///
/// 条件：处理方式为 `skip` + 阈值合法 + 内存事实里有这个账号的读数且**严格小于**
/// 阈值（等于阈值仍可用）。缺省不处理，显式 skip 才参与过滤。
/// 判不出的情况 —— 显式 off、无读数、unlimited、账号已删 —— 一律放行：
/// 跳过是对「这个账号此刻没钱」的断言，断言拿不出证据就不能拦请求。
pub fn balance_blocked(account: &Value, facts: &HashMap<String, BalanceFact>) -> bool {
    let provider = account.get("provider").and_then(Value::as_str).unwrap_or("");
    let default_mode = default_low_balance_mode(provider);
    let (mode, threshold) = match account.get("lowBalance") {
        // 缺省关闭，显式选择后才参与余额过滤
        None => (default_mode, DEFAULT_LOW_BALANCE_THRESHOLD),
        Some(config) => {
            let mode = config
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or(default_mode);
            let threshold = valid_threshold(config.get("threshold"))
                .unwrap_or(DEFAULT_LOW_BALANCE_THRESHOLD);
            (mode, threshold)
        }
    };
    if mode != "skip" {
        return false;
    }
    let Some(id) = account.get("id").and_then(Value::as_str) else {
        return false;
    };
    let Some(fact) = facts.get(id) else {
        return false;
    };
    fact.identity == identity(account) && matches!(fact.remaining, Some(remaining) if remaining < threshold)
}

/// 阈值的合法性口径：有限且 > 0。DB / JSON 里可能有手工脏值，宁可放行也不猜。
fn valid_threshold(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|threshold| threshold.is_finite() && *threshold > 0.0)
}

// ─── 旧 kv 快照的一次性迁移 ─────────────────────────────────

/// 全局任务时代的快照（kv `usageQuerySnapshot`）导入本表后删除该键。
///
/// 为什么在启动时做：升级后余额列不能变空白 —— 旧快照里的每行结论（含失败行）
/// 仍是「那次查询的事实」，导入后界面照常按「上次读数」展示，直到各自的
/// 下一次到期查询把它们逐个刷新。
///
/// 原子性：读键、逐行导入、删键在一个事务里完成，中断则整体回滚、下次启动
/// 重试。失败只记 verbose（旧键还在，数据没丢），不阻断启动。
fn migrate_legacy_snapshot() {
    let Some(db) = database() else { return };
    let imported = db.with_mut(migrate_snapshot_rows);
    match imported {
        Some(Ok(0)) => { /* 没有旧快照（全新安装或已迁移）：无事可做 */ }
        Some(Ok(count)) => {
            logging::log("[Usage]", &format!("已把旧版余额快照的 {count} 条结果迁入新记录表"));
        }
        other => {
            logging::verbose(
                "[Usage]",
                &format!("旧余额快照迁移未完成（下次启动重试）：{other:?}"),
            );
        }
    }
}

fn migrate_snapshot_rows(conn: &mut rusqlite::Connection) -> Result<usize, String> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|error| error.to_string())?;
    let read = |key: &str| -> Result<Option<Value>, String> {
        let text: Option<String> = tx.query_row("SELECT value FROM kv WHERE key=?1", [key], |row| row.get(0)).optional().map_err(|error| error.to_string())?;
        text.map(|text| serde_json::from_str(&text).map_err(|error| error.to_string())).transpose()
    };
    let mut background = read("backgroundTaskState")?;
    let standalone = read(LEGACY_SNAPSHOT_KEY)?;
    let nested = background.as_ref().and_then(|value| value.get(LEGACY_SNAPSHOT_KEY)).and_then(|state| state.get("value")).cloned();
    let mut imported = 0;
    for snapshot in standalone.iter().chain(nested.iter()) {
        let at = snapshot.get("at").and_then(Value::as_i64).unwrap_or(0);
        let rows = snapshot.get("results").and_then(Value::as_array).ok_or("旧余额快照缺少结果数组")?;
        for row in rows {
            let Some(id) = row.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) else { continue };
            let usage = row.get("usage").filter(|value| !value.is_null());
            let (remaining, unlimited) = usage.map(extract_remaining).unwrap_or((None, false));
            let queried_at = row.get("queriedAt").and_then(Value::as_i64).filter(|at| *at > 0)
                .or_else(|| usage.and_then(|usage| usage.pointer("/creditDetails/fetchedAt")).and_then(Value::as_i64).filter(|at| *at > 0)).unwrap_or(at.max(0));
            tx.execute("INSERT INTO account_usage_records
                (account_id,usage,error,code,remaining,unlimited,last_success_at,last_attempt_at,updated_at,queried_at)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8,?8)
                ON CONFLICT(account_id) DO UPDATE SET
                usage=CASE WHEN excluded.last_success_at>last_success_at THEN excluded.usage ELSE usage END,
                remaining=CASE WHEN excluded.last_success_at>last_success_at THEN excluded.remaining ELSE remaining END,
                unlimited=CASE WHEN excluded.last_success_at>last_success_at THEN excluded.unlimited ELSE unlimited END,
                last_success_at=MAX(last_success_at,excluded.last_success_at),
                error=CASE WHEN excluded.queried_at>queried_at THEN excluded.error ELSE error END,
                code=CASE WHEN excluded.queried_at>queried_at THEN excluded.code ELSE code END,
                last_attempt_at=MAX(last_attempt_at,excluded.last_attempt_at),updated_at=MAX(updated_at,excluded.updated_at),queried_at=MAX(queried_at,excluded.queried_at)
                WHERE identity='' AND (queried_at<excluded.queried_at OR last_success_at<excluded.last_success_at)",
                params![id,usage.map(Value::to_string),row.get("error").and_then(Value::as_str),row.get("code").and_then(Value::as_str),remaining,unlimited as i64,if usage.is_some() {queried_at} else {0},queried_at]
            ).map_err(|error| error.to_string())?;
            imported += 1;
        }
    }
    if nested.is_some() {
        if let Some(object) = background.as_mut().and_then(Value::as_object_mut) { object.remove(LEGACY_SNAPSHOT_KEY); }
        tx.execute("UPDATE kv SET value=?1 WHERE key='backgroundTaskState'", [background.unwrap().to_string()]).map_err(|error| error.to_string())?;
    }
    if standalone.is_some() { tx.execute("DELETE FROM kv WHERE key=?1", [LEGACY_SNAPSHOT_KEY]).map_err(|error| error.to_string())?; }
    tx.commit().map_err(|error| error.to_string())?;
    Ok(imported)
}

/// 启动时把表里的成功读数装进内存事实表（选路的初始视野）。
pub fn sync_facts() {
    let Ok(mut slot) = facts_slot().lock() else { return };
    let facts: HashMap<String, BalanceFact> = load_all()
        .into_iter()
        .filter(|(_, record)| record.usage.is_some())
        .filter_map(|(id, record)| {
            record
                .remaining
                .map(|remaining| (id, BalanceFact { identity: record.identity.clone(), remaining: Some(remaining) }))
        })
        .collect();
    *slot = facts;
}
