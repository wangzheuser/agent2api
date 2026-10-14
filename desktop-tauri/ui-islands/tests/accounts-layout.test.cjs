const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')

const root = path.resolve(__dirname, '..')
const read = file => fs.readFileSync(path.join(root, file), 'utf8')
const columns = read('src/islands/accounts-columns.ts')
const page = read('src/islands/accounts-page.tsx')
const panels = read('src/islands/accounts-panels.tsx')

const keysFrom = (text, pattern) => [...text.matchAll(pattern)].map(match => match[1])
const declared = keysFrom(
  columns.slice(columns.indexOf('export const ACCOUNT_COLUMNS'), columns.indexOf('/* ─── 列宽')),
  /key:\s*'([^']+)'/g,
)
const colgroup = keysFrom(page.slice(page.indexOf('function ColGroup'), page.indexOf('/* ─── 挂载')),
  /col\('([^']+)'\)/g)
const header = keysFrom(panels.slice(panels.indexOf('export function TableHead'), panels.indexOf('export function NameEyeButton')),
  /data-col='([^']+)'/g)

assert.deepEqual(colgroup, declared, '账号表 colgroup 必须与列定义保持同一列序')
assert.deepEqual(header, declared, '账号表表头必须与列定义保持同一列序')
assert.equal(header.includes('priority'), false, '账号表不能恢复已移除的优先级列')
assert.equal(header.includes('provider'), false, '账号表不能恢复已移除的提供商列')

console.log('accounts layout columns stay aligned')
