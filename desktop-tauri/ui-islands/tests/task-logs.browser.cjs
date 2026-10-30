const { chromium } = require('playwright');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
const root = path.resolve(__dirname, '../../..');
const evidence = path.join(root, '.verify/task-logs');
fs.mkdirSync(evidence, { recursive: true });
const href = file => pathToFileURL(path.join(root, 'desktop-tauri/ui', file)).href;
const fixtureFile = path.join(evidence, 'fixture.html');
const styles = ['css/tokens.css', 'css/layout.css', 'css/components.css', 'css/page-logs.css', 'css/page-tasks.css', 'css/page-settings.css', 'islands/ui.css'];
fs.writeFileSync(fixtureFile, `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
${styles.map(file => `<link rel="stylesheet" href="${href(file)}">`).join('')}</head><body>
<section class="page active" data-page="logs"></section><section class="page" data-page="tasks"></section><section class="page" data-page="settings"></section>
<script>
window.fixture={queries:[],saves:[],clear:[],toasts:[],epoch:'initial',latest:100000,cap:100000,fail:false,defer:false,pending:[],retention:{logMaxEntries:100000,logRetentionDays:30,requestRetentionDays:30,dailyRetentionDays:365}};
window.wbApp={currentPage:'logs',toast(message,kind){fixture.toasts.push({message,kind})},showPage(page){this.currentPage=page;document.querySelectorAll('.page').forEach(el=>el.classList.toggle('active',el.dataset.page===page))}};
window.wbFilterMemory={load(key,defaults){return {...defaults,...JSON.parse(localStorage.getItem(key)||'{}')}},save(key,value){localStorage.setItem(key,JSON.stringify(value))}};
fixture.tasks=[{id:'zcodeAutoClaim',label:'ZCode 自动领取套餐',runner:'backend',canRun:true},{id:'workbuddyGrowth',label:'WorkBuddy 福利巡检',runner:'backend',canRun:true},{id:'logsAutoRefresh',label:'日志页自动刷新',runner:'frontend',canRun:false}].map(t=>({...t,description:'fixture',unit:'minutes',enabled:false,interval:10,min:1,max:1440,defaultInterval:10,running:false,lastRunAt:null,lastResult:null,nextRunAt:null}));
fixture.result=q=>{
 const p=new URLSearchParams(q),task=p.get('taskId'),snapshotReset=!!p.get('snapshotEpoch')&&p.get('snapshotEpoch')!==fixture.epoch;
 const snapshot=snapshotReset?fixture.latest:Number(p.get('snapshotId'))||fixture.latest;
 const high=Math.min(snapshot,fixture.latest),total=Math.min(fixture.cap,fixture.latest);
 const matched=p.get('keyword')==='no-match'?0:Math.min(total,task?Math.floor(high/2):high);
 const offset=snapshotReset?0:Number(p.get('offset'))||0,limit=Number(p.get('limit'))||200;
 const entries=Array.from({length:Math.max(0,Math.min(limit,matched-offset))},(_,i)=>({id:high-(offset+i)*(task?2:1),ts:Date.now(),level:'info',category:'server',taskId:task||null,message:'fixture log '+(high-(offset+i)*(task?2:1))}));
 return {entries,total,matched,max:fixture.cap,snapshotId:snapshot,snapshotEpoch:fixture.epoch,snapshotReset,categories:{server:'服务',model:'模型'},tasks:{zcodeAutoClaim:'ZCode 自动领取套餐',workbuddyGrowth:'WorkBuddy 福利巡检'}};
};
window.workbuddyDesktop={
 async getLogs(q){fixture.queries.push(q);if(fixture.defer){fixture.defer=false;return new Promise(resolve=>fixture.pending.push(()=>resolve(fixture.result(q))))}return fixture.result(q)},
 async getLogStats(){return {lastId:fixture.latest,total:Math.min(fixture.latest,fixture.cap)}},
 async getScheduledTasks(){return {tasks:fixture.tasks}},
 async getRetention(){return {...fixture.retention}},
 async saveRetention(patch){fixture.saves.push(patch);if(fixture.fail)throw new Error('fixture persistence failed');Object.assign(fixture.retention,patch);fixture.cap=fixture.retention.logMaxEntries;return {...fixture.retention,removedLogs:0}},
 async getStorage(){return {}},async clearLogs(q){fixture.clear.push(q);return {}},async exportLogs(){return {count:fixture.cap,file:'fixture.jsonl'}}
};
</script><script src="${href('islands/ui.js')}"></script></body></html>`);

async function run() {
  const browser = await chromium.launch({ ...(process.platform === 'win32' ? { channel: 'msedge' } : {}), headless: true });
  const checks = [];
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(pathToFileURL(fixtureFile).href);
    const logs = page.locator('[data-page="logs"]');
    await logs.locator('.log-row').nth(49).waitFor();
    assert.equal(await logs.locator('.log-row').count(), 50);
    assert.match(await logs.locator('#logs-badge').innerText(), /100000.*100000/);
    assert.equal(await page.evaluate(() => new URLSearchParams(fixture.queries.at(-1)).get('limit')), '50');
    await logs.getByRole('combobox', { name: '每页条数' }).click();
    await page.getByRole('option', { name: '50', exact: true }).waitFor();
    assert.equal(await page.getByRole('option', { name: '全部', exact: true }).count(), 0);
    await page.keyboard.press('Escape');
    await page.getByRole('listbox').waitFor({state:'detached'});
    for (const size of [500,1000,2000,5000,10000,50]) {
      await logs.getByRole('combobox', { name: '每页条数' }).click();
      await page.getByRole('option', {name:String(size),exact:true}).click();
      await page.waitForFunction(size => document.querySelectorAll('[data-page="logs"] .log-row').length === size, size);
      assert.equal(await page.evaluate(() => new URLSearchParams(fixture.queries.at(-1)).get('limit')), String(size));
      assert.equal(await page.evaluate(() => localStorage.getItem('workbuddy-desktop-table-size:logs')), String(size));
    }
    checks.push('explicit large page sizes 500/1000/2000/5000/10000 query and render their requested page without changing default 50');
    await logs.getByRole('button', { name: '下一页', exact: true }).click();
    await page.waitForFunction(() => fixture.queries.some(q => new URLSearchParams(q).get('offset') === '50'));
    const previous = await logs.locator('.log-row').first().innerText();
    await page.evaluate(async () => { fixture.latest+=10; await wbLogsPanel.load({silent:true}); });
    assert.equal(await logs.locator('.log-row').first().innerText(), previous);
    assert.equal(await page.evaluate(() => new URLSearchParams(fixture.queries.at(-1)).get('snapshotId')), '100000');
    checks.push('100k server paging stays at 50 DOM rows and stable snapshot during inserts');
    await page.evaluate(async () => {fixture.epoch='restored';fixture.latest=120000;await wbLogsPanel.load({silent:true});});
    await page.waitForFunction(() => new URLSearchParams(fixture.queries.at(-1)).get('offset') === '0');
    await logs.getByRole('button', { name: '下一页', exact: true }).click();
    await page.waitForFunction(() => new URLSearchParams(fixture.queries.at(-1)).get('offset') === '50');
    await page.evaluate(async () => {fixture.cap=30;await wbLogsPanel.load({silent:true});});
    await page.waitForFunction(() => new URLSearchParams(fixture.queries.at(-1)).get('offset') === '0');
    await page.waitForFunction(() => document.querySelectorAll('[data-page="logs"] .log-row').length === 30);
    await page.evaluate(async () => {fixture.cap=100000;await wbLogsPanel.load({silent:true});});
    checks.push('clear or restore generation resets history; retention shrink corrects an out-of-range page');

    await logs.getByRole('combobox', { name: '按定时任务筛选' }).click();
    await page.getByRole('option', { name: 'ZCode 自动领取套餐', exact: true }).click();
    await page.waitForFunction(() => new URLSearchParams(fixture.queries.at(-1)).get('taskId') === 'zcodeAutoClaim');
    assert.equal(await page.evaluate(() => new URLSearchParams(fixture.queries.at(-1)).get('offset')), '0');
    await logs.getByRole('searchbox', { name: '搜索消息关键词' }).fill('no-match');
    await logs.getByText('没有符合筛选条件的日志', { exact: true }).waitFor();
    assert.equal(await logs.locator('.log-row').count(), 0);
    await page.evaluate(() => wbApp.showPage('tasks'));
    const taskCard = page.locator('[data-task="zcodeAutoClaim"]');
    await taskCard.getByRole('button', { name: '查看日志', exact: true }).click();
    await logs.locator('.log-row').first().waitFor();
    const jump = await page.evaluate(() => Object.fromEntries(new URLSearchParams(fixture.queries.at(-1))));
    assert.equal(jump.taskId, 'zcodeAutoClaim');
    for (const key of ['keyword','category','level','start']) assert.equal(jump[key], undefined);
    assert.equal(await page.locator('[data-task="logsAutoRefresh"]').getByRole('button', { name: '查看日志' }).count(), 0);
    assert.equal(await page.evaluate(() => JSON.parse(localStorage.getItem('workbuddy-desktop-logs-filters')).taskId), 'zcodeAutoClaim');
    checks.push('task filter composes with keyword, persists, and task-card jump clears conflicting filters');

    await page.evaluate(() => {fixture.defer=true;void wbLogsPanel.load();});
    await page.waitForFunction(() => fixture.pending.length === 1);
    await logs.getByRole('searchbox', { name: '搜索消息关键词' }).fill('no-match');
    await logs.getByText('没有符合筛选条件的日志', { exact: true }).waitFor();
    await page.evaluate(() => fixture.pending.shift()());
    await page.waitForTimeout(100);
    assert.equal(await logs.locator('.log-row').count(), 0);
    checks.push('late old response cannot overwrite a newer filter');

    await page.evaluate(() => {wbApp.showPage('settings');wbSettingsPanel.showCategory('data');wbSettingsPanel.renderRetention(fixture.retention);});
    const input = page.locator('#settings-retention-log-max');
    await input.waitFor();
    assert.equal(await input.inputValue(), '100000');
    for (const invalid of ['0','100001','1.5']) {
      await input.fill(invalid); await input.press('Tab');
      await page.waitForTimeout(50);
    }
    assert.equal(await page.evaluate(() => fixture.saves.length), 0);
    await input.fill('1000'); await input.press('Tab');
    let dialog = page.getByRole('dialog');
    await dialog.getByText(/100000 条改为 1000 条/).waitFor();
    await dialog.getByRole('button', { name: '取消', exact: true }).click();
    await dialog.waitFor({ state: 'detached' });
    assert.equal(await input.inputValue(), '100000');
    assert.equal(await page.evaluate(() => fixture.saves.length), 0);
    await input.fill('1000'); await input.press('Tab');
    await dialog.getByRole('button', { name: '继续并清理', exact: true }).click();
    await page.waitForFunction(() => fixture.retention.logMaxEntries === 1000);
    assert.deepEqual(await page.evaluate(() => fixture.saves[0]), {logMaxEntries:1000});
    await input.fill('2000'); await input.press('Tab');
    await page.waitForFunction(() => fixture.retention.logMaxEntries === 2000);
    await page.evaluate(() => {fixture.fail=true;});
    await input.fill('3000'); await input.press('Tab');
    await page.waitForFunction(() => fixture.saves.length === 3);
    await page.waitForFunction(() => document.querySelector('#settings-retention-log-max').value === '2000');
    assert.equal(await page.evaluate(() => fixture.retention.logMaxEntries), 2000);
    assert.ok(await page.evaluate(() => fixture.toasts.some(t => t.message.includes('fixture persistence failed'))));
    checks.push('limits validate; shrink cancel does not save; confirmed shrink and growth save immediately; failure restores persisted value');
    await page.screenshot({path:path.join(evidence,'retention-desktop.png')});
    await page.evaluate(async () => {wbApp.showPage('logs');await wbLogsPanel.showTask('zcodeAutoClaim');});
    await page.screenshot({path:path.join(evidence,'logs-desktop.png')});
    await page.setViewportSize({ width: 390, height: 844 });
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
    assert.ok(await logs.locator('.log-row').count() <= 50);
    await page.screenshot({path:path.join(evidence,'logs-mobile.png')});
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({passed:checks.length,checks,pageErrors:errors},null,2));
  } finally { await browser.close(); }
}
run().catch(error => { console.error(error);process.exitCode=1; });
