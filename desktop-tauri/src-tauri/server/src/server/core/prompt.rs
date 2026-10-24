//! 网关自有系统提示词：`passthrough` / `custom` / `append` / `prepend` 四模式。
//!
//! 照搬 workbuddy2api 的 `internal/prompt/prompt.go`（模式语义、插入位置、守卫
//! 逐条对齐；`prepend` 是本地新增的第四种，参考项目没有），它是脱敏之外的第二层
//! 防护 —— 两层**叠加、互不替代**：
//!
//! ```text
//!   ① 提示词层（本模块）：出站前用网关自有提示词**替换/追加/前置** system 消息，
//!      从源头消灭「system 来源」的指纹 —— 客户端注入的模板句根本没机会出站。
//!   ② 指纹脱敏层（core::sanitize）：改写/剥离请求体里残留的指纹，覆盖
//!      user / assistant / tool_calls / reasoning_content 这些**用户内容**，
//!      以及网关提示词自己万一命中的情况（两层的顺序见 `upstream::payload`）。
//! ```
//!
//! ── 为什么要有提示词层（而不是只靠脱敏）─────────────────────
//! 上游按**逐字精确匹配**拦截（非语义审核），所以「把模板句改一个字」能绕过
//! ——那是脱敏层在做的事。但模板句是**成片**的（Claude Code / Codex 的 system
//! 提示词有几十句），脱敏只能覆盖实测命中过的那几句；换一个客户端版本就可能
//! 冒出新的一句。`custom` 模式从根上解决：客户端 system 整段换成网关自己那份，
//! 上游看到的 system 永远是网关写的、可控的一段文本。
//!
//! ── 四模式的语义 ────────────────────────────────────────────
//!   - `passthrough`（**默认**）：不动 system，客户端原样出站 —— 与改造前
//!     的行为逐字相同。此时唯一生效的是脱敏层；
//!   - `custom`：删除 messages 里**所有** `system` / `developer` 消息，在头部
//!     插入一条网关自有提示词（客户端项目规范随之消失，适合「我要用网关的提示词」
//!     的用户）；
//!   - `append`：在**开头连续**的 `system` / `developer` 块之后插入一条网关
//!     自有提示词，既有消息逐字不动（客户端项目规范优先，网关提示词做补充）；
//!   - `prepend`：在 `messages[0]` 插入一条网关自有提示词，客户端消息（含它
//!     自己的 system 块）整体后移（网关规则优先，客户端规范跟在它后面）。
//!     `append` 与 `prepend` 只差插入位置；客户端本来就没有 system 时两者输出
//!     完全相同。两者都**不**改变「会不会被拦」—— 指纹是逐字匹配、与位置无关，
//!     它们解决的是「谁的规则优先」。
//!
//! ── 降级（`core::degrade`）只作用于 passthrough / append / prepend ─
//! `custom` 模式下 system 已经由网关接管，再撞内容拦截就不是 system 指纹的问题
//! 了（多半是用户内容本身触发审核），所以它**不进降级路径**（`degradable`）。
//! `append` / `prepend` 在降级期都退化为替换（`rewrite` + 中性提示词）：带着指纹
//! 原文重试是确定性再撞墙，替换是一次性最小抢救。
//!
//! ── 本模块只做纯文本变换 ─────────────────────────────────────
//! 模式与提示词文本从哪来（配置文件键 / 提示词文件 / 内置默认）由 `config` 决定，
//! 开关时机由转发层决定；本模块无状态、无锁、无 IO，只回答「给定 body 与提示词，
//! 出站的那份应该长什么样」。

use serde_json::{json, Value};

/// 降级提示词：内容拦截误报时用的**最小中性提示词**（对应参考项目的
/// `prompt.Degraded`）。
///
/// 刻意极简：降级期间要的是「尽量不含任何会被逐字匹配的模板句」，同时不改变
/// 用户指令的合法性语义 —— 它不是对抗审核的手段，只是绕开**误报**。
pub const DEGRADED_PROMPT: &str = "You are a helpful assistant. Answer in the user's language, \
follow the user's instructions, and keep replies direct and concise.";

/// 内置默认提示词：`custom` / `append` 模式**未指定文件**时用它。
///
/// 与参考项目一样「网关自己有一份能直接用的提示词」，省得用户为了试一下
/// `custom` 模式还得先准备文件。文本是**我们自己写的**通用编码助手提示词
/// （参考项目那份是它自己的 300 行提示词，与本项目的产品定位无关，故不照搬）：
/// 要点是「不包含任何会被上游逐字拦截的指纹句」，用户想换成自己的提示词就把
/// 路径填进 `promptFile`（指向任何 UTF-8 文本文件即可）。
pub const BUILT_IN_PROMPT: &str = "\
You are a helpful coding assistant working inside the user's local development environment.

- Answer in the user's language unless asked otherwise.
- Be direct and concise: lead with the answer, then the details that matter.
- Use the tools you are given to read and change files, and never claim to have run \
something you did not actually run.
- Prefer the smallest change that solves the problem, and match the style of the surrounding code.
- When something is uncertain, say so and check instead of guessing.
- Do not run destructive commands (deleting data, resetting repositories, force-pushing) \
without explicit confirmation from the user.";

/// 系统提示词的四种模式（配置文件里的字符串就是契约，见 `config::KEY_PROMPT_MODE`）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptMode {
    /// 透传客户端 system（默认；与改造前行为逐字相同）
    #[default]
    Passthrough,
    /// 用网关自有提示词替换客户端的 system / developer 消息
    Custom,
    /// 在开头连续的 system / developer 块之后追加一条网关自有提示词
    Append,
    /// 在 messages 最前面插入一条网关自有提示词，客户端 system 排在它之后
    Prepend,
}

impl PromptMode {
    /// 全部合法值（界面下拉与 400 文案共用一份，避免两处各写一遍）
    pub const ALL: [PromptMode; 4] = [
        PromptMode::Passthrough,
        PromptMode::Custom,
        PromptMode::Append,
        PromptMode::Prepend,
    ];

    /// 配置值与接口响应里的字符串形态
    pub fn as_str(self) -> &'static str {
        match self {
            PromptMode::Passthrough => "passthrough",
            PromptMode::Custom => "custom",
            PromptMode::Append => "append",
            PromptMode::Prepend => "prepend",
        }
    }

    /// 解析配置值：大小写不敏感 + 去首尾空白（对应 Go 的
    /// `strings.ToLower(strings.TrimSpace(...))`）。空串按**默认**处理，
    /// 与参考项目「缺省 passthrough」一致；非法值给 `None`（由调用方决定
    /// 报错还是回落 —— 手改文件回落、走接口 400）。
    pub fn parse(text: &str) -> Option<Self> {
        let normalized = text.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Some(PromptMode::Passthrough);
        }
        PromptMode::ALL
            .into_iter()
            .find(|mode| mode.as_str() == normalized)
    }

    /// 界面与日志用的中文标签
    pub fn label(self) -> &'static str {
        match self {
            PromptMode::Passthrough => "透传客户端 system",
            PromptMode::Custom => "替换为网关提示词",
            PromptMode::Append => "追加网关提示词",
            PromptMode::Prepend => "网关提示词在前（客户端 system 追加其后）",
        }
    }

    /// 该模式是否需要一份提示词文本（= 除了透传以外的所有模式）。
    ///
    /// 三处消费方都靠它决定「要不要读盘 / 要不要校验文件」：`config::resolve_choice`
    /// （不读盘的话文本为空，模式会静默退化成透传）、`api::prompt` 的两处保存校验
    /// （不校验的话坏路径能存进去）。写成 `!= Passthrough` 而不是穷举 `matches!`，
    /// 是为了让以后新增模式**默认被包含**，不会因为漏改一处而静默失效。
    pub fn needs_text(self) -> bool {
        self != PromptMode::Passthrough
    }

    /// 该模式是否会在降级时改用中性提示词。
    ///
    /// `custom` 为假：system 已由网关接管，内容拦截不再指向 system 指纹
    /// （见模块头「降级只作用于 passthrough / append / prepend」）。
    pub fn degradable(self) -> bool {
        self != PromptMode::Custom
    }
}

/// 提示词文本的来源（界面与日志要能回答「这次用的到底是哪一份」）。
///
/// 优先级也是这个顺序的**解释**：界面里编辑的那份 > 提示词文件 > 内置默认
/// （见 `config::resolve_choice`）。三档都有各自的用户故事：图省事直接改界面、
/// 已有自己维护的提示词文件、什么都不配用内置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptSource {
    /// 没有提示词文本（`passthrough` 模式）
    None,
    /// 内置默认（`custom` / `append` 且既没编辑正文、也没指定文件）
    BuiltIn,
    /// 用户指定的提示词文件
    File,
    /// 用户在设置页里**直接编辑**的正文（优先级最高）
    Inline,
}

impl PromptSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptSource::None => "none",
            PromptSource::BuiltIn => "builtin",
            PromptSource::File => "file",
            PromptSource::Inline => "inline",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PromptSource::None => "不透传（passthrough）",
            PromptSource::BuiltIn => "内置默认提示词",
            PromptSource::File => "提示词文件",
            PromptSource::Inline => "界面里编辑的提示词",
        }
    }
}

/// 网关自带提示词的**三段正文**：身份句 / 稳定段 / 动态段。
///
/// ── 为什么是三段而不是一整段 ─────────────────────────────────
/// 上游按**结构**校验（2026-09-28 实测：三段各自成块 200、三段并成一段
/// 405/3012，见 `providers::zcode::OFFICIAL_PROMPT_NOTE`），所以「编辑这段
/// 提示词」在界面上就是**分别编辑三段**：合成一整段会让用户以为在编辑同一件事，
/// 而那样编辑出来的结果上游根本不收。
///
/// 同一类型既表示**官方原文**（`zcode::plan` 从 `zcode_system.json` 造，
/// 含 `{cwd}` 这类占位符），也表示**用户改过的正文**（配置里存的那份）——
/// 两者形状一致，取值时逐段合并（[`Self::or`]），不必再造一套中间类型。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GatewayBlocks {
    /// 第一段：身份句（官方 `You are ZCode, an interactive coding agent`）
    pub identity: String,
    /// 第二段：稳定段（工具用法、项目规范那一大段）
    pub stable: String,
    /// 第三段：动态段（沟通方式 / 上下文管理 + Environment）
    pub dynamic: String,
}

impl GatewayBlocks {
    /// 三个段名：就是配置与本模块的字段名，集中一处，免得各调用点手写字符串拼错
    pub const FIELDS: [&'static str; 3] = ["identity", "stable", "dynamic"];

    /// 取一段（段名见 [`Self::FIELDS`]；认不出的段名给空串，与「这段没配」同义）
    pub fn get(&self, field: &str) -> &str {
        match field {
            "identity" => &self.identity,
            "stable" => &self.stable,
            "dynamic" => &self.dynamic,
            _ => "",
        }
    }

    /// 写一段（段名见 [`Self::FIELDS`]）
    pub fn set(&mut self, field: &str, value: String) {
        match field {
            "identity" => self.identity = value,
            "stable" => self.stable = value,
            "dynamic" => self.dynamic = value,
            _ => {}
        }
    }

    /// 三段全空（= 没有覆盖，配置里不该留这么一个空壳）
    pub fn is_empty(&self) -> bool {
        Self::FIELDS.iter().all(|field| self.get(field).trim().is_empty())
    }

    /// 逐段合并：自己这一段是空白就用 `fallback` 的那一段。
    ///
    /// 「用户只改了一段」是常态（比如只把身份句改成自己的名字），所以合并必须是
    /// 逐段的 —— 整份覆盖会让用户为了改一行而被迫把官方那几千字符也抄一遍。
    pub fn or(&self, fallback: &GatewayBlocks) -> GatewayBlocks {
        let pick = |field: &str| {
            let mine = self.get(field);
            if mine.trim().is_empty() {
                fallback.get(field).to_string()
            } else {
                mine.to_string()
            }
        };
        GatewayBlocks {
            identity: pick("identity"),
            stable: pick("stable"),
            dynamic: pick("dynamic"),
        }
    }
}

/// 一次请求的提示词决定（全局默认 + 各家覆盖的**借用**视图）。
///
/// 借用而不是克隆：本结构由 `upstream::forward` 从配置快照里取一次，随
/// `ProviderContext` 借给整条转发链 —— 提示词可能有几百行，逐请求克隆一份
/// 纯属浪费（`config::current()` 已经克隆了整份配置，那份克隆在本函数栈帧里
/// 活得比 ctx 久，所以这里借得到）。
///
/// ── 为什么按家分派 ──────────────────────────────────────────
/// 「用哪份提示词」是**按上游**不同的问题（见 `config::KEY_PROMPT_PROVIDERS`），
/// 而模式语义（透传 / 替换 / 追加）是同一条加工链（[`PromptChoice::apply`]）——
/// 所以这里只做「取哪一份」的分派，加工一个字节都不分叉。`provider_id` 在
/// `payload::send_body` 就有，那正是「某一家即将发送之前」的时刻。
#[derive(Clone, Copy, Debug)]
pub struct PromptPlan<'a> {
    /// 未单独配置的家用的那一份（对应全局的 `promptMode` / `promptFile`）
    pub default: PromptChoice<'a>,
    /// 按客户端请求模型精确匹配的全局正文覆盖。
    model_prompts: &'a std::collections::BTreeMap<String, String>,
    /// 逐家覆盖（借自配置快照；空表 = 所有家都用 default）
    providers: &'a std::collections::BTreeMap<String, crate::server::config::ProviderPrompt>,
}

impl<'a> PromptPlan<'a> {
    /// 组装一次请求的提示词决定（**唯一构造点**：`config::RuntimeConfig::prompt_plan`）。
    ///
    /// 模型映射与逐家覆盖都不对外暴露字段：调用方只通过统一选择方法按请求模型和
    /// 提供商取一份决定，避免各处自行实现优先级。
    ///
    /// 「网关自带提示词」的开关**不在这里**：它由 `config::gateway_prompt_enabled`
    /// 直接读（那是一条热路径上的轻量读取，见那里的说明）。两个入口读同一张表，
    /// 各自不重复实现「缺省 = 装」的默认值，就不会有一处漏改。
    pub fn new(
        default: PromptChoice<'a>,
        model_prompts: &'a std::collections::BTreeMap<String, String>,
        providers: &'a std::collections::BTreeMap<String, crate::server::config::ProviderPrompt>,
    ) -> Self {
        Self { default, model_prompts, providers }
    }

    /// 这一家实际要用的那一份（没单独配就走 `default`）。
    ///
    /// 返回**借用**视图而不是值：模型/提供商覆盖里带着长文本，克隆一次就等于
    /// 把「借用视图」这层设计白做了。
    pub fn for_provider(&self, provider_id: &str, requested_model: &str) -> PromptChoice<'_> {
        let base = match self.providers.get(provider_id) {
            Some(entry) => PromptChoice {
                mode: entry.mode,
                text: &entry.text,
                source: entry.source,
            },
            None => self.default,
        };
        match self.model_prompts.get(requested_model) {
            Some(text) if !text.trim().is_empty() => PromptChoice {
                text,
                source: PromptSource::Inline,
                ..base
            },
            _ => base,
        }
    }
}

/// 单一提供商实际生效的提示词决定（模式 + 文本的借用视图）。
///
/// 全局默认与逐家覆盖都归一成这个形状之后，加工链（[`Self::apply`]、降级判定）
/// 只有一份实现 ——「全局这么做、某一家那么做」这种分叉从类型上就写不出来。
#[derive(Clone, Copy, Debug)]
pub struct PromptChoice<'a> {
    pub mode: PromptMode,
    /// `custom` / `append` 要用的提示词文本（`passthrough` 下为空串）
    pub text: &'a str,
    /// 这段文本的来源（内置默认 / 提示词文件 / 无）—— 详细日志与设置页要能回答
    /// 「这次用的到底是哪一份」
    pub source: PromptSource,
}

impl<'a> PromptChoice<'a> {
    /// 降级期该用哪份文本：可降级的模式用中性提示词，否则用模式自己的文本
    pub fn text_for(&self, degraded: bool) -> &str {
        if degraded && self.mode.degradable() {
            DEGRADED_PROMPT
        } else {
            self.text
        }
    }

    /// 出站前把提示词落到 body 上：`Some(副本)` = 改过，`None` = **不动**
    /// （`passthrough` 且未降级 —— 默认路径，调用方据此零拷贝沿用客户端原始体）。
    ///
    /// `degraded` 是**本次请求**是否已进入降级（请求开始时状态机已生效，或
    /// 本次请求撞了内容拦截后由转发层置位，见 `upstream::provider_loop`）。
    pub fn apply(&self, body: &Value, degraded: bool) -> Option<Value> {
        let text = self.text_for(degraded);
        if text.is_empty() {
            return None;
        }
        match self.mode {
            // 透传且未降级：没有任何要动的地方（上面 text 为空已经返回了，
            // 这里再挡一次，语义上「passthrough 就是不动」）
            PromptMode::Passthrough => {
                if degraded {
                    rewrite(body, text)
                } else {
                    None
                }
            }
            // 降级期：append / prepend 也退化为替换（见模块头）
            PromptMode::Append if !degraded => append(body, text),
            PromptMode::Prepend if !degraded => prepend(body, text),
            PromptMode::Custom | PromptMode::Append | PromptMode::Prepend => rewrite(body, text),
        }
    }
}

/// 替换：删除 messages 里**所有** `system` / `developer` 消息，头部插入一条
/// 网关提示词（对应 Go 的 `prompt.Rewrite`）。
///
/// 无 `messages` 字段（或不是数组）→ 只补一条 `messages: [网关 system]`，
/// 其余字段原样保留 —— 与参考项目同口径：转发链的关键路径上「绝不失败」，
/// 形态怪异时也要产出一份能发出去的 body。
///
/// 与 Go 的一处差异：Go 在 JSON 解析失败时返回原 body；本模块拿到的已经是
/// `Value`，没有解析这一步。
pub fn rewrite(body: &Value, text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    let mut next = body.clone();
    let object = next.as_object_mut()?;
    let gateway = gateway_message(text);
    match object.get_mut("messages").and_then(Value::as_array_mut) {
        Some(messages) => {
            // 先放网关提示词，再逐个保留非 system/developer —— 插入位置与
            // 过滤在同一次遍历里完成（参考项目也是这么写的）
            let mut kept: Vec<Value> = Vec::with_capacity(messages.len() + 1);
            kept.push(gateway);
            kept.extend(
                messages
                    .iter()
                    .filter(|message| !is_system_role(message))
                    .cloned(),
            );
            *messages = kept;
        }
        None => {
            object.insert("messages".to_string(), Value::Array(vec![gateway]));
        }
    }
    Some(next)
}

/// 追加：在**开头连续**的 `system` / `developer` 块之后插入一条网关提示词，
/// 既有消息逐字不动（对应 Go 的 `prompt.Append`）。
///
/// 「开头连续块」的判定与 [`rewrite`] 的删除口径**逐条一致**：从 `messages[0]`
/// 起向后只要是 system/developer 就继续，遇到第一条非 system/developer
/// （含非对象消息、无 role 消息）即停。
pub fn append(body: &Value, text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    let mut next = body.clone();
    let object = next.as_object_mut()?;
    let gateway = gateway_message(text);
    match object.get_mut("messages").and_then(Value::as_array_mut) {
        Some(messages) => {
            let insert_at = messages
                .iter()
                .take_while(|message| is_system_role(message))
                .count();
            messages.insert(insert_at, gateway);
        }
        None => {
            object.insert("messages".to_string(), Value::Array(vec![gateway]));
        }
    }
    Some(next)
}

/// 前置：把网关提示词插到 `messages[0]`，客户端消息（含它自己的 system 块）
/// 整体后移一位。
///
/// 与 [`append`] 的**唯一**差别是插入位置：append 把网关提示词放在开头连续
/// system 块**之后**（客户端规范优先，网关只做补充），prepend 放在**最前**
/// （网关规则优先，客户端规范跟在它后面）。客户端本来就没有 system 时两者
/// 输出完全相同（append 的 `insert_at` 也是 0）。
///
/// 不合并成一条 system：两条并列才能在请求日志与调试报文里分清哪句是网关写的。
pub fn prepend(body: &Value, text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    let mut next = body.clone();
    let object = next.as_object_mut()?;
    let gateway = gateway_message(text);
    match object.get_mut("messages").and_then(Value::as_array_mut) {
        Some(messages) => {
            messages.insert(0, gateway);
        }
        None => {
            object.insert("messages".to_string(), Value::Array(vec![gateway]));
        }
    }
    Some(next)
}

/// 网关提示词的消息形态。
///
/// 角色用 `system` 而不是 `developer`：上游的 role 白名单里没有 `developer`
/// （见 `providers::workbuddy::normalize` 的角色归一），插 `developer` 等于
/// 制造一次必然归一 + 多余的拦截风险窗口。参考项目也是同一条理由。
fn gateway_message(text: &str) -> Value {
    json!({ "role": "system", "content": text })
}

/// 该消息是否承载 system 级指令：角色**精确**等于 `system` 或 `developer`。
///
/// 精确匹配（不是前缀/包含）：其余角色（user / assistant / tool / 任意未知值）
/// 一律不动 —— 这是「既有消息逐字不动」的判据，宽松一点就会误删用户内容。
fn is_system_role(message: &Value) -> bool {
    message
        .get("role")
        .and_then(Value::as_str)
        .map(|role| role == "system" || role == "developer")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{PromptChoice, PromptMode, PromptPlan, PromptSource, DEGRADED_PROMPT};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn plan<'a>(
        mode: PromptMode,
        global: &'a str,
        model_prompts: &'a BTreeMap<String, String>,
        providers: &'a BTreeMap<String, crate::server::config::ProviderPrompt>,
    ) -> PromptPlan<'a> {
        PromptPlan::new(
            PromptChoice { mode, text: global, source: PromptSource::Inline },
            model_prompts,
            providers,
        )
    }

    #[test]
    fn exact_model_match_overrides_provider_text_and_keeps_mode() {
        let mut model_prompts = BTreeMap::from([("model-a".to_string(), "model text".to_string())]);
        let providers = BTreeMap::from([(
            "provider-a".to_string(),
            crate::server::config::ProviderPrompt {
                mode: PromptMode::Append,
                file: None,
                inline: Some("provider text".to_string()),
                text: "provider text".to_string(),
                source: PromptSource::Inline,
                file_error: None,
            },
        )]);
        let first_plan = plan(PromptMode::Custom, "global text", &model_prompts, &providers);
        let prompt = first_plan.for_provider("provider-a", "model-a");
        assert_eq!(prompt.mode, PromptMode::Append);
        assert_eq!(prompt.text, "model text");

        model_prompts.insert("model-a-suffix".to_string(), "suffix".to_string());
        let second_plan = plan(PromptMode::Custom, "global text", &model_prompts, &providers);
        let unmatched = second_plan.for_provider("provider-a", "model");
        assert_eq!(unmatched.text, "provider text");
        let whitespace_unmatched = second_plan.for_provider("provider-a", " model-a ");
        assert_eq!(whitespace_unmatched.text, "provider text");
    }

    #[test]
    fn model_prompt_reuses_custom_and_append_behavior() {
        let models = BTreeMap::from([("model".to_string(), "model text".to_string())]);
        let providers = BTreeMap::new();
        let body = json!({
            "messages": [
                {"role": "system", "content": "client"},
                {"role": "user", "content": "hello"}
            ]
        });

        let custom = plan(PromptMode::Custom, "global", &models, &providers)
            .for_provider("provider", "model")
            .apply(&body, false)
            .expect("custom mode should rewrite");
        assert_eq!(custom["messages"][0]["content"], "model text");
        assert_eq!(custom["messages"].as_array().unwrap().len(), 2);

        let append = plan(PromptMode::Append, "global", &models, &providers)
            .for_provider("provider", "model")
            .apply(&body, false)
            .expect("append mode should add a message");
        assert_eq!(append["messages"][0]["content"], "client");
        assert_eq!(append["messages"][1]["content"], "model text");

        let degraded = plan(PromptMode::Append, "global", &models, &providers)
            .for_provider("provider", "model")
            .apply(&body, true)
            .expect("degraded append should rewrite with the neutral prompt");
        assert_eq!(degraded["messages"][0]["content"], DEGRADED_PROMPT);

        let passthrough = plan(PromptMode::Passthrough, "global", &models, &providers)
            .for_provider("provider", "model")
            .apply(&body, false);
        assert!(passthrough.is_none());
    }

    /// prepend 与 append 只差插入位置：客户端有 system 块时，网关提示词
    /// 落在它**前面**（append 落在它后面）。
    #[test]
    fn prepend_puts_gateway_prompt_before_client_system() {
        let models = BTreeMap::new();
        let providers = BTreeMap::new();
        let body = json!({
            "messages": [
                {"role": "system", "content": "client-a"},
                {"role": "developer", "content": "client-b"},
                {"role": "user", "content": "hello"}
            ]
        });

        let prepended = plan(PromptMode::Prepend, "gateway", &models, &providers)
            .for_provider("provider", "model")
            .apply(&body, false)
            .expect("prepend mode should add a message");
        let messages = prepended["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["content"], "gateway");
        assert_eq!(messages[0]["role"], "system");
        // 客户端消息逐字不动、顺序不变，只是整体后移一位
        assert_eq!(messages[1]["content"], "client-a");
        assert_eq!(messages[2]["content"], "client-b");
        assert_eq!(messages[3]["content"], "hello");

        // 同一份 body 走 append：网关提示词落在开头连续 system 块之后
        let appended = plan(PromptMode::Append, "gateway", &models, &providers)
            .for_provider("provider", "model")
            .apply(&body, false)
            .expect("append mode should add a message");
        let messages = appended["messages"].as_array().unwrap();
        assert_eq!(messages[0]["content"], "client-a");
        assert_eq!(messages[2]["content"], "gateway");
    }

    /// 客户端本来就没有 system 时，prepend 与 append 输出完全相同（都插在 0 号位）
    #[test]
    fn prepend_matches_append_when_client_has_no_system() {
        let models = BTreeMap::new();
        let providers = BTreeMap::new();
        let body = json!({ "messages": [{ "role": "user", "content": "hello" }] });

        let prepended = plan(PromptMode::Prepend, "gateway", &models, &providers)
            .for_provider("provider", "model")
            .apply(&body, false)
            .expect("prepend should add a message");
        let appended = plan(PromptMode::Append, "gateway", &models, &providers)
            .for_provider("provider", "model")
            .apply(&body, false)
            .expect("append should add a message");
        assert_eq!(prepended, appended);
    }

    /// 没有 messages 字段时补一个只含网关提示词的数组（与 append / rewrite 同口径）
    #[test]
    fn prepend_creates_messages_when_absent() {
        let models = BTreeMap::new();
        let providers = BTreeMap::new();
        let body = json!({ "model": "m" });
        let prepended = plan(PromptMode::Prepend, "gateway", &models, &providers)
            .for_provider("provider", "model")
            .apply(&body, false)
            .expect("prepend should add a message");
        assert_eq!(prepended["messages"].as_array().unwrap().len(), 1);
        assert_eq!(prepended["messages"][0]["content"], "gateway");
        assert_eq!(prepended["model"], "m");
    }

    /// 降级期 prepend 退化为替换：客户端 system 全删、只留中性提示词
    #[test]
    fn prepend_degrades_to_rewrite() {
        let models = BTreeMap::new();
        let providers = BTreeMap::new();
        let body = json!({
            "messages": [
                {"role": "system", "content": "client"},
                {"role": "user", "content": "hello"}
            ]
        });
        let degraded = plan(PromptMode::Prepend, "gateway", &models, &providers)
            .for_provider("provider", "model")
            .apply(&body, true)
            .expect("degraded prepend should rewrite");
        let messages = degraded["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], DEGRADED_PROMPT);
        assert_eq!(messages[1]["content"], "hello");
    }

    /// 逐提供商覆盖：某家配成 prepend 时，全局是什么模式都不影响它
    #[test]
    fn provider_level_prepend_overrides_global_mode() {
        let models = BTreeMap::new();
        let providers = BTreeMap::from([(
            "provider-a".to_string(),
            crate::server::config::ProviderPrompt {
                mode: PromptMode::Prepend,
                file: None,
                inline: Some("provider gateway".to_string()),
                text: "provider gateway".to_string(),
                source: PromptSource::Inline,
                file_error: None,
            },
        )]);
        let body = json!({
            "messages": [
                {"role": "system", "content": "client"},
                {"role": "user", "content": "hello"}
            ]
        });
        // 全局是 custom，但这家的覆盖是 prepend
        let global_plan = plan(PromptMode::Custom, "global text", &models, &providers);
        let prompt = global_plan.for_provider("provider-a", "model");
        assert_eq!(prompt.mode, PromptMode::Prepend);
        let applied = prompt.apply(&body, false).expect("prepend should add a message");
        let messages = applied["messages"].as_array().unwrap();
        assert_eq!(messages[0]["content"], "provider gateway");
        assert_eq!(messages[1]["content"], "client");

        // 没配过的家仍走全局的 custom
        let other = global_plan.for_provider("provider-b", "model");
        assert_eq!(other.mode, PromptMode::Custom);
    }
}
