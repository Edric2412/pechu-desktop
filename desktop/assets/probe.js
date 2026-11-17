// A TIMING PROBE for finding out why the page is slow to appear in the desktop window. It is only added to the page when the
// program is started with PECHU_PROBE=1 (see crates/server/src/assets.rs), never for normal use. It watches the page load and
// the first seconds of drawing, then sends one summary line to the server, which prints it (POST /api/native/probe).
(() => {
  'use strict';
  const marks = {};
  const mark = (name) => { if (!(name in marks)) marks[name] = Math.round(performance.now()); };

  document.addEventListener('DOMContentLoaded', () => mark('domContentLoaded_ms'));
  window.addEventListener('load', () => mark('load_ms'));
  new MutationObserver(() => {
    const app = document.getElementById('app');
    if (app && app.children.length) mark('appHasContent_ms');
    if (document.querySelector('.backdrop, [class*="backdrop"] svg, body > svg, #backdrop')) mark('backdropInDom_ms');
  }).observe(document, { childList: true, subtree: true });

  // how smoothly the page is drawn: the time between animation frames for the first ~4 seconds
  const frames = [];
  let last = performance.now();
  const tick = (now) => {
    frames.push(now - last);
    last = now;
    if (now < 4500) requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);

  setTimeout(() => {
    const paints = {};
    for (const p of performance.getEntriesByType('paint')) paints[p.name + '_ms'] = Math.round(p.startTime);
    const resources = performance.getEntriesByType('resource');
    const slowFrames = frames.filter((f) => f > 50).length;
    const report = {
      ...marks,
      ...paints,
      resources: resources.length,
      slowestResource_ms: Math.round(Math.max(0, ...resources.map((r) => r.responseEnd - r.startTime))),
      frames: frames.length,
      frame_avg_ms: Math.round(frames.reduce((a, b) => a + b, 0) / Math.max(1, frames.length)),
      frame_max_ms: Math.round(Math.max(0, ...frames)),
      frames_over_50ms: slowFrames,
      first_frame_gaps_ms: frames.slice(0, 8).map(Math.round),
    };
    // JSON, not plain text: the server refuses plain-text POSTs on purpose (a cross-site protection, like the web version's)
    fetch('/api/native/probe', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(report), keepalive: true });
  }, 8000);

  // End-to-end check of F11 (see window-keys.js): press it from here, then see whether the window really changed size.
  const size = () => ({ inner: [innerWidth, innerHeight], screen: [screen.width, screen.height] });
  setTimeout(() => {
    const before = size();
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'F11', bubbles: true, cancelable: true }));
    setTimeout(() => {
      fetch('/api/native/probe', { method: 'POST', headers: { 'content-type': 'application/json' }, keepalive: true,
        body: JSON.stringify({ f11_before: before, f11_after: size() }) });
    }, 2000);
  }, 9000);
})();
