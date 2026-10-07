const { chromium } = require('playwright')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { pathToFileURL } = require('node:url')

const root = process.env.MODEL_TEST_ROOT || path.resolve(__dirname, '../../..')
const evidence = path.join(root, '.verify/model-test-disabled-binding')
fs.mkdirSync(evidence, { recursive: true })
const href = file => pathToFileURL(path.join(root, 'desktop-tauri/ui', file)).href
const data = {
  models: ['Raw-A', 'Raw-B', 'Raw-C'].map((id, i) => ({ id, provider: 'qoder', providerLabel: 'Qoder', source: 'remote', enabled: i === 2 })),
  mappings: [
    { alias: 'qoder/a', target: 'Raw-A', provider: 'qoder', enabled: true, reasoning: 'high' },
    { alias: 'qoder/b', target: 'Raw-B', provider: 'qoder', enabled: false },
  ],
}
const html = `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><link rel="stylesheet" href="${href('islands/ui.css')}"><style>.page{display:block}body{padding:20px}</style></head><body><section class="page active" data-page="gateway"></section><script>
window.fixture={data:${JSON.stringify(data)},accounts:[{id:'fixture-account',provider:'qoder',enabled:true,available:true,name:'测试账号',priority:0}],calls:[]};
window.wbApp={currentPage:'gateway',getState:()=>({accounts:{accounts:fixture.accounts}}),toast(){}};
window.wbProviders={customList:()=>[],refreshCustom:async()=>{},labelOf:id=>id,all:()=>[{id:'qoder',label:'Qoder'}]};
window.workbuddyDesktop={getModelManage:async()=>fixture.data,testModel:async body=>{fixture.calls.push(body);return {success:true,status:200,reply:'fixture OK',account_id:body.account_id,upstream_model:body.model,duration_ms:10}},terminateStatsRequest:async()=>{}};
</script><script src="${href('islands/ui.js')}"></script></body></html>`
const file = path.join(evidence, 'model-test-fixture.html')
fs.writeFileSync(file, html)

async function run() {
  const browser = await chromium.launch({ ...(process.platform === 'win32' ? { channel: 'msedge' } : {}), headless: true })
  const errors = []
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } })
    page.on('pageerror', error => errors.push(error.message))
    await page.goto(pathToFileURL(file).href)
    await page.getByRole('button', { name: '测试', exact: true }).first().waitFor()
    const row = id => page.locator('tbody tr').filter({ has: page.getByText(id, { exact: true }).first() }).first()
    if (process.env.EXPECT_OLD === '1') {
      assert.equal(await row('Raw-A').getByRole('button', { name: '测试', exact: true }).isDisabled(), true)
      assert.equal(await row('Raw-B').getByRole('button', { name: '测试', exact: true }).isDisabled(), true)
      console.log('BASELINE: default-off test buttons are disabled; no upstream calls')
    } else {
      for (const id of ['Raw-A', 'Raw-B', 'Raw-C']) {
        await row(id).getByRole('button', { name: '测试', exact: true }).click()
        await page.getByRole('button', { name: '开始测试', exact: true }).click()
        await page.getByText('fixture OK', { exact: true }).waitFor()
        const request = await page.evaluate(() => fixture.calls.at(-1))
        assert.equal(request.model, id)
        assert.equal(request.provider, 'qoder')
        assert.equal(request.account_id, 'fixture-account')
        assert.equal(Object.hasOwn(request, 'test_target'), false)
        const dialogs = page.getByRole('dialog')
        await dialogs.last().getByRole('button', { name: '关闭', exact: true }).first().click()
        await dialogs.first().getByRole('button', { name: '关闭', exact: true }).first().click()
      }
      assert.deepEqual(await page.evaluate(() => fixture.data), data)
      assert.equal(await page.evaluate(() => fixture.calls.length), 3)
      console.log('MODIFIED: default-off, all-bindings-off and default-on tests send the original model and pinned account without changing mappings')
    }
    await page.evaluate(() => { fixture.accounts[0].enabled = false; wbModelsPanel.render() })
    await page.waitForFunction(() => [...document.querySelectorAll('button')].filter(button => button.textContent.trim() === '测试').every(button => button.disabled))
    await page.screenshot({ path: path.join(evidence, process.env.EXPECT_OLD === '1' ? 'baseline.png' : 'modified.png') })
    assert.deepEqual(errors, [])
    console.log('PASS: no usable account still disables every test button; no browser errors')
  } finally {
    await browser.close()
  }
}
run().catch(error => { console.error(error); process.exitCode = 1 })
