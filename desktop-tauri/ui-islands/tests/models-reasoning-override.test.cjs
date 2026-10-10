const test = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const vm = require('node:vm')
const ts = require('../node_modules/typescript')
const root = path.resolve(__dirname, '../src/islands')

function load(name, window = {}, cache = {}) {
  if (cache[name]) return cache[name]
  const compiled = ts.transpileModule(fs.readFileSync(path.join(root, `${name}.ts`), 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText
  const context = { exports: {}, window, require: dependency => load(dependency.replace(/^\.\//, ''), window, cache) }
  vm.runInNewContext(compiled, context, { filename: `${name}.ts` })
  return cache[name] = context.exports
}

test('default and forced indices remain independent and provider scoped', () => {
  const { buildIndex } = load('models-reasoning')
  const mappings = [{ alias: 'Alias', target: 'Target', provider: 'zcode', reasoning: 'medium', reasoningOverride: 'max' },
    { alias: 'alias', target: 'target', provider: 'catpaw', reasoning: 'high' }]
  assert.equal(buildIndex(mappings)(' alias ', 'target', 'ZCODE'), 'medium')
  assert.equal(buildIndex(mappings, 'reasoningOverride')('ALIAS', 'target', 'zcode'), 'max')
  assert.equal(buildIndex(mappings, 'reasoningOverride')('alias', 'target', 'catpaw'), '')
  assert.equal(buildIndex([{ ...mappings[0], reasoningOverride: '' }], 'reasoningOverride')('alias', 'target', 'zcode'), '')
})

test('custom edits, refresh imports and legacy same-name clear preserve binding semantics', async () => {
  let provider = { id: 'custom-fixture', name: 'Fixture', models: [{ id: 'upstream', enabled: true, reasoning: 'medium', reasoningOverride: 'max' }],
    mappings: [{ alias: 'alias', target: 'upstream', enabled: true, reasoning: 'low', reasoningOverride: 'high' }] }
  const window = { wbProviders: { customList: () => [provider], refreshCustom: async () => {}, customRequest: async (method, url, body) => {
    assert.equal(method, 'POST'); assert.match(url, /models/)
    provider = { ...provider, ...JSON.parse(JSON.stringify(body)) }
    return provider
  } } }
  const api = load('models-custom-source', window)
  await api.setBinding(provider.id, 'alias', 'upstream', { enabled: false })
  assert.equal(provider.models[0].reasoningOverride, 'max')
  assert.equal(provider.mappings[0].reasoningOverride, 'high')
  await api.setBinding(provider.id, 'alias', 'upstream', { enabled: true, reasoningOverride: 'max' })
  assert.equal(provider.mappings[0].reasoning, 'low')
  await api.setBinding(provider.id, 'alias', 'upstream', { reasoningOverride: '' })
  assert.equal(provider.mappings[0].reasoningOverride, '')
  assert.equal(provider.models[0].reasoningOverride, 'max', 'alias never mutates target default')
  await api.addModels(provider.id, ['upstream', 'new-model'])
  assert.equal(provider.models[0].reasoningOverride, 'max')
  assert.equal(provider.models.length, 2)
  provider.mappings.push({ alias: 'upstream', target: 'upstream', reasoningOverride: '' })
  assert.equal(api.buildView(provider.id).mappings.find(m => m.isDefault && m.alias === 'upstream').reasoningOverride, '')
  await api.setBinding(provider.id, 'upstream', 'upstream', { enabled: false })
  assert.equal(provider.models[0].reasoning, 'medium')
  assert.equal(provider.models[0].reasoningOverride, '')
})
