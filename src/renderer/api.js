'use strict';
/**
 * src/renderer/api.js —— 桥接层包装（渲染进程）
 *
 * 职责：
 *  1. 把 window.qk（preload 注入）包一层，逐一透出 docs/IPC_CONTRACT.md 第 2~6 节的全部方法。
 *  2. 对缺失的方法做**安全降级**：调用时返回一个带中文文案的拒绝（code: 'NO_BRIDGE'），
 *     而不是在加载期抛异常。这样 UI 在 "window.qk 未注入" 的环境下仍可正常渲染。
 *  3. 统一错误形状：所有失败都归一化为 Error，且保证 err.code / err.message / err.raw 存在。
 *  4. NEED_LOGIN 统一上报：任何接口抛出 err.code === 'NEED_LOGIN' 都会触发 onNeedLogin 订阅，
 *     便于 UI 弹出"登录状态已失效"提示条（不必在每个调用点重复判断）。
 *  5. 内置**默认关闭**的开发假数据模式（?mock=1），用于在普通浏览器中独立验证 UI 交互。
 *
 * 本文件不发起任何网络请求，也不使用任何 Node/Electron API。
 */

(function (global) {
  /** 契约 §2~§6 中列出的方法清单，用于自检与提示。 */
  var CONTRACT = {
    'auth.status': 1, 'auth.verify': 1, 'auth.login': 1, 'auth.logout': 1, 'auth.quota': 1, 'auth.onChange': 1,
    'fs.list': 1, 'fs.search': 1, 'fs.info': 1, 'fs.crumbs': 1, 'fs.mkdir': 1, 'fs.rename': 1,
    'fs.move': 1, 'fs.copy': 1, 'fs.remove': 1, 'fs.taskStatus': 1,
    'transfer.list': 1, 'transfer.upload': 1, 'transfer.download': 1, 'transfer.pause': 1,
    'transfer.resume': 1, 'transfer.retry': 1, 'transfer.cancel': 1, 'transfer.clear': 1, 'transfer.onUpdate': 1,
    'dialog.pickDirs': 1, 'dialog.pickFiles': 1, 'dialog.saveDir': 1, 'dialog.confirm': 1,
    'dialog.openPath': 1, 'dialog.showInFolder': 1,
    'settings.get': 1, 'settings.set': 1
  };

  /* ------------------------------------------------------------------ *
   * 错误
   * ------------------------------------------------------------------ */

  function mkError(code, message, raw) {
    var e = new Error(message || '操作失败');
    e.code = code || 'UNKNOWN';
    if (raw !== undefined) e.raw = raw;
    return e;
  }

  /** 把任意抛出物归一化为 { code, message, raw } 形状的 Error。 */
  function normalizeError(err, fallbackMessage) {
    if (!err) return mkError('UNKNOWN', fallbackMessage || '操作失败');
    if (typeof err === 'string') return mkError('UNKNOWN', err);
    var code = typeof err.code === 'string' && err.code ? err.code : 'UNKNOWN';
    var message = err.message || err.error_info || fallbackMessage || '操作失败';
    if (err instanceof Error) {
      if (!err.code) err.code = code;
      if (!err.message) err.message = message;
      if (err.raw === undefined) err.raw = err.raw;
      return err;
    }
    return mkError(code, message, err.raw !== undefined ? err.raw : err);
  }

  function isNeedLogin(err) {
    return !!err && (err.code === 'NEED_LOGIN' || err.bizCode === 31001);
  }

  /* ------------------------------------------------------------------ *
   * 登录失效广播
   * ------------------------------------------------------------------ */

  var needLoginSubs = [];

  function fireNeedLogin(info) {
    needLoginSubs.slice().forEach(function (cb) {
      try { cb(info || {}); } catch (e) { /* 订阅者异常不得影响调用方 */ }
    });
  }

  function onNeedLogin(cb) {
    if (typeof cb !== 'function') return function () {};
    needLoginSubs.push(cb);
    return function () {
      var i = needLoginSubs.indexOf(cb);
      if (i >= 0) needLoginSubs.splice(i, 1);
    };
  }

  /* ------------------------------------------------------------------ *
   * 桥接层探测
   * ------------------------------------------------------------------ */

  var mockRequested = false;
  try {
    mockRequested = /(^|[?&])mock=1(&|$)/.test(global.location.search || '');
  } catch (e) { mockRequested = false; }

  /** 开发用假数据桥（默认关闭；仅在 URL 带 ?mock=1 时启用）。 */
  var mockBridge = mockRequested && global.QKMOCK ? global.QKMOCK : null;

  var bridge = mockBridge || (global.qk && typeof global.qk === 'object' ? global.qk : null);
  var isMock = !!mockBridge;

  function hasMethod(ns, name) {
    if (!bridge) return false;
    var target = ns ? bridge[ns] : bridge;
    return !!target && typeof target[name] === 'function';
  }

  function bridgeInfo() {
    if (isMock) {
      return { injected: true, mock: true, text: '开发假数据模式（?mock=1）· 未连接真实后端' };
    }
    if (!bridge) {
      return { injected: false, mock: false, text: '未注入桥接层（window.qk 不存在）' };
    }
    var missing = [];
    Object.keys(CONTRACT).forEach(function (path) {
      var parts = path.split('.');
      if (!hasMethod(parts[0], parts[1])) missing.push(path);
    });
    return {
      injected: true,
      mock: false,
      missing: missing,
      text: missing.length
        ? '桥接层已注入，但缺少 ' + missing.length + ' 个方法（' + missing.join('、') + '）'
        : '桥接层已注入，全部 ' + Object.keys(CONTRACT).length + ' 个契约方法可用'
    };
  }

  /* ------------------------------------------------------------------ *
   * 调用包装
   * ------------------------------------------------------------------ */

  function missingError(what) {
    return mkError('NO_BRIDGE', '未注入桥接层，' + what + ' 不可用。请通过 npm start 启动桌面端。');
  }

  /**
   * 透出一个方法。缺失时返回拒绝的 Promise（带中文提示），绝不抛同步异常。
   */
  function expose(ns, name, options) {
    var opts = options || {};
    return function () {
      var args = Array.prototype.slice.call(arguments);
      var target = ns ? (bridge && bridge[ns]) : bridge;
      var fn = target && typeof target[name] === 'function' ? target[name] : null;
      if (!fn) {
        if (opts.silent) return Promise.resolve(opts.fallback);
        return Promise.reject(missingError((ns ? ns + '.' : '') + name + '()'));
      }
      var result;
      try {
        result = fn.apply(target, args);
      } catch (err) {
        var syncErr = normalizeError(err);
        if (isNeedLogin(syncErr)) fireNeedLogin({ source: ns + '.' + name });
        return Promise.reject(syncErr);
      }
      return Promise.resolve(result).catch(function (err) {
        var ne = normalizeError(err);
        if (isNeedLogin(ne)) fireNeedLogin({ source: ns + '.' + name });
        throw ne;
      });
    };
  }

  /**
   * 透出一个订阅方法。返回取消订阅函数；桥接层缺失或订阅失败时返回空操作。
   * candidates 形如 [['transfer','onUpdate'], ['onTransferUpdate'], ['on','qk:transfer-update']]
   */
  function subscribe(candidates, cb) {
    if (typeof cb !== 'function') return function () {};
    for (var i = 0; i < candidates.length; i++) {
      var path = candidates[i];
      var ns = path.length > 1 ? path[0] : null;
      var name = path[path.length - 1];
      var target = ns ? (bridge && bridge[ns]) : bridge;
      if (!target || typeof target[name] !== 'function') continue;
      try {
        var off = target[name](cb);
        return typeof off === 'function' ? off : function () {};
      } catch (e) {
        // 试下一个候选
      }
    }
    return function () {};
  }

  /* ------------------------------------------------------------------ *
   * 对外 API
   * ------------------------------------------------------------------ */

  var QKAPI = {
    isMock: isMock,
    injected: !!bridge,
    CONTRACT: CONTRACT,
    bridgeInfo: bridgeInfo,
    onNeedLogin: onNeedLogin,
    isNeedLogin: isNeedLogin,
    normalizeError: normalizeError,
    has: hasMethod,

    auth: {
      status: expose('auth', 'status'),
      verify: expose('auth', 'verify'),
      login: expose('auth', 'login'),
      logout: expose('auth', 'logout'),
      quota: expose('auth', 'quota', { silent: true, fallback: null }),
      /** 非契约必需，主进程可能提供；缺失时返回 null。 */
      getUserInfo: expose('auth', 'getUserInfo', { silent: true, fallback: null }),
      /** 订阅登录态变化，返回取消订阅函数。 */
      onChange: function (cb) {
        return subscribe([['auth', 'onChange'], ['onAuthChanged'], ['on', 'qk:auth-changed']], cb);
      }
    },

    fs: {
      list: expose('fs', 'list'),
      search: expose('fs', 'search'),
      info: expose('fs', 'info'),
      crumbs: expose('fs', 'crumbs'),
      mkdir: expose('fs', 'mkdir'),
      rename: expose('fs', 'rename'),
      move: expose('fs', 'move'),
      copy: expose('fs', 'copy'),
      remove: expose('fs', 'remove'),
      taskStatus: expose('fs', 'taskStatus')
    },

    transfer: {
      list: expose('transfer', 'list'),
      upload: expose('transfer', 'upload'),
      download: expose('transfer', 'download'),
      pause: expose('transfer', 'pause'),
      resume: expose('transfer', 'resume'),
      retry: expose('transfer', 'retry'),
      cancel: expose('transfer', 'cancel'),
      clear: expose('transfer', 'clear'),
      /** 非契约必需（主进程可能提供）：在资源管理器中定位该任务的文件。 */
      openItem: expose('transfer', 'openItem', { silent: true, fallback: null }),
      onUpdate: function (cb) {
        return subscribe([['transfer', 'onUpdate'], ['onTransferUpdate'], ['on', 'qk:transfer-update']], cb);
      }
    },

    dialog: {
      pickDirs: expose('dialog', 'pickDirs'),
      pickFiles: expose('dialog', 'pickFiles'),
      saveDir: expose('dialog', 'saveDir', { silent: true, fallback: null }),
      confirm: expose('dialog', 'confirm'),
      openPath: expose('dialog', 'openPath'),
      showInFolder: expose('dialog', 'showInFolder'),
      onFilesDropped: function (cb) { return subscribe([['dialog', 'onFilesDropped']], cb); },
      /**
       * 拖拽上传取真实路径（封装 webUtils.getPathForFile）。
       * 桥接层未暴露或返回空时返回 ''，由 UI 提示改用上传按钮。
       */
      getPathForFile: function (file) {
        if (!hasMethod('dialog', 'getPathForFile')) return '';
        try {
          var p = bridge.dialog.getPathForFile(file);
          return typeof p === 'string' ? p : '';
        } catch (e) {
          return '';
        }
      }
    },

    settings: {
      get: expose('settings', 'get'),
      set: expose('settings', 'set')
    },

    /** 主进程 qk:toast 推送订阅。 */
    onToast: function (cb) {
      return subscribe([['onToast'], ['on', 'qk:toast']], cb);
    },
    /** 等价于 transfer.onUpdate，二者有其一即可。 */
    onTransferUpdate: function (cb) {
      return subscribe([['onTransferUpdate'], ['transfer', 'onUpdate'], ['on', 'qk:transfer-update']], cb);
    }
  };

  global.QKAPI = QKAPI;

  /* ------------------------------------------------------------------ *
   * 开发假数据桥（?mock=1）—— 仅在显式开启时构造，默认不产生任何副作用。
   * 不引用任何外部资源；仅用于在浏览器中验证分页、多选、二次确认、传输进度等交互。
   * ------------------------------------------------------------------ */

  if (mockRequested) {
    global.QKMOCK = createMockBridge();
    global.QKAPI = QKAPI;           // 保持引用
    QKAPI.isMock = true;
    QKAPI.injected = true;
    QKAPI.bridgeInfo = function () {
      return { injected: true, mock: true, text: '开发假数据模式（?mock=1）· 未连接真实后端' };
    };
    // 重新绑定到 mock 桥
    rebind(QKAPI, global.QKMOCK);
  } else {
    delete global.QKMOCK;
  }

  /** mock 桥在 QKAPI 之后构造，这里把已导出的方法重新指向 mock 实现。 */
  function rebind(api, qk) {
    bridge = qk;
    api.auth = {
      status: expose('auth', 'status'),
      verify: expose('auth', 'verify'),
      login: expose('auth', 'login'),
      logout: expose('auth', 'logout'),
      quota: expose('auth', 'quota', { silent: true, fallback: null }),
      getUserInfo: expose('auth', 'getUserInfo', { silent: true, fallback: null }),
      onChange: function (cb) { return subscribe([['auth', 'onChange']], cb); }
    };
    api.fs = {
      list: expose('fs', 'list'),
      search: expose('fs', 'search'),
      info: expose('fs', 'info'),
      crumbs: expose('fs', 'crumbs'),
      mkdir: expose('fs', 'mkdir'),
      rename: expose('fs', 'rename'),
      move: expose('fs', 'move'),
      copy: expose('fs', 'copy'),
      remove: expose('fs', 'remove'),
      taskStatus: expose('fs', 'taskStatus')
    };
    api.transfer = {
      list: expose('transfer', 'list'),
      upload: expose('transfer', 'upload'),
      download: expose('transfer', 'download'),
      pause: expose('transfer', 'pause'),
      resume: expose('transfer', 'resume'),
      retry: expose('transfer', 'retry'),
      cancel: expose('transfer', 'cancel'),
      clear: expose('transfer', 'clear'),
      openItem: expose('transfer', 'openItem', { silent: true, fallback: null }),
      onUpdate: function (cb) { return subscribe([['transfer', 'onUpdate']], cb); }
    };
    api.dialog = {
      pickDirs: expose('dialog', 'pickDirs'),
      pickFiles: expose('dialog', 'pickFiles'),
      saveDir: expose('dialog', 'saveDir', { silent: true, fallback: null }),
      confirm: expose('dialog', 'confirm'),
      openPath: expose('dialog', 'openPath'),
      showInFolder: expose('dialog', 'showInFolder'),
      getPathForFile: function (file) {
        if (!qk.dialog || typeof qk.dialog.getPathForFile !== 'function') return '';
        try {
          var p = qk.dialog.getPathForFile(file);
          return typeof p === 'string' ? p : '';
        } catch (e) { return ''; }
      }
    };
    api.settings = { get: expose('settings', 'get'), set: expose('settings', 'set') };
    api.onToast = function (cb) { return subscribe([['onToast']], cb); };
    api.onTransferUpdate = function (cb) { return subscribe([['onTransferUpdate']], cb); };
  }

  /* ================================================================== *
   * 以下为假数据实现（仅在 ?mock=1 时被调用）
   * ================================================================== */

  function createMockBridge() {
    var now = Date.now();
    var items = {};          // fid -> FileItem
    var kids = {};           // pdirFid -> [fid]
    var seq = 0;

    function fid(prefix) { seq += 1; return (prefix || 'm') + '_' + seq.toString(36) + '_' + Math.floor(Math.random() * 1e6).toString(36); }

    function add(name, dir, size, pdirFid, updatedAt, category) {
      var id = fid(dir ? 'd' : 'f');
      var ext = dir ? '' : (name.indexOf('.') >= 0 ? name.split('.').pop().toLowerCase() : '');
      items[id] = {
        fid: id, name: name, dir: !!dir, size: dir ? 0 : size,
        updatedAt: updatedAt, createdAt: updatedAt - 86400000 * 30,
        category: dir ? 0 : (category || 0), ext: ext, pdirFid: String(pdirFid)
      };
      (kids[String(pdirFid)] = kids[String(pdirFid)] || []).push(id);
      return id;
    }

    // 根目录结构
    var folders = ['我的文档', '图片', '视频素材', '项目资料', '安装包', '大量文件测试'];
    var folderIds = {};
    folders.forEach(function (n, i) {
      folderIds[n] = add(n, true, 0, '0', now - (i + 1) * 3600000);
    });

    // 根目录散落文件 + 分页用的大量文件
    var extPool = [
      ['pdf', 3], ['docx', 3], ['xlsx', 3], ['pptx', 3], ['txt', 3], ['md', 3],
      ['jpg', 2], ['png', 2], ['zip', 5], ['rar', 5], ['mp4', 1], ['mov', 1], ['mp3', 4], ['exe', 0], ['js', 3]
    ];
    function pickExt(i) { return extPool[i % extPool.length]; }

    var rootFiles = 180;
    for (var i = 0; i < rootFiles; i++) {
      var e = pickExt(i);
      var size = (i % 7 === 0) ? 1024 * 1024 * (3 + (i % 40)) : 8137 * (i + 3);
      add('示例文件_' + pad(i + 1, 3) + '.' + e[0], false, size, '0', now - i * 5400000, e[1]);
    }

    // 大量文件测试目录：验证无限滚动 + 大批量渲染
    var big = folderIds['大量文件测试'];
    for (var j = 0; j < 680; j++) {
      var e2 = pickExt(j + 3);
      add('批量测试_' + pad(j + 1, 4) + '.' + e2[0], false, 4096 * (j + 1), big, now - j * 600000, e2[1]);
    }
    // 其它目录放少量内容
    [['我的文档', ['季度总结.docx', '会议纪要.md', '合同扫描件.pdf']],
     ['图片', ['封面.jpg', '截图_001.png', '素材_002.jpg']],
     ['视频素材', ['片段_A.mp4', '片段_B.mov']],
     ['项目资料', ['需求说明.pdf', '架构图.png', '数据.zip']],
     ['安装包', ['toolkit-setup.exe', 'runtime.zip']]
    ].forEach(function (pair) {
      var pid = folderIds[pair[0]];
      pair[1].forEach(function (n, k) {
        var e3 = pickExt(k + 1);
        add(n, false, 1024 * 512 * (k + 1) + 7777, pid, now - k * 7200000, e3[1]);
      });
    });

    function pad(n, w) { var s = String(n); while (s.length < w) s = '0' + s; return s; }

    /* ---------- 账号 ---------- */
    var account = {
      loggedIn: true, expired: false, nickname: '演示账号',
      avatar: '', memberLabel: '普通用户', userId: 'mock-user-0001'
    };
    var accountSubs = [];
    function emitAccount() {
      var snap = JSON.parse(JSON.stringify(account));
      accountSubs.slice().forEach(function (cb) { try { cb(snap); } catch (e) {} });
    }
    var quotaInfo = { total: 10 * 1024 * 1024 * 1024 * 1024, used: 136 * 1024 * 1024 * 1024 };

    /* ---------- 设置 ---------- */
    var settings = {
      downloadDir: 'C:\\Users\\Public\\Downloads',
      uploadConcurrency: 2,
      downloadConcurrency: 2,
      theme: 'light'
    };

    /* ---------- 异步任务（mkdir/move/copy/remove 受理后延迟完成） ---------- */
    var asyncTasks = {};
    function acceptAsync(applyFn) {
      var id = fid('task');
      asyncTasks[id] = { status: 0, done: false };
      setTimeout(function () {
        try { applyFn(); asyncTasks[id] = { status: 1, done: true }; }
        catch (e) { asyncTasks[id] = { status: 2, done: true }; }
      }, 900 + Math.random() * 600);
      return { taskId: id, done: false };
    }

    /* ---------- 传输任务 ---------- */
    var transfers = [];
    var transferSubs = [];
    var MB = 1024 * 1024;

    function newTask(kind, name, size, extra) {
      var t = {
        id: fid('t'), kind: kind, name: name, size: size, transferred: 0,
        status: 'running', error: undefined, errorCode: undefined, attempts: 0,
        createdAt: Date.now(), updatedAt: Date.now(), speed: kind === 'download' ? 3.4 * MB : 1.15 * MB, eta: -1
      };
      if (extra) for (var k in extra) t[k] = extra[k];
      transfers.unshift(t);
      return t;
    }

    newTask('download', '批量测试_0001.zip', 88 * MB, { pdirFid: undefined, localDir: settings.downloadDir });
    var paused = newTask('upload', '季度总结.docx', 12.6 * MB, { localPath: 'D:\\work\\季度总结.docx', pdirFid: folderIds['我的文档'] });
    paused.status = 'paused'; paused.transferred = 4.1 * MB; paused.speed = 0;
    var failed = newTask('download', '片段_A.mp4', 320 * MB, { localDir: settings.downloadDir });
    failed.status = 'error'; failed.error = '下载链接已过期，请重试'; failed.errorCode = 'LINK_EXPIRED';
    failed.transferred = 96 * MB; failed.speed = 0; failed.attempts = 3;
    var doneTask = newTask('upload', '架构图.png', 2.2 * MB, { localPath: 'D:\\work\\架构图.png', pdirFid: folderIds['项目资料'] });
    doneTask.status = 'done'; doneTask.transferred = doneTask.size; doneTask.speed = 0; doneTask.eta = 0;

    var timer = null;
    function emitTransfers() {
      var snap = transfers.map(function (t) {
        return JSON.parse(JSON.stringify(t));
      });
      transferSubs.slice().forEach(function (cb) { try { cb(snap); } catch (e) {} });
    }
    function tick() {
      var active = false;
      transfers.forEach(function (t) {
        if (t.status !== 'running') { t.speed = 0; return; }
        active = true;
        var prev = t.transferred;
        t.transferred = Math.min(t.size, t.transferred + t.speed * 0.25);
        t.updatedAt = Date.now();
        var remain = t.size - t.transferred;
        t.eta = t.speed > 0 ? Math.ceil(remain / t.speed) : -1;
        if (t.transferred >= t.size) {
          t.status = 'done'; t.speed = 0; t.eta = 0;
          pushToast('info', '「' + t.name + '」' + (t.kind === 'upload' ? '上传完成' : '下载完成'));
        }
        if (prev === t.transferred) { /* 无进展 */ }
      });
      emitTransfers();
      if (!active && timer) { clearInterval(timer); timer = null; }
    }
    function ensureTimer() {
      if (timer) return;
      timer = setInterval(tick, 250);   // 4 次/秒，符合契约的节流上限
    }
    ensureTimer();

    /* ---------- toast ---------- */
    var toastSubs = [];
    function pushToast(level, message) {
      toastSubs.slice().forEach(function (cb) { try { cb({ level: level, message: message }); } catch (e) {} });
    }

    /* ---------- 文件系统辅助 ---------- */
    function visibleChildren(pdirFid) {
      var ids = (kids[String(pdirFid)] || []).slice();
      var list = ids.map(function (id) { return items[id]; }).filter(Boolean);
      list.sort(function (a, b) {
        if (a.dir !== b.dir) return a.dir ? -1 : 1;
        return b.updatedAt - a.updatedAt;
      });
      return list;
    }
    function pageOf(list, page, size) {
      var p = Math.max(1, page || 1);
      var s = Math.max(1, size || 100);
      var start = (p - 1) * s;
      var slice = list.slice(start, start + s);
      return {
        list: JSON.parse(JSON.stringify(slice)),
        total: list.length,
        page: p,
        size: s,
        hasMore: start + slice.length < list.length
      };
    }
    function findByFid(id) { return items[String(id)] || null; }
    function removeRecursive(id) {
      var it = items[id];
      if (!it) return;
      if (it.dir) (kids[id] || []).slice().forEach(removeRecursive);
      delete items[id]; delete kids[id];
      var parent = kids[String(it.pdirFid)];
      if (parent) {
        var i = parent.indexOf(id);
        if (i >= 0) parent.splice(i, 1);
      }
    }
    function cloneRecursive(id, toPdirFid) {
      var it = items[id];
      if (!it) return null;
      var copyName = it.name;
      var clone = add(copyName, it.dir, it.size, toPdirFid, Date.now(), it.category);
      if (it.dir) (kids[id] || []).slice().forEach(function (childId) { cloneRecursive(childId, clone); });
      return clone;
    }

    /* ---------- 桥接实现 ---------- */
    var qk = {
      auth: {
        status: function () { return Promise.resolve(JSON.parse(JSON.stringify(account))); },
        verify: function () {
          return new Promise(function (res) { setTimeout(function () { res(JSON.parse(JSON.stringify(account))); }, 320); });
        },
        login: function () {
          return new Promise(function (res) {
            setTimeout(function () {
              account.loggedIn = true; account.expired = false;
              account.nickname = '演示账号'; account.memberLabel = '普通用户';
              emitAccount();
              pushToast('info', '登录成功');
              res(JSON.parse(JSON.stringify(account)));
            }, 600);
          });
        },
        logout: function () {
          account.loggedIn = false; account.expired = false; account.nickname = undefined;
          emitAccount(); pushToast('info', '已退出登录');
          return Promise.resolve();
        },
        quota: function () { return Promise.resolve(JSON.parse(JSON.stringify(quotaInfo))); },
        getUserInfo: function () { return this.status(); },
        onChange: function (cb) {
          if (typeof cb !== 'function') return function () {};
          accountSubs.push(cb);
          return function () { var i = accountSubs.indexOf(cb); if (i >= 0) accountSubs.splice(i, 1); };
        }
      },

      fs: {
        list: function (p) {
          p = p || {};
          return new Promise(function (res, rej) {
            setTimeout(function () {
              if (account.expired && mockFlags.failWhenExpired) { rej(mkError('NEED_LOGIN', '登录状态已失效，请重新登录')); return; }
              res(pageOf(visibleChildren(p.pdirFid || '0'), p.page, p.size));
            }, 260);
          });
        },
        search: function (p) {
          p = p || {};
          var kw = String(p.keyword || '').toLowerCase();
          var all = Object.keys(items).map(function (k) { return items[k]; })
            .filter(function (it) { return !it.dir && it.name.toLowerCase().indexOf(kw) >= 0; });
          all.sort(function (a, b) { return b.updatedAt - a.updatedAt; });
          return new Promise(function (res) { setTimeout(function () { res(pageOf(all, p.page, p.size)); }, 320); });
        },
        info: function (id) {
          return new Promise(function (res, rej) {
            var it = findByFid(id);
            setTimeout(function () { it ? res(JSON.parse(JSON.stringify(it))) : rej(mkError('NOT_FOUND', '资源不存在')); }, 120);
          });
        },
        crumbs: function (id) {
          return new Promise(function (res) {
            var chain = [{ fid: '0', name: '全部文件' }];
            if (String(id) !== '0') {
              var cur = findByFid(id);
              var guard = 0;
              while (cur && guard++ < 64) {
                chain.splice(1, 0, { fid: cur.fid, name: cur.name });
                cur = findByFid(cur.pdirFid);
              }
            }
            setTimeout(function () { res(chain); }, 90);
          });
        },
        mkdir: function (p) {
          p = p || {};
          var name = String(p.name || '').trim();
          if (!name) return Promise.reject(mkError('INVALID_ARG', '文件夹名称不能为空'));
          var sibs = visibleChildren(p.pdirFid || '0');
          if (sibs.some(function (s) { return s.name === name; })) {
            return Promise.reject(mkError('CONFLICT', '当前目录下已存在同名文件夹'));
          }
          return Promise.resolve(acceptAsync(function () {
            add(name, true, 0, p.pdirFid || '0', Date.now());
          }));
        },
        rename: function (p) {
          p = p || {};
          var it = findByFid(p.fid);
          if (!it) return Promise.reject(mkError('NOT_FOUND', '资源不存在'));
          var nn = String(p.name || '').trim();
          if (!nn) return Promise.reject(mkError('INVALID_ARG', '名称不能为空'));
          return new Promise(function (res) { setTimeout(function () { it.name = nn; it.updatedAt = Date.now(); res(); }, 220); });
        },
        move: function (p) {
          p = p || {};
          var to = String(p.toFid || '0');
          return Promise.resolve(acceptAsync(function () {
            (p.fids || []).forEach(function (id) {
              var it = findByFid(id);
              if (!it || String(id) === to) return;
              var from = kids[String(it.pdirFid)];
              if (from) { var i = from.indexOf(String(id)); if (i >= 0) from.splice(i, 1); }
              it.pdirFid = to;
              (kids[to] = kids[to] || []).push(String(id));
            });
          }));
        },
        copy: function (p) {
          p = p || {};
          var to = String(p.toFid || '0');
          return Promise.resolve(acceptAsync(function () {
            (p.fids || []).forEach(function (id) { cloneRecursive(String(id), to); });
          }));
        },
        remove: function (p) {
          p = p || {};
          return Promise.resolve(acceptAsync(function () {
            (p.fids || []).forEach(function (id) { removeRecursive(String(id)); });
          }));
        },
        taskStatus: function (p) {
          p = p || {};
          return new Promise(function (res) {
            setTimeout(function () {
              res((p.taskIds || []).map(function (id) {
                var t = asyncTasks[id] || { status: 2, done: true };
                return { taskId: id, status: t.status, done: t.done };
              }));
            }, 150);
          });
        }
      },

      transfer: {
        list: function () { return Promise.resolve(transfers.map(function (t) { return JSON.parse(JSON.stringify(t)); })); },
        upload: function (p) {
          p = p || {};
          var ids = (p.localPaths || []).map(function (path) {
            var name = String(path).split(/[\\/]/).pop();
            var t = newTask('upload', name, (2 + Math.random() * 40) * MB, { localPath: path, pdirFid: p.pdirFid });
            return t.id;
          });
          ensureTimer(); emitTransfers();
          return Promise.resolve(ids);
        },
        download: function (p) {
          p = p || {};
          var fids = p.fids || [];
          var names = p.names || [];
          var ids = fids.map(function (id, i) {
            var it = findByFid(id);
            var name = names[i] || (it ? it.name : '未知文件');
            var size = it && it.size ? it.size : (5 + Math.random() * 80) * MB;
            var t = newTask('download', name, size, { fid: id, localDir: p.localDir || settings.downloadDir });
            return t.id;
          });
          ensureTimer(); emitTransfers();
          return Promise.resolve(ids);
        },
        pause: function (id) { setStatus(id, 'paused'); return Promise.resolve(); },
        resume: function (id) {
          setStatus(id, 'running');
          var t = findTransfer(id);
          if (t) { t.speed = t.kind === 'download' ? 3.4 * MB : 1.15 * MB; t.eta = -1; }
          ensureTimer();
          return Promise.resolve();
        },
        retry: function (id) {
          var t = findTransfer(id);
          if (t) { t.status = 'running'; t.error = undefined; t.errorCode = undefined; t.attempts += 1; t.speed = 2.1 * MB; }
          ensureTimer(); emitTransfers();
          return Promise.resolve();
        },
        cancel: function (id) { setStatus(id, 'canceled'); return Promise.resolve(); },
        clear: function (ids) {
          transfers = transfers.filter(function (t) { return (ids || []).indexOf(t.id) < 0; });
          emitTransfers();
          return Promise.resolve();
        },
        openItem: function () { return Promise.resolve(); },
        onUpdate: function (cb) {
          if (typeof cb !== 'function') return function () {};
          transferSubs.push(cb);
          return function () { var i = transferSubs.indexOf(cb); if (i >= 0) transferSubs.splice(i, 1); };
        }
      },

      dialog: {
        pickDirs: function () { return Promise.resolve([settings.downloadDir]); },
        pickFiles: function () {
          return Promise.resolve([
            'D:\\演示\\报告_' + pad(Math.floor(Math.random() * 90) + 10, 2) + '.pdf',
            'D:\\演示\\素材包.zip'
          ]);
        },
        saveDir: function () { return Promise.resolve(settings.downloadDir); },
        confirm: function (p) {
          p = p || {};
          return Promise.resolve(global.confirm((p.title ? p.title + '\n\n' : '') + (p.message || '')));
        },
        openPath: function (pth) { pushToast('info', '（假数据模式）打开：' + pth); return Promise.resolve(); },
        showInFolder: function (pth) { pushToast('info', '（假数据模式）定位：' + pth); return Promise.resolve(); },
        getPathForFile: function () { return ''; }   // 模拟"拿不到真实路径"，用于验证降级提示
      },

      settings: {
        get: function () { return Promise.resolve(JSON.parse(JSON.stringify(settings))); },
        set: function (patch) {
          patch = patch || {};
          for (var k in patch) if (Object.prototype.hasOwnProperty.call(settings, k)) settings[k] = patch[k];
          return Promise.resolve(JSON.parse(JSON.stringify(settings)));
        }
      },

      onToast: function (cb) {
        if (typeof cb !== 'function') return function () {};
        toastSubs.push(cb);
        return function () { var i = toastSubs.indexOf(cb); if (i >= 0) toastSubs.splice(i, 1); };
      },
      onTransferUpdate: function (cb) { return qk.transfer.onUpdate(cb); }
    };

    function findTransfer(id) {
      for (var i = 0; i < transfers.length; i++) if (transfers[i].id === id) return transfers[i];
      return null;
    }
    function setStatus(id, status) {
      var t = findTransfer(id);
      if (!t) return;
      t.status = status;
      t.updatedAt = Date.now();
      if (status !== 'running') t.speed = 0;
      emitTransfers();
    }

    /* ---------- 供自检使用的假数据控制台 ---------- */
    var mockFlags = { failWhenExpired: true };
    qk.__mock = {
      /** 模拟登录态失效（验证顶部提示条）。 */
      expire: function () {
        account.loggedIn = false; account.expired = true;
        emitAccount();
        pushToast('warn', '登录状态已失效，请重新登录');
      },
      /** 恢复登录态。 */
      restore: function () { account.loggedIn = true; account.expired = false; emitAccount(); },
      /** 主动推送一条 toast，验证 toast 组件。 */
      toast: function (level, message) { pushToast(level || 'info', message || '测试消息'); },
      /** 模拟一个失败传输任务，验证"重试"。 */
      failTask: function () {
        var t = transfers[0];
        if (t) { t.status = 'error'; t.error = '网络请求失败，请检查网络连接'; t.errorCode = 'NETWORK'; t.speed = 0; emitTransfers(); }
      },
      /** 追加文件到当前目录，验证"刷新后分页"与性能。 */
      stats: function () {
        return {
          items: Object.keys(items).length,
          transfers: transfers.length,
          rootChildren: (kids['0'] || []).length
        };
      },
      flags: mockFlags
    };

    return qk;
  }
})(typeof window !== 'undefined' ? window : globalThis);
