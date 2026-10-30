const net = require('node:net');
const { once } = require('node:events');

function readBytes(socket, size) {
  return new Promise((resolve, reject) => {
    function cleanup() {
      socket.off('readable', read);
      socket.off('error', fail);
      socket.off('close', fail);
      socket.off('end', fail);
    }
    function fail() {
      cleanup();
      reject(new Error('proxy_handshake_failed'));
    }
    function read() {
      const data = socket.read(size);
      if (data !== null) {
        cleanup();
        resolve(data);
      } else if (socket.destroyed || socket.readableEnded) {
        fail();
      }
    }
    socket.on('readable', read);
    socket.once('error', fail);
    socket.once('close', fail);
    socket.once('end', fail);
    read();
  });
}

// Chromium 不支持 SOCKS5 认证；仅在回环端口替它握手，CONNECT/TLS 原样交给既定出口。
async function browserProxy(proxy) {
  if (!proxy || !proxy.server.startsWith('socks5:') || !(proxy.username || proxy.password)) {
    return { proxy, close: async () => {} };
  }
  const target = new URL(proxy.server);
  const username = Buffer.from(proxy.username || '');
  const password = Buffer.from(proxy.password || '');
  if (!username.length || username.length > 255 || !password.length || password.length > 255) {
    throw new Error('proxy_credentials_invalid');
  }
  const sockets = new Set();
  function track(socket) {
    sockets.add(socket);
    socket.once('close', () => sockets.delete(socket));
    socket.setTimeout(15000, () => socket.destroy());
    return socket;
  }
  const server = net.createServer(client => {
    track(client);
    let upstream;
    function stop() {
      client.destroy();
      if (upstream) upstream.destroy();
    }
    client.on('error', stop);
    client.once('close', () => upstream?.destroy());
    void (async () => {
      const greeting = await readBytes(client, 2);
      if (greeting[0] !== 5 || !greeting[1]) throw new Error('proxy_client_invalid');
      const methods = await readBytes(client, greeting[1]);
      if (!methods.includes(0)) throw new Error('proxy_client_auth_invalid');
      upstream = track(net.connect({
        host: target.hostname.replace(/^\[|\]$/g, ''),
        port: Number(target.port || 1080),
      }));
      upstream.on('error', stop);
      upstream.once('close', () => client.destroy());
      await once(upstream, 'connect');
      upstream.write(Buffer.from([5, 1, 2]));
      const method = await readBytes(upstream, 2);
      if (method[0] !== 5 || method[1] !== 2) throw new Error('proxy_auth_method_rejected');
      upstream.write(Buffer.concat([
        Buffer.from([1, username.length]), username, Buffer.from([password.length]), password,
      ]));
      const auth = await readBytes(upstream, 2);
      if (auth[0] !== 1 || auth[1] !== 0) throw new Error('proxy_auth_rejected');
      client.write(Buffer.from([5, 0]));
      client.setTimeout(0);
      upstream.setTimeout(0);
      client.pipe(upstream);
      upstream.pipe(client);
    })().catch(stop);
  });
  server.maxConnections = 64;
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  return {
    proxy: { server: `socks5://127.0.0.1:${server.address().port}`, bypass: proxy.bypass },
    close: async () => {
      const closed = new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
      for (const socket of sockets) socket.destroy();
      await closed;
    },
  };
}

module.exports = { browserProxy };
