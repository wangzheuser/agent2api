const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const guardSource = fs.readFileSync(path.resolve(__dirname, '../desktop-tauri/ui/zcode-captcha-pool.js'), 'utf8');
const config = { region: 'fixture', prefix: 'fixture', sceneId: 'fixture' };

function harness() {
  let now = 100000, timerId = 0;
  const timers = new Map();
  const window = {
    setTimeout(fn, delay) { const id = ++timerId; timers.set(id, { fn, at: now + delay }); return id; },
    clearTimeout(id) { timers.delete(id); },
  };
  const document = { readyState: 'complete' };
  const captcha = { isBusy: () => false };
  window.wbAliyunCaptcha = captcha;
  const context = vm.createContext({ window, document, Date: { now: () => now }, console });
  async function step() {
    for (let i = 0; i < 30; i++) await Promise.resolve();
    const next = [...timers].sort((a, b) => a[1].at - b[1].at)[0];
    assert.ok(next, '应存在待执行的定时器');
    timers.delete(next[0]); now = next[1].at; next[1].fn();
    for (let i = 0; i < 30; i++) await Promise.resolve();
  }
  return { window, context, captcha, step };
}

// 执行真正注入 headless 面板的脚本，而非手写桥接 mock，防止合并时再次漏掉方法。
function installWebShim(h, route) {
  const file = process.env.WEB_SHIM_FILE || path.resolve(__dirname, '../desktop-tauri/src-tauri/server/src/web_shim.rs');
  const shim = fs.readFileSync(file, 'utf8').match(/pub fn shim_js\(\)[\s\S]*?r#"([\s\S]*?)"#/)[1];
  Object.assign(h.context, {
    localStorage: { getItem: key => key === 'agent2api.webKey' ? 'fixture-key' : null },
    setTimeout: h.window.setTimeout,
    URLSearchParams,
    fetch: async (url, init) => {
      const { status = 200, body } = await route(url, init);
      return { status, text: async () => JSON.stringify(body) };
    },
  });
  vm.runInContext(shim, h.context);
  return h.window.workbuddyDesktop;
}

test('headless 验证码桥接读取库存、回传令牌并保留错误语义', async () => {
  const h = harness(), calls = [];
  const stats = { ready: 2, target: 3, producer: { mode: 'server', failures: 0 } };
  let failed = false;
  const bridge = installWebShim(h, (url, init) => {
    calls.push({ url, ...init });
    return failed ? { status: 409, body: { success: false, error: 'fixture producer owns pool' } }
      : { body: { success: true, data: stats } };
  });
  assert.deepEqual(JSON.parse(JSON.stringify(await bridge.zcodeCaptchaStats())), stats);
  const tokens = [{ param: 'fixture-only', region: 'cn' }];
  assert.deepEqual(JSON.parse(JSON.stringify(await bridge.pushZcodeCaptchaTokens(tokens))), stats);
  await bridge.pushZcodeCaptchaTokens(null);
  assert.deepEqual(calls.map(call => [call.url, call.method, call.body]), [
    ['/api/zcode/captcha', 'GET', undefined],
    ['/api/zcode/captcha', 'POST', JSON.stringify({ tokens })],
    ['/api/zcode/captcha', 'POST', '{"tokens":[]}'],
  ]);
  assert.ok(calls.every(call => call.headers['x-api-key'] === 'fixture-key'));
  failed = true;
  await assert.rejects(bridge.zcodeCaptchaStats(), /fixture producer owns pool/);
  await assert.rejects(bridge.pushZcodeCaptchaTokens(tokens), /fixture producer owns pool/);
});

for (const region of ['cn', 'intl']) {
  for (const mode of ['browser', 'server']) {
    test(`headless ${region} 令牌守卫通过真实桥接轮询，${mode} 模式遵循补货边界`, async () => {
      const h = harness();
      let ready = 0, reads = 0, minted = 0;
      h.captcha.mintTraceless = async () => { minted++; return 'fixture-only'; };
      installWebShim(h, (url, init) => {
        if (url.endsWith('/zcode-claim/captcha-config')) {
          return { body: { success: true, data: { ...config, region, enabled: true } } };
        }
        if (url === '/api/session') return { body: { success: true, data: {} } };
        assert.equal(url, '/api/zcode/captcha');
        if (init.method === 'POST') {
          assert.equal(mode, 'browser', '服务器接管时网页不得回传令牌');
          const { tokens } = JSON.parse(init.body);
          assert.deepEqual(tokens, [{ param: 'fixture-only', region }]);
          ready += tokens.length;
        } else reads++;
        return { body: { success: true, data: {
          ready, target: 3, needsTokens: ready < 3, captchaAccountId: `fixture-${region}`, producer: { mode },
        } } };
      });
      vm.runInContext(guardSource, h.context);
      for (let i = 0; i < 14 && ready < 3; i++) await h.step();
      h.window.wbZcodeCaptchaPool.stop();
      assert.ok(reads > 0, '守卫必须实际读取库存，不能因桥接缺失静默跳过');
      assert.equal(ready, mode === 'browser' ? 3 : 0);
      assert.equal(minted, mode === 'browser' ? 3 : 0);
    });
  }
}

test('合并前的成长、任务和账号选路桥接仍发送正确的管理请求', async () => {
  const h = harness(), calls = [];
  const bridge = installWebShim(h, (url, init) => {
    calls.push([url, init.method, init.body]);
    return { body: { success: true, data: { fixture: true } } };
  });
  const id = 'fixture a/&';
  const patch = { autoGrowth: false, creditFloor: 10 };
  const action = { id, action: 'claim_available', expectedIdentity: 'fixture-identity' };
  await bridge.getAutoClawTasks(id);
  await bridge.getWorkBuddyGrowth(id);
  await bridge.workBuddyGrowthAction(action);
  await bridge.getWorkBuddyPolicy(id);
  await bridge.updateWorkBuddyPolicy(id, patch);
  await bridge.getAccountSelection();
  await bridge.saveAccountSelection({ accountSelection: 'priority' });
  assert.deepEqual(calls, [
    [`/api/accounts/autoclaw/tasks?id=${encodeURIComponent(id)}`, 'GET', undefined],
    [`/api/accounts/workbuddy/growth?id=${encodeURIComponent(id)}`, 'GET', undefined],
    ['/api/accounts/workbuddy/growth/action', 'POST', JSON.stringify(action)],
    [`/api/accounts/workbuddy/policy?id=${encodeURIComponent(id)}`, 'GET', undefined],
    [`/api/accounts/workbuddy/policy?id=${encodeURIComponent(id)}`, 'PATCH', JSON.stringify(patch)],
    ['/api/account-selection', 'GET', undefined],
    ['/api/account-selection', 'PUT', '{"accountSelection":"priority"}'],
  ]);
});
