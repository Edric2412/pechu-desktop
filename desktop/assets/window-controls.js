// Desktop app only (the server adds this script to the page; the web version never loads it).
//
// The desktop window has no title bar, so this script draws a small cluster of its own at the top-right of the page: a grip to
// drag the window by (double-click maximizes), then minimize, maximize and close. The page itself cannot control the window (it
// has no native powers on purpose), so every button just asks the app's own server, which passes the request on to the window.
// The look is in window-controls.css. The cluster is hidden while the window is fullscreen (F11).
(() => {
  'use strict';
  if (!document || typeof document.createElement !== 'function' || typeof fetch !== 'function') return;

  const send = (command) => {
    // JSON, not plain text: the server refuses plain-text POSTs on purpose (a cross-site protection)
    fetch('/api/native/window/' + command, {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'content-type': 'application/json' },
      body: '{}',
    }).catch(() => { /* the app is closing */ });
  };

  // 16px outline icons, 1.5px round stroke: the same set and style as the page's own icons (public/js/ui.js)
  const icon = (paths) => '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' + paths + '</svg>';
  const ICONS = {
    grip: '<path d="M6 4.5h.01M10 4.5h.01M6 8h.01M10 8h.01M6 11.5h.01M10 11.5h.01"/>',
    minimize: '<path d="M4 11.5h8"/>',
    maximize: '<rect x="3.5" y="3.5" width="9" height="9" rx="1.8"/>',
    close: '<path d="M4 4l8 8M12 4l-8 8"/>',
  };

  const build = () => {
    const root = document.createElement('div');
    root.className = 'pw-controls';
    root.setAttribute('role', 'group');
    root.setAttribute('aria-label', 'Window controls');

    // the grip is not a button: dragging with the mouse has no keyboard form (the window manager's own keys move the window)
    const grip = document.createElement('div');
    grip.className = 'pw-grip';
    grip.setAttribute('aria-hidden', 'true');
    grip.innerHTML = icon(ICONS.grip);
    let doubled = false; // the second press of a double-click already maximized, so the dblclick event must not do it again
    grip.addEventListener('mousedown', (e) => {
      if (e.button !== 0) return; // the primary button only
      e.preventDefault(); // no text selection while dragging
      doubled = e.detail >= 2;
      send(doubled ? 'maximize' : 'drag'); // the second press of a double-click may never reach the page once a drag has begun
    });
    grip.addEventListener('dblclick', () => {
      if (!doubled) send('maximize');
      doubled = false;
    });
    root.append(grip);

    for (const [command, label] of [['minimize', 'Minimize'], ['maximize', 'Maximize or restore'], ['close', 'Close']]) {
      const b = document.createElement('button');
      b.type = 'button';
      b.className = 'pw-btn pw-' + command;
      b.setAttribute('aria-label', label);
      b.innerHTML = icon(ICONS[command]);
      b.addEventListener('click', () => send(command));
      root.append(b);
    }
    document.body.append(root);

    // fullscreen (F11): the page fills the whole screen, and a window needs no buttons then
    const sync = () => { root.hidden = window.innerWidth === screen.width && window.innerHeight === screen.height; };
    window.addEventListener('resize', sync);
    sync();
  };

  if (document.body) build();
  else document.addEventListener('DOMContentLoaded', build);
})();
