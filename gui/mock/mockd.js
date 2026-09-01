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

import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';

const PROTO = 1;
const DAEMON = 'recalld-mock/0.5';
const SCHEMA = 6;
const REPLAY_MAX = 200; // deliberately small: overrunning it must be reachable

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
];

/// The pinned "You" speaker. It exists in the mock's voicebank from the start
/// (a previous session's microphone minted it) so the transcript's distinct
/// treatment is reachable without waiting for a live enrolment.
const YOU_SPEAKER = 8;

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

// name: null means "not named yet" — the onboarding case (DESIGN §5).
// `languages` is schema v5: which languages this voice actually speaks, so a
// wrong-language transcript can be corrected rather than merely noticed. `null`
// is "any", the default, and is what almost every voice starts as.
const SPEAKERS = [
  { id: 1, name: 'Kira', auto: 'Speaker_03', languages: ['de'], first_seen: '2026-07-02T18:24:00Z' },
  { id: 2, name: null, auto: 'Speaker_07', first_seen: '2026-07-02T18:31:00Z' },
  { id: 3, name: null, auto: 'Speaker_12', first_seen: '2026-07-11T21:02:00Z' },
  { id: 4, name: 'Ash', auto: 'Speaker_18', first_seen: '2026-07-19T19:47:00Z' },
  { id: 5, name: null, auto: 'Speaker_31', first_seen: '2026-08-14T20:50:00Z' },
  { id: 6, name: null, auto: 'Speaker_44', first_seen: '2026-08-29T20:19:00Z' },
  // Unnamed, like any other voice the daemon minted — the difference is where
  // its label comes from, not whether the user has typed one.
  { id: 8, name: null, auto: 'You', first_seen: '2026-08-29T20:12:00Z' },
  // A one-off: one grunt, a second of speech, no name. The kind of row the
  // mint bar now prevents and `speakers.prune` sweeps up when it slipped
  // through anyway (0.6.1).
  { id: 9, name: null, auto: 'Speaker_52', first_seen: '2026-08-31T18:07:00Z' },
];

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

/// Which conversation a canned row belongs to (schema v6). Blocks of five, so
/// the history really does contain several threads with different people in
/// them — the transcript's separators and the person page's "people they talk
/// with" both need more than one to say anything.
const THREAD_BLOCK = 5;
const threadFor = (i) => 500 + Math.floor(i / THREAD_BLOCK);

// A fixed history so every run of the GUI and every screenshot looks the same.
function buildHistory() {
  const out = [];
  const base = Date.parse('2026-08-31T18:05:00Z');
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
      source: mine ? 'mic' : i % 5 === 3 ? 'Discord' : 'VRChat.exe',
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
    thread: threadFor(27),
  });
  return out;
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

export function startMock({ sockPath = defaultMockSocket(), feedMs = 2000, seqStart = 41823, quiet = false } = {}) {
  const log = quiet ? () => {} : (...a) => console.log('[mockd]', ...a);

  const state = {
    seq: seqStart,
    paused: false,
    speakers: SPEAKERS.map((s) => ({ ...s })),
    segments: buildHistory(),
    sources: SOURCES.map((s) => ({ ...s })),
    // Off by default, exactly as the real daemon ships it.
    mic: { enabled: false, mode: 'follow', active: false, device: null },
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

  /// The `{name, auto}` pair every place a person appears in the graph, so a
  /// client can render a voice it has never queried.
  function person(id) {
    const sp = speakerById(id);
    return { name: sp?.name ?? null, auto: sp?.auto ?? `Speaker_${id}` };
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
      first_seen: s.first_seen,
      segments: c.get(s.id)?.segments ?? 0,
      total_ms: c.get(s.id)?.total_ms ?? 0,
    }));
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
      // 0.6.1: measured by the retention sweeper, not by this call. The audio
      // figure moves as the feed runs, so the footer and the Sources card have
      // something that actually changes to render.
      storage: storagePayload(),
      models: ['silero-vad', 'segmentation-3.0', 'eres2net-en', 'parakeet-tdt-110m'],
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
    // Interleaved, not separate: the user's own voice arrives in the same
    // stream as everybody else's, and the only thing that marks it is where it
    // came from. Every third tick, while the mic is actually capturing.
    if (micActive() && state.feedIdx % 3 === 2) {
      state.feedIdx += 1;
      emitMine();
      return;
    }
    const [sp, text, overlap, score] = CANNED_LINES[state.feedIdx % CANNED_LINES.length];
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
      source: state.feedIdx % 6 === 5 ? 'Discord' : 'VRChat.exe',
      speaker: mintedSpeaker ? 77 : overlap > 0.1 ? null : sp,
      text,
      t_ms: now,
      t_ns: String(now) + '000000',
      dur_ms: 1600 + ((state.feedIdx * 911) % 3800),
      overlap_frac: overlap,
      match_score: overlap > 0.1 ? null : score,
      thread: liveThread(),
    };
    state.segments.push(seg);
    state.queue = state.feedIdx % 4;
    emit('segments', 'segment', seg);
  }
  /// One turn off the user's own microphone. Note what is NOT here: a
  /// match_score. There was no comparison, so there is no score to report, and
  /// a client that renders one would be inventing it.
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
        if (code !== 'de' && code !== 'en') {
          throw err('bad_params', `unknown language ${JSON.stringify(code)}; this daemon classifies de and en only`);
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

    'speakers.split'(params) {
      const sp = speakerById(Number(params?.id));
      if (!sp) throw err('not_found', `no speaker ${params?.id}`);
      const op = runOp('speakers.split', 6, () => {
        const fresh = {
          id: Math.max(...state.speakers.map((s) => s.id)) + 1,
          name: null,
          auto: `Speaker_${String(50 + state.nextOp % 40).padStart(2, '0')}`,
          first_seen: new Date().toISOString(),
        };
        state.speakers.push(fresh);
        // Hand a third of the source speaker's segments to the new identity.
        let n = 0;
        for (const seg of state.segments) {
          if (seg.speaker === sp.id && n++ % 3 === 0) seg.speaker = fresh.id;
        }
        emit('relabel', 'relabel', { speaker: fresh.id, name: null });
        return { created: fresh.id };
      });
      return { op };
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
          first_seen: sp.first_seen,
        },
        languages: sp.languages ?? null,
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
        recent_threads: [...threads]
          .sort((a, b) => b - a)
          .slice(0, 12)
          .map((t) => threadPayload(t)),
      };
    },

    'thread.get'(params) {
      const id = Number(params?.id);
      const rows = state.segments.filter((s) => s.thread === id);
      if (!rows.length) throw err('not_found', `no thread ${params?.id}`);
      return { ...threadPayload(id), segments: rows };
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
      seg.text = String(params?.text ?? '');
      seg.corrected = true;
      emit('segments', 'segment', seg);
      return { segment_id: seg.id, text: seg.text };
    },

    search(params) {
      const q = String(params?.q ?? '').trim().toLowerCase();
      let rows = state.segments;
      if (q) rows = rows.filter((s) => s.text.toLowerCase().includes(q));
      if (params?.speaker != null) rows = rows.filter((s) => s.speaker === Number(params.speaker));
      if (params?.source) rows = rows.filter((s) => s.source === params.source);
      if (params?.from) rows = rows.filter((s) => s.t_ms >= Date.parse(params.from));
      if (params?.to) rows = rows.filter((s) => s.t_ms <= Date.parse(params.to));
      const limit = Number(params?.limit ?? 50);
      const hits = rows.slice(-limit).reverse();
      return { hits, total: rows.length, q: params?.q ?? '' };
    },

    transcript(params) {
      let rows = state.segments;
      if (params?.session != null) rows = rows.filter((s) => s.session === Number(params.session));
      if (params?.speaker != null) rows = rows.filter((s) => s.speaker === Number(params.speaker));
      if (params?.from) rows = rows.filter((s) => s.t_ms >= Date.parse(params.from));
      if (params?.to) rows = rows.filter((s) => s.t_ms <= Date.parse(params.to));
      const limit = Number(params?.limit ?? 200);
      return { segments: rows.slice(-limit), sessions: SESSIONS };
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

  function matchDelete(params) {
    let rows = state.segments;
    if (params?.speaker != null) rows = rows.filter((s) => s.speaker === Number(params.speaker));
    if (params?.session != null) rows = rows.filter((s) => s.session === Number(params.session));
    if (params?.from) rows = rows.filter((s) => s.t_ms >= Date.parse(params.from));
    if (params?.to) rows = rows.filter((s) => s.t_ms <= Date.parse(params.to));
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

  return {
    server,
    state,
    sockPath,
    emit,
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
  });
  process.on('SIGUSR1', () => mock.restart(1));
  const bye = () => {
    mock.close();
    process.exit(0);
  };
  process.on('SIGINT', bye);
  process.on('SIGTERM', bye);
}
