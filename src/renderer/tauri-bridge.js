'use strict';
/** Tauri 2 adapter for the existing renderer contract; no Node or Electron runtime. */
(function (global) {
  if (global.qk) return; // Keep the original Electron build usable during migration.
  var tauri = global.__TAURI__;
  if (!tauri || !tauri.core || typeof tauri.core.invoke !== 'function') return;

  function call(method, params) {
    return tauri.core.invoke('qk_call', { method: method, params: params || {} }).catch(function (error) {
      // Tauri serializes Rust errors directly. Preserve NEED_LOGIN and protocol codes.
      if (typeof error === 'string') {
        try { error = JSON.parse(error); } catch (_) { error = { message: error }; }
      }
      var descriptor = error && typeof error === 'object' ? error : {};
      throw Object.assign({ code: 'UNKNOWN', message: '操作失败' }, descriptor);
    });
  }
  function method(name, key) {
    return function (value) {
      var params = key ? {} : (value || {});
      if (key && value !== undefined) params[key] = value;
      return call(name, params);
    };
  }
  function listen(name, callback) {
    if (typeof callback !== 'function') return function () {};
    var disposed = false;
    var off = null;
    Promise.resolve(tauri.event.listen(name, function (event) {
      if (disposed) return;
      try { callback(event.payload); } catch (_) { /* Isolate subscriber failures. */ }
    })).then(function (unsubscribe) {
      if (disposed) unsubscribe();
      else off = unsubscribe;
    }).catch(function () { /* A closed window can race event registration. */ });
    return function () { disposed = true; if (off) { off(); off = null; } };
  }
  var platform = /Mac/i.test(global.navigator.platform) ? 'darwin' :
    (/Win/i.test(global.navigator.platform) ? 'win32' : 'linux');
  var api = {
    auth: {}, fs: {}, transfer: {}, dialog: {}, settings: {},
    platform: platform, runtime: 'tauri',
    app: { quit: function () { return call('app.quit'); } },
    onToast: function (cb) { return listen('qk:toast', cb); },
    onTransferUpdate: function (cb) { return listen('qk:transfer-update', cb); }
  };
  ['status', 'verify', 'login', 'logout', 'quota', 'getUserInfo'].forEach(function (name) {
    api.auth[name] = method('auth.' + name);
  });
  api.auth.onChange = function (cb) { return listen('qk:auth-changed', cb); };
  ['list', 'search', 'mkdir', 'rename', 'move', 'copy', 'remove', 'taskStatus'].forEach(function (name) {
    api.fs[name] = method('fs.' + name);
  });
  ['info', 'crumbs'].forEach(function (name) { api.fs[name] = method('fs.' + name, 'fid'); });
  ['list', 'upload', 'download'].forEach(function (name) { api.transfer[name] = method('transfer.' + name); });
  ['pause', 'resume', 'retry', 'cancel', 'openItem'].forEach(function (name) {
    api.transfer[name] = method('transfer.' + name, 'id');
  });
  api.transfer.clear = method('transfer.clear', 'ids');
  api.transfer.onUpdate = api.onTransferUpdate;
  ['pickDirs', 'pickFiles', 'confirm'].forEach(function (name) { api.dialog[name] = method('dialog.' + name); });
  api.dialog.saveDir = method('dialog.saveDir', 'defaultPath');
  ['openPath', 'showInFolder'].forEach(function (name) { api.dialog[name] = method('dialog.' + name, 'path'); });
  // Web File objects intentionally contain no native paths in WKWebView/WebView2.
  // Native Tauri drag/drop supplies paths through a dedicated event instead.
  api.dialog.getPathForFile = function () { return ''; };
  api.dialog.onFilesDropped = function (cb) {
    return listen('tauri://drag-drop', function (payload) {
      if (payload && Array.isArray(payload.paths)) cb(payload.paths);
    });
  };
  api.settings.get = method('settings.get');
  api.settings.set = method('settings.set');
  Object.keys(api).forEach(function (key) {
    if (api[key] && typeof api[key] === 'object') Object.freeze(api[key]);
  });
  global.qk = Object.freeze(api);
})(window);
