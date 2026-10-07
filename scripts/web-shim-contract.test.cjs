'use strict';
const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

// 执行真实 headless bridge，不复制其实现，不访问网络或使用真实账号。
const source = fs.readFileSync(path.join(__dirname, '../desktop-tauri/src-tauri/server/src/web_shim.rs'), 'utf8');
const script = source.match(/r#"(\(function[\s\S]*?)"#/)[1];
function fixture(respond, auth) {
  const calls = [], storage = new Map();
  if (auth) storage.set('agent2api.panelAuth', JSON.stringify(auth));
  const popup = { closed: false, location: {}, close() { this.closed = true; } };
  const window = { location: { origin: 'https://fixture.invalid', href: 'https://fixture.invalid/' }, open: () => popup };
  const context = { window, URL, console, document: { getElementById: () => null },
    localStorage: { getItem: k => storage.get(k) || null, setItem: (k, v) => storage.set(k, v), removeItem: k => storage.delete(k) },
    setTimeout: callback => { queueMicrotask(callback); return 1; }, clearTimeout() {},
    fetch: async (url, init) => {
      calls.push({ url, init });
      const [status, payload] = respond(url, init, calls.length);
      return { status, ok: status >= 200 && status < 300, text: async () => JSON.stringify(payload), json: async () => payload };
    },
  };
  vm.runInNewContext(script, context);
  return { bridge: window.workbuddyDesktop, calls, popup, storage };
}

test('remote callback preserves state and posts only to the gateway', async () => {
  const f = fixture(() => [200, { success: true, data: { accepted: true } }]);
  await f.bridge.submitLoginCallback('state-fixture', 'http://127.0.0.1:1234/callback?code=fixture');
  assert.equal(f.calls[0].url, '/api/session/login/callback');
  assert.deepEqual(JSON.parse(f.calls[0].init.body), { state: 'state-fixture', callbackUrl: 'http://127.0.0.1:1234/callback?code=fixture' });
});

test('raccoon web login redirects to the panel origin with the original state', async () => {
  const f = fixture(url => [200, { success: true, data: url === '/api/session/login/start'
    ? { state: 'state-fixture', authUrl: 'https://authorize.invalid/login?login_source=desktop' }
    : { done: true, session: { id: 'fixture' } } }]);
  const result = await f.bridge.startLogin('cn', 'personal', 'raccoon');
  const auth = new URL(f.popup.location.href), callback = new URL(auth.searchParams.get('redirect'));
  assert.equal(auth.searchParams.get('login_source'), 'web');
  assert.equal(callback.origin, 'https://fixture.invalid');
  assert.equal(callback.pathname, '/api/session/login/raccoon-callback');
  assert.equal(callback.searchParams.get('state'), 'state-fixture');
  assert.equal(result.id, 'fixture');
  assert.equal(f.popup.closed, true);
});

test('panel body-token refresh rotates stored tokens and retries once', async () => {
  const f = fixture((url, init, call) => call === 1
    ? [401, { error: { type: 'panel_login_required' } }]
    : url.startsWith('/api/panel/refresh')
      ? [200, { data: { accessToken: 'new-fixture', refreshToken: 'new-refresh-fixture' } }]
      : [200, { success: true, data: { ok: true } }], { access: 'old-fixture', refresh: 'old-refresh-fixture' });
  assert.equal((await f.bridge.getState()).ok, true);
  assert.equal(f.calls.length, 3);
  assert.equal(f.calls[1].url, '/api/panel/refresh?auth-mode=body');
  assert.equal(f.calls[1].init.headers['x-panel-refresh'], 'old-refresh-fixture');
  assert.equal(f.calls[2].init.headers['x-panel-token'], 'new-fixture');
  assert.equal(f.calls[2].init.headers['x-panel-refresh'], undefined);
  assert.deepEqual(JSON.parse(f.storage.get('agent2api.panelAuth')), { access: 'new-fixture', refresh: 'new-refresh-fixture' });
});

test('cookie-mode refresh never opts into body tokens', async () => {
  const f = fixture((url, init, call) => call === 1
    ? [401, { error: { type: 'panel_login_required' } }]
    : [200, { success: true, data: {} }]);
  await f.bridge.getState();
  assert.equal(f.calls[1].url, '/api/panel/refresh');
  assert.equal(f.calls[1].init.headers['x-panel-auth-mode'], undefined);
  assert.equal(f.storage.has('agent2api.panelAuth'), false);
});
