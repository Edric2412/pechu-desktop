// Sarvam's list prices in rupees per minute of audio, before tax (docs.sarvam.ai pricing page, November 2026).
// Shown next to the buttons that spend money, so the cost is known before pressing them.
export const PER_MINUTE = 0.49;
export const PER_MINUTE_SPEAKERS = 0.59; // speaker labels are billed higher

// "about ₹25" when the length of the recording is known, otherwise "₹0.49 a minute"
export function priceLabel(durationSec, speakers) {
  const rate = speakers ? PER_MINUTE_SPEAKERS : PER_MINUTE;
  if (!(durationSec > 0)) return `₹${rate.toFixed(2)} a minute`;
  return `about ₹${Math.max(1, Math.ceil((durationSec / 60) * rate))}`;
}

// Which meetings get the "Improve with AI" prompt: any with a saved recording and a transcript, once Sarvam is switched on.
// Not while a job is running, and not without a recording to listen to again.
export const canImprove = (m, sarvamOn) => !!(sarvamOn && m?.audio && m?.transcript?.trim() && (m.status === 'ready' || m.status === 'error'));

// A live meeting that Sarvam has not listened to yet: its transcript is still the live captions.
export const isLiveCaptions = (m) => m?.source === 'live' && !m?.transcriptLive;

// The few lines the "Improve with AI" pop-up shows. Sarvam has no way to read the balance through its API, so the person is
// pointed at "Credits Left" on the Sarvam dashboard instead.
export function improveLines({ transcriber, durationSec, fromLive = true }) {
  const lines = ['Sarvam will listen to the saved recording again for a more accurate transcript.'];
  const sarvam = /sarvam/i.test(transcriber || '');
  lines.push(fromLive && sarvam ? 'The live captions already used Sarvam, so this roughly doubles the cost for this meeting.'
    : sarvam ? 'Sarvam already transcribed this recording, so this costs about the same again.'
    : 'This uses Sarvam credits.');
  const check = 'Pechu cannot see your Sarvam balance, so check “Credits Left” in your Sarvam dashboard first.';
  lines.push(durationSec > 0 ? `It needs ${priceLabel(durationSec, false)} in credits. ${check}` : check);
  return lines;
}

// What a meeting has cost so far, from the usage the server keeps on it (server/usage.ts). Sarvam is priced in rupees at the
// rates above (live captions are assumed to cost the same per minute as the saved recording); Gemini is shown as the work it did,
// because its price depends on the model and plan. Returns null for a meeting with nothing recorded (older meetings, or none used).
const mins = (sec) => (sec < 30 ? 'under a minute' : `${Math.round(sec / 60)} min`);
const tokens = (n) => (n >= 1000 ? `${Math.round(n / 1000)}k` : String(n));
export function spendSummary(usage) {
  const u = usage || {};
  const live = u.sarvamLiveSec || 0, saved = u.sarvamSec || 0, speakers = u.sarvamSpeakerSec || 0;
  const rows = [];
  let rupees = 0;
  if (live + saved + speakers > 0) {
    rupees = Math.max(1, Math.ceil(((live + saved) / 60) * PER_MINUTE + (speakers / 60) * PER_MINUTE_SPEAKERS));
    const parts = [live && `${mins(live)} live`, saved + speakers && `${mins(saved + speakers)} of the saved recording`].filter(Boolean);
    rows.push({ label: 'Sarvam cost', value: `about ₹${rupees}`, detail: parts.join(' + ') });
  }
  const calls = u.geminiCalls || 0, gl = u.geminiLiveSec || 0;
  if (calls || gl) {
    const parts = [calls && `${calls} request${calls === 1 ? '' : 's'}, ${tokens(u.geminiTokensIn || 0)} tokens in and ${tokens(u.geminiTokensOut || 0)} out`, gl && `${mins(gl)} of live captions`].filter(Boolean);
    rows.push({ label: 'Gemini used', value: parts.join(' · '), detail: '' });
  }
  return rows.length ? { rupees, rows } : null;
}
