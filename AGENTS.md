# AGENTS.md — 开发、华为云部署与正式发版流程

> 本文档面向维护者与 AI 代理：Agent2API（workbuddy）桌面端、Docker 镜像和华为云部署流程。
> 日常华为云部署与正式 GitHub/Docker Hub 发版是两条不同流程，必须按对应章节执行。

## 0. 版本号约定

- 版本号形如 `X.Y.Z`（如 `2.7.2`），日常小版本「递增 0.01」= 末位 +1（如 `2.7.8` → `2.7.9`）。
- **递增幅度以用户要求为准**：默认用上面的「递增 0.01」；用户另有要求时（如「这次递增 0.1」）一律听用户的，按用户指定的幅度算出新版本号，不要自作主张套用默认幅度。
- **版本号要同步改 5 处**（缺一处会导致安装包与显示版本对不上）：
  1. `package.json`（根）
  2. `desktop-tauri/package.json`
  3. `desktop-tauri/src-tauri/Cargo.toml`
  4. `desktop-tauri/src-tauri/tauri.conf.json`
  5. `desktop-tauri/src-tauri/server/Cargo.toml`
- 应用内「关于」与更新检查读的是 `env!("CARGO_PKG_VERSION")`（第 5 处），改完跑一次 `cargo check` 让 `Cargo.lock` 跟着刷新。

## 1. 提交：保证工作区干净

发版前必须把工作区收干净，tag 里的代码就是发出去的代码：

```bash
git status --short        # 必须为空；有改动就先提交或清理
cargo check               # 或前端有改动时做一次构建自检
```

## 2. 发布提交：更新日志写进提交信息

**发布提交的提交信息 = 更新日志**。原因：发版收尾脚本创建 / 更新 GitHub Release 时，正文取「tag 所指提交的提交信息」（`git log -1 --format=%B`）。

- 格式沿用历史版本的编号清单，每行一条，带分类前缀：`新增：` / `优化：` / `修复：` / `更新：` / `移除：`，最后一条固定 `版本：X.Y.Z → X.Y.Z+1`；
- 内容取「上个 tag 以来的全部提交」综合整理（`git log vX.Y.Z..HEAD --oneline`），纯 CI/临时的调试提交归并成一条流程性描述即可；
- 本次没有代码改动时用空提交承载：`git commit --allow-empty -F 更新日志.txt`（历史发版两种做法都有先例）。

## 3. 日常华为云部署

华为云（ssh-manager 服务器名 `huaweiyun`，<华为云IP-见本地ssh-manager配置>，国内节点）的日常部署以远程
`dev/pr-integration` 分支为唯一代码来源。项目本地分支名为 `pr-integration`，不要把
本地分支名误写成远程分支名。

**部署形态与旧 us2 流程不同**：服务器项目目录 `/opt/docker_projects/agent2api` 不是
git 仓库，服务器上没有 `update_version.sh`，也不在服务器内构建。镜像是「本地/构建机
构建 → `docker save/load` 传输 → 固定 tag 替换」；实际生效的 Compose 文件是
`.deploy/docker-compose.huawei.yml`（端口 `0.0.0.0:19050→3065`，`TZ=Asia/Shanghai`，
数据卷 `./data`）。服务器只有 2 核 / 1.7GB 内存，禁止在服务器内做 Rust 构建或测试。

### 3.1 本地提交与推送

1. 在本地完成代码修改、必要测试和审查。
2. 将变更提交到本地 `pr-integration`；其他分支的变更先合并到该分支。
3. 确认工作区没有未提交的业务改动后，推送到部署使用的远程分支：

   ```bash
   git status --short
   git push origin HEAD:dev/pr-integration
   ```

   推送目标必须是 `origin/dev/pr-integration`。不得通过直接修改服务器文件来绕过 Git 推送流程。

### 3.2 华为云镜像发布与替换

1. 在本地（或构建机）从要发布的提交构建镜像，tag 必须带版本与提交短哈希：
   `agent2api:<版本>-<提交前8位>`（如 `agent2api:3.0.9-43accea5`）。
2. 传输并加载到服务器后，**先做版本化备份**，再替换：
   - 备份 `data/`（SQLite 库）与 `.deploy/docker-compose.huawei.yml` 到
     `backups/agent2api-pre-<tag>-<UTC时间戳>/`（沿用既有命名）；
   - 更新 `.deploy/docker-compose.huawei.yml` 的 `image:` 为新 tag；
   - `docker compose --env-file .env -f .deploy/docker-compose.huawei.yml up -d` 替换容器。
     `--env-file` 不能省：compose 默认只读「compose 文件所在目录」的 `.env`，也就是
     `.deploy/.env`，而项目的变量文件在**项目根目录**。漏掉它时 compose 里所有
     `${AGENT2API_*:-}`、`${ZCODE_*:-0}` 都渲染成默认值 —— 面板管理员变成空（回落到
     库内注册的管理员）、ZCode 直连开关被静默压回 0，且 `docker compose config` 看不
     出异常（渲染结果本身就是「合法」的默认值）。`scripts/deploy-hw.sh` 已固定传入。
3. 面板管理员由根 `.env` 的 `AGENT2API_ADMIN_USER` / `AGENT2API_ADMIN_PASSWORD` 提供，
   `access::verify_login` **环境变量优先**：一旦这对变量非空，面板登录只认它们，
   库内注册的管理员（kv `panelAdmin`）与「首次注册」入口同时失效。改动后必须用
   `POST /api/panel/login` 实测一次（登录需过 ALTCHA PoW：`GET /api/panel/captcha`
   取 salt/challenge/maxnumber，枚举 n 使 `sha256(salt+n)==challenge`，payload 为
   base64(JSON{algorithm,challenge,number,salt,signature})，放进登录体的 `captcha` 字段）。
4. 替换前确认服务器上没有正在运行的部署操作；不得执行 `docker compose down -v`、
   全局 prune 或删除未知备份。
5. 镜像内 `AGENT2API_SELF_UPDATE` 在华为云应保持 `0`：面板内自更新只替换
   容器文件、不更新 compose 镜像 tag，会造成版本漂移；版本一律走本节第 2 步的镜像替换。
   注意本仓库当前**没有**把它接进编译单元（`core/update/mod.rs` 未声明 `mod selfupdate;`，
   `selfupdate.rs` 与 `selfupdate::enabled/supported/apply` 均无调用点），所以设 `0`
   目前只是把口径写明确保将来接线时默认关闭，而不是修一个已存在的行为。
6. `update_version.sh`、`release.env`、`.deploy/status/` 等 us2 时代机制已废弃，
   不适用于华为云。

### 3.3 部署验收

替换完成后运行 `bash scripts/deploy-hw.sh verify <预期tag>`，并人工确认：

- `docker images` 中目标镜像 tag 存在；`docker inspect agent2api` 的
  `Config.Image` 已切换到目标 tag，`StartedAt` 已更新，`RestartCount` 从本批次基线核对；
- 容器处于运行/健康状态；`/health`、`/v1/models`、管理面板和认证边界符合预期；
- 至少完成一次授权范围内的真实 API 验证，并检查服务日志无新增 fatal/panic；
- 原有数据卷 `./data`、持久化配置和 `backups/` 未被替换或删除。

健康检查、页面 200 或认证成功只能证明对应层级，不得单独宣称真实模型请求成功。
容器仍使用旧镜像、目标镜像不存在、或替换进程尚未结束时，均判定为「尚未完成」，
不能按脚本退出码结案。

### 3.4 失败与回滚

部署失败时保留现场，记录失败阶段（构建、传输、备份或容器替换）。回滚 = 把
`.deploy/docker-compose.huawei.yml` 的 `image:` 改回上一个 tag 后
`docker compose --env-file .env -f .deploy/docker-compose.huawei.yml up -d`；数据库异常时优先用
`backups/` 里最近的版本化备份恢复 `data/`。不得删除未知备份，不得全局 prune。
回滚后重新验证容器、健康端点、认证边界和既有业务路径。

### 3.5 ZCode 直连开关（华为云口径待重验）

`ZCODE_INTL_CLAIM_DIRECT=1` / `ZCODE_INTL_START_PLAN_DIRECT=1` 是 **us2（海外节点）
实测代理领取 405/3012、直连 200 后得出的结论**。华为云是国内节点，实测直连
`api.workbuddy.pro` 不通（超时），当前容器两开关均为 `0`（沿用账号代理出口）。
us2 的直连结论**不能**照搬到华为云：改动开关前必须先用授权账号在面板做一次真实
领取/推理对照，确认到账后才能写入 `.env` / compose。

### 3.6 部署口径维护

当前部署目标是华为云 `huaweiyun`；分支口径、compose 路径、端口、镜像命名或部署方式
变化时，必须先同步更新本节、`scripts/deploy-hw.sh`、`scripts/verify-hw.sh` 与
`scripts/HW_DEPLOYMENT.md`，不能只修改其中一处。

## 4. 正式版本发布：打 tag 并推送

```bash
git push origin main
git tag vX.Y.Z
git push origin vX.Y.Z
```

推 `v*` tag 会**同时触发两个工作流**（`.github/workflows/`）：

| 工作流 | 产出 | 说明 |
|---|---|---|
| `build.yml`（build） | Windows NSIS 安装包 + macOS universal dmg | `macos` / `windows` 两个 job 构建并上传 artifact（macOS 包**只能在 CI 构建**，无法从 Windows 交叉编译）；安装包只挂 artifact，GitHub Release 由本地脚本挂载（见第 5 节） |
| `docker.yml`（docker） | Docker Hub `aimodcc/agent2api:<版本>` + `:latest`（amd64 / arm64 双架构） | 手动 `workflow_dispatch` 触发时只出 `:dev` 测试 tag，不碰正式 tag |

跟踪进度（手动跑 gh 前要先设代理，见第 7 节）：

```bash
gh run list --limit 4          # 确认工作流都已触发
gh run watch <run-id> --exit-status
```

## 5. 发版收尾：挂 GitHub Release（本地脚本一条命令）

GitHub Release **不由 CI 发布**（Release 本来就不会自动创建），由本地脚本
一步挂载：

```bash
bash scripts/release.sh vX.Y.Z            # 自动找该 tag 的成功 build run
bash scripts/release.sh vX.Y.Z <run-id>   # 或显式指定 run
```

脚本做三件事：下载 build 的两个安装包 artifact 到 dist/ → 按 tag 提交信息
（= 更新日志，见第 2 节）创建 / 更新 GitHub Release 并挂附件 → 打印验收
提示。幂等可重跑（`--clobber` 覆盖附件）。

## 6. 验收清单（三处核对）

- [ ] GitHub Release：`gh release view vX.Y.Z`（手动跑 gh 前先设代理，见第 7 节）—— 正文日志齐全，exe / dmg 两个附件都在；
- [ ] Docker Hub：`aimodcc/agent2api` 的 Tags 页出现 `<版本>` 与 `latest`，Pushed 时间一致；
- [ ] 安装包「关于」页版本号与 tag 一致。

## 7. 已知坑与排查

- **GHCR 新包默认私有**：若以后镜像改推 GHCR，首次推送后需到包设置手动改 Public（当前推的是 Docker Hub，无此问题）。
- **gh 不读 git 的代理配置，跑 gh 前要先设代理环境变量**：`gh` 是 Go 程序，不读 `~/.gitconfig` 里的 `http(s).proxy`，也不读 Windows 系统代理（WinINET），只认 `HTTPS_PROXY` / `HTTP_PROXY` 环境变量。本机 GitHub 直连时通时不通，手动执行 gh 命令前先设：

  ```bash
  export HTTPS_PROXY=$(git config --get-urlmatch http.proxy https://github.com || git config --get https.proxy)
  ```

  只设 `HTTPS_PROXY` 就够（gh 的请求全是 HTTPS，实测不读 `HTTP_PROXY`）。第 5 节的 `scripts/release.sh` 已内置这一步（从 git 配置读取后透传给 gh），用脚本发版无需手动设。代理没开时 gh 会立刻报 `proxyconnect tcp: ... connection refused` 而不回退直连 —— 与 `git push` 的表现一致。

## 8. 上游同步与本地能力保护

本地 `pr-integration` 是长期维护的下游产品分支，不是可随时用上游覆盖的临时分支。
本章适用于已授权的上游同步与合并；不自动授权提交、push、部署或移除功能。
验收目标是“本地行为契约仍成立且上游新增能力被正确整合”，不是仅证明没有冲突或构建成功。

### 8.1 冻结输入，保留当前工作

1. 先检查当前分支、tracking、工作区、暂存区、未跟踪文件、`MERGE_HEAD`、各 worktree 和远程目标；记录合并前本地提交 `H`、本轮上游提交 `U`、共同祖先 `B`。
2. 检查是否为浅仓库；历史不足时先补齐历史，再计算共同祖先和评估合并，不在错误基线上处理冲突。
3. 用户要求保持当前分支时，不切换工作区分支。本地 `main` 仅在确认可快进且未被其他 worktree 检出时更新；分叉或占用先分析，不机械执行强制更新。
4. 保留已有未提交工作、正在进行的合并和诊断产物；不擅自 stash、abort、reset、清理或将它们纳入本次提交。范围不清时先确认，不覆盖其他任务的工作。
5. 在隔离副本或适用的临时 worktree 中预演；记录候选源码树及测试输入。上游、本地输入或候选源码改变后，重新判断影响范围并重跑对应验收，不沿用旧结果。

### 8.2 建立并保护本地行为契约

- 合并前从本地独有提交、当前净差异、原始修复、最后有效实现、设计文档和已有测试梳理能力清单。提交仍在历史中、函数仍存在或文件仍有差异，都不是功能保留的证明。
- 每项记录：稳定契约 ID、稳定测试 ID、业务行为、来源提交、影响入口与调用链、配置/数据依赖、验证方法、验收证据及状态。新功能和修复应在同一变更中更新契约并提供验证。
- `docs/local-contracts.json` 是唯一的本地能力保护清单，记录稳定 ID、来源、入口和测试映射；本章表格只解释保护域，不维护第二套逐项清单。以合并前本地提交 `H` 中的版本为最低基线，不允许候选通过删除清单、入口或降低断言自行缩减要求。
- 状态仅使用：`必须保留`、`上游等价实现`、`已批准替代/移除`、`缺失`、`待验证`。上游吸收须有等价行为证据；替代或移除须明确业务变化并得到用户确认，不能静默降级。
- 初建清单不得把当前已知回归当成正确基线；“缺失”和“待验证”必须保留为待解决项，不能因当前版本也失败而降低预期。

保护域说明如下；新增能力须更新 JSON 清单，不把本表当作永久完整清单：

| 保护域 | 必须核验的行为 |
|---|---|
| 三协议用量与原始报文 | Chat/Responses/Anthropic 流式与非流式最终用量一致；晚到 usage 和 telemetry 补齐；缓存读取/创建正确拆分；缺失与显式零分开；零值占位不覆盖真实计数；报文不重复采集、分片及截断状态正确 |
| 账号路由与错误分派 | balanced/priority/roundRobin 策略、容量与冷却有效；冷却按上游真名；HTTP 200 业务错误及 405 拒绝正确分类；指定账号测试不换号；积分保护不被兜底路由绕过 |
| 原生 MiniMax Code、LobsterAI | 注册、添加账号、网页登录/手动凭证、刷新、模型、转发、余额、奖励和 UI 入口完整；部分凭证更新保留 refresh token；领取确认不冒充额度到账 |
| 自定义奖励提供商 | 模型与奖励凭证独立；仅明确启用的账号参与；原生提供商与 reward-custom 分派互不混淆；凭证不通过公共视图、日志或错误泄露 |
| 自动签到与任务调度 | 已完成不重复领取；活跃保活不冒充签到；部分失败不触发整批高频重试；窗口、去重、开关和失败状态保持约定 |
| WorkBuddy 国际版活跃任务 | 地区分派、会话创建及 ACP 链路可用；真实正文和正常终止才算通道成功；错误、空流、截断不能算成功；保活成功不等于奖励到账 |
| 积分、成长福利与使用策略 | 明细精度、到期时间、账号身份及缓存新鲜度正确；未知余额不当作已核验零；查询失败不冒充成功或跳过；自动操作遵守显式开关与条件 |
| Qoder 国内/国际功能 | 端点、模型刷新、额度分桶、活动窗口、签到结果及展示按地区一致；错误类别不触发错误的网关切换 |
| ZCode 套餐、验证码与思考等级 | 领取和推理出口独立；验证码一次性使用、过期清理、取消/超时释放等待者；套餐领取去重及切换正确；三协议参数与客户端优先级一致 |
| Docker/远程登录与面板会话 | headless、桌面和远程回调路径均适用；state、已登记回调目标、凭证刷新和面板会话校验保持；不以桌面登录正常代替 Docker 验收 |
| 数据库、配置与账号兼容 | 上游和本地两条历史数据线均可升级；字段、默认值、provider ID 和配置键的意义保持；旧数据、索引和缺失/零值语义不丢失 |
| 前端、生成产物及部署 | 后端能力仍有对应 UI/API/bridge 入口；真实构建产物匹配源码；测试源码树、提交、推送目标和部署镜像可对应；部署仍遵守第 3 章 |

### 8.3 不只审查冲突文件

合并前审查 `B → U` 的上游变化；候选生成后同时审查 `H → 候选` 和 `U → 候选`：

- 前者检查本地行为是否改变或丢失，后者检查本地增量最终落位及上游新增能力是否被旧实现覆盖。
- 自动合并的文件也须纳入影响分析，重点检查调用点、分派/注册、参数、默认值、配置键、schema、测试和前端入口的删除或变更。
- 共享协议、路由、账号存储和任务分派变化时，扩大到相关提供商与入口验证，不能只测本次出现冲突的提供商。
- 不以整文件接受 ours/theirs、全局合并策略或盲目重放旧修复代替语义整合；相同能力允许改用上游实现，但必须满足对应契约。

### 8.4 按完整链路解决冲突

先读相关实现、每个调用方、注册/分派和测试上下文，再修改。提供商链路至少覆盖：

```text
注册 → 添加账号 → 登录/回调 → 存储/刷新 → 模型目录/映射
    → 选路/转发 → 用量/余额/奖励 → 调度 → API/bridge/UI
```

按“用户要求保留的行为 + 上游必要适配”做最小修改。函数还在但调用路径、默认选项或 UI 入口已丢失，仍判定为功能丢失；只恢复旧实现而丢掉上游新增能力也不合格。

### 8.5 防止实现和测试一起被删除

- 复用现有 Rust、Node 和浏览器测试；仅为关键链路补少量独立契约测试，不要求把所有内联测试搬走或复制成两套。
- 检查关键用例是否存在、是否被忽略、断言是否被削弱及样本是否失去代表性。删除、改名或替代须提供契约映射和等价验证；测试数量、覆盖率、过滤后零用例及退出码不能单独作为通过依据。
- 用量测试按最终完整计数验收，并验证正文及时输出；不要把“字段必须全部在首帧出现”当成唯一修复方式，也不要通过整轮等待掩盖流式回归。
- P0 每次必跑：用量、账号选路/错误切换、凭证刷新、奖励去重及关键迁移。P1 按影响链路扩展提供商、积分/成长、地区及配置测试。涉及 UI、headless、登录或运行材料时补对应平台和生成产物验收。
- Rust 从 `desktop-tauri/src-tauri` 执行 `cargo check --locked`、`cargo test -p agent2api-server --locked`，并显式执行关键契约用例。前端除 typecheck/build 外还须执行受影响的 Node/浏览器业务测试；build 不等于业务测试通过。
- `scripts/check-merge-contracts.py` 使用标准库检查清单、入口、测试存在性与合并前基线；`--rust-list` 校验所有 target 的真实编译列表，`--node-report` 校验完整 TAP 中实际通过且未跳过的 Node 用例；失败、取消、截断和过滤后零用例均不能通过。基线的实现入口、状态、来源与测试映射不得由候选静默削弱。它不证明断言未削弱或业务正确，仍须代码审查和行为测试。
- 合并验收完全以本地门禁为准。`scripts/local-merge-gates.py --base-ref <H>` 串行完成合并工具、契约清单和 H 基线门禁，再并行完成 Rust 与前端/浏览器门禁；它使用 `scripts/replay-merge-baseline.py`、`scripts/check-merge-contracts.py` 和真实候选测试，输出独立报告与摘要到 `.verify/local-merge-gates/`。不访问 GitHub，不读取 GitHub Actions 状态，也不把远端提交状态当作业务验收。
- `.github/workflows/verify.yml` 已移除；`build.yml`、`docker.yml` 等仍只服务于明确的正式发版产物，不参与上游合入放行。提交、push 前必须完成本地候选验收、`merge-flow.py seal/check --commit` 和敏感信息检查，push 后只核对目标远端完整 SHA；门禁自身、清单、保护用例和断言的削弱须单独审查。
- 业务模拟优先使用无秘密、无真实领取及模型费用的样本；真实外部验证遵循既有授权与副作用边界。

### 8.6 双来源数据兼容

本地与上游可能对相同 schema 版本号、配置键或 provider ID 赋予不同含义。合并不能仅比较版本号：

1. 分别用上游历史结构和本地历史结构构造样本，覆盖全新库、存量升级、重复升级及中断重试。
2. 验证实际列、索引、默认值、账号身份、计数精度和旧数据读取，不以 `user_version` 到达目标值作为成功证明。
3. 已发布迁移不重新赋义或仅改编号；使用前向兼容迁移解决分叉，并验证必要字段最终确实存在。
4. 账号/配置迁移严格遵守已选业务边界；无历史数据或已明确不迁移时，不擅自增加遗留迁移。

### 8.7 验收报告与否决条件

每项保护能力必须填写最终状态，并附源码树/提交、实际命令、退出状态及测试或接口/报文证据。分别报告构建、模拟业务和真实外部业务结果；不得用一句“已保留本地业务逻辑”代替逐项结论。

下列情况不进入业务发布验收：

- 必须保留的能力为缺失或待验证，或关键用例被删除/忽略而无等价证明；
- 存在未解决冲突、测试失败、迁移结构缺项、业务断言不满足或生成产物与源码不匹配；
- 验证后的候选源码树已改变，或推送/部署目标与已验收提交不一致。

失败时保留合并前 SHA、工作内容和验收证据，按已约定恢复方式处理；不擅自破坏性重置、清库或重写远端历史。恢复后重验对应行为，副本恢复不宣称生产已回滚。

### 8.8 提交、推送与后续维护

- 仅在用户授权范围内提交、push 和部署；业务推送目标及部署验收继续遵守第 3 章，不因合并授权自动执行正式发版。
- 提交前按文件范围核对 staged/提交差异，排除凭据、运行数据、备份和 `diagnostic-artifacts/`；核对实际推送提交及远端 SHA，不强推。
- 独立文档提交只包含明确指定文件，保留其他任务的未提交工作；它不意味着业务工作区已干净或可部署。纯文档变更检查文档结构和差异，不为此启动业务构建、真实模型请求或部署。
- 后续本地修复/新功能同步更新契约与验证；上游吸收后改为等价实现并保留验证，已批准移除才退出保护。通用修复可评估回馈上游以减少长期差异，但不自动对外提交。

### 8.9 实现演进不是删除行为

1. 契约 `id` 与每个 `tests[].id` 是持久身份，不随文件名、函数名或模块移动重新生成。一个业务能力可以拆成多个入口/用例；新能力增加新契约，不以替换原 ID 掩盖旧能力丢失。
2. 默认保护 H 的业务行为、P0 等级、来源提交、状态、实现入口和测试映射。等价技术重构允许改路径、API 或测试接线，但须在对应契约的 `evolution` 声明旧→新映射、原因、独立审查证据，并绑定 H 契约核心内容的 SHA256。摘要用检查器的 `contract_digest()` 计算，不是整个 JSON 文件的哈希。
3. `implementation_moves` 的 `from` 是 H 路径、`to` 是候选入口列表；`test_moves` 的 `from` 是 H 测试 ID、`to` 是候选测试 ID 列表，`mode` 为 `adapted` 或 `replacement`，附具体原因。目标不能为空，不能指向不存在的用例；当前跨测试kind迁移需先扩展对应真实报告接线与反例验证，再放行，不能把义务迁出所有验收域；本轮差异必须覆盖完整，旧摘要不得用于本轮放行。历史声明可以保留，但无本轮变化时不当作新的证明。
4. 原始独立 Rust/Node/浏览器断言先原样执行，原始报告与候选报告分开。API 变更导致旧接线编译/运行失败时，先分类并保留失败，再用清单顶层 `replay_patches` 在隔离副本中适配；每项包含 `path`、`files:[{path,sha256}]`、`reason`、`review`，绑定 H 原测试原始字节。一次性历史接线／平台时序适配可另加完整 `base_ref` SHA，仅在该 H 应用；未来 H 不匹配时只跳过该补丁，仍完整执行未来 H 原断言，不忽略匹配 H 的哈希错误。补丁只改声明的原独立测试文件，等价适配禁止改生产实现、样本输入、降低期望或跳过断言；已获具体授权的退休用例如与保留用例共用文件，调整须显式审阅并保留原始失败，不能顺带弱化保留契约。
5. 接线适配须由独立审阅者确认输入/断言语义不变；脚本只核验边界、哈希、补丁可应用和实际结果，不把非空 `review` 当语义证明。适配结果标注 `ADAPTED_REPLAY`，保留 `.original` 失败与 `.adapted` 结果，不把它说成原样通过。H 内联 Rust 用例按声明映射到候选编译/执行证据单独验证，不与 H 独立报告合并，也不宣称冻结了原内联断言。
6. 真实业务替代/退出保护不是等价技术重构。须先获得用户对具体行为变化的授权，在绑定 H 的 `evolution.business_change` 中保存 `authorization`、`reason` 和准确的 `previous_behavior`，由独立审阅核对授权与变化。退休契约保留原 ID、来源和审计信息，状态为 `已批准替代/移除`；新增替代能力另建契约。AI 自己填写一段“已批准”不是授权。

等价技术演进的字段形状如下（示意值须替换为本轮真实 ID、路径、哈希和证据）：

```json
{
  "evolution": {
    "baseline_sha256": "<H契约核心SHA256>",
    "reason": "上游把入口拆分；业务输入输出不变",
    "review": "<独立审阅记录及对应候选树>",
    "implementation_moves": [{"from": "old.rs", "to": ["new.rs"]}],
    "test_moves": [{"from": "stable-case-id", "to": ["stable-case-id"], "mode": "adapted", "reason": "模块路径调整"}]
  }
}
```

### 8.10 AI 执行入口、验收封存与提交

`scripts/merge-flow.py` 是标准库薄记录入口，不替 AI 做业务判断，也不自动 merge、stage、commit 或 push。每轮在被忽略的诊断目录保留独立记录，续作复用同一记录：

每轮先用 `scripts/prepare-merge-candidate.py` 从冻结的 H 创建隔离 worktree；脚本先拒绝上游删除 `AGENTS.md`、`docs/local-contracts.json` 或合并门禁脚本，再创建候选。主工作区只允许保留冻结时已存在的未跟踪诊断文件，不能在主工作区进入 `MERGE_HEAD`。在候选 worktree 内先 freeze，再运行 `preflight` 固化快照，然后执行 `git merge --no-commit --no-ff <U>` 并解决冲突。合并完成后运行 `check-merge-closure.py`；注册、迁移、桌面登录接线或生成产物闭包失败时，不得进入 Rust/前端长门禁。

```sh
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json freeze --upstream upstream/main
# 读取上游/本地历史、预演、按调用链实施；不切换当前工作分支。
# 先生成产物并明确暂存本轮文件；其他任务有改动时使用隔离候选，不替别人stash或提交。
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json preflight
git merge --no-commit --no-ff <U>
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json impact
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json overlap
# 对每个共同修改路径明确取舍；重复能力默认以上游实现为权威
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json overlap \
  --decision <path>=upstream_equivalent
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json plan
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json run --name integration-closure -- python scripts/check-merge-closure.py --base-ref <H> --report diagnostic-artifacts/<本轮>/integration-closure.json
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json status
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json run --name local-gates -- python scripts/local-merge-gates.py --base-ref <H>
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json run --name tools -- python -B -m unittest discover -s scripts -p 'test_*merge*.py' -v
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json run --name server --cwd desktop-tauri/src-tauri -- cargo test -p agent2api-server --locked
# 独立且不写源码的验收可合并启动，JSON 数组元素为 name/cwd/argv。
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json run-batch --tasks diagnostic-artifacts/<本轮>/gates.json
# 按同样方式记录清单检查、完整Node TAP、前端typecheck/build、浏览器、H重放和新增专项验收。
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json seal --required tools server <本轮其他必需验收名>
# 已执行 plan 时可省略 --required；显式参数只能增加验收项，不能缩减计划必需 gate。
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json seal
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json check
# 用户授权提交/push后执行；提交树须仍等于封存的候选tree。
python scripts/merge-flow.py --record diagnostic-artifacts/<本轮>/run.json check --commit HEAD
```

`overlap` 固定比较 `B→U` 与 `B→H`，输出 `overlap.json`，记录共同修改路径、规范化新增行、Rust/TypeScript 声明、API/config/JSON 字段、测试 ID 和生成产物信号。候选树变化会使报告失效，必须重新运行 `overlap` 和 `plan`。每个共同修改路径必须在封存前明确一种决策：

- `upstream_equivalent`：两边是同一能力，采用上游实现；
- `local_extension`：保留本地扩展，并说明它不被上游能力覆盖；
- `semantic_conflict`：业务语义确有冲突，已完成代码层取舍并保留验证；
- `local_only`：上游对应改动不构成本地同一能力，保留本地实现。

未分类路径会阻止 `seal`/`check`，不能用“无冲突”或“函数仍存在”替代决策。`overlap` 只记录分析和取舍，不自动修改源码、暂存、合并、提交或推送；决策完成后仍须按 `plan` 计算的门禁执行本地验收。

- `status` 输出当前阶段、下一动作、候选 tree、未完成回执、失败原因和冻结的 H/U/B；续作先读取它，不重复 freeze 或启动已在运行的验收。
- `plan` 固定当前候选 tree，比较 `B→U`、`H→候选`、`U→候选`，按变更路径和 `docs/local-contracts.json` 计算受影响领域、契约及必需 gate，并把摘要写入记录。候选改变后必须重新 plan、impact 和受影响验收。
- `run-batch` 只适用于彼此独立、不会修改受控源码/索引/分支的任务；任务并行执行，记录仍由单一进程串行汇总。任一任务失败、切换分支或候选 tree 改变，批次整体不能封存。
- `seal` 未提供 `--required` 时自动读取 `plan` 的必需 gate；没有计划或计划为空时仍拒绝空验收。`check` 成功后会把提交核验状态落盘，异常可由 `status` 继续定位。

- 同一记录的 run/seal/check 串行执行；原生排他锁阻止多代理覆盖回执。异常退出残留锁时先核对锁文件内 PID 与相关进程确实结束，再处理该锁，不自动抢占。
- 回执记录 argv、工作目录、候选 Git tree、实际退出码、输出哈希和执行前后是否变动。生成/测试命令修改受控文件、新源码未暂存、冲突未解、分支改变、旧树回执或输出篡改均不封存。新修改后明确重新暂存并重跑受影响验收，不复用旧树绿灯。
- 必需验收项由本轮影响分析明确，P0 必跑、受影响 P1/新增能力补齐；不把脚本允许的一条任意成功命令当业务验收。脚本不检查任意 shell 命令的语义，独立审阅须核对回执覆盖真实业务入口。
- 当前名字报告接口遇到受保护同名跨文件/target或重复运行名时拒绝歧义，不把另一个同名PASS当证据；保留稳定ID、用演进声明调整测试显示名，或先补精确locator报告。仅名字证据不覆盖未执行源与未映射同名输出的所有情况，须审阅真实target/文件。
- Rust 用全量 `cargo test -p agent2api-server --locked` 和所有 target 的 `-- --list`，清单检查器接收 `--rust-list`；Node/浏览器由 `replay-merge-baseline.py candidate-node|candidate-browser` 从清单动态发现，用例新增不必另改硬编码清单。`candidate-node --report <完整TAP文件>` 的结果交给检查器 `--node-report`。`scripts/local-merge-gates.py` 已固化这套本地命令矩阵，并在契约门禁通过后并行执行 server/frontend 两组独立验收。
- H 验收执行 `replay-merge-baseline.py gate|rust|frontend --base-ref <H>`；后两项各用独立 `--report`，Rust 同时传 `--candidate-rust-list <候选全量编译列表>` 校验演进后的内联入口。原样与适配两种结果分开，首次无 H 清单明确 BOOTSTRAP。
- 提交前检查 staged 范围及敏感信息；提交后核对封存 tree，push 到授权目标并核对远端完整 SHA。远端 SHA 核对不替代本地门禁，也不需要等待 GitHub CI。部署仍是独立授权流程，不能在未完成部署验收时结案为可部署。

### 8.11 每轮可复用的 AI 提示词

```text
按 AGENTS.md 第8章执行本轮已授权的上游同步/合入。
先检查分支、dirty/index/MERGE_HEAD/worktrees与历史完整性，保留他人工作，冻结H/U/B。
从原始修复和独有历史梳理契约；审查B→U、H→候选、U→候选，不只处理冲突。
逐项分类：保留、上游等价吸收、技术演进、新增能力、回归、需用户决定的业务变化。
读入口/调用方/分派/配置/数据/bridge/UI整条链，制定文件级方案及验收矩阵再实施。
允许重构入口，用稳定契约/测试ID及绑定H的演进映射证明等价；不把路径钉死。
原样重放H独立断言；接线不兼容时先保留失败、只做经审阅的测试接线适配，保留输入及期望。
新功能增加契约；行为删改先获得具体授权，不由AI自签批准。内联断言改动独立审阅。
生成产物后暂存明确范围，记录同一候选tree的实际命令/输出/退出码；候选变化重验。
独立审阅双来源迁移、关键协议/路由/凭证/奖励及本轮受影响平台，逐项报告证据及已知边界。
按授权提交与push，验证提交tree和远端完整SHA；不自动部署/发版/强推，不等待GitHub CI。
```
