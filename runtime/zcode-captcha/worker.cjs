// Docker 的独立 SDK 宿主。stdin/stdout 只传结构化消息，不接收账号凭据。
const { BrowserHost } = require('./browser.cjs');
const { createInterface } = require('node:readline');
const { readFileSync } = require('node:fs');
const { createServer } = require('node:http');

async function main() {
  const source = readFileSync(process.env.AGENT2API_CAPTCHA_UI_SCRIPT || '/app/ui/aliyun-captcha.js');
  const server = createServer((req, res) => {
    res.setHeader('Cache-Control', 'no-store');
    if (req.url === '/captcha.js') {
      res.setHeader('Content-Type', 'application/javascript');
      res.end(source);
    } else {
      res.setHeader('Content-Type', 'text/html; charset=utf-8');
      res.end('<!doctype html><html lang="zh-CN"><body><script src="/captcha.js"></script></body></html>');
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  let browser;
  let host;
  let context;
  let closing = false;
  async function reset() {
    const old = host;
    host = null;
    browser = null;
    context = null;
    if (old) await old.close().catch(() => {});
  }
  async function close() {
    if (closing) return;
    closing = true;
    input.close();
    await reset();
    server.close();
  }
  process.once('SIGTERM', () => void close());
  const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
  input.once('close', () => void close());
  for await (const line of input) {
    if (closing) break;
    let timer;
    let result;
    try {
      const config = JSON.parse(line);
      if (!config.sceneId || !config.prefix || !config.region) throw new Error('config');
      result = await Promise.race([
        (async () => {
          if (!browser) {
            host = new BrowserHost();
            browser = await host.start();
          }
          if (closing) throw new Error('closed');
          context = await browser.newContext(config.proxy ? {
            proxy: { ...config.proxy, bypass: '127.0.0.1,localhost' },
          } : {});
          const page = await context.newPage();
          await page.goto(`http://127.0.0.1:${server.address().port}/`, { timeout: 10000 });
          const param = await page.evaluate(config => window.wbAliyunCaptcha.mintTraceless(config), config);
          return { param, region: config.region };
        })(),
        new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('deadline')), 50000); }),
      ]);
    } catch (error) {
      // SDK 错误可能包含 proof/URL；只回固定代码。失败后彻底重建浏览器。
      result = { error: String(error.message).includes('[SDK:F001]') ? 'sdk_rejected_F001' : 'sdk_failed' };
      await reset();
    } finally {
      clearTimeout(timer);
      if (context) await context.close().catch(() => {});
      context = null;
    }
    if (!closing) process.stdout.write(JSON.stringify(result) + '\n');
  }
  await close();
}

main().catch(() => { process.stderr.write('captcha_worker_failed\n'); process.exit(1); });
