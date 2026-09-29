# Docker ZCode 验证码生产者

Docker 镜像启动后，由 Rust 进程管理 Node 子进程。子进程在 Xvfb 中运行普通 Chromium，通过仅监听本机的 CDP 调用官方 SDK。每次生成使用独立浏览器上下文，不依赖管理员网页，也不与登录、领套餐验证码共用 SDK 全局状态。账号 JWT、管理员密码和网关 Key 不传给子进程；账号设置中的代理仅通过管道传递。

- 有启用的活动套餐账号时维持 3 枚库存；60 秒开始换新，95 秒过期。
- 空池请求最多等待 60 秒，取消立即释放等待者；并发请求各消费独立令牌。
- 生成失败重建浏览器与 SDK，子进程失败由 Rust 重启，失败退避 2～15 秒。
- 重发请求重新取令牌；不会复用上一次发送过的 proof。
- `/api/zcode/captcha` 的 `producer`、`waiting`、`timedOut`、`fresh` 提供状态；日志仅记录计数及固定错误码。

`AGENT2API_ZCODE_CAPTCHA_WORKER` 指向 `worker.cjs`，Docker 默认设置。非 Docker headless 需安装 Node 18+、Chromium、Xvfb、`/bin/kill` 和本目录锁定依赖，再设置此变量。未设置时保持桌面网页生产方式。`AGENT2API_CHROMIUM_PATH` 可覆盖浏览器位置。内部 HTTP/CDP/X11 不新增 Docker 端口映射。

上游 SDK 持续拒绝（如 `sdk_rejected_F001`）会如实显示失败，客户端最终收到限时等待后的 503；后台恢复不是对上游可用性的保证。

验证：`npm run test:captcha`、在 `desktop-tauri/src-tauri` 运行 `cargo test -p agent2api-server --locked`，再实测三种协议、SSE 结束、跨令牌寿命、并发超过库存、浏览器退出恢复。`/health` 正常不代表上游推理成功。

发布通过项目原有入口，保留上一提交、固定镜像和 SQLite 一致性备份。程序回滚只恢复原镜像与提交，不覆盖运行数据库。
