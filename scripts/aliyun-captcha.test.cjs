const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const ui = path.resolve(__dirname, '../desktop-tauri/ui');
const source = fs.readFileSync(path.join(ui, 'aliyun-captcha.js'), 'utf8');
const guardSource = fs.readFileSync(path.join(ui, 'zcode-captcha-pool.js'), 'utf8');
const config = { region: 'fixture', prefix: 'fixture', sceneId: 'fixture' };

function harness() {
  let now = 100000;
  let timerId = 0;
  let sdk;
  const timers = new Map();
  const elements = new Map();
  const window = {
    setTimeout(fn, delay) {
      const id = ++timerId;
      timers.set(id, { fn, at: now + delay });
      return id;
    },
    clearTimeout(id) { timers.delete(id); },
    initAliyunCaptcha(options) {
      sdk = options;
      options.getInstance({ destroy() {} });
    },
  };
  const document = {
    readyState: 'complete',
    documentElement: { lang: 'zh-CN' },
    getElementById(id) { return elements.get(id) || null; },
    createElement() {
      return { style: {}, setAttribute() {}, click() {}, remove() { elements.delete(this.id); } };
    },
    body: { appendChild(element) { elements.set(element.id, element); } },
  };
  const context = vm.createContext({ window, document, Date: { now: () => now }, atob: value => Buffer.from(value, 'base64').toString('utf8'), console });
  vm.runInContext(source, context);
  const captcha = window.wbAliyunCaptcha;
  // 排空 SDK 初始化与业务回调中的 Promise 链；时钟只由 step 推进。
  async function flush() {
    for (let i = 0; i < 30; i++) await Promise.resolve();
  }
  async function step() {
    await flush();
    const next = [...timers].sort((a, b) => a[1].at - b[1].at)[0];
    assert.ok(next, '应存在待执行的定时器');
    timers.delete(next[0]);
    now = next[1].at;
    next[1].fn();
    await flush();
  }
  async function begin(request = async () => ({ captchaResult: true, bizResult: true })) {
    const outcome = captcha.solve(config, request).then(
      value => ({ value }), error => ({ error }),
    );
    await flush();
    if (!captcha.isBusy()) await step(); // 首次初始化需要预热。
    assert.equal(captcha.isBusy(), true);
    return { outcome };
  }
  return { window, context, captcha, step, begin, timers, flush, sdk: () => sdk };
}

function mintHarness() {
  const h = harness();
  const instances = [];
  h.window.initAliyunCaptcha = options => {
    const instance = { destroyed: false, destroy() { this.destroyed = true; }, startTracelessVerification() {} };
    instances.push({ options, instance });
    options.getInstance(instance);
  };
  const mint = () => {
    const result = { state: 'pending' };
    result.promise = h.captcha.mintTraceless(config).then(
      value => { result.state = 'resolved'; result.value = value; },
      error => { result.state = 'rejected'; result.error = error; },
    );
    return result;
  };
  return { ...h, instances, mint };
}

test('静默验证无效串立即拒绝并允许下一次生成', async () => {
  const h = mintHarness();
  const old = h.mint(); await h.flush();
  h.instances[0].options.success('invalid'); await h.flush();
  assert.equal(old.state, 'rejected');
  assert.equal(h.timers.size, 0);
  const next = h.mint(); await h.flush();
  const proof = Buffer.from(JSON.stringify({ securityToken: 'x'.repeat(220) })).toString('base64');
  h.instances[1].options.success(proof); await next.promise;
  assert.equal(next.value, proof);
});

test('旧实例 success/fail/onError 不改变新生成任务', async () => {
  const h = mintHarness();
  const old = h.mint(); await h.flush(); await h.step();
  assert.equal(old.state, 'rejected');
  const next = h.mint(); await h.flush();
  h.instances[0].options.success('invalid');
  h.instances[0].options.fail({}); h.instances[0].options.onError({});
  await h.flush();
  assert.equal(next.state, 'pending');
  assert.equal(h.instances[1].instance.destroyed, false);
  await h.step(); assert.equal(next.state, 'rejected');
});

test('静默生成并发调用不会覆盖第一个等待者', async () => {
  const h = mintHarness();
  const first = h.mint(), second = h.mint(); await h.flush();
  assert.equal(first.state, 'pending'); assert.equal(second.state, 'rejected');
  await h.step(); assert.equal(first.state, 'rejected');
});

test('初始化错误立即结束，晚到的实例被销毁且不会污染重试', async () => {
  const h = mintHarness(); let options;
  h.window.initAliyunCaptcha = value => { options = value; value.onError({}); };
  const first = h.mint(); await h.flush();
  assert.equal(first.state, 'rejected'); assert.equal(h.timers.size, 0);
  let destroyed = false;
  options.getInstance({ destroy() { destroyed = true; } });
  assert.equal(destroyed, true);
});

for (const mode of ['success', 'business_false', 'request_reject', 'cancel', 'timeout', 'sdk_error', 'empty_param']) {
  test(`${mode} 结束后释放忙碌状态并恢复 ZCode 补货`, async () => {
    const h = harness();
    const { outcome } = await h.begin(async () => {
      if (mode === 'request_reject') throw new Error('fixture business error');
      return { captchaResult: true, bizResult: mode !== 'business_false' };
    });
    if (mode === 'cancel') assert.equal(h.captcha.cancel(), true);
    else if (mode === 'timeout') await h.step();
    else if (mode === 'sdk_error') h.sdk().onError(new Error('fixture SDK error'));
    else await h.sdk().captchaVerifyCallback(mode === 'empty_param' ? '' : 'fixture');
    const result = await outcome;
    assert.equal(Boolean(result.error), !['success', 'business_false'].includes(mode));
    if (!result.error) assert.equal(result.value.bizResult, mode !== 'business_false');
    assert.equal(h.captcha.isBusy(), false);
    assert.equal(h.captcha.cancel(), false, '结束后重复取消应无副作用');

    let ready = 0;
    let minted = 0;
    h.captcha.mintTraceless = async () => { minted++; return 'fixture-only'; };
    h.window.workbuddyDesktop = {
      zcodeCaptchaStats: async () => ({ ready, target: 3, needsTokens: ready < 3, captchaAccountId: 'fixture' }),
      zcodeClaimCaptchaConfig: async () => ({ ...config, enabled: true }),
      pushZcodeCaptchaTokens: async tokens => { ready += tokens.length; },
    };
    vm.runInContext(guardSource, h.context);
    for (let i = 0; i < 9 && ready < 3; i++) await h.step();
    assert.equal(ready, 3);
    assert.equal(minted, 3);
    h.window.wbZcodeCaptchaPool.stop();
  });
}

test('旧业务回调晚到时保留新验证码流程的忙碌状态', async () => {
  const h = harness();
  let finishOld;
  const old = await h.begin(() => new Promise(resolve => { finishOld = resolve; }));
  const callback = h.sdk().captchaVerifyCallback('fixture-old');
  h.captcha.cancel();
  assert.ok((await old.outcome).error);
  const current = await h.begin();
  finishOld({ captchaResult: true, bizResult: true });
  await callback;
  assert.equal(h.captcha.isBusy(), true);
  await h.sdk().captchaVerifyCallback('fixture-current');
  assert.equal((await current.outcome).value.bizResult, true);
  assert.equal(h.captcha.isBusy(), false);
});

test('旧超时回调晚到时保留新验证码流程的忙碌状态', async () => {
  const h = harness();
  const old = await h.begin();
  const timeout = [...h.timers.values()][0].fn;
  await h.sdk().captchaVerifyCallback('fixture-old');
  await old.outcome;
  const current = await h.begin();
  timeout();
  assert.equal(h.captcha.isBusy(), true);
  h.captcha.cancel();
  assert.ok((await current.outcome).error);
  assert.equal(h.captcha.isBusy(), false);
});
