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
const DAEMON = 'recalld-mock/0.3';
const SCHEMA = 2;
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

const SOURCES = [
  { match_key: 'VRChat.exe', binary: 'wine64-preloader', display: 'VRChat', allowed: true, first_seen: '2026-07-02T18:22:00Z', last_seen: '2026-08-31T18:05:00Z', streams: 1 },
  { match_key: 'Discord', binary: 'Discord', display: 'Discord', allowed: true, first_seen: '2026-07-02T19:01:00Z', last_seen: '2026-08-31T17:44:00Z', streams: 1 },
  { match_key: 'firefox', binary: 'firefox', display: 'Firefox', allowed: false, first_seen: '2026-07-03T09:14:00Z', last_seen: '2026-08-31T12:30:00Z', streams: 0 },
  { match_key: 'spotify', binary: 'spotify', display: 'Spotify', allowed: false, first_seen: '2026-07-05T22:10:00Z', last_seen: '2026-08-30T23:58:00Z', streams: 0 },
  { match_key: 'mpv', binary: 'mpv', display: 'mpv', allowed: false, first_seen: '2026-08-14T20:44:00Z', last_seen: '2026-08-14T22:02:00Z', streams: 0 },
];

// name: null means "not named yet" — the onboarding case (DESIGN §5).
const SPEAKERS = [
  { id: 1, name: 'Kira', auto: 'Speaker_03', first_seen: '2026-07-02T18:24:00Z' },
  { id: 2, name: null, auto: 'Speaker_07', first_seen: '2026-07-02T18:31:00Z' },
  { id: 3, name: null, auto: 'Speaker_12', first_seen: '2026-07-11T21:02:00Z' },
  { id: 4, name: 'Ash', auto: 'Speaker_18', first_seen: '2026-07-19T19:47:00Z' },
  { id: 5, name: null, auto: 'Speaker_31', first_seen: '2026-08-14T20:50:00Z' },
  { id: 6, name: null, auto: 'Speaker_44', first_seen: '2026-08-29T20:19:00Z' },
];

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

// A fixed history so every run of the GUI and every screenshot looks the same.
function buildHistory() {
  const out = [];
  const base = Date.parse('2026-08-31T18:05:00Z');
  for (let i = 0; i < 26; i++) {
    const [sp, text, overlap, score] = CANNED_LINES[i % CANNED_LINES.length];
    const t = base + i * 47_000;
    out.push({
      id: 1000 + i,
      session: SESSIONS[i % 3 === 2 ? 2 : i % 2].id,
      source: i % 5 === 3 ? 'Discord' : 'VRChat.exe',
      speaker: overlap > 0.1 ? null : sp,
      text,
      t_ms: t,
      t_ns: String(t) + '000000',
      dur_ms: 1800 + ((i * 733) % 4200),
      overlap_frac: overlap,
      match_score: overlap > 0.1 ? null : score,
    });
  }
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
    return state.speakers.map((s) => ({
      id: s.id,
      name: s.name,
      auto: s.auto,
      first_seen: s.first_seen,
      segments: c.get(s.id)?.segments ?? 0,
      total_ms: c.get(s.id)?.total_ms ?? 0,
    }));
  }

  function statusPayload() {
    return {
      uptime_s: Math.round((Date.now() - state.startedAt) / 1000),
      paused: state.paused,
      queue_depth: state.queue,
      drops: state.drops,
      sources_capturing: state.sources.filter((s) => s.allowed && s.streams > 0).length,
      sources_allowed: state.sources.filter((s) => s.allowed).length,
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
    };
    state.segments.push(seg);
    state.queue = state.feedIdx % 4;
    emit('segments', 'segment', seg);
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
      const s = state.sources.find((x) => x.match_key === params?.match_key);
      if (!s) throw err('not_found', `no source ${params?.match_key}`);
      s.allowed = !!params?.allowed;
      s.streams = s.allowed ? 1 : 0;
      emit('sources', 'source', { match_key: s.match_key, allowed: s.allowed });
      emit('status', 'status', statusPayload());
      return { match_key: s.match_key, allowed: s.allowed };
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
