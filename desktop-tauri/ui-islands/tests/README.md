# WorkBuddy 积分明细回归

在 `desktop-tauri/ui-islands` 目录运行：

```sh
node --test tests/*.test.*
npm --prefix ../ui-kit run typecheck
npm run typecheck
npm run build
node tests/accounts-credit-dialog.browser.cjs
node tests/accounts-credit-overview.browser.cjs
node tests/accounts-growth.browser.cjs
```

首次准备依赖时在两个前端目录执行 `npm ci`。Playwright 已作为锁定的测试依赖：Windows 使用已有 Microsoft Edge；Linux/macOS 先执行 `npx --no-install playwright install chromium`（Linux CI 使用 `--with-deps`），再运行同一套浏览器测试。测试加载真实构建产物，使用本地构造账号与余额，不连接服务端。截图及结果写入仓库根目录被忽略的 `.verify/`，CI 将其上传为验收工件。

仓库根目录还需运行 `node --test --test-reporter=tap scripts/*.test.cjs desktop-tauri/ui-islands/tests/*.test.*`，覆盖验证码生命周期、两地区补货/桥接、成长/任务/选路管理桥接、远程回调和面板令牌轮转，以及全部前端 Node 业务测试。唯一能力清单位于 `docs/local-contracts.json`，CI 入口为 `.github/workflows/verify.yml`；成功构建不替代这些业务回归。

缓存测试覆盖在途去重、60 秒新鲜度、服务端时钟偏差、快照顺序、失败旧值及账号身份变更。浏览器检查覆盖双击、键盘、触屏、焦点、分段定位、刷新失败、国内／国际版和 320px 布局。

合入验收同时执行根目录的检查器自检与 `check-merge-contracts.py --base-ref <H>`；Rust 使用所有 target 的 `-- --list` 报告，Node 使用上述命令的完整 TAP。CI 在隔离候选副本中按原路径重放 H 的独立 Rust、Node、浏览器断言，并用当前检查器执行 H 原始门禁自测，使用独立 H 报告避免同名候选用例掩盖原始缺测。首次 H 无清单时明确 BOOTSTRAP；内联 Rust 断言仍需对照差异审查。

未来新增用例由清单稳定测试 ID 映射，Node/浏览器通过 `replay-merge-baseline.py candidate-node|candidate-browser` 动态执行；模块/API 演进、独立原始断言接线适配及精确候选 tree 封存按 AGENTS.md 第 8.9–8.11 节执行。
