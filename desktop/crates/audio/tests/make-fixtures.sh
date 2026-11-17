#!/usr/bin/env bash
# Regenerates the small audio files in tests/fixtures/. The tests only READ these files, so nothing here is needed to
# run `cargo test`; run this script only when a fixture has to be rebuilt. Needs python3 and gst-launch-1.0 (GStreamer).
#
# Every file holds the same 2-second signal: a 440 Hz tone from 0.0 s to 0.8 s, silence from 0.8 s to 1.2 s,
# and the tone again from 1.2 s to 2.0 s. The tests check that the length, the tone and the gap survive decoding.
set -euo pipefail
cd "$(dirname "$0")/fixtures"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

python3 -I - "$work/source.wav" <<'PY'
import math, struct, sys, wave
rate = 44100
frames = bytearray()
for i in range(rate * 2):
    t = i / rate
    tone = not (0.8 <= t < 1.2)
    v = int(12000 * math.sin(2 * math.pi * 440 * t)) if tone else 0
    frames += struct.pack("<hh", v, v)  # stereo, same in both channels
with wave.open(sys.argv[1], "wb") as w:
    w.setnchannels(2); w.setsampwidth(2); w.setframerate(rate); w.writeframes(bytes(frames))
PY

# encode NAME RATE CHANNELS "encoder and muxer elements"
encode() {
  gst-launch-1.0 -q filesrc location="$work/source.wav" ! wavparse ! audioconvert ! audioresample ! \
    "audio/x-raw,rate=$2,channels=$3" ! $4 ! filesink location="$1"
}
encode tone-8k-mono.wav     8000  1 "wavenc"
encode tone-16k-mono.wav    16000 1 "wavenc"
encode tone-8k-mono.aiff    8000  1 "aiffmux"
encode tone-44k-stereo.flac 44100 2 "flacenc"
encode tone-44k-stereo.mp3  44100 2 "lamemp3enc target=bitrate bitrate=32 cbr=true"
encode tone-44k-stereo.ogg  44100 2 "vorbisenc ! oggmux"
encode tone-44k-stereo.m4a  44100 2 "avenc_aac bitrate=48000 ! aacparse ! mp4mux"
encode tone-48k-mono.webm   48000 1 "opusenc bitrate=24000 ! webmmux"
# like the browser's recorder: the file does not say how long it is, and clusters have no size
encode tone-48k-mono-live.webm 48000 1 "opusenc bitrate=24000 ! webmmux streamable=true"
encode tone-48k-mono.opus   48000 1 "opusenc bitrate=24000 ! oggmux"
ls -l
