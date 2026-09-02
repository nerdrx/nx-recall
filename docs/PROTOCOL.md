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
  - **A re-decode is on the record (0.8.1).** Every replacement writes an
    `operations` row `op: "segments.redecode"` with `prior_state:
    {segment_id, text, asr_model_id, text_via}` — the words, model and route
    *before* the pass, exactly as `segments.correct` keeps a person's. It is
    not a correction: `accuracy.summary` and the "hand-corrected, needs no
    second opinion" rule key on `segments.correct` alone.
  - **One word gets no verdict (0.8.1).** Rows with fewer than two words are
    marked checked with `asr_confidence: null`, never `shaky`: a one-word
    transcript cannot disagree by degrees, and on the first day of real use
    "H", "Yeah." and "Mm-hmm" were a third of the shaky rows. Same floor as
    the mint bar.
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
- **Accuracy.** `accuracy.summary` → `{corrections, estimated_wer, by_source:
  [{source, corrections, estimated_wer}], by_speaker: [{speaker_id,
  corrections, estimated_wer}], since_ns, since_ms}`, computed from
  `segments.correct` operations. `prior_state` is
  `{"segment_id": N, "text": "<the transcript before the edit>"}` — verified,
  already written by `segments.correct`, and nothing had to be extended. The
  *corrected* text is recovered by reading one segment's corrections in order:
  each one's result is the next one's `prior_state.text`, and the last one's is
  the row as it now stands, so a turn corrected twice contributes two
  measurements. `estimated_wer` is the **mean of the per-correction word error
  rates** (word-level Levenshtein over the corrected word count), not a corpus
  ratio; it is `null` — never `0` — when nothing has been corrected, and a
  correction of a turn that had no transcript at all is skipped. `since_ns` is
  the oldest correction counted (a string; `since_ms` renders), `null` when
  there are none. Every row is bucketed by the segment's source and speaker **as
  they are now**, so a reassignment moves past corrections with it.
- **One query box.** `search.ask {q, limit?}` → `{q, total, interpretation:
  {query, speaker_id, speaker_label, from_ns, to_ns, from_ms, to_ms, mode},
  hits: [...]}`. The daemon parses a natural-language question — a named
  speaker (case-insensitive, de/en possessives: `Aspens`, `von Aspen`,
  `Aspen's`; a one-edit slip is forgiven on names of five characters or more,
  and only *named* voices are matched), a de/en time reference, and the
  remaining words as the query — then runs the search with those facets.
  `from_ns`/`to_ns` are strings and the window is `[from, to)`.
  - **Time is read backwards here.** A question is about what has already been
    said, so `am Montag` / `on Monday` is the most recent such day (today
    included) and not `timeref`'s next one, and `gestern` / `last week` /
    `letzten Monat` have no forward reading at all. This is a second,
    deliberately retrospective table (`crate::ask`), not a flag on `timeref`.
  - **Language words are not a facet.** There is no `lang` in `interpretation`;
    `auf Deutsch` in a question is dropped with the rest of the question's
    grammar (interrogatives, auxiliaries, articles, the verbs of saying), since
    it is neither something to filter by nor a word any turn contains.
  - `mode` says which engine answered: `"hybrid"` (FTS fused with the vector
    leg — the semantic model is installed), `"fts"` (keyword only; a missing
    semantic model is **not** an error here, unlike `search.semantic`), or
    `"facets"` — the whole question was facets, so `query` is empty and `hits`
    is the slice of transcript those facets select, newest first.
- **Notes to self.** A MIC segment whose text starts with a wake phrase
  (`recall, merk dir`, `recall, remember`, `recall, notiz`, `recall, note`;
  case/punctuation-insensitive, and `merke dir` as a German alias) becomes a
  note. The wake word tolerates one edit, because the decoder hands back
  "Ricall" and "Recoll" on short turns; markers of five characters or more do
  too, `note` does not. `notes.list {limit?, state?}` → `{state, notes: [{id,
  segment_id, text, t_ms, t_ns, state, created_ms}]}` — `t_ms`/`t_ns` are the
  segment's start, i.e. when it was *said*. `notes.set_state {id, state:
  "open"|"done"|"dismissed"}` returns the note and broadcasts it. Event `note`
  (topic `segments`) when one is created, when a re-decode changes its words,
  and on every state change. One note per segment: a turn coming back through
  the pipeline updates its note rather than filing a second, and a note whose
  words have not changed is not re-announced — so a dismissal is never undone
  by a re-decode. A note with no text after the wake phrase is not a note. The
  segment itself stays in the transcript, and purging it purges the note.
- **Briefs.** `person.brief {id}` → `{speaker, last_heard_ms, last_heard_ns,
  open_to_you: [commitment], open_from_you: [commitment], recent_topics:
  [{topic, thread_id, last_ms, last_ns}], notes_mentioning: [note]}`. Open means
  `candidate` or `confirmed`; `done` and `dismissed` are decisions and are not
  re-raised. `open_to_you` is what **they** promised (`who` is this person,
  including rows with no counterparty); `open_from_you` is what **you** promised
  them (`who` is your pinned voice, `to` is this person). `recent_topics` is
  `threads.topic` for their recent conversations, each label once, newest first
  — empty until the Tier 3 pass has run, which is off by default.
  `notes_mentioning` matches the speaker's **name**, so an unnamed voice has
  none. `commitment` and `note` are the shapes `commitments.list` and
  `notes.list` return. Clients may show it when a `roster` join event names a
  linked speaker (the join itself is unchanged).

## 0.9.0 — `translation` on a segment (contract for two parallel builds)

A sibling track adds a translation to turns in languages the user does not read.
It is a purely additive field on a segment row and on the `segment` event, and
every client that has never heard of it ignores it (see "Versioning rules"):

```json
{"id": 41902, "text": "…", "lang": "de",
 "translation": {"lang": "en", "text": "…", "via": "nllb-200"}}
```

The shape the caption surfaces already read, written down here so the two builds
cannot drift apart:

- **`translation` is absent on most rows, and absent means nothing.** A turn in
  a language the reader speaks needs no second line. A client must not render an
  empty one, and must not infer "not translated yet" from its absence — there is
  no pending state on the wire.
- **`text` is required and is the translated words.** A block with no `text`, or
  with only whitespace in it, is treated as absent. Renderers trim it.
- **`lang` is the language the TRANSLATION is in** — the target, not the source.
  The source is the segment's own `lang`. Short tag (`"en"`, `"de"`). Optional:
  a block with no `lang` still renders, just without the tag beside it.
- **`via` names what produced it** (a model id, `"cloud"` never — nothing leaves
  the machine). Provenance for a sheet; no surface renders it today.
- **The original is never replaced.** Both caption surfaces draw the translation
  *under* the words that were actually said, in a lighter weight. A client that
  substituted one for the other would be putting words in somebody's mouth.
- **It travels on the re-published segment too.** A `text_via: "context"`
  re-decode changes the words, so a translation of the old words is stale: the
  daemon either re-translates and re-publishes both, or omits `translation`
  entirely on that event. It must not re-publish a segment whose `text` moved
  while its `translation` did not.

Rendered by `gui/src/renderer/captions.js` (the desktop caption bar) and by
`crates/nx-recall-overlay/src/raster.rs` (the headset one), and read in one
place each: `translationOf()` and `Turn::translation`.

## Versioning rules

- `proto` bumps only on breaking changes; additive fields/methods/events are free.
- Clients MUST ignore unknown fields and unknown event types.
- The daemon MUST keep serving proto N−1 for one release after N ships (hub updates
  restart the daemon under a possibly-stale GUI — DESIGN §2).

## 0.9.0 — ground truth (Discord)

Every accuracy number this daemon has had is either somebody else's benchmark
or `accuracy.summary`, which measures the transcripts **you chose to correct**
— biased high by construction, and silent about speaker identity, which nobody
corrects one turn at a time. Speaker identity has therefore never been
measured at all, and "the deferred labelling pass" has been on the plan since
day one because the honest alternative was a transcript and a pen.

Discord already has the labels. Its client draws a speaking ring per user, and
a Vencord plugin (`RecallBridge`, in `nerdrx/vencord-nx-plugins`) can read the
flux event that ring is drawn from. This section is that stream and what is
done with it. It is additive: no method, event, field or behaviour described
above this line changes, and `proto` stays `1`.

- **What is reachable, and what is not.** Discord decodes remote voice in the
  **native engine**, so per-user audio is not reachable from a plugin and this
  is not speaker separation — the daemon still records one mixed stream off
  the speakers, exactly as before. What *is* reachable and exact: per-user
  `SPEAKING` start/stop, voice-channel membership, the current channel, guild
  nicknames, and whether the local user is muted or deafened. That is
  who-spoke-when, and who-spoke-when is a yardstick. **A truth verdict is
  never a label**: `segments.speaker_id` stays whatever the voicebank decided,
  and nothing in here writes it.

- **The loopback ingest.** `[truth] enabled = false`, `port = 7797`, off until
  `recalld truth on`. `crate::server`'s first line is "No TCP, ever (DESIGN
  §8)" and this is the one exception, paid for rather than waived: a Discord
  renderer cannot open a unix socket, so the listener binds `127.0.0.1` and
  **only** `127.0.0.1` (there is no config key for the interface — the only
  correct value is the hard-coded one, and every accepted peer's address is
  re-checked and hung up on if it is not loopback), and the 0600 unix socket's
  access control is replaced by a bearer token in a 0600 file at
  `<config dir>/truth.token`, printed by `recalld truth token` and overridable
  for tests with `NXR_TRUTH_TOKEN`.

  | route | body | replies |
  |---|---|---|
  | `POST /v1/discord/speaking` | NDJSON, `{t_ms, user_id, speaking, name, channel_id}` | `204` |
  | `POST /v1/discord/voice` | NDJSON, `{t_ms, ev: "join"\|"leave"\|"self", user_id, name, channel_id, self_mute?, self_deaf?}` | `204` |
  | `GET /v1/health` | — | `200 {ok, service, proto, spans, open}` |

  `401` without a `Bearer` token (health included; the body is never read),
  `413` over 1 MB, `204` for everything else — a route that does not exist yet
  is a newer plugin talking to an older daemon, and a fire-and-forget client
  cannot act on a `404`. `Access-Control-Allow-Origin` is `https://discord.com`
  and never `*`: the renderer posts from that origin, and `*` would let any
  page the user happens to have open write into their recordings.

  - **`t_ms` is `Date.now()`, and that is the right clock.** Segments carry
    UTC epoch nanoseconds (the pipeline derives them from a `clock::Anchor`,
    monotonic *within* a session but anchored to wall-clock UTC), so both
    sides of every comparison in this section are wall-clock UTC on one
    machine and nothing is converted. Comparing against a monotonic clock
    would have been silently wrong for the life of the feature, which is why
    it is written down rather than assumed.
  - **One malformed line does not fail a batch.** It is counted in
    `rejected` and skipped. Failing the batch would make the plugin retry it
    forever over one bad line, and the plugin retries on any non-2xx.
  - **A `leave` closes an open ring.** A client that vanishes mid-word sends
    no stop, and without this the span would run to the timeout and claim
    speech that did not happen.

- **Storage (schema v11).** `truth_speaking(id, user_id, name, channel_id,
  t_start_ns, t_end_ns)` — an **observation**, not an annotation: it hangs off
  no segment, because it arrives before the turn it will be compared with
  exists. `t_end_ns` is NULL between a start and its stop; a second start
  closes the first at its own timestamp (a dropped batch, not two mouths), and
  a row still open `[truth].open_span_timeout_s` (30 s) after it started is
  closed **at the timeout**, not at now — the last thing anybody actually
  knows is that they were talking when we lost them.
  `discord_users(user_id PRIMARY KEY, name, speaker_id, via, linked_at_ns,
  first_seen_ns, last_seen_ns)`. Four columns on `segments`
  (`truth_user_id`, `truth_verdict`, `truth_coverage`, `truth_enrol_ns`) and
  one on `speaker_prototypes` (`via`). Idempotent like every migration, and
  with no backfill: truth exists from the day the plugin starts sending it.

- **The verdict.** An idle pass (`crate::truth`, the worker/gating/lock
  discipline of `crate::quality` — gather under the store lock, judge with
  none held, commit under it again; never in the capture path) walks segments
  of a **Discord** session — matched on `[truth].sources`, lower-case
  substrings against the source's match key and display name, default
  `["discord", "vesktop"]`; VRChat is not Discord and never matches — and
  computes, per Discord user, `coverage(u) = overlap_ms / dur_ms`, that user's
  own overlapping spans merged first so coverage can never exceed 1 however
  the plugin's batches interleaved.

  | condition | `truth_verdict` | scored against |
  |---|---|---|
  | two or more users ≥ 0.2 | `overlap` | the overlap gate |
  | one user ≥ 0.8, nobody else ≥ 0.2 | `single` | the identity ladder |
  | one user in [0.2, 0.8), nobody else ≥ 0.2 | `partial` | nothing |
  | every user < 0.2, truth data within 5 min | `nobody` | nothing |
  | no truth data within 5 min | `unknown` | nothing |

  - **`partial` is a fifth verdict and it had to exist.** A VAD span whose
    edges run past the words is common. Folding it into `single` would
    quietly lower a bar that was set at 0.8 on purpose; folding it into
    `overlap` would claim a second voice that is not there. It is named,
    counted, and excluded from both scores.
  - **`nobody` and `unknown` are different facts.** `nobody` means truth
    covers this moment and says none of these accounts was talking — usually
    the local user on a mic Discord is not carrying, and worth reading when
    it is not, since the plugin reports the local user too. `unknown` means
    the plugin was not running. An `unknown` is **re-examined** if truth
    later covers it (a plugin started mid-call, a batch that finally
    flushed); every other verdict is written once and never re-read.
  - The local user is a Discord user like any other: their own `SPEAKING`
    arrives with their own id. The mic is still a separate source with a
    separate session, and nothing here changes how it is labelled.

- **Linking a user to a voice.** `truth.link {user_id, speaker_id}` /
  `truth.unlink {user_id}` → the user row; `truth.users` → `{users: [row]}`,
  where a row is `{user_id, name, speaker, speaker_name, via, linked_ms,
  first_seen_ms, last_seen_ms, agreement, segments}`. The daemon also links
  **automatically, and only when there is nothing to decide**: a Discord user
  whose `single` segments of ≥ 1 s were labelled by the voicebank as one
  speaker **≥ 90% of the time over ≥ 20 labelled segments** is linked with
  `via: "truth"`. 89% does not link — at that rate the minority voice is a
  merge somebody has to look at, not noise. Turns the ladder *declined* are
  not evidence either way and are in neither half of the fraction; a hand link
  (`via: "manual"`) is never overwritten. Event **`truth`** on topic `relabel`
  carries the same row shape on every link and unlink.
  - **Discord names are never applied to a voice.** `name` is a per-guild
    nickname somebody picked for a joke last Tuesday. It is recorded on the
    `discord_users` row and exposed so a client can **offer** it; naming a
    voice stays `speakers.name`, i.e. a decision a person makes.

- **The measurement, which is the point.** `truth.summary` →
  `{segments_labelled, single, overlap, partial, nobody, unknown,
  min_duration_ms, identity: {n, correct, wrong, unlabelled, precision,
  recall, by_speaker: [{speaker_id, user_id, n, correct, wrong}]},
  overlap_gate: {threshold, flagged_when_overlap, flagged_when_single,
  precision, recall}, caveat}`. `recalld truth report` prints it. This is what
  replaces the deferred labelling pass, so the numbers are built to be honest
  rather than flattering:
  - Identity is scored on `single` segments **of at least 1 s** belonging to a
    **linked** user, and on nothing else. A sub-second turn is a grunt the
    voicebank refuses anyway; scoring it would measure the floor rather than
    the model. `min_duration_ms` says so on every reply and `caveat` says it
    in words.
  - `correct` is a segment whose speaker is the one linked to its truth user,
    `wrong` is a different one, `unlabelled` is the ladder declining. So
    `precision = correct/(correct+wrong)` is over the turns it answered on and
    `recall = correct/n` is over every turn it was asked about. **Both**,
    because a ladder that answers rarely and rightly and one that answers
    always and often wrongly are different failures and one number hides it.
  - The overlap gate is scored the same way against `overlap` verdicts, with
    `threshold` the live `[identity].max_overlap` it was scored at.
  - Every ratio is `null` — never `0` — when there is nothing to divide. An
    untested gate has no precision, and reporting one as perfectly imprecise
    is a lie in the same family as reporting an unmeasured error rate as zero.
  - `unknown` counts the Discord turns no truth covers, verdict-stamped or
    not, so "the plugin was off for most of this" is visible in the report
    rather than hidden by a smaller denominator.

- **Enrolment from truth.** Behind `[truth] enrol = true` (off by default —
  it is the one thing here that changes future behaviour instead of merely
  measuring it). A `single` segment of ≥ 3 s with `truth_coverage ≥ 0.95`
  belonging to a **linked** user is enrolled into that voice's bank with
  `speaker_prototypes.via = "truth"` — **only if it also passes
  `identity::decide`'s existing four enrol conditions** against the bank as it
  stands. Ground truth says whose voice it is; it does not say the recording
  is worth keeping, and `identity.rs` is the only thing that has ever decided
  that. Every candidate is stamped `truth_enrol_ns` whether it enrolled or
  not, so a refused turn is not re-examined forever.

- **`truth.status`** → `{listening, enabled, port, label, enrol, sources,
  token_path, spans, open_spans, last_span_ms, users, linked, counters}`.
  `listening` is the address actually bound and `enabled` is the intention;
  they differ when the port was taken, and a client showing only the second
  would lie about a daemon that failed to bind.

## 0.9.0 — the night shift

A third reading of the day's *shaky* rows, decoded overnight on the GPU and
applied — if at all — by a vote. Everything in this section is additive: a
client that ignores it sees exactly the 0.8.x contract.

**Why only shaky rows.** `spike/FINDINGS.md` §11 refused to ship an ensemble
because three ASR models disagree with each other on about half of real lobby
sentences and nothing said which to believe. §12 found the exception: on rows
the cross-check already calls `shaky`, whisper-large-v3 disagrees with the live
text 76% of the time against 27% on `solid` rows. The night shift is scoped to
exactly that bucket, and to nothing else.

### Segment rows and events

- **`night_text`** (string or `null`) — what the overnight decoder read.
  Present on any row the night shift has reached, **whether or not the vote
  replaced anything**, and `null` everywhere else. It is an annotation, not a
  transcript: a client shows it beside the words ("the night shift read: …"),
  never instead of them.
- **`text_via`** gains a fifth value, **`"night"`**. It appears only where the
  vote actually replaced the words, and then `night_text` carries the same
  string. `"live"`, `"context"`, `"arbiter"` and `null` are unchanged.

A replacement re-publishes the segment event and follows the 0.8.1 rules
without exception: an `operations` row `op: "segments.redecode"` with
`prior_state: {segment_id, text, asr_model_id, text_via}`, the invalidated
`asr_confidence` **cleared** (the verdict was about the words that are gone),
and it is emphatically not a `segments.correct` — machine edits and people's
edits stay distinguishable.

An annotation changes nothing else at all: `text`, `text_via` and
`asr_confidence` all stand, and no `operations` row is written. A client can
therefore treat `night_text != null && text_via != "night"` as "a second
opinion exists and was not acted on", which is the state most rows are in.

### `status.asr.night`

Always present, always this shape — "the GPU decoder is not built" and "an
older daemon" have to be tellable apart, and a missing key says neither:

```json
"night": {
  "enabled": false, "available": false,
  "window": "03:00-07:00", "gpu_busy_max_pct": 20, "replace": true,
  "how": "the night shift is not installed. `recalld models fetch --night` …",
  "phase": "off", "replaced": 0, "annotated": 0,
  "skipped_busy": 0, "last_run_ms": 0
}
```

- `phase` — `off` (`[night].enabled` is false, the shipped state), `unavailable`
  (the model, the built runtime or the cross-check decoder is missing),
  `blocked` (a gate is shut: the clock, a pause, or a busy GPU), `idle` (nothing
  left to read), `running`.
- `replaced` / `annotated` — rows this daemon has rewritten and rows it has only
  annotated, since start-up.
- `skipped_busy` — batches not started because the GPU was over
  `gpu_busy_max_pct`. A number that keeps climbing is not a fault; it is the
  feature standing out of the way.
- `last_run_ms` — wall time of the last batch.
- `how` — non-null exactly when `available` is false, and it names both halves
  of the install, because they are fetched differently (see below).

### Two things a client should say out loud

- **The runtime is compiled, not downloaded.** `recalld models fetch --night`
  brings the GGML model (~1.03 GB); `recalld models build-night` clones
  whisper.cpp at a pinned tag into the models directory and **builds** it, which
  needs git, cmake, a C++ compiler and a GPU backend's headers. No GPU-capable
  `whisper-cli` is published for an AMD card, so there is nothing to download.
  This is the only asset in the program that behaves this way and a UI that
  presents it as a download will be wrong for fifteen minutes.
- **The night shift is off, and it is off for two independent reasons.** The
  model is a gigabyte nothing else needs, and the GPU it wants is the one
  drawing the user's frames. `[night]` carries the hours (`window`), an idle
  fallback (`also_when_idle_min`), the GPU ceiling (`gpu_busy_max_pct`), a
  nightly row budget, and `replace` — the switch between "the night shift may
  rewrite a row" and "it may only annotate one". `replace` ships **true**
  because the rule below was measured (FINDINGS §13: 91.1% → 44.3% word error
  on shaky lab spans, 51.4% relative, none of the 20 touched rows made worse,
  against a gate of ≥30% relative and under 5% harmed). Turning it off is a
  supported choice and leaves every `night_text` in place.

### The vote, stated for clients

The daemon never replaces a transcript on the night decoder's word alone. Two
of the three readings — live (parakeet v3), cross-check (canary 180m), night
(whisper-large-v3) — must agree with each other *and* disagree with the stored
words, and the winner must then clear the arbiter's guards: caption text
stripped, at least two words, and a language that matches the row's — checked
by the daemon's own classifier **and** by requiring a function word of that
language, because "Tack för att ni tittade" is Swedish, has an ö in it, and
would otherwise classify as German. That last clause is not hypothetical: it is
a string large-v3 produced on this user's German audio (§12), and a vote that
replaced a bad German transcript with a good Swedish one would have made the
row worse in the most convincing possible way.
## 0.9.0 — the assistant

Three things the daemon does *for* you rather than *to* the recording, and one
new method between them. Everything here is additive: a client that ignores all
of it behaves exactly as it did against 0.8.2.

`status` gains `assist: {reminders, digest, translate_to}` so a client can tell
"switched off" from "an older daemon", and five counters under `counters`:
`digests_written`, `digests_refused`, `translated`, `translation_declined`,
`reminders_fired`. Schema goes to **v11** (two columns on `notes`, two on
`segments`, one `digests` table; no backfill of any of it).

### Reminders that fire

A note whose words carry a time reference that was **still in the future when
they were said** gets a due date, resolved by the same Tier 2 parser
(`crate::timeref`) against the segment's own capture time — the same clock a
commitment's due date uses. There is no model in this path and no second parser.

Wake phrases gain three markers, because reminders need the verb that means
them: **`recall, erinner mich`**, **`recall, erinnere mich`**, **`recall, remind
me`**, alongside 0.8.0's four. `me` is two letters and is matched exactly.

`timeref` goes to **version 2**: clock hours spelled as words are read (`um
zehn`, `at ten`, `zehn Uhr`, `um ein Uhr`). A number word is only a time with a
preposition in front of it or a unit behind it — `ich hab drei Sachen offen` is
three things. The version is bumped rather than absorbed because a row written
by version 1 genuinely could not carry those hours.

Note objects (`notes.list`, the `note` event) gain four fields:

| field | meaning |
|---|---|
| `due_ms` / `due_ns` | when it asked to come back, or `null` — which is most notes |
| `fired` | whether the reminder has been announced. **Not "done"**: a reminder that has gone off is still an open note |
| `fired_ms` | when it was announced, or `null` |

New event **`reminder`** (topic `segments`) → `{note_id, text, due_ms, due_ns,
segment_id, t_ms}`. It is the one event in this protocol a client is expected to
**interrupt somebody with**. The `note` it is about is published immediately
after it carrying `fired: true`, so a list already on screen repaints from that
rather than re-querying — the reminder is the alarm, the note is the row.

A note fires **exactly once**. `notes.fired_at_ns` is the whole state machine and
the write that sets it only matches a row that is still unfired, so two
overlapping ticks announce it once between them. Two things put a note back on
the list, both deliberate: a snooze, and a re-decode that **changed the words** —
the words are the date, so a reminder that fired for the sentence before has not
fired for this one.

`notes.set_state` gains an optional **`snooze_min`** (1–10080). It is only valid
with `state: "open"` and is refused otherwise, because "done, but remind me
again" is a contradiction rather than something to half-honour. On a note with
no date at all it **gives** it one, which is the only way to ask to be reminded
of something you said without a time in it. The reply and the `note` broadcast
are the same as any other state change.

The scheduler does **not** stand down while capture is paused, and it is the one
background worker that does not. Pause means nothing new is written down; a
reminder writes nothing about what is being said, it delivers something you
already said.

### The daily digest

An idle worker summarises conversations that have settled: ended at least
`[assist] digest_settle_min` (30) minutes ago, at least `digest_min_turns` (8)
turns with words in them, and no digest yet. It runs behind `crate::enrich`'s
gates plus one of its own — **the enrichment queue comes first**, because a
commitment is what somebody is waiting on and a paragraph about last night is
not. Same model, same jail, same lock discipline (model time and store-lock time
never overlap).

**`digest.list {day?, limit?}`** → `{day, total, digests: [...]}`, newest
conversation first. `day` is a local calendar day, `"YYYY-MM-DD"`; a string that
is not one is refused rather than silently matching nothing. Each digest is:

```json
{"thread_id": 12, "day": "2026-09-02", "lang": "de",
 "summary": "…one paragraph…", "open": ["B schickt A morgen den Link"],
 "participants": [{"speaker_id": 3, "label": "Aspen"}],
 "started_ms": …, "started_ns": "…", "ended_ms": …, "ended_ns": "…",
 "turns": 12, "model_id": "qwen2.5-3b-instruct-q4_k_m@1", "created_ms": …}
```

New event **`digest`** (topic `segments`) carries the same object. It is only
ever new — one digest per conversation, ever — so a client unshifts rather than
reconciling. A conversation that resumes after its digest was written keeps the
one it has; the alternative is a paragraph that changes under a reader.

**Conversations the model declined are not listed.** A refusal is written down
(so the worker stops asking) and is invisible to clients, which is why
`digests_refused` is a counter and not a row. It is not a failure: eight turns of
"ja / ne / lol" is a conversation by the threading rule and nothing worth a
paragraph.

CLI: `recalld digest [day]`, where DAY may also be `today` or `yesterday`.

#### The measurement, and what it forced

`spike/digest_bench` — six traps in the shapes a lobby actually produces
(backchannel in both languages, greetings, round callouts, agreement, a
microphone check) and four real conversations, on four pinned cores at nice 19.

One model call that decided *and* wrote degraded **monotonically** as the prompt
grew:

| prompt | traps refused | right language | open list right |
|---|---:|---:|---:|
| short, no language steering | **6/6** | 1/4 | 1/4 |
| + an `open` clause and a language order | 5/6 | 2/4 | 4/4 |
| + a worked trap example as well | 1/6 | 3/4 | 4/4 |

Every clause that made the summary better made the refusal worse. So the verdict
gets **its own call**, with a grammar that cannot express a summary at all, and
the summary — which only runs for a conversation that passed — gets all the
steering it wants. **Shipped: 6/6 traps refused, 4/4 conversations summarised,
3/4 in the right language, 3.4 s median.** A trap costs one short call instead of
one long one, so the split is also cheaper (3.4 s against 11.7 s).

The one language miss is the deliberately bilingual conversation, where the
daemon asks for the reader's language and the model answers in the dialogue's.

### Translation for turns you cannot read

`[assist] translate_to` (empty by default, and empty is off) names the language
the reader has. An idle pass translates committed turns whose `lang` is stamped,
is not `translate_to`, and is not one of the user's own speaker languages —
those are skipped **without a model call at all**. Turns under
`translate_min_words` (3) are skipped too.

Segment rows and events gain **`translation`**:

```json
"translation": {"lang": "de", "text": "…", "via": "qwen2.5-3b-instruct-q4_k_m@1"}
```

or `null`. An **object** and not a bare string, because a client showing a
translation has to be able to say which language it is in and which model wrote
it. `null` is the answer for every row until the pass has looked, for every row
already in that language, and on every machine where `translate_to` is empty —
it is *not* "this turn needs no translation".

Three guards, and they are the feature:

- The grammar admits **one field**. The model cannot explain, answer the line,
  or comment on it.
- **An echo is dropped.** A model that hands its input back has not translated
  it, and a row claiming otherwise would tell a reader the sentence was already
  in their language.
- **A wrong-language answer is dropped.** The stopword classifier reads what came
  back; a clear reading of the wrong language is discarded. "I could not tell" is
  *not* a rejection — a three-word answer often votes for nothing, and dropping
  those would lose the short turns this is most useful on.

A declined turn is **marked** (`translation_via` set, `translation` NULL) so the
queue stays finite. A re-decode that changes the words clears both, putting the
row back at the end of the queue.

Measured (`spike/translate_bench.py`): 20 FLEURS sentence ids present in both
`en_us` and `de_de` — FLEURS is parallel, so the German reference is a human's.
Scored by cosine in the multilingual-e5-small space the daemon already uses.

| | cosine |
|---|---:|
| two *different* German FLEURS sentences (the floor) | 0.792 |
| the English input against the German reference (passthrough) | 0.906 |
| **qwen2.5-3b's German against the German reference** | **0.948** |

Gate was ≥ 0.80 and it passes. Read the floor row before the headline: e5 is
multilingual and its space is crowded, so 0.80 sits barely above the bottom, and
the number that actually says the feature works is the **0.906 → 0.948** gap — a
real translation beats simply showing the untranslated line.

### Correction-driven glossary re-read — measured, NOT shipped

§12 gate 2 rejected biasing the transducer toward the whole vocabulary (+9.1%
relative recall against a +20% bar). The narrow version was specified and
measured: when a correction introduces a word W, re-decode **only** the other
turns whose live text holds a word within edit distance 2 of W, with W as the
single hotword.

`spike/glossary_reread_bench.py` — LibriSpeech dev-clean through Opus 24k, 2 987
neighbour utterances surveyed with the shipped decoder, the 40 words it actually
gets wrong taken as the planted corrections, 70 utterances tripping the
near-miss filter:

| configuration | target recall | target WER |
|---|---:|---:|
| greedy — what ships | 47.1% | 5.1% |
| `modified_beam_search`, no hotword | 50.0% | 5.0% |
| + the one corrected word @1.5 | 50.0% | 4.9% |

**+6.1% relative against the live text, and +0.0% against `modified_beam_search`
alone.** The entire gain is the decoder change; the hotword contributes nothing
on the candidate set. Gate was +30%. **Not shipped**, and there is no
`text_via: "glossary"` route.

Two things worth recording, because they are the expensive part to rediscover.
The first bench built its word list by rarity alone and measured 98.4% recall in
every configuration — the shipped decoder already got those words right, so the
gate was a tautology; a glossary is for words a decoder is *wrong* about, and
selecting on that is a step the bench cannot skip. And this was **not** a binding
limitation: `crate::asr::TimedAsr` already drives sherpa's C API directly and
sets `hotwords_file`, `modeling_unit` and `bpe_vocab`, so with §12's synthesised
`bpe.vocab` the Rust side could have done this safely. It was not worth doing.

## 0.9.2 — `replay.get`: a conversation, played back

Replay plays a thread's turns in order with the transcript reading along. The
audio for it still comes from `segments.audio`, one turn at a time; this method
exists for everything a client has to know **before** it fetches a single byte.

- **`replay.get {thread}`** → `{thread, turns: [...]}`, in time order:

  ```json
  {"thread": 502,
   "turns": [{"id": 1010, "t_ms": 1756670000000, "t_ns": "1756670000000000000",
              "dur_ms": 4930, "speaker": 5, "speaker_name": "Speaker_31",
              "text": "my mic keeps cutting out, is it better now?",
              "has_audio": false}]}
  ```

  Field conventions, and they are the ones already in force everywhere else:
  `speaker` is the **id**, because names change and ids do not, and
  `speaker_name` is the resolved convenience beside it (`null` on an unlabelled
  turn, and following merges through `speaker_resolved`). `t_ns` is a string
  like every nanosecond value on the wire; `t_ms` and `dur_ms` are what the
  scrubber does arithmetic on. An unknown id — and a conversation whose every
  turn has been deleted, which is the same thing to a client holding a link to
  it — is `err:not_found`, the same answer `thread.get` gives.

- **`has_audio` means the file is on disk right now, and this is the only place
  on the wire where it does.** The ordinary segment shape (`thread.get`,
  `transcript`, `search`) also carries a `has_audio`, and there it means
  `audio_path != ''` — *the database still names a file*. That is a weaker
  claim, and the gap between the two is not hypothetical: `retention`'s
  reconcile pass counts rows whose file has vanished (`dangling_paths`) and
  deliberately leaves them alone, because the transcript is kept when the
  recording is not. A player built on the weaker flag draws a playable mark
  over a turn that answers `err:gone` the moment it is asked for. This method
  stats the file — one `stat` per turn, over a conversation that is tens of
  turns long, against the round trip per turn it saves.

- **It is a snapshot, and a client must not treat it as a promise.** Retention
  can sweep between this reply and the fetch, so `err:gone` from
  `segments.audio` stays an ordinary answer that a player handles by reading
  through the turn — `has_audio` only decides what is drawn before anybody
  presses anything.

### Why this is not `thread.get`

`thread.get` already returns the whole conversation, in the ordinary segment
shape, in one round trip, and reusing it was the first thing tried. Two reasons
it is the wrong query for this:

1. The `has_audio` it carries is the weaker one above, which is precisely the
   field a player is built on.
2. It carries the whole segment — translations, `night_text`, every provenance
   field — for a client that wants six values per turn to draw a bar with.

Nothing about `thread.get` changes; it remains the way to *render* a
conversation, and a client that only wants to read one should keep using it.

### What a client is expected to do with it

Stated because the daemon's half is small and the contract is mostly about the
client's:

- **A turn with no audio is read through, not skipped.** Hold it for its
  `dur_ms` at the current rate. The words are the record and the sound is the
  perishable copy of it; racing through the parts of an evening that can no
  longer be heard is backwards.
- **Say that once per conversation, not once per turn.** The count is
  `turns.filter(t => !t.has_audio).length` and it is one sentence in the
  player, not a badge on every row it is true of.
- **Compress the gaps.** Real silence between turns runs to tens of seconds
  (threads are cut at `[graph].thread_gap_s`, 20 s, so anything shorter than
  that can appear inside one). The desktop client caps a gap at 700 ms.
- **One sound at a time.** `segments.audio` is also the per-segment preview,
  and a client that can play both has to make them share one owner: a preview
  starting stops a replay, and a replay starting stops a preview.

`mock/mockd.js` implements the method with the same shape, and its thread 502
mixes turns that still sound with one whose audio has aged out — a client that
never meets a mixed conversation never renders one.
## 0.10.0 — worlds and turn-taking

Two additions, both of which are memory rather than capture: **where** a
conversation happened, and **how** the people in it took turns. Nothing here
needs a model, a network or a download; both halves are queries over rows the
daemon already had.

Schema **v12**. Additive and idempotent like every migration since v9: one
table (`visits`) and one column (`threads.world_id`), read by nothing that
already existed. A 0.9.x daemon opening a v12 database refuses it, as it always
has; a 0.10.0 daemon opening a v11 one migrates in place.

### Worlds

`roster.rs` has parsed VRChat's `Joining wrld_…:12345~region(eu)` and
`Entering Room: <name>` lines since Step 4, and until now it threw both away
after stamping a roster row. They are now kept:

- **`visits(id, session_id, world_id, world_name, instance_id, t_start_ns,
  t_end_ns)`.** One row per world entry. `t_end_ns` is NULL until the next
  entry — or the daemon's shutdown — closes it. `session_id` is nullable and
  frequently null: the roster comes from a log file that knows nothing about
  capture, and a world entered while nothing was being recorded is still a
  visit. `world_name` is nullable because the name arrives on a *separate* log
  line a moment after the id, and sometimes never arrives; a world with no name
  renders as its id.
- **`threads.world_id`.** The visit that was open when the conversation's first
  turn happened, stamped once at `create_thread` and never revisited. A
  conversation that ran across a world change belongs to the world it started
  in — a thread with two places is not a thing anybody can be shown. NULL for
  everything that is not VRChat: a Discord call and a microphone-only session
  happen nowhere, and saying so is more useful than naming the last world the
  user was in.

**Instances are recorded and never grouped on.** "The Great Pug" is a place a
person remembers; `12345~region(eu)` is a lobby number that changes every time
the door opens.

#### Backfill — what was possible, and what was not

There is **no backfill from `segments`**. Inventing a visit for a conversation
that predates the table would be a claim about where somebody was, which is the
kind of guess the tier boundary exists to prevent.

What *is* recovered is whatever the evidence still on disk supports: on every
start the roster tailer reads every `output_log_*.txt` in its log directories,
writes each world entry it finds (idempotent on `(world_id, t_start_ns)`, so
this is unconditional and free on the second run), and then stamps
`threads.world_id` on conversations that have none from the visits it just
learned. VRChat rotates and prunes those logs, so in practice that reaches back
days, not months — and a database older than the surviving logs keeps threads
with no world for ever. **Nothing fills those in.**

#### Methods

- **`person.get`** gains `worlds: [{world_id, name, visits, last_ms, last_ns,
  minutes_together, together_ms}]`, top 8 by time. `minutes_together` is the
  wall-clock length of the conversations that person took part in *there* — not
  how long they were in the world, which the daemon does not know. It knows
  when **it** was in a world and when a **voice** was talking; the conversation
  is the honest intersection.
- **`worlds.list {limit?}`** → `{total, worlds: [{world_id, name, visits,
  last_ms, last_ns, people: [{speaker_id, label}], topics: [...]}]}`, newest
  visit first. `topics` is Tier 2 output (`threads.topic`) and is an empty list
  on a machine that has never run enrichment, which is most of them.
- **`thread.get`** gains `world: {world_id, name} | null`.
- **`digest.list`** rows gain `world` (the id, or null).

#### The `world` facet

`search`, `search.semantic` and `search.ask` all accept `world`. It is either
an id (`wrld_…`, matched exactly) or a **case-insensitive substring of a
name** ("pug"). A facet matching no known world selects **nothing** — never
everything: an unmatched facet quietly widening to "everywhere" would answer a
question nobody asked.

`search.ask` learns the phrase, in both languages: `in <world>`,
`in the <world> world`, `in der <world> Welt`. The interpretation gains
`world_id` and `world_label`, and the pill is removable like every other. Three
rules keep it from eating questions:

- **The preposition is mandatory.** A world name is free text and can contain
  anybody's name; "who mentioned the Great Pug" is a search, not a filter.
- **Longest name wins**, so a world called "Pug" cannot steal a question about
  "The Great Pug".
- **A German article may stand in for the world's own.** "in der Great Pug
  Welt" resolves to *The Great Pug*, and the trailing "Welt"/"world" is
  swallowed rather than left in the query as a word nobody said.

#### Events

A world entry now publishes a second event on topic `roster`, named `visit`:
`{world_id, instance, name, t}`. It is deliberately not a rename of the `roster`
`world`/`room` events — `roster` says who is present, `visit` says a place was
entered — and `name` is whatever is known *at that moment*: null on the entry,
filled in on the room line a second later.

### Turn-taking

`person.stats {id, days?}` and a `stats` block on `thread.get`. Every number is
a query over turns that already exist; nothing is stored and nothing is
cached, so a deleted segment stops counting the moment it is deleted.

Four of the six are **definitions**, not measurements, and they ship with their
definitions attached — `person.stats` returns a `definitions` object and
`recalld stats` prints it under the numbers. The definitions are part of the
contract:

| field | definition |
|---|---|
| `share` | their speech nanoseconds over the speech nanoseconds of **every identified voice** in the same conversations. Unlabelled turns count towards neither side. |
| `mean_turn_ms` | their speech over their turn count. |
| `longest_monologue_ms` | the longest unbroken run of their turns inside one conversation, from the run's first start to its last end — so the pauses *inside* a monologue count towards it. Broken by any turn of another identified voice; an unlabelled turn does not break it. |
| `interruptions_given` / `_received` | see below. |
| `median_latency_ms` | see below. `null`, never `0`, when they never answered anybody inside the cap. |
| `turns_per_minute` | their turn count over the summed wall-clock length of the conversations considered. |

**Interruption, stated honestly.** Turn B interrupts turn A when, in the same
conversation, (1) A and B have different identified speakers, (2) B starts
strictly inside A — `A.t_start < B.t_start < A.t_end` — and (3) B's
`overlap_frac` is at or above **0.10**, the same line above which `identity`
refuses to put a name to a voice.

This is an approximation, and the way it fails is worth stating. `overlap_frac`
is a property of B's own audio — the share of B's speech frames in which the
segmentation model heard two people — and **it does not name the second
voice**. Condition (2) supplies the name, from the clock, and the clock cannot
tell a genuine interruption from a back-channel "mhm" or from two people
starting a sentence at once. Condition (3) is what stops every clock
coincidence counting: a turn that merely *begins* while another runs, with no
overlapped speech in it at all, is two microphones being generous about a
boundary. The count is a floor on rudeness and a ceiling on nothing.

**Response latency.** For each of their turns whose immediately preceding
*identified* turn belongs to somebody else: the gap from that turn's end to
theirs, and the reported figure is the **median**. A negative gap is an
overlap, not a response, and a gap over **5 000 ms** is a lull that happened to
end with them speaking. Both are **dropped rather than clamped** — clamping
would let a silent hour vote for "5 s" and drag the median towards a number
nobody experienced.

#### Methods

- **`person.stats {id, days?}`** → `{id, days, from_ms, turns, speech_ms,
  conversation_speech_ms, share, mean_turn_ms, longest_monologue_ms,
  interruptions_given, interruptions_received, median_latency_ms, span_ms,
  turns_per_minute, definitions, by_conversation}`. `by_conversation` is the
  last 10 as `[{thread_id, share, turns, last_ms}]`. `days` must be positive;
  a voice that does not exist is `err:not_found`.
- **`thread.get`** gains `stats: {shares: [{speaker_id, turns, share,
  speech_ms}]}`, most talkative first. `share` is **speech time, not turn
  count**: two people take the same number of turns and one of them talks four
  times as long, and it is the second fact a person recognises.
- **`digest.list`** participants gain `share` and `turns`, so the Memory
  digest card can draw a share bar without a second round trip. Attached to the
  participant rather than offered as a parallel list, because a client that has
  to join two arrays to draw one bar will eventually join them wrong.

### CLI

- `recalld worlds [--limit N]` — every place a conversation has happened, with
  who is there and what gets talked about.
- `recalld stats <speaker_id> [--days N]` — the numbers above, with the
  interruption and latency definitions printed underneath them.
