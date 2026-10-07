const { chromium } = require('playwright')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { pathToFileURL } = require('node:url')

const root = path.resolve(__dirname, '../../..')
const evidence = path.resolve(process.env.AGENT2API_BROWSER_EVIDENCE || path.join(root, '.verify/reasoning-force-override'))
fs.mkdirSync(evidence, { recursive: true })
const href = file => pathToFileURL(path.join(root, 'desktop-tauri/ui', file)).href
const styles = ['css/tokens.css', 'css/layout.css', 'css/components.css', 'css/page-gateway.css', 'islands/ui.css']
const html = `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">${styles.map(file => `<link rel="stylesheet" href="${href(file)}">`).join('')}<style>body{padding:20px}.page{display:block!important}</style></head><body><section class="page active" data-page="gateway"></section><script>
window.fixture={writes:[],customWrites:[],view:{models:[{id:'glm-5.3-flash',name:'GLM',provider:'zcode',providerLabel:'ZCode',enabled:true,source:'builtin',aliases:['fixture-alias']}],mappings:[{alias:'glm-5.3-flash',target:'glm-5.3-flash',provider:'zcode',enabled:true,reasoning:'low',isDefault:true},{alias:'fixture-alias',target:'glm-5.3-flash',provider:'zcode',enabled:true,reasoning:'medium'}]},custom:{id:'custom-fixture',name:'Custom Fixture',models:[{id:'custom-model',reasoning:'low'}],mappings:[]}};
window.wbApp={currentPage:'gateway',toast(){}};
window.wbProviders={all:()=>[{id:'zcode',label:'ZCode'}],labelOf:id=>id,customList:()=>[fixture.custom],refreshCustom:async()=>{},customRequest:async(method,url,body)=>{fixture.customWrites.push(structuredClone(body));Object.assign(fixture.custom,body);return fixture.custom}};
window.workbuddyDesktop={getModelManage:async()=>fixture.view,addModelMapping:async(alias,target,provider,reasoning,enabled,reasoningOverride)=>{fixture.writes.push({alias,target,provider,reasoning,enabled,reasoningOverride});const item=fixture.view.mappings.find(m=>m.alias===alias&&m.target===target&&m.provider===provider);if(reasoning!==undefined)item.reasoning=reasoning;if(enabled!==undefined)item.enabled=enabled;if(reasoningOverride!==undefined)item.reasoningOverride=reasoningOverride;return fixture.view}};
</script><script src="${href('islands/ui.js')}"></script></body></html>`
const fixtureFile = path.join(evidence, 'fixture.html')
fs.writeFileSync(fixtureFile, html)

async function run() {
  const browser = await chromium.launch({ ...(process.platform === 'win32' ? { channel: 'msedge' } : {}), headless: true })
  const checks = [], errors = []
  try {
    const page = await browser.newPage({ viewport: { width: 1100, height: 900 } })
    page.on('pageerror', error => errors.push(error.message))
    await page.goto(pathToFileURL(fixtureFile).href)
    await page.locator('#models').getByRole('button', { name: 'medium', exact: true }).click()
    let dialog = page.getByRole('dialog')
    await dialog.waitFor()
    assert.equal(await dialog.locator('#mapping-reasoning').innerText(), 'medium')
    assert.equal(await dialog.locator('#mapping-reasoning-override').innerText(), '未设置')
    const positions = await dialog.evaluate(el => [el.querySelector('#mapping-reasoning').getBoundingClientRect().top, el.querySelector('#mapping-reasoning-override').getBoundingClientRect().top])
    assert.ok(positions[1] > positions[0])
    await dialog.locator('#mapping-reasoning-override').click()
    await page.getByRole('option', { name: 'max', exact: true }).click()
    await dialog.screenshot({ path: path.join(evidence, 'forced-max.png') })
    await dialog.getByRole('button', { name: '保存等级', exact: true }).click()
    await page.waitForFunction(() => fixture.writes.length === 1)
    assert.deepEqual(await page.evaluate(() => fixture.writes[0]), { alias: 'fixture-alias', target: 'glm-5.3-flash', provider: 'zcode', reasoning: 'medium', enabled: undefined, reasoningOverride: 'max' })
    checks.push('new dropdown is below default; save leaves default medium and forces max')
    await page.locator('#models').getByRole('button', { name: '强制 max', exact: true }).click()
    dialog = page.getByRole('dialog')
    await dialog.waitFor()
    assert.equal(await dialog.locator('#mapping-reasoning').innerText(), 'medium')
    assert.equal(await dialog.locator('#mapping-reasoning-override').innerText(), 'max')
    await page.setViewportSize({ width: 380, height: 850 })
    await dialog.screenshot({ path: path.join(evidence, 'forced-narrow.png') })
    await dialog.locator('#mapping-reasoning-override').click()
    await page.getByRole('option', { name: '未设置', exact: true }).click()
    await dialog.getByRole('button', { name: '保存等级', exact: true }).click()
    await page.waitForFunction(() => fixture.writes.length === 2)
    assert.equal(await page.evaluate(() => fixture.writes[1].reasoningOverride), '')
    assert.equal(await page.evaluate(() => fixture.writes[1].reasoning), 'medium')
    checks.push('reopen preserves both fields; clear sends empty override without clearing default')
    await page.setViewportSize({ width: 1100, height: 900 })
    await page.evaluate(() => wbModelsPanel.selectProvider('custom-fixture'))
    await page.locator('#models').getByRole('button', { name: 'low', exact: true }).click()
    dialog = page.getByRole('dialog')
    await dialog.waitFor()
    await dialog.locator('#mapping-reasoning-override').click()
    await page.getByRole('option', { name: 'high', exact: true }).click()
    await dialog.getByRole('button', { name: '保存等级', exact: true }).click()
    await page.waitForFunction(() => fixture.customWrites.length === 1)
    assert.equal(await page.evaluate(() => fixture.customWrites[0].models[0].reasoningOverride), 'high')
    assert.equal(await page.evaluate(() => fixture.customWrites[0].models[0].reasoning), 'low')
    checks.push('same UI saves independent forced field for custom providers')
    assert.deepEqual(errors, [])
    fs.writeFileSync(path.join(evidence, 'report.json'), JSON.stringify({ checks, errors }, null, 2))
    console.log(`PASS ${checks.length} reasoning override browser checks`)
  } finally { await browser.close() }
}
run().catch(error => { console.error(error); process.exitCode = 1 })
