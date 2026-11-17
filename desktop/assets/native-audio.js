// Desktop app only (the server adds this script to the page; the web version never loads it).
//
// The web view does not list "Monitor of ..." inputs, so on its own the UI's "Device audio" switch has nothing to offer unless
// the person has set up a loopback device. This script adds ONE extra input to the list the UI already reads:
//   "Monitor of this computer (system audio)"
// and, when the UI opens that input, supplies a normal audio track made from the sound the app's own server captures
// (GET /api/native/system-audio). The UI itself is not touched: it sees an input whose label contains "Monitor", exactly as it
// would in a browser that lists them.
//
// Harmless where the routes do not exist: the status check fails and nothing is added.
(() => {
  'use strict';
  const devices = navigator.mediaDevices;
  if (!devices || typeof devices.enumerateDevices !== 'function' || typeof devices.getUserMedia !== 'function') return;

  const DEVICE_ID = 'pechu-system-audio';
  const LABEL = 'Monitor of this computer (system audio)';
  const realEnumerate = devices.enumerateDevices.bind(devices);
  const realGetUserMedia = devices.getUserMedia.bind(devices);

  // Asked once per page: can the app capture this computer's sound at all?
  let availability = null;
  const available = () => {
    availability ??= fetch('/api/native/system-audio/status', { credentials: 'same-origin' })
      .then((r) => (r.ok ? r.json() : { available: false }))
      .then((j) => j.available === true)
      .catch(() => false);
    return availability;
  };

  devices.enumerateDevices = async () => {
    const list = await realEnumerate();
    if (!(await available())) return list;
    const ours = { deviceId: DEVICE_ID, kind: 'audioinput', label: LABEL, groupId: 'pechu' };
    ours.toJSON = () => ({ deviceId: ours.deviceId, kind: ours.kind, label: ours.label, groupId: ours.groupId });
    return [...list, ours];
  };

  // The constraint can be {deviceId: 'x'}, {deviceId: {exact: 'x'}}, {deviceId: {ideal: 'x'}} or a list; the UI uses exact.
  const asksForSystemAudio = (constraints) => {
    const audio = constraints && constraints.audio;
    if (!audio || typeof audio !== 'object') return false;
    let id = audio.deviceId;
    if (id && typeof id === 'object' && !Array.isArray(id)) id = id.exact ?? id.ideal;
    if (Array.isArray(id)) id = id[0];
    return id === DEVICE_ID;
  };

  devices.getUserMedia = (constraints) => (asksForSystemAudio(constraints) ? systemAudioStream() : realGetUserMedia(constraints));

  const failed = (why) => new DOMException(why, 'NotReadableError');

  // Opens the socket, waits for the format message, then plays every PCM chunk into an audio track.
  async function systemAudioStream() {
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    const socket = new WebSocket(`${scheme}://${location.host}/api/native/system-audio`);
    socket.binaryType = 'arraybuffer';

    const format = await new Promise((resolve, reject) => {
      socket.onmessage = (e) => {
        if (typeof e.data !== 'string') return;
        try {
          const message = JSON.parse(e.data);
          if (message.type === 'format' && message.sampleRate > 0 && message.channels > 0) resolve(message);
        } catch { /* not ours */ }
      };
      socket.onerror = () => reject(failed('System audio could not be captured.'));
      socket.onclose = () => reject(failed('System audio could not be captured.'));
    });

    // The context runs at the capture's own rate, so nothing has to be resampled.
    const context = new AudioContext({ sampleRate: format.sampleRate });
    await context.resume();
    const destination = context.createMediaStreamDestination();
    const stream = destination.stream;
    const track = stream.getAudioTracks()[0];

    const LEAD = 0.2; // seconds of sound kept in hand, so a late chunk does not make a gap
    const MAX_LAG = 0.8; // if we fall this far behind, skip ahead instead of drifting
    let playAt = 0;

    socket.onmessage = (e) => {
      if (typeof e.data === 'string') return;
      const samples = new Int16Array(e.data);
      const frames = Math.floor(samples.length / format.channels);
      if (!frames) return;
      const buffer = context.createBuffer(format.channels, frames, format.sampleRate);
      for (let channel = 0; channel < format.channels; channel++) {
        const out = buffer.getChannelData(channel);
        for (let i = 0; i < frames; i++) out[i] = samples[i * format.channels + channel] / 32768; // the data is interleaved
      }
      const now = context.currentTime;
      if (playAt < now + 0.02 || playAt > now + MAX_LAG) playAt = now + LEAD;
      const source = context.createBufferSource();
      source.buffer = buffer;
      source.connect(destination);
      source.start(playAt);
      playAt += frames / format.sampleRate;
    };

    let finished = false;
    const finish = () => {
      if (finished) return;
      finished = true;
      socket.onmessage = socket.onerror = socket.onclose = null;
      try { socket.close(); } catch { /* already closed */ }
      try { context.close(); } catch { /* already closed */ }
    };
    // The UI stops its tracks when the recording ends; that is also what stops the capture on the server.
    const stopTrack = track.stop.bind(track);
    track.stop = () => { finish(); stopTrack(); };
    socket.onclose = socket.onerror = () => {
      if (finished) return;
      finish();
      track.dispatchEvent(new Event('ended')); // the server stopped (for example the app is closing)
    };
    return stream;
  }
})();
