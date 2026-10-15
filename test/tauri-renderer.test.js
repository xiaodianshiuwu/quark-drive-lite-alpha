'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { range } = require('../src/renderer/virtual-list');
const source = fs.readFileSync(path.join(__dirname, '../src/renderer/tauri-bridge.js'), 'utf8');
const plain = value => JSON.parse(JSON.stringify(value));

function bridge(invoke, listen = async () => () => {}) {
  const window = { navigator: { platform: 'MacIntel' }, __TAURI__: { core: { invoke }, event: { listen } } };
  vm.runInNewContext(source, { window });
  return window.qk;
}

test('Tauri bridge preserves the renderer contract and named scalar parameters', async () => {
  const calls = [];
  const qk = bridge(async (command, input) => { calls.push([command, plain(input)]); return 'ok'; });
  await qk.fs.info('file-id');
  await qk.fs.crumbs('dir-id');
  await qk.transfer.pause('task-id');
  await qk.transfer.clear(['a', 'b']);
  await qk.settings.set({ downloadParallel: 6 });
  await qk.dialog.openPath('/fixture/file');
  await qk.dialog.saveDir('/fixture');
  await qk.auth.login();
  assert.deepEqual(calls, [
    ['qk_call', { method: 'fs.info', params: { fid: 'file-id' } }],
    ['qk_call', { method: 'fs.crumbs', params: { fid: 'dir-id' } }],
    ['qk_call', { method: 'transfer.pause', params: { id: 'task-id' } }],
    ['qk_call', { method: 'transfer.clear', params: { ids: ['a', 'b'] } }],
    ['qk_call', { method: 'settings.set', params: { downloadParallel: 6 } }],
    ['qk_call', { method: 'dialog.openPath', params: { path: '/fixture/file' } }],
    ['qk_call', { method: 'dialog.saveDir', params: { defaultPath: '/fixture' } }],
    ['qk_call', { method: 'auth.login', params: {} }]
  ]);
  assert.equal(qk.platform, 'darwin');
  assert.equal(qk.dialog.getPathForFile({ path: '/untrusted' }), '');
});

test('Tauri rejects with NEED_LOGIN intact rather than losing its code', async () => {
  const qk = bridge(async () => { throw { code: 'NEED_LOGIN', message: '会话过期', httpStatus: 401 }; });
  await assert.rejects(qk.auth.verify(), error => error.code === 'NEED_LOGIN' && error.httpStatus === 401);
});

test('Tauri listener disposal before asynchronous registration closes the listener', async () => {
  let resolve;
  let stopped = 0;
  let handler;
  let received = 0;
  const qk = bridge(async () => {}, async (name, cb) => {
    assert.equal(name, 'qk:auth-changed');
    handler = cb;
    await new Promise(r => { resolve = r; });
    return () => { stopped++; };
  });
  const off = qk.auth.onChange(() => { received++; });
  off();
  handler({ payload: { loggedIn: true } });
  resolve();
  await new Promise(r => setImmediate(r));
  assert.equal(stopped, 1);
  assert.equal(received, 0);
});

test('virtual rows remain bounded across a million items and preserve complete scroll extent', () => {
  for (const count of [0, 1, 100, 5001, 1000000]) {
    for (const top of [0, 100, count * 23, count * 46, Number.MAX_SAFE_INTEGER]) {
      const r = range(count, top, 720, 46, 8);
      assert.ok(r.start >= 0 && r.end <= count && r.start <= r.end);
      assert.ok(r.end - r.start <= Math.ceil(720 / 46) + 17);
      assert.equal(r.before + (r.end - r.start) * 46 + r.after, count * 46);
    }
  }
  assert.equal(range(5001, 5001 * 46, 720, 46, 8).end, 5001);
});

// Launch only a fresh project-specific headless browser profile, never the user's browser session.
const browserPath = process.env.QCK_BROWSER_PATH;
test('renderer DOM: 5,100 entries, scrolling, retained selection, paging and search', { skip: !browserPath, timeout: 45000 }, async t => {
  const os = require('node:os');
  const { spawn } = require('node:child_process');
  const { pathToFileURL } = require('node:url');
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'qck-tauri-renderer-'));
  const child = spawn(browserPath, ['--headless', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
    '--remote-debugging-port=0', '--remote-debugging-address=127.0.0.1', '--window-size=1280,900',
    '--user-data-dir=' + profile, 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] });
  const deadline = Date.now() + 15000;
  let socket;
  t.after(async () => {
    if (socket) socket.close();
    child.kill('SIGTERM');
    await new Promise(resolve => { if (child.exitCode !== null) resolve(); else child.once('exit', resolve); });
    fs.rmSync(profile, { recursive: true, force: true });
  });
  const portFile = path.join(profile, 'DevToolsActivePort');
  while (!fs.existsSync(portFile)) {
    if (Date.now() > deadline) throw new Error('Headless browser did not expose a project debug port');
    await new Promise(r => setTimeout(r, 50));
  }
  const port = fs.readFileSync(portFile, 'utf8').split('\n')[0];
  const targets = await (await fetch('http://127.0.0.1:' + port + '/json/list')).json();
  socket = new WebSocket(targets.find(item => item.type === 'page').webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { socket.addEventListener('open', resolve, { once: true }); socket.addEventListener('error', reject, { once: true }); });
  const pending = new Map();
  let seq = 0;
  socket.addEventListener('message', event => {
    const message = JSON.parse(event.data);
    if (!message.id || !pending.has(message.id)) return;
    const { resolve, reject } = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) reject(new Error(message.error.message)); else resolve(message.result);
  });
  function cdp(method, params = {}) {
    return new Promise((resolve, reject) => { const id = ++seq; pending.set(id, { resolve, reject }); socket.send(JSON.stringify({ id, method, params })); });
  }
  async function evaluate(expression) {
    const r = await cdp('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + ': ' + (r.exceptionDetails.exception?.description || ''));
    return r.result.value;
  }
  await cdp('Page.enable');
  await cdp('Page.addScriptToEvaluateOnNewDocument', { source: `
    window.__fixtureCalls = [];
    window.__TAURI__ = {
      event: { listen: async () => () => {} },
      core: { invoke: async (command, request) => {
        window.__fixtureCalls.push(request);
        const p = request.params;
        const item = n => ({fid:String(n), name:'fixture-'+n+'.txt', dir:false, size:n, updatedAt:1, ext:'txt'});
        switch (request.method) {
          case 'auth.status': return {loggedIn:true, expired:false, nickname:'Fixture'};
          case 'auth.quota': return {total:1000000, used:1, free:999999};
          case 'settings.get': return {downloadDir:'/fixture',uploadConcurrency:2,downloadConcurrency:2,downloadParallel:4,uploadPartConcurrency:2,theme:'light'};
          case 'fs.crumbs': return [{fid:'0',name:'全部文件'}];
          case 'fs.list': {
            const start=(p.page-1)*p.size;
            return {list:Array.from({length:Math.min(p.size,5100-start)},(_,i)=>item(start+i+1)),total:5100,page:p.page,size:p.size,hasMore:start+p.size<5100};
          }
          case 'fs.search': return {list:[item(7000)],total:1,page:1,size:p.size,hasMore:false};
          case 'transfer.list': return [];
          default: throw {code:'FIXTURE_UNEXPECTED',message:request.method};
        }
      }}
    };` });
  await cdp('Page.navigate', { url: pathToFileURL(path.join(__dirname, '../src/renderer/index.html')).href });
  while (!(await evaluate('!!(window.QKUI && QKUI.state.loadedOnce && !QKUI.state.loading)'))) {
    if (Date.now() > deadline) throw new Error('Renderer did not finish initial fixture load');
    await new Promise(r => setTimeout(r, 30));
  }
  assert.ok(await evaluate('QKAPI.injected && QKAPI.bridgeInfo().missing.length === 0'));
  await evaluate('(async () => { while (QKUI.state.hasMore) await QKUI.loadPage({}); })()');
  assert.equal(await evaluate('QKUI.state.items.length'), 5100);
  assert.ok(await evaluate('QKUI.getRowCount() <= 40 && document.querySelectorAll(".row").length <= 40'));
  await evaluate('document.querySelector(".row input").click()');
  assert.ok(await evaluate('QKUI.state.selected.has("1")'));
  await evaluate('document.getElementById("listwrap").scrollTop=46*2500');
  await new Promise(r => setTimeout(r, 70));
  assert.ok(await evaluate('Number(document.querySelector(".row").dataset.fid)>2400 && QKUI.getRowCount()<=40'));
  await evaluate('document.getElementById("listwrap").scrollTop=46*5100');
  await new Promise(r => setTimeout(r, 70));
  assert.ok(await evaluate('!!document.querySelector(".row[data-fid=\\"5100\\"]") && QKUI.getRowCount()<=40'));
  await evaluate('document.getElementById("listwrap").scrollTop=0');
  await new Promise(r => setTimeout(r, 70));
  assert.ok(await evaluate('document.querySelector(".row[data-fid=\\"1\\"] input").checked'));
  await evaluate('document.getElementById("check-all").click()');
  assert.equal(await evaluate('QKUI.state.selected.size'), 5100);
  await evaluate('document.getElementById("search-input").value="fixture";document.getElementById("search-input").dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}))');
  await new Promise(r => setTimeout(r, 70));
  assert.equal(await evaluate('QKUI.state.items[0].fid'), '7000');
  assert.equal(await evaluate('QKUI.state.selected.size'), 0);
  assert.equal(await evaluate('QKUI.getRowCount()'), 1);
});
