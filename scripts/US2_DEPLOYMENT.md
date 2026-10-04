# us2 日常部署

us2 的代码来源固定为 `origin/dev/pr-integration`，服务器项目目录为
`/opt/docker_projects/agent2api`。服务器上的 `update_version.sh` 仍是唯一发布入口，
包装器只负责提交后台任务、记录状态和轮询结果。

```bash
cd /opt/docker_projects/agent2api
scripts/deploy-us2.sh start dev/pr-integration
scripts/deploy-us2.sh wait
```

`start` 会返回 `operation_id`、状态文件和日志路径。SSH 断开或客户端超时后，使用
同一个 ID 查询，不要重复启动：

```bash
scripts/deploy-us2.sh status <operation-id>
scripts/deploy-us2.sh wait <operation-id> 1200
```

部署完成后，先做基础验收，再做真实业务语义验证：

```bash
scripts/deploy-us2.sh verify <预期提交>
```

基础验收覆盖服务器提交、Compose 配置、容器健康状态、`/health`、`/v1/models`、管理
面板和近期 `fatal`/`panic` 日志；余额、签到、活动领取和模型请求需要使用授权账号单独验证。
