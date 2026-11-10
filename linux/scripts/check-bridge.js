#!/usr/bin/env node
// End-to-end check of the loopback bridge, without Zalo's app.asar:
//   node linux/scripts/check-bridge.js <port> <asarPath>
// Exercises the same calls `HostScripts/*.js` make: sync members, IPC channels,
// crypto primitives and the asar-served pages.
'use strict';
const assert = require('node:assert');
const http = require('node:http');

const [port, asar] = process.argv.slice(2);
if (!port) {
  console.error('usage: check-bridge.js <port> [asarPath]');
  process.exit(2);
}

const request = (path, body) =>
  new Promise((resolve, reject) => {
    const payload = JSON.stringify(body);
    const req = http.request(
      { host: '127.0.0.1', port, path, method: 'POST', headers: { 'content-type': 'application/json', 'content-length': Buffer.byteLength(payload) } },
      (res) => {
        let data = '';
        res.on('data', (c) => (data += c));
        res.on('end', () => resolve({ status: res.statusCode, body: data }));
      },
    );
    req.on('error', reject);
    req.end(payload);
  });

const get = (path) =>
  new Promise((resolve, reject) => {
    http
      .get({ host: '127.0.0.1', port, path }, (res) => {
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => resolve({ status: res.statusCode, body: Buffer.concat(chunks) }));
      })
      .on('error', reject);
  });

const sync = async (payload) => {
  const res = await request('/sync', payload);
  return JSON.parse(res.body);
};

let passed = 0;
const check = (name, fn) => fn().then((detail) => { passed++; console.log(`✓ ${name}${detail ? ' — ' + detail : ''}`); });

(async () => {
  await check('getPathSync(userData)', async () => {
    const { value } = await sync({ path: '$zelectron.getPathSync', args: ['userData'] });
    assert.ok(typeof value === 'string' && value.length > 0);
    return value;
  });

  await check('electron-store ghi/đọc đồng bộ', async () => {
    await sync({ path: '$zelectron.storeWrite', args: ['__check', { at: 42 }] });
    const { value } = await sync({ path: '$zelectron.storeRead', args: ['__check'] });
    assert.strictEqual(value.at, 42);
  });

  await check('sha256("olaz")', async () => {
    const { value: handle } = await sync({ path: '$znode.crypto.__hashCreate', args: ['sha256'] });
    await sync({ path: '$znode.crypto.__hashUpdate', args: [handle, 'olaz'] });
    const { value } = await sync({ path: '$znode.crypto.__hashDigest', args: [handle, 'hex'] });
    assert.match(value, /^[0-9a-f]{64}$/);
    return value.slice(0, 16) + '…';
  });

  await check('AES-256-CBC roundtrip', async () => {
    const key = Buffer.alloc(32, 7).toString('base64');
    const iv = Buffer.alloc(16, 3).toString('base64');
    const { value: h } = await sync({ path: '$znode.crypto.__cipherCreate', args: ['cipher', 'aes-256-cbc', { __bytes: key }, { __bytes: iv }] });
    await sync({ path: '$znode.crypto.__cipherUpdate', args: [h, { __bytes: Buffer.from('olaz').toString('base64') }] });
    const enc = (await sync({ path: '$znode.crypto.__cipherFinal', args: [h, 'base64'] })).value;
    const { value: d } = await sync({ path: '$znode.crypto.__cipherCreate', args: ['decipher', 'aes-256-cbc', { __bytes: key }, { __bytes: iv }] });
    await sync({ path: '$znode.crypto.__cipherUpdate', args: [d, { __bytes: enc }] });
    const dec = (await sync({ path: '$znode.crypto.__cipherFinal', args: [d, 'utf8'] })).value;
    assert.strictEqual(Buffer.from(dec.__bytes, 'base64').toString(), 'olaz');
  });

  await check('fs.statSync thư mục dữ liệu', async () => {
    const { value: home } = await sync({ path: '$zelectron.getPathSync', args: ['userData'] });
    const { value } = await sync({ path: '$znode.fs.statSync', args: [home] });
    assert.strictEqual(value.__stats, true);
    assert.strictEqual(value.isDirectory, true);
  });

  await check('fs write/read', async () => {
    const dir = '/tmp/olaz-check';
    await sync({ path: '$znode.fsExtra.ensureDirSync', args: [dir] });
    await sync({ path: '$znode.fs.writeFileSync', args: [`${dir}/a.txt`, { __bytes: Buffer.from('xin chào').toString('base64') }] });
    const { value } = await sync({ path: '$znode.fs.readFileSync', args: [`${dir}/a.txt`] });
    assert.strictEqual(Buffer.from(value.__bytes, 'base64').toString(), 'xin chào');
  });

  await check('missing file → ENOENT', async () => {
    const { error } = await sync({ path: '$znode.fs.readFileSync', args: ['/tmp/olaz-check/không-có'] });
    assert.match(error || '', /^ENOENT:/);
  });

  await check('zsafe-storage encrypt/decrypt', async () => {
    const { value: encrypted } = await sync({ channel: 'zsafestorage-execute', args: ['encryptString', 'bí mật'] });
    const { value: plain } = await sync({ channel: 'zsafestorage-execute', args: ['decryptString', encrypted] });
    assert.strictEqual(plain, 'bí mật');
    await sync({ channel: 'zsafestorage-execute', args: ['set', 'k', 'v'] });
    const { value } = await sync({ channel: 'zsafestorage-execute', args: ['get', 'k'] });
    assert.strictEqual(value, 'v');
  });

  await check('SQLite bị tắt (IndexedDB mode)', async () => {
    const { value } = await sync({ channel: 'sqlite-available', args: [] });
    assert.strictEqual(value, false);
    const { value: failed } = await sync({ channel: 'fail-load-sqlite3', args: [] });
    assert.strictEqual(failed, true);
  });

  await check('kênh lạ trả undefined, không treo', async () => {
    const { value } = await sync({ channel: '__khong_ton_tai', args: [] });
    assert.strictEqual(value.__undefined, true);
  });

  await check('zproto encrypt/decrypt', async () => {
    const secret = Buffer.from('token-thiết-bị').toString('base64');
    const { value: encrypted } = await sync({ channel: 'zproto-encrypt-with-keychain', args: ['device', [{ __bytes: secret }]] });
    assert.strictEqual(encrypted.length, 1);
    const { value: plain } = await sync({ channel: 'zproto-decrypt-with-keychain', args: ['device', encrypted] });
    assert.strictEqual(Buffer.from(plain[0].__bytes, 'base64').toString(), 'token-thiết-bị');
  });

  await check('trang chẩn đoán nội bộ', async () => {
    const index = await get('/__olaz/index.html');
    assert.strictEqual(index.status, 200);
    assert.match(index.body.toString(), /__zaloBridge/);
  });

  if (asar) {
    await check('renderer phục vụ từ app.asar', async () => {
      const res = await get('/app/login.html');
      assert.strictEqual(res.status, 200);
      assert.match(res.body.toString(), /Zalo/);
      const script = await get('/app/main.js').catch(() => ({ status: 0 }));
      return `login.html → ${res.status}, main.js → ${script.status}`;
    });
  }

  console.log(`\n${passed} kiểm tra đạt`);
})().catch((err) => {
  console.error('✗ ' + err.message);
  process.exit(1);
});
