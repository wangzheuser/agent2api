import assert from 'node:assert/strict'
import { test } from 'node:test'
import { build } from 'esbuild'
import { fileURLToPath } from 'node:url'

const bundle = await build({ entryPoints: [fileURLToPath(new URL('../src/islands/settings-model.ts', import.meta.url))], bundle: true, format: 'esm', platform: 'node', write: false })
const { ACCOUNT_SELECTION_OPTIONS, TIPS, NOTES } = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`)

test('cacheAffinity settings retain all existing choices and explain session fallback', () => {
  assert.deepEqual(ACCOUNT_SELECTION_OPTIONS.map(option => option.value), ['balanced', 'priority', 'roundRobin', 'cacheAffinity'])
  assert.equal(ACCOUNT_SELECTION_OPTIONS[0].label, '负载均衡（默认）')
  assert.equal(ACCOUNT_SELECTION_OPTIONS[3].label, '会话均衡亲和')
  assert.match(TIPS.accountSelection, /可靠会话复用账号/)
  assert.match(TIPS.accountSelection, /新会话按会话占用与实时负载均衡/)
  assert.match(TIPS.accountSelection, /没有可靠会话标识时仅均衡、不持久绑定/)
  assert.match(NOTES.accountSelection, /默认使用负载均衡/)
})
