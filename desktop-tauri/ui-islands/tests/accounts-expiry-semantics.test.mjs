import assert from 'node:assert/strict'
import { test } from 'node:test'
import { build } from 'esbuild'
import { fileURLToPath } from 'node:url'

const bundle = await build({
  entryPoints: [fileURLToPath(new URL('../src/islands/accounts-domain.ts', import.meta.url))],
  bundle: true,
  format: 'esm',
  platform: 'node',
  write: false,
})
const { expiryScopeOf } = await import(
  `data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`,
)

test('expiry scope distinguishes routing, control-plane and estimated sessions', () => {
  assert.equal(expiryScopeOf({ id: 'office', provider: 'officeace' }), 'control-plane')
  assert.equal(expiryScopeOf({ id: 'loomy', provider: 'loomy' }), 'estimated-session')
  assert.equal(expiryScopeOf({ id: 'codearts', provider: 'codearts' }), 'routing')
  assert.equal(expiryScopeOf({ id: 'unknown', provider: 'unknown' }), 'routing')
})
