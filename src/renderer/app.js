'use strict';
/**
 * src/renderer/app.js —— 视图层（纯原生 DOM，无框架）
 *
 * 依赖：window.QKAPI（api.js 提供），绝不直接访问 window.qk。
 * 只负责渲染与交互；所有网络/磁盘操作都交给桥接层。
 */

(function () {
  var API = window.QKAPI;

  /* ================================================================== *
   * 常量与状态
   * ================================================================== */

  var PAGE_SIZE = 100;
  var VIRTUAL_OVERSCAN = 8;      // 只挂载可见行及前后缓冲行
  var ROOT_FID = '0';
  var TASK_POLL_MS = 600;
  var TASK_POLL_MAX = 40;       // 最多轮询 40 次（约 24s）

  var STATUS_TEXT = {
    pending: '等待中', running: '进行中', paused: '已暂停',
    error: '失败', done: '已完成', canceled: '已取消'
  };

  var state = {
    view: 'files',                 // files | transfers | settings
    fid: ROOT_FID,                 // 当前目录
    crumbs: [{ fid: ROOT_FID, name: '全部文件' }],
    mode: 'browse',                // browse | search
    keyword: '',
    items: [],                     // 已加载的条目（按顺序）
    itemMap: {},                   // fid -> FileItem
    page: 0,
    hasMore: false,
    total: -1,
    loading: false,
    loadedOnce: false,
    selected: new Set(),
    lastIndex: -1,
    account: null,
    quota: null,
    settings: {
      downloadDir: '',
      uploadConcurrency: 2,
      downloadConcurrency: 2,
      downloadParallel: 16,
      downloadStrategy: 'auto',
      uploadPartConcurrency: 3,
      theme: 'light',
    },
    transfers: [],
    transfersLoaded: false,
    expired: false,
    uploading: false
  };

  var el = {};
  var rowMap = new Map();          // fid -> tr element
  var transferMap = new Map();     // id  -> element
  var unsubs = [];
  var toastSeq = 0;
  var listSeq = 0;
  var listFrame = null;
  var transferSeq = 0;
  var topSpacer = null;
  var bottomSpacer = null;

  function $(id) { return document.getElementById(id); }

  /* ================================================================== *
   * 工具函数
   * ================================================================== */

  function h(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined && text !== null) n.textContent = text;
    return n;
  }

  function icon(name, cls) {
    var svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('class', 'ic' + (cls ? ' ' + cls : ''));
    svg.setAttribute('aria-hidden', 'true');
    var use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
    use.setAttribute('href', '#i-' + name);
    svg.appendChild(use);
    return svg;
  }

  function pad2(n) { return String(n).length < 2 ? '0' + n : String(n); }

  function fmtSize(bytes) {
    var b = Number(bytes);
    if (!isFinite(b) || b <= 0) return b === 0 ? '—' : '—';
    var units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
    var i = 0;
    while (b >= 1024 && i < units.length - 1) { b /= 1024; i += 1; }
    var v = i === 0 ? b : b.toFixed(b >= 100 ? 0 : (b >= 10 ? 1 : 2));
    return v + ' ' + units[i];
  }

  function fmtTime(ts) {
    var t = Number(ts);
    if (!isFinite(t) || t <= 0) return '—';
    var d = new Date(t);
    return d.getFullYear() + '-' + pad2(d.getMonth() + 1) + '-' + pad2(d.getDate()) +
      ' ' + pad2(d.getHours()) + ':' + pad2(d.getMinutes());
  }

  function fmtSpeed(bps) {
    var v = Number(bps);
    if (!isFinite(v) || v <= 0) return '';
    return fmtSize(v) + '/s';
  }

  function fmtEta(sec) {
    var s = Number(sec);
    if (!isFinite(s) || s < 0) return '';
    if (s < 60) return '剩余 ' + Math.max(1, Math.round(s)) + ' 秒';
    if (s < 3600) return '剩余 ' + Math.round(s / 60) + ' 分钟';
    return '剩余 ' + (s / 3600).toFixed(1) + ' 小时';
  }

  /** 根据扩展名/分类选择图标。分类见契约：1 视频 2 图片 3 文档 4 音频 5 压缩包。 */
  function iconFor(item) {
    if (item.dir) return 'folder';
    var ext = String(item.ext || '').toLowerCase();
    if (['jpg', 'jpeg', 'png', 'gif', 'webp', 'bmp', 'svg', 'heic', 'tiff'].indexOf(ext) >= 0) return 'file-image';
    if (['mp4', 'mkv', 'avi', 'mov', 'wmv', 'flv', 'webm', 'm4v', 'ts'].indexOf(ext) >= 0) return 'file-video';
    if (['mp3', 'flac', 'wav', 'aac', 'ogg', 'm4a', 'wma'].indexOf(ext) >= 0) return 'file-audio';
    if (['zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'iso'].indexOf(ext) >= 0) return 'file-archive';
    if (['js', 'ts', 'json', 'html', 'css', 'py', 'java', 'go', 'c', 'cpp', 'cs', 'sh', 'xml', 'yml', 'yaml', 'sql'].indexOf(ext) >= 0) return 'file-code';
    if (['txt', 'md', 'doc', 'docx', 'pdf', 'xls', 'xlsx', 'ppt', 'pptx', 'csv', 'rtf'].indexOf(ext) >= 0) return 'file-doc';
    if (item.category === 1) return 'file-video';
    if (item.category === 2) return 'file-image';
    if (item.category === 3) return 'file-doc';
    if (item.category === 4) return 'file-audio';
    if (item.category === 5) return 'file-archive';
    return 'file';
  }

  function catClass(item) {
    if (item.dir) return 'cat-dir';
    var ic = iconFor(item);
    return 'cat-' + ic.replace('file-', '') || 'cat-other';
  }

  /* ================================================================== *
   * toast
   * ================================================================== */

  function toast(level, message, ms) {
    if (!message) return;
    if (el.toasts.childElementCount >= 5) el.toasts.removeChild(el.toasts.firstChild);
    var lv = (level === 'warn' || level === 'error') ? level : 'info';
    var node = h('div', 'toast toast-' + lv);
    node.appendChild(icon(lv === 'error' ? 'error' : (lv === 'warn' ? 'warn' : 'info'), 'toast-ic'));
    var body = h('div', 'toast-body');
    body.appendChild(h('div', 'toast-title', lv === 'error' ? '出错了' : (lv === 'warn' ? '注意' : '提示')));
    body.appendChild(h('div', 'toast-msg', String(message)));
    node.appendChild(body);
    var close = h('button', 'toast-close');
    close.type = 'button';
    close.title = '关闭';
    close.appendChild(icon('close'));
    close.addEventListener('click', function () { remove(); });
    node.appendChild(close);
    el.toasts.appendChild(node);
    var id = ++toastSeq;
    node.dataset.toastId = String(id);
    var timer = setTimeout(remove, ms || (lv === 'error' ? 6000 : 3200));
    function remove() {
      clearTimeout(timer);
      if (!node.parentNode) return;
      node.classList.add('toast-out');
      setTimeout(function () { if (node.parentNode) node.parentNode.removeChild(node); }, 160);
    }
  }

  /** 统一错误上报：NEED_LOGIN 由 QKAPI.onNeedLogin 统一驱动提示条，这里只负责文案。 */
  function reportError(err, fallback) {
    var e = API.normalizeError(err, fallback);
    if (API.isNeedLogin(e)) {
      toast('error', '登录状态已失效，请重新登录');
      return e;
    }
    toast('error', e.message || fallback || '操作失败');
    return e;
  }

  /* ================================================================== *
   * 顶部提示条
   * ================================================================== */

  function banner(key, level, text, actions) {
    var old = el.banners.querySelector('[data-banner="' + key + '"]');
    if (old) old.parentNode.removeChild(old);
    var node = h('div', 'banner banner-' + level);
    node.setAttribute('data-banner', key);
    node.appendChild(icon(level === 'error' ? 'error' : (level === 'warn' ? 'warn' : 'info'), 'banner-ic'));
    node.appendChild(h('div', 'banner-text', text));
    (actions || []).forEach(function (a) {
      var b = h('button', 'btn btn-sm ' + (a.primary ? 'btn-primary' : ''));
      b.type = 'button';
      b.textContent = a.label;
      b.addEventListener('click', a.onClick);
      node.appendChild(b);
    });
    var close = h('button', 'banner-close');
    close.type = 'button';
    close.title = '关闭';
    close.appendChild(icon('close'));
    close.addEventListener('click', function () { if (node.parentNode) node.parentNode.removeChild(node); });
    node.appendChild(close);
    el.banners.appendChild(node);
  }

  function clearBanner(key) {
    var old = el.banners.querySelector('[data-banner="' + key + '"]');
    if (old) old.parentNode.removeChild(old);
  }

  function showNoBridgeBanner(info) {
    banner('bridge', 'warn', '未注入桥接层（window.qk 不存在）：当前只能查看界面，文件与传输功能不可用。' +
      '请通过 npm start 启动桌面端后再试。', [
      { label: '重新检测', onClick: function () { location.reload(); } }
    ]);
  }

  function showExpiredBanner() {
    state.expired = true;
    renderAccount();
    banner('auth', 'error', '登录状态已失效，请重新登录', [
      {
        label: '重新登录', primary: true, onClick: function () {
          doLogin();
        }
      }
    ]);
  }

  function clearExpiredBanner() {
    state.expired = false;
    clearBanner('auth');
  }

  async function doLogin() {
    try {
      var s = await API.auth.login();
      applyAccount(s);
      clearExpiredBanner();
      toast('info', '登录成功');
      refreshQuota();
      reloadCurrent();
    } catch (err) {
      reportError(err, '登录失败');
    }
  }

  async function doLogout() {
    var ok = await confirmDialog({
      title: '退出登录',
      message: '确定要退出当前账号吗？',
      detail: '退出后本地保存的登录凭证会被清除，需要重新登录才能继续使用。',
      confirmText: '退出登录'
    });
    if (!ok) return;
    try {
      await API.auth.logout();
      applyAccount({ loggedIn: false, expired: false });
      clearExpiredBanner();
      toast('info', '已退出登录');
    } catch (err) {
      reportError(err, '退出登录失败');
    }
  }

  /* ================================================================== *
   * 账号区
   * ================================================================== */

  function applyAccount(s) {
    if (!s || typeof s !== 'object') return;
    var previous = state.account;
    state.account = s;
    if (previous && previous.loggedIn && (!s.loggedIn || s.expired ||
        (previous.userId && s.userId && previous.userId !== s.userId))) {
      // Invalidate in-flight pages as well as the rendered rows on account changes.
      state.quota = null;
      state.fid = ROOT_FID;
      state.mode = 'browse';
      state.keyword = '';
      state.crumbs = [{ fid: ROOT_FID, name: '全部文件' }];
      el.searchInput.value = '';
      el.searchClear.hidden = true;
      resetList();
      renderCrumbs();
      renderEmpty(s.expired ? '登录状态已失效，请重新登录' : '请先登录夸克网盘', false);
    }
    if (s.expired === true) showExpiredBanner();
    else if (s.loggedIn === true) clearExpiredBanner();
    renderAccount();
    renderSettings();
  }

  function renderAccount() {
    var box = el.account;
    box.textContent = '';
    var a = state.account;

    if (!API.injected) {
      box.appendChild(h('span', 'acct-hint', '未连接后端'));
      return;
    }
    if (!a || (!a.loggedIn && !a.expired)) {
      box.appendChild(h('span', 'acct-hint', a && a.expired ? '登录已失效' : '未登录'));
      var lb = h('button', 'btn btn-sm btn-primary');
      lb.type = 'button';
      lb.textContent = '登录';
      lb.addEventListener('click', doLogin);
      box.appendChild(lb);
      return;
    }
    if (a.expired) {
      box.appendChild(h('span', 'acct-hint', '登录已失效'));
      var rb = h('button', 'btn btn-sm btn-primary');
      rb.type = 'button';
      rb.textContent = '重新登录';
      rb.addEventListener('click', doLogin);
      box.appendChild(rb);
      return;
    }

    // 头像：仅接受 data: 内联图，避免任何外部网络请求
    var av = h('span', 'avatar');
    if (a.avatar && /^data:image\//i.test(a.avatar)) {
      var img = h('img');
      img.src = a.avatar;
      img.alt = '';
      av.appendChild(img);
    } else {
      var ch = (a.nickname || '用').trim().charAt(0) || '用';
      av.appendChild(h('span', 'avatar-text', ch));
    }
    box.appendChild(av);

    var meta = h('div', 'acct-meta');
    meta.appendChild(h('div', 'acct-name', a.nickname || '已登录'));
    var sub = h('div', 'acct-sub');
    sub.appendChild(h('span', 'acct-tag', a.memberLabel || '普通用户'));
    var q = state.quota;
    if (q && q.total > 0) {
      sub.appendChild(h('span', 'acct-quota-text',
        '已用 ' + fmtSize(q.used) + ' / ' + fmtSize(q.total)));
    }
    meta.appendChild(sub);
    if (q && q.total > 0) {
      var bar = h('div', 'quota-bar');
      var fill = h('i');
      var pct = Math.min(100, Math.max(0, (q.used / q.total) * 100));
      fill.style.width = (pct < 0.6 && pct > 0 ? 0.6 : pct) + '%';
      bar.appendChild(fill);
      meta.appendChild(bar);
    }
    box.appendChild(meta);

    var out = h('button', 'btn btn-icon btn-ghost');
    out.type = 'button';
    out.title = '退出登录';
    out.setAttribute('aria-label', '退出登录');
    out.appendChild(icon('logout'));
    out.addEventListener('click', doLogout);
    box.appendChild(out);
  }

  async function refreshQuota() {
    var account = state.account;
    try {
      var q = await API.auth.quota();
      if (state.account !== account || !account || !account.loggedIn) return;
      if (q && typeof q.total === 'number') { state.quota = q; renderAccount(); }
    } catch (e) { /* 容量信息失败不打扰用户 */ }
  }

  /* ================================================================== *
   * 自绘对话框
   * ================================================================== */

  function closeOverlay(node) {
    if (!node || !node.parentNode) return;
    node.classList.add('overlay-out');
    setTimeout(function () { if (node.parentNode) node.parentNode.removeChild(node); }, 140);
  }

  function openOverlay(build) {
    var wrap = h('div', 'overlay');
    var panel = h('div', 'dlg');
    wrap.appendChild(panel);
    el.overlayRoot.appendChild(wrap);
    requestAnimationFrame(function () { wrap.classList.add('overlay-in'); });
    var closed = false;
    function close(result) {
      if (closed) return;
      closed = true;
      document.removeEventListener('keydown', onKey, true);
      closeOverlay(wrap);
      if (resolveFn) resolveFn(result);
    }
    var resolveFn = null;
    var api = { wrap: wrap, panel: panel, close: close, setResolve: function (fn) { resolveFn = fn; } };
    wrap.addEventListener('mousedown', function (e) { if (e.target === wrap) close(null); });
    function onKey(e) {
      if (e.key === 'Escape') { e.stopPropagation(); close(null); }
    }
    document.addEventListener('keydown', onKey, true);
    build(api);
    var focusable = panel.querySelector('input, button.btn-primary, button');
    if (focusable) setTimeout(function () { try { focusable.focus(); } catch (e) {} }, 30);
    return api;
  }

  /** 二次确认对话框。danger=true 时确认按钮为红色。 */
  function confirmDialog(opts) {
    var o = opts || {};
    return new Promise(function (resolve) {
      var api = openOverlay(function (ui) {
        ui.setResolve(resolve);
        ui.panel.classList.add('dlg-confirm');
        var head = h('div', 'dlg-head');
        head.appendChild(icon(o.danger ? 'warn' : 'info', 'dlg-ic' + (o.danger ? ' danger' : '')));
        head.appendChild(h('h3', 'dlg-title', o.title || '请确认'));
        ui.panel.appendChild(head);

        var body = h('div', 'dlg-body');
        body.appendChild(h('p', 'dlg-msg', o.message || ''));
        if (o.detail) body.appendChild(h('p', 'dlg-detail', o.detail));
        if (o.list && o.list.length) {
          var ul = h('ul', 'dlg-list');
          o.list.slice(0, 12).forEach(function (t) { ul.appendChild(h('li', null, t)); });
          if (o.list.length > 12) ul.appendChild(h('li', 'muted', '…等共 ' + o.list.length + ' 项'));
          body.appendChild(ul);
        }
        ui.panel.appendChild(body);

        var foot = h('div', 'dlg-foot');
        var cancel = h('button', 'btn');
        cancel.type = 'button';
        cancel.textContent = o.cancelText || '取消';
        cancel.addEventListener('click', function () { ui.close(false); });
        var ok = h('button', 'btn ' + (o.danger ? 'btn-danger' : 'btn-primary'));
        ok.type = 'button';
        ok.textContent = o.confirmText || '确定';
        ok.addEventListener('click', function () { ui.close(true); });
        foot.appendChild(cancel);
        foot.appendChild(ok);
        ui.panel.appendChild(foot);
        ok.dataset.autofocus = '1';
        cancel.focus();
      });
      api.wrap.addEventListener('keydown', function (e) {
        if (e.key === 'Enter' && !e.isComposing) {
          var ok = api.panel.querySelector('.dlg-foot .btn:last-child');
          if (ok) ok.click();
        }
      });
    });
  }

  /** 文本输入对话框（新建文件夹 / 重命名）。validate 返回错误文案或 null。 */
  function promptDialog(opts) {
    var o = opts || {};
    return new Promise(function (resolve) {
      var api = openOverlay(function (ui) {
        ui.setResolve(resolve);
        ui.panel.classList.add('dlg-prompt');
        var head = h('div', 'dlg-head');
        head.appendChild(icon(o.icon || 'rename', 'dlg-ic'));
        head.appendChild(h('h3', 'dlg-title', o.title || '请输入'));
        ui.panel.appendChild(head);

        var body = h('div', 'dlg-body');
        if (o.message) body.appendChild(h('p', 'dlg-msg', o.message));
        var input = h('input', 'input dlg-input');
        input.type = 'text';
        input.value = o.value || '';
        input.placeholder = o.placeholder || '';
        input.spellcheck = false;
        input.autocomplete = 'off';
        body.appendChild(input);
        var err = h('p', 'dlg-error');
        err.hidden = true;
        body.appendChild(err);
        ui.panel.appendChild(body);

        var foot = h('div', 'dlg-foot');
        var cancel = h('button', 'btn');
        cancel.type = 'button';
        cancel.textContent = '取消';
        cancel.addEventListener('click', function () { ui.close(null); });
        var ok = h('button', 'btn btn-primary');
        ok.type = 'button';
        ok.textContent = o.confirmText || '确定';
        ok.addEventListener('click', submit);
        foot.appendChild(cancel);
        foot.appendChild(ok);
        ui.panel.appendChild(foot);

        input.addEventListener('keydown', function (e) {
          if (e.key === 'Enter' && !e.isComposing) { e.preventDefault(); submit(); }
        });
        input.addEventListener('input', function () { err.hidden = true; });

        function submit() {
          var v = input.value.trim();
          if (!v) { err.textContent = '不能为空'; err.hidden = false; input.focus(); return; }
          if (o.validate) {
            var msg = o.validate(v);
            if (msg) { err.textContent = msg; err.hidden = false; input.focus(); return; }
          }
          ui.close(v);
        }
        setTimeout(function () {
          input.focus();
          var dot = input.value.lastIndexOf('.');
          if (dot > 0 && !o.selectAll) input.setSelectionRange(0, dot); else input.select();
        }, 30);
      });
    });
  }

  /**
   * 目录选择器：fs.crumbs + fs.list 逐层浏览，根目录 fid 恒为 '0'。
   * 返回选中的目标目录 fid，取消返回 null。
   */
  function pickDirDialog(opts) {
    var o = opts || {};
    var cur = { fid: ROOT_FID, name: '全部文件' };
    var crumbs = [{ fid: ROOT_FID, name: '全部文件' }];
    var exclude = o.excludeFids || [];
    var chosen = null;
    var listBox, crumbBox, confirmBtn, loadSeq = 0;

    return new Promise(function (resolve) {
      var api = openOverlay(function (ui) {
        ui.setResolve(resolve);
        ui.panel.classList.add('dlg-picker');

        var head = h('div', 'dlg-head');
        head.appendChild(icon('move', 'dlg-ic'));
        head.appendChild(h('h3', 'dlg-title', o.title || '选择目标目录'));
        ui.panel.appendChild(head);

        crumbBox = h('div', 'picker-crumbs');
        ui.panel.appendChild(crumbBox);

        listBox = h('div', 'picker-list');
        var loading = h('div', 'picker-loading', '加载中…');
        listBox.appendChild(loading);
        ui.panel.appendChild(listBox);

        var foot = h('div', 'dlg-foot');
        var hint = h('span', 'picker-hint', '未选中子目录时表示使用当前所在目录');
        var cancel = h('button', 'btn');
        cancel.type = 'button';
        cancel.textContent = '取消';
        cancel.addEventListener('click', function () { ui.close(null); });
        confirmBtn = h('button', 'btn btn-primary');
        confirmBtn.type = 'button';
        confirmBtn.textContent = o.confirmText || '移动到此处';
        confirmBtn.addEventListener('click', function () {
          ui.close(chosen === null ? cur.fid : chosen);
        });
        foot.appendChild(hint);
        foot.appendChild(cancel);
        foot.appendChild(confirmBtn);
        ui.panel.appendChild(foot);

        loadDir(ROOT_FID);
      });

      function renderCrumbs() {
        crumbBox.textContent = '';
        crumbs.forEach(function (c, i) {
          if (i > 0) crumbBox.appendChild(icon('chevron', 'crumb-sep'));
          var b = h('button', 'crumb-item');
          b.type = 'button';
          b.textContent = c.name;
          b.disabled = i === crumbs.length - 1;
          b.addEventListener('click', function () {
            crumbs = crumbs.slice(0, i + 1);
            cur = { fid: c.fid, name: c.name };
            chosen = null;
            loadDir(c.fid);
          });
          crumbBox.appendChild(b);
        });
      }

      async function loadDir(fid, page, append) {
        page = page || 1;
        var seq = append ? loadSeq : ++loadSeq;
        var more = listBox.querySelector(".picker-more");
        if (more) more.remove();
        if (!append) listBox.textContent = '';
        var indicator = h('div', 'picker-loading', '加载中…');
        listBox.appendChild(indicator);
        renderCrumbs();
        try {
          var res = await API.fs.list({ pdirFid: fid, page: page, size: 100 });
          if (seq !== loadSeq) return;
          var dirs = (res && res.list ? res.list : []).filter(function (it) { return it.dir; });
          indicator.remove();
          if (!dirs.length && !append) {
            listBox.appendChild(h('div', 'picker-empty', '当前目录下没有子文件夹'));
          } else {
            dirs.forEach(function (d) {
              var row = h('button', 'picker-row');
              row.type = 'button';
              row.dataset.fid = d.fid;
              var disabled = exclude.indexOf(d.fid) >= 0;
              if (disabled) { row.disabled = true; row.title = '不能选择该项自身'; }
              row.appendChild(icon('folder', 'picker-ic'));
              row.appendChild(h('span', 'picker-name', d.name));
              row.appendChild(h('span', 'picker-enter', '进入'));
              row.addEventListener('click', function (e) {
                if (e.target && e.target.classList.contains('picker-enter')) return;
                if (disabled) return;
                if (chosen === d.fid) { chosen = null; }
                else chosen = d.fid;
                Array.prototype.forEach.call(listBox.querySelectorAll('.picker-row'), function (r) {
                  r.classList.toggle('selected', r.dataset.fid === chosen);
                });
              });
              row.addEventListener('dblclick', function () {
                if (disabled) return;
                crumbs = crumbs.concat([{ fid: d.fid, name: d.name }]);
                cur = { fid: d.fid, name: d.name };
                chosen = null;
                loadDir(d.fid);
              });
              var enter = row.querySelector('.picker-enter');
              enter.addEventListener('click', function (e) {
                e.stopPropagation();
                crumbs = crumbs.concat([{ fid: d.fid, name: d.name }]);
                cur = { fid: d.fid, name: d.name };
                chosen = null;
                loadDir(d.fid);
              });
              listBox.appendChild(row);
            });
          }
          if (res && res.hasMore) {
            var next = h('button', 'btn picker-more', '加载更多目录');
            next.type = 'button';
            next.addEventListener('click', function () { loadDir(fid, page + 1, true); });
            listBox.appendChild(next);
          }
        } catch (err) {
          if (seq !== loadSeq) return;
          listBox.textContent = '';
          var e2 = reportError(err, '读取目录失败');
          listBox.appendChild(h('div', 'picker-empty', e2.message || '读取目录失败'));
        }
      }
    });
  }

  /* ================================================================== *
   * 右键菜单
   * ================================================================== */

  function closeCtx() { el.ctx.hidden = true; el.ctx.textContent = ''; }

  function openCtx(x, y, entries) {
    el.ctx.textContent = '';
    entries.forEach(function (en) {
      if (en.sep) { el.ctx.appendChild(h('div', 'ctx-sep')); return; }
      var b = h('button', 'ctx-item' + (en.danger ? ' danger' : ''));
      b.type = 'button';
      b.appendChild(icon(en.icon || 'file'));
      b.appendChild(h('span', null, en.label));
      b.disabled = !!en.disabled;
      b.addEventListener('click', function () { closeCtx(); en.run(); });
      el.ctx.appendChild(b);
    });
    el.ctx.hidden = false;
    var r = el.ctx.getBoundingClientRect();
    var left = Math.min(x, window.innerWidth - r.width - 8);
    var top = Math.min(y, window.innerHeight - r.height - 8);
    el.ctx.style.left = Math.max(8, left) + 'px';
    el.ctx.style.top = Math.max(8, top) + 'px';
  }

  /* ================================================================== *
   * 顶部栏：面包屑 / 搜索 / 账号
   * ================================================================== */

  function renderCrumbs() {
    var box = el.crumbs;
    box.textContent = '';
    if (state.mode === 'search') {
      box.appendChild(h('span', 'crumb-search-label', '搜索结果：'));
      box.appendChild(h('span', 'crumb-current', state.keyword));
      return;
    }
    state.crumbs.forEach(function (c, i) {
      if (i > 0) box.appendChild(icon('chevron', 'crumb-sep'));
      var last = i === state.crumbs.length - 1;
      var b = h('button', 'crumb-item' + (last ? ' current' : ''));
      b.type = 'button';
      b.textContent = c.name;
      b.title = last ? c.name : ('返回 ' + c.name);
      if (!last) b.addEventListener('click', function () { navigate(c.fid, c.name); });
      box.appendChild(b);
    });
  }

  async function loadCrumbs(fid) {
    var seq = listSeq;
    if (String(fid) === ROOT_FID) {
      state.crumbs = [{ fid: ROOT_FID, name: '全部文件' }];
      renderCrumbs();
      return;
    }
    try {
      var cs = await API.fs.crumbs(fid);
      if (seq !== listSeq || state.mode !== "browse" || String(fid) !== state.fid) return;
      if (Array.isArray(cs) && cs.length) {
        state.crumbs = cs.map(function (c, i) {
          return { fid: String(c.fid), name: c.name || (i === 0 ? '全部文件' : '未命名') };
        });
      }
    } catch (err) {
      if (seq !== listSeq) return;
      reportError(err, '读取路径失败');
    }
    renderCrumbs();
  }

  function setTopbarForView() {
    var filesView = state.view === 'files';
    [el.crumbs, el.searchBox, el.btnMkdir, el.btnUpload, el.btnRefresh].forEach(function (n) {
      if (n) n.hidden = !filesView;
    });
  }

  /* ================================================================== *
   * 文件列表
   * ================================================================== */

  async function navigate(fid, name) {
    state.mode = 'browse';
    state.fid = String(fid);
    state.selected.clear();
    state.lastIndex = -1;
    el.searchInput.value = '';
    el.searchClear.hidden = true;
    resetList();
    renderCrumbs();
    if (name) {
      // 先给出即时反馈，随后用后端 crumbs 校正
      state.crumbs = state.crumbs.slice(0, Math.max(1, state.crumbs.length));
    }
    loadCrumbs(state.fid);
    await loadPage({ reset: true });
    el.listwrap.scrollTop = 0;
  }

  function reloadCurrent() {
    if (state.view !== 'files') return;
    if (state.mode === 'search') { runSearch(state.keyword); return; }
    resetList();
    loadCrumbs(state.fid);
    loadPage({ reset: true });
  }

  function resetList() {
    listSeq += 1;
    state.selected.clear();
    state.lastIndex = -1;
    rowMap.forEach(function (n) { if (n.parentNode) n.parentNode.removeChild(n); });
    rowMap.clear();
    state.items = [];
    state.itemMap = {};
    state.page = 0;
    state.hasMore = false;
    state.total = -1;
    state.loading = false;
    state.loadedOnce = false;
    el.list.textContent = '';
    topSpacer = null;
    bottomSpacer = null;
    el.listwrap.scrollTop = 0;
    clearEmpty();
    renderListFoot();
    updateSelectAll();
  }

  function findItem(fid) { return state.itemMap[String(fid)] || null; }

  function selectedItems() {
    var out = [];
    state.selected.forEach(function (fid) { var it = findItem(fid); if (it) out.push(it); });
    return out;
  }

  async function loadPage(opts) {
    var o = opts || {};
    if (state.loading) return;
    if (!o.reset && !state.hasMore) return;
    var seq = listSeq;
    state.loading = true;
    renderListFoot();
    var nextPage = o.reset ? 1 : state.page + 1;
    try {
      var params = { page: nextPage, size: PAGE_SIZE };
      var res;
      if (state.mode === 'search') {
        params.keyword = state.keyword;
        res = await API.fs.search(params);
      } else {
        params.pdirFid = state.fid;
        params.sort = 'file_type:asc,updated_at:desc';
        res = await API.fs.list(params);
      }
      if (seq !== listSeq) return;
      var list = (res && Array.isArray(res.list)) ? res.list.filter(function (x) { return x && x.fid; }) : [];
      if (o.reset) {
        state.items = [];
        state.itemMap = {};
        state.page = 0;
      }
      state.page = Number(res && res.page) > 0 ? Number(res.page) : nextPage;
      state.total = res && typeof res.total === 'number' ? res.total : -1;
      state.hasMore = !!(res && res.hasMore);
      list.forEach(function (it) {
        var key = String(it.fid);
        if (state.itemMap[key]) return;
        state.itemMap[key] = it;
        state.items.push(it);
      });
      // 容错：若主进程未正确返回 hasMore，但本页刚好满页且（总数未知或还没取完），
      // 仍继续尝试下一页；下一页返回不足一页时自然停止，不会无限加载。
      if (!state.hasMore && list.length >= PAGE_SIZE && (state.total < 0 || state.items.length < state.total)) {
        state.hasMore = true;
      }
      state.loadedOnce = true;
      await renderRows(seq);
    } catch (err) {
      if (seq !== listSeq) return;
      state.loadedOnce = true;
      var e = reportError(err, '读取目录失败');
      if (state.items.length === 0) renderEmpty(e.message || '读取目录失败', true);
    } finally {
      if (seq !== listSeq) return;
      state.loading = false;
      renderListFoot();
      updateSelectAll();
      maybeAutoFill();
    }
  }

  /** 首屏不足一屏且仍有下一页时继续拉取，保证滚动容器可滚动。 */
  function maybeAutoFill() {
    if (!state.hasMore || state.loading) return;
    if (el.listwrap.scrollHeight <= el.listwrap.clientHeight + 8) {
      loadPage({});
    }
  }

  function createRow(item) {
    var row = h('div', 'row');
    row.dataset.fid = String(item.fid);
    row.setAttribute('role', 'listitem');

    var cbCell = h('span', 'cb-cell');
    var label = h('label', 'cb');
    var input = h('input');
    input.type = 'checkbox';
    input.setAttribute('aria-label', '选择 ' + item.name);
    label.appendChild(input);
    label.appendChild(h('i'));
    cbCell.appendChild(label);
    row.appendChild(cbCell);

    var nameCell = h('span', 'col-name');
    var ic = h('span', 'fileicon ' + catClass(item));
    ic.appendChild(icon(iconFor(item)));
    nameCell.appendChild(ic);
    nameCell.appendChild(h('span', 'fname', item.name));
    row.appendChild(nameCell);

    row.appendChild(h('span', 'col-size', item.dir ? '—' : fmtSize(item.size)));
    row.appendChild(h('span', 'col-time', fmtTime(item.updatedAt || item.createdAt)));

    var ops = h('span', 'col-ops');
    OPS.forEach(function (op) {
      var b = h('button', 'op' + (op.danger ? ' danger' : ''));
      b.type = 'button';
      b.dataset.op = op.key;
      b.title = op.title;
      b.setAttribute('aria-label', op.title);
      b.appendChild(icon(op.icon));
      ops.appendChild(b);
    });
    row.appendChild(ops);
    return row;
  }

  function updateRow(row, item) {
    var nameEl = row.querySelector('.fname');
    if (nameEl && nameEl.textContent !== item.name) nameEl.textContent = item.name;
    var sizeEl = row.querySelector('.col-size');
    var sizeText = item.dir ? '—' : fmtSize(item.size);
    if (sizeEl && sizeEl.textContent !== sizeText) sizeEl.textContent = sizeText;
    var timeEl = row.querySelector('.col-time');
    var timeText = fmtTime(item.updatedAt || item.createdAt);
    if (timeEl && timeEl.textContent !== timeText) timeEl.textContent = timeText;
    var icWrap = row.querySelector('.fileicon');
    var want = 'fileicon ' + catClass(item);
    if (icWrap && icWrap.className !== want) {
      icWrap.className = want;
      icWrap.textContent = '';
      icWrap.appendChild(icon(iconFor(item)));
    }
    var cb = row.querySelector('input[type=checkbox]');
    var sel = state.selected.has(String(item.fid));
    if (cb && cb.checked !== sel) cb.checked = sel;
    if (row.classList.contains('selected') !== sel) row.classList.toggle('selected', sel);
  }

  /** Visible rows only: data and selection survive when their DOM nodes leave the viewport. */
  async function renderRows(seq) {
    if (seq !== listSeq) return;
    if (!topSpacer) {
      topSpacer = h('div', 'list-spacer');
      bottomSpacer = h('div', 'list-spacer');
      topSpacer.setAttribute('aria-hidden', 'true');
      bottomSpacer.setAttribute('aria-hidden', 'true');
      el.list.appendChild(topSpacer);
      el.list.appendChild(bottomSpacer);
    }
    var rowHeight = parseFloat(getComputedStyle(el.list).getPropertyValue('--row-h')) || 46;
    var range = window.QKVirtualList.range(state.items.length, el.listwrap.scrollTop,
      el.listwrap.clientHeight || 600, rowHeight, VIRTUAL_OVERSCAN);
    var visible = new Set();
    for (var i = range.start; i < range.end; i++) visible.add(String(state.items[i].fid));
    rowMap.forEach(function (row, fid) {
      if (!visible.has(fid)) {
        if (row.parentNode) row.parentNode.removeChild(row);
        rowMap.delete(fid);
      }
    });
    topSpacer.style.height = range.before + 'px';
    bottomSpacer.style.height = range.after + 'px';
    var ref = topSpacer.nextSibling;
    for (var j = range.start; j < range.end; j++) {
      var item = state.items[j];
      var key = String(item.fid);
      var row = rowMap.get(key);
      if (!row) {
        row = createRow(item);
        rowMap.set(key, row);
      }
      updateRow(row, item);
      row.setAttribute('aria-posinset', String(j + 1));
      row.setAttribute('aria-setsize', String(state.total >= 0 ? state.total : state.items.length));
      if (row === ref) ref = ref.nextSibling;
      else el.list.insertBefore(row, ref || bottomSpacer);
    }
    updateSelectAll();
    if (!state.items.length) renderEmpty(state.mode === 'search' ? '没有找到匹配的文件' : '这个文件夹是空的', false);
    else clearEmpty();
  }

  function scheduleRows() {
    if (listFrame !== null) return;
    listFrame = requestAnimationFrame(function () {
      listFrame = null;
      if (state.view === 'files') renderRows(listSeq);
    });
  }

  function renderEmpty(message, isError) {
    clearEmpty();
    var box = h('div', 'empty' + (isError ? ' empty-error' : ''));
    box.id = 'list-empty';
    box.appendChild(icon(isError ? 'error' : 'folder-open', 'empty-ic'));
    box.appendChild(h('div', 'empty-title', message));
    if (!isError) {
      box.appendChild(h('div', 'empty-sub', state.mode === 'search'
        ? '换个关键词试试，或清空搜索返回浏览。'
        : '把文件拖拽到窗口，或点击右上角「上传」。'));
    }
    el.listwrap.appendChild(box);
  }

  function clearEmpty() {
    var n = $('list-empty');
    if (n && n.parentNode) n.parentNode.removeChild(n);
  }

  function renderListFoot() {
    var foot = el.listfoot;
    foot.textContent = '';
    if (state.loading && state.items.length) {
      var l = h('div', 'foot-line');
      l.appendChild(h('span', 'spinner'));
      l.appendChild(h('span', null, '加载中…'));
      foot.appendChild(l);
      return;
    }
    if (!state.loadedOnce) return;
    if (state.hasMore) {
      foot.appendChild(h('div', 'foot-line muted', '向下滚动加载更多'));
      return;
    }
    var totalText = state.total >= 0
      ? '已全部加载，共 ' + state.total + ' 项'
      : (state.items.length ? '没有更多了' : '');
    if (!totalText) return;
    foot.appendChild(h('div', 'foot-line muted', totalText));
  }

  function updateSelectAll() {
    var box = el.checkAll;
    var loaded = state.items.length;
    var selCount = 0;
    state.selected.forEach(function (fid) { if (state.itemMap[fid]) selCount += 1; });
    box.checked = loaded > 0 && selCount === loaded;
    box.indeterminate = selCount > 0 && selCount < loaded;
    box.disabled = loaded === 0;

    var n = state.selected.size;
    if (n > 0) {
      el.bulkbar.hidden = false;
      el.bulkCount.textContent = '已选 ' + n + ' 项' + (selCount !== n ? '（含已隐藏项）' : '');
    } else {
      el.bulkbar.hidden = true;
    }
  }

  function setSelected(fid, on) {
    fid = String(fid);
    if (on) state.selected.add(fid); else state.selected.delete(fid);
    var row = rowMap.get(fid);
    if (row) {
      row.classList.toggle('selected', on);
      var cb = row.querySelector('input[type=checkbox]');
      if (cb) cb.checked = on;
    }
    updateSelectAll();
  }

  function clearSelection() {
    var fids = Array.from(state.selected);
    state.selected.clear();
    fids.forEach(function (fid) {
      var row = rowMap.get(fid);
      if (row) {
        row.classList.remove('selected');
        var cb = row.querySelector('input[type=checkbox]');
        if (cb) cb.checked = false;
      }
    });
    state.lastIndex = -1;
    updateSelectAll();
  }

  /**
   * 切换选中态。
   * 重要：调用方**不能**对复选框的 click 调用 preventDefault —— 浏览器会在事件派发结束后
   * 把 checked 回滚到点击前的值，导致"状态已选中但复选框显示未勾选"。
   * 因此这里以复选框原生切换后的 checked 为准，再同步 state。
   */
  function toggleSelect(fid, index, e) {
    fid = String(fid);
    var shift = e && e.shiftKey;
    var ctrl = e && (e.ctrlKey || e.metaKey);
    var row = rowMap.get(fid);
    var box = row ? row.querySelector('input[type=checkbox]') : null;
    var checkboxClick = e && e.target && e.target.type === "checkbox";
    var on = checkboxClick && box ? !!box.checked : !state.selected.has(fid);

    if (shift && state.lastIndex >= 0) {
      var a = Math.min(state.lastIndex, index);
      var b = Math.max(state.lastIndex, index);
      for (var i = a; i <= b; i++) {
        var it = state.items[i];
        if (it) {
          if (on) state.selected.add(String(it.fid)); else state.selected.delete(String(it.fid));
        }
      }
      renderRows(listSeq);
      state.lastIndex = index;
      return;
    }
    if (ctrl) {
      setSelected(fid, on);
      state.lastIndex = index;
      return;
    }
    clearSelection();
    if (on) setSelected(fid, true);
    state.lastIndex = index;
  }

  /* ================================================================== *
   * 文件操作
   * ================================================================== */

  var OPS = [
    { key: 'download', title: '下载', icon: 'download' },
    { key: 'rename', title: '重命名', icon: 'rename' },
    { key: 'move', title: '移动到', icon: 'move' },
    { key: 'copy', title: '复制到', icon: 'copy' },
    { key: 'remove', title: '删除', icon: 'trash', danger: true }
  ];

  function currentDirName() {
    var c = state.crumbs[state.crumbs.length - 1];
    return c ? c.name : '全部文件';
  }

  async function openItem(item) {
    if (!item) return;
    if (item.dir) { navigate(item.fid, item.name); return; }
    downloadItems([item]);
  }

  /** 目录分页加载：把一条异步任务受理结果轮询到结束，再刷新列表。 */
  async function awaitTask(accepted, what) {
    if (!accepted) { setTimeout(reloadCurrent, 700); return; }
    if (accepted.done === true) { setTimeout(reloadCurrent, 300); return; }
    var taskId = accepted.taskId;
    if (!taskId) { setTimeout(reloadCurrent, 900); return; }
    for (var i = 0; i < TASK_POLL_MAX; i++) {
      await sleep(TASK_POLL_MS);
      try {
        var arr = await API.fs.taskStatus({ taskIds: [taskId] });
        var st = arr && arr[0];
        if (st && st.done === true) {
          reloadCurrent();
          if (what) toast('info', what + '完成');
          return;
        }
      } catch (err) {
        reportError(err, '任务状态查询失败，尚未确认操作完成');
        reloadCurrent();
        return;
      }
    }
    toast('warn', '尚未确认任务完成，请刷新后检查结果');
    reloadCurrent();
  }

  function sleep(ms) { return new Promise(function (r) { setTimeout(r, ms); }); }

  async function downloadItems(items) {
    var files = (items || []).filter(function (i) { return i && !i.dir; });
    if (!files.length) { toast('warn', '所选项目中没有可下载的文件'); return; }
    var dir = state.settings.downloadDir;
    if (!dir) {
      try {
        dir = await API.dialog.saveDir();
      } catch (err) { reportError(err, '选择下载目录失败'); return; }
      if (!dir) { toast('warn', '未选择下载目录，已取消下载'); return; }
      await saveSettings({ downloadDir: dir });
    }
    try {
      var ids = await API.transfer.download({
        fids: files.map(function (f) { return f.fid; }),
        localDir: dir,
        names: files.map(function (f) { return f.name; })
      });
      var n = Array.isArray(ids) ? ids.length : files.length;
      toast('info', '已加入下载队列：' + n + ' 个文件');
      refreshTransfers();
    } catch (err) {
      reportError(err, '下载失败');
    }
  }

  async function doUploadPaths(paths) {
    if (!paths || !paths.length) return;
    if (state.mode === 'search') {
      toast('warn', '搜索状态下请先返回浏览目录再上传');
      return;
    }
    try {
      var ids = await API.transfer.upload({ localPaths: paths, pdirFid: state.fid });
      var n = Array.isArray(ids) ? ids.length : paths.length;
      toast('info', '已加入上传队列：' + n + ' 个文件，上传到「' + currentDirName() + '」');
      refreshTransfers();
      setView('transfers');
    } catch (err) {
      reportError(err, '上传失败');
    }
  }

  async function onUploadClick() {
    if (!API.injected) { toast('warn', '未注入桥接层，无法上传'); return; }
    try {
      var paths = await API.dialog.pickFiles();
      if (!paths || !paths.length) return;
      await doUploadPaths(paths);
    } catch (err) {
      reportError(err, '选择文件失败');
    }
  }

  async function onMkdirClick() {
    if (state.mode === 'search') { toast('warn', '搜索状态下不能新建文件夹，请先返回浏览目录'); return; }
    var name = await promptDialog({
      title: '新建文件夹',
      message: '将在「' + currentDirName() + '」下创建文件夹。',
      placeholder: '请输入文件夹名称',
      confirmText: '创建',
      icon: 'folder-plus',
      validate: validateName
    });
    if (name === null) return;
    try {
      var accepted = await API.fs.mkdir({ pdirFid: state.fid, name: name });
      toast('info', '已提交创建「' + name + '」');
      awaitTask(accepted, '创建文件夹');
    } catch (err) {
      reportError(err, '新建文件夹失败');
    }
  }

  function validateName(name) {
    if (/[\\/:*?"<>|]/.test(name)) return '名称不能包含 \\ / : * ? " < > | 等字符';
    if (name.length > 255) return '名称过长';
    if (name === '.' || name === '..') return '名称无效';
    return null;
  }

  async function onRename(item) {
    if (!item) return;
    var name = await promptDialog({
      title: '重命名',
      message: '原名称：' + item.name,
      value: item.name,
      confirmText: '保存',
      icon: 'rename',
      validate: validateName
    });
    if (name === null || name === item.name) return;
    try {
      var accepted = await API.fs.rename({ fid: item.fid, name: name });
      item.name = name;
      var row = rowMap.get(String(item.fid));
      if (row) updateRow(row, item);
      toast('info', '已重命名为「' + name + '」');
      // 契约中 rename 返回 TaskAccepted：服务端无 task_id 时是 {done:true}，
      // 若确实返回了异步任务则同样轮询到结束再刷新。
      if (accepted && accepted.taskId && accepted.done !== true) {
        awaitTask(accepted, '重命名');
      }
    } catch (err) {
      reportError(err, '重命名失败');
    }
  }

  async function doTransferOp(kind, items) {
    var list = (items || []).filter(Boolean);
    if (!list.length) return;
    var isMove = kind === 'move';
    var exclude = list.filter(function (i) { return i.dir; }).map(function (i) { return String(i.fid); });
    var targetFid = await pickDirDialog({
      title: isMove ? '选择移动到的目录' : '选择复制到的目录',
      confirmText: isMove ? '移动到此处' : '复制到此处',
      excludeFids: exclude
    });
    if (targetFid === null || targetFid === undefined) return;
    if (isMove && String(targetFid) === String(state.fid)) {
      toast('warn', '目标目录与当前目录相同，无需移动');
      return;
    }
    var targetName = await dirNameOf(targetFid);
    var ok = await confirmDialog({
      title: isMove ? '确认移动' : '确认复制',
      message: (isMove ? '将移动 ' : '将复制 ') + list.length + ' 个项目到「' + targetName + '」',
      detail: isMove
        ? '移动后原位置将不再显示这些项目。'
        : '复制会在目标目录生成副本，可能占用额外容量。',
      confirmText: isMove ? '移动' : '复制',
      list: list.map(function (i) { return i.name + (i.dir ? '（文件夹）' : ''); })
    });
    if (!ok) return;
    try {
      var accepted = isMove
        ? await API.fs.move({ fids: list.map(function (i) { return i.fid; }), toFid: targetFid })
        : await API.fs.copy({ fids: list.map(function (i) { return i.fid; }), toFid: targetFid });
      toast('info', '已提交' + (isMove ? '移动' : '复制') + '：' + list.length + ' 个项目');
      clearSelection();
      awaitTask(accepted, isMove ? '移动' : '复制');
      refreshQuota();
    } catch (err) {
      reportError(err, isMove ? '移动失败' : '复制失败');
    }
  }

  async function dirNameOf(fid) {
    var found = state.crumbs.filter(function (c) { return String(c.fid) === String(fid); })[0];
    if (found) return found.name;
    if (String(fid) === ROOT_FID) return '全部文件';
    try {
      var it = await API.fs.info(fid);
      if (it && it.name) return it.name;
    } catch (e) { /* 忽略 */ }
    return '所选目录';
  }

  async function doRemove(items, opts) {
    var list = (items || []).filter(Boolean);
    if (!list.length) return;
    var o = opts || {};
    var folderCount = list.filter(function (i) { return i.dir; }).length;
    // 二次确认：删除一律为危险操作，按钮红色
    var ok = await confirmDialog({
      title: o.bulk ? '确认批量删除' : '确认删除',
      message: '即将删除 ' + list.length + ' 个项目' + (folderCount ? '（其中文件夹 ' + folderCount + ' 个）' : '') + '，删除后可在网盘回收站找回。',
      detail: '此操作不可在当前客户端撤销。',
      confirmText: '删除',
      danger: true,
      list: list.map(function (i) { return i.name + (i.dir ? '（文件夹）' : ''); })
    });
    if (!ok) return;
    try {
      var accepted = await API.fs.remove({ fids: list.map(function (i) { return i.fid; }) });
      toast('info', '已提交删除：' + list.length + ' 个项目');
      clearSelection();
      awaitTask(accepted, '删除');
      refreshQuota();
    } catch (err) {
      reportError(err, '删除失败');
    }
  }

  async function onBulk(action) {
    var items = selectedItems();
    if (action === 'clear') { clearSelection(); return; }
    if (!items.length) { toast('warn', '请先选择项目'); return; }
    if (action === 'download') return downloadItems(items);
    if (action === 'move') return doTransferOp('move', items);
    if (action === 'copy') return doTransferOp('copy', items);
    if (action === 'remove') return doRemove(items, { bulk: true });
  }

  /* ================================================================== *
   * 传输列表
   * ================================================================== */

  function transferActions(t) {
    var acts = [];
    if (t.status === 'running' || t.status === 'pending') {
      acts.push({ key: 'pause', title: '暂停', icon: 'pause' });
      acts.push({ key: 'cancel', title: '取消', icon: 'cancel', danger: true });
    } else if (t.status === 'paused') {
      acts.push({ key: 'resume', title: '继续', icon: 'resume' });
      acts.push({ key: 'cancel', title: '取消', icon: 'cancel', danger: true });
    } else if (t.status === 'error') {
      acts.push({ key: 'retry', title: '重试', icon: 'retry' });
      acts.push({ key: 'cancel', title: '取消', icon: 'cancel', danger: true });
    } else {
      acts.push({ key: 'open', title: '打开所在文件夹', icon: 'folder-open' });
    }
    if (t.status === 'done' || t.status === 'canceled' || t.status === 'error') {
      acts.push({ key: 'clear', title: '清除记录', icon: 'close' });
    }
    return acts;
  }

  function createTransferRow(t) {
    var row = h('div', 'trow');
    row.dataset.id = t.id;

    var ic = h('span', 'ticon');
    ic.appendChild(icon(t.kind === 'upload' ? 'upload' : 'download'));
    row.appendChild(ic);

    var info = h('div', 'tinfo');
    var line1 = h('div', 'tname-row');
    line1.appendChild(h('span', 'tname', t.name));
    line1.appendChild(h('span', 'tdir ' + (t.kind === 'upload' ? 'up' : 'down'), t.kind === 'upload' ? '上传' : '下载'));
    info.appendChild(line1);

    var meta = h('div', 'tmeta');
    meta.appendChild(h('span', 'tstate', STATUS_TEXT[t.status] || t.status));
    meta.appendChild(h('span', 'tdot', '·'));
    meta.appendChild(h('span', 'tbytes', ''));
    meta.appendChild(h('span', 'tspeed', ''));
    meta.appendChild(h('span', 'teta', ''));
    var terr = h('span', 'terror');
    terr.hidden = true;
    meta.appendChild(terr);
    info.appendChild(meta);

    var bar = h('div', 'tbar');
    bar.appendChild(h('i'));
    info.appendChild(bar);
    row.appendChild(info);

    var ops = h('div', 'tops');
    ops.dataset.ops = '';
    row.appendChild(ops);

    updateTransferRow(row, t);
    return row;
  }

  function updateTransferRow(row, t) {
    var pct = t.size > 0 ? Math.min(100, Math.max(0, (t.transferred / t.size) * 100)) : (t.status === 'done' ? 100 : 0);
    var bar = row.querySelector('.tbar > i');
    if (bar && bar.dataset.pct !== String(pct)) {
      bar.dataset.pct = String(pct);
      bar.style.width = pct.toFixed(2) + '%';
    }
    var stateEl = row.querySelector('.tstate');
    var stText = STATUS_TEXT[t.status] || t.status;
    if (stateEl && stateEl.textContent !== stText) stateEl.textContent = stText;
    row.dataset.status = t.status;

    var bytes = fmtSize(t.transferred) + ' / ' + fmtSize(t.size);
    var bytesEl = row.querySelector('.tbytes');
    if (bytesEl && bytesEl.textContent !== bytes) bytesEl.textContent = bytes;

    var sp = t.status === 'running' ? fmtSpeed(t.speed) : '';
    var spEl = row.querySelector('.tspeed');
    if (spEl && spEl.textContent !== sp) spEl.textContent = sp;

    var eta = t.status === 'running' ? fmtEta(t.eta) : '';
    var etaEl = row.querySelector('.teta');
    if (etaEl && etaEl.textContent !== eta) etaEl.textContent = eta;

    var errEl = row.querySelector('.terror');
    var errText = t.status === 'error'
      ? ('失败：' + (t.error || '未知错误') + (t.errorCode ? '（' + t.errorCode + '）' : '') +
        (t.attempts ? ' · 已重试 ' + t.attempts + ' 次' : ''))
      : '';
    if (errEl) {
      var showErr = !!errText;
      if (errEl.hidden === showErr) errEl.hidden = !showErr;
      if (errEl.textContent !== errText) errEl.textContent = errText;
    }

    var ops = row.querySelector('.tops');
    var wantKeys = transferActions(t).map(function (a) { return a.key; }).join(',');
    if (ops.dataset.ops !== wantKeys) {
      ops.dataset.ops = wantKeys;
      ops.textContent = '';
      transferActions(t).forEach(function (a) {
        var b = h('button', 'op' + (a.danger ? ' danger' : ''));
        b.type = 'button';
        b.dataset.tact = a.key;
        b.title = a.title;
        b.setAttribute('aria-label', a.title);
        b.appendChild(icon(a.icon));
        ops.appendChild(b);
      });
    }
  }

  function renderTransfers() {
    var list = state.transfers || [];
    var alive = new Set();
    list.forEach(function (t) {
      alive.add(t.id);
      var row = transferMap.get(t.id);
      if (!row) {
        row = createTransferRow(t);
        transferMap.set(t.id, row);
        el.tlist.appendChild(row);
      } else {
        updateTransferRow(row, t);
      }
    });
    transferMap.forEach(function (row, id) {
      if (!alive.has(id)) {
        if (row.parentNode) row.parentNode.removeChild(row);
        transferMap.delete(id);
      }
    });

    var active = list.filter(function (t) { return t.status === 'running' || t.status === 'pending'; }).length;
    var finished = list.filter(function (t) { return t.status === 'done' || t.status === 'canceled'; }).length;
    el.transfersSummary.textContent = list.length
      ? ('共 ' + list.length + ' 个任务' + (active ? '，进行中 ' + active + ' 个' : '') + (finished ? '，已结束 ' + finished + ' 个' : ''))
      : '';

    if (!list.length) {
      if (!el.tlist.querySelector('.empty')) {
        var box = h('div', 'empty');
        box.appendChild(icon('transfer', 'empty-ic'));
        box.appendChild(h('div', 'empty-title', '暂无传输任务'));
        box.appendChild(h('div', 'empty-sub', '上传或下载文件后，任务会显示在这里。'));
        el.tlist.appendChild(box);
      }
    } else {
      var old = el.tlist.querySelector('.empty');
      if (old) old.parentNode.removeChild(old);
    }

    if (active > 0) {
      el.transferBadge.hidden = false;
      el.transferBadge.textContent = String(active);
    } else {
      el.transferBadge.hidden = true;
    }
  }

  async function refreshTransfers() {
    var seq = ++transferSeq;
    try {
      var list = await API.transfer.list();
      if (seq !== transferSeq) return;
      state.transfers = Array.isArray(list) ? list : [];
      state.transfersLoaded = true;
      renderTransfers();
    } catch (err) {
      if (!API.injected) return;
      reportError(err, '读取传输列表失败');
    }
  }

  async function onTransferAction(id, act) {
    var t = (state.transfers || []).filter(function (x) { return x.id === id; })[0];
    try {
      if (act === 'pause') { await API.transfer.pause(id); toast('info', '已暂停'); }
      else if (act === 'resume') { await API.transfer.resume(id); toast('info', '已继续'); }
      else if (act === 'retry') { await API.transfer.retry(id); toast('info', '已开始重试'); }
      else if (act === 'cancel') { await API.transfer.cancel(id); toast('info', '已取消'); }
      else if (act === 'clear') { await API.transfer.clear([id]); }
      else if (act === 'open') {
        var done = false;
        if (API.has('transfer', 'openItem')) {
          try {
            var r = await API.transfer.openItem(id);
            // Tauri commands commonly return JSON null after a successful shell action.
            done = r !== false;
          } catch (e) { done = false; }
        }
        if (!done) {
          var p = t && (t.localDir || t.localPath);
          if (p) { await API.dialog.showInFolder(p); }
          else { toast('warn', '该任务没有可定位的本地路径'); }
        }
      }
      refreshTransfers();
    } catch (err) {
      reportError(err, '操作失败');
    }
  }

  async function clearFinished() {
    var ids = (state.transfers || []).filter(function (t) {
      return t.status === 'done' || t.status === 'canceled';
    }).map(function (t) { return t.id; });
    if (!ids.length) { toast('info', '没有已结束的任务需要清除'); return; }
    var ok = await confirmDialog({
      title: '清除传输记录',
      message: '将从列表中移除 ' + ids.length + ' 个已结束的任务记录。',
      detail: '仅清除记录，不会删除已下载的本地文件或云盘文件。',
      confirmText: '清除'
    });
    if (!ok) return;
    try {
      await API.transfer.clear(ids);
      toast('info', '已清除 ' + ids.length + ' 条记录');
      refreshTransfers();
    } catch (err) {
      reportError(err, '清除记录失败');
    }
  }

  /* ================================================================== *
   * 设置
   * ================================================================== */

  function applyTheme(theme) {
    var t = theme === 'dark' ? 'dark' : 'light';
    document.documentElement.setAttribute('data-theme', t);
    Array.prototype.forEach.call(el.themeSeg.querySelectorAll('.seg'), function (b) {
      var on = b.dataset.theme === t;
      b.classList.toggle('active', on);
      b.setAttribute('aria-checked', on ? 'true' : 'false');
    });
  }

  async function loadSettings() {
    el.envHint.textContent = API.bridgeInfo().text;
    try {
      var s = await API.settings.get();
      if (s && typeof s === 'object') {
        state.settings = {
          downloadDir: s.downloadDir || '',
          uploadConcurrency: Number(s.uploadConcurrency) || 2,
          downloadConcurrency: Number(s.downloadConcurrency) || 2,
          downloadParallel: Number(s.downloadParallel) || 16,
          downloadStrategy: ['auto', 'sequential', 'parallel'].indexOf(s.downloadStrategy) >= 0 ? s.downloadStrategy : 'auto',
          uploadPartConcurrency: Number(s.uploadPartConcurrency) || 3,
          theme: s.theme === 'dark' ? 'dark' : 'light'
        };
        applyTheme(state.settings.theme);
        renderSettings();
        return;
      }
    } catch (err) {
      if (API.injected && !API.isMock) reportError(err, '读取设置失败');
    }
    applyTheme(state.settings.theme);
    renderSettings();
  }

  function renderSettings() {
    el.dlDir.value = state.settings.downloadDir || '';
    el.dlDir.placeholder = state.settings.downloadDir ? '' : '尚未设置（首次下载时会提示选择）';
    el.upConc.value = state.settings.uploadConcurrency;
    el.downConc.value = state.settings.downloadConcurrency;
    if (el.upPart) el.upPart.value = state.settings.uploadPartConcurrency;
    if (el.downParallel) el.downParallel.value = state.settings.downloadParallel;
    if (el.downStrategy) el.downStrategy.value = state.settings.downloadStrategy;
    applyTheme(state.settings.theme);
    var a = state.account;
    el.setAccountHint.textContent = !API.injected
      ? '未注入桥接层，无法读取登录状态。'
      : (a && a.expired ? '登录状态已失效，请点击「重新登录」。'
        : (a && a.loggedIn ? '当前已登录：' + (a.nickname || '未知账号') + '（' + (a.memberLabel || '普通用户') + '）'
          : '当前未登录。'));
  }

  async function saveSettings(patch) {
    try {
      var s = await API.settings.set(patch);
      if (s && typeof s === 'object') Object.assign(state.settings, s);
      applyTheme(state.settings.theme);
      renderSettings();
      return true;
    } catch (err) {
      renderSettings();
      reportError(err, '保存设置失败');
      return false;
    }
  }

  async function pickDownloadDir() {
    try {
      var dir = await API.dialog.saveDir(state.settings.downloadDir || undefined);
      if (!dir) return;
      if (await saveSettings({ downloadDir: dir })) toast('info', '下载目录已更新');
    } catch (err) {
      reportError(err, '选择目录失败');
    }
  }

  /* ================================================================== *
   * 视图切换
   * ================================================================== */

  function setView(view) {
    state.view = view;
    Array.prototype.forEach.call(el.nav.querySelectorAll('.nav-item'), function (b) {
      b.classList.toggle('active', b.dataset.view === view);
    });
    el.viewFiles.hidden = view !== 'files';
    el.viewTransfers.hidden = view !== 'transfers';
    el.viewSettings.hidden = view !== 'settings';
    setTopbarForView();
    if (view === 'files') scheduleRows();
    if (view === 'transfers') refreshTransfers();
    if (view === 'settings') renderSettings();
  }

  /* ================================================================== *
   * 拖拽上传
   * ================================================================== */

  function setupDragDrop() {
    var depth = 0;

    function hasFiles(e) {
      var dt = e.dataTransfer;
      if (!dt) return false;
      if (dt.types && Array.prototype.indexOf.call(dt.types, 'Files') >= 0) return true;
      return !!(dt.files && dt.files.length);
    }

    window.addEventListener('dragenter', function (e) {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth += 1;
      if (state.view !== 'files') return;
      el.dropmask.hidden = false;
      el.dropSub.textContent = '文件将上传到「' + currentDirName() + '」';
    });

    window.addEventListener('dragover', function (e) {
      if (!hasFiles(e)) return;
      e.preventDefault();
      if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
    });

    window.addEventListener('dragleave', function (e) {
      if (!hasFiles(e)) return;
      depth = Math.max(0, depth - 1);
      if (depth === 0) el.dropmask.hidden = true;
    });

    window.addEventListener('drop', async function (e) {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth = 0;
      el.dropmask.hidden = true;
      if (API.has('dialog', 'onFilesDropped')) return; // Tauri delivers native paths separately.
      var files = Array.prototype.slice.call(e.dataTransfer.files || []);
      if (!files.length) return;
      if (state.view !== 'files') {
        toast('warn', '请先切换到「全部文件」再拖拽上传');
        setView('files');
        return;
      }
      await handleDroppedFiles(files);
    });
  }

  async function handleDroppedFiles(files) {
    if (!API.injected) { toast('warn', '未注入桥接层，无法上传文件'); return; }
    if (!API.has('dialog', 'getPathForFile')) {
      toast('warn', '当前桥接层未提供文件路径读取能力，拖拽上传不可用，请使用「上传」按钮选择文件。', 7000);
      return;
    }
    var paths = [];
    var failed = [];
    files.forEach(function (f) {
      var p = API.dialog.getPathForFile(f);
      if (p) paths.push(p); else failed.push(f.name);
    });
    if (!paths.length) {
      toast('warn', '无法获取拖入文件的真实路径，拖拽上传不可用，请使用「上传」按钮选择文件。', 7000);
      return;
    }
    if (failed.length) {
      toast('warn', failed.length + ' 个文件无法获取真实路径，已跳过；请使用「上传」按钮补充上传。', 6000);
    }
    await doUploadPaths(paths);
  }

  /* ================================================================== *
   * 事件绑定
   * ================================================================== */

  function bindEvents() {
    // 导航
    el.nav.addEventListener('click', function (e) {
      var btn = e.target.closest ? e.target.closest('.nav-item') : null;
      if (btn) setView(btn.dataset.view);
    });

    // 搜索
    el.searchInput.addEventListener('keydown', function (e) {
      if (e.key === 'Enter' && !e.isComposing) {
        e.preventDefault();
        runSearch(el.searchInput.value.trim());
      } else if (e.key === 'Escape') {
        el.searchInput.value = '';
        el.searchClear.hidden = true;
        leaveSearch();
      }
    });
    el.searchInput.addEventListener('input', function () {
      el.searchClear.hidden = !el.searchInput.value;
    });
    el.searchClear.addEventListener('click', function () {
      el.searchInput.value = '';
      el.searchClear.hidden = true;
      leaveSearch();
      el.searchInput.focus();
    });

    // 顶部按钮
    el.btnUpload.addEventListener('click', onUploadClick);
    el.btnMkdir.addEventListener('click', onMkdirClick);
    el.btnRefresh.addEventListener('click', function () {
      if (state.mode === 'search') runSearch(state.keyword); else reloadCurrent();
    });

    // 列表滚动：无限加载
    el.listwrap.addEventListener('scroll', function () {
      if (state.view !== 'files') return;
      scheduleRows();
      if (state.loading || !state.hasMore) return;
      if (el.listwrap.scrollTop + el.listwrap.clientHeight >= el.listwrap.scrollHeight - 240) {
        loadPage({});
      }
    }, { passive: true });
    window.addEventListener('resize', scheduleRows);
    if (typeof ResizeObserver === 'function') {
      var listResizeObserver = new ResizeObserver(scheduleRows);
      listResizeObserver.observe(el.listwrap);
      unsubs.push(function () { listResizeObserver.disconnect(); });
    }

    // 列表交互（事件委托）
    el.list.addEventListener('click', function (e) {
      var t = e.target;
      var row = t.closest ? t.closest('.row') : null;
      if (!row) return;
      var fid = row.dataset.fid;
      var idx = state.items.findIndex(function (i) { return String(i.fid) === String(fid); });
      if (idx < 0) return;

      if (t.closest('.cb')) {
        if (t.type !== 'checkbox') return;
        // 不调用 preventDefault：让复选框自行切换后再同步选中态
        toggleSelect(fid, idx, e);
        return;
      }
      var opBtn = t.closest('.op');
      if (opBtn) {
        e.stopPropagation();
        runRowOp(opBtn.dataset.op, state.items[idx]);
        return;
      }
      var nameEl = t.closest('.fname');
      if (nameEl && e.detail === 1) {
        // 单击名称：仅选中；双击才进入（见 dblclick）
        if (!(e.ctrlKey || e.metaKey || e.shiftKey)) {
          var wasSelected = state.selected.has(String(fid));
          clearSelection();
          if (!wasSelected || state.selected.size !== 1) setSelected(fid, true);
          state.lastIndex = idx;
        } else {
          toggleSelect(fid, idx, e);
        }
      }
    });

    el.list.addEventListener('dblclick', function (e) {
      var row = e.target.closest ? e.target.closest('.row') : null;
      if (!row) return;
      if (e.target.closest('.op') || e.target.closest('.cb')) return;
      var item = findItem(row.dataset.fid);
      if (item) openItem(item);
    });

    el.list.addEventListener('contextmenu', function (e) {
      var row = e.target.closest ? e.target.closest('.row') : null;
      if (!row) return;
      e.preventDefault();
      var item = findItem(row.dataset.fid);
      if (!item) return;
      if (!state.selected.has(String(item.fid))) {
        clearSelection();
        setSelected(item.fid, true);
      }
      var entries = [];
      if (!item.dir) entries.push({ label: '下载', icon: 'download', run: function () { downloadItems([item]); } });
      if (item.dir) entries.push({ label: '打开', icon: 'folder-open', run: function () { openItem(item); } });
      entries.push({ label: '重命名', icon: 'rename', run: function () { onRename(item); } });
      entries.push({ label: '移动到…', icon: 'move', run: function () { doTransferOp('move', selectedItems()); } });
      entries.push({ label: '复制到…', icon: 'copy', run: function () { doTransferOp('copy', selectedItems()); } });
      entries.push({ sep: true });
      entries.push({ label: '删除', icon: 'trash', danger: true, run: function () { doRemove(selectedItems()); } });
      openCtx(e.clientX, e.clientY, entries);
    });

    el.checkAll.addEventListener('change', function () {
      if (el.checkAll.checked) {
        state.items.forEach(function (i) { state.selected.add(String(i.fid)); });
        rowMap.forEach(function (row, fid) {
          row.classList.add('selected');
          var cb = row.querySelector('input[type=checkbox]');
          if (cb) cb.checked = true;
        });
      } else {
        clearSelection();
      }
      updateSelectAll();
    });

    // 批量操作条
    el.bulkbar.addEventListener('click', function (e) {
      var b = e.target.closest ? e.target.closest('[data-bulk]') : null;
      if (b) onBulk(b.dataset.bulk);
    });

    // 传输列表
    el.tlist.addEventListener('click', function (e) {
      var b = e.target.closest ? e.target.closest('.op') : null;
      if (!b) return;
      var row = b.closest('.trow');
      if (row) onTransferAction(row.dataset.id, b.dataset.tact);
    });
    el.btnClearFinished.addEventListener('click', clearFinished);

    // 设置
    el.btnPickDir.addEventListener('click', pickDownloadDir);
    el.btnOpenDir.addEventListener('click', async function () {
      if (!state.settings.downloadDir) { toast('warn', '尚未设置下载目录'); return; }
      try { await API.dialog.openPath(state.settings.downloadDir); }
      catch (err) { reportError(err, '打开目录失败'); }
    });
    el.upConc.addEventListener('change', function () {
      var v = clampInt(el.upConc.value, 1, 8, 2);
      el.upConc.value = v;
      saveSettings({ uploadConcurrency: v });
    });
    if (el.upPart) el.upPart.addEventListener('change', function () {
      var v = clampInt(el.upPart.value, 1, 8, 3);
      el.upPart.value = v;
      saveSettings({ uploadPartConcurrency: v });
    });
    el.downConc.addEventListener('change', function () {
      var v = clampInt(el.downConc.value, 1, 5, 2);
      el.downConc.value = v;
      saveSettings({ downloadConcurrency: v });
    });
    if (el.downParallel) el.downParallel.addEventListener('change', function () {
      var v = clampInt(el.downParallel.value, 1, 16, 16);
      el.downParallel.value = v;
      saveSettings({ downloadParallel: v });
    });
    if (el.downStrategy) el.downStrategy.addEventListener('change', function () {
      saveSettings({ downloadStrategy: el.downStrategy.value });
    });
    el.themeSeg.addEventListener('click', function (e) {
      var b = e.target.closest ? e.target.closest('.seg') : null;
      if (!b) return;
      saveSettings({ theme: b.dataset.theme });
    });
    el.btnVerify.addEventListener('click', async function () {
      try {
        var s = await API.auth.verify();
        applyAccount(s);
        if (s && s.expired) toast('warn', '登录状态已失效');
        else if (s && s.loggedIn) toast('info', '登录状态有效');
        else toast('warn', '当前未登录，请先登录');
      } catch (err) { reportError(err, '校验登录状态失败'); }
    });
    el.btnLogin2.addEventListener('click', doLogin);
    el.btnLogout2.addEventListener('click', doLogout);
    el.btnEnvCheck.addEventListener('click', function () { location.reload(); });

    // 全局
    document.addEventListener('click', function (e) {
      if (!el.ctx.hidden && !e.target.closest('.ctxmenu')) closeCtx();
    });
    window.addEventListener('resize', closeCtx);
    window.addEventListener('blur', closeCtx);

    document.addEventListener('keydown', function (e) {
      var tag = (e.target && e.target.tagName) || '';
      var typing = tag === 'INPUT' || tag === 'TEXTAREA' || (e.target && e.target.isContentEditable);
      if (e.key === 'Escape' && !typing) {
        if (!el.ctx.hidden) { closeCtx(); return; }
        if (state.selected.size) { clearSelection(); return; }
      }
      if (typing) return;
      if (state.view !== 'files') return;
      if ((e.ctrlKey || e.metaKey) && (e.key === 'a' || e.key === 'A')) {
        e.preventDefault();
        el.checkAll.checked = true;
        el.checkAll.dispatchEvent(new Event('change'));
      } else if (e.key === 'Delete' && state.selected.size) {
        e.preventDefault();
        doRemove(selectedItems(), { bulk: state.selected.size > 1 });
      }
    });

    window.addEventListener('beforeunload', function () {
      unsubs.forEach(function (f) { try { f(); } catch (e) {} });
      unsubs = [];
    });
  }

  function clampInt(v, min, max, dflt) {
    var n = parseInt(v, 10);
    if (!isFinite(n)) return dflt;
    return Math.min(max, Math.max(min, n));
  }

  function runRowOp(op, item) {
    if (!item) return;
    if (op === 'download') return downloadItems([item]);
    if (op === 'rename') return onRename(item);
    if (op === 'move') return doTransferOp('move', [item]);
    if (op === 'copy') return doTransferOp('copy', [item]);
    if (op === 'remove') return doRemove([item]);
  }

  function runSearch(keyword) {
    if (!API.injected) { toast('warn', '未注入桥接层，无法搜索'); return; }
    var kw = String(keyword || '').trim();
    if (!kw) { leaveSearch(); return; }
    state.mode = 'search';
    state.keyword = kw;
    state.selected.clear();
    el.searchClear.hidden = false;
    resetList();
    renderCrumbs();
    loadPage({ reset: true });
  }

  function leaveSearch() {
    if (state.mode !== 'search') return;
    state.mode = 'browse';
    state.keyword = '';
    el.searchInput.value = '';
    el.searchClear.hidden = true;
    resetList();
    renderCrumbs();
    loadCrumbs(state.fid);
    loadPage({ reset: true });
  }

  /* ================================================================== *
   * 启动
   * ================================================================== */

  function cacheEls() {
    el.banners = $('banners');
    el.nav = $('nav');
    el.transferBadge = $('transfer-badge');
    el.sidebarFoot = $('sidebar-foot');
    el.btnQuit = $('btn-quit');
    el.crumbs = $('crumbs');
    el.searchBox = $('search-box');
    el.searchInput = $('search-input');
    el.searchClear = $('search-clear');
    el.btnRefresh = $('btn-refresh');
    el.btnMkdir = $('btn-mkdir');
    el.btnUpload = $('btn-upload');
    el.account = $('account');
    el.viewFiles = $('view-files');
    el.viewTransfers = $('view-transfers');
    el.viewSettings = $('view-settings');
    el.bulkbar = $('bulkbar');
    el.bulkCount = $('bulk-count');
    el.checkAll = $('check-all');
    el.listwrap = $('listwrap');
    el.list = $('list');
    el.listfoot = $('listfoot');
    el.tlist = $('tlist');
    el.transfersSummary = $('transfers-summary');
    el.btnClearFinished = $('btn-clear-finished');
    el.dlDir = $('set-download-dir');
    el.upConc = $('set-up-conc');
    el.downConc = $('set-down-conc');
    el.upPart = $('set-up-part');
    el.downParallel = $('set-down-parallel');
    el.downStrategy = $('set-down-strategy');
    el.themeSeg = $('set-theme');
    el.btnPickDir = $('btn-pick-dir');
    el.btnOpenDir = $('btn-open-dir');
    el.btnVerify = $('btn-verify');
    el.btnLogin2 = $('btn-login-2');
    el.btnLogout2 = $('btn-logout-2');
    el.envHint = $('set-env');
    el.btnEnvCheck = $('btn-env-check');
    el.setAccountHint = $('set-account-hint');
    el.dropmask = $('dropmask');
    el.dropSub = $('dropmask-sub');
    el.toasts = $('toasts');
    el.overlayRoot = $('overlay-root');
    el.ctx = $('ctxmenu');
  }

  function subscribeChannels() {
    unsubs.push(API.auth.onChange(function (s) { applyAccount(s); }));

    var onTasks = function (tasks) {
      if (!Array.isArray(tasks)) return;
      transferSeq += 1;
      state.transfers = tasks;
      state.transfersLoaded = true;
      renderTransfers();
    };
    unsubs.push(API.transfer.onUpdate(onTasks));

    unsubs.push(API.onToast(function (payload) {
      if (!payload || typeof payload !== 'object') return;
      var level = payload.level === 'warn' || payload.level === 'error' ? payload.level : 'info';
      if (payload.message) toast(level, payload.message);
    }));

    unsubs.push(API.onNeedLogin(function () { showExpiredBanner(); }));
  }

  async function boot() {
    cacheEls();
    if (el.btnQuit) {
      el.btnQuit.hidden = !API.has('app', 'quit');
      el.btnQuit.addEventListener('click', async function () {
        try { await API.app.quit(); }
        catch (err) { reportError(err, '退出客户端失败'); }
      });
    }
    bindEvents();
    setupDragDrop();
    unsubs.push(API.dialog.onFilesDropped(function (paths) {
      el.dropmask.hidden = true;
      if (state.view !== 'files') {
        toast('warn', '请先切换到「全部文件」再拖拽上传');
        setView('files');
        return;
      }
      doUploadPaths(paths).catch(function (err) { reportError(err, '拖拽上传失败'); });
    }));

    var info = API.bridgeInfo();
    el.envHint.textContent = info.text;
    el.sidebarFoot.textContent = API.isMock ? '开发假数据模式' : (API.injected ? '' : '未连接后端');

    if (!API.injected) {
      showNoBridgeBanner(info);
      // 无桥接层：仍渲染骨架与空态，不白屏、不报错
      renderCrumbs();
      renderEmpty('未注入桥接层，无法读取文件列表', true);
      el.checkAll.disabled = true;
      setTopbarForView();
      renderAccount();
      renderTransfers();
      renderSettings();
      return;
    }

    if (info.mock) {
      banner('mock', 'info', '开发假数据模式（?mock=1）：列表与传输任务均为本地假数据，未连接真实后端。', []);
    } else if (info.missing && info.missing.length) {
      banner('bridge', 'warn', '桥接层缺少以下方法：' + info.missing.join('、') + '，相关功能将不可用。', []);
    }

    subscribeChannels();
    applyTheme(state.settings.theme);
    setView('files');
    renderCrumbs();
    renderAccount();
    loadSettings();

    try {
      var acct = await API.auth.status();
      applyAccount(acct);
      if (acct && acct.expired) showExpiredBanner();
      if (acct && acct.loggedIn) refreshQuota();
    } catch (err) {
      if (API.isNeedLogin(err)) showExpiredBanner();
      else reportError(err, '读取登录状态失败');
    }

    await loadPage({ reset: true });
    refreshTransfers();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }

  // 便于在浏览器控制台做自检（例如 QKUI.state）
  window.QKUI = {
    state: state,
    toast: toast,
    setView: setView,
    reload: reloadCurrent,
    navigate: navigate,
    loadPage: loadPage,
    getRowCount: function () { return rowMap.size; },
    version: '1.0.0'
  };
})();
