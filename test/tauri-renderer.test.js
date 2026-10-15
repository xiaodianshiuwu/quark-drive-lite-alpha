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
  await qk.app.quit();
  assert.deepEqual(calls, [
    ['qk_call', { method: 'fs.info', params: { fid: 'file-id' } }],
    ['qk_call', { method: 'fs.crumbs', params: { fid: 'dir-id' } }],
    ['qk_call', { method: 'transfer.pause', params: { id: 'task-id' } }],
    ['qk_call', { method: 'transfer.clear', params: { ids: ['a', 'b'] } }],
    ['qk_call', { method: 'settings.set', params: { downloadParallel: 6 } }],
    ['qk_call', { method: 'dialog.openPath', params: { path: '/fixture/file' } }],
    ['qk_call', { method: 'dialog.saveDir', params: { defaultPath: '/fixture' } }],
    ['qk_call', { method: 'auth.login', params: {} }],
    ['qk_call', { method: 'app.quit', params: {} }]
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
test('renderer DOM: virtual paging, file operations, transfers, settings and account lifecycle', { skip: !browserPath, timeout: 45000 }, async t => {
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
  async function until(expression, label) {
    const expires = Date.now() + 5000;
    while (!(await evaluate(expression))) {
      if (Date.now() >= expires) throw new Error('Renderer fixture timed out: ' + label);
      await new Promise(r => setTimeout(r, 25));
    }
  }
  async function click(selector) {
    await until('!!document.querySelector(' + JSON.stringify(selector) + ')', selector);
    await evaluate('document.querySelector(' + JSON.stringify(selector) + ').click()');
  }
  await cdp('Page.enable');
  await cdp('Page.addScriptToEvaluateOnNewDocument', { source: `
    window.__fixtureCalls = [];
    window.__fixtureListeners = {};
    window.__fixtureEmit = (name, payload) => (window.__fixtureListeners[name] || []).forEach(cb => cb({payload:structuredClone(payload)}));
    window.__fixture = {
      account:{loggedIn:true,expired:false,nickname:'Fixture',userId:'fixture-user'},
      settings:{downloadDir:'/fixture',uploadConcurrency:2,downloadConcurrency:2,downloadParallel:16,downloadStrategy:'auto',uploadPartConcurrency:3,theme:'light'},
      tasks:[], names:{}, serial:0
    };
    window.__TAURI__ = {
      event: { listen: async (name, cb) => {
        (window.__fixtureListeners[name] ||= []).push(cb);
        return () => { window.__fixtureListeners[name] = window.__fixtureListeners[name].filter(fn => fn !== cb); };
      } },
      core: { invoke: async (command, request) => {
        window.__fixtureCalls.push(request);
        const p = request.params;
        const fixture = window.__fixture;
        const item = n => ({fid:String(n), name:fixture.names[n] || 'fixture-'+n+'.txt', dir:false, size:n, updatedAt:1, ext:'txt'});
        const snapshot = value => structuredClone(value);
        const emitTasks = () => window.__fixtureEmit('qk:transfer-update', fixture.tasks);
        switch (request.method) {
          case 'auth.status': case 'auth.verify': return snapshot(fixture.account);
          case 'auth.login': fixture.account={loggedIn:true,expired:false,nickname:'Fixture',userId:'fixture-user'};window.__fixtureEmit('qk:auth-changed',fixture.account);return snapshot(fixture.account);
          case 'auth.logout': fixture.account={loggedIn:false,expired:false};window.__fixtureEmit('qk:auth-changed',fixture.account);return snapshot(fixture.account);
          case 'auth.quota': return {total:1000000, used:1, free:999999};
          case 'settings.get': return snapshot(fixture.settings);
          case 'settings.set': Object.assign(fixture.settings,p);return snapshot(fixture.settings);
          case 'fs.crumbs': return [{fid:'0',name:'全部文件'}];
          case 'fs.list': {
            if (fixture.holdPage) return new Promise(resolve => fixture.releasePage=resolve);
            if (!fixture.account.loggedIn) throw {code:'NEED_LOGIN',message:'请先登录'};
            if (fixture.directoryMode) return {list:[{fid:'folder-A',name:'Target A',dir:true},{fid:'folder-B',name:'Target B',dir:true}],total:2,page:1,size:p.size,hasMore:false};
            const start=(p.page-1)*p.size;
            return {list:Array.from({length:Math.min(p.size,5100-start)},(_,i)=>item(start+i+1)),total:5100,page:p.page,size:p.size,hasMore:start+p.size<5100};
          }
          case 'fs.search': return {list:[item(7000)],total:1,page:1,size:p.size,hasMore:false};
          case 'fs.mkdir': case 'fs.move': case 'fs.copy': case 'fs.remove': return {done:true};
          case 'fs.rename': fixture.names[p.fid]=p.name;return {done:true};
          case 'transfer.list': {
            if (fixture.holdTransfers) return new Promise(resolve => fixture.releaseTransfers=resolve);
            return snapshot(fixture.tasks);
          }
          case 'transfer.upload': case 'transfer.download': {
            const upload=request.method==='transfer.upload';
            const paths=upload?p.localPaths:p.fids;
            const ids=paths.map((value,i) => {
              const id='task-'+(++fixture.serial);
              fixture.tasks.push({id,kind:upload?'upload':'download',name:upload?value.split('/').pop():p.names[i],status:'running',size:100,transferred:25,speed:1,eta:75,localPath:upload?value:'/fixture/'+p.names[i],localDir:'/fixture'});
              return id;
            });
            emitTasks();return ids;
          }
          case 'transfer.pause': case 'transfer.resume': case 'transfer.retry': case 'transfer.cancel': {
            fixture.tasks.find(task=>task.id===p.id).status=({'transfer.pause':'paused','transfer.resume':'running','transfer.retry':'running','transfer.cancel':'canceled'})[request.method];
            emitTasks();return null;
          }
          case 'transfer.clear': fixture.tasks=fixture.tasks.filter(task=>!p.ids.includes(task.id));emitTasks();return null;
          case 'transfer.openItem': return null;
          case 'dialog.pickFiles': return ['/fixture/local-upload.bin'];
          case 'dialog.saveDir': return '/fixture';
          case 'dialog.openPath': case 'dialog.showInFolder': case 'app.quit': return null;
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

  await t.test('settings persist upload PUT concurrency, download strategy, connection limit and theme', async () => {
    await click('[data-view="settings"]');
    assert.equal(await evaluate('document.getElementById("set-up-part").value'), '3');
    assert.equal(await evaluate('document.getElementById("set-down-strategy").value'), 'auto');
    for (const [id, value] of [['set-up-part', '5'], ['set-down-parallel', '12'], ['set-down-strategy', 'parallel']]) {
      await evaluate('document.getElementById(' + JSON.stringify(id) + ').value=' + JSON.stringify(value) + ';document.getElementById(' + JSON.stringify(id) + ').dispatchEvent(new Event("change",{bubbles:true}))');
    }
    await click('[data-theme="dark"].seg');
    await until('__fixture.settings.uploadPartConcurrency===5 && __fixture.settings.downloadParallel===12 && __fixture.settings.downloadStrategy==="parallel" && document.documentElement.dataset.theme==="dark"', 'settings saved');
    await click('[data-view="files"]');
  });

  await t.test('rename, mkdir, move, copy and confirmed removal retain their native command parameters', async () => {
    await click('.row [data-op="rename"]');
    await evaluate('document.querySelector(".overlay:not(.overlay-out) .dlg-input").value="renamed.txt"');
    await click('.overlay:not(.overlay-out) .dlg-foot .btn:last-child');
    await until('QKUI.state.items[0].name==="renamed.txt"', 'renamed row');
    assert.deepEqual(await evaluate('__fixtureCalls.find(call=>call.method==="fs.rename").params'), { fid: '7000', name: 'renamed.txt' });

    await click('#search-clear');
    await until('QKUI.state.mode==="browse" && QKUI.state.loadedOnce && !QKUI.state.loading', 'browse after search');
    const beforeMkdir = await evaluate('__fixtureCalls.filter(call=>call.method==="fs.list").length');
    await click('#btn-mkdir');
    await evaluate('document.querySelector(".overlay:not(.overlay-out) .dlg-input").value="unique-test-dir"');
    await click('.overlay:not(.overlay-out) .dlg-foot .btn:last-child');
    await until('__fixtureCalls.filter(call=>call.method==="fs.list").length>' + beforeMkdir + ' && !QKUI.state.loading', 'mkdir refreshed');
    assert.deepEqual(await evaluate('__fixtureCalls.find(call=>call.method==="fs.mkdir").params'), { pdirFid: '0', name: 'unique-test-dir' });

    await evaluate('__fixture.directoryMode=true;document.getElementById("search-input").value="fixture";document.getElementById("search-input").dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}))');
    await until('QKUI.state.items[0]?.fid==="7000" && !QKUI.state.loading', 'search to operate on file');
    for (const [kind, fid] of [['move', 'folder-A'], ['copy', 'folder-B']]) {
      await click('.row [data-op="' + kind + '"]');
      await click('.overlay:not(.overlay-out) .picker-row[data-fid="' + fid + '"]');
      await click('.overlay:not(.overlay-out) .dlg-foot .btn:last-child');
      await until('!!document.querySelector(".overlay:not(.overlay-out) .dlg-confirm")', 'confirm ' + kind);
      await click('.overlay:not(.overlay-out) .dlg-foot .btn:last-child');
      await until('__fixtureCalls.some(call=>call.method==="fs.' + kind + '")', kind + ' submitted');
      assert.deepEqual(await evaluate('__fixtureCalls.find(call=>call.method==="fs.' + kind + '").params'), { fids: ['7000'], toFid: fid });
    }
    await click('.row [data-op="remove"]');
    await click('.overlay:not(.overlay-out) .dlg-foot .btn:first-child');
    assert.equal(await evaluate('__fixtureCalls.filter(call=>call.method==="fs.remove").length'), 0);
    await click('.row [data-op="remove"]');
    await click('.overlay:not(.overlay-out) .dlg-foot .btn:last-child');
    await until('__fixtureCalls.some(call=>call.method==="fs.remove")', 'confirmed removal submitted');
    assert.deepEqual(await evaluate('__fixtureCalls.find(call=>call.method==="fs.remove").params'), { fids: ['7000'] });
  });

  await t.test('download progress, pause/resume/cancel/retry, file reveal and record cleanup', async () => {
    await click('.row [data-op="download"]');
    await until('QKUI.state.transfers.length===1', 'download task');
    assert.deepEqual(await evaluate('__fixtureCalls.find(call=>call.method==="transfer.download").params'), { fids: ['7000'], names: ['renamed.txt'], localDir: '/fixture' });
    await click('[data-view="transfers"]');
    await evaluate('__fixture.tasks[0].transferred=50;__fixtureEmit("qk:transfer-update",__fixture.tasks)');
    assert.equal(await evaluate('document.querySelector(".tbar>i").style.width'), '50%');
    for (const [action, status] of [['pause', 'paused'], ['resume', 'running'], ['cancel', 'canceled']]) {
      await click('.trow [data-tact="' + action + '"]');
      await until('document.querySelector(".trow").dataset.status===' + JSON.stringify(status), action);
    }
    await click('.trow [data-tact="clear"]');
    await until('!document.querySelector(".trow")', 'canceled record cleared');
    await evaluate('QKUI.setView("files")');
    await click('.row [data-op="download"]');
    await until('QKUI.state.transfers.length===1', 'second download');
    await evaluate('__fixture.tasks[0].status="error";__fixture.tasks[0].error="fixture error";__fixtureEmit("qk:transfer-update",__fixture.tasks);QKUI.setView("transfers")');
    await click('.trow [data-tact="retry"]');
    await until('QKUI.state.transfers[0].status==="running"', 'retry running');
    await evaluate('__fixture.tasks[0].status="done";__fixture.tasks[0].transferred=100;__fixtureEmit("qk:transfer-update",__fixture.tasks)');
    await click('.trow [data-tact="open"]');
    await until('__fixtureCalls.some(call=>call.method==="transfer.openItem")', 'native reveal');
    assert.equal(await evaluate('__fixtureCalls.filter(call=>call.method==="transfer.openItem").length'), 1);
    assert.equal(await evaluate('__fixtureCalls.filter(call=>call.method==="dialog.showInFolder").length'), 0);
    await click('#btn-clear-finished');
    await click('.overlay:not(.overlay-out) .dlg-foot .btn:last-child');
    await until('QKUI.state.transfers.length===0', 'done record cleanup');
  });

  await t.test('native file picker and Tauri drag/drop queue uploads in the current directory', async () => {
    await evaluate('QKUI.setView("files");QKUI.navigate("0")');
    await until('QKUI.state.loadedOnce && !QKUI.state.loading', 'upload target loaded');
    await click('#btn-upload');
    await until('QKUI.state.transfers.length===1 && QKUI.state.view==="transfers"', 'picker upload task');
    assert.deepEqual(await evaluate('__fixtureCalls.find(call=>call.method==="transfer.upload").params'), { localPaths: ['/fixture/local-upload.bin'], pdirFid: '0' });
    await evaluate('QKUI.setView("files");__fixtureEmit("tauri://drag-drop",{paths:["/fixture/dropped.bin"]})');
    await until('QKUI.state.transfers.length===2', 'native drop upload task');
    assert.deepEqual(await evaluate('__fixtureCalls.filter(call=>call.method==="transfer.upload")[1].params'), { localPaths: ['/fixture/dropped.bin'], pdirFid: '0' });
  });

  await t.test('a pushed task snapshot wins over an older in-flight task listing', async () => {
    await evaluate('__fixture.holdTransfers=true;QKUI.setView("transfers")');
    await until('typeof __fixture.releaseTransfers==="function"', 'pending task snapshot');
    await evaluate('__fixture.tasks[0].status="paused";__fixtureEmit("qk:transfer-update",__fixture.tasks);__fixture.holdTransfers=false;__fixture.releaseTransfers([])');
    await new Promise(r => setTimeout(r, 30));
    assert.equal(await evaluate('QKUI.state.transfers[0].status'), 'paused');
  });

  await t.test('logout clears account files, selection, quota and invalidates an in-flight page; login reloads', async () => {
    await evaluate('QKUI.setView("files");__fixture.holdPage=true;QKUI.reload()');
    await until('typeof __fixture.releasePage==="function"', 'pending page');
    await click('#btn-logout-2');
    await click('.overlay:not(.overlay-out) .dlg-foot .btn:last-child');
    await until('QKUI.state.account.loggedIn===false', 'logged out');
    assert.equal(await evaluate('QKUI.state.items.length'), 0);
    assert.equal(await evaluate('QKUI.state.selected.size'), 0);
    assert.equal(await evaluate('QKUI.state.quota'), null);
    await evaluate('__fixture.holdPage=false;__fixture.releasePage({list:[{fid:"stale",name:"old account",dir:false}],total:1,page:1,size:100,hasMore:false})');
    await new Promise(r => setTimeout(r, 30));
    assert.equal(await evaluate('QKUI.state.items.length'), 0);
    await click('#account .btn');
    await until('QKUI.state.account.loggedIn && QKUI.state.loadedOnce && !QKUI.state.loading && QKUI.state.items.length===2', 'login reload');
  });
});
