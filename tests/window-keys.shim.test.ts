// desktop/assets/window-keys.js makes F11 toggle fullscreen in the desktop window (the window has no title bar). It is loaded
// here with a fake document and fetch, so the test needs no window and no server.
import { expect, test } from "bun:test";
import { join } from "node:path";

const SOURCE = await Bun.file(join(import.meta.dir, "..", "desktop", "assets", "window-keys.js")).text();

function fakePage() {
  const calls: { url: string; init: any }[] = [];
  let handler: ((e: any) => void) | null = null;
  const document = { addEventListener: (type: string, fn: (e: any) => void) => { if (type === "keydown") handler = fn; } };
  const fetchFake = async (url: string, init: any) => { calls.push({ url, init }); return { ok: true }; };
  new Function("document", "fetch", SOURCE)(document, fetchFake);
  const press = (key: string, extra: object = {}) => {
    const e = { key, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; }, ...extra };
    handler?.(e);
    return e;
  };
  return { calls, press };
}

test("F11 asks the server to toggle fullscreen, once per press, and the browser's own F11 is suppressed", () => {
  const page = fakePage();
  const e = page.press("F11");
  expect(e.defaultPrevented).toBe(true);
  expect(page.calls.length).toBe(1);
  expect(page.calls[0]!.url).toBe("/api/native/window/fullscreen");
  expect(page.calls[0]!.init.method).toBe("POST");
  expect(page.calls[0]!.init.headers["content-type"]).toBe("application/json"); // the server refuses plain-text POSTs
  expect(page.calls[0]!.init.credentials).toBe("same-origin");
});

test("no other key does anything, and F11 with a modifier is left alone", () => {
  const page = fakePage();
  for (const k of ["F10", "F12", "Escape", "f", "Enter"]) expect(page.press(k).defaultPrevented).toBe(false);
  expect(page.press("F11", { ctrlKey: true }).defaultPrevented).toBe(false);
  expect(page.press("F11", { altKey: true }).defaultPrevented).toBe(false);
  expect(page.calls.length).toBe(0);
});

test("a held-down key does not flip the window back and forth", () => {
  const page = fakePage();
  page.press("F11");
  page.press("F11", { repeat: true });
  page.press("F11", { repeat: true });
  expect(page.calls.length).toBe(1);
});

test("the script does nothing where there is no page to listen to", () => {
  expect(() => new Function("document", "fetch", SOURCE)(undefined, undefined)).not.toThrow();
});
