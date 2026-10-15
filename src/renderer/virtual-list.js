'use strict';
/** Fixed-height range math shared by the DOM list and its offline checks. */
(function (global) {
  function range(count, scrollTop, viewportHeight, rowHeight, overscan) {
    count = Math.max(0, Math.floor(Number(count) || 0));
    rowHeight = Math.max(1, Number(rowHeight) || 46);
    overscan = Math.max(0, Math.floor(Number(overscan) || 0));
    viewportHeight = Math.max(rowHeight, Number(viewportHeight) || rowHeight);
    var top = Math.max(0, Math.min(Number(scrollTop) || 0, Math.max(0, count * rowHeight - viewportHeight)));
    var start = Math.max(0, Math.floor(top / rowHeight) - overscan);
    var end = Math.min(count, Math.ceil((top + viewportHeight) / rowHeight) + overscan);
    return { start: start, end: end, before: start * rowHeight, after: (count - end) * rowHeight };
  }
  var api = Object.freeze({ range: range });
  if (typeof module === 'object' && module.exports) module.exports = api;
  else global.QKVirtualList = api;
})(typeof window === 'object' ? window : globalThis);
