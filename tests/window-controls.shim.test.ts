// desktop/assets/window-controls.js draws the desktop window's own minimize / maximize / close buttons (the window has no title
// bar). It is loaded here with a minimal fake page and fetch, so the test needs no window, no browser and no server.
import { expect, test } from "bun:test";
import { join } from "node:path";

const SOURCE = await Bun.file(join(import.meta.dir, "..", "desktop", "assets", "window-controls.js")).text();

class FakeEl {
  className = "";
  innerHTML = "";
  hidden = false;
  type = "";
  attrs: Record<string, string> = {};
  kids: FakeEl[] = [];
  listeners: Record<string, ((e: any) => void)[]> = {};
  constructor(public tag: string) {}
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  append(...k: FakeEl[]) { this.kids.push(...k); }
  addEventListener(type: string, fn: (e: any) => void) { (this.listeners[type] ??= []).push(fn); }
  fire(type: string, extra: object = {}) {
    const e = { button: 0, detail: 1, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; }, ...extra };
    for (const fn of this.listeners[type] ?? []) fn(e);
    return e;
  }
  find(pred: (el: FakeEl) => boolean): FakeEl | undefined {
    if (pred(this)) return this;
    for (const k of this.kids) { const f = k.find(pred); if (f) return f; }
    return undefined;
  }
}

function fakePage(opts: { loading?: boolean } = {}) {
  const calls: { url: string; init: any }[] = [];
  const body = new FakeEl("body");
  const docListeners: Record<string, (() => void)[]> = {};
  const document: any = {
    body: opts.loading ? null : body,
    createElement: (tag: string) => new FakeEl(tag),
    addEventListener: (type: string, fn: () => void) => { (docListeners[type] ??= []).push(fn); },
  };
  const win: any = { listeners: [] as (() => void)[], innerWidth: 1280, innerHeight: 860,
    addEventListener(type: string, fn: () => void) { if (type === "resize") this.listeners.push(fn); } };
  const screen = { width: 1920, height: 1080 };
  const fetchFake = async (url: string, init: any) => { calls.push({ url, init }); return { ok: true }; };
  new Function("document", "fetch", "window", "screen", SOURCE)(document, fetchFake, win, screen);
  if (opts.loading) { document.body = body; for (const fn of docListeners.DOMContentLoaded ?? []) fn(); }
  const root = body.kids[0]!;
  const button = (label: string) => root.find((el) => el.attrs["aria-label"] === label)!;
  const grip = root.find((el) => el.className === "pw-grip")!;
  const resize = (w: number, h: number) => { win.innerWidth = w; win.innerHeight = h; for (const fn of win.listeners) fn(); };
  return { calls, root, button, grip, resize, screen, body };
}

const expectJsonPost = (call: { url: string; init: any }, command: string) => {
  expect(call.url).toBe("/api/native/window/" + command);
  expect(call.init.method).toBe("POST");
  expect(call.init.headers["content-type"]).toBe("application/json"); // the server refuses plain-text POSTs
  expect(call.init.credentials).toBe("same-origin");
  expect(call.init.body).toBe("{}");
};

test("each button asks the server for its own command, as a same-origin JSON POST", () => {
  const page = fakePage();
  page.button("Minimize").fire("click");
  page.button("Maximize or restore").fire("click");
  page.button("Close").fire("click");
  expect(page.calls.length).toBe(3);
  expectJsonPost(page.calls[0]!, "minimize");
  expectJsonPost(page.calls[1]!, "maximize");
  expectJsonPost(page.calls[2]!, "close");
});

test("the buttons are real buttons with accessible names, and the cluster is a labelled group", () => {
  const page = fakePage();
  const buttons = page.root.kids.filter((k) => k.tag === "button");
  expect(buttons.length).toBe(3);
  for (const b of buttons) {
    expect(b.type).toBe("button");
    expect(b.attrs["aria-label"]?.length).toBeGreaterThan(0);
    expect(b.innerHTML).toContain("<svg");
  }
  expect(page.root.attrs.role).toBe("group");
  expect(page.root.attrs["aria-label"]).toBe("Window controls");
  expect(page.body.kids).toEqual([page.root]);
});

test("the grip starts a drag only on a primary-button press", () => {
  const page = fakePage();
  const e = page.grip.fire("mousedown");
  expect(e.defaultPrevented).toBe(true); // no text selection while dragging
  expect(page.calls.length).toBe(1);
  expectJsonPost(page.calls[0]!, "drag");
  page.grip.fire("mousedown", { button: 2 });
  page.grip.fire("mousedown", { button: 1 });
  expect(page.calls.length).toBe(1);
});

test("a double-click on the grip maximizes once, whether or not the page sees the second press", () => {
  const page = fakePage();
  // second press arrives as a mousedown with a click count of 2, then the dblclick event
  page.grip.fire("mousedown", { detail: 1 });
  page.grip.fire("mousedown", { detail: 2 });
  page.grip.fire("dblclick");
  expect(page.calls.map((c) => c.url)).toEqual(["/api/native/window/drag", "/api/native/window/maximize"]);
  // a dblclick with no counted press before it still maximizes
  const other = fakePage();
  other.grip.fire("dblclick");
  expect(other.calls.map((c) => c.url)).toEqual(["/api/native/window/maximize"]);
});

test("the cluster hides while the window is fullscreen and shows again afterwards", () => {
  const page = fakePage();
  expect(page.root.hidden).toBe(false);
  page.resize(page.screen.width, page.screen.height);
  expect(page.root.hidden).toBe(true);
  page.resize(1280, 860);
  expect(page.root.hidden).toBe(false);
  page.resize(page.screen.width, page.screen.height - 40); // maximized under a panel is not fullscreen
  expect(page.root.hidden).toBe(false);
});

test("the cluster floats on the page: no box, background, border, shadow or blur around it", async () => {
  const css = await Bun.file(join(import.meta.dir, "..", "desktop", "assets", "window-controls.css")).text();
  const rule = css.match(/\.pw-controls\s*\{([^}]*)\}/)![1]!;
  for (const word of ["background", "border", "box-shadow", "backdrop-filter", "filter"]) expect(rule).not.toContain(word);
  expect(rule).toContain("position: fixed"); // out of the page's layout, so nothing moves
  expect(css).toContain(".pw-close:hover"); // the buttons keep their own states
  expect(css).toContain(":focus-visible");
});

test("it waits for the page body when the script runs in the head", () => {
  const page = fakePage({ loading: true });
  expect(page.body.kids.length).toBe(1);
  expect(page.button("Close")).toBeDefined();
});

test("the script does nothing where there is no page to draw on", () => {
  expect(() => new Function("document", "fetch", SOURCE)(undefined, undefined)).not.toThrow();
  expect(() => new Function("document", "fetch", SOURCE)({}, undefined)).not.toThrow();
});
