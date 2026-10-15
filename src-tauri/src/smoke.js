(async () => {
  const checks = [], check = (name, ok) => checks.push({name, ok: !!ok});
  try {
    check('bundled interface', document.querySelector('.brand-text')?.textContent.includes('夸克网盘'));
    check('native bridge', !!window.qk && !!window.__TAURI__);
    check('quit action available', !!document.getElementById('btn-quit') && typeof window.qk.app.quit === 'function');
    check('visible interface', document.querySelector('.sidebar')?.getBoundingClientRect().width > 100 && document.querySelector('.topbar')?.getBoundingClientRect().height > 0);
    const settings = await window.qk.settings.get();
    check('Rust settings command', typeof settings.downloadDir === 'string');
    const changed = await window.qk.settings.set({theme: 'dark'});
    check('Rust settings persist', changed.theme === 'dark' && (await window.qk.settings.get()).theme === 'dark');
    await window.qk.settings.set({theme: settings.theme});
    const auth = await window.qk.auth.status();
    check('Rust auth status', typeof auth.loggedIn === 'boolean');
    const tasks = await window.qk.transfer.list();
    check('Rust persisted tasks', Array.isArray(tasks));
    let code; try { await window.qk.fs.list({pdirFid:'0'}); } catch (e) { code = e.code; }
    check('NEED_LOGIN error bridge', auth.loggedIn || code === 'NEED_LOGIN');
    document.querySelector('[data-view="settings"]').click();
    check('settings view', !document.querySelector('#view-settings').hidden);
    document.querySelector('[data-view="transfers"]').click();
    check('transfers view', !document.querySelector('#view-transfers').hidden);
    document.querySelector('[data-view="files"]').click();
    check('no mock data', !location.search.includes('mock=1'));
  } catch(e) { checks.push({name:'exception',ok:false,code:e.code || 'JS',message:String(e.message).slice(0,180)}); }
  await window.__TAURI__.core.invoke('smoke_report', {report:{checks, passed: checks.every(c => c.ok), runtime:'Tauri 2 / system WebView / Rust', userAgent:navigator.userAgent}});
})();
