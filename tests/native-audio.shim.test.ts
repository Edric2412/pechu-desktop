// desktop/assets/native-audio.js adds a "Monitor of this computer" input to the desktop app's page. It is loaded here with a fake
// navigator, WebSocket and AudioContext, so these tests need no window, no sound and no server.
import { expect, test } from "bun:test";
import { join } from "node:path";

const SOURCE = await Bun.file(join(import.meta.dir, "..", "desktop", "assets", "native-audio.js")).text();
const UI_MONITOR_WORDS = /monitor|loopback|blackhole|soundflower|stereo mix|cable output|virtual/i; // public/js/pages/record.js isLoopback

class FakeSocket {
  static all: FakeSocket[] = [];
  binaryType = "";
  closed = false;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  constructor(public url: string) { FakeSocket.all.push(this); }
  close() { this.closed = true; }
  say(data: unknown) { this.onmessage?.({ data }); }
}

function fakeBrowser(status: () => Promise<{ ok: boolean; json: () => Promise<unknown> }>) {
  FakeSocket.all = [];
  const calls: unknown[] = [];
  const starts: number[] = [];
  const track = { stopped: false, stop() { this.stopped = true; }, dispatchEvent() { return true; } };
  const context = { closed: false, currentTime: 1, sampleRate: 0 };
  class FakeContext {
    currentTime = 1;
    constructor(opts: { sampleRate: number }) { context.sampleRate = opts.sampleRate; }
    async resume() {}
    close() { context.closed = true; }
    createMediaStreamDestination() { return { stream: { getAudioTracks: () => [track] } }; }
    createBuffer(channels: number, frames: number) {
      const data = Array.from({ length: channels }, () => new Float32Array(frames));
      return { channels: data, getChannelData: (i: number) => data[i]! };
    }
    createBufferSource() { return { connect() {}, buffer: null as unknown, start: (t: number) => { starts.push(t); } }; }
  }
  const navigator = {
    mediaDevices: {
      enumerateDevices: async () => [{ deviceId: "mic1", kind: "audioinput", label: "Built-in microphone" }],
      getUserMedia: async (c: unknown) => { calls.push(c); return { real: true }; },
    },
  };
  const location = { protocol: "http:", host: "127.0.0.1:47317" };
  let statusAsked = 0;
  const fetchFake = async (url: string) => { statusAsked++; expect(url).toBe("/api/native/system-audio/status"); return status(); };
  new Function("navigator", "location", "fetch", "WebSocket", "AudioContext", "DOMException", "Event", SOURCE)(
    navigator, location, fetchFake, FakeSocket, FakeContext, DOMException, Event,
  );
  return { devices: navigator.mediaDevices, calls, starts, track, context, statusAsked: () => statusAsked };
}
const yes = async () => ({ ok: true, json: async () => ({ available: true }) });

test("when system audio cannot be captured, the device list is exactly what the browser gave", async () => {
  for (const status of [
    async () => ({ ok: true, json: async () => ({ available: false, reason: "no PipeWire" }) }),
    async () => ({ ok: false, json: async () => ({}) }),
    async () => { throw new Error("network down"); },
  ]) {
    const b = fakeBrowser(status);
    expect((await b.devices.enumerateDevices()).map((d: any) => d.deviceId)).toEqual(["mic1"]);
  }
});

test("when it can, one extra audio input appears after the real ones, with a label the UI treats as device audio", async () => {
  const b = fakeBrowser(yes);
  const list: any[] = await b.devices.enumerateDevices();
  expect(list.map((d) => d.deviceId)).toEqual(["mic1", "pechu-system-audio"]);
  expect(list[1].kind).toBe("audioinput");
  expect(UI_MONITOR_WORDS.test(list[1].label)).toBe(true);
  await b.devices.enumerateDevices();
  expect(b.statusAsked()).toBe(1); // the question is asked once per page
});

test("every other getUserMedia call goes to the browser untouched", async () => {
  const b = fakeBrowser(yes);
  for (const c of [{ audio: true }, { audio: { deviceId: { exact: "mic1" } } }, { video: true }, undefined]) await b.devices.getUserMedia(c as any);
  expect(b.calls).toEqual([{ audio: true }, { audio: { deviceId: { exact: "mic1" } } }, { video: true }, undefined]);
  expect(FakeSocket.all.length).toBe(0);
});

test("asking for the system audio input opens the socket, plays what arrives, and stopping the track stops the capture", async () => {
  const b = fakeBrowser(yes);
  const pending = b.devices.getUserMedia({ audio: { deviceId: { exact: "pechu-system-audio" }, echoCancellation: false } });
  const socket = FakeSocket.all[0]!;
  expect(socket.url).toBe("ws://127.0.0.1:47317/api/native/system-audio");
  socket.say(JSON.stringify({ type: "format", sampleRate: 48000, channels: 2 }));
  const stream: any = await pending;
  expect(b.context.sampleRate).toBe(48000); // no resampling: the context runs at the capture's rate
  expect(stream.getAudioTracks()[0]).toBe(b.track);

  // two interleaved stereo frames: (L=16384,R=-16384) and (L=32767,R=0)
  socket.say(new Int16Array([16384, -16384, 32767, 0]).buffer);
  socket.say(new Int16Array([0, 0]).buffer);
  expect(b.starts.length).toBe(2);
  expect(b.starts[1]!).toBeCloseTo(b.starts[0]! + 2 / 48000, 6); // the second chunk starts right where the first ends

  expect(socket.closed).toBe(false);
  stream.getAudioTracks()[0].stop();
  expect(socket.closed).toBe(true);
  expect(b.context.closed).toBe(true);
  expect(b.track.stopped).toBe(true);
});

test("if the socket fails before it says its format, the call fails the way an unreadable microphone does", async () => {
  const b = fakeBrowser(yes);
  const pending = b.devices.getUserMedia({ audio: { deviceId: "pechu-system-audio" } }); // the plain-string form too
  FakeSocket.all[0]!.onclose?.();
  await expect(pending).rejects.toMatchObject({ name: "NotReadableError" });
});

test("the script does nothing in a page that has no media devices", () => {
  expect(() => new Function("navigator", "location", "fetch", "WebSocket", "AudioContext", "DOMException", "Event", SOURCE)({}, {}, null, null, null, null, null)).not.toThrow();
});
