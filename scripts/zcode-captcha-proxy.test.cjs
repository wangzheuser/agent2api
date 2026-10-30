const assert = require('node:assert/strict');
const net = require('node:net');
const { once } = require('node:events');
const test = require('node:test');
const { browserProxy } = require(process.env.CAPTCHA_PROXY_MODULE || '../runtime/zcode-captcha/proxy.cjs');

async function listen(server) {
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  return server.address().port;
}

function reader(socket) {
  let buffered = Buffer.alloc(0);
  const waiters = [];
  function drain() {
    while (waiters.length && buffered.length >= waiters[0].size) {
      const { size, resolve } = waiters.shift();
      resolve(buffered.subarray(0, size));
      buffered = buffered.subarray(size);
    }
  }
  socket.on('data', data => { buffered = Buffer.concat([buffered, data]); drain(); });
  socket.on('error', () => {});
  return size => new Promise(resolve => { waiters.push({ size, resolve }); drain(); });
}

test('captcha proxy preserves existing direct HTTP and unauthenticated SOCKS routes', async () => {
  for (const proxy of [null, { server: 'http://127.0.0.1:9', username: 'fixture', password: 'fixture' },
    { server: 'socks5://127.0.0.1:9' }]) {
    const route = await browserProxy(proxy);
    assert.equal(route.proxy, proxy);
    await route.close();
  }
});

test('captcha authenticated SOCKS bridge preserves credentials destination bytes and cleanup', { timeout: 10000 }, async t => {
  const clients = new Set();
  const upstream = net.createServer(socket => {
    clients.add(socket);
    socket.once('close', () => clients.delete(socket));
    const read = reader(socket);
    void (async () => {
      assert.deepEqual(await read(3), Buffer.from([5, 1, 2]));
      socket.write(Buffer.from([5]));
      setImmediate(() => socket.write(Buffer.from([2])));
      assert.deepEqual(await read(2), Buffer.from([1, 7]));
      assert.equal((await read(7)).toString(), 'fixture');
      assert.equal((await read(1))[0], 8);
      assert.equal((await read(8)).toString(), 'password');
      socket.write(Buffer.from([1, 0]));
      const connect = await read(11);
      assert.deepEqual(connect, Buffer.from([5, 1, 0, 3, 4, 116, 101, 115, 116, 1, 187]));
      socket.write(Buffer.from([5, 0, 0, 1, 127, 0, 0, 1, 1, 187]));
      socket.write(await read(4));
    })().catch(error => { socket.destroy(); t.diagnostic(error.message); });
  });
  const port = await listen(upstream);
  const route = await browserProxy({
    server: `socks5://127.0.0.1:${port}`, username: 'fixture', password: 'password',
    bypass: '127.0.0.1,localhost',
  });
  t.after(async () => {
    await route.close();
    for (const socket of clients) socket.destroy();
    await new Promise(resolve => upstream.close(resolve));
  });
  assert.equal(route.proxy.username, undefined);
  assert.equal(route.proxy.password, undefined);
  assert.equal(route.proxy.bypass, '127.0.0.1,localhost');
  const client = net.connect(Number(new URL(route.proxy.server).port), '127.0.0.1');
  const read = reader(client);
  const closed = once(client, 'close');
  await once(client, 'connect');
  client.write(Buffer.from([5]));
  client.write(Buffer.from([1, 0]));
  assert.deepEqual(await read(2), Buffer.from([5, 0]));
  client.write(Buffer.from([5, 1, 0, 3, 4, 116, 101, 115, 116, 1, 187]));
  assert.deepEqual(await read(10), Buffer.from([5, 0, 0, 1, 127, 0, 0, 1, 1, 187]));
  client.write(Buffer.from('ping'));
  assert.equal((await read(4)).toString(), 'ping');
  await route.close();
  await closed;
  route.close = async () => {};
});

test('captcha SOCKS bridge rejects invalid credential lengths before listening', async () => {
  await assert.rejects(browserProxy({ server: 'socks5://127.0.0.1:9', username: 'x'.repeat(256), password: 'fixture' }),
    { message: 'proxy_credentials_invalid' });
});

test('captcha SOCKS authentication rejection closes clients without forwarding CONNECT', { timeout: 10000 }, async t => {
  let forwarded = false;
  const upstream = net.createServer(socket => {
    const read = reader(socket);
    void (async () => {
      await read(3);
      socket.write(Buffer.from([5, 2]));
      await read(19);
      socket.on('data', () => { forwarded = true; });
      socket.write(Buffer.from([1, 1]));
    })().catch(() => socket.destroy());
  });
  const port = await listen(upstream);
  const route = await browserProxy({
    server: `socks5://127.0.0.1:${port}`, username: 'fixture', password: 'incorrect',
  });
  t.after(async () => {
    await route.close();
    await new Promise(resolve => upstream.close(resolve));
  });
  const client = net.connect(Number(new URL(route.proxy.server).port), '127.0.0.1');
  const closed = once(client, 'close');
  await once(client, 'connect');
  client.write(Buffer.from([5, 1, 0]));
  await closed;
  assert.equal(forwarded, false);
});
