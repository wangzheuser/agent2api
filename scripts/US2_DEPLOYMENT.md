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

ZCode 领取与推理可使用不同出口：`ZCODE_CLAIM_DIRECT=1`（国内版）或
`ZCODE_INTL_CLAIM_DIRECT=1`（国际版）将对应地区的领取探测、验证码配置、服务端
领取验证码及领取提交统一为直连，推理与余额继续使用账号代理；默认 `0` 沿用账号
出口。应先用同账号、同设备和同版本做领取对照，并查余额确认实际到账后启用。
us2 已于 2026-10-05 验证代理领取返回 `405/3012`、直连领取返回 `200/code:0`，
两个国际版账号当天套餐均到账，因此专用 Compose 启用 `ZCODE_INTL_CLAIM_DIRECT=1`。

活动套餐推理另由 `ZCODE_START_PLAN_DIRECT=1` / `ZCODE_INTL_START_PLAN_DIRECT=1`
控制，默认 `0`；它同时影响该通道的推理发送和服务端验证码生产者，编码套餐与余额
保持账号出口。us2 对同一国际版账号实测：完整官方 system 下代理仍回 `405/3012`，
直连返回 `OK`、正常终止且带 usage，因此同时启用 `ZCODE_INTL_START_PLAN_DIRECT=1`。
