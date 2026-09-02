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

Daemon replies with its version, the current event sequence number, and the id of
this **run** of the daemon:

```json
{"welcome": {"proto": 1, "daemon": "recalld/0.7", "seq": 41823, "schema": 10, "boot": "18f3c0a1d4b2e900"}}
```

If `proto` is unsupported the daemon replies `{"error": {"code": "proto", ...}}` and
closes. A client reconnecting after a daemon restart compares `seq`: if it is lower
than the client's last-seen (daemon restarted) or the gap exceeds the replay buffer,
the client does a full resync (re-runs its queries).

**`boot` (0.7.5).** An opaque string, constant for the life of one daemon process and
different after every restart. Sequence numbers restart at 0 with the process, so a
`seq` alone cannot tell two runs apart: a client that remembered 40 across a restart,
reconnecting once the new daemon had published 60 events, used to be handed events
41–60 of an unrelated stream and applied them as the continuation of its own. A client
that keeps a `seq` across a reconnect **MUST** send the `boot` it was welcomed with on
`events.since`; a mismatch is answered `err: resync` instead of a replay. The parameter
is optional so that older clients still work — they simply do not get this protection.
A client that only ever follows the live stream from the current `welcome.seq` does
not need it.

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
| `speakers.list` | | id, name, counts, total time, `languages` |
| `speakers.name` | `{id, name}` | retroactive; broadcasts `relabel`. On a **merge tombstone**: `err:conflict` naming the canonical voice (0.7.5) — it holds no rows, so the write would land nowhere while the reply and the event claimed otherwise |
| `speakers.set_languages` | `{id, languages}` | which languages this voice speaks; broadcasts `relabel`. Same `err:conflict` on a tombstone (0.7.5), and for a sharper reason: the read resolved through the tombstone while the write did not |
| `speakers.prune` | `{apply?}` | list (default) or sweep the one-off voices. With `apply`, `voices` is what was **removed** (0.7.5) — it used to repeat the preview, which the client had already shown in its own confirmation |
| `speakers.delete` | `{id, keep_voiceprint?}` | delete one voice: its conversations always, its voiceprint unless kept. Works on a voice with **no segments left** — see below |
| `speakers.merge` | `{from, into}` | tombstone, no chains; broadcasts `relabel` |
| `speakers.split` | `{id}` | **async op** (below); work completes inline — the reply carries the op handle **plus** the outcome: `{op, kept, minted, auto, moved_segments, moved_prototypes, ambiguous, centroid_similarity, embed_model_id, resync, seq}`. Data-driven refusals (one voice, golden conflict) come back as `err:refused`. The minted speaker's `relabel` carries `split_from`. Past ~100 changed rows the per-segment events are skipped and `resync: true` tells clients to re-query. |
| `segments.reassign` | `{segment_id, speaker_id}` | a soft-deleted segment is `err:not_found` (0.7.5) |
| `segments.correct` | `{segment_id, text}` | feeds anchor per DESIGN §5; a soft-deleted segment is `err:not_found` (0.7.5) |
| `person.get` | `{id}` | the person page in one reply: totals, co-presence edges, recent conversations |
| `thread.get` | `{id}` | one conversation's segments, in order. A conversation whose every turn has been deleted is `err:not_found` (0.7.5), not an empty shell |
| `search` | `{q, speaker?, source?, from?, to?, limit?}` | FTS5 over transcripts |
| `search.semantic` | `{q, mode?, speaker?, source?, from?, to?, limit?}` | search by meaning; `mode: "hybrid"` fuses it with FTS. `err:unavailable` when the optional model is not installed — see below |
| `transcript` | `{session?, speaker?, from?, to?, limit?}` | chronological page — see "Paging the transcript" |
| `delete.preview` / `delete.run` | `{speaker?, session?, from?, to?}` | preview returns counts+bytes; run is an **async op** |
| `pause` / `resume` | | global capture pause (the panic path; must be instant) |
| `status` | | uptime, queue depth, drop counters, models loaded |

## Semantic search

`search.semantic` ranks turns by what they **mean** rather than by which words
they contain, over 384-dimension sentence embeddings of the transcript
(`multilingual-e5-small`, 384-d, int8). It exists for the query nobody can
phrase: *what did she say about that world* — and, because the two languages in
use here are German and English, for the German query that has to find the
English turn.

```json
{"id": 12, "method": "search.semantic", "params": {"q": "die Welt mit den Walen", "mode": "hybrid", "limit": 50}}
```

| param | meaning |
|---|---|
| `q` | the query. No syntax: unlike `search`, there is nothing to get wrong |
| `mode` | `"semantic"` (default) or `"hybrid"` |
| `speaker` / `source` / `from` / `to` / `limit` | exactly as `search` |

`mode: "hybrid"` runs FTS **and** the vector scan and fuses the two ranked
lists with reciprocal-rank fusion (k=60). Fusing *ranks* rather than scores is
deliberate: there is no exchange rate between FTS5's bm25 and a cosine, and any
constant that claimed there was would be a number nobody could justify.

The reply is `search`'s, plus:

```json
{"id": 12, "ok": {
  "total": 2, "q": "...", "mode": "hybrid", "model": "multilingual-e5-small-int8@1",
  "took_ms": 31.4,
  "hits": [{"id": 918, "...": "...", "via": "both", "rrf": 0.0323, "score": 0.871}]
}}
```

* **`via`** — `"keyword"`, `"semantic"` or `"both"`: which leg found this row.
  A client must show it. "These words are in there" and "this seemed to mean
  the same thing" are different claims, and a person deciding whether to trust
  a hit needs to know which one they are looking at.
* **`rrf`** — the fusion score the list is ordered by.
* **`score`** — cosine, **only present when the vector leg scored the row**. A
  keyword-only hit has no cosine and none is invented for it.

Ordering is fully determined (fusion score, then the better of the two ranks,
then segment id), so the same query twice is the same list twice.

### When the model is not installed

The embedding model is **optional** — `recalld models fetch --semantic`, ~135 MB
— and everything else works without it. `search.semantic` then answers:

```json
{"id": 12, "err": {"code": "unavailable", "msg": "semantic search is not installed. `recalld models fetch --semantic` installs ..."}}
```

An error, never an empty `hits` list: a client that rendered emptiness would be
telling the user she never said it. `status` carries the same answer up front,
so a client can decide what to offer before anyone types anything:

```json
"semantic": {"available": false, "how": "semantic search is not installed. ..."}
"semantic": {"available": true, "model": "multilingual-e5-small-int8@1", "dim": 384,
             "resident": 41822, "resident_bytes": 64238592,
             "indexed": 41822, "eligible": 41830, "pending": 8}
```

`pending` is transcripts captured before the model was installed (or corrected
since); `recalld semantic backfill` clears it. New turns are embedded as they
are transcribed.

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
`{"method": "events.since", "params": {"seq": N, "boot": "..."}}` replays it, or
returns `err: resync` if N has fallen out **or** if `boot` names a different run of
the daemon (see the handshake). The reply carries `{events, replayed, seq, boot}`.

### The `source` event (sources topic, 0.7.5)

```json
{"seq": 41827, "ev": "source", "data": {
  "id": 3, "match_key": "VRChat.exe", "kind": "app", "binary": "VRChat.exe",
  "display": "VRChat", "display_name": "VRChat", "allowed": true,
  "first_seen": "2026-08-31T18:45:50Z", "last_seen": "2026-09-01T20:11:03Z",
  "streams": 1, "state": "capturing"
}}
```

The body is a `sources.list` row, field for field, plus `state`. One shape, so a
client folds an arrival in exactly as it folds in a toggle and can never end up with
a half-described row.

`state` is what the source is doing at the instant the event was published:

| state | meaning |
|---|---|
| `seen` | on the graph, not being captured — either not allowed, or allowed and not yet attached |
| `capturing` | a capture stream is open on it right now |
| `stopped` | it was being captured; the capture stopped while the application stayed |
| `gone` | the node left the graph — the application closed its stream or quit |

Published on: an application first appearing on the graph (**whatever the allowlist
says about it** — default-deny is only usable if a client can show the user what was
refused), a capture starting, a capture stopping, the node going away, and a
`sources.set` toggle. Before 0.7.5 only the toggle published anything at all, so a
program launched after a client connected never appeared in its Sources list and
could therefore never be allowed without restarting the client.

`state` is the live answer; the row's `streams` count is the database's and can trail
it by one queue hop, because a capture that has just stopped still has an open session
row until the pipeline has drained the end event and written the last segment. A
client rendering a "capturing now" light should believe `state`.

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

### Per-speaker languages, provenance and storage (schema 5)

Three additive changes, all of them about the daemon saying *why* rather than
only *what*.

- **`speakers.list` rows carry `languages`**: a sorted array of BCP-47 tags
  (`["de"]`, `["de","en"]`) or `null`, which means *any* and is the default.
- **`speakers.set_languages {id, languages}`** → `{id, languages, seq}`.
  `languages` may be an array, a single string, or `null` / `[]` / `["any"]`,
  all three of which clear the declaration. Tags are lower-cased, de-duplicated
  and sorted, so one setting has one representation. Only `de` and `en` are
  accepted — the daemon classifies exactly those two, and a tag it cannot check
  is a correction it can never make; anything else is `err:params`. Broadcast on
  the existing **`relabel`** event, carrying `languages` *and* the current
  `name`, so a client folding it in never has to choose between the two facts.
  A `relabel` without a `languages` key says nothing about languages and must
  not clear them.

  What the declaration *does* is not symmetric, because the model catalogue is
  not. A voice pinned to exactly one language, and only such a voice, gets its
  transcripts checked against the text classifier. `en` + a German-looking
  transcript → the segment is decoded again with the English-only export, whose
  language is a property of the model rather than a hint; the new text replaces
  the old **only** if it clears the guards below, and `asr_model_id` moves with
  it. `de` + an English-looking transcript → up to 0.7.6 there was no
  German-constrained decoder to re-run it with, so the words were kept and the
  row was marked; since 0.7.7 the optional `--arbiter-de` model makes this
  direction work exactly like the other one. Either way the *speaker* is untouched: a voice does not become
  less recognisable by having been decoded in the wrong language.

  The declaration is load-bearing and a wrong one costs transcript quality: if
  a voice really does speak German and is declared English-only, the re-decode
  will replace good German with English-sounding nonsense. That is the honest
  consequence of a hard constraint, it is reversible (widen the languages), and
  it is why the default is `any` and the GUI copy says what each choice does.

- **Segment rows carry `lang` and `label_via`.** `lang` is the transcript's
  language when one is known — from the model when the model only speaks one,
  otherwise from the text classifier — and `null` when nobody could tell, which
  is a real answer. (Since 0.7.7 they also carry `lang_via`, which says *which*
  of those it was; see below.) `label_via` is how the *speaker* got there:
  `"match"` (the voicebank), `"mic"` (provenance, never a comparison),
  `"manual"` (a person said so), or `"proximity"`. **A client must render
  `proximity` as uncertain**: that label was inherited from the confident turns
  either side of a fragment too short to identify, so it is a guess with a good
  reason rather than a measurement, and it carries `match_score: null`.

- **`speakers.prune {apply?}`** → `{apply, count, voices, ...}`. Without
  `apply` it lists and changes nothing: `voices` is `[{id, auto, name,
  segments, total_ms, speech_ns}]` for every voice with at most one segment and
  under three seconds of speech, plus `max_segments` and `max_speech_ms` so a
  client can explain the bar. With `apply: true` it deletes them — the segments
  are soft-deleted exactly as `delete.run` does them, so the undo window still
  applies, and the identity goes with its prototypes and goldens — and answers
  `{count, removed, segments}`. It **refuses the pinned "You" speaker and every
  named voice**, whatever the counts say. Each removed voice is announced as a
  `relabel` carrying `pruned: true`, which is *not* a merge: nothing moved
  anywhere, the id simply stops existing and its rows go back to nameless.

- **`status` carries `storage`**, or `null` before anything has measured it:
  `{db_bytes, audio_bytes, audio_files, goldens_bytes, models_bytes,
  total_bytes, measured_at_utc_ns}` — the last as a **string**, like every other
  nanosecond value on the wire. It is measured by the retention sweeper once per
  pass (and once at start-up), never by the `status` call itself: the status
  poll runs every three seconds in every open client and the answer costs a walk
  of the whole data directory. `null` means "not measured yet" and a client must
  render it as such; zeroes would be a claim about an empty disk.

- **`status.counters`** gains `too_slight` (turns that matched nobody and were
  under the mint bar), `proximity_labelled`, `redecoded` and `lang_mismatch`
  (0.7.7 adds five more — see the language prior below); and,
  in 0.7.5, `gaps_discarded` — turns thrown away because the audio under them had
  a hole in it. `drops` says buffers were lost; this says a *turn* was, which is
  the number that tells a user whether the queue is big enough. A gap is never
  spliced: the half-built turn on the near side of it is discarded rather than
  joined to the words on the far side.

- **`status` carries `last_sweep` (0.7.5)**, or `null` before the sweeper has run
  once. The same block is pushed as a `sweep` event on the `status` topic after
  every pass:

  ```json
  {"seq": 41830, "ev": "sweep", "data": {
    "started_at_utc_ns": "1788283200000000000",
    "purged": 12, "purged_rows": 12, "purged_empty": 0, "aged_audio": 40,
    "unlinked_files": 52, "orphans_removed": 1, "orphan_files": 1,
    "orphan_goldens": 0, "dangling": 0, "dangling_paths": 0,
    "dangling_goldens": 0, "pruned_threads": 2, "errors": 0, "vacuumed": true
  }}
  ```

  Everything the retention sweeper does used to reach the daemon's log and
  nowhere else: an unlink that failed, an orphan removed, a row pointing at a
  file that is gone. Those are the outcomes that say whether deletion is really
  happening and whether anything is being destroyed that should not be, so they
  are now something a client can show. `errors` is the number of things the
  sweep tried and could not do — a sweep that has quietly stopped working, or
  started failing, is visible as a non-zero count rather than as silence. A
  failed sweep reports `errors: 1` and a `failed` string saying why.
  `started_at_utc_ns` is a **string**, like every nanosecond value on the wire.
  `null` means "not swept yet"; zeroes would claim a clean sweep that never ran.

### Deleting a voice (0.6.4)

**`speakers.delete {id, keep_voiceprint?}`** → `{id, name, keep_voiceprint,
removed_speaker, segments, total_segments, prototypes, embeddings, goldens,
threads, msg, seq}`.

DESIGN §8 always specified a choice here — "keep the bank entry (still labeled
going forward) or nuke it so they re-enroll fresh" — and only the first half of
the *first* option was ever built. Deleting by speaker went through
`delete.run`, which is scoped by **segments**: it never touched the speaker row,
its prototypes, its embeddings or its goldens, so a deleted voice kept its
voiceprint and went on matching new audio — and once its segments were gone the
same call matched zero rows and did nothing at all, for ever. This is the method
that is scoped by the **voice**.

- Both halves soft-delete the voice's live segments, with the same undo window
  and the same `purge` events (batched, ids named) `delete.run` produces, and
  the same operations rows: one per batch of segment ids, plus one for the
  identity carrying what it was called, what it owned and which half ran.
- `keep_voiceprint: true` — the speaker row, prototypes, goldens and embeddings
  stay. The voice keeps matching and keeps being labelled. Announced as a plain
  `relabel`; the reply's `msg` says the voiceprint was kept, and a client should
  show it rather than inventing its own sentence.
- `keep_voiceprint: false` (the default) — prototypes, this voice's embeddings,
  its goldens (rows and files) and the speaker row go too. Announced exactly as
  a sweep is, `relabel` with `pruned: true`: not a merge, nothing moved, the id
  stops existing.
- **A voice with no live segments is a normal input**, not an error: it is the
  case the method exists for.
- Two refusals, both `err:refused`. The pinned **"You"** voice — deleting it
  would not stop you being recorded, since the next turn through the microphone
  mints the pin again, so the message names `mic.set` as the switch that does
  work and date/session deletes as the way to remove what you have said. And a
  **merge target** on the nuke path: other voices are tombstoned onto that row
  and `speakers.merged_into` is a real foreign key, so the refusal names the
  count and points at `keep_voiceprint: true`, which still takes every
  conversation. A tombstone id is `err:conflict` naming the canonical voice, as
  `speakers.split` does.
- `delete.run` is unchanged and stays the method for deletes scoped by **date or
  session** — those are about words rather than about a person.

### Conversation threads and the person page (schema 6)

The memory graph's Tier 1 ([GRAPH.md](GRAPH.md)): derived, deterministic, always
on. Two read methods and one additive field, and nothing here is stored twice —
every number is a query over live segments, so a deleted segment stops counting
the instant it is deleted.

- **Segment rows carry `thread`**: the id of the conversation the turn belongs
  to, or `null`. A client draws a boundary where it changes and renders a `null`
  exactly as it rendered every row before threading existed — which is what
  makes this additive rather than a proto bump. Threads are per session, cut by
  `[graph].thread_gap_s` (20 s) of silence and by turn-taking: two pairs talking
  past each other in one instance are two threads. The rule is written out in
  `crates/recalld/src/threads.rs`, including what it gets wrong on purpose.

  A turn is threaded **once**, when it is stored. A later rename, merge or
  reassignment does not re-thread it: which conversation a turn was part of is a
  fact about the clock and the room, and re-deriving it on every relabel would
  make the same transcript thread differently depending on when you looked.

- **`person.get {id}`** → the whole person page in one round trip, because the
  page is one question and half a person is worse than a spinner:

  ```json
  {"id": 12,
   "speaker": {"id": 12, "you": false, "name": "Kira", "auto": "Speaker_03",
               "languages": ["de"], "first_seen": "2026-07-02T18:24:00Z"},
   "languages": ["de"],
   "totals": {"segments": 412, "speech_ms": 1832000, "speech_ns": "1832000000000",
              "sessions": 9, "threads": 31,
              "first_heard_ms": ..., "first_heard_ns": "...",
              "last_heard_ms": ...,  "last_heard_ns": "..."},
   "edges": [{"speaker_id": 4, "name": "Ash", "auto": "Speaker_18",
              "threads": 9, "seconds": 412.5, "speech_ms": 412500,
              "last_ns": "...", "last_ms": ..., "roster_seconds": null}],
   "recent_threads": [{"thread_id": 31, "session": 3,
                       "started_ns": "...", "started_ms": ...,
                       "ended_ns": "...", "ended_ms": ...,
                       "segments": 14,
                       "participants": [{"speaker_id": 12, "name": "Kira",
                                         "auto": "Speaker_03"}],
                       "preview": "wait, which portal was it —"}]}
  ```

  Field conventions, tightly:

  - `name` is `null` until a person names the voice; `auto` is the generated
    label and is always present. Same split as `speakers.list`, in every place a
    person appears here — edges and thread participants included, so a client
    can render a voice it has never queried.
  - **An edge means a shared *conversation*, not a shared instance.** A public
    lobby has forty people in it and you spoke to two; edges come from threads,
    which is the whole reason threads exist. `threads` is how many they shared.
  - `seconds` (and the identical `speech_ms`) is **how much the other person
    spoke in those shared conversations** — their speech, not the intersection
    of two speech timelines. People take turns, so an intersection would be
    near zero and would say nothing about a friendship.
  - `roster_seconds` is the co-presence the VRChat roster can vouch for: time
    both display names were in the same instance. It is `null` — not `0` —
    whenever either voice has no user-given name matching a roster line, which
    is the common case. Zero would be a claim; `null` is the truth.
  - `last_ns`/`started_ns`/`ended_ns` are **strings**, like every nanosecond
    value on the wire; the `_ms` twins are for rendering.
  - `recent_threads` is newest-first and capped (12). `preview` is the first
    thing anybody said in the conversation, or `null` — a handle, never a
    summary. Tier 1 does not summarise.
  - `participants` is ordered most-talkative-first, which is the order a person
    reads a list of names in.
  - An unknown id is `err:not_found`, not an empty page.

- **`thread.get {id}`** → `{thread_id, session, started_ns/ms, ended_ns/ms,
  participants, preview, segments}`. `segments` are **the ordinary segment
  shape**, in time order, so a client renders a conversation with the code it
  already has for a transcript. Unknown id is `err:not_found`.

- Deleting is deletion: purging a segment drops it from every total and every
  edge, and a thread whose last row goes is deleted with it. A soft delete by
  date or session (`delete.run`) hides the rows but keeps the thread, because
  the rows are still coming back. `speakers.delete` is the exception and says
  so in its reply's `threads`: deleting a *person* takes the conversations that
  have nothing live left in them, because a thread is only an index into the
  transcript, it is re-derivable from it, and an index into rows no view can
  reach is not a conversation.

### The memory graph, Tiers 2 and 3 (schema 7)

[GRAPH.md](GRAPH.md)'s remaining two tiers: when a turn was talking about, who
owes what to whom, and what a conversation was about. Everything here is
**derived, second-class and re-derivable** — every row carries its provenance,
every read of it joins back to a live segment, and deleting the segment or the
person deletes it.

Additive, as always: a client that does not know these methods is a client that
does not show the Memory view, and nothing else changes.

#### `graph.summary` → the whole view in one round trip

```json
{"counts": {"time_refs": 41, "commitments": 12, "open": 5,
            "candidates": 4, "confirmed": 1, "done": 6, "dismissed": 1,
            "from_rules": 3, "from_llm": 9,
            "topics": 7, "threads": 31, "threads_enriched": 24,
            "threads_pending": 7},
 "enrichment": {"phase": "idle", "reason": null, "thread": null,
                "batch_done": 0, "batch_total": 0,
                "walked": 24, "found": 9, "retracted": 2, "labelled": 24,
                "last_error": null, "last_run_utc_ns": "..."},
 "config": {"enabled": false, "installed": true, "llm_threads": 4,
            "llm_threads_min": 1, "llm_threads_max": 32,
            "gpu_layers": 0, "llm_model": "qwen2.5-3b-instruct-q4_k_m.gguf",
            "thread_gap_s": 20.0, "batch_threads": 4,
            "min_thread_segments": 3, "download_bytes": 1946604700}}
```

- `open` is `candidates + confirmed` — what is still owed, in either sense. It
  is what the GUI's rail badge counts, because a badge is a number you are
  meant to act on and a settled commitment is not one.
- `enrichment.phase` is one of **`off` · `unavailable` · `blocked` · `idle` ·
  `running`**. `reason` is always present for `blocked` and `unavailable` and
  is a sentence a person can act on ("capture is paused — nothing is written
  down, including this"). A client with copy for three states should collapse
  `blocked` into idle-with-a-reason and `unavailable` into a "not installed"
  note. As of **0.7.2** `blocked` has exactly two causes, both transient — a
  pause and a full capture queue. A running game is **not** one of them: the
  worker keeps reading while you play, pinned and at nice 19 (GRAPH.md Tier 3).
- `config.installed` is whether the optional model is **on disk**, which is a
  different question from `enabled`. Both are needed: on and not installed is a
  real state, and it has to read as "fetch it" rather than as a failure.
- `config.download_bytes` is what turning it on would cost, so a client's copy
  does not hard-code a number that could drift. `config.llm_threads_min` and
  `config.llm_threads_max` are there for the same reason: a client builds the
  control for `llm_threads` out of the range `graph.set` will accept.

#### `commitments.list {state?, limit?}`

`state` filters to one of `candidate` · `confirmed` · `done` · `dismissed`;
omitting it returns every state, which is what a client showing history wants.
An unknown state is `err:params`, never a silently empty list.

Soonest-due first, and **undated rows sort last**: a promise with no date is not
overdue, it is merely open.

```json
{"id": 41, "segment": 9012, "thread": 505,
 "who": {"speaker_id": 4, "name": "Ash", "auto": "Speaker_18"},
 "to":  {"speaker_id": 1, "name": "Kira", "auto": "Speaker_03"},
 "what": "cut the recording and send it over",
 "said": "I'll cut the recording and send it over on Friday",
 "due_ms": 1788480000000, "due_ns": "1788480000000000000",
 "due_raw": "on Friday", "due_kind": "weekday",
 "state": "candidate", "source": "llm",
 "model_id": "qwen2.5-3b-instruct-q4_k_m", "confidence": 0.75,
 "t_ms": ..., "t_ns": "...", "created_ms": ..., "updated_ms": ...}
```

Field conventions, tightly:

- **`source` and `state` are different questions and must never be conflated.**
  `source` is which tier claimed this — `"rules"` is a modal-pattern match and
  a guess, `"llm"` is the local model under a verdict-first grammar. `state` is
  what a *person* has decided. A client must render the two distinctly; the
  reference client marks the source in the row itself rather than in a tooltip.
- **Nothing has ever been acted on.** `candidate` means the daemon noticed
  something. Only `commitments.set_state` moves a row, and only a human calls it.
- `confidence` on an `llm` row is the **bake-off's measured precision**, not a
  per-answer score: the model does not report one, and inventing a per-row
  number would be worse than reporting the one that was measured.
- `said` is the transcript line the claim is about. It travels with the claim so
  a person can disagree with it without going to look — a commitment nobody can
  check against the words is not evidence of anything.
- `due_raw` is the phrase as spoken ("morgen", "on Friday"); `due_ms`/`due_ns`
  are it resolved against **when it was said**. `due_ms` is `null` when nobody
  said a date, which is not the same as overdue. `due_kind` is the precision:
  `weekday` · `day` · `week` · `weekend` · `clock` · `in`.
- `name` is `null` until somebody names the voice; `auto` is always present.
  Same split as everywhere else a person appears. Ids are canonical — a merge
  tombstone is resolved before it goes on the wire.
- `to` is `null` when the promise was made to a conversation with more than two
  people in it: the daemon will not guess which of them was meant.

#### `commitments.set_state {id, state}` → the updated row

The one thing that moves a commitment, and the daemon never calls it itself.
Broadcast as a `commitment` event on the **ops** topic, so a decision made in
the CLI reaches an open window without either re-querying. Unknown id is
`err:not_found`; an unknown state is `err:params`.

#### `topics.list {limit?, per_topic?}`

```json
{"topics": [{"topic": "world portals", "threads": 4, "segments": 37,
             "last_ms": ..., "last_ns": "...", "thread_ids": [606, 601, 500]}]}
```

Most recently heard first. `thread_ids` is newest-first and capped by
`per_topic` — a topic is a way *into* conversations, not a list of every one
that ever mentioned it. Topics are written by Tier 3, so with the local model
off this is an empty list rather than an error, and a client renders "nothing
yet".

#### `graph.get` / `graph.set {enabled?, llm_threads?, gpu_layers?}`

The Tier 3 settings, live **and** persisted to `config.toml` — live because the
switch is in the UI and a switch that needs a restart is not a switch, persisted
because a switch that forgets is worse. `graph.set` needs at least one field
(`err:params` otherwise), clamps what it is given, and answers with what is now
true rather than with what it was asked for. The reply carries
`config.persisted`, exactly like `mic.set`.

`llm_threads` is clamped to **1–32** (`config.llm_threads_min`/`_max`), never
refused: zero threads is a daemon that cannot run the model, and two hundred is
one that tries. It is the setting that replaced standing down while a game runs
(0.7.2), and it takes effect on the **next** model call — threads are an
argument to a `llama-cli` invocation, so a conversation already being read
finishes at the width it started with.

#### `graph.enrich {action}` — `"start"` | `"stop"`

The imperative wrapper over the same switch, because a person pressing a button
in the Memory view is not editing a setting, they are asking for the pass to
happen. Asking for what is already true answers `{"changed": false}` and is not
an error. Nothing is interrupted mid-conversation: the worker stands down at the
next boundary, which is at most one model call away.

#### The `graph` event (status topic)

The worker's state, pushed whenever a client would render it differently — the
same shape as `graph.summary`'s `enrichment` block. It also rides inside
`status.graph`, so a client that missed an event still converges on the truth,
exactly as the mic block does. A counter moving on its own is **not** an event:
a loop that ticks every twenty seconds must not push a hundred and eighty
identical frames an hour into everybody's replay buffer.

#### Deletion

Purging a segment deletes its `time_refs` and its `commitment`. Deleting a
*person* deletes every commitment they made **and every commitment made to
them** — "you owe Kira the shader link" must not survive deleting Kira — and
`speakers.delete` reports both counts in its reply (`commitments`, `time_refs`).
A *soft* delete needs no cascade: every graph read joins to a live segment, so
hiding the transcript line hides what was inferred from it and undoing the
delete brings both back.

- `delete.run` with a filter matching every live segment is refused unless the
  request carries `confirm_everything: true` — one absent parameter must never
  mean "wipe the transcript".
- `search` returns `total` = all matches (pre-LIMIT); hits are the newest first.
- A limited unanchored `transcript` returns the NEWEST `limit` rows, ascending.

### The conversational language prior (0.7.7)

A conversation has a language, and that is usable evidence about turns the
decoder got wrong. Two additive changes on the wire and one new method.

- **Segment rows carry `lang_via`** alongside the `lang` they have carried since
  schema 5. It says how the *language* got there, and it is the words'
  provenance the way `label_via` is the speaker's:

  | value | meaning | did the text change? |
  |---|---|---|
  | `"model"` | the ASR export only speaks one language | no |
  | `"classified"` | the text classifier read the words | no |
  | `"context"` | the classifier could not tell, so the row took the language the rest of its **thread** was speaking | no |
  | `"re-decode"` | an arbiter re-read the audio under a hard language constraint and its words won | **yes** |
  | `"mismatch"` | the language is in dispute and nothing could settle it; `lang` is `null` | no |
  | `null` | no words, so no language | no |

  A client needs to act on exactly two of them. `"re-decode"` means the words on
  screen came from a *different model* than every other row — `asr_model_id`
  says which — and a client that shows provenance for a name should show it for
  the words. `"mismatch"` means the daemon is openly unsure, which is why `lang`
  is null rather than a guess. `"context"` needs no UI: it is a language, from
  a weaker source, over unchanged text.

  **The context**, precisely: the majority language over a thread's last
  `[lang].context_window` (10) clear stamps, requiring at least
  `context_min_clear` (3) of them and at least `context_min_agree` (0.7)
  agreement. Below that bar the conversation has no language and nothing
  happens — which is the right answer for a greeting and for a genuinely
  bilingual room. Stamps that were themselves inherited never count as evidence:
  a context that fed on its own inferences would confirm itself.

  **Priority.** An explicit single-language `speakers.set_languages` declaration
  beats the context, always, and that path behaves exactly as it did in 0.6.1.
  The context applies to untagged voices, bilingual ones, and turns nobody could
  put a voice to at all — a decoder flip is a property of the audio, not of
  whether the voicebank recognised the speaker.

  **What is not symmetric any more.** Up to 0.7.6 a German-looking transcript
  from an English-only voice was re-decoded and an English-looking one from a
  German-only voice could only be flagged, because the catalogue had no German
  decoder. `models fetch --arbiter-de` adds one (Whisper base with its language
  token forced), so both directions now re-decode. That makes the hazard
  symmetric too: a wrong declaration now produces confident nonsense in either
  direction. It is reversible (widen the languages, or `lang repair` once the
  declaration is right), and it is why the default is `any`.

  **When a re-decode may replace text**, all of it measured
  (`spike/arbiter_de.py`): the segment is at least
  `[lang].arbiter_min_duration_s` (1.5 s) long — below that the arbiter's own
  word precision is 28% against 54% at 1.5 s, and replacing one wrong transcript
  with a differently wrong one is not a correction; the arbiter's output
  survives caption-stripping (Whisper narrates non-speech: `(soft music)`,
  `[Applause]`), has at least `arbiter_min_words` (2) words, and reads as the
  language it was asked for. Anything less flags the row and keeps the words.

- **`lang.repair {limit?}`** → the backlog walk, bounded and synchronous:

  ```json
  {"arbiters": ["en", "de"], "scanned": 100, "repaired": 41,
   "repaired_de": 39, "repaired_en": 2, "settled": 3, "kept": 12,
   "too_short": 28, "unavailable": 0, "undecidable": 16, "no_audio": 0,
   "flagged": 214, "repairable": 198}
  ```

  Every count is a row and they partition `scanned`; `flagged` and `repairable`
  are the backlog *after* the run, so a client can loop until `flagged` stops
  falling. `settled` is a row whose disagreement had simply gone away — the
  declaration changed since it was marked — and whose classifier reading was
  restored. `limit` defaults to 100 and is clamped to 500: a whole backlog
  belongs to `recalld lang repair`, which runs in its own process at idle
  priority and can be interrupted. Every row whose stored words or language
  moved is announced as an ordinary **`segment`** event, so an open transcript
  updates without re-querying. With no arbiter installed the method answers
  honestly — `scanned: 0` and a `note` naming the fetch — rather than clearing
  marks it cannot justify.

- **`status.counters`** gains `context_stamped`, `flips_suspected`,
  `redecoded_de`, `redecoded_en` and `repairs`, beside the existing `redecoded`
  (their sum across both directions) and `lang_mismatch`.

  0.8.0 adds four more from the idle quality worker: `redecoded_context` and
  `redecode_skipped_no_audio` (a short turn with no neighbouring clip inside
  `[asr].context_max_gap_s` — there is no continuous recording, so a turn alone
  in a silence has no context to be read with), and `solid` / `shaky` from the
  cross-check.

## Paging the transcript

`transcript` pages in both directions with no second method, because **which
end the `limit` bites off is decided by whether the query is anchored**, and
only `from` and `session` anchor it:

> **A `transcript` call carrying `to` but no `from` and no `session` returns the
> newest `limit` rows strictly before `to`, ascending — so passing the timestamp
> of the oldest row you hold walks one page further back, and a page shorter
> than `limit` means you have reached the beginning of the archive.**

`from` is inclusive (`>=`), `to` is exclusive (`<`) — which is what makes
"page again from the first row I already have" return the rows *before* it
rather than repeating it. Both accept ISO-8601 or a number (milliseconds under
`1e15`, nanoseconds at or above it). Every reply is ascending by time whichever
end was trimmed, so a client renders chronologically without re-sorting.

The three shapes, together:

| call | meaning |
|---|---|
| `{limit: 600}` | the live tail — the newest 600 rows |
| `{to: <oldest held>, limit: 400}` | one page further back |
| `{from: <day start>, to: <day end>}` | a bounded range, oldest first (anchored) |

This is what the desktop client's infinite scrollback and its date picker are
built on. `mock/mockd.js` implements it identically and `gui/test/paging.test.js`
holds it to the same expectation table as the daemon's own unit test — a
divergence in this one method is not a mock detail, it is a bug the GUI cannot
see until a user hits it.

## 0.8.0 — the accuracy round (contract for three parallel builds)

- **Transcript confidence.** Segment rows/events gain `asr_confidence`:
  `"solid"` (a second decoder agreed), `"shaky"` (it disagreed), or `null`
  (no cross-check ran). Flag only — the text is never replaced by the
  cross-check. Segments also gain `text_via`: `"live"` (first pass),
  `"context"` (re-decoded with surrounding session audio), `"arbiter"`
  (language arbiter). A re-decode re-publishes the segment event.

  Two clarifications the implementation forced:

  - **`null` is not "fine".** It means nothing has checked those words, which
    is the state of every row on a machine that has not run
    `recalld models fetch --confidence` (~154 MB, Canary 180m). `status.asr`
    carries `{confidence: {enabled, available, tau, how}}` so a client can tell
    "not installed" from "an older daemon". A context re-decode **clears** the
    flag it invalidates: a `solid` verdict about words that have since been
    replaced is a claim nobody ever made.
  - **A re-decode may fill a transcript that was empty.** The live pass stores
    a decode with no words as `text: null`, and a short fragment the decoder
    made nothing of is precisely what the surrounding audio rescues, so
    `text_via: "context"` can arrive on a row a client is showing as silent.
- **Vocabulary.** `vocab.get` → `{user: [...], auto: {roster: [...], worlds:
  [...], corrections: [...], speakers: [...]}, effective: [...],
  applied_to_decoder: false}`; `vocab.set {terms: [...]}` replaces the user
  glossary (persisted) and answers with the same object. Event `vocab` on
  change (topic `status`). `effective` is the union, user glossary first, then
  named speakers, correction words, roster names and world names, de-duplicated
  case-insensitively and capped at `[asr].vocab_max_terms` (500).

  **The daemon does not bias the transducer toward `effective`, and the field
  `applied_to_decoder` says so on every reply.** That is a change to this
  contract, and it is a measurement, not an omission: `spike/hotwords_bench.py`
  put contextual biasing at +9.1% relative recall on the targeted words against
  a +20% gate, only under `modified_beam_search` (which costs 1.6 pp of WER on
  its own before a single hotword is added), with the glossary bleeding into
  unrelated utterances at the strongest setting — control WER 8.3% → 29.4%.
  The list is assembled, stored, served and announced; nothing is fed to the
  recognizer until something can use it without that trade.
- **Accuracy.** `accuracy.summary` → `{corrections, estimated_wer,
  by_source: [{source, corrections, estimated_wer}], by_speaker: [{speaker_id,
  corrections, estimated_wer}], since_ns}` computed from `segments.correct`
  operations (the pre-correction text lives in `prior_state`).
- **One query box.** `search.ask {q, limit?}` → `{interpretation: {query,
  speaker_id?, speaker_label?, from_ns?, to_ns?, mode}, hits: [...]}`. The
  daemon parses a natural-language question — speaker mentions, time
  references in de/en (the Tier-2 parser), the remaining words as the query —
  and runs the hybrid search with those facets. The interpretation is returned
  so the GUI can show what it understood and let the user correct a facet.
- **Notes to self.** A MIC segment whose text starts with a wake phrase
  (`recall, merk dir`, `recall, remember`, `recall, notiz`, `recall, note`;
  case/punctuation-insensitive) becomes a note. `notes.list {limit?, state?}`
  → `{notes: [{id, segment_id, text, t_ms, t_ns, state}]}`,
  `notes.set_state {id, state: "open"|"done"|"dismissed"}`. Event `note`
  (topic `segments`) when one is created. The segment itself stays in the
  transcript.
- **Briefs.** `person.brief {id}` → `{speaker, last_heard_ms, open_to_you:
  [commitment], open_from_you: [commitment], recent_topics: [...],
  notes_mentioning: [...]}`. Clients may show it when a `roster` join event
  names a linked speaker (the join itself is unchanged).

## Versioning rules

- `proto` bumps only on breaking changes; additive fields/methods/events are free.
- Clients MUST ignore unknown fields and unknown event types.
- The daemon MUST keep serving proto N−1 for one release after N ships (hub updates
  restart the daemon under a possibly-stale GUI — DESIGN §2).
