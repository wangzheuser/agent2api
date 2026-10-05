# P0–P1 奖励型提供商集成方案

## 目标与边界

当前网关把“模型转发凭证”和“签到/活动凭证”放在同一条提供商账号链路里。四家候选的登录态、奖励钱包和模型 API 形态不同，直接复制四套内置 provider 会扩大登录、刷新、模型目录和余额改造范围。因此本阶段采用分层接入：

1. 复用现有 custom provider 的模型转发、模型目录、代理和账号优先级。
2. 在 custom provider 上增加奖励 profile，奖励凭证独立存储，只在服务端使用。
3. 统一提供奖励 profile 清单、账号奖励凭证配置、状态查询和领取 API。
4. 把奖励账号加入现有手动/定时签到调度，使用独立的 `reward-custom` 选项。
5. 奖励数量、资格、重复领取和有效期均以实时上游响应为准；本地只记录统一结果，不写死社区快照。

## 四家落位

| 优先级 | profile | 模型转发预置 | 奖励凭证 | 领取链路 | 钱包语义 |
| --- | --- | --- | --- | --- | --- |
| P0 | `astudio` | OpenAI 兼容 `https://maas-api.cn-huabei-1.xf-yun.com/v1` | AStudio Cookie（含 `ssoSessionId` / `account_id` / `token`） | `POST /tenant-app/init-web`；前后余额用 `totalAmount` 差值核验 | 普通积分与 Spark 积分分开回报 |
| P0 | `dumate` | OpenAI 兼容 `https://dumate-svc.baidu.com/gateway/apis/v1` | 百度搭子网页 Cookie | 先读 `loginBonusInfo`，再 `POST /api/dumate/points/loginBonus` | 领取结果以 `points`/`quota` 及上游业务码为准 |
| P1 | `minimax-code` | Anthropic `https://agent.minimax.cn/mavis/api/v1/llm/v1` | MiniMax Code access token | `GET /minimax-cloud/api/v1/signin/status` 后 `POST .../claim` | 签到赠予积分仅 MiniMax Code 使用、30 天有效 |
| P1 | `lobsterai` | OpenAI 兼容 `https://lobsterai-server.youdao.com/api/proxy/v1` | LobsterAI access token | slot → activity context → `actions/check_in`，每次带幂等键 | `free`/`campaign` 等额度桶由状态接口返回 |

模型 API Key 与奖励凭证是两个字段：模型调用使用 custom 账号的 `apiKey`，奖励领取使用 `rewardCredential`。只有配置了奖励 profile 的 custom provider 才进入奖励签到范围；普通 custom provider 不会被误打签到接口。

## GitHub 参考与候选取舍

本次选型以“能通过签到或活动任务周期性增加 API 可用额度”为硬条件，优先选择有公开请求链路、能在无桌面客户端的服务端执行、且可以在领取前后判断幂等状态的实现：

| 参考项目 | 公开链路 | 对本项目的用法 |
| --- | --- | --- |
| [weixiaokuan123/minimax-proxy](https://github.com/weixiaokuan123/minimax-proxy) | MiniMax Code `signin/status` → `signin/claim`，面板状态 `1/2/3/4`，领取结果 `1/2` | 直接复用端点和状态码语义；不把 Coding Plan、开放平台余额和签到赠予积分混成一个钱包 |
| [wearetheone777/dumate2api](https://github.com/wearetheone777/dumate2api) | DuMate `loginBonusInfo` → `loginBonus`，Cookie 网页态，另有 `quota_overview` | 复用 Cookie 认证和积分查询；任务/抽奖接口先不自动执行，避免把一次性活动误判成每日奖励 |
| [jacek4yang/lobsterai-proxy](https://github.com/jacek4yang/lobsterai-proxy) | 动态 slot → context → `actions/check_in`，请求带 `configRevision` 和 UUID 幂等键 | 复用动态活动码和 `creditItems` 分桶解析；slot 不可用时只返回不可领取，不发写请求 |
| [aceHubert/newapi-ai-check-in](https://github.com/aceHubert/newapi-ai-check-in) | Huan/WONG/AnyRouter/薄荷等 New API 或跨站签到活动 | 作为后续候选池。标准 `/api/user/checkin`、OAuth 登录奖励和跨站转盘需要独立凭证/周期语义，暂不混入本次四家适配器 |

AStudio 的上游链路来自当前官网静态客户端：登录初始化调用 `POST /xingchen-studio/tenant-app/init-web`，积分页调用 `GET /xingchen-studio/points/balance`。实现只用 `points/balance` 前后差值判断实际入账；没有差值时不会报告新增奖励。四家都不写死社区快照中的“每日多少积分”，上线前应使用脱敏测试账号各执行一次 `status → claim → status`，并把上游实际回执记录到验收报告。

## 接口与存储计划

### P0

- `GET /api/reward-providers`：返回四家 profile、协议、奖励凭证类型、端点和实时字段说明；不返回任何账号凭证。
- `POST /api/rewards/configure`：`{accountId, rewardCredential}`，只允许已注册 profile 的 custom 账号，空字符串清除奖励凭证；响应只返回 `rewardCredentialConfigured` 和尾号。
- `GET /api/rewards/status?id=...`：先读上游状态，不执行领取；返回 `claimable`、`alreadyCompleted`、`reward`、`wallet`、`expiresAt` 等已脱敏结果。
- `POST /api/rewards/claim`：执行幂等状态检查和领取；统一返回 `{success, alreadyCompleted, msg, reward, wallet, rawSummary}`。
- 自动签到：现有 `POST /api/auto-checkin` 的 `providerOptions` 增加 `reward-custom`，手动/定时批量签到复用同一 `checkin_for` 分派。

### P1

- 用真实账号逐家验证 status → claim → 再 status 的结果，补充模型目录和 token 续期；本次代码保留 profile 级适配边界，凭证失效返回可识别错误，不自动刷新共享桌面 refresh token。
- 若某家需要内置 provider 的 OAuth/桌面导入，再把该 profile 迁移为 `ProviderKind`，不改变奖励 API 与结果结构。

## 验收标准

- 纯函数测试覆盖四家响应解析、已领取、不可领取、动态活动未投放、奖励字段缺失、嵌套业务码、活动码路径编码和敏感字段脱敏。
- 不配置 `rewardProfile` 的 custom provider 不进入签到目标集合。
- 重复领取先读状态；上游已领取不会再次发起领取。
- `apiKey` 和 `rewardCredential` 均不会出现在公开账号、profile 列表、日志或错误摘要中。
- Rust 服务测试通过，前端预置目录类型检查通过；提交前在独立副本执行回滚脚本并确认基线测试仍可通过。

当前代码验收结果：当前分支基线服务测试 `389 passed; 0 failed; 5 ignored`，修改后服务测试 `404 passed; 0 failed; 5 ignored`；`ui-kit` typecheck、`ui-islands` build、奖励模块定向 `rustfmt --check` 和 `git diff --check` 均通过。真实账号领取尚未执行，因此四家上游到账金额与账号资格仍以发布前的脱敏实测为准。

## 实施顺序

1. 增加 profile 注册表和 custom provider 的 `rewardProfile` 读写校验。
2. 增加 custom 账号奖励凭证的写入、脱敏公开形态和读取快照。
3. 实现四家状态/领取适配器及统一 API。
4. 接入手动/自动签到分派和预置目录。
5. 写解析/幂等/范围过滤测试，运行 Rust 与 UI 检查。
6. 检查 diff、敏感信息和 worktree 状态，提交并推送到 `origin/dev/pr-integration`。
