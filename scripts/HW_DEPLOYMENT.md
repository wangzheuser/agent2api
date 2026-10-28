# 华为云日常部署

华为云（ssh-manager 服务器名 `huaweiyun`，1.95.124.238，国内节点）的部署形态：
**服务器不拉代码、不在服务器内构建**。镜像在本地/构建机构建后 `docker save/load`
传输上去，再由 `.deploy/docker-compose.huawei.yml` 固定 tag 替换。服务器只有
2 核 / 1.7GB 内存，禁止在服务器内做 Rust 构建或测试。

关键路径：

- 项目目录：`/opt/docker_projects/agent2api`（不是 git 仓库）
- 生效 compose：`.deploy/docker-compose.huawei.yml`（端口 `0.0.0.0:19050→3065`，
  `TZ=Asia/Shanghai`，数据卷 `./data`）
- 版本化备份：`backups/agent2api-pre-<tag>-<UTC时间戳>/`

## 发布流程

1. 本地提交并推送：`git push origin HEAD:dev/pr-integration`（代码来源始终是远程分支，
   服务器上没有源码，不要在服务器改文件）。
2. 本地构建镜像，tag 格式 `agent2api:<版本>-<提交前8位>`：

   ```bash
   cd desktop-tauri/src-tauri   # Dockerfile 在仓库根，见 docker-compose.yml 的 build: .
   docker build -t agent2api:3.0.9-43accea5 .
   docker save agent2api:3.0.9-43accea5 | gzip > dist/agent2api-3.0.9-43accea5.tar.gz
   ```

3. 传输到服务器并加载：

   ```bash
   scp dist/agent2api-3.0.9-43accea5.tar.gz root@1.95.124.238:/tmp/
   ssh root@1.95.124.238 'gunzip -c /tmp/agent2api-3.0.9-43accea5.tar.gz | docker load'
   ```

4. 服务器上一条命令完成「备份 → 改 compose image → up -d → 验收」：

   ```bash
   cd /opt/docker_projects/agent2api
   scripts/deploy-hw.sh switch 3.0.9-43accea5
   ```

   （脚本从仓库同步到服务器 `scripts/` 下；或直接
   `bash <(cat scripts/deploy-hw.sh) switch <tag>`。）

5. 验收：`switch` 已内含基础验收；真实模型请求、余额、签到和活动领取需要用授权
   账号在面板单独做语义验证。

## 回滚

```bash
cd /opt/docker_projects/agent2api
scripts/deploy-hw.sh rollback <上一个tag>
```

数据库异常时用 `backups/` 里最近的 `data.tar.gz` 恢复（先停容器）。

## ZCode 直连开关（重要，华为云口径与 us2 不同）

`ZCODE_INTL_CLAIM_DIRECT` / `ZCODE_INTL_START_PLAN_DIRECT` 是 **us2（海外节点）
实测代理领取 405/3012、直连 200 后启用的结论**。华为云是国内节点，直连
`api.workbuddy.pro` 不通，当前两开关保持 `0`（沿用账号代理出口）。改动开关前必须
用授权账号做真实领取/推理对照并确认到账，不得照搬 us2 结论。

## 与旧 us2 流程的差异（历史）

us2 时代的 `update_version.sh`、`release.env`、`.deploy/status/`、服务器内构建与
`scripts/deploy-us2.sh` / `verify-us2.sh` 已随迁移废弃删除；华为云不再使用。
