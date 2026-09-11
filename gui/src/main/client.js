// NDJSON client for recalld's unix socket (docs/PROTOCOL.md v1).
//
// Deliberately free of any `electron` import: the whole reconnect/resync
// state machine is the part most likely to be wrong, so it stays plain Node
// and is unit-tested headless (test/client.test.js) against mock/mockd.js.
//
// The contract this implements, in the order it happens:
//
//   1. connect to $XDG_RUNTIME_DIR/nx-recall.sock (or $NX_RECALL_SOCK)
//   2. client speaks first:  {"hello":{"proto":1,"client":"nx-recall-gui/0.1"}}
//   3. daemon answers:       {"welcome":{"proto":1,"daemon":...,"seq":N,...}}
//   4. subscribe to the topics we render
//   5. catch up: events.since(lastSeq) — or declare a full resync
//
// Resync rules (PROTOCOL "Handshake"): the client compares the welcome `seq`
// with its own last-seen. Lower means the daemon restarted; an `err: resync`
// from events.since means the gap exceeded the daemon's replay buffer. Either
// way the client re-runs its queries — it never tries to patch a hole.

import net from 'node:net';
import os from 'node:os';
import { EventEmitter } from 'node:events';

export const PROTO = 1;
export const CLIENT_NAME = 'nx-recall-gui/0.1';

// "status" is an ADDITIVE topic this client asks for (see README of the report:
// pause state has to reach every client, and PROTOCOL's topic list predates the
// tray). A daemon that does not know it simply never sends those events, and
// the GUI's 3 s status poll covers the gap — so asking is free either way.
export const TOPICS = ['segments', 'relabel', 'sources', 'ops', 'roster', 'status'];

const RECONNECT_MIN = 250;
const RECONNECT_MAX = 5000;
const REQUEST_TIMEOUT = 15000;

// One NDJSON line's byte budget, matched by the daemon (service::MAX_FRAME_BYTES
// in crates/recalld). It exists so a daemon that never sends a newline cannot
// grow this buffer without bound — but `segments.audio` puts a whole WAV in one
// frame (10 MB capped on disk, ~13.4 MB once base64'd), so the guard has to sit
// above what the daemon is allowed to produce. Below it, the guard would hang up
// mid-reply on exactly the request that asked for the audio.
const MAX_FRAME_BYTES = 16 * 1024 * 1024;

export function defaultSocketPath() {
  if (process.env.NX_RECALL_SOCK) return process.env.NX_RECALL_SOCK;
  const run = process.env.XDG_RUNTIME_DIR || `/run/user/${typeof process.getuid === 'function' ? process.getuid() : 1000}`;
  return `${run}/nx-recall.sock`;
}

/**
 * Events emitted:
 *   state   {status:'connecting'|'connected'|'offline', daemon, schema, seq, error}
 *   event   {seq, ev, data}         — every daemon event, live or replayed
 *   resync  {reason}                — re-run every query; the view is stale
 *   caughtup {from, to, replayed}   — events.since succeeded, no resync needed
 */
export class RecallClient extends EventEmitter {
  constructor({ socketPath = defaultSocketPath(), clientName = CLIENT_NAME, autoReconnect = true } = {}) {
    super();
    this.socketPath = socketPath;
    this.clientName = clientName;
    this.autoReconnect = autoReconnect;

    this.sock = null;
    this.frameParts = [];
    this.frameBytes = 0;
    this.frameRejected = false;
    this.nextId = 1;
    this.pending = new Map(); // id → {resolve, reject, timer}
    this.status = 'offline';
    this.daemon = null;
    this.schema = null;
    this.lastSeq = null; // highest event seq this client has *applied*
    this.backoff = RECONNECT_MIN;
    this.retryTimer = null;
    this.closed = false;

    // While catching up, live events are queued so the replay can be applied in
    // seq order first — otherwise a live event bumps lastSeq past the replay and
    // the whole replayed window gets dropped as "already seen".
    this.catchingUp = false;
    this.queued = [];
  }

  // -- connection ----------------------------------------------------------

  connect() {
    if (this.closed || this.sock) return;
    this._setState('connecting');
    const sock = net.createConnection({ path: this.socketPath });
    this.sock = sock;
    sock.setEncoding('utf8');
    sock.on('connect', () => this._onConnect());
    sock.on('data', (chunk) => this._onData(chunk));
    sock.on('error', (err) => this._onClose(err));
    sock.on('close', () => this._onClose(null));
  }

  close() {
    this.closed = true;
    if (this.retryTimer) clearTimeout(this.retryTimer);
    this.retryTimer = null;
    this._failPending(new Error('client closed'));
    if (this.sock) this.sock.destroy();
    this.sock = null;
    this._setState('offline');
  }

  async _onConnect() {
    let welcome;
    try {
      welcome = await this._handshake();
    } catch (e) {
      // A refused handshake is not a transient blip — but reconnecting is still
      // right: the daemon may be mid-restart under a hub update (DESIGN §2).
      this._setState('offline', e.message);
      if (this.sock) this.sock.destroy();
      return;
    }

    this.daemon = welcome.daemon ?? null;
    this.schema = welcome.schema ?? null;
    const prior = this.lastSeq;
    this._setState('connected');

    this.catchingUp = true;
    this.queued = [];
    try {
      await this.request('subscribe', { topics: TOPICS });
    } catch (e) {
      // A connection without a subscription is the worst state this client can
      // be in: the socket is open, the status bar is green, and not one event
      // will ever arrive again (audit finding #13). It used to warn and carry
      // on into catch-up, which made the app permanently deaf until somebody
      // restarted it. Drop the socket instead — `_onClose` then schedules the
      // ordinary bounded-backoff reconnect, and the next handshake subscribes.
      this.emit('warn', `subscribe failed: ${e.message} — dropping the connection so it is retried`);
      this.catchingUp = false;
      this.queued = [];
      if (this.sock) this.sock.destroy();
      return;
    }

    // Only NOW is the connection worth anything, so only now does the backoff
    // reset. Resetting it on the socket opening would turn a daemon that
    // accepts connections and refuses subscriptions into a 250 ms hot loop.
    this.backoff = RECONNECT_MIN;

    if (prior == null) {
      // First connection of this session: nothing to catch up, everything to load.
      this._rebaseSeq(welcome.seq);
      this._finishCatchup();
      this.emit('resync', { reason: 'first-connect' });
      return;
    }
    if (typeof welcome.seq === 'number' && welcome.seq < prior) {
      // Sequence went backwards → the daemon restarted and its counter reset.
      // Rebasing is not cosmetic: _applyEvent drops anything at or below
      // lastSeq as a replay duplicate, so keeping the pre-restart number would
      // silently swallow the whole new stream until it climbed back past it.
      this._rebaseSeq(welcome.seq);
      this._finishCatchup();
      this.emit('resync', { reason: 'daemon-restart' });
      return;
    }
    try {
      const res = await this.request('events.since', { seq: prior });
      const evts = Array.isArray(res?.events) ? res.events : [];
      for (const ev of evts) this._applyEvent(ev);
      this._finishCatchup();
      this.emit('caughtup', { from: prior, to: this.lastSeq, replayed: evts.length });
    } catch (e) {
      // The gap is unrecoverable, so the client re-queries from the daemon's
      // current position rather than trying to resume from a lost one.
      this._rebaseSeq(welcome.seq);
      this._finishCatchup();
      this.emit('resync', { reason: e?.code === 'resync' ? 'replay-buffer-overrun' : `events.since failed: ${e.message}` });
    }
  }

  // Adopt the daemon's sequence position. Queued live events that are already
  // ahead of it survive; anything at or below it is history we are about to
  // re-query anyway.
  _rebaseSeq(seq) {
    if (typeof seq !== 'number') return;
    this.lastSeq = seq;
    this.queued = this.queued.filter((e) => typeof e.seq !== 'number' || e.seq > seq);
    // The last `state` was emitted before we knew the daemon's position, so
    // anything showing seq (the status footer, the e2e report) would otherwise
    // keep quoting the pre-restart number.
    if (this.status === 'connected') this._setState('connected');
  }

  _finishCatchup() {
    this.catchingUp = false;
    const q = this.queued;
    this.queued = [];
    for (const ev of q) this._applyEvent(ev);
  }

  _onClose(err) {
    if (!this.sock) return;
    this.sock.removeAllListeners();
    this.sock.destroy();
    this.sock = null;
    this.frameParts = [];
    this.frameBytes = 0;
    this.frameRejected = false;
    this.catchingUp = false;
    this.queued = [];
    this._failPending(err || new Error('daemon connection closed'));
    if (this.status !== 'offline') this._setState('offline', err ? err.message : null);
    if (this.closed || !this.autoReconnect) return;
    const wait = this.backoff;
    this.backoff = Math.min(RECONNECT_MAX, Math.round(this.backoff * 1.8));
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      this.connect();
    }, wait);
    if (this.retryTimer.unref) this.retryTimer.unref();
  }

  _setState(status, error = null) {
    this.status = status;
    this.emit('state', {
      status,
      daemon: this.daemon,
      schema: this.schema,
      seq: this.lastSeq,
      socketPath: this.socketPath,
      error,
    });
  }

  // -- framing -------------------------------------------------------------

  _onData(chunk) {
    if (this.frameRejected) return;
    let start = 0;
    while (start < chunk.length) {
      const nl = chunk.indexOf('\n', start);
      const part = chunk.slice(start, nl < 0 ? chunk.length : nl);
      // Check every frame BEFORE parsing, including complete frames in one
      // read. Count UTF-8 bytes, not UTF-16 code units. The socket's decoder
      // preserves characters split across reads.
      this.frameBytes += Buffer.byteLength(part, 'utf8');
      if (this.frameBytes > MAX_FRAME_BYTES) {
        this.frameParts = [];
        this.frameBytes = 0;
        this.frameRejected = true;
        this.emit('warn', 'oversized frame from daemon — dropping connection');
        if (this.sock) this.sock.destroy();
        return;
      }
      if (part) this.frameParts.push(part);
      if (nl < 0) return;
      // Join once per complete frame. Repeated concatenation + newline scans
      // otherwise copy/scan a large audio reply again on every socket read.
      const line = this.frameParts.join('').trim();
      this.frameParts = [];
      this.frameBytes = 0;
      start = nl + 1;
      if (!line) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        this.emit('warn', 'dropped unparseable line from daemon');
        continue;
      }
      this._dispatch(msg);
    }
  }

  _send(obj) {
    if (!this.sock || this.sock.destroyed) throw new Error('daemon offline');
    this.sock.write(JSON.stringify(obj) + '\n');
  }

  _dispatch(msg) {
    if (msg == null || typeof msg !== 'object') return;

    if (msg.welcome && this._welcomeWaiter) {
      const w = this._welcomeWaiter;
      this._welcomeWaiter = null;
      w.resolve(msg.welcome);
      return;
    }
    if (msg.error && this._welcomeWaiter) {
      const w = this._welcomeWaiter;
      this._welcomeWaiter = null;
      w.reject(Object.assign(new Error(msg.error.msg || msg.error.code || 'handshake refused'), { code: msg.error.code }));
      return;
    }

    if (msg.id != null && (msg.ok !== undefined || msg.err !== undefined)) {
      const p = this.pending.get(msg.id);
      if (!p) return; // late reply to a request we already timed out — ignore
      this.pending.delete(msg.id);
      clearTimeout(p.timer);
      if (msg.err) p.reject(Object.assign(new Error(msg.err.msg || msg.err.code || 'request failed'), { code: msg.err.code }));
      else p.resolve(msg.ok);
      return;
    }

    if (typeof msg.ev === 'string') {
      if (this.catchingUp) this.queued.push(msg);
      else this._applyEvent(msg);
      return;
    }
    // PROTOCOL "Versioning rules": unknown message shapes are ignored, never fatal.
  }

  _applyEvent(msg) {
    if (typeof msg.seq === 'number') {
      if (this.lastSeq != null && msg.seq <= this.lastSeq) return; // duplicate from replay
      this.lastSeq = msg.seq;
    }
    this.emit('event', msg);
  }

  // -- requests ------------------------------------------------------------

  _handshake() {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this._welcomeWaiter = null;
        reject(new Error('daemon did not answer hello'));
      }, REQUEST_TIMEOUT);
      this._welcomeWaiter = {
        resolve: (w) => {
          clearTimeout(timer);
          if (w.proto !== PROTO) {
            reject(new Error(`daemon speaks proto ${w.proto}, this client speaks ${PROTO}`));
            return;
          }
          resolve(w);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      };
      try {
        this._send({ hello: { proto: PROTO, client: this.clientName, host: os.hostname() } });
      } catch (e) {
        clearTimeout(timer);
        this._welcomeWaiter = null;
        reject(e);
      }
    });
  }

  request(method, params = {}, { timeout = REQUEST_TIMEOUT } = {}) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      let timer;
      try {
        this._send({ id, method, params });
      } catch (e) {
        reject(e);
        return;
      }
      timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method} timed out`));
      }, timeout);
      if (timer.unref) timer.unref();
      this.pending.set(id, { resolve, reject, timer });
    });
  }

  _failPending(err) {
    for (const [, p] of this.pending) {
      clearTimeout(p.timer);
      p.reject(err);
    }
    this.pending.clear();
    if (this._welcomeWaiter) {
      const w = this._welcomeWaiter;
      this._welcomeWaiter = null;
      w.reject(err);
    }
  }
}
