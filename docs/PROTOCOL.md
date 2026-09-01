# recalld socket protocol — v1 draft

Unix domain socket at `$XDG_RUNTIME_DIR/nx-recall.sock`, mode 0600. No TCP.
Newline-delimited JSON, UTF-8, one message per line, both directions.

Design constraints this encodes (DESIGN §2, §8): protocol versioning is mandatory;
clients are dumb views that must survive daemon restarts; relabels are broadcast and
retroactive; long operations must not block other clients.

## Handshake

Client speaks first:

```json
{"hello": {"proto": 1, "client": "nx-recall-gui/0.1"}}
```

Daemon replies with its version and the current event sequence number:

```json
{"welcome": {"proto": 1, "daemon": "recalld/0.3", "seq": 41823, "schema": 2}}
```

If `proto` is unsupported the daemon replies `{"error": {"code": "proto", ...}}` and
closes. A client reconnecting after a daemon restart compares `seq`: if it is lower
than the client's last-seen (daemon restarted) or the gap exceeds the replay buffer,
the client does a full resync (re-runs its queries).

## Requests

```json
{"id": 7, "method": "search", "params": {"q": "portal world", "speaker": 12, "limit": 50}}
{"id": 7, "ok": {...}}                          ← exactly one terminal reply per id
{"id": 7, "err": {"code": "...", "msg": "..."}}
```

Methods (initial set):

| method | params | notes |
|---|---|---|
| `sources.list` / `sources.set` | `{match_key, allowed}` | live toggle, no restart. `sources.set` **refuses** `match_key: "mic"` — see `mic.set` |
| `mic.get` / `mic.set` | `{enabled?, mode?}` | the microphone switch; live, no restart |
| `speakers.list` | | id, name, counts, total time |
| `speakers.name` | `{id, name}` | retroactive; broadcasts `relabel` |
| `speakers.merge` | `{from, into}` | tombstone, no chains; broadcasts `relabel` |
| `speakers.split` | `{id}` | **async op** (below); work completes inline — the reply carries the op handle **plus** the outcome: `{op, kept, minted, auto, moved_segments, moved_prototypes, ambiguous, centroid_similarity, embed_model_id, resync, seq}`. Data-driven refusals (one voice, golden conflict) come back as `err:refused`. The minted speaker's `relabel` carries `split_from`. Past ~100 changed rows the per-segment events are skipped and `resync: true` tells clients to re-query. |
| `segments.reassign` | `{segment_id, speaker_id}` | |
| `segments.correct` | `{segment_id, text}` | feeds anchor per DESIGN §5 |
| `search` | `{q, speaker?, source?, from?, to?, limit?}` | FTS now, +vec later |
| `transcript` | `{session?, speaker?, from?, to?}` | chronological page |
| `delete.preview` / `delete.run` | `{speaker?, session?, from?, to?}` | preview returns counts+bytes; run is an **async op** |
| `pause` / `resume` | | global capture pause (the panic path; must be instant) |
| `status` | | uptime, queue depth, drop counters, models loaded |

## Async operations

Long work (split re-cluster, bulk delete) returns immediately:

```json
{"id": 9, "ok": {"op": "op_412"}}
```

then progresses over the event stream (`op.progress`, `op.done`, `op.failed` with
the op id). Other requests keep flowing meanwhile.

## Events (pub/sub)

Subscribe: `{"id": 3, "method": "subscribe", "params": {"topics": ["segments", "relabel", "sources", "ops", "roster"]}}`.

Every event carries a monotonically increasing `seq`:

```json
{"seq": 41824, "ev": "segment", "data": {"id": ..., "session": ..., "speaker": ..., "text": ..., "overlap_frac": ...}}
{"seq": 41825, "ev": "relabel", "data": {"speaker": 12, "name": "Kira"}}
{"seq": 41826, "ev": "roster",  "data": {"ev": "join", "who": "...", "t": ...}}
```

`relabel` events mean: every view showing that speaker id updates in place — clients
never need to re-query for a rename. The daemon keeps a short replay buffer;
`{"method": "events.since", "params": {"seq": N}}` replays it or returns
`err: resync` if N has fallen out.

## Field conventions (v1 clarifications, born from the first real client)

- **Timestamps on events**: both `t_ms` (JSON number, ms since epoch — for display)
  and `t_ns` (JSON **string**, UTC nanoseconds — full fidelity exceeds 2^53).
- `status` is also a subscribable topic; the daemon pushes a `status` event on state
  change (pause especially). Clients may poll the method as well.
- `events.since` replies with the batch inline: `{"ok": {"events": [...]}}`.
  Clients dedupe by seq, so stream re-push is tolerated but the batch is canonical.
- `speakers.list` rows: `auto` (generated "Speaker_NN" label) alongside `name`
  (null until the user names them).
- `relabel` carries `merged_into` when caused by a merge. `delete.run` completion is
  followed by a `purge` event naming removed rows.
- `sources.list` rows: display name, binary, first_seen, last_seen, active streams.
- After any resync, clients rebase `lastSeq` onto the welcome/resync seq; the daemon
  guarantees `welcome.seq` reflects the live counter and subsequent events increase
  strictly from it.
- **Voice preview.** `segments.audio {id}` → `{id, wav_b64, duration_ms, sample_rate,
  bytes}`: that segment's stored WAV (16 kHz mono), standard padded base64 in the
  JSON frame. `err:not_found` = no such live segment (unknown id, or soft-deleted);
  `err:gone` = the row is still in the transcript but its audio is not (retention
  expired, or the file went missing) — the message says so, and a client must show it
  as a fact rather than a failure; `err:refused` = the file is over 10 MB, which no
  real segment is. `duration_ms`/`sample_rate` come from the WAV header, not the row.
- `speakers.sample {id, limit?}` → `{id, samples: [{segment_id, t_ms, t_ns,
  duration_ms, text, match_score}]}`: that voice's live segments **that still have a
  file on disk**, ranked longest-first (bucketed to whole seconds) then best-matched,
  `limit` default 3, max 20. It is the "play me this voice" query, so the naming flow
  does not page through transcripts. An empty `samples` is a valid answer (all of that
  voice's audio has aged out); an unknown speaker is `err:not_found`.
- **Frame budget**: one NDJSON line may reach 16 MB (`service::MAX_FRAME_BYTES`), the
  size the 10 MB audio cap can produce once base64'd. Clients MUST accept frames that
  large; a smaller guard drops the connection mid-reply instead of failing one request.

### The microphone (schema 4)

The user's own default input is a capture source like any other in the schema and
nothing like one in the consent model, so it is additive everywhere and never
folded into the allowlist.

- **`sources.list` rows carry `kind`**: `"app"` or `"mic"`. A client that does not
  know the difference must not render the microphone as an application — it hears
  the *room*, not one program, and its copy has to say so.
- **`sources.set` refuses `match_key: "mic"`** with `err:refused`, naming `mic.set`.
  The two would otherwise write to different halves of the config (`[rules]` vs
  `[mic]`) and disagree about a consent decision.
- **`mic.get`** → `{enabled, mode, active, state, device, you_speaker}`. `device` is
  the `[mic].device` pin (config-file only — a machine setup decision, not a click)
  or `null` for "follow the default source". `you_speaker` is the pinned speaker id,
  or `null` until the microphone has produced a segment.
- **`mic.set {enabled?, mode?}`** → the same block plus `persisted`. Both fields are
  optional and applied independently, so a client can change the mode without
  knowing whether the switch was already on. `mode` is `"follow"` or `"always"`;
  anything else is `err:params`, never a default. A call with neither field is
  `err:params`. The change is live *and* written to `config.toml`, so it survives a
  daemon restart; `persisted: false` means only the former.
- **`state`** is the one string worth printing, and there are five:
  `off`, `following:idle` (enabled, waiting for an allowed application),
  `following:active` (an allowed application is captured, so the room is being
  recorded), `always:active`, and `always:idle` — which means the switch is on in
  `always` mode and the daemon has not managed to open an input device. A missing
  microphone must never read as "recording".
- **`status`** carries `mic` (the same block, without `you_speaker`) and the flat
  `mic_state` string, plus `counters.mic_segments` / `mic_enrolled` / `mic_goldens`.
- **`mic` event**, on the existing **`status`** topic: `{"ev": "mic", "data": {...}}`,
  the same block. Published when the switch moves and when the capture thread opens
  or closes the stream — which is the only way a client can see a `follow`-mode
  transition, since nothing else in the protocol reports it. No new topic, so no
  client has to change its subscription and an older one ignores it under the
  versioning rule. There is deliberately **no** roster-style start/stop event beyond
  this: the segment stream already shows what is being recorded.
- **`speakers.list` rows carry `you`**: `true` for the pinned speaker the microphone
  labels, `false` for everyone else. Exactly one row can be `true`. It is not a
  match: segments from the microphone carry `speaker` with `match_score: null`,
  because there was no comparison to score. `overlap_frac` is still populated, so a
  client can still distrust the *audio* of a segment whose *name* is certain — which
  is what speakers-bleed looks like when the user runs loudspeakers.
- A **merge moves the pin.** `speakers.merge {from: <You>, into: <someone>}` leaves
  the tombstone a tombstone and re-points `you_speaker` at the target; the next
  microphone segment lands there. Merging in the other direction changes nothing.
  A second "You" is never minted.

## Versioning rules

- `proto` bumps only on breaking changes; additive fields/methods/events are free.
- Clients MUST ignore unknown fields and unknown event types.
- The daemon MUST keep serving proto N−1 for one release after N ships (hub updates
  restart the daemon under a possibly-stale GUI — DESIGN §2).
