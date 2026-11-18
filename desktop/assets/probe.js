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

  // Scroll smoothness: scroll a long page of glass panels down and up for 3 s and record the time between frames, once for the
  // page as it is and then with one more suspect switched off each time (see `scenarios` below).
  const scrollRun = (ms = 3000) => new Promise((resolve) => {
    const max = Math.max(1, document.documentElement.scrollHeight - innerHeight);
    const gaps = [];
    let start = 0, prev = 0;
    const step = (now) => {
      if (!start) start = prev = now;
      else { gaps.push(now - prev); prev = now; }
      const t = (now - start) / ms; // 0..1
      scrollTo(0, max * (1 - Math.abs(2 * t - 1))); // down, then back up
      if (t < 1) requestAnimationFrame(step);
      else resolve({
        frames: gaps.length,
        avg_ms: +(gaps.reduce((a, b) => a + b, 0) / Math.max(1, gaps.length)).toFixed(1),
        worst_ms: Math.round(Math.max(0, ...gaps)),
        over_33ms: gaps.filter((g) => g > 33).length,
      });
    };
    requestAnimationFrame(step);
  });
  setTimeout(async () => {
    const filler = document.createElement('div');
    filler.style.cssText = 'display:grid;gap:16px;margin-top:24px';
    for (let i = 0; i < 14; i++) {
      const p = document.createElement('section');
      p.className = 'panel';
      p.innerHTML = '<h2>Panel ' + (i + 1) + '</h2>' + '<p>Meeting notes and transcript text for the scroll test. </p>'.repeat(8);
      filler.append(p);
    }
    (document.getElementById('app') || document.body).append(filler);
    // Each scenario adds one more style on top of the previous ones (so the cost of each piece shows as the drop from the line above).
    // The first line is the page as it is; the last repeats it with everything removed again, to show how much the machine drifted.
    const scenarios = [
      ['as_is', ''],
      ['no_fixed_backdrop', '.backdrop { display: none !important; }'],
      ['no_view_layer', '.view { will-change: auto !important; }'],
      ['no_panel_shadow', '.panel { box-shadow: none !important; }'],
      ['no_panel_border_radius', '.panel { border-radius: 0 !important; }'],
    ];
    const results = {};
    const style = document.createElement('style');
    document.head.append(style);
    let css = '';
    const settle = () => new Promise((r) => setTimeout(r, 600));
    for (const [name, add] of scenarios) {
      css += add + '\n'; style.textContent = css;
      scrollTo(0, 0); await settle();
      results[name] = await scrollRun();
    }
    style.textContent = ''; scrollTo(0, 0); await settle();
    results.as_is_again = await scrollRun();
    style.remove(); filler.remove(); scrollTo(0, 0);
    fetch('/api/native/probe', { method: 'POST', headers: { 'content-type': 'application/json' }, keepalive: true,
      body: JSON.stringify({ scroll: results, lenis: document.documentElement.classList.contains('lenis') }) });

    // The Record page, idle (nothing recording): what do its always-running pieces cost? Same idea as above: each scenario switches
    // off one more suspect. `js_ms` is the time the page's own animation-frame code takes per frame (the waveform drawing and
    // maths); `avg_ms` / `slow` are the frame gaps as before.
    location.hash = '#/record';
    await new Promise((r) => setTimeout(r, 2500));
    let jsAcc = 0;
    const rawRaf = window.requestAnimationFrame.bind(window);
    window.requestAnimationFrame = (cb) => rawRaf((t) => { const s = performance.now(); try { cb(t); } finally { jsAcc += performance.now() - s; } });
    const idleRun = async (ms = 3000) => {
      jsAcc = 0;
      const gaps = []; let prev = 0;
      await new Promise((resolve) => { const step = (now) => { if (prev) gaps.push(now - prev); prev = now; if (gaps.length && now - start > ms) resolve(); else rawRaf(step); }; const start = performance.now(); rawRaf(step); });
      const n = Math.max(1, gaps.length);
      return { frames: gaps.length, avg_ms: +(gaps.reduce((a, b) => a + b, 0) / n).toFixed(1), worst_ms: Math.round(Math.max(0, ...gaps)), slow: gaps.filter((g) => g > 25).length, js_ms: +(jsAcc / n).toFixed(2) };
    };
    const origSet = CSSStyleDeclaration.prototype.setProperty, origFill = CanvasRenderingContext2D.prototype.fill, origStroke = CanvasRenderingContext2D.prototype.stroke;
    const steps = [
      ['as_is', () => {}],
      ['no_button_halo', () => { style.textContent = '.rec-btn { animation: none !important; }'; }],
      ['no_loudness_glow_update', () => { CSSStyleDeclaration.prototype.setProperty = function (n, v, p) { return n === '--lvl' ? undefined : origSet.call(this, n, v, p); }; }],
      ['no_canvas_painting', () => { CanvasRenderingContext2D.prototype.fill = () => {}; CanvasRenderingContext2D.prototype.stroke = () => {}; }],
      ['wave_hidden', () => { style.textContent += '\n.wave-wrap canvas { display: none !important; }'; }],
    ];
    const rec = {};
    document.head.append(style);
    for (const [name, apply] of steps) { apply(); await settle(); rec[name] = await idleRun(); }
    CSSStyleDeclaration.prototype.setProperty = origSet; CanvasRenderingContext2D.prototype.fill = origFill; CanvasRenderingContext2D.prototype.stroke = origStroke;
    style.textContent = ''; await settle();
    rec.as_is_again = await idleRun();
    style.remove(); window.requestAnimationFrame = rawRaf; location.hash = '#/';
    fetch('/api/native/probe', { method: 'POST', headers: { 'content-type': 'application/json' }, keepalive: true, body: JSON.stringify({ record_page: rec }) });
  }, 12000);

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
