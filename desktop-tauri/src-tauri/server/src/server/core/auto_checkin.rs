//! 定时签到 —— 每天在指定时刻执行一次「全部账号签到」。
//!
//! 对照 Node 版 src/workbuddy-auto-checkin.mjs 全量移植。与 /api/accounts/checkin
//! 用的是同一段签到逻辑（`core::billing::checkin::run_checkin`），
//! 因此范围过滤、版本分流、串行防风的规则完全一致，不存在两套行为。
//!
//! ── 触发方式：轮询 + 当天去重（不是单个定时器）────────────────
//! Node 版用 `setInterval(tick, 30s)`：单个 setTimeout 在系统休眠、锁屏、时钟被改
//! 之后会漂移甚至整段错过，而每 30 秒比一次「当前是否已过今天的触发点」，
//! 唤醒后自然补上。Rust 侧用等价的 `sleep(30s)` 循环，每个 tick 都从**墙上时钟**
//! （北京时间，见 `core::beijing`）重新判定，因此与 Node 一样具备自愈能力：
//! libuv 与 tokio 的定时器都基于单调时钟，机器休眠期间都不推进，
//! 醒来的第一次 tick 会把错过的时点补上 —— 两者在这点上是同一套语义。
//!
//! ── 时区口径：固定 UTC+8，不跟机器时区（issue #138）────────────
//! 「今天是否已触发」与触发时刻都按**北京时间**算（`core::beijing`）。
//! 启动补签保留本地窗口门控：配置触发时刻未到时不提前领取，避免 Qoder
//! 国际版在 10:00 活动窗口前误记上一窗口；轮询与启动补签共用 `due_now`。
//!
//! ── 配置读写 ────────────────────────────────────────────────
//! 状态存在 config.json 的 `autoCheckin` 字段（`{enabled, time, lastFiredDate,
//! lastResult}`）。Node 每次 getState/tick 都重读 config.json；Rust 侧读
//! `config::current()` 的内存快照（config.rs 已经全量保留未知字段，写回也走它），
//! 于是「别人写进 config.json 的字段不会被吃掉」这条契约继续成立。
//! 差异：Node 会看到用户手工改盘上的 config.json 后的新值，Rust 的内存快照
//! 只在启动与写入时刷新 —— 手工改文件需要重启（与 config.rs 的既有设计一致）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, FixedOffset, TimeZone};
use serde_json::{json, Map, Value};

use crate::server::config;
use crate::server::core::account_store::AccountStore;
use crate::server::core::beijing;
use crate::server::core::billing::checkin;
use crate::server::core::billing::BillingService;
use crate::server::logging;

/// 轮询间隔：30 秒足够精确到分钟，又不会让计时器显得忙（Node 版 TICK_MS）
pub const TICK_MS: u64 = 30_000;

/// 默认触发时刻：零点一分（Node 版 DEFAULT_TIME）
pub const DEFAULT_TIME: &str = "00:01";

/// 可勾选的签到提供商（界面上的复选框）。默认全选。
///
///   - **WorkBuddy 国内版**：腾讯的每日签到接口；
///   - **WorkBuddy 国际版**：每日活跃任务（活动探测 + 条件领取 + 免费模型保活，
///     `billing::activity` 的 `workbuddy_daily_activity`）。国际版没有国内版的
///     普通签到接口，但上游客户端把这条活跃链接在同一个每日调度上 —— 保活的
///     成功不落 `checkinAt`、单独统计在 `active`；免费模型链可在签到中心自定义
///     （`billing::keepalive`）。
///   - **小浣熊**：「桌面登录积分」链路（`providers::raccoon` 的每日积分发放）；
///     首次登录奖励（电脑端 / 手机端各一条端点）是一次性新手福利，不在这条
///     每日链路里（见 `providers::raccoon::onboarding`，挂新手任务分组）；
///   - **AutoClaw 国内版 / 国际版**：通用任务接口的 `daily_signin` 任务
///     （`providers::autoclaw::checkin`）。两个地区**都支持** —— 任务接口在
///     两地是同一套路径、同一套任务 id，只是站点不同（已实测），因此两家
///     都列进来；地区由 `billing::checkin` 从账号的 provider 反查。
///   - **Qoder 中国版 / 国际版**：活动（campaign）领取链路；国际版叠加
///     UMID 风控身份，活动窗口未开时不落完成状态。
///   - **Trae**：SOLO 的 `checkin_credits` 领取。
///   - **MiniMax Code**：原生 Provider 每日签到；**LobsterAI**：活动奖励。
///   - **Loomy / KukuAI**：各自每日任务；预置 API 通过 `reward-custom` 聚合。
///
/// 这是「有签到或每日活跃任务」的清单，不是「有积分概念」的清单：CatPaw 有积分查询
/// 但没有签到，因此不在此列 —— 它的账号在批量签到里被算作 `skipped`。
/// 加一家之前先确认它的签到链路真的存在（一个点了必然报错的复选框比没有更糟）。
pub const CHECKIN_PROVIDERS: [&str; 13] = [
    "workbuddy",
    "workbuddy-intl",
    "raccoon",
    "autoclaw",
    "autoclaw-intl",
    "qoder",
    "qoder-intl",
    "trae",
    "minimax-code",
    "lobsterai",
    // Custom providers opt into the reward profile adapters.  The virtual id
    // keeps the persisted scheduler setting independent from generated
    // `custom-*` provider ids.
    "reward-custom",
    "loomy",
    "kuku",
];

/// 缺省的签到提供商集合（全选）
pub fn default_providers() -> Vec<String> {
    CHECKIN_PROVIDERS.iter().map(|id| id.to_string()).collect()
}

/// 提供商的展示名（从注册表查，查不到就原样回显 id）。
///
/// 这是**签到语境**的展示名；自定义奖励和本地原生提供商保持专属文案。
pub fn provider_label(id: &str) -> &str {
    match id {
        "reward-custom" => "预置 API 奖励（AStudio / DuMate）",
        "minimax-code" => "MiniMax Code（每日签到）",
        "lobsterai" => "LobsterAI（活动奖励）",
        other => crate::server::core::providers::PROVIDERS
            .iter()
            .find(|meta| meta.id == other)
            .map(|meta| meta.label)
            .unwrap_or(other),
    }

}

/// 归一化配置里的提供商清单：只认 CHECKIN_PROVIDERS 里的 id（去重、保持顺序），
/// 缺失 / 空数组 / 全是非法值都回落到「全选」—— 旧配置文件里没有这个字段，
/// 读出来必须是合法的默认行为。
///
/// 存量兼容：Qoder 拆家（2026-10）前的老清单只有 9 项、没有 `qoder-intl`，
/// 而显式清单不会被「缺啥补啥」—— 不补的话，拆家前勾了 Qoder 的用户拆家后
/// 自动签到会静默漏掉国际版账号。这里按账号迁移的同一口径补齐：勾了
/// `qoder` 的，紧跟其后补上 `qoder-intl`（要签中国版就要签国际版，拆家只是
/// 同一套能力的地区分身）；两者都没勾的（明确不签 Qoder）不动。
pub fn normalize_providers(value: Option<&Value>) -> Vec<String> {
    let Some(list) = value.and_then(Value::as_array) else {
        return default_providers();
    };
    let picked: Vec<String> = CHECKIN_PROVIDERS
        .iter()
        .filter(|id| list.iter().any(|item| item.as_str() == Some(*id)))
        .map(|id| id.to_string())
        .collect();
    let mut picked = if picked.is_empty() {
        default_providers()
    } else {
        picked
    };
    if let Some(pos) = picked.iter().position(|id| id == "qoder") {
        if !picked.contains(&"qoder-intl".to_string()) {
            picked.insert(pos + 1, "qoder-intl".to_string());
        }
    }
    picked
}

/// config.json 里承载定时签到状态的键
const CONFIG_KEY: &str = "autoCheckin";

/// 配置类错误（对应 Node 版 AutoCheckinConfigError）。
///
/// **注意 statusCode 是 400 而不是 409**：Node 版 `AutoCheckinConfigError`
/// 构造里写死 `statusCode = 400`，于是 server.mjs 的 `throw new
/// AutoCheckinConfigError('签到正在执行中，请稍候')`（run 路由）经 errorPayload
/// 得到的是 **400 + OpenAI 风格 body**，而不是 409。实测（2026-09-17）用非法
/// time 打 POST /api/auto-checkin 拿到的正是
/// `400 {"error":{"message":"时间超出范围（00:00 - 23:59）","type":"invalid_request_error"}}`。
/// 契约以 Node 版为准，这里保持一致（`CHECKIN_BUSY_STATUS` 单独抽出，
/// 万一确认要用 409，改一处即可）。
#[derive(Clone, Debug)]
pub struct AutoCheckinConfigError {
    pub message: String,
    pub status_code: i32,
}

/// run 路由「正在执行中」的状态码：与 Node 的 AutoCheckinConfigError 一致为 400
pub const CHECKIN_BUSY_STATUS: i32 = 400;

impl AutoCheckinConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status_code: 400,
        }
    }
}

impl std::fmt::Display for AutoCheckinConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

// ─── 时间工具（对照 Node 的同名函数）────────────────────────

/// JS `String(value)` 的近似：只用于 normalizeTime 的输入转换。
/// `value ?? ''` 在 Node 里把 null/undefined 变成空串，其余走 String()：
/// 数字/布尔取字面量，对象/数组在 JS 里是 "[object Object]" / 逗号拼接 ——
/// 这些形态只可能来自手改的请求体或 config.json，最终都会被正则拒掉。
fn js_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Object(_)) => "[object Object]".to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// 校验并归一化 HH:MM，返回补零后的文本（对应 Node 版 normalizeTime）。
///
/// 正则 `^(\d{1,2}):(\d{1,2})$` 手写等价实现：冒号两侧各 1-2 位 ASCII 数字，
/// 且**整串**必须如此（`\d` 在 JS 里不含全角数字、不允许前后空白 ——
/// 前后空白在 Node 里由 `String(value).trim()` 提前去掉）。
pub fn normalize_time(value: Option<&Value>) -> Result<String, AutoCheckinConfigError> {
    let text = js_text(value);
    let text = text.trim();
    let parsed = text
        .split_once(':')
        .filter(|(head, tail)| {
            let digits = |part: &str| {
                !part.is_empty()
                    && part.len() <= 2
                    && part.bytes().all(|byte| byte.is_ascii_digit())
            };
            digits(head) && digits(tail) && !tail.contains(':')
        })
        .and_then(|(head, tail)| Some((head.parse::<u32>().ok()?, tail.parse::<u32>().ok()?)));
    let Some((hour, minute)) = parsed else {
        return Err(AutoCheckinConfigError::new(
            "时间格式应为 HH:MM（例如 00:01）",
        ));
    };
    if hour > 23 || minute > 59 {
        return Err(AutoCheckinConfigError::new("时间超出范围（00:00 - 23:59）"));
    }
    Ok(format!("{hour:02}:{minute:02}"))
}

/// 北京日期键 YYYY-MM-DD：用于「今天是否已触发」的判定。
///
/// 为什么不能用 UTC 算：UTC+8 的凌晨 00:01 在 UTC 下还是前一天，会让
/// 「今天签过没有」的判重错一整天（移植来源 Node 版用本地时区，作者在
/// 国内恰好等于 UTC+8 —— 这里显式钉住北京时间，见 `core::beijing`）。
pub fn date_key(now: DateTime<FixedOffset>) -> String {
    beijing::date_key(now)
}

/// 北京时间的今天（`YYYY-MM-DD`）——「今天是否已触发」判定的统一入口。
pub fn today_key() -> String {
    beijing::today_key()
}

/// 今天的触发时刻（北京时间）；已过则返回 Some，未到返回 None。
/// 对应 Node 版 dueNow：非法的 time 文本在这里同样表现为 None（Node 是抛异常，
/// 但调用方一律 catch 后 return，结果一致）。
fn due_now(time_text: &str, now: DateTime<FixedOffset>) -> Option<DateTime<FixedOffset>> {
    let target = today_at(time_text, now)?;
    if now >= target {
        Some(target)
    } else {
        None
    }
}

/// 距离下一次触发的毫秒数（用于界面显示「下次执行」）。
/// 对应 Node 版 msUntilNext：目标时刻已过则顺延到明天同一时刻。
pub fn ms_until_next(time_text: &str, now: DateTime<FixedOffset>) -> i64 {
    let Some(mut target) = today_at(time_text, now) else {
        return 0;
    };
    if target <= now {
        target += chrono::Duration::days(1);
    }
    (target - now).num_milliseconds()
}

/// 今天的 HH:MM 时刻（北京时间）；time 文本非法时 None。
///
/// Node 用 `target.setHours(hour, minute, 0, 0)`（就地改到当天）；
/// chrono 没有 set_* 的就地 API，用 `with_ymd_and_hms` 重建。
/// 固定偏移没有夏令时问题（构造恒为 `Single`），因此不需要 Node 那种
/// 「DST 当天该时刻不存在」的兜底 —— `single()` 取不到就当 None，
/// 也就是这台机器上的时钟环境异常到连北京时间的固定偏移都构造不出来。
fn today_at(time_text: &str, now: DateTime<FixedOffset>) -> Option<DateTime<FixedOffset>> {
    let normalized = normalize_time(Some(&Value::String(time_text.to_string()))).ok()?;
    let (hour, minute) = normalized.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    let naive = now.date_naive().and_hms_opt(hour, minute, 0)?;
    beijing::offset()
        .from_local_datetime(&naive)
        .single()
}

// ─── 状态读写 ───────────────────────────────────────────────

/// 从 config.json 读出的调度状态（对应 Node 版 readState 的返回）
struct CheckinState {
    enabled: bool,
    time: String,
    /// 要签到的提供商（见 CHECKIN_PROVIDERS；缺省全选）
    providers: Vec<String>,
    last_fired_date: Option<String>,
    last_result: Option<Value>,
}

/// `autoCheckin` 字段的原始对象（非对象/缺失都给空对象）
fn raw_object() -> Map<String, Value> {
    match config::current().raw().get(CONFIG_KEY) {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

/// 读状态。time 被手工改坏时回落到 DEFAULT_TIME，不让调度起不来（Node 同）。
fn read_state() -> CheckinState {
    let raw = raw_object();
    let mut providers = normalize_providers(raw.get("providers"));
    // H 的 workbuddy 选项包含国际版；新版显式保存的选择按独立 provider 解释。
    if raw
        .get("providersVersion")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        < 2
        && providers.iter().any(|id| id == "workbuddy")
        && !providers.iter().any(|id| id == "workbuddy-intl")
    {
        providers.push("workbuddy-intl".to_string());
        providers = normalize_providers(Some(&json!(providers)));
    }
    let enabled = raw.get("enabled").and_then(Value::as_bool) == Some(true);
    let time = raw
        .get("time")
        .filter(|value| !value.is_null())
        .and_then(|value| normalize_time(Some(value)).ok())
        .unwrap_or_else(|| DEFAULT_TIME.to_string());
    let last_fired_date = raw
        .get("lastFiredDate")
        .and_then(Value::as_str)
        .map(str::to_string);
    let last_result = raw
        .get("lastResult")
        .filter(|value| value.is_object())
        .cloned();
    CheckinState {
        enabled,
        time,
        providers,
        last_fired_date,
        last_result,
    }
}

/// 合并写回 `autoCheckin`（对应 Node 版 writeState 的 `{...current, ...patch}`）。
/// 走 config.rs 的 `update_raw_field`，config.json 里的其它字段原样保留。
fn write_state(patch: Value) {
    let mut merged = raw_object();
    if let Value::Object(patch) = patch {
        for (key, value) in patch {
            merged.insert(key, value);
        }
    }
    config::update_raw_field(CONFIG_KEY, Value::Object(merged));
}

// ─── 服务句柄 ───────────────────────────────────────────────

/// 句柄内部状态：下载/签到这类长任务只在这里留「是否在跑」与调度任务句柄。
struct Inner {
    /// 是否已有一次签到在执行（对应 Node 的闭包变量 running）
    running: bool,
    /// 调度循环任务句柄（对应 Node 的 timer）
    task: Option<tokio::task::JoinHandle<()>>,
    /// 停机标志：stop() 置位后循环退出（幂等；再次 start 会重置）
    stopped: Arc<AtomicBool>,
}

/// 定时签到服务句柄：内部一把锁 + Clone，与 update / models 同构。
///
/// 锁只用来读写 running 与任务句柄这类微秒级操作；签到执行全程在锁外
/// （硬约束：持锁不做网络请求、不 await）。`running` 用 CAS 抢锁的方式表达
/// 「同时只跑一次」，与 Node 单线程下 `if (running) return` 的语义一致。
#[derive(Clone)]
pub struct AutoCheckin {
    store: AccountStore,
    billing: BillingService,
    inner: Arc<Mutex<Inner>>,
}

/// 抢到「执行中」标记的守卫：无论正常返回还是中途被取消（任务 abort），
/// Drop 时都会把 running 复位 —— 对应 Node 的 `finally { running = false }`，
/// 同时避免 abort 把 running 永久卡在 true。
struct RunningGuard {
    inner: Arc<Mutex<Inner>>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        match self.inner.lock() {
            Ok(mut guard) => guard.running = false,
            Err(poisoned) => poisoned.into_inner().running = false,
        }
    }
}

impl AutoCheckin {
    pub fn new(store: AccountStore, billing: BillingService) -> Self {
        Self {
            store,
            billing,
            inner: Arc::new(Mutex::new(Inner {
                running: false,
                task: None,
                stopped: Arc::new(AtomicBool::new(false)),
            })),
        }
    }

    /// 取锁；锁中毒（持锁 panic）不致命，接管内部数据继续用
    /// （与账号存储、脱敏器同一策略）
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 是否正在执行签到（getState().running）
    fn is_running(&self) -> bool {
        self.lock().running
    }

    // ─── 对外状态 ───────────────────────────────────────────

    /// 对外暴露的状态（对应 Node 版 getState，含界面要显示的「下次执行」）
    pub fn state(&self) -> Value {
        let state = read_state();
        let now = beijing::now();
        let next_run_at = if state.enabled {
            let next = logging::now_ms() + ms_until_next(&state.time, now);
            Value::from(next)
        } else {
            Value::Null
        };
        json!({
            "enabled": state.enabled,
            "time": state.time,
            // 界面按这份清单勾选复选框；顺序即 CHECKIN_PROVIDERS 的注册顺序
            "providers": state.providers,
            "providerOptions": CHECKIN_PROVIDERS.iter().map(|id| {
                json!({ "id": id, "label": provider_label(id) })
            }).collect::<Vec<_>>(),
            "lastFiredDate": state.last_fired_date,
            "lastFiredToday": state.last_fired_date.as_deref() == Some(date_key(now).as_str()),
            "nextRunAt": next_run_at,
            "lastResult": state.last_result,
            "running": self.is_running(),
        })
    }

    // ─── 执行 ───────────────────────────────────────────────

    /// 执行一次签到并记录结果；失败只记日志，不影响下一次调度。
    ///
    /// 返回 None 的两种情形（Node 同）：① 已有一次在跑（本次跳过）；
    /// ② 执行抛错。路由层据此把 ① 翻成「签到正在执行中，请稍候」。
    pub async fn fire(&self, reason: &str) -> Option<Value> {
        // 先抢「执行中」标记；抢不到说明上一次还在跑
        let guard = {
            let mut inner = self.lock();
            if inner.running {
                drop(inner);
                logging::verbose("[Checkin]", "上一次签到仍在执行，本次跳过");
                return None;
            }
            inner.running = true;
            RunningGuard {
                inner: self.inner.clone(),
            }
        };

        let today = today_key();
        // 先落日期再执行：即便签到中途进程被杀，也不会在重启后反复补签
        write_state(json!({ "lastFiredDate": today }));
        logging::log("[Checkin]", &format!("⏰ 定时签到开始（{reason}）"));

        let outcome = match checkin::run_checkin(
            &self.store,
            &self.billing,
            read_state().providers.as_slice(),
            None,
            reason,
        )
        .await
        {
            Ok(result) => Some(self.record_success(&result, &today, reason)),
            Err(error) => {
                // 失败也写 lastResult（failed 里放错误文案），并结束本日调度窗口。
                // 失败账号仍可通过面板手动重试，避免单个账号让整批任务每 30 秒重跑。
                write_state(json!({
                    "lastFiredDate": Value::String(today.to_string()),
                    "lastResult": {
                        "at": logging::now_ms(),
                        "date": today,
                        "reason": reason,
                        "succeeded": 0,
                        "completedAccountIds": [],
                        "active": 0,
                        "total": 0,
                        "skipped": 0,
                        "failed": [error.message.clone()],
                        "failedCount": 1,
                    }
                }));
                logging::log("[Checkin]", &format!("❌ 定时签到失败: {}", error.message));
                None
            }
        };
        drop(guard);
        outcome
    }

    /// 本轮是否有「Qoder 国际版账号没领到且不算已签」的行 —— 有则今天保持
    /// 未落账状态，等窗口开了重试（见 tick 的说明）。
    fn qoder_intl_pending(result: &Value) -> bool {
        result
            .get("results")
            .and_then(Value::as_array)
            .map(|items| {
                items.iter().any(|item| {
                    item.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.starts_with("qoder-global-"))
                        && crate::server::core::billing::checkin::claim_pending(
                            &item.get("claim").cloned().unwrap_or(Value::Null),
                        )
                })
            })
            .unwrap_or(false)
    }

    /// 成功分支：汇总 + 记录 lastResult + 日志（对应 Node fire 里的 try 主体）
    fn record_success(&self, result: &Value, today: &str, reason: &str) -> Value {
        let number = |key: &str| result.get(key).and_then(Value::as_u64).unwrap_or(0);
        let succeeded = number("succeeded");
        let active = number("active");
        let total = number("total");
        let skipped = number("skipped");
        let available = result
            .get("available")
            .and_then(Value::as_u64)
            .unwrap_or(total.saturating_add(skipped));
        // 失败明细：`名字（错误）`，名字缺失时退到 id，再退到「未知账号」
        let failures = failed_account_labels(result);
        let summary = json!({
            "at": logging::now_ms(),
            "date": today,
            "reason": reason,
            "succeeded": succeeded,
            "completedAccountIds": completed_account_ids(result),
            "active": active,
            "total": total,
            "eligible": total,
            "available": available,
            "skipped": skipped,
            // 只留前 5 条：面板展示用，避免 config.json 被长列表撑大（Node 同）
            "failed": failures.iter().take(5).cloned().collect::<Vec<_>>(),
            "failedCount": failures.len(),
        });
        write_state(daily_state_patch(today, summary.clone()));
        logging::log(
            "[Checkin]",
            &format!(
                "定时签到完成：成功 {succeeded}/{available} 个（实际执行 {total}，跳过 {skipped}）{}{}",
                if failures.is_empty() {
                    String::new()
                } else {
                    format!("，失败 {} 个", failures.len())
                },
                if active > 0 {
                    format!("，活跃保活 {active} 个")
                } else {
                    String::new()
                },
            ),
        );
        // qoderIntlPending 不进 lastResult（那是给界面看的快照），单独放在
        // fire 的返回值上供 tick 决定今天要不要保持未落账
        let mut summary = summary;
        if let Some(object) = summary.as_object_mut() {
            object.insert(
                "qoderIntlPending".to_string(),
                Value::Bool(Self::qoder_intl_pending(result)),
            );
        }
        summary
    }

    // ─── 调度 ───────────────────────────────────────────────

    /// 轮询回调：到点且今天没签过就执行（对应 Node 版 tick）。
    ///
    /// ── Qoder 国际版的窗口补领（2026-10，issue #140）────────────
    /// 国际版「每日 100 Credits」的官方窗口**每天 10:00（UTC+8）开启**，而
    /// 缺省触发时刻是 00:01 —— 那时窗口还没开，活动列表里没有积分活动，
    /// 领取轮次对它只能得到「无活动」。若就此落 `lastFiredDate` 整天不再
    /// 重试，国际版账号等于永远领不到。所以主轮次**照常在配置时刻跑**，
    /// 但当轮次里存在「Qoder 国际版无活动未落账」的行时，今天暂不落
    /// `lastFiredDate`：之后每个 tick 会重新进来，直到 10:00 窗口开启领到
    /// （或活动行出现 CLAIMED）才落账 —— 与 10router「no-activity 刻意不
    /// 记忆、下个 tick 再看」同一语义。`fire` 返回该信息，None（在跑/出错）
    /// 与普通完成照旧落账。
    pub async fn tick(&self) {
        let state = read_state();
        if !state.enabled {
            return;
        }
        let now = beijing::now();
        // 未到点（含 time 非法）直接返回
        if due_now(&state.time, now).is_none() {
            return;
        }
        if state.last_fired_date.as_deref() == Some(date_key(now).as_str()) {
            return;
        }
        match self.fire("到点触发").await {
            Some(result) => {
                // 领取轮次完成，但里面有「Qoder 国际版无活动」的行 → 不落账，
                // 今天窗口开了之后由后续 tick 重试
                if result.get("qoderIntlPending").and_then(Value::as_bool) == Some(true) {
                    write_state(json!({ "lastFiredDate": Value::Null }));
                    logging::log(
                        "[Checkin]",
                        "⏳ Qoder 国际版的活动窗口未开（官方每日 10:00 UTC+8），今天会自动重试到领到为止",
                    );
                }
            }
            // None = 已有轮次在跑或执行出错：照旧落账（fire 内部已写），行为不变
            None => {}
        }
    }

    /// 起调度循环（对应 Node 版 schedule 的 `if (timer) return`）：
    /// 已有存活任务就不重复起，避免 configure 每次保存都叠一个循环。
    fn schedule(&self) {
        {
            let mut inner = self.lock();
            if let Some(task) = &inner.task {
                if !task.is_finished() {
                    return;
                }
            }
            inner.stopped = Arc::new(AtomicBool::new(false));
            let stopped = inner.stopped.clone();
            let service = self.clone();
            // 用 crate::spawn_task：本函数可能从 setup 钩子（主线程）或 axum handler
            // 调用，`crate::spawn_task` 兜底进全局运行时，
            // 不会像裸 `tokio::spawn` 那样在非运行时上下文 panic（release 是 panic=abort）
            inner.task = Some(crate::spawn_task(async move {
                loop {
                    // 先睡再 tick：与 setInterval 的「间隔后首次触发」一致，
                    // 也避开 start() 里那次补签刚发起的窗口
                    tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
                    if stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    service.tick().await;
                }
            }));
        }
    }

    /// 启动调度（对应 Node 版 start）。
    ///
    /// 开启状态下检查一次；只有配置的触发时刻已到且今天尚未完成时才补签。
    pub fn start(&self) {
        let state = read_state();
        if !state.enabled {
            return;
        }
        self.schedule();
        let now = beijing::now();
        let today = today_key();
        let due = due_now(&state.time, now).is_some();
        if state.last_fired_date.as_deref() != Some(today.as_str()) && due {
            let service = self.clone();
            crate::spawn_task(async move {
                service.fire("启动补签").await;
            });
        } else if state.last_fired_date.as_deref() == Some(today.as_str()) {
            logging::verbose(
                "[Checkin]",
                &format!("定时签到已于今日执行，下次 {}", state.time),
            );
        } else {
            logging::verbose(
                "[Checkin]",
                &format!(
                    "定时签到尚未到点（当前 {}，计划 {}），今天到点后执行",
                    now.format("%H:%M:%S"),
                    state.time
                ),
            );
        }
    }

    /// 停调度（进程退出时用）。幂等：重复调用不会报错。
    ///
    /// 与 Node 的 `clearInterval` 语义对齐：只结束**循环**，不打断正在跑的
    /// 那一次签到（abort 只会让任务在下一个 await 点结束，config.json 的写入是
    /// 同步的，已落盘的 lastFiredDate 不会被回滚）。
    pub fn stop(&self) {
        let task = {
            let mut inner = self.lock();
            if inner.task.is_none() {
                return;
            }
            inner.stopped.store(true, Ordering::SeqCst);
            inner.task.take()
        };
        if let Some(task) = task {
            task.abort();
        }
    }

    // ─── 设置 ───────────────────────────────────────────────

    /// 保存设置（对应 Node 版 configure）。关闭时循环归还调度循环继续跑，
    /// 但 tick 读到 enabled=false 会直接返回 —— 与 Node 一致（Node 的
    /// configure 也只 start 不 stop，关闭靠 tick 早退生效）。
    pub fn configure(&self, payload: &Value) -> Result<Value, AutoCheckinConfigError> {
        let mut patch = Map::new();
        if let Some(value) = payload.get("enabled") {
            // Node: `enabled !== undefined` → `patch.enabled = enabled === true`，
            // 也就是说 null / 字符串 / 0 都会把开关置为 false（不是「忽略」）
            patch.insert(
                "enabled".to_string(),
                Value::Bool(value == &Value::Bool(true)),
            );
        }
        if let Some(value) = payload.get("time") {
            patch.insert(
                "time".to_string(),
                Value::String(normalize_time(Some(value))?),
            );
        }
        if let Some(value) = payload.get("providers") {
            // 勾选清单：只认注册过的提供商 id（去重、按注册顺序落盘）；
            // 一个都不勾没有意义 —— 定时到点也无账号可签，直接拒绝并说明。
            let Some(list) = value.as_array() else {
                return Err(AutoCheckinConfigError::new("providers 必须是字符串数组"));
            };
            let picked: Vec<Value> = CHECKIN_PROVIDERS
                .iter()
                .filter(|id| list.iter().any(|item| item.as_str() == Some(*id)))
                .map(|id| Value::String(id.to_string()))
                .collect();
            if picked.is_empty() {
                let names = CHECKIN_PROVIDERS
                    .iter()
                    .map(|id| provider_label(id))
                    .collect::<Vec<_>>()
                    .join("、");
                return Err(AutoCheckinConfigError::new(format!(
                    "至少勾选一家签到提供商（{names}）"
                )));
            }
            patch.insert("providers".to_string(), Value::Array(picked));
            patch.insert("providersVersion".to_string(), json!(2));
        }
        if patch.is_empty() {
            return Err(AutoCheckinConfigError::new("没有需要更新的字段"));
        }

        let before = read_state();
        write_state(Value::Object(patch));
        let mut after = read_state();
        let today = today_key();
        // 修改到更晚的时间时，旧的同日执行标记不能挡住新的触发点。
        // 只有在新时间尚未到达时清理，避免把已经完成的当日任务无条件重跑。
        if before.time != after.time
            && before.last_fired_date.as_deref() == Some(today.as_str())
            && due_now(&after.time, beijing::now()).is_none()
        {
            write_state(json!({ "lastFiredDate": Value::Null }));
            after = read_state();
        }
        if before.enabled != after.enabled {
            logging::log(
                "[Checkin]",
                &format!(
                    "定时签到已{}（{}）",
                    if after.enabled { "开启" } else { "关闭" },
                    after.time
                ),
            );
        } else if before.time != after.time {
            logging::log("[Checkin]", &format!("定时签到时间已改为 {}", after.time));
        }
        if before.providers != after.providers {
            let names = |list: &[String]| {
                list.iter()
                    .map(|id| provider_label(id))
                    .collect::<Vec<_>>()
                    .join("、")
            };
            logging::log(
                "[Checkin]",
                &format!("签到范围已改为 {}", names(&after.providers)),
            );
        }
        if after.enabled {
            self.start();
        }
        Ok(self.state())
    }

    /// 当前配置的签到提供商（账号页批量签到与定时签到共用同一份口径）。
    pub fn configured_providers(&self) -> Vec<String> {
        read_state().providers
    }

    /// 立即执行一次（界面上的「立即签到」按钮走这里，与定时触发同一条路径）。
    /// 正在执行中返回 None —— 路由层翻成「签到正在执行中，请稍候」。
    pub async fn run_now(&self) -> Option<Value> {
        self.fire("手动触发").await
    }
}

/// 只从本轮实际执行且无错误的领取结果提取账号，不从选中范围或成功总数推断。
pub fn completed_account_ids(result: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for item in result
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if item.get("error").is_some_and(|error| !error.is_null())
            || item.get("skipped").and_then(Value::as_bool) == Some(true)
            || item.get("status").and_then(Value::as_str) == Some("skipped")
        {
            continue;
        }
        let Some(claim) = item.get("claim") else {
            continue;
        };
        if claim.get("error").is_some_and(|error| !error.is_null())
            || claim.get("skipped").and_then(Value::as_bool) == Some(true)
            || claim.get("status").and_then(Value::as_str) == Some("skipped")
            || claim.get("claimUnconfirmed").and_then(Value::as_bool) == Some(true)
            || claim_auth_failed(item, claim)
        {
            continue;
        }
        let completed = claim.get("success").and_then(Value::as_bool) == Some(true)
            || claim.get("alreadyCompleted").and_then(Value::as_bool) == Some(true)
            || claim.get("alreadyClaimed").and_then(Value::as_bool) == Some(true)
            || matches!(
                claim.get("status").and_then(Value::as_str),
                Some("claimed" | "already_claimed")
            );
        if completed {
            if let Some(id) = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
            {
                if !ids.iter().any(|existing| existing == id) {
                    ids.push(id.to_string());
                }
            }
        }
    }
    ids
}

/// 调度摘要与签到历史共用失败口径；保活只能中和“奖励尚未确认”，不能隐藏领取错误。
pub fn failed_account_labels(result: &Value) -> Vec<String> {
    result
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let error = item
                .get("error")
                .and_then(Value::as_str)
                .filter(|message| !is_benign_completion_message(message));
            let claim_message = item
                .get("claim")
                .filter(|value| value.get("success").and_then(Value::as_bool) == Some(false))
                .and_then(|value| {
                    let message = value
                        .get("msg")
                        .and_then(Value::as_str)
                        .unwrap_or("签到领取失败");
                    let auth_failed = claim_auth_failed(item, value);
                    let rejected = value
                        .get("code")
                        .and_then(Value::as_i64)
                        .is_some_and(|code| code != 0);
                    let neutral_keepalive = activity_keepalive_succeeded(item)
                        && !rejected
                        && matches!(
                            message,
                            "日活奖励尚未确认"
                                | "有效对话完成，日活奖励尚未确认"
                                | "网页会话完成，日活奖励尚未确认"
                                | "仅执行保活，未领取奖励"
                        );
                    let benign = value.get("alreadyCompleted").and_then(Value::as_bool)
                        == Some(true)
                        || is_benign_completion_message(message)
                        || (!rejected && message == "活动未开启")
                        || neutral_keepalive;
                    (auth_failed || !benign).then_some(message)
                });
            let error = error.or(claim_message)?;
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .or_else(|| item.get("id").and_then(Value::as_str))
                .unwrap_or("未知账号");
            Some(format!("{name}（{error}）"))
        })
        .collect()
}

/// 计费签到接口把「今天已经完成」作为 HTTP 错误返回；这类结果不会阻塞
/// 下一次活动窗口，也不应让定时任务进入高频重试。
fn is_benign_completion_message(message: &str) -> bool {
    [
        "当前没有可领取的签到活动",
        "当前没有可领取的奖励",
        "无每日签到活动",
        "今天已签到",
        "今日已签到",
        "今日已领取",
        "已签到",
        "已领取",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

fn claim_auth_failed(item: &Value, claim: &Value) -> bool {
    match claim.get("code").and_then(Value::as_i64) {
        Some(401 | 403) => true,
        Some(1001) => item.get("provider").and_then(Value::as_str) == Some("trae"),
        _ => false,
    }
}

fn activity_keepalive_succeeded(item: &Value) -> bool {
    item.get("activity")
        .and_then(|value| value.get("pokeSucceeded"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// 一次批处理即消耗当天的自动签到窗口；失败详情保留在 `lastResult`，
/// 手动入口仍可单独重试失败账号，避免整批任务在 30 秒轮询中重复运行。
fn daily_state_patch(today: &str, summary: Value) -> Value {
    json!({
        "lastFiredDate": Value::String(today.to_string()),
        "lastResult": summary,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        activity_keepalive_succeeded, daily_state_patch, default_providers,
        is_benign_completion_message, provider_label,
    };

    #[test]
    fn completed_checkin_errors_do_not_trigger_retries() {
        assert!(is_benign_completion_message(
            "计费接口返回 HTTP 400: 今天已签到，请明天再来"
        ));
        assert!(is_benign_completion_message("今日已领取"));
        assert!(is_benign_completion_message("当前没有可领取的签到活动"));
        assert!(!is_benign_completion_message(
            "计费接口返回 HTTP 401: 凭证已过期"
        ));
    }

    #[test]
    fn successful_activity_keepalive_does_not_trigger_retries() {
        assert!(activity_keepalive_succeeded(&json!({
            "activity": { "pokeSucceeded": true },
            "claim": { "success": false, "msg": "有效对话完成，日活奖励尚未确认" }
        })));
        assert!(!activity_keepalive_succeeded(&json!({
            "activity": { "pokeSucceeded": false }
        })));
    }

    #[test]
    fn partial_failure_closes_daily_window_and_keeps_failure_details() {
        let patch = daily_state_patch(
            "2026-10-05",
            json!({"failed": ["Trae（设备限流）"], "failedCount": 1}),
        );
        assert_eq!(patch["lastFiredDate"], "2026-10-05");
        assert_eq!(patch["lastResult"]["failedCount"], 1);
    }

    #[test]
    fn qoder_is_default_and_labeled_for_both_regions() {
        assert!(default_providers().iter().any(|id| id == "qoder"));
        assert_eq!(provider_label("qoder"), "Qoder 中国版");
        assert!(default_providers().iter().any(|id| id == "minimax-code"));
        assert_eq!(provider_label("minimax-code"), "MiniMax Code（每日签到）");
        assert!(default_providers().iter().any(|id| id == "lobsterai"));
        assert_eq!(provider_label("lobsterai"), "LobsterAI（活动奖励）");
        assert!(default_providers().iter().any(|id| id == "reward-custom"));
        assert_eq!(
            provider_label("reward-custom"),
            "预置 API 奖励（AStudio / DuMate）"
        );
    }
}

// ─── 进程级句柄（供停机路径调用 stop）─────────────────────────

/// 进程级定时签到句柄。
///
/// 为什么不用 ServerState 就够了：停机入口是 `backend::shutdown(&AppState)`，
/// 那里只有 Tauri 的应用状态，拿不到 ServerState；而 Node 版退出时明确会
/// `autoCheckin.stop()`（server.mjs 1047-1049 的 closeAll）。用进程级句柄
/// （与 config / logging 同一个模式）让停机路径能直接拿到它。
/// ServerState 里那份与全局这份是**同一实例**（bootstrap 时装入）。
static GLOBAL: OnceLock<AutoCheckin> = OnceLock::new();

/// 初始化进程级句柄（启动时调用一次，幂等）
pub fn init_global(service: AutoCheckin) -> AutoCheckin {
    let _ = GLOBAL.set(service.clone());
    service
}

/// 停掉进程级调度；未初始化时什么都不做（路由以外的调用点用）
pub fn stop_global() {
    if let Some(service) = GLOBAL.get() {
        service.stop();
    }
}
