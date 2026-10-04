const { chromium } = require('playwright');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
const root = path.resolve(__dirname, '../../..');
const evidence = path.join(root, '.verify/workbuddy-credit-details');
fs.mkdirSync(evidence, {recursive:true});
const ui = path.join(root, 'desktop-tauri/ui');
const href = relative => pathToFileURL(path.join(ui, relative)).href;
const styles = ['css/tokens.css','css/layout.css','css/components.css','css/page-accounts.css','css/page-accounts-providers.css','css/page-accounts-table.css','islands/ui.css'];
const html = `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">${styles.map(file=>`<link rel="stylesheet" href="${href(file)}">`).join('')}<style>body{padding:20px}.page{display:block!important}</style></head><body><section class="page active" data-page="accounts"></section><script>
window.fixture={calls:[],pending:[],accounts:[{id:'A',uid:'demo-cn',name:'国内示例账号',provider:'workbuddy',edition:'cn',enabled:true,addedAt:1},{id:'B',uid:'demo-intl',name:'国际示例账号',provider:'workbuddy',edition:'intl',enabled:true,addedAt:2},{id:'C',name:'其他提供商',provider:'catpaw',enabled:true,addedAt:3}]};
window.wbApp={getState:()=>({accounts:{accounts:fixture.accounts}}),toast(){}};
window.workbuddyDesktop={getAllBalances(id){fixture.calls.push(id);return new Promise((resolve,reject)=>fixture.pending.push({resolve,reject}));}};
</script><script src="${href('islands/ui.js')}"></script></body></html>`;
fs.writeFileSync(path.join(evidence,'browser-fixture.html'),html);
let serial = 0;
function details(overrides={}) {
 const now=Date.now();
 const segment=(id,amount,delay)=>({id,resourceId:id,packageCode:'same-package',name:id==='monthly'?'月度基础额度':'活动奖励',remaining:amount,total:id==='monthly'?500:100,expiresAt:now+delay,expiresAtText:null,expiryStatus:'known',entitlementEndsAt:id==='monthly'?now+365*86400000:null,state:'active'});
 return {version:1,kind:'personal',fetchedAt:now+serial++,complete:true,remaining:580.5,unlimited:false,unattributedRemaining:0,issues:[],segments:[segment('reward-a',100,12*3600000),segment('reward-b',100,12*3600000),segment('monthly',380.5,28*86400000)],...overrides};
}
const usage=d=>({kind:d.kind,totalLeft:Math.floor(d.remaining||0),planLeft:380,bonusLeft:200,unlimited:d.unlimited,creditDetails:d});
async function apply(page,d,id='A') {await page.evaluate(({id,value})=>window.wbAccountsView.applyBalances({results:[{id,usage:value}]}),{id,value:usage(d)});}
async function resolve(page,d) {await page.evaluate(value=>fixture.pending.shift().resolve({results:[{id:'A',usage:value}]}),usage(d));}
async function run() {
 const browser=await chromium.launch({channel:'msedge',headless:true});
 const checks=[];
 try {
  const page=await browser.newPage({viewport:{width:1100,height:960},locale:'zh-CN',timezoneId:'Asia/Shanghai'});
  const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.goto(pathToFileURL(path.join(evidence,'browser-fixture.html')).href);
  const trigger=page.locator('.credit-balance-trigger').first();
  await trigger.waitFor(); await apply(page,details());
  await trigger.click();assert.equal(await page.getByRole('dialog').count(),0); checks.push('mouse single click stays closed');
  await trigger.dblclick();await page.getByRole('dialog').waitFor();
  assert.equal(await page.locator('.credits-balance').innerText(),'580.50');
  assert.equal(await page.locator('.credits-row').count(),3);
  assert.equal(await page.locator('.credits-segment').count(),3);
  assert.equal(await page.evaluate(()=>fixture.calls.length),0);checks.push('double click opens fresh cache; independent resource identities preserved');
  await page.mouse.move(1,1);await page.locator('.credits-segment').first().hover();await page.locator('[data-slot=tooltip-content]').waitFor();
  assert.match(await page.locator('[data-slot=tooltip-content]').innerText(),/100.00/);checks.push('segment tooltip exposes amount and expiry');
  await page.locator('.credits-segment').nth(2).click();
  assert.equal(await page.locator('.credits-row').nth(2).getAttribute('aria-pressed'),'true');
  assert.equal(await page.locator('.credits-row').nth(2).evaluate(el=>el===document.activeElement),true);checks.push('segment click selects and focuses matching row');
  for(let i=0;i<12;i++) await page.keyboard.press('Tab');
  assert.equal(await page.evaluate(()=>!!document.activeElement?.closest('[role=dialog]')),true); checks.push('dialog traps keyboard focus');
  await page.getByRole('button',{name:'刷新当前账号',exact:true}).click();
  assert.equal(await page.locator('.credits-balance').innerText(),'580.50');
  assert.equal(await page.getByRole('button',{name:'刷新当前账号',exact:true}).isDisabled(),true);
  await page.evaluate(()=>fixture.pending.shift().reject(new Error('fixture network failure')));
  await page.getByRole('alert').waitFor();assert.match(await page.getByRole('alert').innerText(),/上次成功/);
  assert.equal(await page.locator('.credits-balance').innerText(),'580.50'); checks.push('refresh retains old value, disables duplicate request, and shows failure');
  await page.getByRole('button',{name:'重试当前账号',exact:true}).click();await resolve(page,details());
  await page.getByRole('alert').waitFor({state:'detached'});
  await page.getByRole('button',{name:'关闭',exact:true}).last().focus();await page.mouse.move(1,1);await page.waitForTimeout(500);await page.screenshot({path:path.join(evidence,'actual-desktop.png')});
  await page.keyboard.press('Escape');await page.getByRole('dialog').waitFor({state:'detached'});
  assert.equal(await trigger.evaluate(el=>el===document.activeElement),true);checks.push('Escape restores focus');
  await trigger.press('Enter');await page.getByRole('dialog').waitFor();
  await page.getByRole('button',{name:'关闭',exact:true}).last().click();
  await trigger.press('Space');await page.getByRole('dialog').waitFor();checks.push('Enter and Space open');
  await page.setViewportSize({width:320,height:800});
  const metrics=await page.getByRole('dialog').evaluate(el=>({width:el.clientWidth,scrollWidth:el.scrollWidth,bodyWidth:el.querySelector('.credits-body').clientWidth,bodyScroll:el.querySelector('.credits-body').scrollWidth}));
  assert.ok(metrics.scrollWidth<=metrics.width+1 && metrics.bodyScroll<=metrics.bodyWidth+1,JSON.stringify(metrics));
  await page.getByRole('button',{name:'关闭',exact:true}).last().focus();await page.mouse.move(1,1);await page.waitForTimeout(500);await page.screenshot({path:path.join(evidence,'actual-narrow.png')});checks.push('320px dialog has no horizontal overflow');
  await page.evaluate(()=>document.documentElement.setAttribute('data-theme','dark'));
  await page.getByRole('button',{name:'关闭',exact:true}).last().focus();await page.mouse.move(1,1);await page.waitForTimeout(500);await page.screenshot({path:path.join(evidence,'actual-dark.png')});
  await apply(page,details({complete:false,issues:['truncated'],remaining:100,segments:[details().segments[0]]}));
  await page.locator('.credits-empty-bar').waitFor();assert.equal(await page.locator('.credits-segment').count(),0);
  assert.match(await page.locator('.credits-summary').innerText(),/已读取积分/);checks.push('partial response shows read amount and suppresses distribution');
  const unknown=details();unknown.segments[0].expiresAt=null;unknown.segments[0].expiryStatus='timezone_unverified';unknown.segments[0].expiresAtText='2026-10-31';unknown.issues=['timezone_unverified'];
  await apply(page,unknown);assert.equal(await page.locator('.credits-expiring').innerText(),'100.00');checks.push('unknown timezone excluded from 24h expiry sum');
  await apply(page,details({kind:'enterprise',unlimited:true,remaining:null,segments:[]}));
  assert.match(await page.locator('.credits-balance').innerText(),/不限量/);assert.equal(await page.locator('.credits-distribution').count(),0);checks.push('enterprise unlimited has no false percentage');
  await page.keyboard.press('Escape');
  await apply(page,details(),'B');
  await page.locator('.credit-balance-trigger').nth(1).press('Enter');
  assert.match(await page.locator('.credits-description').innerText(),/国际/);checks.push('international account routes through same dialog');
  assert.equal(await page.locator('.credit-balance-trigger').count(),3);checks.push('all balance-capable providers expose the same dialog trigger');
  await page.close();
  const touch=await browser.newPage({viewport:{width:1100,height:960},hasTouch:true,locale:'zh-CN'});
  touch.on('pageerror',e=>errors.push(e.message));await touch.goto(pathToFileURL(path.join(evidence,'browser-fixture.html')).href);
  await touch.locator('.credit-balance-trigger').first().waitFor();await apply(touch,details());
  await touch.locator('.credit-balance-trigger').first().tap();await touch.getByRole('dialog').waitFor();checks.push('touch tap opens');
  assert.deepEqual(errors,[]);checks.push('no browser runtime errors');
  const result={result:'PASS',checks,metrics,exitStatus:0};fs.writeFileSync(path.join(evidence,'browser-verification.json'),JSON.stringify(result,null,2)+'\n');console.log(JSON.stringify(result,null,2));
 } finally {await browser.close();}
}
run().catch(error=>{console.error(error);process.exitCode=1;});
