const test = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const vm = require('node:vm')
const ts = require('../node_modules/typescript')

const source = path.resolve(__dirname, '../src/islands/models-mapping.ts')

function load() {
  const compiled = ts.transpileModule(fs.readFileSync(source, 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText
  const context = { exports: {} }
  vm.runInNewContext(compiled, context, { filename: source })
  return context.exports
}

test('new mappings use regional prefixes and preserve the model id verbatim', () => {
  const { defaultMappingAlias } = load()
  const cases = [
    ['workbuddy', 'workbuddy-cn'],
    ['workbuddy-intl', 'workbuddy-ai'],
    ['zcode', 'zcode-cn'],
    ['zcode-intl', 'zcode-ai'],
    ['autoclaw', 'autoclaw-cn'],
    ['autoclaw-intl', 'autoclaw-ai'],
    ['qoder', 'qoder-cn'],
    ['qoder-intl', 'qoder-ai'],
    ['monkeycode', 'monkeycode-cn'],
    ['monkeycode-intl', 'monkeycode-ai'],
    ['accio-cn', 'accio-cn'],
    ['accio', 'accio-ai'],
  ]
  for (const [provider, prefix] of cases) {
    assert.equal(defaultMappingAlias(provider, 'Model.ID/v1'), `${prefix}/Model.ID/v1`, provider)
  }
})

test('ordinary providers use lowercase provider ids', () => {
  const { defaultMappingAlias } = load()
  assert.equal(defaultMappingAlias('CommandCode', 'model-x'), 'commandcode/model-x')
  assert.equal(defaultMappingAlias('  My.Provider  ', 'MiXeD.Model'), 'my.provider/MiXeD.Model')
})
