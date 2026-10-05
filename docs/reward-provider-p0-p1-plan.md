# P0–P1 奖励型提供商集成方案与实施状态

## 最终落位

本阶段把“模型反代凭证”和“奖励领取凭证”按提供商协议拆开：

- **MiniMax Code** 和 **LobsterAI** 是原生 Provider，使用统一账号存储、网页登录、手动凭证、刷新、模型目录、额度查询和自动奖励入口。
- **AStudio** 和 **百度搭子 DuMate** 仍是“添加账号 → 预置 API”模板，只通过 `rewardProfile` 参加预置 API 奖励；模型 API Key 与奖励 Cookie 分开保存。
- 不迁移四家旧 custom 账号、旧 `rewardProfile` 或历史奖励记录。原生账号 ID 只使用 `minimax-code`、`lobsterai`。

## 协议与钱包

| 提供商 | 模型协议 | 主要上游 | 奖励链路 | 额度展示 |
| --- | --- | --- | --- | --- |
| MiniMax Code | Anthropic Messages | `agent.minimax.cn/mavis/api/v1/llm/v1/messages?beta=true` | `signin/status` → `signin/claim` | `gift`、`paid`、`platform` 分桶，按包保留最早到期时间 |
| LobsterAI | OpenAI Chat Completions | `lobsterai-server.youdao.com/api/proxy/v1/chat/completions` | 动态 `slot` → `context` → `actions/check_in` | `free`、`campaign`、`subscription`、`creditItems` 与总额 |
| AStudio | OpenAI 兼容预置 API | `maas-api.cn-huabei-1.xf-yun.com/v1` | 预置 API `rewardProfile=astudio` | 复用现有 AStudio 奖励解析 |
| DuMate | OpenAI 兼容预置 API | `dumate-svc.baidu.com/gateway/apis/v1` | 预置 API `rewardProfile=dumate` | 复用现有 DuMate 奖励解析 |

MiniMax Code 发送 Anthropic 头、`?beta=true` 和工具 `type: custom`，上游 SSE 交给现有 Anthropic 转换链。LobsterAI 发送 `clientVersion=2026.9.4`、`User-Agent: LobsterAI/2026.9.4` 与 `X-LobsterAI-Client-Version`，动态读取 `activityCode` 和 `configRevision`，每次领取生成 UUID 幂等键。

两家原生账号都支持 flat/nested 凭证、JWT 身份回退、refresh-flight 单飞、刷新后二次快照、refresh token 轮换的条件写回以及 401 单次刷新重试。公开账号只返回身份、`tokenTail`、到期时间和刷新状态，不返回 token。

## 奖励调度

自动签到选项固定为：

```text
workbuddy
raccoon
autoclaw
autoclaw-intl
qoder
trae
minimax-code
lobsterai
reward-custom
```

展示文案固定为：

```text
MiniMax Code（每日签到）
LobsterAI（活动奖励）
预置 API 奖励（AStudio / DuMate）
```

结果行保留 `success`、`alreadyCompleted`、`claimable`、`status`、`reward`、`wallet`、`expiresAt`、`msg`，并增加 `rewardKind`：

- `daily_checkin`：每日签到；
- `activity_reward`：动态活动奖励；
- `preset_api_reward`：AStudio / DuMate 预置 API 奖励。

只有上游明确返回成功或已完成时才写入账号的当天 `checkinAt`。无活动、无资格、没有 `check_in` 动作和不可领取状态都返回中性结果，不重复写完成状态。

## UI 与账号入口

- `desktop-tauri/ui/preset-providers.js` 仅保留预置 API 的 AStudio、DuMate，且二者位于列表末尾并保持该顺序。
- 添加账号弹窗的原生 Provider 区域增加 MiniMax Code、LobsterAI；两家都提供网页登录和手动 `accessToken / refreshToken`。
- 原生网页登录继续复用 `/api/session/login/start`、`/api/session/login/callback`、`/api/session/login/wait`，回调由一次性 `state` 保护。

## GitHub 参考

- [weixiaokuan123/minimax-proxy](https://github.com/weixiaokuan123/minimax-proxy)：MiniMax Code Messages、签到状态/领取和额度分桶。
- [jacek4yang/lobsterai-proxy](https://github.com/jacek4yang/lobsterai-proxy)：LobsterAI OAuth refresh、动态活动 slot/context/check-in、SSE 业务错误。
- [netease-youdao/LobsterAI](https://github.com/netease-youdao/LobsterAI)：官方活动字段、版本头和授权参数事实来源。
- [qixing-jk/all-api-hub](https://github.com/qixing-jk/all-api-hub)、[aceHubert/newapi-ai-check-in](https://github.com/aceHubert/newapi-ai-check-in)：provider registry、幂等领取、余额核验和状态分类。

实现借鉴协议和结构，没有复制第三方代码片段；实际接入仍以当前上游响应和账号资格为准，不把社区快照中的固定奖励金额写死。

## P1 后续项

当前原生 Provider 使用静态模型兜底清单，并在目录层保留远程刷新扩展点。后续可以在确认官方目录端点和缓存语义后增加远程模型刷新、账号详情额度分桶、延迟到账核验、刷新/活动状态指标和协议变更 fixture。没有授权测试账号时，不能把 fixture 或 HTTP 200 解释为真实奖励到账。

## 验证与交付

本次实现已覆盖 provider 注册与 adapter 穷举、flat/nested 凭证、OAuth state/callback、刷新条件写回、MiniMax 状态与额度聚合、LobsterAI 动态活动与幂等键、SSE 业务错误、自定义奖励白名单、默认签到选项和 `rewardKind`。

验证命令：

```text
cargo test --manifest-path desktop-tauri/src-tauri/server/Cargo.toml
cargo clippy --manifest-path desktop-tauri/src-tauri/server/Cargo.toml -- -D warnings
npm --prefix desktop-tauri run build
```

完整 Rust 测试结果为 `446 passed; 0 failed; 5 ignored`，前端 Tauri 生产构建通过。完整 `cargo clippy -- -D warnings` 仍被基线中的既有 lint 阻断；新增 Provider、账号存储和奖励路径未产生定向 lint。真实 MiniMax/LobsterAI 账号请求、额度到账和签到到账留待具备授权测试账号后验收。
