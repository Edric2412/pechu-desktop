// Desktop app only (the server adds this script to the page; the web version never loads it).
//
// The desktop window has no title bar, so it has no maximize button either. This script makes F11 toggle fullscreen, as in a
// browser. The page itself cannot control the window (it has no native powers on purpose), so it just asks the app's own
// server, which passes the request on to the window.
(() => {
  'use strict';
  if (!document || typeof document.addEventListener !== 'function' || typeof fetch !== 'function') return;

  document.addEventListener('keydown', (e) => {
    // plain F11 only: Ctrl/Alt/Shift/Meta+F11 are left to the system
    if (e.key !== 'F11' || e.ctrlKey || e.altKey || e.shiftKey || e.metaKey) return;
    e.preventDefault(); // the web engine's own F11 does nothing useful here
    if (e.repeat) return; // holding the key must not flip the window back and forth
    // JSON, not plain text: the server refuses plain-text POSTs on purpose (a cross-site protection)
    fetch('/api/native/window/fullscreen', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'content-type': 'application/json' },
      body: '{}',
    }).catch(() => { /* the app is closing */ });
  });
})();
