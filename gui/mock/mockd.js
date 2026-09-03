#!/usr/bin/env node
// mockd — a stand-in recalld that speaks docs/PROTOCOL.md v1 over a unix socket.
//
// It exists because the GUI must be developed and verified before the real
// daemon's socket lands, and because a mock is the only way to test the parts
// of the contract a real daemon makes hard to produce on demand: a replay-buffer
// overrun, a daemon restart with a reset sequence counter, a multi-second async
// delete, a relabel broadcast racing a live feed.
//
// Usage:
//   node mock/mockd.js [--sock PATH] [--feed-ms 2000] [--seq N] [--quiet]
//                      [--no-semantic]
//
// Defaults to $NX_RECALL_MOCK_SOCK, else $XDG_RUNTIME_DIR/nx-recall-mock.sock.
// It NEVER listens on TCP — same rule as the real daemon.
//
// Signals: SIGUSR1 simulates a daemon restart (drops every client and resets
// the sequence counter), which is exactly the case the GUI must resync from.
// It also bumps the advertised version string, because the restart the real
// daemon performs is the one it performs onto a REPLACED binary (0.5.3): the
// window that was already open is now the old client of a new daemon, and that
// string is the only place it can find that out.
//
// SIGUSR2 fires the events that a canned world cannot produce on its own,
// one per signal, in order:
//
//   1st — a `note` event: a MIC turn that began with a wake phrase (0.8.0).
//   2nd — one whole PARTIAL TURN (0.11.0): three `partial` events a second
//         apart, each carrying more of the same sentence, then the `segment`
//         that replaces them. The one thing a canned feed cannot produce by
//         accident, because the three partials and the segment have to share a
//         `t_start_ns` or the replace rule is untested.
//   3rd — a `reminder` and the `note` it is about, plus a `digest` for a
//         conversation the local model has just read (0.9.0).
//   4th — a `roster` JOIN naming a voice the user has named (Kira), which is
//         what a client turns into a brief.
//   5th — the SAME join again, immediately, so a client's brief debounce is
//         testable rather than merely assertable in prose.
//   6th — a `visit` event (0.10.0): a world entry, with its name. What the
//         Worlds card and the "Where you meet" chips are fed by.
//
// A signal rather than a timer because both are things a test has to be able to
// place: a note that arrives at second 19 of a two-minute run lands in whatever
// step happens to be on screen, and proves nothing about the one that cares.

import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';
// The highlight palette, from the renderer's own copy of it rather than a
// fourth transcription. `speakers.palette` is supposed to be the daemon
// telling a client which tokens exist, so a mock that carried its own list
// could drift from the GUI it is serving and the drift would look like a
// working feature. gui/test/palette.test.js already holds this file to
// crates/recalld/src/palette.rs, which is the list this stands in for.
import { PALETTE, accent } from '../src/renderer/lib/palette.js';

const PROTO = 1;
const DAEMON = 'recalld-mock/0.5';
const SCHEMA = 12;
const REPLAY_MAX = 200; // deliberately small: overrunning it must be reachable
// Copied from crates/recalld/src/service.rs::SPLIT_EVENT_CAP. Past it a split
// stops publishing one `segment` event per moved row and says `resync: true`
// instead; a mock that never did that is how a client ends up ignoring the flag.
export const SPLIT_EVENT_CAP = 100;

export function defaultMockSocket() {
  if (process.env.NX_RECALL_MOCK_SOCK) return process.env.NX_RECALL_MOCK_SOCK;
  const run = process.env.XDG_RUNTIME_DIR || `/run/user/${process.getuid?.() ?? 1000}`;
  return path.join(run, 'nx-recall-mock.sock');
}

// ---------------------------------------------------------------------------
// canned world
// ---------------------------------------------------------------------------

const SESSIONS = [
  { id: 1, world: 'Ghost Club', started: '2026-08-29T20:12:00Z' },
  { id: 2, world: 'The Great Pug', started: '2026-08-30T21:40:00Z' },
  { id: 3, world: 'Murder 4', started: '2026-08-31T18:05:00Z' },
];

// `kind` is schema v4: "mic" is a source row like any other and is governed by
// its own switch rather than by the allowlist, which is why the GUI renders it
// somewhere else entirely.
const SOURCES = [
  { match_key: 'VRChat.exe', kind: 'app', binary: 'wine64-preloader', display: 'VRChat', allowed: true, first_seen: '2026-07-02T18:22:00Z', last_seen: '2026-08-31T18:05:00Z', streams: 1 },
  { match_key: 'Discord', kind: 'app', binary: 'Discord', display: 'Discord', allowed: true, first_seen: '2026-07-02T19:01:00Z', last_seen: '2026-08-31T17:44:00Z', streams: 1 },
  { match_key: 'firefox', kind: 'app', binary: 'firefox', display: 'Firefox', allowed: false, first_seen: '2026-07-03T09:14:00Z', last_seen: '2026-08-31T12:30:00Z', streams: 0 },
  { match_key: 'spotify', kind: 'app', binary: 'spotify', display: 'Spotify', allowed: false, first_seen: '2026-07-05T22:10:00Z', last_seen: '2026-08-30T23:58:00Z', streams: 0 },
  { match_key: 'mpv', kind: 'app', binary: 'mpv', display: 'mpv', allowed: false, first_seen: '2026-08-14T20:44:00Z', last_seen: '2026-08-14T22:02:00Z', streams: 0 },
  { match_key: 'mic', kind: 'mic', binary: 'mic', display: 'Microphone', allowed: false, first_seen: '2026-07-02T18:22:00Z', last_seen: '2026-08-31T18:05:00Z', streams: 0 },
  // 0.10.0: the room microphone. A source row like the headset's, governed by
  // its own switch, and off — a second physical mic is nobody's default.
  { match_key: 'room', kind: 'room', binary: 'room', display: 'Room microphone', allowed: false, first_seen: '2026-08-14T20:44:00Z', last_seen: '2026-08-31T18:05:00Z', streams: 0 },
];

/// The capture devices `devices.list` offers (0.10.0). Three, because the
/// interesting case needs all three: the headset that is already the system
/// default and must NOT be picked, a desk mic that should be, and a webcam
/// whose microphone is the thing people pick by accident.
const DEVICES = [
  { node_name: 'alsa_input.usb-Index_HMD-00.mono-fallback', description: 'Index HMD Microphone', is_default: true },
  { node_name: 'alsa_input.usb-Blue_Yeti-00.analog-stereo', description: 'Yeti Stereo Microphone', is_default: false },
  { node_name: 'alsa_input.usb-046d_HD_Pro_Webcam_C920-02.analog-stereo', description: 'HD Pro Webcam C920 Analog Stereo', is_default: false },
];

/// The Discord bridge's accounts (0.9.0's ground truth). Three, one of them
/// already linked — the state the card actually has to render is "some are
/// linked and some are not", not either extreme.
const DISCORD_USERS = [
  { user_id: '184920348293', name: 'kira', speaker: 3, speaker_name: 'Kira', via: 'truth', linked_ms: Date.parse('2026-08-30T21:52:00Z'), first_seen_ms: Date.parse('2026-08-29T20:14:00Z'), last_seen_ms: Date.parse('2026-08-31T18:41:00Z'), agreement: 0.96, segments: 148 },
  { user_id: '229481003922', name: 'mara.exe', speaker: null, speaker_name: null, via: null, linked_ms: null, first_seen_ms: Date.parse('2026-08-29T20:19:00Z'), last_seen_ms: Date.parse('2026-08-31T18:38:00Z'), agreement: null, segments: 92 },
  { user_id: '773310028471', name: 'toaster', speaker: null, speaker_name: null, via: null, linked_ms: null, first_seen_ms: Date.parse('2026-08-30T21:44:00Z'), last_seen_ms: Date.parse('2026-08-30T23:10:00Z'), agreement: null, segments: 17 },
];

/// The pinned "You" speaker. It exists in the mock's voicebank from the start
/// (a previous session's microphone minted it) so the transcript's distinct
/// treatment is reachable without waiting for a live enrolment.
const YOU_SPEAKER = 8;

/// The header every exported file carries, and the permission slip for
/// overwriting one (0.10.0). Verbatim from `crate::export::MARKER`.
const EXPORT_MARKER = '<!-- nx-recall export -->';

// What the user says into their own microphone. Lines rather than tones: the
// point of the mic feed is that these rows render differently, and a reviewer
// looking at a screenshot has to be able to tell which ones they are.
const MY_LINES = [
  'hold on, I am going to move to the other side of the bar',
  'yeah I can hear you fine now',
  'I think the portal in the stairwell only opens at night',
  'give me a second, my headset is doing the thing again',
  'that is the world I was talking about earlier',
];

/**
 * The sentence SIGUSR2's partial turn arrives one piece at a time (0.11.0).
 *
 * Long enough that three provisional readings are visibly different from each
 * other and from the final, which is the only way the "updated in place" and
 * "replaced without a jump" claims can be tested rather than asserted.
 */
const PARTIAL_LINE = 'the door behind the bar goes back into the same instance if you take it twice';

// name: null means "not named yet" — the onboarding case (DESIGN §5).
// `languages` is schema v5: which languages this voice actually speaks, so a
// wrong-language transcript can be corrected rather than merely noticed. `null`
// is "any", the default, and is what almost every voice starts as.
//
// `colour`/`icon` are the per-person highlight: a palette TOKEN (never a hex —
// see gui/src/renderer/lib/palette.js) and a short emoji, both null for a
// voice nobody has decorated. Two of them start highlighted on purpose. A
// fixture where every row is null would let a view that never reads the fields
// pass the e2e — the highlight has to be on screen on FIRST paint, before
// anything has called `speakers.set`, or "does the list render one?" and "does
// setting one work?" are the same untested question.
const SPEAKERS = [
  { id: 1, name: 'Kira', auto: 'Speaker_03', languages: ['de'], colour: 'violet', icon: '\u{1F319}', first_seen: '2026-07-02T18:24:00Z' },
  { id: 2, name: null, auto: 'Speaker_07', colour: null, icon: null, first_seen: '2026-07-02T18:31:00Z' },
  { id: 3, name: null, auto: 'Speaker_12', colour: null, icon: null, first_seen: '2026-07-11T21:02:00Z' },
  { id: 4, name: 'Ash', auto: 'Speaker_18', colour: 'teal', icon: '\u{2728}', first_seen: '2026-07-19T19:47:00Z' },
  { id: 5, name: null, auto: 'Speaker_31', colour: null, icon: null, first_seen: '2026-08-14T20:50:00Z' },
  { id: 6, name: null, auto: 'Speaker_44', colour: null, icon: null, first_seen: '2026-08-29T20:19:00Z' },
  // Unnamed, like any other voice the daemon minted — the difference is where
  // its label comes from, not whether the user has typed one.
  { id: 8, name: null, auto: 'You', colour: null, icon: null, first_seen: '2026-08-29T20:12:00Z' },
  // A one-off: one grunt, a second of speech, no name. The kind of row the
  // mint bar now prevents and `speakers.prune` sweeps up when it slipped
  // through anyway (0.6.1).
  { id: 9, name: null, auto: 'Speaker_52', colour: null, icon: null, first_seen: '2026-08-31T18:07:00Z' },
  // The 0.6.4 bug, sitting in the fixture where it can be pressed: a voice with
  // NO segments at all. Its words were deleted at some point; the voiceprint
  // stayed and goes on matching. Delete-by-segment matched nothing here, so the
  // ⋯ menu's Delete was a silent no-op for ever — which is why the list has to
  // carry one of these and the e2e has to delete it through the real UI.
  { id: 10, name: null, auto: 'Speaker_58', colour: null, icon: null, first_seen: '2026-08-30T21:41:00Z' },
];

/// The commitment state machine. Nothing but a human click moves a row off
/// `candidate`, at either end of the socket.
const COMMITMENT_STATES = ['candidate', 'confirmed', 'done', 'dismissed'];

/// What `[graph].llm_threads` may be set to — `crate::control::GRAPH_THREADS`,
/// as `[min, max]`. Kept here rather than in the view so the mock refuses the
/// same values the daemon refuses.
const GRAPH_THREADS = [1, 32];

/// How long a highlight icon may be, and how "long" is counted.
///
/// Grapheme CLUSTERS, mirroring the daemon's own rule, because every other
/// unit of length lies about emoji: 👩‍🚀 is one thing a person picked, one
/// cluster, two "characters" by `Intl` if you split it wrong, seven UTF-16
/// units, and eleven bytes. Two clusters is enough for a pair like 🌙✨ and
/// short enough that an icon can never crowd a name out of a row.
const MAX_ICON_GRAPHEMES = 2;

const ICON_SEGMENTER = new Intl.Segmenter('en', { granularity: 'grapheme' });
function graphemes(s) {
  return [...ICON_SEGMENTER.segment(s)].length;
}

/// Whitespace and C0/C1 control characters. Deliberately narrower than
/// `\p{C}`: U+200D ZERO WIDTH JOINER is `Cf` and every joined emoji needs it.
const WS_OR_CONTROL = /[\s\p{Cc}]/u;

/// What counts as a one-off voice, matching the daemon's own bar.
const PRUNE_MAX_SEGMENTS = 1;
const PRUNE_MAX_SPEECH_MS = 3000;

// One voice whose audio has aged out of retention while its text stayed. The
// GUI has to say so in place ("no audio kept for this voice") rather than
// offering a play button that does nothing — so the mock always has a voice
// that answers `gone`, and the e2e always exercises that branch.
const NO_AUDIO_SPEAKER = 5;

// --- generated audio --------------------------------------------------------
// Real bytes, no fixtures: a short 16 kHz mono sine sweep per speaker, so the
// GUI decodes and plays an actual WAV and two voices audibly differ. The pitch
// is derived from the speaker id, which makes "did I press the right row?"
// answerable by ear during a manual look.

const TONE_MS = 1200;
const TONE_RATE = 16000;
const tones = new Map();

function riffWav(pcm, rate) {
  const head = Buffer.alloc(44);
  head.write('RIFF', 0);
  head.writeUInt32LE(36 + pcm.length, 4);
  head.write('WAVE', 8);
  head.write('fmt ', 12);
  head.writeUInt32LE(16, 16); // fmt chunk size
  head.writeUInt16LE(1, 20); // PCM
  head.writeUInt16LE(1, 22); // mono
  head.writeUInt32LE(rate, 24);
  head.writeUInt32LE(rate * 2, 28); // byte rate
  head.writeUInt16LE(2, 32); // block align
  head.writeUInt16LE(16, 34); // bits
  head.write('data', 36);
  head.writeUInt32LE(pcm.length, 40);
  return Buffer.concat([head, pcm]);
}

function toneFor(speakerId) {
  const key = speakerId ?? 0;
  const cached = tones.get(key);
  if (cached) return cached;
  const n = Math.round((TONE_RATE * TONE_MS) / 1000);
  const from = 150 + ((Math.abs(Number(key)) * 53) % 320); // 150..470 Hz
  const to = from * 1.5;
  const pcm = Buffer.alloc(n * 2);
  const fade = TONE_RATE * 0.02;
  let phase = 0;
  for (let i = 0; i < n; i += 1) {
    phase += (2 * Math.PI * (from + ((to - from) * i) / n)) / TONE_RATE;
    // Fade the ends, or every clip starts and stops with a click.
    const env = Math.min(1, i / fade, (n - i) / fade);
    pcm.writeInt16LE(Math.round(Math.sin(phase) * 0.28 * env * 32767), i * 2);
  }
  const wav = riffWav(pcm, TONE_RATE);
  tones.set(key, wav);
  return wav;
}

const CANNED_LINES = [
  [1, 'wait, which portal was it — the one behind the bar or the one in the stairwell?', 0.02, 0.71],
  [2, 'the stairwell one, but it only opens after the lights go down', 0.03, 0.66],
  [3, 'I got dropped into the wrong instance again, give me a second', 0.01, 0.62],
  [1, 'no rush, we are still waiting on two people', 0.04, 0.74],
  [null, 'that world with the rain, I forget what it was called', 0.42, 0.31],
  [4, 'you mean Ghost Club? the rain is only in the late map', 0.02, 0.69],
  [2, 'I built a smaller version of that for the meetup thing', 0.05, 0.58],
  [3, 'send me the link, I want to look at how you did the shaders', 0.02, 0.64],
  [null, 'both of them talking at once here', 0.61, 0.24],
  // Nobody was talking over anybody — the voice simply matched nothing in the
  // voicebank. It is a different failure from an overlap refusal and the GUI
  // has to say so, so the mock always produces one of each.
  [null, 'someone in the corner I do not recognise at all', 0.03, null],
  [5, 'my mic keeps cutting out, is it better now?', 0.08, 0.55],
  [1, 'much better. you were clipping badly before', 0.03, 0.72],
  [6, 'has anyone actually finished that map or are we all pretending', 0.02, 0.51],
  [4, 'I finished it once, on a Tuesday, and never again', 0.01, 0.7],
  [2, 'we should do the photo thing before everyone logs off', 0.03, 0.6],
  [3, 'give me five minutes, I need to fix my avatar first', 0.02, 0.63],
];

/**
 * The `translation` sibling track, on two of the canned lines (0.8.3).
 *
 * A separate track is adding `translation: {lang, text, via}` to segments whose
 * turn was in a language the user does not read. It is purely additive — every
 * other row carries no such field and every client that does not know about it
 * ignores it (PROTOCOL "Versioning rules") — so the mock's job here is small
 * and specific: make sure at least one row on the LIVE feed has one, because a
 * rendering nobody ever produces is a rendering nobody ever checks.
 *
 * Keyed by index into CANNED_LINES rather than woven into it, for the same
 * reason the pathological rows are: those tuples are positional, several
 * fixtures elsewhere in this file count on their contents, and a fifth element
 * on two of sixteen would be a trap for the next person to read them.
 *
 * Both are speaker 1, who is declared `de` — which is the case the field
 * exists for. `lang` is the language of the TEXT here, not of the turn.
 */
/**
 * Every language the daemon offers as a target or as one you read (0.10.2).
 *
 * The real one is `lang::OFFERED` and this is a copy of it, which is the usual
 * mock bargain: the list is on the wire precisely so a client does not have to
 * carry one, and the mock has to put something there for the client to read.
 */
const LANGUAGES = [
  { code: 'en', name: 'English' },
  { code: 'de', name: 'German' },
  { code: 'fr', name: 'French' },
  { code: 'es', name: 'Spanish' },
  { code: 'it', name: 'Italian' },
  { code: 'pt', name: 'Portuguese' },
  { code: 'nl', name: 'Dutch' },
  { code: 'pl', name: 'Polish' },
  { code: 'ru', name: 'Russian' },
  { code: 'uk', name: 'Ukrainian' },
  { code: 'ja', name: 'Japanese' },
  { code: 'zh', name: 'Chinese' },
  { code: 'ko', name: 'Korean' },
  { code: 'tr', name: 'Turkish' },
  { code: 'sv', name: 'Swedish' },
  { code: 'da', name: 'Danish' },
  { code: 'no', name: 'Norwegian' },
  { code: 'fi', name: 'Finnish' },
  { code: 'cs', name: 'Czech' },
];

const TRANSLATED = new Map([
  [0, { lang: 'en', text: 'wait — which portal was it, the one behind the bar or the one in the stairwell?', via: 'nllb-200' }],
  [3, { lang: 'en', text: 'no rush, we are still waiting on two people', via: 'nllb-200' }],
]);

/// Which conversation a canned row belongs to (schema v6). Blocks of five, so
/// the history really does contain several threads with different people in
/// them — the transcript's separators and the person page's "people they talk
/// with" both need more than one to say anything.
const THREAD_BLOCK = 5;
const threadFor = (i) => 500 + Math.floor(i / THREAD_BLOCK);

// ---------------------------------------------------------------------------
// worlds (0.10.0)
// ---------------------------------------------------------------------------
//
// Two, because one proves nothing: a person page's "Where you meet" is a LIST
// and a world facet has to be able to exclude something. Threads alternate
// between them so both have people in them and neither has all of them.
const WORLDS = [
  { world_id: 'wrld_4432ea9b-729c-46e3-8eaf-846aa0a37fdd', name: 'The Great Pug' },
  { world_id: 'wrld_9c1f0a2b-11d4-4f7a-9c33-0b7e5a6d8e10', name: 'Ghost Club' },
];
/// Which world a conversation happened in. Deterministic and deliberately not
/// random: an e2e step that clicks "The Great Pug" must find the same rows on
/// every run.
const worldOfThread = (thread) =>
  thread == null ? null : WORLDS[thread % WORLDS.length].world_id;

// ---------------------------------------------------------------------------
// the back catalogue (0.7.4)
// ---------------------------------------------------------------------------
//
// Two dozen canned rows was enough while the transcript was a 600-row window
// and nothing could ask for anything older. Infinite scrollback can, and a
// fixture smaller than one page cannot exercise a single seam — so the mock now
// carries fourteen earlier evenings behind the canned ones.
//
// Everything about it is deliberately BEHIND the canned rows and beneath their
// ids: the newest 600 (what the client opens on) is still filler-then-canned
// with every canned fixture present and in the same relative order, the person
// page's `recent_threads` still surfaces the canned threads because their ids
// are higher, and no e2e step that counts or clicks a canned row moves.
const FILLER_DAYS = 14;
const FILLER_PER_DAY = 105; // 14 × 105 = 1470 rows, ~3.7 pages of scrollback
const FILLER_SPEAKERS = [1, 2, 3, 4, 6]; // never 5, 8, 9 or 10 — all four are fixtures
const FILLER_LINES = [
  'I keep meaning to redo the lighting in that room',
  'did anyone else get dropped when the instance filled up',
  'it runs fine until about twelve people and then it does not',
  'that is the third time tonight',
  'I have a build of it somewhere, I will dig it out',
  'no, the other one, the small one with the stairs',
  'we should write some of this down at some point',
  'my headset battery is about to go, hold on',
  'that sounds like a driver thing more than a game thing',
  'honestly it looked better before you fixed it',
  'give me a minute, I am reading the changelog',
  'okay that is genuinely clever',
  'I never understood why they did it that way',
  'someone said they were going to look at it and then nobody did',
  'it is late, I should probably log off soon',
];

/// The single oldest row in the archive, and the only place this phrase
/// appears. It is what the e2e searches for to prove a jump to the far end of
/// the history renders AND SURVIVES the live feed — audit finding #12, which
/// discarded exactly this row at exactly the moment it was merged in.
const OLDEST_LINE = 'the obsidian lighthouse world, before they took it down';

/// 0.11.0 — which capture source each filler voice is heard through.
///
/// Pinned per VOICE rather than rotated per row, because that is the fact the
/// source history exists to show and the rotation hid it: some people you only
/// ever meet on Discord, some only in VRChat, and some in both. Voices 3 and 6
/// are Discord-only; voice 1 is the cross-source one (every seventh turn on
/// Discord, the rest in VRChat); the rest are VRChat-only. The user's own voice
/// arrives on `mic` from the canned block, so all three kinds of chip render.
const SPEAKER_SOURCE = { 1: 'both', 3: 'Discord', 6: 'Discord' };

function fillerSource(speaker, i) {
  const rule = SPEAKER_SOURCE[speaker];
  if (rule === 'Discord') return 'Discord';
  if (rule === 'both') return i % 7 === 3 ? 'Discord' : 'VRChat.exe';
  return 'VRChat.exe';
}

function buildFiller(base) {
  const out = [];
  for (let d = 0; d < FILLER_DAYS; d += 1) {
    const evening = base - (FILLER_DAYS - d) * 86_400_000;
    for (let r = 0; r < FILLER_PER_DAY; r += 1) {
      const i = d * FILLER_PER_DAY + r;
      const t = evening + r * 40_000;
      // Three of the five voices per conversation, rotating, rather than all
      // five in every one. Otherwise every thread separator in fourteen
      // evenings names the same people in the same order, which tells a
      // reviewer looking at a screenshot nothing at all — and gives the person
      // page's "people they talk with" no edges worth weighing.
      const block = Math.floor(i / THREAD_BLOCK);
      const cast = [block, block + 1, block + 3].map((n) => FILLER_SPEAKERS[n % FILLER_SPEAKERS.length]);
      const speaker = cast[i % cast.length];
      out.push({
        // 10000..11469. Clear of the canned block (1000..1104) by a wide
        // margin, and deliberately ABOVE it rather than below: ids in this
        // fixture are handed out by insertion, not by time, and the one thing
        // that must never collide is the id — an overlapping id made
        // `segments.audio` hand back the wrong row's tone and nothing else
        // noticed for the length of a test run.
        id: 10_000 + i,
        session: SESSIONS[i % 2].id,
        source: fillerSource(speaker, i),
        speaker,
        text: i === 0 ? OLDEST_LINE : FILLER_LINES[i % FILLER_LINES.length],
        t_ms: t,
        t_ns: String(t) + '000000',
        dur_ms: 1600 + ((i * 617) % 3800),
        overlap_frac: 0.02,
        match_score: 0.6 + ((i % 30) / 100),
        label_via: 'match',
        lang_via: 'classified',
        lang: speaker === 1 ? 'de' : 'en',
        // 0.8.0: what the SECOND decoder made of the same audio. "solid" is
        // agreement, "shaky" is disagreement, and the text on screen is the
        // first decoder's either way — the cross-check is a flag, never a
        // replacement. Every eleventh row disagrees, which is roughly the rate
        // the real bake-off measured and enough that any page has a few.
        asr_confidence: i % 11 === 5 ? 'shaky' : 'solid',
        text_via: 'live',
        // 0.9.0: the English turns come with a German reading under them, and
        // the German ones do not — which is the rule the daemon follows (a
        // turn already in your language is not a turn to translate). An object
        // and not a string: a client showing a translation has to be able to
        // say which language it is in and which model wrote it.
        translation:
          speaker === 1
            ? null
            : {
                lang: 'de',
                text: TRANSLATIONS[i % TRANSLATIONS.length],
                via: 'qwen2.5-3b-instruct-q4_k_m@1',
              },
        // Blocks of five, as above, but numbered BELOW the canned threads so
        // "recent conversations" still means the canned ones.
        thread: 100 + block,
      });
    }
  }
  return out;
}

// A fixed history so every run of the GUI and every screenshot looks the same.
function buildHistory() {
  const base = Date.parse('2026-08-31T18:05:00Z');
  const out = buildFiller(base);
  for (let i = 0; i < 26; i++) {
    const t = base + i * 47_000;
    // Every seventh row is the user, from a session where the microphone was
    // on. It comes from `source: "mic"`, carries the pinned speaker, and has NO
    // match_score — the label is provenance, not a comparison — so the
    // transcript's distinct treatment is visible on first paint rather than
    // only after a live enrolment.
    // The stride is chosen so it never displaces the canned lines the other
    // tests depend on — the aged-out voice, and one of each kind of nameless.
    const mine = i % 9 === 4;
    const [sp, text, overlap, score] = mine
      ? [YOU_SPEAKER, MY_LINES[Math.floor(i / 9) % MY_LINES.length], 0.02, null]
      : CANNED_LINES[i % CANNED_LINES.length];
    const speaker = overlap > 0.1 ? null : sp;
    out.push({
      id: 1000 + i,
      session: SESSIONS[i % 3 === 2 ? 2 : i % 2].id,
      source: mine ? 'mic' : fillerSource(speaker, i),
      speaker,
      text,
      t_ms: t,
      t_ns: String(t) + '000000',
      dur_ms: 1800 + ((i * 733) % 4200),
      overlap_frac: overlap,
      match_score: overlap > 0.1 ? null : score,
      // schema v5. `label_via` is how the speaker got here; the value that
      // changes what a client renders is "proximity" (below).
      label_via: speaker == null ? null : mine ? 'mic' : 'match',
      lang: speaker === 1 ? 'de' : 'en',
      // 0.7.7: how the LANGUAGE got there. "classified" is the ordinary
      // answer; the two below are the ones the transcript says something about.
      lang_via: 'classified',
      // 0.8.0, the cross-check and the words' own provenance.
      asr_confidence: mine ? 'solid' : i % 4 === 1 ? 'shaky' : 'solid',
      text_via: 'live',
      // schema v6: which conversation this turn is part of.
      thread: threadFor(i),
    });
  }

  // Two rows that only exist since 0.6.1, both of which the GUI has to render
  // differently from everything above.
  const base2 = base + 26 * 47_000;
  out.push({
    // Inherited from the confident turns around it: a name with no score
    // behind it. It reads as uncertain, and the "?" says why.
    id: 1100,
    session: SESSIONS[2].id,
    source: 'VRChat.exe',
    speaker: 1,
    text: 'mm',
    t_ms: base2,
    t_ns: String(base2) + '000000',
    dur_ms: 600,
    overlap_frac: 0.02,
    match_score: null,
    label_via: 'proximity',
    lang: null,
    lang_via: null,
    // Six hundred milliseconds of "mm": too short for a second decoder to have
    // an opinion about, which is exactly what a null cross-check means.
    asr_confidence: null,
    text_via: 'live',
    thread: threadFor(26),
  });
  out.push({
    // The one-off voice: one grunt, and the whole reason a sweep exists.
    id: 1101,
    session: SESSIONS[2].id,
    source: 'VRChat.exe',
    speaker: 9,
    text: 'huh',
    t_ms: base2 + 47_000,
    t_ns: String(base2 + 47_000) + '000000',
    dur_ms: 900,
    overlap_frac: 0.03,
    match_score: 0.38,
    label_via: 'match',
    lang: null,
    lang_via: null,
    asr_confidence: null,
    text_via: 'live',
    thread: threadFor(27),
  });


  // schema v7, the memory graph's Tiers 2 and 3: three turns that are actually
  // promises, so the Memory view has something real to point at. Appended
  // rather than woven into CANNED_LINES, because the indices of those rows are
  // load-bearing for half the e2e suite.
  PROMISES.forEach((p, i) => {
    const t = base2 + (2 + i) * 47_000;
    out.push({
      id: p.segment,
      session: SESSIONS[2].id,
      source: 'VRChat.exe',
      speaker: p.who,
      text: p.said,
      t_ms: t,
      t_ns: String(t) + '000000',
      dur_ms: 2600 + i * 300,
      overlap_frac: 0.02,
      match_score: 0.66,
      label_via: 'match',
      lang: p.lang,
      lang_via: 'classified',
      asr_confidence: 'solid',
      text_via: 'live',
      thread: threadFor(28 + i),
    });
  });
  // 0.7.7, the conversational language prior. Two rows, because they are the
  // two states a person can see and they read differently:
  out.push({
    // A German turn the multilingual model decoded as English, caught by the
    // conversation around it and re-read by the German arbiter. The words on
    // screen came from a different model than every other row here, and the
    // "?" says so.
    id: 1110,
    session: SESSIONS[2].id,
    source: 'VRChat.exe',
    speaker: 1,
    text: 'ich glaube das ist der einzige Weg',
    t_ms: base2 + 5 * 47_000,
    t_ns: String(base2 + 5 * 47_000) + '000000',
    dur_ms: 2400,
    overlap_frac: 0.02,
    match_score: 0.31,
    label_via: 'match',
    lang: 'de',
    lang_via: 're-decode',
    // The arbiter wrote these words, and the second decoder agrees with them.
    // Two different facts about the same row, which is why they are two fields.
    asr_confidence: 'solid',
    text_via: 'arbiter',
    thread: threadFor(31),
  });
  out.push({
    // A suspected flip nothing could settle: too short to re-read. The words
    // stand and the language is in doubt, which is what `lang: null` with a
    // "mismatch" provenance means.
    id: 1111,
    session: SESSIONS[2].id,
    source: 'VRChat.exe',
    speaker: 2,
    text: 'and then it just works',
    t_ms: base2 + 6 * 47_000,
    t_ns: String(base2 + 6 * 47_000) + '000000',
    dur_ms: 1100,
    overlap_frac: 0.02,
    match_score: 0.72,
    label_via: 'match',
    lang: null,
    lang_via: 'mismatch',
    asr_confidence: 'solid',
    text_via: 'live',
    thread: threadFor(31),
  });

  // 0.8.0, the row the whole confidence feature is for and the only place this
  // wording appears: the second decoder read it differently, so the words are
  // flagged, and the pipeline went back and re-read it with the session audio
  // either side. Named rather than generated because the e2e searches for it —
  // a shaky mark has to be provable in the SEARCH results too, and a fixture
  // that only exists somewhere in a rotation cannot be searched for.
  out.push({
    id: 1112,
    session: SESSIONS[2].id,
    source: 'VRChat.exe',
    speaker: 2,
    text: 'the shader thing on the second floor was flickering again',
    t_ms: base2 + 7 * 47_000,
    t_ns: String(base2 + 7 * 47_000) + '000000',
    dur_ms: 2900,
    overlap_frac: 0.03,
    match_score: 0.68,
    label_via: 'match',
    lang: 'en',
    lang_via: 'classified',
    asr_confidence: 'shaky',
    text_via: 'context',
    thread: threadFor(31),
  });

  // The two notes-to-self, as what they actually are: ordinary MIC turns that
  // happen to begin with a wake phrase. The segment STAYS in the transcript
  // (PROTOCOL) — a note is a second reading of a turn, not a turn that was
  // moved somewhere else.
  NOTES.forEach((n, i) => {
    const t = base2 + (8 + i) * 47_000;
    n.t_ms = t;
    n.t_ns = String(t) + '000000';
    out.push({
      id: n.segment_id,
      session: SESSIONS[2].id,
      source: 'mic',
      speaker: YOU_SPEAKER,
      text: n.said,
      t_ms: t,
      t_ns: n.t_ns,
      dur_ms: 3100 + i * 400,
      overlap_frac: 0.02,
      match_score: null,
      label_via: 'mic',
      lang: n.lang,
      lang_via: 'classified',
      asr_confidence: 'solid',
      text_via: 'live',
      thread: threadFor(32),
    });
  });
  return out;
}

/// Notes to self (0.8.0). A MIC turn whose text starts with a wake phrase
/// becomes one; `text` is what is left after the phrase, because "recall, merk
/// dir" is the addressing and not the note.
///
/// Two of them, in the two languages the daemon classifies, and one already
/// `done` — a list where every row is in the same state cannot show that the
/// state chips mean anything.
/// 0.9.0: German lines to hang under the English filler, so a transcript page
/// shows the second row under the first and the driver can check that the
/// original survives above it.
///
/// They are stand-ins, not translations of the filler: the index wraps
/// independently of FILLER_LINES, so a given pair does not correspond. What is
/// being demonstrated is the SHAPE — two lines, the original on top, the
/// reading quiet and marked — and a mock that had to keep two lists in step
/// would break every time either grew.
const TRANSLATIONS = [
  'warte, welches Portal war das — das hinter der Bar oder das im Treppenhaus?',
  'das im Treppenhaus, aber es geht erst auf, wenn das Licht ausgeht',
  'ich bin schon wieder in der falschen Instanz gelandet, einen Moment',
  'hast du das Video von dem Bar-World-Abend noch?',
  'ja klar, ich schick dir morgen den Link',
  'perfekt, danke dir',
  'der Shader frisst allerdings Performance',
  'ich baue sowieso nur für den PC',
];

const NOTES = [
  {
    id: 700,
    segment_id: 1120,
    lang: 'de',
    said: 'recall, merk dir dass der Link zu der Map noch fehlt',
    text: 'dass der Link zu der Map noch fehlt',
    state: 'open',
    t_ms: 0,
    t_ns: '0',
  },
  {
    id: 701,
    segment_id: 1121,
    lang: 'en',
    said: 'recall, remember to export the shader graph before Friday',
    text: 'to export the shader graph before Friday',
    state: 'done',
    t_ms: 0,
    t_ns: '0',
  },
  // 0.9.0: two notes with a date in them, which is what makes a note a
  // reminder. `due_in` is relative to boot so the card looks the same on every
  // run — one still ahead (the chip is the accent, and the snooze buttons are
  // live), one already fired (the chip is quiet).
  {
    id: 704,
    segment_id: 1124,
    lang: 'de',
    said: 'recall, erinner mich morgen um zehn an den Link',
    text: 'morgen um zehn an den Link',
    state: 'open',
    t_ms: 0,
    t_ns: '0',
    due_in: 40 * 60 * 1000,
    fired: false,
  },
  {
    id: 705,
    segment_id: 1125,
    lang: 'en',
    said: 'recall, remind me at 8 pm to send the recording',
    text: 'at 8 pm to send the recording',
    state: 'open',
    t_ms: 0,
    t_ns: '0',
    due_in: -20 * 60 * 1000,
    fired: true,
  },
];

/// 0.9.0: one paragraph per conversation, as the local model writes them.
/// Two, on two different days, so the card has both of its groups.
/// 0.11.6: a digest is prose about people, so the canned ones say people's
/// names. Both shapes the daemon can serve are here, because a client has to
/// draw them the same way and the only honest way to know that is to have one
/// of each in the fixture:
///
/// * 501 is a digest written since 0.11.6 (`rendered: "names"`) — the model
///   was handed the labels and wrote them, so `summary_raw` already reads like
///   the summary.
/// * 503 was written before it (`rendered: "legacy"`) — the model wrote
///   letters and the daemon substituted names on the way out, so the raw still
///   says "A" and "B". Nothing was re-generated to get there and no model was
///   asked anything.
const DIGESTS = [
  {
    thread_id: 501,
    lang: 'de',
    day_offset: 1,
    rendered: 'names',
    summary:
      'Kira hat nach dem Shader von dem Avatar gefragt, den Speaker 07 gestern gezeigt hat. Speaker 07 hat die Datei noch und will den Link morgen schicken; heute kommt er nicht mehr dazu.',
    open: ['Speaker 07 schickt Kira morgen den Link'],
    people: [1, 2],
  },
  {
    thread_id: 503,
    lang: 'en',
    day_offset: 0,
    rendered: 'legacy',
    summary:
      'Speaker 07 asked whether anybody recorded the meetup. Speaker 12 had OBS running for about two hours and offered to cut it down to the world tour section before sending it over.',
    summary_raw:
      'A asked whether anybody recorded the meetup. B had OBS running for about two hours and offered to cut it down to the world tour section before sending it over.',
    open: ['Speaker 12 cuts the recording and sends it to Speaker 07'],
    open_raw: ['B cuts the recording and sends it to A'],
    people: [2, 3],
  },
];

/// The states a note can be in. Same shape of rule as the commitments: only a
/// person moves one, at either end of the socket.
const NOTE_STATES = ['open', 'done', 'dismissed'];

/// The third note, delivered live on the first SIGUSR2 so a client's "a note
/// arrived" path is drivable rather than only reachable by talking.
const LIVE_NOTE = {
  id: 702,
  segment_id: 1122,
  lang: 'en',
  said: 'recall, note that the portal in the stairwell only opens at night',
  text: 'that the portal in the stairwell only opens at night',
  state: 'open',
};

/// The user glossary, and the three auto groups the daemon derives (PROTOCOL
/// "Vocabulary"). The user half is editable; the auto halves are facts about
/// what has been heard and are read-only wherever they are shown.
const VOCAB_USER = ['PhysBones', 'Ghost Club', 'Rowan'];
const VOCAB_AUTO = {
  roster: ['Kira', 'Ash', 'Rowan', 'nyxx__', 'orbital_moth'],
  worlds: ['Ghost Club', 'The Great Pug', 'Murder 4', 'Obsidian Lighthouse'],
  corrections: ['stairwell', 'shader', 'instance'],
};

/// The commitments the graph found, and the turns they were found in.
///
/// Two sources on purpose, because the GUI has to render them differently:
/// `rules` is a pattern match and a guess, `llm` is the local model under a
/// verdict-first grammar. One of each has a due date and one has none, because
/// "sure, I'll send it over" is a promise with no deadline and the list has to
/// sort that honestly (undated last, never first).
const DAY = 86_400_000;
const PROMISES = [
  {
    segment: 1102,
    who: 2,
    lang: 'de',
    said: 'ja klar, ich schick dir morgen den Link zu der Map',
    what: 'den Link zu der Map schicken',
    to: 3,
    due_raw: 'morgen',
    due_in: DAY,
    due_kind: 'day',
    source: 'llm',
    confidence: 0.75,
  },
  {
    segment: 1103,
    who: 4,
    lang: 'en',
    said: "I'll cut the recording and send it over on Friday",
    what: 'cut the recording and send it over',
    to: 1,
    due_raw: 'on Friday',
    due_in: 3 * DAY,
    due_kind: 'weekday',
    source: 'llm',
    confidence: 0.75,
  },
  {
    segment: 1104,
    who: 1,
    lang: 'de',
    said: 'mach ich, versprochen',
    what: 'mach ich, versprochen',
    to: 2,
    due_raw: null,
    due_in: null,
    due_kind: null,
    source: 'rules',
    confidence: 0.25,
  },
  // 0.8.0: two promises with the USER at one end of them. Everything above is
  // between other people, which was fine while the Memory list was the only
  // surface — a brief is the first thing that asks "what is open between me and
  // the person who just walked in", and that question needs both directions to
  // have an answer.
  {
    segment: 1105,
    who: 1,
    lang: 'de',
    said: 'ich schick dir morgen den Link zu der Map',
    what: 'den Link zu der Map schicken',
    to: YOU_SPEAKER,
    due_raw: 'morgen',
    due_in: DAY,
    due_kind: 'day',
    source: 'llm',
    confidence: 0.78,
  },
  {
    segment: 1106,
    who: YOU_SPEAKER,
    lang: 'en',
    said: "I'll show you the world tour thing I built at the weekend",
    what: 'show the world tour',
    to: 1,
    due_raw: 'at the weekend',
    due_in: 2 * DAY,
    due_kind: 'day',
    source: 'llm',
    confidence: 0.7,
  },
];

/// Topic labels, as the Tier 3 pass would have written them: one string per
/// conversation, on the thread itself.
const THREAD_TOPICS = {
  500: 'world portals',
  501: 'shader work',
  502: 'avatar troubles',
  503: 'the meetup photo',
  [500 + Math.floor(28 / THREAD_BLOCK)]: 'shader work',
};

// --- a stand-in for meaning -------------------------------------------------
// The real leg is 118 MB of ONNX. This is a lookup table with the same SHAPE:
// a query expands to a handful of related surface forms, in both languages, so
// the view's cross-language case is demonstrable without weights in the repo.

const GLOSS = [
  ['whale', 'wal', 'cetacea'],
  ['world', 'welt', 'instance', 'instanz'],
  ['portal', 'portal'],
  ['fountain', 'brunnen'],
  ['cat', 'katze'],
  ['coffee', 'kaffee'],
  ['train', 'zug'],
  ['late', 'verspätung', 'delayed'],
  ['dentist', 'zahnarzt', 'tooth', 'zahn'],
  ['mic', 'mikro', 'mikrofon', 'microphone'],
  ['fridge', 'kühlschrank', 'hum', 'brummt'],
  ['lost', 'verlaufen', 'maze', 'labyrinth'],
  ['avatar', 'avatar', 'physbones'],
];

/** Query -> the surface forms a real embedding would put nearby. */
export function expand(q) {
  const words = q
    .toLowerCase()
    .split(/[^\p{L}\p{N}]+/u)
    .filter((w) => w.length > 2);
  const out = new Set(words.map((w) => ` ${w}`));
  for (const w of words) {
    for (const row of GLOSS) {
      // Prefix matching, but not so loose that "den" reaches "dentist":
      // a shorter query word only matches a longer gloss term from four
      // characters up, and a longer one only within three of a stem.
      const near = (t) =>
        t === w || (w.length >= 4 && t.startsWith(w)) || (w.startsWith(t) && w.length - t.length <= 3);
      if (row.some(near)) {
        for (const t of row) out.add(` ${t}`);
      }
    }
  }
  return [...out];
}

/** Reciprocal-rank fusion, k=60, matching docs/PROTOCOL.md. */
export function rrf(keyword, semantic, k = 60) {
  const at = new Map();
  const push = (id, rank, leg) => {
    if (!at.has(id)) at.set(id, { id, score: 0, via: leg, keyword_rank: null, semantic_rank: null });
    const e = at.get(id);
    if (e[`${leg}_rank`] != null) return;
    e[`${leg}_rank`] = rank;
    e.score += 1 / (k + rank);
    if (e.keyword_rank != null && e.semantic_rank != null) e.via = 'both';
    else e.via = leg;
  };
  keyword.forEach((id, i) => push(id, i + 1, 'keyword'));
  semantic.forEach((id, i) => push(id, i + 1, 'semantic'));
  const best = (e) => Math.min(e.keyword_rank ?? Infinity, e.semantic_rank ?? Infinity);
  return [...at.values()].sort((a, b) => b.score - a.score || best(a) - best(b) || a.id - b.id);
}

// ---------------------------------------------------------------------------
// the transcript's paging, as a conformance twin of the daemon's
// ---------------------------------------------------------------------------

/**
 * `service::time_param`, in JavaScript. ISO-8601 or a number; a number under
 * 1e15 is milliseconds and anything at or above it is nanoseconds. Returns
 * milliseconds, because that is what the mock's rows are stored in.
 *
 * The mock used to call `Date.parse` on this, which silently returned NaN for
 * every numeric timestamp — so a client that paged with `to: seg.t_ms` (the
 * shape the daemon documents and accepts) got the WHOLE history back from a
 * mock that thought it had no filter at all.
 */
export function mockTimeParam(value) {
  if (value == null) return null;
  if (typeof value === 'number') {
    return Math.abs(value) >= 1e15 ? Math.round(value / 1e6) : value;
  }
  const n = Date.parse(String(value));
  return Number.isFinite(n) ? n : null;
}

/**
 * `Store::segment_rows`, in JavaScript, held to the same expectation table as
 * the daemon's own unit test (crates/recalld/src/store.rs
 * `a_to_only_transcript_page_is_the_newest_rows_before_it`, and
 * gui/test/paging.test.js which asserts both halves against it).
 *
 * The rule that matters, and the reason this function exists rather than four
 * lines of `filter`: a query is ANCHORED by `from` or `session` and by nothing
 * else. Anchored, the limit takes the OLDEST rows in range; unanchored, it
 * takes the NEWEST — which is what makes `{to, limit}` mean "the newest limit
 * rows strictly before T", the primitive the GUI pages backwards on.
 */
export function transcriptPage(all, params = {}) {
  const from = mockTimeParam(params?.from);
  const to = mockTimeParam(params?.to);
  const session = params?.session == null ? null : Number(params.session);
  const speaker = params?.speaker == null ? null : Number(params.speaker);
  const source = params?.source == null ? null : String(params.source);
  const limit = Math.min(10_000, Math.max(1, Number(params?.limit ?? 500)));

  const rows = all
    .filter((s) => (session == null || s.session === session)
      && (speaker == null || s.speaker === speaker)
      && (source == null || s.source === source)
      // `from` is inclusive and `to` is exclusive — the asymmetry is what lets
      // a client page with the timestamp of a row it already holds.
      && (from == null || s.t_ms >= from)
      && (to == null || s.t_ms < to))
    .sort((a, b) => a.t_ms - b.t_ms || a.id - b.id);

  const anchored = from != null || session != null;
  return anchored ? rows.slice(0, limit) : rows.slice(-limit);
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

export function startMock({
  sockPath = defaultMockSocket(),
  feedMs = 2000,
  seqStart = 41823,
  quiet = false,
  // 0.6.5: whether this mock daemon has the optional semantic model. Both
  // answers are real states of a real install and the UI has to be right in
  // both, so the driver runs the app against each.
  semantic = true,
} = {}) {
  const log = quiet ? () => {} : (...a) => console.log('[mockd]', ...a);

  const state = {
    seq: seqStart,
    paused: false,
    speakers: SPEAKERS.map((s) => ({ ...s })),
    segments: buildHistory(),
    sources: SOURCES.map((s) => ({ ...s })),
    // Off by default, exactly as the real daemon ships it.
    mic: { enabled: false, mode: 'follow', active: false, device: null },
    // 0.10.0: the second, physical microphone. Off, and with no device — the
    // state a fresh install is in, and the one the card has to be usable from.
    room: { enabled: false, mode: 'follow', device: null },
    devices: DEVICES.map((d) => ({ ...d })),
    // 0.9.0's Discord bridge, on and having heard from the plugin a moment
    // ago, so the "receiving" light is reachable in a screenshot.
    truth: { enabled: true, listening: '127.0.0.1:7797', last_event_ms: Date.now() - 4_000 },
    truthUsers: DISCORD_USERS.map((u) => ({ ...u })),
    // The memory graph (schema v7). Tier 3 is off, like the real daemon, and
    // the model IS installed — so the view's "turn it on" path is reachable
    // rather than blocked behind a 1.9 GB download nobody can do in a test.
    graph: { enabled: false, installed: true, llm_threads: 4, gpu_layers: 0 },
    enrichment: {
      phase: 'off',
      reason: null,
      thread: null,
      batch_done: 0,
      batch_total: 0,
      walked: 0,
      found: 0,
      retracted: 0,
      labelled: 0,
      last_error: null,
      last_run_utc_ns: null,
    },
    enrichTimer: null,
    commitments: PROMISES.map((p, i) => ({
      id: 900 + i,
      segment: p.segment,
      who: p.who,
      to: p.to,
      what: p.what,
      said: p.said,
      due_ms: p.due_in == null ? null : Date.now() + p.due_in,
      due_raw: p.due_raw,
      due_kind: p.due_kind,
      state: 'candidate',
      source: p.source,
      model_id: p.source === 'llm' ? 'qwen2.5-3b-instruct-q4_k_m' : 'promise-rules@1',
      confidence: p.confidence,
      created_at: Date.now(),
      updated_at: Date.now(),
    })),
    topics: { ...THREAD_TOPICS },
    // 0.8.0 --------------------------------------------------------------
    notes: NOTES.map((n) => ({
      ...n,
      // 0.9.0. Absolute at boot, so a reminder that is "in forty minutes" is
      // still in forty minutes however long the mock has been up.
      due_ms: n.due_in == null ? null : Date.now() + n.due_in,
      due_ns: n.due_in == null ? null : String(Date.now() + n.due_in) + '000000',
      fired: !!n.fired,
      fired_ms: n.fired ? Date.now() + n.due_in : null,
    })),
    digests: DIGESTS.map((d) => ({ ...d })),
    vocab: { user: [...VOCAB_USER] },
    /// The translation settings (0.10.2), live and settable like the real
    /// daemon's. Deliberately NOT the shipped defaults: the mock's job is to
    /// be the state worth looking at, and `read_languages: ['de']` with a
    /// German target is the only combination the canned translations above are
    /// coherent with — the English turns carry a German reading, and the
    /// German ones carry none.
    assist: { translate_to: 'de', read_languages: ['de'], translation_display: 'main' },
    /// Every `segments.correct` this daemon has served, which is where
    /// `accuracy.summary` comes from: the pre-correction text lives in the
    /// record, so a WER estimate is arithmetic over real edits rather than a
    /// number the daemon made up. Seeded with three, so the card has something
    /// honest to show before anybody has corrected anything in this session.
    corrections: [],
    /// Which of SIGUSR2's deliveries comes next (see the header).
    nudges: 0,
    myLineIdx: 0,
    tombstones: new Map(), // merged-away speaker id → surviving id (never chained)
    replay: [],
    nextSegId: 2000,
    nextOp: 400,
    restarts: 0,
    feedIdx: 0,
    startedAt: Date.now(),
    drops: 0,
    queue: 0,
    semantic,
  };

  const clients = new Set(); // {sock, topics:Set, name}

  function emit(topic, ev, data) {
    state.seq += 1;
    const frame = { seq: state.seq, ev, data };
    state.replay.push({ topic, frame });
    if (state.replay.length > REPLAY_MAX) state.replay.shift();
    const line = JSON.stringify(frame) + '\n';
    for (const c of clients) {
      if (c.topics.has(topic)) {
        try {
          c.sock.write(line);
        } catch {
          /* a dead client is reaped by its own close handler */
        }
      }
    }
    return frame;
  }

  // What this daemon calls itself. It moves with every simulated restart: a
  // hub update replaces the binary and the daemon comes back as a different
  // version, which is what a still-running GUI has to notice. $NX_RECALL_MOCK_V2
  // pins the post-restart string when a test wants to name it.
  function daemonId() {
    if (!state.restarts) return DAEMON;
    return process.env.NX_RECALL_MOCK_V2 || `${DAEMON}+${state.restarts}`;
  }

  function speakerById(id) {
    const resolved = state.tombstones.get(id) ?? id;
    return state.speakers.find((s) => s.id === resolved) ?? null;
  }

  /// A segment's canonical speaker, following tombstones like the daemon does.
  function owner(seg) {
    if (seg.speaker == null) return null;
    return state.tombstones.get(seg.speaker) ?? seg.speaker;
  }

  /// The `{name, auto, colour, icon}` block every place a person appears in
  /// the graph, so a client can render a voice it has never queried.
  ///
  /// The highlight travels with the name for exactly that reason: a
  /// participant chip, a commitment's "who", an edge in "people they talk
  /// with" are all drawn from THIS and never from `speakers.list`, so a client
  /// that had to join the two would paint the memory graph in the hashed
  /// colours while the transcript beside it used the chosen ones.
  function person(id) {
    const sp = speakerById(id);
    return {
      name: sp?.name ?? null,
      auto: sp?.auto ?? `Speaker_${id}`,
      colour: sp?.colour ?? null,
      icon: sp?.icon ?? null,
    };
  }

  /// One conversation, without its words — the shape both graph methods share.
  function threadPayload(id) {
    const rows = state.segments.filter((s) => s.thread === id);
    const spoke = new Map();
    for (const seg of rows) {
      const who = owner(seg);
      if (who == null) continue;
      spoke.set(who, (spoke.get(who) ?? 0) + seg.dur_ms);
    }
    const started = rows.length ? Math.min(...rows.map((s) => s.t_ms)) : 0;
    const ended = rows.length ? Math.max(...rows.map((s) => s.t_ms + s.dur_ms)) : 0;
    return {
      thread_id: id,
      session: rows[0]?.session ?? null,
      started_ms: started,
      started_ns: String(started) + '000000',
      ended_ms: ended,
      ended_ns: String(ended) + '000000',
      segments: rows.length,
      // Most talkative first: the order a person reads a list of names in.
      participants: [...spoke.entries()]
        .sort((a, b) => b[1] - a[1])
        .map(([id]) => ({ speaker_id: id, ...person(id) })),
      preview: rows.find((s) => s.speaker != null && s.text)?.text ?? null,
      // 0.10.0: where it happened, and who did the talking.
      world: worldPayload(worldOfThread(id)),
      stats: { shares: sharesOf(rows) },
    };
  }

  // ---- 0.10.0: worlds and turn-taking ------------------------------------

  /// Resolve a `world` facet the way the daemon does: an id matches exactly, a
  /// name matches as a case-insensitive substring, and a facet nobody has a
  /// world for selects NOTHING rather than everything.
  function resolveWorld(facet) {
    const f = String(facet ?? '').trim();
    if (!f) return null;
    if (f.startsWith('wrld_')) return [f];
    const hit = WORLDS.filter((w) => w.name.toLowerCase().includes(f.toLowerCase()));
    return hit.length ? hit.map((w) => w.world_id) : ['\u0000no-such-world'];
  }

  function withinWorld(rows, facet) {
    const ids = resolveWorld(facet);
    if (!ids) return rows;
    return rows.filter((s) => ids.includes(worldOfThread(s.thread)));
  }

  function worldPayload(worldId) {
    if (!worldId) return null;
    const w = WORLDS.find((x) => x.world_id === worldId);
    return { world_id: worldId, name: w?.name ?? null };
  }

  /// Every identified voice's share of a set of rows, most talkative first.
  /// The same definition the daemon uses: speech time, not turn count.
  function sharesOf(rows) {
    const acc = new Map();
    let total = 0;
    for (const seg of rows) {
      const who = owner(seg);
      if (who == null) continue;
      const c = acc.get(who) ?? { turns: 0, ms: 0 };
      c.turns += 1;
      c.ms += seg.dur_ms;
      acc.set(who, c);
      total += seg.dur_ms;
    }
    return [...acc.entries()]
      .map(([speaker_id, c]) => ({
        speaker_id,
        turns: c.turns,
        speech_ms: c.ms,
        share: total ? c.ms / total : 0,
      }))
      .sort((a, b) => b.speech_ms - a.speech_ms || a.speaker_id - b.speaker_id);
  }

  /// The turns of every conversation this voice took part in — the whole
  /// conversation, because a share is a fraction of somebody else's speech too.
  function turnsWith(speakerId, days) {
    const threads = new Set(
      state.segments.filter((s) => owner(s) === speakerId).map((s) => s.thread).filter((t) => t != null)
    );
    const from = days == null ? null : Date.now() - days * DAY;
    return state.segments
      .filter((s) => s.thread != null && threads.has(s.thread) && (from == null || s.t_ms >= from))
      .sort((a, b) => a.thread - b.thread || a.t_ms - b.t_ms || a.id - b.id);
  }

  /// The stats block, to the same definitions the daemon documents. The mock
  /// is not a second implementation of the rules — it is a fixture that has to
  /// be the right SHAPE — but the arithmetic is the real arithmetic so the
  /// GUI's bars and captions are exercised against numbers that add up.
  function statsOf(speakerId, days) {
    const turns = turnsWith(speakerId, days);
    const byThread = new Map();
    for (const t of turns) {
      if (!byThread.has(t.thread)) byThread.set(t.thread, []);
      byThread.get(t.thread).push(t);
    }
    let mine = 0;
    let total = 0;
    let myTurns = 0;
    let longest = 0;
    let span = 0;
    let given = 0;
    let received = 0;
    const latencies = [];
    const by = [];

    for (const [thread, rows] of byThread) {
      const start = Math.min(...rows.map((r) => r.t_ms));
      const end = Math.max(...rows.map((r) => r.t_ms + r.dur_ms));
      span += end - start;
      let runStart = null;
      let runEnd = 0;
      let threadMine = 0;
      let threadTotal = 0;
      let threadTurns = 0;
      rows.forEach((r, i) => {
        const who = owner(r);
        if (who == null) return;
        total += r.dur_ms;
        threadTotal += r.dur_ms;
        if (who === speakerId) {
          mine += r.dur_ms;
          myTurns += 1;
          threadMine += r.dur_ms;
          threadTurns += 1;
          if (runStart == null) {
            runStart = r.t_ms;
            runEnd = r.t_ms + r.dur_ms;
          } else {
            runEnd = Math.max(runEnd, r.t_ms + r.dur_ms);
          }
          longest = Math.max(longest, runEnd - runStart);
        } else if (runStart != null) {
          runStart = null;
        }
        // An interruption: it starts inside somebody else's turn AND its own
        // audio holds overlapped speech. The mock has no overlap column, so it
        // stands in the one thing it does have — a turn that begins before the
        // one before it has ended.
        for (const a of rows.slice(0, i)) {
          const other = owner(a);
          if (other == null || other === who) continue;
          if (a.t_ms < r.t_ms && r.t_ms < a.t_ms + a.dur_ms) {
            if (who === speakerId) given += 1;
            if (other === speakerId) received += 1;
          }
        }
        if (who === speakerId) {
          const prev = rows.slice(0, i).reverse().find((p) => owner(p) != null);
          if (prev && owner(prev) !== speakerId) {
            const gap = r.t_ms - (prev.t_ms + prev.dur_ms);
            if (gap >= 0 && gap <= 5000) latencies.push(gap);
          }
        }
      });
      if (threadTurns) {
        by.push({
          thread_id: thread,
          turns: threadTurns,
          share: threadTotal ? threadMine / threadTotal : 0,
          last_ms: end,
        });
      }
    }
    latencies.sort((a, b) => a - b);
    const median = latencies.length
      ? latencies.length % 2
        ? latencies[(latencies.length - 1) / 2]
        : Math.trunc((latencies[latencies.length / 2 - 1] + latencies[latencies.length / 2]) / 2)
      : null;
    by.sort((a, b) => b.last_ms - a.last_ms || b.thread_id - a.thread_id);
    return {
      turns: myTurns,
      speech_ms: mine,
      conversation_speech_ms: total,
      share: total ? mine / total : 0,
      mean_turn_ms: myTurns ? Math.trunc(mine / myTurns) : 0,
      longest_monologue_ms: longest,
      interruptions_given: given,
      interruptions_received: received,
      median_latency_ms: median,
      span_ms: span,
      turns_per_minute: span ? myTurns / (span / 60000) : 0,
      by_conversation: by.slice(0, 10),
    };
  }

  function counts() {
    const m = new Map();
    for (const seg of state.segments) {
      if (seg.speaker == null) continue;
      const id = state.tombstones.get(seg.speaker) ?? seg.speaker;
      const c = m.get(id) ?? { segments: 0, total_ms: 0 };
      c.segments += 1;
      c.total_ms += seg.dur_ms;
      m.set(id, c);
    }
    return m;
  }

  function speakerList() {
    const c = counts();
    const you = youSpeaker();
    return state.speakers.map((s) => ({
      id: s.id,
      name: s.name,
      auto: s.auto,
      // Exactly one row can be true: the voice the microphone pins.
      you: s.id === you,
      // schema v5: null is "any", which is what a voice speaks until somebody
      // says otherwise.
      languages: s.languages ?? null,
      // The per-person highlight. A TOKEN, not a colour, and null for almost
      // everybody — this is the single projection every view reads a voice
      // out of, so a field missing here is a field missing from the whole app.
      colour: s.colour ?? null,
      icon: s.icon ?? null,
      first_seen: s.first_seen,
      segments: c.get(s.id)?.segments ?? 0,
      total_ms: c.get(s.id)?.total_ms ?? 0,
      // 0.11.0: where this voice has actually been heard. Derived from the
      // segments, exactly as the daemon derives it, so the chips can never
      // disagree with the transcript the same mock is serving.
      sources: speakerSources(s.id),
    }));
  }

  /// A voice's source history, most-heard first.
  function speakerSources(spId) {
    const acc = new Map();
    for (const seg of state.segments) {
      if (owner(seg) !== spId) continue;
      const row = acc.get(seg.source) ?? { segments: 0, last: 0 };
      row.segments += 1;
      row.last = Math.max(row.last, seg.t_ms + seg.dur_ms);
      acc.set(seg.source, row);
    }
    return [...acc.entries()]
      .map(([source, row]) => {
        const meta = state.sources.find((s) => s.match_key === source);
        return {
          source,
          name: meta?.display ?? source,
          kind: meta?.kind ?? 'app',
          segments: row.segments,
          last_ms: row.last,
          last_ns: String(row.last) + '000000',
        };
      })
      .sort((a, b) => b.segments - a.segments || a.source.localeCompare(b.source));
  }

  /// The voices a sweep would take: one segment at most, under three seconds
  /// of speech, unnamed, and never the pinned "You".
  function pruneCandidates() {
    const c = counts();
    const you = youSpeaker();
    return state.speakers
      .filter((s) => !s.name && s.id !== you)
      .map((s) => ({
        id: s.id,
        auto: s.auto,
        name: s.name,
        // The preview is a list of people about to be deleted, and it is the
        // one screen where recognising a voice at a glance matters most — so
        // it gets the same highlight every other list of people carries.
        colour: s.colour ?? null,
        icon: s.icon ?? null,
        segments: c.get(s.id)?.segments ?? 0,
        total_ms: c.get(s.id)?.total_ms ?? 0,
      }))
      .filter((s) => s.segments <= PRUNE_MAX_SEGMENTS && s.total_ms < PRUNE_MAX_SPEECH_MS);
  }

  /// The pinned speaker, followed through tombstones exactly as the daemon
  /// does: merging "You" into a named voice moves the pin, it does not mint a
  /// second user.
  function youSpeaker() {
    const resolved = state.tombstones.get(YOU_SPEAKER) ?? YOU_SPEAKER;
    return state.speakers.some((s) => s.id === resolved) ? resolved : null;
  }

  /// The mic state machine, mirroring `capture::mic_plan`: `follow` is open
  /// exactly while an allowed application is capturing, `always` ignores that.
  function micActive() {
    if (!state.mic.enabled) return false;
    if (state.mic.mode === 'always') return true;
    return state.sources.some((s) => s.kind !== 'mic' && s.allowed && s.streams > 0);
  }

  function micState() {
    if (!state.mic.enabled) return 'off';
    const active = micActive();
    return state.mic.mode === 'always' ? (active ? 'always:active' : 'always:idle') : active ? 'following:active' : 'following:idle';
  }

  function micPayload() {
    return {
      enabled: state.mic.enabled,
      mode: state.mic.mode,
      active: micActive(),
      state: micState(),
      device: state.mic.device,
    };
  }

  /// The room tap's state machine (0.10.0). The mic's, plus the one state the
  /// headset cannot be in: on with no device chosen, which is not "waiting".
  function roomActive() {
    if (!state.room.enabled || !state.room.device) return false;
    if (state.room.mode === 'always') return true;
    return state.sources.some((s) => s.kind === 'app' && s.allowed && s.streams > 0);
  }

  function roomState() {
    if (!state.room.enabled) return 'off';
    if (!state.room.device) return 'needs-device';
    const active = roomActive();
    return state.room.mode === 'always' ? (active ? 'always:active' : 'always:idle') : active ? 'following:active' : 'following:idle';
  }

  function roomPayload() {
    return {
      enabled: state.room.enabled,
      mode: state.room.mode,
      active: roomActive(),
      state: roomState(),
      device: state.room.device,
    };
  }

  /// Disk usage, in the four parts that behave differently. Derived from the
  /// canned world rather than invented: audio is ~32 KB per second of segment,
  /// which is what 16 kHz 16-bit mono actually weighs.
  function storagePayload() {
    const audioMs = state.segments.reduce((n, s) => n + s.dur_ms, 0);
    const audio_bytes = Math.round((audioMs / 1000) * 32000);
    const db_bytes = 180 * 1024 + state.segments.length * 900;
    const goldens_bytes = 3 * 32000 * 4;
    const models_bytes = 707 * 1024 * 1024;
    return {
      db_bytes,
      audio_bytes,
      audio_files: state.segments.length,
      goldens_bytes,
      models_bytes,
      total_bytes: db_bytes + audio_bytes + goldens_bytes + models_bytes,
      measured_at_utc_ns: String(Date.now()) + '000000',
    };
  }

  // --- the memory graph ---------------------------------------------------

  function graphConfig() {
    return {
      enabled: state.graph.enabled,
      installed: state.graph.installed,
      llm_threads: state.graph.llm_threads,
      // The range `graph.set` clamps to, on the wire exactly as the daemon
      // sends it (crate::control::GRAPH_THREADS): the Memory tab builds its
      // stepper out of this rather than out of a number in its own source.
      llm_threads_min: GRAPH_THREADS[0],
      llm_threads_max: GRAPH_THREADS[1],
      gpu_layers: state.graph.gpu_layers,
      llm_model: 'qwen2.5-3b-instruct-q4_k_m.gguf',
      thread_gap_s: 20,
      batch_threads: 4,
      min_thread_segments: 3,
      download_bytes: 1_946_604_700,
    };
  }

  function commitmentPayload(c) {
    return {
      id: c.id,
      segment: c.segment,
      thread: state.segments.find((s) => s.id === c.segment)?.thread ?? null,
      who: { speaker_id: c.who, ...person(c.who) },
      to: c.to == null ? null : { speaker_id: c.to, ...person(c.to) },
      what: c.what,
      said: c.said,
      due_ms: c.due_ms,
      due_ns: c.due_ms == null ? null : String(c.due_ms) + '000000',
      due_raw: c.due_raw,
      due_kind: c.due_kind,
      state: c.state,
      source: c.source,
      model_id: c.model_id,
      confidence: c.confidence,
      t_ms: state.segments.find((s) => s.id === c.segment)?.t_ms ?? null,
      t_ns: state.segments.find((s) => s.id === c.segment)?.t_ns ?? null,
      created_ms: c.created_at,
      updated_ms: c.updated_at,
    };
  }

  function graphCounts() {
    const by = (k) => state.commitments.filter((c) => c.state === k).length;
    const src = (k) => state.commitments.filter((c) => c.source === k).length;
    const threads = new Set(state.segments.map((s) => s.thread).filter((t) => t != null));
    const enriched = new Set(Object.keys(state.topics).map(Number));
    return {
      time_refs: state.commitments.filter((c) => c.due_raw).length,
      commitments: state.commitments.length,
      open: by('candidate') + by('confirmed'),
      candidates: by('candidate'),
      confirmed: by('confirmed'),
      done: by('done'),
      dismissed: by('dismissed'),
      from_rules: src('rules'),
      from_llm: src('llm'),
      topics: new Set(Object.values(state.topics)).size,
      threads: threads.size,
      threads_enriched: [...enriched].filter((t) => threads.has(t)).length,
      threads_pending: [...threads].filter((t) => !enriched.has(t)).length,
    };
  }

  /// Flip the Tier 3 switch, and act like a worker that has been asked to run.
  ///
  /// Turning it on walks a short batch so the GUI can be driven through all
  /// three states it has copy for — off, running with a progress line, and idle
  /// — without a 1.9 GB model in the test rig.
  function setEnrichment(on) {
    state.graph.enabled = on;
    if (state.enrichTimer) {
      clearInterval(state.enrichTimer);
      state.enrichTimer = null;
    }
    if (!on) {
      state.enrichment = { ...state.enrichment, phase: 'off', reason: null, thread: null, batch_done: 0, batch_total: 0 };
      return;
    }
    if (!state.graph.installed) {
      state.enrichment = {
        ...state.enrichment,
        phase: 'unavailable',
        reason: 'the local model is not installed — `recalld models fetch --graph` downloads it (1.9 GB)',
      };
      return;
    }
    const total = 4;
    state.enrichment = { ...state.enrichment, phase: 'running', reason: null, batch_done: 0, batch_total: total };
    const op = `graph_${state.nextOp++}`;
    let done = 0;
    state.enrichTimer = setInterval(() => {
      done += 1;
      state.enrichment = {
        ...state.enrichment,
        batch_done: done,
        walked: state.enrichment.walked + 1,
        labelled: state.enrichment.labelled + 1,
        last_run_utc_ns: String(Date.now()) + '000000',
      };
      emit('ops', 'op.progress', { op, kind: 'graph.enrich', done, total, frac: done / total });
      emit('status', 'graph', { ...state.enrichment });
      if (done >= total) {
        clearInterval(state.enrichTimer);
        state.enrichTimer = null;
        state.enrichment = { ...state.enrichment, phase: 'idle', thread: null, batch_done: 0, batch_total: 0 };
        emit('ops', 'op.done', { op, kind: 'graph.enrich', done: total, total });
        emit('status', 'graph', { ...state.enrichment });
      }
    }, 700);
    if (state.enrichTimer.unref) state.enrichTimer.unref();
  }

  // --- 0.8.0: vocabulary, accuracy, notes, one query box, briefs ----------

  /// The list the transducer is actually biased toward: the user glossary plus
  /// every auto group, deduplicated, user terms first. Order is not decoration
  /// — a hotword list has a budget, and what a person typed outranks what the
  /// daemon noticed.
  function effectiveVocab() {
    const out = [];
    const seen = new Set();
    for (const term of [...state.vocab.user, ...VOCAB_AUTO.roster, ...VOCAB_AUTO.worlds, ...VOCAB_AUTO.corrections]) {
      const key = term.toLowerCase();
      if (seen.has(key)) continue;
      seen.add(key);
      out.push(term);
    }
    return out;
  }

  function vocabPayload() {
    return {
      user: [...state.vocab.user],
      auto: {
        roster: [...VOCAB_AUTO.roster],
        worlds: [...VOCAB_AUTO.worlds],
        corrections: [...VOCAB_AUTO.corrections],
      },
      effective: effectiveVocab(),
    };
  }

  /// The languages a turn may be in without being translated: what was set,
  /// plus the target, which is read by definition. The daemon folds it in the
  /// same way and for the same reason — a target you would then translate away
  /// from is not a setting anybody meant.
  function effectiveRead() {
    const out = [...state.assist.read_languages];
    const to = state.assist.translate_to;
    if (to && !out.includes(to)) out.push(to);
    return out;
  }

  /// `assist.get`'s answer, `assist.set`'s reply, and the `assist` event, from
  /// one function so the three cannot drift.
  function assistState() {
    return {
      translate_to: state.assist.translate_to,
      read_languages: effectiveRead(),
      translation_display: state.assist.translation_display,
      languages: LANGUAGES.map((l) => ({ ...l })),
    };
  }

  /// Word-level error rate between what was transcribed and what a person said
  /// it should have been. A real daemon runs a proper alignment; this is the
  /// same NUMBER SHAPE from a cheap one, which is all a client can be written
  /// against — the point is that the figure moves when somebody corrects
  /// something, and that it is derived rather than invented.
  function wordDiff(before, after) {
    const a = String(before ?? '').toLowerCase().split(/\s+/).filter(Boolean);
    const b = String(after ?? '').toLowerCase().split(/\s+/).filter(Boolean);
    const bag = new Map();
    for (const w of a) bag.set(w, (bag.get(w) ?? 0) + 1);
    let kept = 0;
    for (const w of b) {
      const n = bag.get(w) ?? 0;
      if (n > 0) {
        bag.set(w, n - 1);
        kept += 1;
      }
    }
    return { errors: Math.max(a.length, b.length) - kept, words: Math.max(1, a.length) };
  }

  function recordCorrection(seg, before, after) {
    const d = wordDiff(before, after);
    state.corrections.push({
      segment_id: seg.id,
      source: seg.source,
      speaker_id: owner(seg),
      errors: d.errors,
      words: d.words,
      at_ms: Date.now(),
    });
  }

  /// Three corrections that happened before this session, so the dashboard has
  /// numbers on first paint. A card whose honest empty state is the only thing
  /// anybody ever photographs is a card nobody has actually looked at.
  function seedCorrections() {
    const seeds = [
      [1000, 'wait, which portal was it — the one behind the bar or the one in the stairwell?', 'wait, which portal was it — the one behind the bar or the one in the stairway?'],
      [1003, 'no rush, we are still waiting on two people', 'no rush, we are still waiting on two more'],
      [10_007, 'we should write some of this down at some point', 'we should write some of it down at some point'],
    ];
    for (const [id, after, before] of seeds) {
      const seg = state.segments.find((s) => s.id === id);
      if (seg) recordCorrection(seg, before, after);
    }
  }

  function wer(rows) {
    const errors = rows.reduce((n, c) => n + c.errors, 0);
    const words = rows.reduce((n, c) => n + c.words, 0);
    return words ? Math.round((errors / words) * 10_000) / 10_000 : 0;
  }

  function accuracyPayload() {
    const group = (key) => {
      const m = new Map();
      for (const c of state.corrections) {
        const k = c[key];
        if (k == null) continue;
        const g = m.get(k) ?? [];
        g.push(c);
        m.set(k, g);
      }
      return [...m.entries()].sort((a, b) => b[1].length - a[1].length);
    };
    // 0.10.1: the cross-check's verdicts over every row, not only the fixed
    // ones — counted from the canned segments so the card's headline and the
    // transcript's shaky marks come from the same rows.
    const cc = (rows) => {
      const solid = rows.filter((g) => g.asr_confidence === 'solid').length;
      const shaky = rows.filter((g) => g.asr_confidence === 'shaky').length;
      return { checked: solid + shaky, solid, shaky, shaky_share: solid + shaky ? shaky / (solid + shaky) : null };
    };
    const segsBySource = (source) => state.segments.filter((g) => (g.source ?? 'app') === source);
    const segsBySpeaker = (id) => state.segments.filter((g) => g.speaker === id);
    return {
      corrections: state.corrections.length,
      estimated_wer: wer(state.corrections),
      edit_rate: wer(state.corrections),
      cross_check: cc(state.segments),
      by_source: group('source').map(([source, rows]) => ({
        source,
        corrections: rows.length,
        estimated_wer: wer(rows),
        edit_rate: wer(rows),
        cross_check: cc(segsBySource(source)),
      })),
      by_speaker: group('speaker_id').map(([speaker_id, rows]) => ({
        speaker_id,
        corrections: rows.length,
        estimated_wer: wer(rows),
        edit_rate: wer(rows),
        cross_check: cc(segsBySpeaker(speaker_id)),
      })),
      since_ns: String(state.startedAt - 30 * DAY) + '000000',
    };
  }

  function notePayload(n) {
    return {
      id: n.id,
      segment_id: n.segment_id,
      text: n.text,
      t_ms: n.t_ms,
      t_ns: n.t_ns,
      state: n.state,
      // 0.9.0: when it asked to come back, and whether it has. `due_ms` is
      // null on most notes — a sentence with no time in it — and `fired` is
      // not "done": a reminder that has gone off is still an open note.
      due_ms: n.due_ms ?? null,
      due_ns: n.due_ms == null ? null : String(n.due_ms) + '000000',
      fired: !!n.fired,
      fired_ms: n.fired_ms ?? null,
    };
  }

  /// One digest on the wire, the shape `digest.list` and the `digest` event
  /// both carry.
  function digestPayload(d) {
    const day = new Date(Date.now() - d.day_offset * DAY);
    const started = day.getTime() - 3 * 3600_000;
    const iso = `${day.getFullYear()}-${String(day.getMonth() + 1).padStart(2, '0')}-${String(
      day.getDate()
    ).padStart(2, '0')}`;
    return {
      thread_id: d.thread_id,
      day: iso,
      lang: d.lang,
      // 0.11.6: `summary` / `open` are the prose a person reads, with names
      // in them. `*_raw` is what the model wrote, so nothing is lost and a
      // client that wants the letters can still have them; `rendered` says
      // which way this row got its names.
      summary: d.summary,
      summary_raw: d.summary_raw ?? d.summary,
      open: d.open ?? [],
      open_raw: d.open_raw ?? d.open ?? [],
      rendered: d.rendered ?? 'names',
      // 0.10.0: the share travels ON the participant, not as a parallel list
      // — a client that has to join two arrays to draw one bar will
      // eventually join them wrong.
      participants: (() => {
        const shares = sharesOf(state.segments.filter((s) => s.thread === d.thread_id));
        return (d.people ?? []).map((id) => {
          const sh = shares.find((x) => x.speaker_id === id);
          return {
            speaker_id: id,
            // 0.11.6: the same label the paragraph above the chips uses — the
            // name if there is one, else the auto label as a name says it
            // ("Speaker 07", not "Speaker_07"). A chip that spells it the
            // other way beside the sentence reads as two different people.
            label: (() => {
              const sp = state.speakers.find((s) => s.id === id);
              return sp?.name ?? (sp?.auto ?? '').replace(/^Speaker_(\d+)$/, 'Speaker $1');
            })(),
            // The highlight beside the label, for the same reason the label is
            // here at all: the digest card draws its chips from this list and
            // nothing else, and a chip in the hashed colour next to a
            // transcript row in the chosen one reads as two different people.
            colour: person(id).colour,
            icon: person(id).icon,
            share: sh?.share ?? null,
            turns: sh?.turns ?? null,
          };
        });
      })(),
      started_ms: started,
      started_ns: String(started) + '000000',
      ended_ms: started + 40 * 60_000,
      ended_ns: String(started + 40 * 60_000) + '000000',
      turns: 12,
      world: worldOfThread(d.thread_id),
      model_id: 'qwen2.5-3b-instruct-q4_k_m@1',
      created_ms: Date.now(),
    };
  }

  /// The Tier-2 parser, in miniature: pull a speaker mention and a time phrase
  /// out of a natural-language question and hand back what is left as the
  /// query. Deliberately the same SHAPE as the daemon's — the interpretation is
  /// returned so the GUI can show what was understood and let a person take a
  /// facet back off, and that only works if the leftovers are really the query.
  const TIME_PHRASES = [
    [/\b(yesterday|gestern)\b/i, 1, 1],
    [/\b(today|heute)\b/i, 0, 0],
    [/\b(last week|letzte woche|this week|diese woche)\b/i, 7, 0],
  ];

  const escapeRe = (t) => String(t).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

  function interpret(q) {
    let rest = String(q ?? '');
    const out = { query: '', mode: state.semantic ? 'hybrid' : 'keyword' };

    // Names first: a speaker called "Heute" would otherwise lose their name to
    // the clock, and a name is the more specific claim.
    for (const sp of state.speakers) {
      const name = sp.name;
      if (!name) continue;
      const re = new RegExp(`\\b${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\b`, 'i');
      if (!re.test(rest)) continue;
      out.speaker_id = sp.id;
      out.speaker_label = name;
      rest = rest.replace(re, ' ');
      break;
    }

    // 0.10.0 — "in <world>", "in the <world> world", "in der <world> Welt".
    // The preposition is mandatory: a world name is free text and "who
    // mentioned the Great Pug" is a search, not a filter. Longest name first.
    for (const w of [...WORLDS].sort((a, b) => b.name.length - a.name.length)) {
      const bare = w.name.replace(/^(the|a)\s+/i, '');
      const alt = `(?:${escapeRe(w.name)}|${escapeRe(bare)})`;
      const re = new RegExp(`\\bin\\s+(?:der|die|das|dem|den|the|a)?\\s*${alt}(?:\\s+(?:welt|world))?\\b`, 'i');
      if (!re.test(rest)) continue;
      out.world_id = w.world_id;
      out.world_label = w.name;
      rest = rest.replace(re, ' ');
      break;
    }

    for (const [re, backDays, spanDays] of TIME_PHRASES) {
      if (!re.test(rest)) continue;
      const midnight = new Date();
      midnight.setHours(0, 0, 0, 0);
      const from = midnight.getTime() - backDays * DAY;
      const to = midnight.getTime() + (1 - spanDays) * DAY;
      out.from_ns = String(from) + '000000';
      out.to_ns = String(to) + '000000';
      rest = rest.replace(re, ' ');
      break;
    }

    // The stop words a question is made of. What is left is what was asked
    // about, and if nothing is left the query is empty — which is a real answer
    // and not an error: "what did Kira say yesterday" is a valid question.
    out.query = rest
      .replace(/\b(what|did|say|said|was|were|about|the|a|an|talk|talked|when|who|hat|hab|habe|gesagt|über|uber|wer|wann)\b/gi, ' ')
      .replace(/[?¿!.,;:]/g, ' ')
      .replace(/\s+/g, ' ')
      .trim();
    // 0.11.0. Was it typed as a question? The daemon says so rather than
    // leaving every client to keep its own interrogative list.
    out.is_question = isQuestion(q);
    return out;
  }

  // --- 0.11.0: is this a question? -----------------------------------------

  // Kept character-for-character in step with the daemon (`ask::is_question`)
  // and with the renderer's copy — see `search.js`. Three copies of one list is
  // how a query becomes a question to the client and not to the daemon.
  const INTERROGATIVES = /^(was|wer|wen|wem|wessen|wann|wo|wohin|woher|wie|warum|wieso|weshalb|wor(?:\u00fc|ue)ber|worum|wovon|welche[rsn]?|what|who|whom|whose|when|where|why|how|which)(?![\w'\u2019])/i;

  // The two questions this fixture can answer, and what it answers with.
  // `cite` is how many of the hits the sentence came from — the ids themselves
  // are read off the reply, never hard-coded, so they cannot point at a row the
  // client was not given.
  const ANSWERS = [
    { match: /portal/, lang: 'en', cite: 2, text: 'The portal is the one in the stairwell, and it only opens after the lights go down.' },
    { match: /shader/, lang: 'en', cite: 1, text: 'Aspen built the shader and put the file on Gumroad.' },
  ];

  // 0.11.x, the Japanese half. Japanese does not front its interrogatives, so
  // the list above finds nothing in a Japanese question; what is positional is
  // the sentence-final particle (`ask.rs`, `JA_QUESTION_ENDINGS`). Longest
  // first, so `ですか` is not read as the bare `か` it ends with.
  const JA_ENDING = /(?:でしょうか|ですか|ますか|かな|か|の)$/;
  const JA_TRAILING = /[?？。.！!…、,」"']+$/;

  function isJapanese(s) {
    let kana = 0;
    let han = 0;
    let latin = 0;
    for (const ch of s) {
      if (/[\u3040-\u30FF\u31F0-\u31FF\uFF66-\uFF9D]/.test(ch)) kana += 1;
      else if (/[\u3400-\u4DBF\u4E00-\u9FFF\uF900-\uFAFF]/.test(ch)) han += 1;
      else if (/\p{L}/u.test(ch)) latin += 1;
    }
    return kana >= 2 || (kana >= 1 && han >= 1) || (han >= 1 && latin === 0 && kana === 0);
  }

  function isQuestion(q) {
    const s = String(q ?? '').trim();
    if (s.endsWith('?') || s.endsWith('？')) return true;
    if (isJapanese(s) && JA_ENDING.test(s.replace(JA_TRAILING, ''))) return true;
    return INTERROGATIVES.test(s.replace(/^[^\p{L}\p{N}]+/u, ''));
  }

  function statusPayload() {
    return {
      uptime_s: Math.round((Date.now() - state.startedAt) / 1000),
      paused: state.paused,
      queue_depth: state.queue,
      drops: state.drops,
      sources_capturing: state.sources.filter((s) => s.allowed && s.streams > 0).length,
      sources_allowed: state.sources.filter((s) => s.allowed).length,
      mic: micPayload(),
      mic_state: micState(),
      // 0.10.0: the room microphone, on the same two keys as the headset.
      room: roomPayload(),
      room_state: roomState(),
      // 0.9.0's Discord ingest, in the four facts the Sources card draws.
      truth: {
        enabled: state.truth.enabled,
        listening: state.truth.enabled ? state.truth.listening : null,
        last_event_ms: state.truth.enabled ? state.truth.last_event_ms : null,
        users: state.truthUsers.length,
      },
      // 0.6.1: measured by the retention sweeper, not by this call. The audio
      // figure moves as the feed runs, so the footer and the Sources card have
      // something that actually changes to render.
      storage: storagePayload(),
      // The memory graph's Tier 3 state rides on `status` too, so a client that
      // missed the `graph` event still converges on the truth.
      graph: { ...state.enrichment },
      models: ['silero-vad', 'segmentation-3.0', 'eres2net-en', 'parakeet-tdt-110m'],
      // 0.6.5: semantic search. Optional in the daemon, so the mock has a
      // switch for it — `--no-semantic` is the state most machines are in and
      // the UI has to be photographed in both.
      semantic: state.semantic
        ? {
            available: true,
            model: 'multilingual-e5-small-int8@1',
            dim: 384,
            resident: state.segments.length,
            resident_bytes: state.segments.length * 384 * 4,
            indexed: state.segments.length,
            eligible: state.segments.length,
            pending: 0,
          }
        : {
            available: false,
            how: 'semantic search is not installed. `recalld models fetch --semantic` installs multilingual-e5-small-int8 (128.9 MB), then `recalld semantic backfill` indexes what has already been said.',
          },
      // 0.9.0: what the three assistant features are set to, so a client can
      // tell "off" from "an older daemon" without guessing. The mock ships
      // with translation ON and pointed at German, because the whole point of
      // a mock is to be the state that is worth looking at.
      assist: {
        reminders: true,
        digest: true,
        // The three values, and NOT the `languages` list: that is
        // `assist.get`'s, because a selector's options do not change and this
        // block is polled every three seconds.
        translate_to: state.assist.translate_to,
        read_languages: effectiveRead(),
        translation_display: state.assist.translation_display,
      },
      segments_total: state.segments.length,
      daemon: daemonId(),
      schema: SCHEMA,
      seq: state.seq,
    };
  }

  // --- the live feed ------------------------------------------------------
  // Every ~2 s a new segment arrives, unless capture is paused. Pause is the
  // marquee feature (DESIGN §8) so it must be visibly, immediately true: a
  // paused mock emits nothing at all, not "slower".
  let feedTimer = null;
  function tick() {
    if (state.paused) return;
    // The Discord bridge is a live stream too: a plugin that is running sends
    // speaking edges continuously, and the card's "receiving" light is drawn
    // from when the last one arrived. Without this the mock would drift into
    // "waiting for Discord" a minute after it started and stay there.
    if (state.truth.enabled) state.truth.last_event_ms = Date.now();
    // Interleaved, not separate: the user's own voice arrives in the same
    // stream as everybody else's, and the only thing that marks it is where it
    // came from. Every third tick, while the mic is actually capturing.
    if (micActive() && state.feedIdx % 3 === 2) {
      state.feedIdx += 1;
      emitMine();
      return;
    }
    const line = state.feedIdx % CANNED_LINES.length;
    const [sp, text, overlap, score] = CANNED_LINES[line];
    state.feedIdx += 1;
    // A voice the client has never seen gets minted mid-session: there is no
    // relabel broadcast for coming into existence, only this segment. The GUI
    // shipped a bug (speakers view frozen at connect-time state) because the
    // mock never exercised this — now it always does, early in the feed.
    let mintedSpeaker = null;
    if (state.feedIdx === 4 && !state.speakers.some((s) => s.id === 77)) {
      mintedSpeaker = { id: 77, name: null, auto: 'Speaker_77', segments: 0, total_ms: 0 };
      state.speakers.push(mintedSpeaker);
    }
    const now = Date.now();
    const seg = {
      id: state.nextSegId++,
      session: SESSIONS[2].id,
      // 0.11.0: a live turn arrives on the source its voice is heard on, like
      // every other turn. A feed that rotated sources independently would walk
      // a Discord-only voice into VRChat one tick at a time.
      source: fillerSource(mintedSpeaker ? 77 : sp, state.feedIdx),
      speaker: mintedSpeaker ? 77 : overlap > 0.1 ? null : sp,
      text,
      t_ms: now,
      t_ns: String(now) + '000000',
      dur_ms: 1600 + ((state.feedIdx * 911) % 3800),
      overlap_frac: overlap,
      match_score: overlap > 0.1 ? null : score,
      // The cross-check runs on live rows too, and disagrees on some of them.
      asr_confidence: state.feedIdx % 5 === 2 ? 'shaky' : 'solid',
      text_via: 'live',
      lang: sp === 1 ? 'de' : 'en',
      thread: liveThread(),
    };
    // Additive and often absent, exactly as it is on the wire: only the two
    // canned lines in TRANSLATED carry one, and only while the speaker they
    // belong to is the one talking.
    const tr = TRANSLATED.get(line);
    if (tr && seg.speaker === sp) seg.translation = { ...tr };
    state.segments.push(seg);
    state.queue = state.feedIdx % 4;
    emit('segments', 'segment', seg);
  }
  /// One turn off the user's own microphone. Note what is NOT here: a
  /// match_score. There was no comparison, so there is no score to report, and
  /// a client that renders one would be inventing it.
  /**
   * 0.11.0 — one turn arriving the way a turn really arrives: provisionally,
   * three times, and then for real.
   *
   * The three partials carry a growing prefix of the same sentence and the
   * SAME `t_start_ns`, because `(session, t_start_ns)` is the whole replace
   * rule and a mock that varied it would let a client pass while never
   * implementing it. The final `segment` carries that identical `t_start_ns`.
   *
   * Deliberately NOT from the microphone: a partial's speaker is a proximity
   * guess or nothing (PROTOCOL 0.11.0), and the interesting row to look at is
   * the one wearing a hedged name rather than the user's own pinned one.
   */
  function emitPartialTurn() {
    const now = Date.now();
    const tStartNs = String(now) + '000000';
    const you = youSpeaker();
    const speaker = state.speakers.find((s) => s.name && s.id !== you)?.id ?? null;
    const words = PARTIAL_LINE.split(' ');
    // Where the three provisional readings stop. The first is deliberately
    // SHORT and the last is deliberately not quite the final text: partials are
    // wrong at first and a client that renders them as settled is the failure
    // this feature has to avoid (FINDINGS §12).
    const cuts = [3, 6, words.length - 1];
    cuts.forEach((cut, i) => {
      setTimeout(() => {
        emit('segments', 'partial', {
          session: SESSIONS[2].id,
          source: 'VRChat.exe',
          speaker,
          speaker_hint: speaker == null ? null : 'proximity',
          t_start_ms: now,
          t_start_ns: tStartNs,
          elapsed_ms: 900 + i * 1000,
          text: words.slice(0, cut).join(' '),
          seq_in_turn: i,
          final: false,
        });
      }, i * 700);
    });
    // …and the turn itself, after the last one. Same start, so a client
    // replaces rather than appends.
    setTimeout(() => {
      const seg = {
        id: state.nextSegId++,
        session: SESSIONS[2].id,
        source: 'VRChat.exe',
        speaker,
        text: PARTIAL_LINE,
        t_ms: now,
        t_ns: tStartNs,
        t_start_ns: tStartNs,
        t_end_ns: String(now + 4200) + '000000',
        dur_ms: 4200,
        overlap_frac: 0.03,
        match_score: 0.71,
        label_via: null,
        lang: 'en',
        lang_via: 'classified',
        asr_confidence: 'solid',
        text_via: 'live',
        thread: liveThread(),
      };
      state.segments.push(seg);
      emit('segments', 'segment', seg);
    }, cuts.length * 700 + 400);
    return { partials: cuts.length, t_start_ns: tStartNs, speaker };
  }

  function emitMine() {
    const now = Date.now();
    const text = MY_LINES[state.myLineIdx % MY_LINES.length];
    state.myLineIdx += 1;
    const seg = {
      id: state.nextSegId++,
      session: SESSIONS[2].id,
      source: 'mic',
      speaker: youSpeaker(),
      text,
      t_ms: now,
      t_ns: String(now) + '000000',
      dur_ms: 2200 + ((state.myLineIdx * 617) % 3400),
      overlap_frac: 0.02,
      match_score: null,
      asr_confidence: 'solid',
      text_via: 'live',
      thread: liveThread(),
    };
    state.segments.push(seg);
    emit('segments', 'segment', seg);
  }

  /// The conversation the live feed is currently in. It rolls over every few
  /// turns so a running GUI sees a thread boundary appear rather than only
  /// finding old ones in the history.
  function liveThread() {
    return 600 + Math.floor(state.feedIdx / 6);
  }

  function startFeed() {
    if (feedTimer) return;
    feedTimer = setInterval(tick, feedMs);
    if (feedTimer.unref) feedTimer.unref();
  }
  function stopFeed() {
    if (feedTimer) clearInterval(feedTimer);
    feedTimer = null;
  }

  // --- async ops ----------------------------------------------------------
  // PROTOCOL "Async operations": return an op handle immediately, then report
  // progress on the event stream while other requests keep flowing.
  function runOp(kind, steps, onDone) {
    const op = `op_${state.nextOp++}`;
    let i = 0;
    const iv = setInterval(() => {
      i += 1;
      if (i >= steps) {
        clearInterval(iv);
        let result = {};
        try {
          result = onDone() ?? {};
        } catch (e) {
          emit('ops', 'op.failed', { op, kind, msg: e.message });
          return;
        }
        emit('ops', 'op.done', { op, kind, ...result });
        return;
      }
      emit('ops', 'op.progress', { op, kind, done: i, total: steps, frac: i / steps });
    }, 350);
    if (iv.unref) iv.unref();
    return op;
  }

  // --- methods ------------------------------------------------------------

  const methods = {
    subscribe(params, client) {
      const topics = Array.isArray(params?.topics) ? params.topics : [];
      client.topics = new Set(topics);
      return { topics: [...client.topics] };
    },

    'events.since'(params) {
      const since = Number(params?.seq);
      if (!Number.isFinite(since)) throw err('bad_params', 'seq required');
      const oldest = state.replay.length ? state.replay[0].frame.seq : state.seq + 1;
      if (since + 1 < oldest) throw err('resync', 'requested sequence has fallen out of the replay buffer');
      return { events: state.replay.filter((r) => r.frame.seq > since).map((r) => r.frame) };
    },

    'sources.list': () => ({ sources: state.sources }),

    'sources.set'(params) {
      // The microphone is a source row and deliberately not an allowlist rule:
      // an app rule is consent about one program's output, and this device
      // hears the room. The refusal names the method that works.
      if (params?.match_key === 'mic') {
        throw err(
          'refused',
          'the microphone is not an application rule — use mic.set {enabled, mode}; it hears the room rather than one program, so it has its own switch and its own default (off)'
        );
      }
      const s = state.sources.find((x) => x.match_key === params?.match_key);
      if (!s) throw err('not_found', `no source ${params?.match_key}`);
      s.allowed = !!params?.allowed;
      s.streams = s.allowed ? 1 : 0;
      emit('sources', 'source', { match_key: s.match_key, allowed: s.allowed });
      // Allowing or denying the last app is a follow-mode transition, and the
      // `mic` event is the only way a client can see one.
      emit('status', 'mic', micPayload());
      emit('status', 'status', statusPayload());
      return { match_key: s.match_key, allowed: s.allowed };
    },

    'mic.get': () => ({ ...micPayload(), you_speaker: youSpeaker() }),

    'mic.set'(params) {
      const enabled = params?.enabled;
      const mode = params?.mode;
      if (enabled === undefined && mode === undefined) {
        throw err('bad_params', 'mic.set needs at least one of enabled, mode');
      }
      if (mode !== undefined && mode !== 'follow' && mode !== 'always') {
        throw err('bad_params', `mode must be "follow" or "always", not ${JSON.stringify(mode)}`);
      }
      if (enabled !== undefined) state.mic.enabled = !!enabled;
      if (mode !== undefined) state.mic.mode = mode;
      // Mirrored onto the row, so `sources.list` and `mic.get` agree.
      const row = state.sources.find((s) => s.kind === 'mic');
      if (row) {
        row.allowed = state.mic.enabled;
        row.streams = micActive() ? 1 : 0;
      }
      emit('status', 'mic', micPayload());
      emit('status', 'status', statusPayload());
      return { ...micPayload(), persisted: true };
    },

    // ---- 0.10.0: the room microphone -------------------------------------

    'room.get': () => roomPayload(),

    'room.set'(params) {
      const { enabled, mode } = params ?? {};
      const hasDevice = params && Object.prototype.hasOwnProperty.call(params, 'device');
      if (enabled === undefined && mode === undefined && !hasDevice) {
        throw err('bad_params', 'room.set needs at least one of enabled, mode, device');
      }
      if (mode !== undefined && mode !== 'follow' && mode !== 'always') {
        throw err('bad_params', `mode must be "follow" or "always", not ${JSON.stringify(mode)}`);
      }
      const after = {
        enabled: enabled === undefined ? state.room.enabled : !!enabled,
        mode: mode === undefined ? state.room.mode : mode,
        device: hasDevice ? (params.device ? String(params.device).trim() || null : null) : state.room.device,
      };
      // The refusal that is the whole shape of the feature: there is no
      // sensible default second input, and following the system default would
      // open the headset the microphone switch is already on.
      if (after.enabled && !after.device) {
        throw err(
          'bad_params',
          'the room microphone needs a device: there is no sensible default for a second input, and following the system default would open the headset the microphone switch is already on. Call devices.list and pass one of its node_name values'
        );
      }
      state.room = after;
      const row = state.sources.find((s) => s.kind === 'room');
      if (row) {
        row.allowed = state.room.enabled;
        row.streams = roomActive() ? 1 : 0;
      }
      emit('status', 'room', roomPayload());
      emit('status', 'status', statusPayload());
      return { ...roomPayload(), persisted: true };
    },

    'devices.list': () => ({ devices: state.devices }),

    // ---- 0.9.0's Discord bridge, as the Sources card reads it -------------

    'truth.status': () => ({
      enabled: state.truth.enabled,
      listening: state.truth.enabled ? state.truth.listening : null,
      port: 7797,
      label: 'nx-recall',
      enrol: false,
      sources: ['discord', 'vesktop'],
      token_path: '/home/you/.config/nx-recall/truth.token',
      spans: 4821,
      open_spans: 1,
      last_span_ms: state.truth.enabled ? state.truth.last_event_ms : null,
      users: state.truthUsers.length,
      linked: state.truthUsers.filter((u) => u.speaker != null).length,
      counters: { speaking: 4821, voice: 96, rejected: 0 },
    }),

    'truth.users': () => ({ users: state.truthUsers.map((u) => ({ ...u })) }),

    'truth.summary': () => ({
      segments_labelled: 612,
      single: 391,
      overlap: 74,
      partial: 88,
      nobody: 41,
      unknown: 18,
      min_duration_ms: 1000,
      identity: { n: 214, correct: 187, wrong: 9, unlabelled: 18, precision: 187 / 196, recall: 187 / 214, by_speaker: [] },
      overlap_gate: { threshold: 0.25, flagged_when_overlap: 61, flagged_when_single: 12, precision: 61 / 73, recall: 61 / 74 },
      caveat: 'Identity is scored on single-speaker turns of at least 1 s belonging to a linked account, and on nothing else.',
    }),

    'truth.link'(params) {
      const u = state.truthUsers.find((x) => x.user_id === String(params?.user_id));
      if (!u) throw err('not_found', `no Discord user ${params?.user_id}`);
      const sp = speakerById(Number(params?.speaker_id));
      if (!sp) throw err('not_found', `no speaker ${params?.speaker_id}`);
      u.speaker = sp.id;
      u.speaker_name = sp.name ?? sp.auto;
      u.via = 'manual';
      u.linked_ms = Date.now();
      // The same row shape the method returns, on the relabel topic.
      emit('relabel', 'truth', { ...u });
      return { ...u };
    },

    'truth.unlink'(params) {
      const u = state.truthUsers.find((x) => x.user_id === String(params?.user_id));
      if (!u) throw err('not_found', `no Discord user ${params?.user_id}`);
      u.speaker = null;
      u.speaker_name = null;
      u.via = null;
      u.linked_ms = null;
      emit('relabel', 'truth', { ...u });
      return { ...u };
    },

    'speakers.list': () => ({ speakers: speakerList() }),

    'speakers.name'(params) {
      const sp = speakerById(Number(params?.id));
      if (!sp) throw err('not_found', `no speaker ${params?.id}`);
      const name = String(params?.name ?? '').trim();
      sp.name = name || null;
      // Retroactive by construction: nothing is rewritten, the broadcast tells
      // every client to relabel in place (PROTOCOL "Events").
      emit('relabel', 'relabel', { speaker: sp.id, name: sp.name });
      return { id: sp.id, name: sp.name };
    },

    // schema v5 / PROTOCOL "Per-speaker languages". Only the two tags the
    // daemon's classifier knows are accepted: a tag it cannot check is a
    // correction it can never make.
    'speakers.set_languages'(params) {
      const sp = speakerById(Number(params?.id));
      if (!sp) throw err('not_found', `no speaker ${params?.id}`);
      const raw = params?.languages;
      const list = raw == null ? [] : Array.isArray(raw) ? raw : [raw];
      const out = [];
      for (const item of list) {
        if (typeof item !== 'string') throw err('bad_params', 'languages must be an array of strings');
        const code = item.trim().toLowerCase();
        if (!code || code === 'any') continue;
        if (code !== 'de' && code !== 'en' && code !== 'ja') {
          throw err('bad_params', `unknown language ${JSON.stringify(code)}; this daemon knows de, en, ja only`);
        }
        if (!out.includes(code)) out.push(code);
      }
      out.sort();
      sp.languages = out.length ? out : null;
      // On the existing relabel event, carrying the name too, so a client
      // folding it in never has to choose between the two facts.
      emit('relabel', 'relabel', { speaker: sp.id, name: sp.name, languages: sp.languages });
      return { id: sp.id, languages: sp.languages };
    },

    /**
     * The per-person highlight: a palette token and a small icon.
     *
     * The shape of the parameters is the whole contract and it is NOT the
     * shape `speakers.name` uses. Both keys are optional, and the three cases
     * are distinct: omitted means "leave this one alone", `null` means "clear
     * it", a value means "set it". That is why every branch below tests
     * `!== undefined` rather than truthiness — `{id, colour: null}` has to
     * clear the colour and leave the icon standing, and a mock that folded
     * with `??` would quietly make the clear button do nothing.
     *
     * Both fields ride out on the existing `relabel` event carrying the name
     * too, exactly as `set_languages` does, so a client folding one in never
     * has to choose between the facts.
     */
    'speakers.set'(params) {
      const id = Number(params?.id);
      const sp = state.speakers.find((s) => s.id === id);
      if (!sp) {
        // A tombstone is not a missing voice, and saying "no speaker 3" about
        // an id that still resolves would send a client looking for a bug it
        // does not have. Same answer `speakers.delete` gives.
        const canonical = state.tombstones.get(id);
        if (canonical != null) throw err('conflict', `speaker ${id} was merged into ${canonical}; highlight ${canonical} instead`);
        throw err('not_found', `no speaker ${params?.id}`);
      }

      // Saying nothing at all is not a way to clear a highlight — clearing is
      // an explicit null, and a request that mentions neither half is a client
      // bug the daemon would rather name than silently succeed at. `err:params`
      // is what recalld answers; a mock that shrugged here would let an e2e
      // pass against a request production refuses.
      if (params?.colour === undefined && params?.icon === undefined) {
        throw err('params', 'speakers.set needs colour, icon, or both (null clears one)');
      }

      if (params?.colour !== undefined) {
        const raw = params.colour;
        // `null` clears. An EMPTY STRING does not: it is not a token, and
        // recalld's `palette::check_colour` refuses it rather than guessing
        // that somebody meant "none". The icon field is the one where blank
        // means clear, because a text input is how an icon is typed and
        // emptying it is how a person says they want none — a colour is picked
        // from swatches and has a "none" swatch of its own.
        if (raw === null) {
          sp.colour = null;
        } else if (typeof raw !== 'string' || !accent(raw.trim())) {
          // The closed set is `palette::TOKENS` in the daemon. A token it
          // cannot resolve is not a colour it can paint, and accepting one
          // would store a highlight that renders as nothing for ever.
          throw err('params', `unknown colour ${JSON.stringify(raw)}; this daemon knows ${PALETTE.map((a) => a.token).join(', ')}`);
        } else {
          sp.colour = raw.trim();
        }
      }

      if (params?.icon !== undefined) {
        const raw = params.icon;
        if (raw === null) sp.icon = null;
        else if (typeof raw !== 'string') throw err('params', 'icon must be a string or null');
        else {
          const icon = raw.trim();
          // Blank after trimming IS a clear, not an error: the daemon stores
          // NULL for it, so a client that sends back an emptied text field
          // gets the obvious behaviour rather than a rejection.
          if (!icon) sp.icon = null;
          // Whitespace and control characters only. Deliberately NOT the whole
          // `\p{C}` class: U+200D ZERO WIDTH JOINER is `Cf`, and banning it
          // would ban every joined emoji — a family or a woman astronaut is
          // one grapheme cluster and a perfectly ordinary thing to pick.
          else if (WS_OR_CONTROL.test(icon)) {
            throw err('params', 'an icon may not contain whitespace or control characters');
          }
          // Two grapheme clusters, not two code units: a joined emoji is one
          // cluster and seven UTF-16 units, so a check that counted .length
          // would refuse an ordinary pick while happily accepting four flags.
          else if (graphemes(icon) > MAX_ICON_GRAPHEMES) {
            throw err('params', `icon is at most ${MAX_ICON_GRAPHEMES} characters, not ${graphemes(icon)}`);
          } else sp.icon = icon;
        }
      }

      emit('relabel', 'relabel', { speaker: sp.id, name: sp.name, colour: sp.colour ?? null, icon: sp.icon ?? null });
      return { id: sp.id, colour: sp.colour ?? null, icon: sp.icon ?? null };
    },

    /// The tokens this daemon will accept, with the hue and the canonical hex
    /// behind each. A READ, and the reason the picker has no list of its own:
    /// a daemon that grows an eleventh colour grows an eleventh swatch, and a
    /// GUI older than the daemon simply never offers the one it cannot render.
    'speakers.palette': () => ({ palette: PALETTE.map((a) => ({ ...a })) }),

    // 0.6.1: the one-off voices sweep. Lists by default; `apply` deletes.
    'speakers.prune'(params) {
      const voices = pruneCandidates();
      if (!params?.apply) {
        return {
          apply: false,
          count: voices.length,
          voices,
          max_segments: PRUNE_MAX_SEGMENTS,
          max_speech_ms: PRUNE_MAX_SPEECH_MS,
        };
      }
      const ids = new Set(voices.map((v) => v.id));
      const removedSegments = state.segments.filter((s) => ids.has(s.speaker)).map((s) => s.id);
      state.segments = state.segments.filter((s) => !ids.has(s.speaker));
      state.speakers = state.speakers.filter((s) => !ids.has(s.id));
      if (removedSegments.length) emit('segments', 'purge', { ids: removedSegments });
      for (const id of ids) emit('relabel', 'relabel', { speaker: id, name: null, pruned: true });
      emit('status', 'status', statusPayload());
      return { apply: true, count: ids.size, removed: [...ids], segments: removedSegments.length, voices };
    },

    // 0.6.4: delete one voice, with DESIGN §8's choice as a parameter. Unlike
    // `delete.run` this is scoped by the VOICE, so a voice with no segments
    // left is a normal input rather than a no-op — that is the whole bug.
    'speakers.delete'(params) {
      const id = Number(params?.id);
      const sp = state.speakers.find((s) => s.id === id);
      if (!sp) {
        const canonical = state.tombstones.get(id);
        if (canonical != null) throw err('conflict', `speaker ${id} was merged into ${canonical}; delete ${canonical} instead`);
        throw err('not_found', `no speaker ${id}`);
      }
      const keep = params?.keep_voiceprint === true;
      if (id === youSpeaker()) {
        throw err(
          'refused',
          'that is your own voice, pinned by your microphone rather than matched. Deleting it would not stop you being recorded — turn the microphone off instead'
        );
      }
      const merged = [...state.tombstones.entries()].filter(([, into]) => into === id);
      if (!keep && merged.length) {
        throw err(
          'refused',
          `speaker ${id} is a merge target: ${merged.length} other voice(s) point at it. Delete with keep_voiceprint: true, or split it apart first`
        );
      }
      const removed = state.segments.filter((s) => owner(s) === id).map((s) => s.id);
      state.segments = state.segments.filter((s) => owner(s) !== id);
      if (removed.length) emit('segments', 'purge', { ids: removed });
      if (keep) {
        emit('relabel', 'relabel', { speaker: id, name: sp.name, languages: sp.languages ?? null });
      } else {
        state.speakers = state.speakers.filter((s) => s.id !== id);
        // Not a merge: nothing moved anywhere, the id stops existing.
        emit('relabel', 'relabel', { speaker: id, name: null, pruned: true });
      }
      emit('status', 'status', statusPayload());
      return {
        id,
        name: sp.name,
        keep_voiceprint: keep,
        removed_speaker: !keep,
        segments: removed.length,
        msg: keep
          ? `${removed.length} conversation(s) deleted. The voiceprint was kept: this voice stays in the bank and will still be labelled going forward.`
          : `${removed.length} conversation(s) deleted and the voiceprint removed — this voice has to enrol again from scratch before it is recognised.`,
      };
    },

    'speakers.merge'(params) {
      const from = Number(params?.from);
      const into = Number(params?.into);
      const a = speakerById(from);
      const b = speakerById(into);
      if (!a || !b) throw err('not_found', 'unknown speaker');
      if (a.id === b.id) throw err('bad_params', 'cannot merge a speaker into itself');
      // Tombstones never chain (DESIGN §6): re-point everything that pointed at a.
      for (const [k, v] of state.tombstones) if (v === a.id) state.tombstones.set(k, b.id);
      state.tombstones.set(a.id, b.id);
      state.speakers = state.speakers.filter((s) => s.id !== a.id);
      for (const seg of state.segments) if (seg.speaker === a.id) seg.speaker = b.id;
      emit('relabel', 'relabel', { speaker: a.id, merged_into: b.id, name: b.name });
      emit('relabel', 'relabel', { speaker: b.id, name: b.name });
      return { from: a.id, into: b.id };
    },

    // PROTOCOL "speakers.split": the work happens INLINE and the reply carries
    // the outcome, not just a handle. This used to answer `{op}` and grind
    // through runOp, which is neither what recalld does nor a shape any client
    // could be written against — and it is why the GUI shipped ignoring
    // `resync` (audit finding #17). Mirrors crates/recalld/src/service.rs:
    // two relabels, the per-row `segment` events UNLESS there are more of them
    // than SPLIT_EVENT_CAP, and `resync` saying which of the two happened.
    'speakers.split'(params) {
      const sp = speakerById(Number(params?.id));
      if (!sp) throw err('not_found', `no speaker ${params?.id}`);
      const fresh = {
        id: Math.max(...state.speakers.map((s) => s.id)) + 1,
        name: null,
        auto: `Speaker_${String(50 + (state.nextOp % 40)).padStart(2, '0')}`,
        first_seen: new Date().toISOString(),
        segments: 0,
        total_ms: 0,
      };
      state.speakers.push(fresh);
      // Every second row of the source voice goes to the new identity — enough
      // that a busy voice lands past the cap and a one-line voice does not, so
      // both branches are reachable from a test.
      const changed = [];
      let n = 0;
      for (const seg of state.segments) {
        if (seg.speaker === sp.id && n++ % 2 === 0) {
          seg.speaker = fresh.id;
          changed.push(seg);
        }
      }
      const seq = emit('relabel', 'relabel', { speaker: sp.id, name: sp.name ?? null }).seq;
      emit('relabel', 'relabel', { speaker: fresh.id, name: null, auto: fresh.auto, split_from: sp.id });
      const resync = changed.length > SPLIT_EVENT_CAP;
      if (!resync) for (const seg of changed) emit('segments', 'segment', seg);

      const op = `op_${state.nextOp++}`;
      const result = {
        op,
        kept: sp.id,
        minted: fresh.id,
        auto: fresh.auto,
        moved_segments: changed.length,
        moved_prototypes: 1,
        ambiguous: 0,
        centroid_similarity: 0.42,
        embed_model_id: 'mock-embed/1',
        // True when the row-level events were suppressed: re-run your queries
        // rather than trusting what you have.
        resync,
        seq,
      };
      emit('ops', 'op.done', { ...result, kind: 'speakers.split' });
      return result;
    },

    // PROTOCOL "Voice preview": the clips worth hearing when naming a voice,
    // longest-first then best-matched, and only ones that still have audio.
    'speakers.sample'(params) {
      const sp = speakerById(Number(params?.id));
      if (!sp) throw err('not_found', `no speaker ${params?.id}`);
      const limit = Math.min(20, Math.max(1, Number(params?.limit ?? 3)));
      // Retention took this one's audio: the voice is real, the clips are not.
      if (sp.id === NO_AUDIO_SPEAKER) return { id: sp.id, samples: [] };
      const rows = state.segments
        .filter((s) => s.speaker != null && (state.tombstones.get(s.speaker) ?? s.speaker) === sp.id)
        .sort(
          (a, b) =>
            Math.floor(b.dur_ms / 1000) - Math.floor(a.dur_ms / 1000) ||
            (b.match_score ?? -1) - (a.match_score ?? -1)
        )
        .slice(0, limit);
      return {
        id: sp.id,
        samples: rows.map((s) => ({
          segment_id: s.id,
          t_ms: s.t_ms,
          t_ns: s.t_ns,
          duration_ms: s.dur_ms,
          text: s.text,
          match_score: s.match_score ?? null,
        })),
      };
    },

    // --- the memory graph, Tier 1 (0.6.2) ---------------------------------
    // Computed from the canned segments the same way the daemon computes it
    // from its own: an edge is a shared CONVERSATION, not a shared instance.

    'person.get'(params) {
      const sp = speakerById(Number(params?.id));
      if (!sp) throw err('not_found', `no speaker ${params?.id}`);
      const mine = state.segments.filter((s) => owner(s) === sp.id);
      const threads = new Set(mine.map((s) => s.thread).filter((t) => t != null));

      const edges = new Map();
      for (const seg of state.segments) {
        if (seg.thread == null || !threads.has(seg.thread)) continue;
        const who = owner(seg);
        if (who == null || who === sp.id) continue;
        const e = edges.get(who) ?? { threads: new Set(), ms: 0, last: 0 };
        e.threads.add(seg.thread);
        e.ms += seg.dur_ms;
        e.last = Math.max(e.last, seg.t_ms);
        edges.set(who, e);
      }

      const totals = {
        segments: mine.length,
        speech_ms: mine.reduce((n, s) => n + s.dur_ms, 0),
        sessions: new Set(mine.map((s) => s.session)).size,
        threads: threads.size,
        first_heard_ms: mine.length ? Math.min(...mine.map((s) => s.t_ms)) : null,
        last_heard_ms: mine.length ? Math.max(...mine.map((s) => s.t_ms)) : null,
      };
      totals.speech_ns = String(totals.speech_ms) + '000000';
      totals.first_heard_ns = totals.first_heard_ms == null ? null : String(totals.first_heard_ms) + '000000';
      totals.last_heard_ns = totals.last_heard_ms == null ? null : String(totals.last_heard_ms) + '000000';

      return {
        id: sp.id,
        speaker: {
          id: sp.id,
          you: sp.id === youSpeaker(),
          name: sp.name,
          auto: sp.auto,
          languages: sp.languages ?? null,
          colour: sp.colour ?? null,
          icon: sp.icon ?? null,
          first_seen: sp.first_seen,
        },
        languages: sp.languages ?? null,
        // 0.11.0: the same shape `speakers.list` carries, for the header chips.
        sources: speakerSources(sp.id),
        totals,
        edges: [...edges.entries()]
          .map(([id, e]) => ({
            speaker_id: id,
            ...person(id),
            threads: e.threads.size,
            seconds: e.ms / 1000,
            speech_ms: e.ms,
            last_ms: e.last,
            last_ns: String(e.last) + '000000',
            // Only voices the user has named can be linked to a roster line,
            // and null is the honest answer for everyone else.
            roster_seconds: person(id).name && sp.name ? Math.round(e.ms / 100) / 10 : null,
          }))
          .sort((a, b) => b.threads - a.threads || b.speech_ms - a.speech_ms),
        // 0.10.0 — where you meet. Grouped by world, most time first, and a
        // world with no name would render as its id (both fixtures have one).
        worlds: (() => {
          const acc = new Map();
          for (const t of threads) {
            const world = worldOfThread(t);
            if (!world) continue;
            const rows = state.segments.filter((x) => x.thread === t);
            if (!rows.length) continue;
            const start = Math.min(...rows.map((x) => x.t_ms));
            const end = Math.max(...rows.map((x) => x.t_ms + x.dur_ms));
            const w = acc.get(world) ?? { days: new Set(), ms: 0, last: 0 };
            w.days.add(new Date(start).toDateString());
            w.ms += end - start;
            w.last = Math.max(w.last, end);
            acc.set(world, w);
          }
          return [...acc.entries()]
            .map(([world_id, w]) => ({
              world_id,
              name: WORLDS.find((x) => x.world_id === world_id)?.name ?? null,
              visits: w.days.size,
              last_ms: w.last,
              last_ns: String(w.last) + '000000',
              minutes_together: w.ms / 60000,
              together_ms: w.ms,
            }))
            .sort((a, b) => b.together_ms - a.together_ms || b.last_ms - a.last_ms)
            .slice(0, 8);
        })(),
        recent_threads: [...threads]
          .sort((a, b) => b - a)
          .slice(0, 12)
          .map((t) => threadPayload(t)),
      };
    },

    // --- 0.10.0: worlds and turn-taking ------------------------------------

    'worlds.list'(params) {
      const limit = Math.min(500, Math.max(1, Number(params?.limit ?? 20)));
      const rows = WORLDS.map((w) => {
        const threads = [...new Set(state.segments.map((s) => s.thread).filter((t) => t != null))]
          .filter((t) => worldOfThread(t) === w.world_id);
        const segs = state.segments.filter((s) => threads.includes(s.thread));
        const spoke = new Map();
        for (const seg of segs) {
          const who = owner(seg);
          if (who == null) continue;
          spoke.set(who, (spoke.get(who) ?? 0) + seg.dur_ms);
        }
        const last = segs.length ? Math.max(...segs.map((s) => s.t_ms + s.dur_ms)) : 0;
        // A visit is an EVENING, not a conversation: several threads happen in
        // one instance, and a world claiming 151 visits from 151 threads would
        // be counting the wrong thing.
        const days = new Set(segs.map((x) => new Date(x.t_ms).toDateString()));
        return {
          world_id: w.world_id,
          name: w.name,
          visits: days.size,
          last_ms: last,
          last_ns: String(last) + '000000',
          people: [...spoke.entries()]
            .sort((a, b) => b[1] - a[1])
            .slice(0, 6)
            .map(([id]) => ({
              speaker_id: id,
              label: person(id).name ?? person(id).auto,
              // The chips in this card are a row of people, and a row of
              // people is where a highlight earns its keep — same fields, same
              // names, as everywhere else a person is serialised.
              colour: person(id).colour,
              icon: person(id).icon,
            })),
          // Tier 2 output: whatever enrichment happened to label these
          // conversations, and an empty list when it labelled none.
          topics: [...new Set(threads.map((t) => state.topics[t]).filter(Boolean))].slice(0, 5),
        };
      }).sort((a, b) => b.last_ms - a.last_ms);
      return { total: rows.length, worlds: rows.slice(0, limit) };
    },

    'person.stats'(params) {
      const sp = speakerById(Number(params?.id));
      if (!sp) throw err('not_found', `no speaker ${params?.id}`);
      const days = params?.days == null ? null : Number(params.days);
      if (days != null && !(days > 0)) throw err('params', 'days must be positive');
      const s = statsOf(sp.id, days);
      return {
        id: sp.id,
        days,
        from_ms: days == null ? null : Date.now() - days * DAY,
        ...s,
        // The caveats travel with the numbers, exactly as the daemon sends
        // them: a client that renders an approximation without its definition
        // is making a claim the daemon did not.
        definitions: {
          interruption:
            'a turn of theirs that starts while somebody else is still talking AND whose own audio holds at least 10% overlapped speech — the same line above which the daemon refuses to name a voice. The overlap says two people were audible, not which two; the clock supplies the name. It cannot tell an interruption from a back-channel.',
          latency:
            "the median gap from the previous speaker's turn ending to theirs starting, over gaps of 0 to 5000 ms. Longer gaps are dropped rather than clamped: past that it is a lull, not a reply.",
          share:
            'their speech time over the speech time of every identified voice in the same conversations',
        },
      };
    },

    'thread.get'(params) {
      const id = Number(params?.id);
      const rows = state.segments.filter((s) => s.thread === id);
      if (!rows.length) throw err('not_found', `no thread ${params?.id}`);
      return { ...threadPayload(id), segments: rows };
    },

    /**
     * `replay.get` — the thin query behind conversation replay (0.9.2).
     *
     * The point of it is `has_audio`, and the point of `has_audio` is that it
     * is NOT always true: a conversation is a mix of turns that still sound and
     * turns retention has taken, and a client that never meets the second kind
     * never renders it. Here that split falls out of the same rule
     * `segments.audio` follows — NO_AUDIO_SPEAKER answers `gone` — so the two
     * can never disagree, and thread 502 (canned rows 10..14) mixes both.
     */
    'replay.get'(params) {
      const id = Number(params?.thread);
      const rows = state.segments.filter((s) => s.thread === id);
      if (!rows.length) throw err('not_found', `no conversation with id ${params?.thread}`);
      return {
        thread: id,
        turns: rows.map((s) => {
          const who = owner(s);
          const p = who == null ? null : person(who);
          return {
            id: s.id,
            t_ms: s.t_ms,
            t_ns: s.t_ns,
            dur_ms: s.dur_ms,
            speaker: s.speaker,
            speaker_name: p ? (p.name ?? p.auto) : null,
            // Beside the resolved name, and resolved the same way: the player
            // draws its turn list from these three fields alone and never
            // consults `speakers.list`, so a highlight that is not here is a
            // highlight that vanishes the moment replay opens.
            speaker_colour: p ? p.colour : null,
            speaker_icon: p ? p.icon : null,
            text: s.text,
            has_audio: who !== NO_AUDIO_SPEAKER,
          };
        }),
      };
    },

    // --- the memory graph, Tiers 2 and 3 (0.7.0) --------------------------

    'graph.summary': () => ({
      counts: graphCounts(),
      enrichment: { ...state.enrichment },
      config: graphConfig(),
    }),

    'graph.get': () => ({ config: graphConfig(), enrichment: { ...state.enrichment } }),

    'graph.set'(params) {
      const { enabled, llm_threads: threads, gpu_layers: layers } = params ?? {};
      if (enabled === undefined && threads === undefined && layers === undefined) {
        throw err('params', 'graph.set needs at least one of enabled, llm_threads, gpu_layers');
      }
      if (enabled !== undefined) setEnrichment(!!enabled);
      if (threads !== undefined) {
        state.graph.llm_threads = Math.max(GRAPH_THREADS[0], Math.min(GRAPH_THREADS[1], Number(threads)));
      }
      if (layers !== undefined) state.graph.gpu_layers = Math.max(0, Number(layers));
      emit('status', 'graph', { ...state.enrichment });
      emit('status', 'status', statusPayload());
      return { config: { ...graphConfig(), persisted: true }, enrichment: { ...state.enrichment } };
    },

    'graph.enrich'(params) {
      const action = String(params?.action ?? 'start').toLowerCase();
      if (action !== 'start' && action !== 'stop') {
        throw err('params', `action must be "start" or "stop", not ${JSON.stringify(params?.action)}`);
      }
      const want = action === 'start';
      if (want === state.graph.enabled) {
        return { config: graphConfig(), enrichment: { ...state.enrichment }, changed: false };
      }
      setEnrichment(want);
      emit('status', 'graph', { ...state.enrichment });
      emit('status', 'status', statusPayload());
      return { config: { ...graphConfig(), persisted: true }, enrichment: { ...state.enrichment } };
    },

    'commitments.list'(params) {
      const want = params?.state;
      if (want != null && !COMMITMENT_STATES.includes(want)) {
        throw err('params', `state must be one of ${JSON.stringify(COMMITMENT_STATES)}, not ${JSON.stringify(want)}`);
      }
      const rows = state.commitments
        .filter((c) => want == null || c.state === want)
        // Undated last, never first: a promise with no date is not overdue, it
        // is merely open, and sorting it above a real deadline would lie.
        .sort((a, b) => (a.due_ms == null) - (b.due_ms == null) || (a.due_ms ?? 0) - (b.due_ms ?? 0));
      return { state: want ?? null, commitments: rows.map(commitmentPayload) };
    },

    'commitments.set_state'(params) {
      const id = Number(params?.id);
      const next = params?.state;
      if (!COMMITMENT_STATES.includes(next)) {
        throw err('params', `state must be one of ${JSON.stringify(COMMITMENT_STATES)}, not ${JSON.stringify(next)}`);
      }
      const row = state.commitments.find((c) => c.id === id);
      if (!row) throw err('not_found', `no commitment with id ${params?.id}`);
      row.state = next;
      row.updated_at = Date.now();
      const payload = commitmentPayload(row);
      // Broadcast, like every other retroactive change.
      emit('ops', 'commitment', payload);
      return payload;
    },

    'topics.list'(params) {
      const perTopic = Math.min(100, Math.max(1, Number(params?.per_topic ?? 12)));
      const groups = new Map();
      for (const [threadId, topic] of Object.entries(state.topics)) {
        const rows = state.segments.filter((s) => s.thread === Number(threadId));
        if (!rows.length) continue;
        const g = groups.get(topic) ?? { topic, threads: [], segments: 0, last: 0 };
        g.threads.push(Number(threadId));
        g.segments += rows.length;
        g.last = Math.max(g.last, ...rows.map((s) => s.t_ms + s.dur_ms));
        groups.set(topic, g);
      }
      return {
        topics: [...groups.values()]
          .sort((a, b) => b.last - a.last)
          .map((g) => ({
            topic: g.topic,
            threads: g.threads.length,
            segments: g.segments,
            last_ms: g.last,
            last_ns: String(g.last) + '000000',
            thread_ids: g.threads.sort((a, b) => b - a).slice(0, perTopic),
          })),
      };
    },

    'segments.audio'(params) {
      const seg = state.segments.find((s) => s.id === Number(params?.id));
      if (!seg) throw err('not_found', `no segment ${params?.id}`);
      const owner = seg.speaker == null ? null : (state.tombstones.get(seg.speaker) ?? seg.speaker);
      if (owner === NO_AUDIO_SPEAKER) {
        throw err(
          'gone',
          `segment ${seg.id} still has its text, but not its audio — the audio retention window expired`
        );
      }
      const wav = toneFor(owner);
      return {
        id: seg.id,
        wav_b64: wav.toString('base64'),
        duration_ms: TONE_MS,
        sample_rate: TONE_RATE,
        bytes: wav.length,
      };
    },

    'segments.reassign'(params) {
      const seg = state.segments.find((s) => s.id === Number(params?.segment_id));
      if (!seg) throw err('not_found', `no segment ${params?.segment_id}`);
      const target = params?.speaker_id == null ? null : speakerById(Number(params.speaker_id));
      if (params?.speaker_id != null && !target) throw err('not_found', 'unknown speaker');
      seg.speaker = target ? target.id : null;
      emit('segments', 'segment', seg);
      return { segment_id: seg.id, speaker: seg.speaker };
    },

    'segments.correct'(params) {
      const seg = state.segments.find((s) => s.id === Number(params?.segment_id));
      if (!seg) throw err('not_found', `no segment ${params?.segment_id}`);
      const before = seg.text;
      seg.text = String(params?.text ?? '');
      seg.corrected = true;
      // 0.8.0: a correction is the daemon's only ground truth about how wrong
      // it was, so it is kept rather than merely applied — `prior_state` on the
      // real daemon, one row in this list here. It is what `accuracy.summary`
      // is computed from and what feeds the corrections vocabulary group.
      if (before !== seg.text) recordCorrection(seg, before, seg.text);
      emit('segments', 'segment', seg);
      return { segment_id: seg.id, text: seg.text, prior_state: { text: before } };
    },

    // --- 0.8.0: vocabulary -------------------------------------------------

    'vocab.get': () => vocabPayload(),

    'vocab.set'(params) {
      const raw = params?.terms;
      if (!Array.isArray(raw)) throw err('bad_params', 'vocab.set needs terms: an array of strings');
      const out = [];
      const seen = new Set();
      for (const item of raw) {
        if (typeof item !== 'string') throw err('bad_params', 'every term must be a string');
        const term = item.trim();
        if (!term) continue;
        if (term.length > 64) throw err('bad_params', `"${term.slice(0, 20)}…" is longer than a hotword can be (64 characters)`);
        if (seen.has(term.toLowerCase())) continue;
        seen.add(term.toLowerCase());
        out.push(term);
      }
      // REPLACES the user glossary — the method is not "add" (PROTOCOL), so a
      // client that sends a shorter list has removed something and meant to.
      state.vocab.user = out;
      const payload = vocabPayload();
      // On `status`, the topic every client already subscribes to, for exactly
      // the reason the `mic` event is there: a new topic would make an older
      // client deaf to it and there is nothing here that wants its own stream.
      emit('status', 'vocab', payload);
      return { ...payload, persisted: true };
    },

    // --- 0.10.2: the translation controls ----------------------------------

    'assist.get': () => assistState(),

    'assist.set'(params) {
      const next = { ...state.assist };
      if (params?.translate_to !== undefined) {
        const code = String(params.translate_to ?? '').trim().toLowerCase();
        if (code && !LANGUAGES.some((l) => l.code === code)) {
          throw err('params', `no language ${JSON.stringify(code)}; translate_to is one of ${LANGUAGES.map((l) => l.code).join(', ')} or "" for off`);
        }
        next.translate_to = code;
      }
      if (params?.read_languages !== undefined) {
        if (!Array.isArray(params.read_languages)) throw err('params', 'read_languages must be an array of strings');
        const out = [];
        for (const item of params.read_languages) {
          if (typeof item !== 'string') throw err('params', 'read_languages must be an array of strings');
          const code = item.trim().toLowerCase();
          if (!LANGUAGES.some((l) => l.code === code)) {
            throw err('params', `no language ${JSON.stringify(code)}; read_languages is any of ${LANGUAGES.map((l) => l.code).join(', ')}`);
          }
          if (!out.includes(code)) out.push(code);
        }
        next.read_languages = out;
      }
      if (params?.translation_display !== undefined) {
        const mode = String(params.translation_display ?? '').trim().toLowerCase();
        if (mode !== 'main' && mode !== 'under') throw err('params', 'translation_display is "main" or "under"');
        next.translation_display = mode;
      }
      if (params?.translate_to === undefined && params?.read_languages === undefined && params?.translation_display === undefined) {
        throw err('params', 'assist.set needs at least one of translate_to, read_languages, translation_display');
      }
      state.assist = next;
      const payload = assistState();
      // On `status`, like `vocab` and `mic`: the display mode changes what a
      // transcript ROW looks like, so every open window has to be told.
      emit('status', 'assist', payload);
      return { ...payload, persisted: true };
    },

    // --- 0.8.0: accuracy ---------------------------------------------------

    'accuracy.summary': () => accuracyPayload(),

    // --- 0.8.0: notes to self ----------------------------------------------

    'notes.list'(params) {
      const want = params?.state;
      if (want != null && !NOTE_STATES.includes(want)) {
        throw err('params', `state must be one of ${JSON.stringify(NOTE_STATES)}, not ${JSON.stringify(want)}`);
      }
      const limit = Math.min(500, Math.max(1, Number(params?.limit ?? 100)));
      const rows = state.notes
        .filter((n) => want == null || n.state === want)
        // Newest first: a note is a thing you left for yourself a moment ago.
        .sort((a, b) => b.t_ms - a.t_ms)
        .slice(0, limit);
      return { notes: rows.map(notePayload) };
    },

    'notes.set_state'(params) {
      const id = Number(params?.id);
      const next = params?.state;
      if (!NOTE_STATES.includes(next)) {
        throw err('params', `state must be one of ${JSON.stringify(NOTE_STATES)}, not ${JSON.stringify(next)}`);
      }
      // 0.9.0: "not now, in ten minutes". The same method, because a snooze IS
      // a state change — the note goes back to open and its date moves.
      const snooze = params?.snooze_min;
      if (snooze != null) {
        if (next !== 'open') {
          throw err(
            'params',
            'snooze_min only makes sense with state "open" — a note that is done or dismissed is not waiting to come back'
          );
        }
        const mins = Number(snooze);
        if (!Number.isFinite(mins) || mins < 1 || mins > 7 * 24 * 60) {
          throw err('params', 'snooze_min must be between 1 and 10080 minutes');
        }
        const row = state.notes.find((n) => n.id === id);
        if (!row) throw err('not_found', `no note with id ${params?.id}`);
        row.state = 'open';
        // A snooze on a note with no date GIVES it one, which is the only way
        // to ask to be reminded of something you said without a time in it.
        row.due_ms = Date.now() + mins * 60_000;
        row.fired = false;
        row.fired_ms = null;
        const out = notePayload(row);
        emit('segments', 'note', out);
        return out;
      }
      const row = state.notes.find((n) => n.id === id);
      if (!row) throw err('not_found', `no note with id ${params?.id}`);
      row.state = next;
      return notePayload(row);
    },

    // --- 0.9.0: the daily digest -------------------------------------------

    'digest.list'(params) {
      const day = params?.day;
      if (day != null && !/^\d{4}-\d{2}-\d{2}$/.test(String(day))) {
        throw err(
          'params',
          `day must be a local calendar day like "2026-09-02", not ${JSON.stringify(day)}`
        );
      }
      const limit = Math.min(500, Math.max(1, Number(params?.limit ?? 50)));
      const rows = state.digests
        .map(digestPayload)
        .filter((d) => day == null || d.day === day)
        .sort((a, b) => b.started_ms - a.started_ms)
        .slice(0, limit);
      return { day: day ?? null, total: rows.length, digests: rows };
    },

    // --- 0.8.0: one query box ----------------------------------------------

    'search.ask'(params) {
      const q = String(params?.q ?? '').trim();
      if (!q) throw err('params', 'q must not be empty');
      const limit = Number(params?.limit ?? 50);
      const interpretation = interpret(q);

      const facets = { limit };
      if (interpretation.speaker_id != null) facets.speaker = interpretation.speaker_id;
      if (interpretation.from_ns) facets.from = Number(interpretation.from_ns) / 1e6;
      if (interpretation.to_ns) facets.to = Number(interpretation.to_ns) / 1e6;
      if (interpretation.world_id) facets.world = interpretation.world_id;

      // A question with no words left in it is a browse, not a search: the
      // facets are the whole query and the keyword leg has nothing to match on.
      if (!interpretation.query) {
        const rows = transcriptPage(state.segments, { ...facets, limit: Math.min(limit, 200) });
        return { interpretation, total: rows.length, hits: [...rows].reverse(), q };
      }
      const res =
        interpretation.mode === 'hybrid'
          ? methods['search.semantic']({ ...facets, q: interpretation.query, mode: 'hybrid' })
          : methods.search({ ...facets, q: interpretation.query });
      return { ...res, interpretation, q };
    },

    // --- 0.11.0: grounded answers -------------------------------------------

    // Two canned answers and one refusal. The mock has no model, so the
    // "answerable" decision is a lookup — but everything AROUND it is the real
    // contract: the same interpretation, the same hits, exactly one of `answer`
    // and `refused`, and citations that are ids the client was actually given.
    // A question the table does not know is refused, which is also the honest
    // default for an archive that mostly does not contain what you asked.
    'search.answer'(params) {
      const asked = methods['search.ask'](params);
      const hits = asked.hits ?? [];
      const q = String(params?.q ?? '').trim().toLowerCase();

      const canned = ANSWERS.find((a) => a.match.test(q));
      if (!canned || !hits.length) {
        return {
          ...asked,
          answer: null,
          refused: { reason: hits.length ? 'the transcript does not say' : 'there is nothing in the archive about that' },
        };
      }
      // Cite rows that are really on the page. A chip that points at a turn the
      // client does not have is a chip that scrolls nowhere, and that bug is
      // exactly what the mock exists to make impossible to ship.
      const citations = hits.slice(0, canned.cite).map((h) => h.id);
      return {
        ...asked,
        answer: {
          text: canned.text,
          lang: canned.lang,
          citations,
          via: 'qwen2.5-3b-instruct-q4_k_m',
          took_ms: 1180,
        },
        refused: null,
      };
    },

    // --- 0.8.0: briefs -----------------------------------------------------

    'person.brief'(params) {
      const sp = speakerById(Number(params?.id));
      if (!sp) throw err('not_found', `no speaker ${params?.id}`);
      const mine = state.segments.filter((s) => owner(s) === sp.id);
      const threads = new Set(mine.map((s) => s.thread).filter((t) => t != null));
      const you = youSpeaker();
      const open = (c) => c.state === 'candidate' || c.state === 'confirmed';
      return {
        speaker: {
          id: sp.id,
          you: sp.id === you,
          name: sp.name,
          auto: sp.auto,
          languages: sp.languages ?? null,
          colour: sp.colour ?? null,
          icon: sp.icon ?? null,
        },
        last_heard_ms: mine.length ? Math.max(...mine.map((s) => s.t_ms)) : null,
        // What THEY owe YOU, and what you owe them. Two lists rather than one
        // with a direction flag, because they are two different feelings and a
        // client renders them in two different sentences.
        open_to_you: state.commitments.filter((c) => open(c) && c.who === sp.id && (c.to === you || c.to == null)).map(commitmentPayload),
        open_from_you: state.commitments.filter((c) => open(c) && c.who === you && c.to === sp.id).map(commitmentPayload),
        recent_topics: [...threads]
          .map((t) => state.topics[t])
          .filter(Boolean)
          .filter((t, i, a) => a.indexOf(t) === i)
          .slice(0, 4),
        notes_mentioning: state.notes
          .filter((n) => sp.name && n.text.toLowerCase().includes(sp.name.toLowerCase()))
          .map(notePayload),
      };
    },

    search(params) {
      const q = String(params?.q ?? '').trim().toLowerCase();
      let rows = state.segments;
      if (q) rows = rows.filter((s) => s.text.toLowerCase().includes(q));
      if (params?.speaker != null) rows = rows.filter((s) => s.speaker === Number(params.speaker));
      if (params?.source) rows = rows.filter((s) => s.source === params.source);
      rows = withinWorld(rows, params?.world);
      // Inclusive `from`, exclusive `to`, ISO or number — the same time
      // semantics every filtered method on this socket uses.
      const from = mockTimeParam(params?.from);
      const to = mockTimeParam(params?.to);
      if (from != null) rows = rows.filter((s) => s.t_ms >= from);
      if (to != null) rows = rows.filter((s) => s.t_ms < to);
      const limit = Number(params?.limit ?? 50);
      const hits = rows.slice(-limit).reverse();
      return { hits, total: rows.length, q: params?.q ?? '' };
    },

    // 0.6.5. Not a real embedding, obviously: a deterministic stand-in whose
    // *shape* is right, so the view's mode toggle, `via` markers and empty
    // states are all exercised without 118 MB of weights in the repository.
    // Meaning is faked as "shares an uncommon word with the query, in either
    // language", plus a tiny hand-written German/English gloss so the
    // cross-language case — the whole point of the feature — is visible.
    'search.semantic'(params) {
      if (!state.semantic) {
        throw err(
          'unavailable',
          'semantic search is not installed. `recalld models fetch --semantic` installs multilingual-e5-small-int8 (128.9 MB), then `recalld semantic backfill` indexes what has already been said.'
        );
      }
      const q = String(params?.q ?? '').trim();
      if (!q) throw err('params', 'q must not be empty');
      const mode = params?.mode ?? 'semantic';
      if (mode !== 'semantic' && mode !== 'hybrid') {
        throw err('params', `mode must be "semantic" or "hybrid", not ${JSON.stringify(mode)}`);
      }
      const limit = Number(params?.limit ?? 50);

      let rows = state.segments;
      if (params?.speaker != null) rows = rows.filter((s) => s.speaker === Number(params.speaker));
      if (params?.source) rows = rows.filter((s) => s.source === params.source);
      rows = withinWorld(rows, params?.world);
      if (params?.from) rows = rows.filter((s) => s.t_ms >= Date.parse(params.from));
      if (params?.to) rows = rows.filter((s) => s.t_ms <= Date.parse(params.to));

      const terms = expand(q);
      const scored = rows
        .map((s) => {
          const hay = ` ${String(s.text ?? '').toLowerCase()} `;
          let n = 0;
          for (const t of terms) if (hay.includes(t)) n += 1;
          return { s, score: n ? Math.min(0.95, 0.62 + n * 0.09) : 0 };
        })
        .filter((x) => x.score > 0)
        .sort((a, b) => b.score - a.score || a.s.id - b.s.id)
        .slice(0, limit);

      const keyword =
        mode === 'hybrid'
          ? rows.filter((s) => String(s.text ?? '').toLowerCase().includes(q.toLowerCase())).slice(0, limit)
          : [];
      const fused = rrf(keyword.map((s) => s.id), scored.map((x) => x.s.id));
      const byId = new Map(state.segments.map((s) => [s.id, s]));
      const byScore = new Map(scored.map((x) => [x.s.id, x.score]));
      const hits = fused.slice(0, limit).map((f) => {
        const seg = byId.get(f.id);
        const out = { ...seg, via: f.via, rrf: f.score };
        if (byScore.has(f.id)) out.score = byScore.get(f.id);
        return out;
      });
      return {
        total: hits.length,
        q,
        mode,
        model: 'multilingual-e5-small-int8@1',
        took_ms: 12 + (q.length % 7),
        hits,
      };
    },

    transcript(params) {
      // 0.10.0: the world facet works here too, which is what makes "show me
      // everything said in The Great Pug" a browse rather than a search for
      // no words.
      return {
        segments: transcriptPage(withinWorld(state.segments, params?.world), params),
        sessions: SESSIONS,
      };
    },

    // ---- 0.10.0: the local Markdown export --------------------------------
    //
    // This mock really writes the files. A card that says "this writes files to
    // your disk and nothing else" is only testable if the files turn up, and a
    // preview that agreed with a run that wrote nothing would prove neither.

    'export.preview': (params) => exportPlan(params),

    'export.run'(params) {
      const plan = exportPlan(params);
      const blocked = plan.files.filter((f) => f.blocked);
      if (blocked.length) {
        throw err(
          'refused',
          `${blocked[0].name} already exists in ${plan.dir} and was not written by NX Recall — it carries no "${EXPORT_MARKER}" header, so overwriting it would destroy somebody's file. Move it, or export into an empty folder`
        );
      }
      let bytes = 0;
      const op = runOp('export.run', Math.max(2, plan.files.length), () => {
        for (const file of plan.files) {
          fs.writeFileSync(path.join(plan.dir, file.name), file.body);
          bytes += file.bytes;
        }
        return { files: plan.files.length, bytes, dir: plan.dir };
      });
      return { op, files: plan.files.length, dir: plan.dir };
    },

    'delete.preview'(params) {
      const rows = matchDelete(params);
      return {
        segments: rows.length,
        bytes: rows.reduce((n, s) => n + s.dur_ms * 32, 0),
        speakers: [...new Set(rows.map((s) => s.speaker).filter((x) => x != null))].length,
      };
    },

    'delete.run'(params) {
      const rows = matchDelete(params);
      const ids = new Set(rows.map((s) => s.id));
      const op = runOp('delete.run', 8, () => {
        state.segments = state.segments.filter((s) => !ids.has(s.id));
        emit('segments', 'purge', { ids: [...ids] });
        return { removed: ids.size };
      });
      return { op };
    },

    pause() {
      state.paused = true;
      stopFeed();
      emit('status', 'status', statusPayload());
      return { paused: true };
    },

    resume() {
      state.paused = false;
      startFeed();
      emit('status', 'status', statusPayload());
      return { paused: false };
    },

    status: () => statusPayload(),
  };

  /// The export's plan, rendered the way `crate::export` renders it: the same
  /// header, the same H2 per conversation, the same `- **HH:MM** Name: text`.
  /// Close enough that a golden read off this mock is a golden of the shape,
  /// which is the half the GUI is responsible for.
  function exportPlan(params) {
    const dir = String(params?.dir ?? '').trim();
    if (!dir) throw err('bad_params', 'dir is required');
    if (!path.isAbsolute(dir)) throw err('refused', `the export directory must be an absolute path; ${dir} is relative`);
    if (dir === '/run/user' || dir.startsWith('/run/user/') || dir.startsWith('/proc') || dir.startsWith('/sys')) {
      throw err('refused', `${dir} is not a place files survive — pick a folder in your home directory`);
    }
    if (!fs.existsSync(dir) || !fs.statSync(dir).isDirectory()) {
      throw err('refused', `${dir} does not exist. The export writes into a folder you already have`);
    }
    const from = params?.from ? Date.parse(params.from) : null;
    const to = params?.to ? Date.parse(params.to) : null;
    const rows = state.segments
      .filter((s) => (from == null || s.t_ms >= from) && (to == null || s.t_ms < to))
      .filter((s) => params?.speaker == null || s.speaker === Number(params.speaker))
      .filter((s) => params?.thread == null || s.thread === Number(params.thread))
      .slice()
      .sort((a, b) => a.t_ms - b.t_ms);

    const days = new Map();
    for (const row of rows) {
      const d = new Date(row.t_ms);
      const key = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
      if (!days.has(key)) days.set(key, []);
      days.get(key).push(row);
    }

    const files = [];
    for (const [day, turns] of days) {
      files.push(describeFile(dir, `${day}.md`, renderDay(day, turns, !!params?.include_translations), turns.length, new Set(turns.map((t) => t.thread ?? 0)).size));
    }
    if (files.length) {
      files.push(describeFile(dir, 'people.md', renderPeople(), 0, 0));
    }
    return {
      dir,
      days: files.filter((f) => f.name !== 'people.md').length,
      conversations: files.reduce((n, f) => n + f.conversations, 0),
      turns: files.reduce((n, f) => n + f.turns, 0),
      bytes: files.reduce((n, f) => n + f.bytes, 0),
      files,
      blocked: files.filter((f) => f.blocked).map((f) => f.name),
    };
  }

  function describeFile(dir, name, body, turns, conversations) {
    const full = path.join(dir, name);
    const exists = fs.existsSync(full);
    let blocked = false;
    if (exists) {
      try {
        blocked = !fs.readFileSync(full, 'utf8').slice(0, 1024).includes(EXPORT_MARKER);
      } catch {
        blocked = true;
      }
    }
    return { name, body, bytes: Buffer.byteLength(body), turns, conversations, exists, blocked };
  }

  function hhmm(ms) {
    const d = new Date(ms);
    return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
  }

  function renderDay(day, turns, translations) {
    let out = `${EXPORT_MARKER}\n# ${day}\n`;
    const groups = new Map();
    for (const t of turns) {
      const key = t.thread ?? null;
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(t);
    }
    let shaky = false;
    for (const [, rows] of groups) {
      const names = [...new Set(rows.map((r) => speakerLabelFor(r)))];
      out += `\n## ${hhmm(rows[0].t_ms)} — ${names.join(', ')}\n`;
      for (const r of rows) {
        const isShaky = r.asr_confidence === 'shaky';
        shaky ||= isShaky;
        const words = String(r.text ?? '').replace(/\s+/g, ' ').trim();
        const body = isShaky ? `_${words}_[^shaky]` : words;
        out += `- **${hhmm(r.t_ms)}** ${speakerLabelFor(r)}: ${body}\n`;
        if (translations && r.translation) out += `  > ${r.translation}\n`;
      }
    }
    if (shaky) {
      out += `\n[^shaky]: A second decoder read this turn differently, so the words are uncertain. The speaker is not in doubt; the transcript is.\n`;
    }
    return out;
  }

  function speakerLabelFor(row) {
    const sp = row.speaker == null ? null : speakerById(row.speaker);
    return sp ? (sp.name ?? sp.auto) : 'Unknown voice';
  }

  function renderPeople() {
    let out = `${EXPORT_MARKER}\n# People\n\nThe voices you have named. Anyone still unnamed is in the transcript but not here.\n\n`;
    for (const sp of state.speakers.filter((s) => s.name).sort((a, b) => a.name.localeCompare(b.name))) {
      const last = state.segments.filter((s) => s.speaker === sp.id).reduce((n, s) => Math.max(n, s.t_ms), 0);
      const languages = sp.languages?.length ? sp.languages.join(', ') : 'any language';
      const d = last ? new Date(last) : null;
      const heard = d
        ? `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')} ${hhmm(last)}`
        : 'never (every turn deleted)';
      out += `- **${sp.name}** — ${languages} — last heard ${heard}\n`;
    }
    return out;
  }

  function matchDelete(params) {
    let rows = state.segments;
    if (params?.speaker != null) rows = rows.filter((s) => s.speaker === Number(params.speaker));
    if (params?.session != null) rows = rows.filter((s) => s.session === Number(params.session));
    // Same time semantics as every other filter on the wire: inclusive `from`,
    // exclusive `to`, ISO or number (`Store::segments_matching`).
    const from = mockTimeParam(params?.from);
    const to = mockTimeParam(params?.to);
    if (from != null) rows = rows.filter((s) => s.t_ms >= from);
    if (to != null) rows = rows.filter((s) => s.t_ms < to);
    return rows;
  }

  function err(code, msg) {
    return Object.assign(new Error(msg), { code });
  }

  // --- wire ---------------------------------------------------------------

  try {
    fs.unlinkSync(sockPath);
  } catch {
    /* no stale socket to clear */
  }

  const server = net.createServer((sock) => {
    const client = { sock, topics: new Set(), hello: false, buf: '' };
    clients.add(client);
    sock.setEncoding('utf8');

    const write = (obj) => {
      try {
        sock.write(JSON.stringify(obj) + '\n');
      } catch {
        /* client vanished mid-write */
      }
    };

    sock.on('data', (chunk) => {
      client.buf += chunk;
      let nl;
      while ((nl = client.buf.indexOf('\n')) >= 0) {
        const line = client.buf.slice(0, nl).trim();
        client.buf = client.buf.slice(nl + 1);
        if (!line) continue;
        let msg;
        try {
          msg = JSON.parse(line);
        } catch {
          continue;
        }

        if (msg.hello) {
          if (msg.hello.proto !== PROTO) {
            write({ error: { code: 'proto', msg: `this daemon speaks proto ${PROTO}` } });
            sock.end();
            return;
          }
          client.hello = true;
          client.name = msg.hello.client;
          log('hello from', client.name);
          write({ welcome: { proto: PROTO, daemon: daemonId(), seq: state.seq, schema: SCHEMA } });
          continue;
        }

        if (!client.hello) {
          write({ error: { code: 'proto', msg: 'hello first' } });
          sock.end();
          return;
        }

        if (msg.id == null || typeof msg.method !== 'string') continue;
        const fn = methods[msg.method];
        if (!fn) {
          write({ id: msg.id, err: { code: 'unknown_method', msg: `no method ${msg.method}` } });
          continue;
        }
        try {
          write({ id: msg.id, ok: fn(msg.params ?? {}, client) ?? {} });
        } catch (e) {
          write({ id: msg.id, err: { code: e.code || 'internal', msg: e.message } });
        }
      }
    });

    sock.on('error', () => {});
    sock.on('close', () => clients.delete(client));
  });

  server.listen(sockPath, () => {
    try {
      fs.chmodSync(sockPath, 0o600);
    } catch {
      /* best effort — the real daemon enforces this */
    }
    log(`listening on ${sockPath} (proto ${PROTO}, seq ${state.seq}, feed ${feedMs}ms)`);
  });

  startFeed();
  seedCorrections();

  /**
   * One SIGUSR2's worth of the 0.8.0 event stream (see the header). Three
   * deliveries, in order, because all three are things a canned world cannot
   * produce on its own and all three have to land where a test can see them.
   */
  function nudge() {
    const n = state.nudges++;
    if (n === 0) {
      if (!state.notes.some((x) => x.id === LIVE_NOTE.id)) {
        const now = Date.now();
        const note = { ...LIVE_NOTE, t_ms: now, t_ns: String(now) + '000000' };
        state.notes.push(note);
        state.segments.push({
          id: note.segment_id,
          session: SESSIONS[2].id,
          source: 'mic',
          speaker: youSpeaker(),
          text: note.said,
          t_ms: now,
          t_ns: note.t_ns,
          dur_ms: 3400,
          overlap_frac: 0.02,
          match_score: null,
          label_via: 'mic',
          lang: 'en',
          lang_via: 'classified',
          asr_confidence: 'solid',
          text_via: 'live',
          thread: liveThread(),
        });
        // First, what the real daemon does all afternoon after 0.8.0: the
        // re-decode worker re-publishes an ARCHIVE row it just stamped — the
        // oldest one here, far outside any live window. A client must treat
        // it as history, not as an arrival (0.8.2: it went under "now").
        const oldest = state.segments.reduce((a, b) => (b.t_ms < a.t_ms ? b : a));
        oldest.text_via = 'context';
        emit('segments', 'segment', oldest);
        // The turn itself stays in the transcript — a note is a second reading
        // of a turn, not a turn that was filed somewhere else — so BOTH events
        // go out, and a client that only knows `segment` still sees the words.
        emit('segments', 'segment', state.segments[state.segments.length - 1]);
        emit('segments', 'note', notePayload(note));
      }
      return { sent: 'note', id: LIVE_NOTE.id };
    }
    if (n === 1) {
      // 0.11.0: one turn, provisionally three times and then for real.
      return { sent: 'partial-turn', ...emitPartialTurn() };
    }
    if (n === 2) {
      // 0.9.0: a reminder coming round, and a conversation the model has just
      // read. Both are things a canned world cannot produce on its own, and
      // both are the events the assistant round's two surfaces are built on.
      const note = state.notes.find((x) => x.due_ms != null && !x.fired)
        ?? state.notes.find((x) => x.state === 'open');
      if (note) {
        note.fired = true;
        note.due_ms = note.due_ms ?? Date.now();
        note.fired_ms = Date.now();
        // The alarm, and then the row — in that order, because a client raises
        // the notification from the first and repaints the list from the
        // second (PROTOCOL 0.9.0).
        emit('segments', 'reminder', {
          note_id: note.id,
          text: note.text,
          due_ms: note.due_ms,
          due_ns: String(note.due_ms) + '000000',
          segment_id: note.segment_id,
          t_ms: note.t_ms,
        });
        emit('segments', 'note', notePayload(note));
      }
      const fresh = {
        thread_id: 500,
        lang: 'de',
        day_offset: 0,
        rendered: 'names',
        summary:
          'Kira und Speaker 12 haben über das Portal im Treppenhaus geredet. Es geht erst auf, wenn das Licht ausgeht; das hinter der Bar führt zurück in dieselbe Instanz.',
        open: [],
        people: [1, 3],
      };
      if (!state.digests.some((d) => d.thread_id === fresh.thread_id)) {
        state.digests.unshift(fresh);
        emit('segments', 'digest', digestPayload(fresh));
      }
      return { sent: 'reminder', note_id: note?.id ?? null, digest: fresh.thread_id };
    }
    // A named voice walking into the instance. `who` is the VRChat display
    // name; linking it to a speaker is the client's job and is deliberately
    // case-insensitive on the user-given name (crates/recalld/src/roster.rs).
    if (n > 5) {
      // 0.10.0. A world entry, on the `roster` topic and deliberately not a
      // rename of the `roster` event: `roster` says who is present, `visit`
      // says a place was entered.
      const w = WORLDS[0];
      emit('roster', 'visit', {
        world_id: w.world_id,
        instance: '12345',
        name: w.name,
        t: String(Date.now()) + '000000',
      });
      return { sent: 'visit', world: w.name };
    }
    const who = state.speakers.find((s) => s.name)?.name ?? 'Kira';
    emit('roster', 'roster', { ev: 'join', who, t: String(Date.now()) + '000000' });
    return { sent: 'roster.join', who, repeat: n > 2 };
  }

  return {
    server,
    state,
    sockPath,
    emit,
    nudge,
    // Simulate the daemon dying and coming back with a fresh counter — the case
    // the GUI has to notice and full-resync from.
    restart(newSeq = 1) {
      // Coming back as a new version is the point, not a detail: see the
      // header note on SIGUSR1.
      state.restarts += 1;
      log(`simulated restart — now ${daemonId()}`);
      for (const c of clients) c.sock.destroy();
      clients.clear();
      state.seq = newSeq;
      state.replay = [];
    },
    close() {
      stopFeed();
      if (state.enrichTimer) clearInterval(state.enrichTimer);
      for (const c of clients) c.sock.destroy();
      clients.clear();
      server.close();
      try {
        fs.unlinkSync(sockPath);
      } catch {
        /* already gone */
      }
    },
  };
}

// --- CLI -------------------------------------------------------------------

const isMain = process.argv[1] && import.meta.url === `file://${path.resolve(process.argv[1])}`;
if (isMain) {
  const args = process.argv.slice(2);
  const get = (flag, dflt) => {
    const i = args.indexOf(flag);
    return i >= 0 && args[i + 1] ? args[i + 1] : dflt;
  };
  const mock = startMock({
    sockPath: get('--sock', defaultMockSocket()),
    feedMs: Number(get('--feed-ms', 2000)),
    seqStart: Number(get('--seq', 41823)),
    quiet: args.includes('--quiet'),
    semantic: !args.includes('--no-semantic'),
  });
  process.on('SIGUSR1', () => mock.restart(1));
  process.on('SIGUSR2', () => mock.nudge());
  const bye = () => {
    mock.close();
    process.exit(0);
  };
  process.on('SIGINT', bye);
  process.on('SIGTERM', bye);
}
