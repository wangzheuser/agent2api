'use strict';
const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require('../node_modules/typescript');

// 使用真实表单配置核对壳侧登记；导航语义另由 login.rs 的 Rust 测试验证。
const source = fs.readFileSync(path.join(__dirname, '../src/islands/add-account-configs.ts'), 'utf8');
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
}).outputText;
const context = { exports: {}, require: name => {
  if (name === '../i18n') return { t: key => key };
  if (name === './add-account-bridge') return { shared: () => ({}) };
  assert.fail(`unexpected import: ${name}`);
} };
vm.runInNewContext(compiled, context);
const login = fs.readFileSync(path.join(__dirname, '../../src-tauri/src/login.rs'), 'utf8');

function explicitProviders(name) {
  const body = login.match(new RegExp(`fn ${name}\\([^]*?^}`, 'm'));
  assert.ok(body, `missing ${name}`);
  const code = body[0].replace(/\/\/[^\n]*|\/\*[^]*?\*\//g, '');
  const providers = new Set();
  for (const arm of code.matchAll(/((?:"[^"]*"\s*\|\s*)*"[^"]*")\s*=>/g)) {
    for (const value of arm[1].matchAll(/"([^"]*)"/g)) providers.add(value[1]);
  }
  return providers;
}

test('every visible web login provider has explicit desktop normalization and navigation policy', () => {
  const providers = context.exports.BUILTIN_CONFIGS
    .filter(config => config.webLogin || config.oauthLogin || (config.provider === 'kuku' && config.smsLogin))
    .map(config => config.provider);
  assert.ok(providers.includes('minimax-code'));
  assert.ok(providers.includes('lobsterai'));
  assert.ok(providers.includes('antigravity'));
  const normalized = explicitProviders('normalize_provider');
  const navigation = explicitProviders('allowed_hosts');
  for (const provider of [...providers, 'workbuddy', 'workbuddy-intl']) {
    assert.ok(normalized.has(provider), `${provider}: missing desktop login registration`);
    assert.ok(navigation.has(provider), `${provider}: inherited WorkBuddy navigation policy`);
  }
});
