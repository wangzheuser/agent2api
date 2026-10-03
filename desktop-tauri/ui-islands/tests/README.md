# WorkBuddy 积分明细回归

在 `desktop-tauri/ui-islands` 目录运行：

```sh
node --test tests/accounts-credit-cache.test.mjs tests/accounts-credit-details.test.cjs
npm run build
node tests/accounts-credit-dialog.browser.cjs
```

浏览器检查需要环境中已有 Playwright 和 Microsoft Edge；Playwright 位于独立工具目录时可通过 `NODE_PATH` 提供。它加载真实构建产物，使用本地构造账号与余额，不连接服务端。截图及结果写入仓库根目录的 `.verify/workbuddy-credit-details/`。

缓存测试覆盖在途去重、60 秒新鲜度、服务端时钟偏差、快照顺序、失败旧值及账号身份变更。浏览器检查覆盖双击、键盘、触屏、焦点、分段定位、刷新失败、国内／国际版和 320px 布局。
