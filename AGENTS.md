# AGENTS.md — 开发、us2 部署与正式发版流程

> 本文档面向维护者与 AI 代理：Agent2API（workbuddy）桌面端、Docker 镜像和 us2 部署流程。
> 日常 us2 部署与正式 GitHub/Docker Hub 发版是两条不同流程，必须按对应章节执行。

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

## 3. 日常 us2 部署

us2 的日常部署以远程 `dev/pr-integration` 分支为唯一代码来源。项目本地分支名为
`pr-integration`，不要把本地分支名误写成远程分支名。

### 3.1 本地提交与推送

1. 在本地完成代码修改、必要测试和审查。
2. 将变更提交到本地 `pr-integration`；其他分支的变更先合并到该分支。
3. 确认工作区没有未提交的业务改动后，推送到 us2 使用的远程分支：

   ```bash
   git status --short
   git push origin HEAD:dev/pr-integration
   ```

   推送目标必须是 `origin/dev/pr-integration`。不得通过直接修改服务器源码来绕过 Git 推送流程。

### 3.2 us2 服务器更新

登录 us2 后，只在项目目录执行项目提供的更新入口：

```bash
cd /opt/docker_projects/agent2api
./update_version.sh
```

`update_version.sh` 是服务器上的部署入口，不要求它存在于本地源码仓库。它负责调用项目的构建、替换和启动流程；不得在服务器上手工执行不受记录的容器替换、数据库卷删除或源码修改。

更新过程可能包含候选镜像构建和容器内 Rust 测试，持续数分钟，并可能在 SSH 通道已经返回部分输出时仍在服务器后台继续执行。必须使用持久 SSH 会话、远程日志或进程轮询等待更新入口的实际进程结束；SSH 工具的退出码、部分构建输出或「命令已启动」不能视为部署完成。发现已有 `update_version.sh` / `.deploy/update-agent2api.sh` / BuildKit / `cargo test` 进程时，不得并发再次执行更新。

服务器项目目录可能保留项目维护的未跟踪运行材料（例如 `backups/`、`update_version.sh`）；只要更新脚本的受控检查允许，不得为了清理工作区删除或覆盖这些材料。部署脚本会先创建版本化数据库与配置备份，再构建候选镜像；如果构建或候选测试失败而 `release.env`、运行容器仍指向旧版本，必须保留备份和失败现场，按失败流程处理。

### 3.3 部署验收

更新脚本成功退出后，必须确认：

- 服务器工作区已更新到预期的 `dev/pr-integration` 提交；
- `.deploy/release.env` 中的提交、版本和镜像标签与目标提交一致；目标镜像确实存在；
- `docker inspect agent2api` 的镜像标签已切换到目标镜像，容器 `StartedAt` 已更新且 `RestartCount` 从本批次基线重新核对；
- Compose 配置可渲染，目标容器处于运行/健康状态；
- `/health`、`/v1/models`、管理面板和认证边界符合预期；
- 至少完成一次授权范围内的真实 API 验证，并检查服务日志无新增 fatal 错误；
- 原有数据卷、持久化配置和共享入口未被替换。

健康检查、页面 200 或认证成功只能证明对应层级，不得单独宣称真实模型请求成功。
容器仍使用旧镜像、`release.env` 仍记录旧提交、目标镜像不存在，或更新进程尚未结束时，均判定为「尚未完成」，不能按脚本退出码结案。

### 3.4 失败与回滚

部署失败时保留现场，先确认没有残留更新进程，再记录失败阶段（备份、候选构建、候选测试、正式镜像或容器替换）。优先使用服务器项目提供的版本化回滚入口恢复程序和配置；不得执行 `docker compose down -v`、全局 prune 或删除未知备份。回滚后重新验证容器、健康端点、认证边界和既有业务路径。

### 3.5 分支口径维护

当前部署入口的正常更新目标是 `origin/dev/pr-integration`。如果服务器脚本改为使用其他远程分支，必须先同步更新本节、服务器更新脚本及验证命令，不能只修改其中一处。

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
