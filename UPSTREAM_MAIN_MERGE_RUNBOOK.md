# `aimod-cc/agent2api` `main` 安全合入 `pr-integration` 执行手册

> 文档状态：**待人工确认的执行方案**
> 编制日期：2026-10-29（Asia/Singapore / UTC+8）
> 目标分支：本地 `pr-integration` → 远程 `origin/dev/pr-integration`
> 上游输入：仅 `upstream/main`，即 `aimod-cc/agent2api` 的 `main`
> 当前项目远程：`origin = wangzheuser/agent2api`> 本轮授权边界：本文件只规定后续执行方式；**不因本文存在而自动授权 merge、commit、push、部署、发版或移除本地功能**。

---

## 0. 先读结论

本项目不能用普通的“拉取上游然后解决几个冲突”方式同步。`pr-integration` 是长期维护的下游产品分支，承载大量上游没有、或已与上游实现分叉的业务能力；正确目标是：

> 在完整吸收上游新增能力的同时，逐项证明本地业务契约仍成立；不能只证明 Git 没冲突、代码能编译或测试数量看起来正常。

截至本文编制时，重新抓取远程后的精确状态是：

| 名称 | 精确提交 | 关系 |
|---|---|---|
| `U = upstream/main` | `68d0a4e4bfd8dc5a1daa296c61e0cc0b10a871da` | 已经是当前分支祖先 |
| `H = pr-integration` | `cc21ce55366ab528277bb711d143b223a28ec8b6` | 当前产品分支 |
| `B = merge-base(H,U)` | `68d0a4e4bfd8dc5a1daa296c61e0cc0b10a871da` | 与 `U` 相同 |
| `origin/dev/pr-integration` | `cc21ce55366ab528277bb711d143b223a28ec8b6` | 与 `H` 一致 |
| `origin/backup/pr-integration-20261029` | `cc21ce55366ab528277bb711d143b223a28ec8b6` | 已有备份 |

当前拓扑为：

```text
upstream/main  68d0a4e4
       │
       └── 本地 main  8efc03ad
                  │
                  └── pr-integration  cc21ce55
```

因此，**按当前 `upstream/main` 执行同步时必须判定为 no-op 并停止**。不得为了留下“同步记录”而制造空 merge commit，也不得把 `upstream/beta` 偷换成 `main`。

本文的主要用途是：当 `aimod-cc/agent2api` 的 `main` 将来前进后，按同一套可审计流程安全合入。

### 0.1 2026-10-29 首次执行记录

本轮已实际执行 fetch、远程核对和冻结检查：

```text
H = cc21ce55366ab528277bb711d143b223a28ec8b6
U = 68d0a4e4bfd8dc5a1daa296c61e0cc0b10a871da
B = 68d0a4e4bfd8dc5a1daa296c61e0cc0b10a871da
origin/dev/pr-integration = H
upstream/main ancestor-of H = true
```

因此本轮严格按 no-op 结束：

- 未创建候选业务 merge；
- 未制造空 merge commit；
- 未使用 `upstream/beta` 替代 `main`；
- 未 push、部署或发版；
- 已把本地 `upstream` push URL 设为 `DISABLED`，并将默认 push remote 固定为 `origin`；
- 已完成第 7.2 节的门禁编排前置修复；
- `python -B -m unittest discover -s scripts -p 'test_*merge*.py' -v` 实际运行 `103` 项测试并全部通过。

前置工具提交完成后必须重新冻结新的 `H/U/B`；若 `U` 仍为新 `H` 的祖先，仍按 no-op 结束。

---

## 1. 仓库身份与分支角色

### 1.1 唯一正确的远程身份

| 角色 | 名称 | 地址 | 权限用途 |
|---|---|---|---|
| 当前项目仓库 | `origin` | `https://github.com/wangzheuser/agent2api.git` | fetch + push |
| 上游仓库 | `upstream` | `https://github.com/aimod-cc/Agent2API.git` | **只 fetch，不 push** |

`haremank/agent2api` 是旧地址，后续流程中不得再作为 fetch、push、Release、更新源或文档事实依据。

### 1.2 目标分支

- 当前本地产品分支：`pr-integration`
- 对应远程部署分支：`origin/dev/pr-integration`
- 华为云日常部署只认 `origin/dev/pr-integration`
- 本文不要求更新 `origin/main`，也不经过本地 `main` 中转

### 1.3 为什么不先更新本地 `main` 再合到当前分支

本地 `main` 不是当前部署权威分支，并且保留了较早的 fork 版本号、CI、Docker、Release 和仓库接管策略。先把上游合入本地 `main`，再把 `main` 合入 `pr-integration`，会额外引入一层与本次目标无关的历史语义和发布策略。

本轮应直接使用：

```text
H = 当前 pr-integration
U = 本轮 upstream/main 的精确 SHA
B = merge-base(H, U)
```

并从 `H` 创建隔离候选 worktree。

---

## 2. 当前分支规模与保护基线

### 2.1 当前分支相对上游主分支的规模

当前 `pr-integration` 相对 `upstream/main`：

- 多出 `224` 个提交
- `448` 个路径发生变化
- 约 `58,621` 行新增、`16,933` 行删除

最近一次明确合入上游 `3.0.8` 的提交为：

```text
61d7dff68964c64c10ceabf25cfb2fcfc6576d8e
合并上游3.0.8并修复多账号签到与用量兼容
```

从该提交到当前 `H` 又新增：

- `33` 个提交
- `177` 个文件变化
- 约 `11,858` 行新增、`2,051` 行删除

这些后续变化集中在：

- Qoder 国内/国际设备名额、设备风控身份、活动领取与多账号公平性
- WorkBuddy 国际版渠道头、风控终止、后台幂等、日活与奖励独立核验
- ZCode 客户端版本、套餐 system 形状、领取结果展示
- 路由、限额、池耗尽、`Retry-After` 与错误分类
- 模型目录、模型 ID、免费模型倍率和助手模型来源
- 凭证有效期、密钥不匹配保护
- 华为云部署流程、根 `.env`、面板管理员及开关

这些不是可丢弃的临时补丁。

### 2.2 本地契约清单

`docs/local-contracts.json` 是本地能力保护的唯一权威清单。当前包含：

- `57` 项必须保留的契约
- `41` 项 P0
- `16` 项 P1
- `343` 个测试映射
  - Rust：`290`
  - Node：`45`
  - 浏览器：`8`

任何候选都不得通过删除清单、删除入口、改名后失联、跳过测试或降低断言来缩减义务。

### 2.3 主要保护域

| 保护域 | 核心要求 |
|---|---|
| 三协议与用量 | Chat / Responses / Anthropic 流式、非流式、晚到 usage、telemetry、缓存计数和原始报文一致 |
| 路由与错误分派 | balanced / priority / roundRobin、容量、冷却、HTTP 200 业务错误、405、429、503 与 `Retry-After` 正确 |
| 会话亲和 | 分配、完成、身份和设置不串号，不因重试、切换或冷却破坏绑定 |
| 原生提供商 | MiniMax Code、LobsterAI、Kuku、Qoder、ZCode、Trae 等完整链路可用 |
| 自动签到与奖励 | 窗口、补签、去重、部分成功、真实到账、未知结果和重试边界正确 |
| WorkBuddy 国际版 | 渠道校验、ACP、日活、奖励、风控终止和后台幂等正确 |
| 凭证与安全 | refresh token 保留、过期语义、密钥保护、验证码生命周期、面板会话轮换正确 |
| 数据库兼容 | 上游历史与本地历史都能前向升级，列、索引、默认值、账号身份和精度不丢失 |
| UI / bridge / 生成产物 | 后端功能有真实入口，源码与 `ui.js` / `ui.css` 一致，不能只保留死代码 |
| 构建与部署 | 版本号一致、发版策略不漂移、华为云根 `.env` 与部署边界保持 |

完整契约 ID 见本文附录 A。

---

## 3. 当前已发现的仓库治理不一致

这些问题不是本轮可以在冲突中“顺手猜”的内容。正式执行前必须明确处理。

### 3.1 `upstream` 仍有旧 push URL

当前本机配置为：

```text
upstream fetch: https://github.com/aimod-cc/Agent2API
upstream push:  https://github.com/haremank/agent2api
```

旧 push URL 必须移除或改成明确失败的哨兵值，防止误推。

建议在正式执行前，仅修改本地 `.git/config`：

```powershell
git remote set-url origin https://github.com/wangzheuser/agent2api.git
git remote set-url upstream https://github.com/aimod-cc/Agent2API.git
git remote set-url --push upstream DISABLED

git config remote.pushDefault origin
git config branch.pr-integration.pushRemote origin
git config branch.main.pushRemote origin
```

验证：

```powershell
git remote -v
git remote get-url origin
git remote get-url upstream
git remote get-url --push upstream
```

预期：

- `origin` fetch/push 都是 `wangzheuser/agent2api`
- `upstream` fetch 是 `aimod-cc/Agent2API`
- `upstream` push 显示 `DISABLED`

### 3.2 `TAKEOVER.md` 已过时

当前文件仍包含：

- `haremank/agent2api` 是本仓库
- `aimod-cc` 已消失

这些陈述已经不是当前事实。后续应单独更新治理文档，不应在解决业务冲突时把旧文档当作仓库身份依据。

建议将该修订做成独立治理提交，不与上游 merge commit 混在一起。

### 3.3 软件更新源存在“注释与值不一致”

当前：

```rust
/// 默认更新仓库：本仓库发布到自己的 GitHub Release
pub const DEFAULT_REPO: &str = "aimod-cc/agent2api";
```

实际值指向上游，但注释声称指向当前仓库。这里涉及产品行为，不能仅根据 Git remote 自动决定。

正式合并前必须选择并记录：

| 选择 | 含义 |
|---|---|
| 保持 `aimod-cc/agent2api` | 产品默认跟随上游 Release；当前仓库只维护部署分支 |
| 改为 `wangzheuser/agent2api` | 产品默认跟随当前项目自己的 Release |
| 仅靠 `WORKBUDDY_UPDATE_REPO` | 默认值保持上游，部署环境显式覆盖 |

没有明确决定时：

- 不得因上游冲突静默改变 `DEFAULT_REPO`
- 不得删除环境变量覆盖能力
- 不得把 README、Cargo metadata、Release 仓库和运行时更新源混为一谈

---

## 4. 非绑定的 beta 风险预览

本节只用于评估未来 `main` 可能吸收的架构变化，**绝不授权合并 beta**。

本文编制时：

```text
upstream/beta = c9ceffc61bc960a5af7c6739f33e741c1fafbd12
标签          = v3.3.0-beta.5
相对 main     = 25 个提交
```

与当前分支的静态预演结果：

- 上游 beta 变化路径：`1,364`
- 与本地变化路径重叠：`290`
- 直接触及本地契约：`47 / 57`
  - P0：`35`
  - P1：`12`
- Git 预演冲突：`243`
  - 内容冲突：`161`
  - 本地修改、上游删除：`80`
  - 双方新增：`2`

双方新增冲突包括：

- `AGENTS.md`
- `desktop-tauri/src-tauri/server/src/server/core/account_store/lobsterai_accounts.rs`

“本地修改、上游删除”主要集中在：

- Trae：10 个文件
- ZCode：8 个文件
- CatPaw：7 个文件
- Qoder：7 个文件
- Loomy：6 个文件
- WorkBuddy：6 个文件
- Antigravity：5 个文件
- AutoClaw：4 个文件
- CodeArts：4 个文件
- Kuku：4 个文件

其原因不是简单清理，而是上游把大量 provider 实现迁入插件架构。

如果这些提交将来进入 `main`，必须按“能力迁移”处理：

1. 先定位上游插件中的等价入口、协议和生命周期。
2. 把本地原生 provider 中的业务修复映射到插件或宿主扩展点。
3. 不得只保留被上游删除的旧 `.rs` 文件；旧文件仍存在不代表注册、分派和 UI 仍可达。
4. 不得只接受上游删除；这会直接丢失签到、奖励、路由、凭证、验证码和地区逻辑。
5. 每项迁移必须更新 `docs/local-contracts.json` 的 `evolution` 映射并提供测试证据。

---

## 5. 不可违反的执行原则

1. **只使用精确 SHA。** fetch 后冻结 `H`、`U`、`B`，之后不再用会移动的 `upstream/main` 代替 `U`。
2. **当前工作区不进入合并状态。** 所有预演和合并都在隔离 worktree 中完成。
3. **先判断 no-op。** 若 `U` 已是 `H` 的祖先，立即停止，不创建候选、不制造 merge commit。
4. **不切换输入。** 用户授权的是 `main`，不得改用 `beta`、tag 或其它分支。
5. **不 rebase、不 squash、不重写历史。** 上游提交应通过普通 merge 保留祖先关系。
6. **不使用全局 ours/theirs 策略。** 禁止 `-X ours`、`-X theirs`、整目录 checkout ours/theirs。
7. **不只看冲突文件。** 自动合并文件也必须参与影响分析和契约映射。
8. **不把编译通过当作业务通过。** 必须运行契约、Rust、Node、浏览器、迁移和生成产物门禁。
9. **不弱化测试。** 删除、忽略、改名、改断言或改变样本都要有等价映射和独立审阅。
10. **候选树改变即重验。** 任何源码、生成产物或暂存区变化都会使旧回执失效。
11. **不自动提交、push、部署。** 每个阶段都需要与授权范围一致。
12. **不强推。** 已推送后的回滚使用新 revert 提交，不重写远端历史。
13. **不提交凭据和运行数据。** 数据库、token、`.env`、日志、备份和诊断产物都不得进入提交。

---

## 6. 执行阶段总览

```text
阶段 0  修正本地 remote 安全配置，确认治理决策
阶段 1  fetch 并冻结 H/U/B；若 no-op 立即停止
阶段 2  创建本轮备份分支
阶段 3  预分析 B→U 与 B→H，建立影响清单
阶段 4  从 H 创建隔离候选 worktree
阶段 5  在候选内执行 --no-commit merge
阶段 6  按完整调用链和契约逐项做语义整合
阶段 7  生成前端产物、明确暂存范围
阶段 8  导出 impact / overlap / plan 并完成逐路径决策
阶段 9  运行闭包、契约、Server、Frontend、浏览器和迁移门禁
阶段 10 独立审阅
阶段 11 seal、commit、check --commit
阶段 12 当前分支 ff-only 到已验证提交
阶段 13 push 到 origin/dev/pr-integration 并核对完整 SHA
阶段 14 部署另行授权和验收
```

---

## 7. 阶段 0：执行前准备

### 7.1 必须先得到的确认

执行人应确认：

- 合并来源严格为 `aimod-cc/agent2api` 的 `main`
- 目标严格为 `pr-integration`
- 是否允许修改本地 Git remote 配置
- 是否允许创建并推送本轮备份分支
- 是否允许提交候选 merge commit
- 是否允许最终推送 `origin/dev/pr-integration`
- `DEFAULT_REPO` 应指向上游还是当前项目
- 本轮是否允许真实外部业务验证；若允许，明确账号、费用和副作用边界

### 7.2 工具编排前置修复及验证要求

本手册编制时发现：`merge-flow.py plan` 产生 `impact-review`、`integration-closure`、`contracts-p0`、`server`、`frontend`、`generated-artifacts` 等独立必需 gate，而旧版 `local-merge-gates.py` 只能一次运行全部分组，无法生成同名、同一候选树的独立回执。

本轮前置实施已按以下方式修复：

1. `local-merge-gates.py` 支持：

   ```text
   --group contracts|server|frontend|all
   ```

2. 每个 group 都会：
   - 验证 `base_ref`
   - 绑定运行前 candidate tree
   - 输出独立 `summary.json`
   - 运行后再次确认 candidate tree 未改变

3. `merge-flow.py plan` 将 `integration-closure` 设为每轮固定必需项，不再只在版本元数据变化时要求。

4. 新增 `scripts/check-merge-impact-review.py`，验证：
   - `overlap.unresolved` 为空
   - `impact`、`overlap`、`plan` 都绑定当前 candidate tree
   - review 文件绑定精确 H/U/B、plan digest 和 overlap digest
   - 每个 overlap path 都有与记录一致的决策、充分理由和证据
   - 不存在未解决的 P0/P1 finding

5. `prepare-merge-candidate.py` 把该 checker 纳入受保护治理文件。

6. 对应正反例已加入 `test_*merge*.py`。在真正开始业务 merge 前必须重新运行：

   ```powershell
   python -B -m unittest discover -s scripts -p 'test_*merge*.py' -v
   python scripts/local-merge-gates.py --help
   python scripts/check-merge-impact-review.py --help
   ```

这些前置工具修改必须先形成独立提交；提交后重新 fetch 并冻结新的 `H/U/B`。不得用 `python -c true`、空命令或只检查文件存在来伪造任何 gate 回执。

---

## 8. 阶段 1：fetch、核对远程并冻结输入

以下示例使用 PowerShell，在项目根目录执行。

```powershell
$ErrorActionPreference = 'Stop'

$Root = (Get-Location).Path
$Branch = (git branch --show-current).Trim()
if ($Branch -ne 'pr-integration') {
    throw "当前分支必须是 pr-integration，实际为 $Branch"
}

# 工作区、暂存区和未跟踪文件必须为空。
$Status = git status --porcelain=v1 --untracked-files=all
if ($Status) {
    $Status
    throw '工作区不干净；不得自动 stash、reset 或 clean'
}

if ((git rev-parse --is-shallow-repository).Trim() -ne 'false') {
    throw '仓库是 shallow clone；先补齐历史'
}

git rev-parse -q --verify MERGE_HEAD *> $null
if ($LASTEXITCODE -eq 0) {
    throw '当前已有 MERGE_HEAD；不得开始新一轮合并'
}

$Tracking = (git rev-parse --abbrev-ref --symbolic-full-name '@{u}').Trim()
if ($Tracking -ne 'origin/dev/pr-integration') {
    throw "tracking 不符合预期：$Tracking"
}

git fetch --prune --tags upstream
git fetch --prune origin

$RemoteULine = (git ls-remote upstream refs/heads/main)
if (-not $RemoteULine) {
    throw '无法读取 upstream/main'
}
$RemoteU = ($RemoteULine -split "`t")[0]
$U = (git rev-parse upstream/main).Trim()
if ($RemoteU -ne $U) {
    throw "本地 upstream/main 与远端不一致：remote=$RemoteU local=$U"
}

$H = (git rev-parse HEAD).Trim()
$OriginH = (git rev-parse origin/dev/pr-integration).Trim()
if ($OriginH -ne $H) {
    throw "当前分支与 origin/dev/pr-integration 不一致：H=$H remote=$OriginH"
}

$Bases = @(git merge-base --all $H $U)
if ($Bases.Count -ne 1) {
    throw "共同祖先不是唯一值：$($Bases -join ', ')"
}
$B = $Bases[0].Trim()

"H=$H"
"U=$U"
"B=$B"
```

### 8.1 强制 no-op 判断

```powershell
git merge-base --is-ancestor $U $H
if ($LASTEXITCODE -eq 0) {
    Write-Host "NO-OP: upstream/main $U 已包含在当前分支 $H 中。"
    Write-Host '不得创建 merge commit；本轮到此结束。'
    return
}
```

截至本文编制时会进入此分支并停止。

### 8.2 上游在冻结后移动怎么办

冻结后仍可在关键阶段只读检查：

```powershell
$NowRemoteU = ((git ls-remote upstream refs/heads/main) -split "`t")[0]
if ($NowRemoteU -ne $U) {
    throw "upstream/main 已从 $U 移动到 $NowRemoteU；本轮输入失效，重新评估"
}
```

不得在旧候选中把新的 `upstream/main` 再 merge 一次。应结束本轮，重新冻结新的 `H/U/B`。

---

## 9. 阶段 2：创建本轮备份

即使已有历史备份，每轮仍应按冻结的 `H` 新建备份。

```powershell
$Stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$Backup = "backup/pr-integration-before-upstream-$Stamp"

git branch $Backup $H
git show -s --format='%H %cI %s' $Backup
```

得到明确 push 授权后：

```powershell
git push --set-upstream origin "${Backup}:refs/heads/${Backup}"
$RemoteBackup = ((git ls-remote origin "refs/heads/$Backup") -split "`t")[0]
if ($RemoteBackup -ne $H) {
    throw "远程备份 SHA 不匹配：expected=$H actual=$RemoteBackup"
}
```

备份分支只用于恢复基线，不作为候选开发分支。

---

## 10. 阶段 3：合并前影响分析

合并前至少审查：

```text
B → U：上游在共同祖先后新增、删除、重构了什么
B → H：本地在共同祖先后新增、删除、修复了什么
```

执行：

```powershell
git log --reverse --format='%H`t%cI`t%s' "$B..$U"
git diff --stat $B $U
git diff --name-status --find-renames $B $U

git log --reverse --format='%H`t%cI`t%s' "$B..$H"
git diff --stat $B $H
git diff --name-status --find-renames $B $H
```

必须建立以下清单：

- 上游新增功能
- 上游删除或迁移的模块
- 上游 schema / provider ID / 配置键变化
- 上游路由、重试、错误类型变化
- 上游登录、凭证和回调变化
- 上游 UI / bridge / API 变化
- 上游版本、构建、Docker、Release、部署变化
- 与 `docs/local-contracts.json` 的实现路径或测试路径重叠
- 两边改动不同文件但属于同一调用链的隐性重叠

可使用 `merge-tree` 做只读预演；它不切换分支、不修改工作区，但可能在对象库写入临时 tree：

```powershell
git merge-tree --write-tree --name-only --messages --merge-base $B $H $U
```

该结果只用于估算冲突，不能替代真实候选和业务审查。

---

## 11. 阶段 4：创建隔离候选 worktree

### 11.1 路径和记录

```powershell
$ShortU = $U.Substring(0, 12)
$Parent = Split-Path $Root -Parent
$Candidate = Join-Path $Parent "agent2api-merge-$ShortU"
$RoundDir = Join-Path $Root "diagnostic-artifacts\upstream-main-$ShortU-$Stamp"
$PrepareRecord = Join-Path $RoundDir 'prepare.json'

if (Test-Path $Candidate) {
    throw "候选目录已存在：$Candidate；先确认是否为本轮续作，不能直接删除"
}
```

### 11.2 创建候选

上游不含本地治理文件，必须显式保留：

```powershell
python scripts/prepare-merge-candidate.py `
  --upstream $U `
  --worktree $Candidate `
  --record $PrepareRecord `
  --preserve-protected-control
```

受保护控制文件至少包括：

- `AGENTS.md`
- `docs/local-contracts.json`
- `scripts/check-merge-contracts.py`
- `scripts/local-merge-gates.py`
- `scripts/merge-flow.py`
- `scripts/replay-merge-baseline.py`

如果候选分支已存在：

- 先检查它是否绑定同一 `H/U/B`
- 有有效 `run.json` 时续作
- 不得自动删除旧 worktree、分支或诊断记录

---

## 12. 阶段 5：冻结候选并开始 merge

进入候选目录：

```powershell
Set-Location $Candidate

$CandidateRound = Join-Path $Candidate "diagnostic-artifacts\upstream-main-$ShortU-$Stamp"
$RunRecord = Join-Path $CandidateRound 'run.json'

python scripts/merge-flow.py --record $RunRecord freeze --upstream $U
python scripts/merge-flow.py --record $RunRecord preflight
python scripts/merge-flow.py --record $RunRecord status
```

确认记录中的 `H/U/B` 与阶段 1 完全一致后：

```powershell
git merge --no-commit --no-ff $U
$MergeExit = $LASTEXITCODE
if ($MergeExit -notin @(0, 1)) {
    throw "git merge 异常退出：$MergeExit"
}

$MergeHead = (git rev-parse MERGE_HEAD).Trim()
if ($MergeHead -ne $U) {
    throw "MERGE_HEAD 不等于冻结的 U：$MergeHead != $U"
}

git diff --name-only --diff-filter=U
```

有冲突是预期现象；不得因为冲突多就改用 ours/theirs 全局策略。

---

## 13. 阶段 6：语义解决规则

### 13.1 每个共同修改路径都要分类

只允许以下四种决策：

| 决策 | 含义 |
|---|---|
| `upstream_equivalent` | 上游已有等价能力，采用上游实现并保留等价证据 |
| `local_extension` | 本地是上游未覆盖的扩展，接入上游新结构 |
| `semantic_conflict` | 双方业务语义不同，已明确取舍并做专项验证 |
| `local_only` | 上游对应变化不是同一能力，保留本地实现 |

不能使用“Git 自动合并成功”“函数还在”“测试名字还在”作为分类理由。

### 13.2 必须沿完整调用链审查

提供商相关至少覆盖：

```text
注册
→ 添加账号
→ 登录 / 回调 / 手动凭证 / 导入
→ 存储 / 加密 / 刷新
→ 模型目录 / 映射
→ 路由 / 会话亲和 / 重试
→ 协议转换 / 转发
→ 用量 / 余额 / 奖励 / 签到
→ 定时任务
→ API / bridge / UI
```

### 13.3 上游迁移到插件架构时

如果 `U` 包含 provider 插件化：

1. 为每个被删除的原生模块记录其本地能力和契约 ID。
2. 找到对应插件、宿主 API、消息协议、配置 schema 和生命周期。
3. 判断本地逻辑应落在：
   - 插件实现
   - 宿主公共逻辑
   - 兼容适配层
   - 本地扩展插件
4. 保留 provider ID、账号身份和历史数据兼容。
5. 验证插件缺失、版本过旧、进程退出、取消调用和并发登录边界。
6. 只有完整链路可达且测试通过，才能删除旧原生模块。

禁止：

- 仅因上游删除就删除本地能力
- 仅把旧文件保留下来但不恢复注册和调用
- 同时保留新旧实现却没有唯一分派规则
- 让凭证在插件消息、日志或错误中泄露

### 13.4 数据库与迁移

对 schema 变化必须分别验证：

- 空数据库初始化
- 上游历史数据库升级
- 本地历史数据库升级
- 重复执行迁移
- 中断后重试
- 列、索引、默认值和唯一约束
- provider ID、账号身份和旧记录读取
- 用量精度及缺失/零值语义

已发布迁移不能重新赋义。双方用了相同版本号但结构不同，应增加前向兼容迁移，不得只改 `SCHEMA_VERSION`。

### 13.5 凭证、登录和安全

必须检查：

- access token / refresh token 的部分更新是否保留未返回字段
- 凭证过期与授权到期是否继续区分
- Docker/headless、桌面、远程回调是否都可用
- state、回调目标和一次性凭证是否绑定正确进程
- 面板管理员环境变量优先级是否保持
- ALTCHA、验证码等待者、取消和超时是否正确释放
- 公共 API、日志、错误和插件诊断是否泄露密钥

### 13.6 路由、重试和用量

必须检查：

- HTTP 状态与业务码分类
- 指定账号测试是否会换号
- 429 / 503 / `Retry-After` 是否正确传递
- 队列繁忙是否被误判为额度耗尽
- HTML 拦截页是否被误判为账号故障
- 会话亲和是否跨账号或跨地区污染
- 三协议最终 usage、缓存计数和原始报文是否一致
- 流式正文是否及时输出，不能为了等 usage 把整轮正文延迟到末尾

### 13.7 签到、奖励和后台任务

必须检查：

- 各提供商窗口、补签、重试和跨天重置
- 已完成不重复领取
- 部分成功不变成整批失败
- 保活成功不冒充奖励到账
- 未知结果不自动重复产生副作用
- Qoder 地区、设备名额和设备身份隔离
- ZCode 套餐领取、验证码和推理出口独立
- 后台状态持久化和重启恢复

### 13.8 UI 和生成产物

冲突只在源码层解决：

- `desktop-tauri/ui-islands/src/`
- `desktop-tauri/ui-kit/`
- i18n 源文件

然后重新运行：

```powershell
npm --prefix desktop-tauri/ui-kit run typecheck
npm --prefix desktop-tauri/ui-islands run typecheck
npm --prefix desktop-tauri/ui-islands run build
```

不得手工拼接：

- `desktop-tauri/ui/islands/ui.js`
- `desktop-tauri/ui/islands/ui.css`

生成产物必须由已解决的源码重新生成。

### 13.9 版本、Release 与部署

若上游改变版本：

- 不自动照抄上游版本
- 先确定本项目版本政策
- 同步检查 5 处版本号和 `Cargo.lock`
- 检查更新比较逻辑、beta 通道、Release 资产、Docker tag
- 保留华为云部署中根 `.env`、`--env-file`、管理员变量和 ZCode 开关口径

上游的容器自更新方案不能静默替代华为云固定镜像 tag 流程。

---

## 14. 阶段 7：契约清单维护

每个受影响契约必须得出最终状态：

- `必须保留`
- `上游等价实现`
- `已批准替代/移除`
- `缺失`
- `待验证`

其中：

- `缺失` 或 `待验证` 会阻止合入
- `已批准替代/移除` 必须有用户对具体行为变化的明确授权
- 技术重构不得伪装成业务移除

若实现路径或测试接线改变，使用 `evolution`：

```json
{
  "evolution": {
    "baseline_sha256": "<H 中该契约核心摘要>",
    "reason": "上游重构原因；业务输入输出保持不变",
    "review": "独立审阅记录与候选 tree",
    "implementation_moves": [
      {"from": "旧路径", "to": ["新路径"]}
    ],
    "test_moves": [
      {
        "from": "稳定测试 ID",
        "to": ["稳定测试 ID 或替代 ID"],
        "mode": "adapted",
        "reason": "仅接线变化，输入和期望不变"
      }
    ]
  }
}
```

原始测试先原样重放。只有 API 接线变化时，才能在隔离副本使用绑定原字节哈希的 replay patch；不能修改生产代码、样本输入或降低期望。

---

## 15. 阶段 8：暂存、impact、overlap 和 plan

### 15.1 暂存要求

先生成产物，再按明确范围暂存。不得用未经审阅的 `git add -A`。

```powershell
git diff --name-only --diff-filter=U
# 必须为空

git diff --check
git diff --cached --check

git status --short
git diff --cached --name-status
git diff --cached --stat
```

确认：

- 无未解决冲突
- 无未暂存的受控源码变化
- 新文件都已归类
- 诊断目录、数据库、凭据、日志和备份未进入暂存区

### 15.2 导出三组差异

```powershell
python scripts/merge-flow.py --record $RunRecord impact
```

这会记录：

```text
B → U
H → candidate
U → candidate
```

三组都必须审阅：

- `H → candidate`：本地行为是否丢失或改变
- `U → candidate`：上游新增能力是否被本地旧实现覆盖
- `B → U`：上游原始意图和结构变化

### 15.3 重叠分析

```powershell
python scripts/merge-flow.py --record $RunRecord overlap
```

查看 `overlap.json`，按领域逐批填写决策：

```powershell
python scripts/merge-flow.py --record $RunRecord overlap `
  --decision 'path/to/a=upstream_equivalent' `
  --decision 'path/to/b=local_extension'
```

重复执行会保留已有决定。直到：

```text
overlap.unresolved = []
```

### 15.4 生成计划

```powershell
python scripts/merge-flow.py --record $RunRecord plan
python scripts/merge-flow.py --record $RunRecord status
```

若候选 tree 后续变化，必须重新运行：

- `impact`
- `overlap`
- `plan`
- 受影响的所有门禁

---

## 16. 阶段 9：验收门禁

### 16.1 快速闭包门禁

长时间 Rust / 前端测试前先运行：

```powershell
python scripts/merge-flow.py --record $RunRecord run `
  --name integration-closure -- `
  python scripts/check-merge-closure.py `
    --base-ref $H `
    --report "$CandidateRound\integration-closure.json"
```

它至少检查：

- API 文件是否注册到 `api/mod.rs`
- schema 迁移编号是否闭合
- 桌面登录 provider 是否接线
- UI 源码变化是否带生成产物
- 版本号是否一致

失败时不能启动长门禁。

### 16.2 合并工具自检

```powershell
python scripts/merge-flow.py --record $RunRecord run `
  --name tools -- `
  python -B -m unittest discover -s scripts -p 'test_*merge*.py' -v
```

### 16.3 契约门禁

在阶段 7.2 的 group 编排修好后：

```powershell
python scripts/merge-flow.py --record $RunRecord run `
  --name contracts-p0 -- `
  python scripts/local-merge-gates.py `
    --base-ref $H `
    --group contracts `
    --report-dir "$CandidateRound\gates-contracts"
```

必须覆盖：

- 合并工具
- 闭包检查
- 版本一致性
- 契约清单
- H 基线 gate

### 16.4 Server 门禁

```powershell
python scripts/merge-flow.py --record $RunRecord run `
  --name server -- `
  python scripts/local-merge-gates.py `
    --base-ref $H `
    --group server `
    --report-dir "$CandidateRound\gates-server"
```

实际命令至少包括：

```text
cargo check --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --locked
cargo test  --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --locked
cargo test  --manifest-path desktop-tauri/src-tauri/Cargo.toml -p agent2api-server --locked -- --list
check-merge-contracts.py --rust-list ...
replay-merge-baseline.py rust --base-ref H ...
```

### 16.5 Frontend / Node / 浏览器门禁

```powershell
python scripts/merge-flow.py --record $RunRecord run `
  --name frontend -- `
  python scripts/local-merge-gates.py `
    --base-ref $H `
    --group frontend `
    --report-dir "$CandidateRound\gates-frontend"
```

实际命令至少包括：

- candidate Node TAP
- Node 契约映射
- `ui-kit` typecheck
- `ui-islands` typecheck
- `ui-islands` build
- H 前端基线重放
- candidate browser 测试

### 16.6 生成产物门禁

```powershell
python scripts/merge-flow.py --record $RunRecord run `
  --name generated-artifacts -- `
  git diff --exit-code -- `
    desktop-tauri/ui/islands/ui.js `
    desktop-tauri/ui/islands/ui.css
```

含义：在生成产物已暂存后再次构建，工作树不能产生新的未暂存差异。

### 16.7 影响审阅回执

独立审阅者完成 `impact-review.md` 后，通过已补充的检查入口记录：

```powershell
python scripts/merge-flow.py --record $RunRecord run `
  --name impact-review -- `
  python scripts/check-merge-impact-review.py `
    --record $RunRecord `
    --review "$CandidateRound\impact-review.md"
```

不得用空命令替代。

`impact-review.md` 必须包含且只能包含一个以下结构块（普通说明文字可写在结构块之外）：

```merge-impact-review
{
  "schema_version": 1,
  "candidate_tree": "<git write-tree>",
  "inputs": {
    "H": "<合并前本地提交>",
    "U": "<冻结的上游提交>",
    "B": "<唯一共同祖先>"
  },
  "plan_digest": "<plan.json digest>",
  "overlap_digest": "<overlap.json digest>",
  "reviewer": "<独立审阅者>",
  "reviewed_at": "<ISO-8601 时间>",
  "summary": "<审阅结论>",
  "paths": [
    {
      "path": "<overlap 路径>",
      "decision": "upstream_equivalent|local_extension|semantic_conflict|local_only",
      "rationale": "<语义取舍及完整调用链说明>",
      "evidence": ["<差异、测试或接口证据>"]
    }
  ],
  "findings": [
    {
      "severity": "P0|P1|P2",
      "status": "resolved|accepted|closed|open",
      "summary": "<finding>"
    }
  ]
}
```

`paths` 必须与 `overlap.json` 的路径集合完全一致，决策必须逐项一致；P0/P1 finding 未解决时 checker 必须失败。

### 16.8 影响域专项测试

通用门禁之外，必须根据 `plan.json` 增加专项测试。例如：

| 影响域 | 追加验证 |
|---|---|
| provider 插件化 | 插件缺失/过旧、登录绑定、调用取消、进程退出、凭证脱敏 |
| 数据库 | 上游与本地 fixture 双历史迁移、重复迁移、索引和默认值 |
| 路由 | 指定账号、会话亲和、冷却、429/503/`Retry-After` |
| WorkBuddy | 11-128、6020、HTML 拦截、日活和奖励独立性 |
| Qoder | 地区分派、设备名额、UMID、活动窗口和多账号 |
| ZCode | 验证码、套餐去重、出口、system 形状、三协议思考参数 |
| 登录/面板 | headless、桌面、远程回调、ALTCHA、管理员变量 |
| 部署 | compose `--env-file`、镜像 tag、数据卷和健康验证 |

真实外部调用需要单独授权，模拟测试通过不能冒充真实上游验证完成。

---

## 17. 阶段 10：独立审阅

独立审阅者不得参与主要冲突实现，至少审查：

1. `B → U`、`H → candidate`、`U → candidate`
2. 每个 overlap 路径的决策理由
3. 47/57 或本轮实际受影响契约的状态
4. 测试是否被删除、忽略、改名或削弱
5. provider 注册、分派和 UI 是否可达
6. 双来源迁移和历史数据
7. 凭证、日志、错误和插件消息泄露
8. GitHub Actions、Docker、Release 和华为云策略
9. `DEFAULT_REPO` 决策是否与授权一致
10. 候选 tree 与所有门禁回执是否一致

独立审阅有未解决的 P0/P1 finding 时不得 seal。

---

## 18. 阶段 11：seal、提交和提交树核验

先确认：

```powershell
python scripts/merge-flow.py --record $RunRecord status
```

然后按 `plan.json` 自动使用必需 gate：

```powershell
python scripts/merge-flow.py --record $RunRecord seal
python scripts/merge-flow.py --record $RunRecord check
```

得到提交授权后再提交：

```powershell
git status --short
git diff --cached --name-status
git diff --cached --check

git commit
$C = (git rev-parse HEAD).Trim()
```

提交应为普通 merge commit，保留两个父提交。

核验：

```powershell
git rev-list --parents -n 1 $C

git merge-base --is-ancestor $H $C
if ($LASTEXITCODE -ne 0) { throw '提交不包含 H' }

git merge-base --is-ancestor $U $C
if ($LASTEXITCODE -ne 0) { throw '提交不包含 U' }

python scripts/merge-flow.py --record $RunRecord check --commit $C
```

`check --commit` 失败时不得继续。

---

## 19. 阶段 12：把已验证候选快进到当前分支

返回原工作区：

```powershell
Set-Location $Root
```

重新确认原分支没有被其它任务移动：

```powershell
$CurrentH = (git rev-parse pr-integration).Trim()
if ($CurrentH -ne $H) {
    throw "pr-integration 已从 $H 移动到 $CurrentH；候选不能直接落位"
}

$Status = git status --porcelain=v1 --untracked-files=all
if ($Status) {
    $Status
    throw '原工作区已改变'
}
```

只允许快进：

```powershell
git merge --ff-only $C
```

再次确认：

```powershell
if ((git rev-parse HEAD).Trim() -ne $C) {
    throw '当前分支未移动到已验证提交'
}
```

不得在原工作区重新执行 merge 或重新解决冲突。

---

## 20. 阶段 13：push 与远端核验

push 需要明确授权。

先刷新并防止覆盖他人提交：

```powershell
git fetch origin
$RemoteBefore = (git rev-parse origin/dev/pr-integration).Trim()
if ($RemoteBefore -ne $H) {
    throw "远端已从冻结基线移动：expected=$H actual=$RemoteBefore"
}
```

先 dry-run：

```powershell
git push --dry-run origin "${C}:refs/heads/dev/pr-integration"
```

再正式推送：

```powershell
git push origin "${C}:refs/heads/dev/pr-integration"
```

核对完整 SHA：

```powershell
$RemoteAfter = ((git ls-remote origin refs/heads/dev/pr-integration) -split "`t")[0]
if ($RemoteAfter -ne $C) {
    throw "远端 SHA 不匹配：expected=$C actual=$RemoteAfter"
}
```

本轮不推 `origin/main`，不打 tag，不发布 Release，不部署华为云，除非另有明确授权。

---

## 21. 失败、停止与回滚

### 21.1 立即停止条件

遇到以下任一情况，停止合并：

- `U` 已是 `H` 祖先：本轮 no-op
- `upstream/main` 在冻结后移动
- 工作区、分支、暂存区或 `MERGE_HEAD` 与记录不一致
- 仓库是 shallow clone 或共同祖先不唯一
- 原分支或远端分支在候选期间移动
- 受保护治理文件被删除或未明确保留
- overlap 仍有未分类路径
- 必须保留的契约为 `缺失` 或 `待验证`
- 关键测试被删除、忽略、过滤后为零或断言削弱
- schema / provider ID / 配置键意义无法确认
- 版本号或生成产物不一致
- 候选 tree 在门禁后改变
- 真实业务变化没有用户授权
- 发现凭据、数据库、日志、备份或敏感数据进入暂存区
- push 目标不是 `origin/dev/pr-integration`
- `DEFAULT_REPO` 等产品决策被冲突静默改变

### 21.2 候选 merge 尚未提交

只在候选 worktree 中：

```powershell
git merge --abort
```

不得在原工作区执行 abort/reset。

保留：

- `prepare.json`
- `run.json`
- diff、overlap、plan 和测试报告
- 失败原因

### 21.3 候选已提交但尚未落到当前分支

保留候选分支和诊断证据，不移动 `pr-integration`。修复后重新生成 candidate tree、impact、plan 和门禁。

### 21.4 已快进本地但尚未 push

不得自动 `reset --hard`。先确认工作区干净并取得恢复授权，再把分支恢复到备份 `H`；恢复后重新核对 SHA。

### 21.5 已 push

禁止强推。使用新的 revert 提交：

```powershell
git revert -m 1 $C
```

revert 也必须经过受影响测试和 push 授权。

### 21.6 已部署

部署回滚属于独立流程：

- 恢复上一镜像 tag
- 使用根 `.env` 和 `--env-file`
- 不删除数据卷和未知备份
- 重新验证健康、认证、管理面板和真实业务路径

Git 回滚不能冒充生产部署已经回滚。

---

## 22. 必须保留的证据

每轮至少保留：

```text
prepare.json
run.json
upstream.diff              # B → U
from-local.diff            # H → candidate
from-upstream.diff         # U → candidate
overlap.json
plan.json
impact-review.md
integration-closure.json
contracts gate 报告
server gate 报告
frontend gate 报告
Rust test list
Node TAP
H baseline replay 报告
独立审阅报告
seal 记录
checked commit / tree
push 前后完整 SHA
```

这些证据放在被忽略的 `diagnostic-artifacts/` 或 `.verify/`，不得加入业务提交。

---

## 23. 本轮最终放行清单

### 输入与仓库

- [ ] `origin` 是 `wangzheuser/agent2api`
- [ ] `upstream` fetch 是 `aimod-cc/Agent2API`
- [ ] `upstream` push 已禁用
- [ ] 当前分支是 `pr-integration`
- [ ] tracking 是 `origin/dev/pr-integration`
- [ ] 工作区、暂存区、未跟踪文件为空
- [ ] 仓库历史完整，无既有 `MERGE_HEAD`
- [ ] `H/U/B` 已冻结且共同祖先唯一
- [ ] `U` 不是 `H` 的祖先；否则已按 no-op 结束

### 候选

- [ ] 候选从 `H` 创建
- [ ] merge 使用精确 `U`
- [ ] 原工作区未进入 merge 状态
- [ ] 所有冲突已按语义解决
- [ ] 所有 overlap 路径已分类
- [ ] 受保护治理文件仍完整
- [ ] 新上游能力没有被本地旧实现覆盖
- [ ] 本地能力没有因上游重构静默丢失

### 契约与验证

- [ ] `docs/local-contracts.json` 未被削弱
- [ ] P0 全部通过
- [ ] 受影响 P1 全部通过
- [ ] 双历史迁移通过
- [ ] Rust / Node / 浏览器门禁通过
- [ ] 前端生成产物匹配源码
- [ ] 专项测试通过
- [ ] 独立审阅无阻断 finding
- [ ] 所有回执绑定同一 candidate tree

### 提交与交付

- [ ] `seal` 成功
- [ ] `check` 成功
- [ ] merge commit 同时包含 `H` 和 `U`
- [ ] `check --commit` 成功
- [ ] 原分支仅通过 `--ff-only` 移动到已验证提交
- [ ] push 前远端仍是冻结的 `H`
- [ ] push 目标是 `origin/dev/pr-integration`
- [ ] push 后完整 SHA 等于候选提交
- [ ] 未自动发版、打 tag 或部署

---

## 附录 A：当前 57 项本地契约

### A.1 协议、用量与原始报文

- `public-model-name-matches-id`
- `anthropic-stream-usage`
- `usage-zero-cache-semantics`
- `responses-stream-usage`
- `stream-bounds`
- `raw-capture-once`
- `upstream-credit-telemetry`
- `usage-identity-and-account-scheduling`
- `catpaw-partial-usage`

### A.2 路由、错误、池耗尽与会话亲和

- `routing-strategies`
- `routing-eligibility`
- `http200-business-errors`
- `direct-switch-defaults`
- `model-reasoning-force-override`
- `model-test-disabled-bindings`
- `session-affinity-allocation`
- `session-affinity-completion`
- `session-affinity-identity`
- `session-affinity-settings`
- `pool-exhausted-short-retry`

### A.3 WorkBuddy、成长、积分与后台任务

- `workbuddy-risk-terminal-safety`
- `workbuddy-background-risk-idempotency`
- `workbuddy-intl-reward-ledger`
- `workbuddy-intl-acp`
- `growth-capability-credit`
- `growth-replay-identity`
- `credit-precision-expiry`
- `credit-overview-regions`
- `credit-cache-identity`
- `workbuddy-credit-policy`
- `workbuddy-split-migration`

### A.4 原生 provider、登录、签到与奖励

- `native-provider-registry`
- `minimax-login-refresh`
- `minimax-checkin-credit`
- `lobster-login-refresh`
- `lobster-checkin-credit`
- `custom-reward-isolation`
- `checkin-retry-window`
- `checkin-center-provider-boundaries`
- `kuku-provider-integration`
- `officeace-control-token-expiry`
- `cline-reset-formats`

### A.5 Qoder 与 ZCode

- `qoder-regional-campaign`
- `qoder-intl-device-quota`
- `zcode-claim-egress`
- `zcode-claim-dedup`
- `zcode-start-plan-system-shape`
- `zcode-reasoning-binding`

### A.6 登录、面板、安全与管理闭包

- `captcha-lifecycle`
- `panel-session-rotation`
- `remote-callback-bridge`
- `restored-management-chain`
- `desktop-login-provider-wiring`
- `database-backup-management-routes`

### A.7 数据库与前端产物

- `schema-dual-history`
- `schema-upstream-usage-v11`
- `ui-real-artifacts`

---

## 附录 B：当前冻结事实

```text
upstream/main:
  68d0a4e4bfd8dc5a1daa296c61e0cc0b10a871da
  2026-10-17T01:18:32+08:00
  v3.0.8 主线提交

pr-integration:
  cc21ce55366ab528277bb711d143b223a28ec8b6
  2026-10-28T22:08:28+08:00
  修复华为云部署漏读根 .env

merge-base:
  68d0a4e4bfd8dc5a1daa296c61e0cc0b10a871da

关系:
  upstream/main ancestor-of pr-integration = true
  origin/dev/pr-integration == pr-integration

当前结论:
  no-op；不得 merge
```

---

## 附录 C：后续执行时的复用提示词

```text
严格按 UPSTREAM_MAIN_MERGE_RUNBOOK.md 执行已授权的 upstream/main → pr-integration 合入。
先重新 fetch 并冻结精确 H/U/B；若 U 已是 H 祖先，按 no-op 结束，绝不改用 beta。
保持原工作区不进入 merge 状态，从 H 创建隔离候选并保留治理文件。
审查 B→U、B→H、H→候选、U→候选，不只处理 Git 冲突。
逐路径做 overlap 分类，逐契约证明本地能力保留和上游新增能力落位。
若上游 provider 插件化，按完整链路迁移能力，不保留不可达死代码，也不接受静默删除。
运行闭包、契约、Server、Frontend、浏览器、双历史迁移和生成产物门禁。
门禁编排缺口未修复时禁止开始业务 merge；不得用空命令伪造回执。
候选 tree 变化后重跑 impact、plan 和受影响验收。
经独立审阅、seal 和 check --commit 后，只用 ff-only 落到 pr-integration。
按单独授权 push 到 origin/dev/pr-integration，核对完整 SHA；不自动发版或部署。
```
