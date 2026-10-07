const { chromium } = require('playwright')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { pathToFileURL } = require('node:url')
const root = path.resolve(__dirname, '../../..')
const evidence = path.join(root, '.verify/cache-affinity')
fs.mkdirSync(evidence, { recursive: true })
const href = file => pathToFileURL(path.join(root, 'desktop-tauri/ui', file)).href
const file = path.join(evidence, 'settings-fixture.html')
fs.writeFileSync(file, `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><link rel="stylesheet" href="${href('islands/ui.css')}"></head><body><section class="page active" data-page="settings"></section><script>
window.fixture={calls:[],fail:false};window.wbApp={toast(){}};
window.workbuddyDesktop={saveAccountSelection:async value=>{fixture.calls.push(value);if(fixture.fail)throw new Error('fixture failure');return value;}};
</script><script src="${href('islands/ui.js')}"></script></body></html>`)
async function run() {
  const browser = await chromium.launch({ ...(process.platform === 'win32' ? { channel: 'msedge' } : {}), headless: true })
  const errors = []
  try {
    const page = await browser.newPage()
    page.on('pageerror', error => errors.push(error.message))
    await page.goto(pathToFileURL(file).href)
    await page.evaluate(() => {wbSettingsPanel.showCategory('gateway');wbSettingsPanel.renderAccountSelection({accountSelection:'balanced'})})
    const select = page.getByRole('combobox', { name: '账号选路策略' })
    await select.click()
    assert.equal(await page.getByRole('option').count(), 4)
    await page.getByRole('option', { name: '会话均衡亲和', exact: true }).click()
    await page.waitForFunction(() => fixture.calls.length === 1)
    assert.deepEqual(await page.evaluate(() => fixture.calls[0]), { accountSelection:'cacheAffinity' })
    assert.match(await select.innerText(), /会话均衡亲和/)
    await page.evaluate(() => { fixture.fail=true })
    await select.click()
    await page.getByRole('option', { name: '轮询', exact: true }).click()
    await page.waitForFunction(() => fixture.calls.length === 2)
    await page.waitForFunction(() => document.querySelector('#settings-account-selection').textContent.includes('会话均衡亲和'))
    assert.deepEqual(errors, [])
    const result={passed:3,checks:['four strategies available','cacheAffinity saved through bridge','failed save restores prior strategy'],pageErrors:errors}
    fs.writeFileSync(path.join(evidence,'SETTINGS-BROWSER.json'),JSON.stringify(result,null,2))
    console.log(JSON.stringify(result,null,2))
  } finally { await browser.close() }
}
run().catch(error => { console.error(error); process.exitCode=1 })
