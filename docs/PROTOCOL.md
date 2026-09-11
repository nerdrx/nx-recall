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
{"welcome": {"proto": 1, "daemon": "recalld/0.11", "seq": 41823, "schema": 15, "boot": "18f3c0a1d4b2e900"}}
```

`schema` is the **database** version, not the protocol one, and it moves far more
often: `proto` is still 1 while `schema` has reached **15** (0.12.0, the
highlight — see "Highlighting a person" below). A client must not gate on it. It
is there so a person reading a bug report can tell which shape the rows on that
machine have, and so a client that knows about a specific migration can say
"this daemon is older than the thing you are asking for" instead of rendering a
silently missing field as an empty one.

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
| `speakers.list` | | id, name, counts, total time, `languages`, `colour`/`icon` |
| `speakers.name` | `{id, name}` | retroactive; broadcasts `relabel`. On a **merge tombstone**: `err:conflict` naming the canonical voice (0.7.5) — it holds no rows, so the write would land nowhere while the reply and the event claimed otherwise |
| `speakers.set_languages` | `{id, languages}` | which languages this voice speaks; broadcasts `relabel`. Same `err:conflict` on a tombstone (0.7.5), and for a sharper reason: the read resolved through the tombstone while the write did not |
| `speakers.set` | `{id, colour?, icon?}` | pin a highlight to a voice (0.12.0, schema 15) — a palette **token** and a short emoji; broadcasts `relabel` carrying both plus the name. An **omitted** key leaves that half alone, an explicit `null` clears it; neither key is `err:params`, not "clear both". Same `err:conflict` on a tombstone, for the same sharper reason as `speakers.set_languages` |
| `speakers.palette` | | the ten accent tokens this daemon paints: `{palette: [{token, hue, hex}]}`. Served rather than assumed, so an eleventh colour does not need a matching client release |
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

### `search`'s query is words, never FTS5 syntax (2026-09-05)

`q` is matched literally, word by word — never as `segments_fts`'s own MATCH
query language. Before this it was passed straight through: a hyphenated
compound (`escape-menü`) or an English contraction (`what's`) reads to FTS5's
tokenizer as a column filter or an unterminated string literal, and the whole
request failed with a raw SQL error rather than a search result. `search.semantic`'s
hybrid leg already treated that failure as "the keyword leg found nothing" and
kept going on the vector leg alone (`Service::search_semantic`); plain `search`
now gets the same literal-words reading up front, by quoting each token, so
neither leg can be handed a query with syntax in it. Found and fixed measuring
`search` against 150 real, LLM-generated queries — see FINDINGS §51.

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
| `flapping` | (0.13.0) the node just left the graph, but the daemon is holding the session open and waiting to see whether the same source reappears within `[capture].flap_grace_ms` — see "Flap tolerance" below |
| `gone` | the node left the graph — the application closed its stream or quit, or a flap's grace window ran out with nothing reappearing |

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
- `speakers.list` rows also carry **`colour`** and **`icon`** (0.12.0, schema 15):
  the highlight a person pinned to that voice, or `null` — which is nearly every
  voice, and is the whole of "not highlighted". `colour` is a palette **token**
  (`"violet"`, `"teal"`, …), never a hex; see "Highlighting a person" for why. Both keys
  are always present, so a client spells them unconditionally the way it does for
  `name`.
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

### Flap tolerance and the stereo probe (0.13.0)

**The problem.** A source's PipeWire node can vanish and reappear inside a few
milliseconds — measured on the live box on 2026-09-05: VRChat ran 16:07–16:19
UTC and the tap logged 37 "capturing … key=VRChat.exe" lines, each on a new
`pw_target` (a fresh `object.serial` every time — device init, a menu
transition, a world load all recreate the playback stream), each one followed
milliseconds later by "source went away; closing session". Before 0.13.0 every
one of those was a hard stop: the session closed, whatever turn was mid-word
was discarded, and the next reconnect opened a brand-new session. Nineteen
sessions and one written segment came out of twelve minutes that was, from the
person's side, an unbroken conversation.

**The fix.** When a captured source's node disappears, the daemon does not
close its session immediately. It waits up to `[capture].flap_grace_ms`
(default 5000; `0` disables the feature and restores the pre-0.13.0 behaviour)
for a node with the **same match key** — and the same `application.process.id`,
when both the old and the new node carry one — to reappear. If it does, the
SAME session id continues: the existing stream is reattached, and the audio
gap is reported to the pipeline as a marker rather than a new session. The
turn that was open across the flap is kept if the gap was under
`[vad].min_silence_ms` (the ordinary silence-join threshold — a flap that
short reads exactly like a breath); at or above it, the turn in progress is
discarded the same way any other capture gap discards one, but the session
itself stays open. If nothing reappears before the grace window elapses, the
session closes for real, exactly as it always did.

A source's node is matched by `application.process.id`, never by
`object.serial` — a flap is by definition a *new* node, so it always carries a
new serial, and matching on that would refuse every flap the feature exists to
absorb. A node with no pid at all (some Flatpak and remote-desktop clients)
still gets flap tolerance: an unknown pid on either side of the gap is treated
as a match rather than a refusal.

Every flap absorbed into an existing session bumps `status.capture.
flaps_absorbed` by one. The daemon logs **one line per flap burst**, not one
per flap — a burst is however many flaps happen back to back before the source
either settles (stays up through a full grace window) or finally gives up —
because nineteen individual "flap absorbed" lines would be exactly the noise
this feature is trying to get out of the transcript.

`status` carries the block:

```json
"capture": {
  "flap_grace_ms": 5000,
  "flaps_absorbed": 18,
  "stereo_probe": {
    "enabled": true,
    "last": {
      "date": "2026-09-05", "wav_path": "probes/vrchat-stereo-2026-09-05.wav",
      "report_path": "probes/vrchat-stereo-2026-09-05.txt",
      "duration_s": 47.2, "windows": 6, "usable_for_azimuth": true,
      "summary": "6 window(s), ILD spread 4.1 dB, ITD spread 210.3 us — turns cluster into distinct positions; the audio carries usable azimuth"
    }
  }
}
```

`stereo_probe.last` is `null` until a probe has actually run — the same
"measured, not assumed" discipline as `storage`; a zeroed block would claim a
recording that never happened.

**The stereo probe.** VRChat is stereo and spatialised, so a voice's left/right
balance and its tiny inter-channel arrival delay were flagged in the Step 0
spike as a candidate second identity signal ("azimuth") — and then parked,
because the spike never had VRChat running long enough to capture a stereo
sample of it, and every capture this daemon makes deliberately downmixes to
mono before a single sample is measured. `[capture].stereo_probe` (default
`true`) answers the question the feature has been blocked on, without building
azimuth identity itself: once a day, the first time a `VRChat.exe` source is
seen, the daemon opens a **second**, independent stream on the same node,
asking for the native channel count instead of the downmix, and records up to
60 seconds of it to `<data-dir>/probes/vrchat-stereo-<date>.wav`. In the same
pass it measures the inter-channel level difference (ILD, dB) and
inter-channel time difference (ITD, µs) of every VAD-active window in the
recording and writes a short plain-text report beside the wav, and the
`usable_for_azimuth` verdict — do the per-turn measurements cluster into
distinct positions, or smear around the centre — is what `status` carries.
Bounded three ways: 60 seconds, once a day, one named application; it is a
diagnostic recording answering one question, never a standing stereo capture.

## 0.14.0 — capture health

Every "audio gap: discarded the turn in progress" warning used to be exactly
that — a warning, with a session id and nothing else. Finding out *why* meant
reading the journal by hand and guessing from what else was running at the
time (FINDINGS §50: 723 of them in one 26-hour window, reconstructed after
the fact, effectively 100% unclassified because nothing recorded the
evidence at the moment it existed). 0.14.0 classifies each one as it happens
and writes it to a `gaps` table (schema v20) instead.

**Causes**, in `crate::pipeline::GapCause`:

| cause | what it means | the fix |
|---|---|---|
| `flap` | a source's node vanished and came back at or above `[vad].min_silence_ms` (§47) | raise `[capture].flap_grace_ms` if this app reconnects slower than the default 5 s |
| `queue_overflow` | the shared capture→VAD queue was over its sample budget and evicted buffers between this session's chunks | raise `[capture].queue_seconds`, or free a CPU core for the inference thread |
| `scheduler_starvation` | the gap's own clock arithmetic shows a stall with no matching queue eviction — the capture thread did not get a buffer to PipeWire's `process` callback in time | check `[runtime].inference_nice` / `inference_cpus` and what else is pinned to those cores |
| `pipewire_xrun` | reserved for PipeWire's own xrun counter | not populated today — `pipewire-rs` 0.10 does not expose it, so a real xrun currently reads as `scheduler_starvation` |
| `session_end` | the capture side went quiet for more than a second before the daemon saw the node disappear | usually a clean app exit; only worth chasing if it repeats mid-session |
| `unexplained` | the classifier could not attribute the gap | should not occur; a nonzero share means the classifier missed a case |

A gap is classified with whatever evidence exists **at the instant it is
discarded** — the queue's own eviction counter, a per-session timestamp of
the last buffer seen — never reconstructed afterward. There is no backfill: a
gap from before this build has no row, because the evidence it needs was
never captured.

`status` carries the block, alongside `flaps_absorbed`:

```json
"capture": {
  "flap_grace_ms": 5000,
  "flaps_absorbed": 18,
  "stereo_probe": { "...": "..." },
  "health": {
    "since_utc_ns": 1788500000000000000,
    "total": 41,
    "by_cause": {"flap": 2, "queue_overflow": 6, "scheduler_starvation": 33},
    "unexplained_share": 0.0,
    "per_hour": [
      {"hour_start_utc_ns": 1788541200000000000, "total": 9,
       "by_cause": {"scheduler_starvation": 9}}
    ],
    "top_sources": [
      {"display_name": "vesktop", "match_key": "vesktop", "count": 18}
    ]
  }
}
```

`health` covers the trailing 24 hours and is always present, like every other
capture block: zero gaps and "an older daemon" must never look the same.
`unexplained_share` is the number worth watching — it is 0.0 on a build where
the classifier covers every reanchor and flap discard, and any drift away
from that is a bug.

`recalld capture health [--days N]` reads the `gaps` table directly (no
running daemon needed, like `recalld speakers`), over a widened window, and
prints the same numbers plus the top offending sources and the top offending
hours — the report behind FINDINGS §50's baseline.

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

### Highlighting a person (schema 15, 0.12.0)

Two nullable columns on a voice — `colour` and `icon` — and a method that sets
them. A person picks somebody out of a wall of names; every surface that draws
that name paints it in their colour and puts their emoji in front of it.

**`speakers.set {id, colour?, icon?}`** → `{id, colour, icon, seq}`. Broadcasts
`relabel` carrying `{speaker, name, colour, icon}` — the name rides along for the
same reason `speakers.set_languages` carries it: a client folds one shape into
its speaker row and must never be made to choose between applying the highlight
and keeping the name.

**`speakers.palette`** → `{palette: [{token, hue, hex}]}`.

#### Why a token and not a colour

`colour` is one of ten **token names**, never a `#rrggbb`. This is the load-
bearing decision in the whole feature, and it is about legibility rather than
tidiness.

A highlight is read on two grounds — NX Clear is light *and* dark (DESIGN §14.1)
— and in three renderers, one of which is the headset overlay
(`crates/nx-recall-overlay`), which rasterises glyphs itself and has no CSS, no
stylesheet and no theme to resolve a colour against. A free-form hex would let
somebody pin `#111111` to a friend and lose that name entirely on the dark
ground, and **the daemon could not warn them, because the daemon does not know
which ground anybody is looking at.**

So what is stored is a token, and each token is one *hue*. Saturation and
lightness belong to the surface: the desktop spends `--sp-s`/`--sp-l` (72%/28%
light, 72%/74% dark), which are the same two numbers every *unhighlighted* voice
is already painted with, and the overlay hard-codes the dark pair because that
surface is always dark. A highlight therefore changes **which** hue a name wears
and never how readable it is — it is painted by the same machinery, at the same
measured contrast, as the automatic colour it replaces.

The ten, in picker order, with the hue each is painted at:

| token | hue | | token | hue |
|---|---|---|---|---|
| `violet` | 268 | | `lime` | 92 |
| `indigo` | 232 | | `amber` | 44 |
| `cyan` | 192 | | `orange` | 22 |
| `teal` | 168 | | `rose` | 350 |
| `green` | 140 | | `magenta` | 312 |

`violet` is the suite's own `#7700FF`. Ten is a deliberate ceiling: these are
meant to be told apart at a glance in a name column, and past about a dozen hues
at one saturation the neighbours stop being distinguishable — a palette nobody
can tell apart is a palette that marks nobody.

The canonical source is `crates/recalld/src/palette.rs`; the list is mirrored in
`gui/src/renderer/lib/palette.js` and `crates/nx-recall-overlay/src/palette.rs`,
and `gui/test/palette.test.js` parses all three and fails if they drift. A token
this build does not know is **not an error anywhere it is read** — it falls back
to the voice's ordinary hashed hue, because a name in the wrong colour is a far
better failure than a name that does not draw.

#### The icon

`icon` is a short emoji: **at most two grapheme clusters**, no whitespace, no
control characters. Two rather than one because a flag is one cluster, so is a
skin-toned wave, so is a ZWJ family — but `🌙✨` is a perfectly reasonable mark
for a person and refusing it would be refusing an aesthetic rather than
enforcing a limit. Three starts to be a word. The count is of *clusters*, not
code units: a check that counted `.length` would refuse an ordinary pick while
happily accepting four flags.

An icon that is empty or blank after trimming **clears** it and is stored as
`NULL`, so there is exactly one representation of "no icon" and every reader can
test it with `IS NULL`. A colour has no such rule — the empty string is not a
token and is refused — because a colour is picked from swatches, which have a
"none" of their own, while an icon is typed into a field and emptying that field
is how a person says they want none.

The headset overlay draws the icon only if the system font really has the glyph,
and silently drops it otherwise. That surface is a monochrome coverage
rasteriser over whatever sans-serif the machine ships, with no colour-emoji path
to fall back to, and `? Kira` in front of somebody's name is worse than no icon
at all. The colour half always works, which is what makes this a degradation
rather than a failure.

#### Omitted is not null

Each parameter is optional, and the two absences mean different things:

- **omitting a key leaves that half unchanged**;
- **passing it as `null` clears it**;
- passing **neither** is `err:params` — not "clear both".

The two halves are independent (a colour with no emoji is a normal thing to
want), so collapsing "say nothing about the icon" into "clear the icon" would
make it impossible to change one without restating the other. Concretely: a
picker that sent both fields on every click would delete the emoji somebody set
a minute earlier the moment they chose a different colour. Clients send only the
half that changed.

`err:params` also covers an unknown token and an over-long or whitespace-bearing
icon. A merge tombstone is `err:conflict`, as with `speakers.name` and
`speakers.set_languages`, and here for the sharper of the two reasons: the read
resolves through the tombstone to the canonical voice while the write would not,
so allowing it would report success, change nothing anybody can see, and record
the wrong voice's prior state in the audit log.

#### Where it appears

Everywhere a voice is named. On `speakers.list` rows; beside `name`/`auto` in
every place a person appears (`person.get`'s speaker, its edges and thread
participants, `thread.get` participants, `person.brief`, the `speakers.prune`
preview, `worlds.list` people, digest participants, both sides of a commitment,
and Discord truth links); and as **`speaker_colour`/`speaker_icon`** next to
`speaker_name` on every segment shape — transcript pages, search hits, replay
turns and the live `segment` event.

It resolves through merge tombstones with the name: a voice merged into a
highlighted one wears the surviving voice's mark, because after a merge there is
one person there.

The one exception is the **`partial` event**, which carries no highlight. A
partial is a provisional caption emitted from the capture path, which does not
touch the database; it carries `speaker`, and a client resolves the highlight
from `speakers.list` exactly as it already resolves the name from `speaker_hint`.

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
               "colour": "violet", "icon": "🌙",
               "languages": ["de"], "first_seen": "2026-07-02T18:24:00Z"},
   "languages": ["de"],
   "totals": {"segments": 412, "speech_ms": 1832000, "speech_ns": "1832000000000",
              "sessions": 9, "threads": 31,
              "first_heard_ms": ..., "first_heard_ns": "...",
              "last_heard_ms": ...,  "last_heard_ns": "..."},
   "edges": [{"speaker_id": 4, "name": "Ash", "auto": "Speaker_18",
              "colour": null, "icon": null,
              "threads": 9, "seconds": 412.5, "speech_ms": 412500,
              "last_ns": "...", "last_ms": ..., "roster_seconds": null}],
   "recent_threads": [{"thread_id": 31, "session": 3,
                       "started_ns": "...", "started_ms": ...,
                       "ended_ns": "...", "ended_ms": ...,
                       "segments": 14,
                       "participants": [{"speaker_id": 12, "name": "Kira",
                                         "auto": "Speaker_03",
                                         "colour": "violet", "icon": "🌙"}],
                       "preview": "wait, which portal was it —"}]}
  ```

  Field conventions, tightly:

  - `name` is `null` until a person names the voice; `auto` is the generated
    label and is always present. Same split as `speakers.list`, in every place a
    person appears here — edges and thread participants included, so a client
    can render a voice it has never queried. Since 0.12.0 the rule covers
    **`colour` and `icon`** too: they ride beside `name` and `auto` on the
    `speaker` block, on every edge and on every thread participant, for exactly
    the same reason. A highlight is read *on a name*, so a place that shows a
    name and not its highlight would be the one place a person's mark went
    missing.
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
  **0.10.1:** `estimated_wer` is no longer a mean of per-line WERs (unbounded:
  a two-word line retyped as ten scored 400%, and a thirteen-line card read
  "112.9%"). It is now the bounded corpus **edit share** — word edits summed
  over the fixed lines, divided by the summed longer word count of each pair
  — also exposed as `edit_rate`, always in `[0, 1]`. Every level additionally
  carries `cross_check: {checked, solid, shaky, shaky_share}` — the second
  decoder's verdicts over EVERY checked row (fixed or not), the one unbiased
  figure on the card; `shaky_share` is `null` when nothing has been checked.
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
  (`truth_user_id`, `truth_verdict`, `truth_coverage`, `truth_enrol_ns`; a
  fifth, `truth_overlap_frac`, arrives with schema v13 below) and one on
  `speaker_prototypes` (`via`). Idempotent like every migration, and with no
  backfill: truth exists from the day the plugin starts sending it.

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

  - **Your own account is not presence on audio your own client made**
    (0.12.1, FINDINGS §34). Coverage is counted for every account exactly as
    above — it describes the *call* — but a user is only **present** in a turn
    if the recording can physically contain their voice, and on a `sources.kind
    = "app"` stream captured from the user's own Discord client, theirs cannot:
    a client never plays your microphone back to you (0.10.1, FINDINGS §17).
    Every Discord account linked to the pinned "You" voice is therefore dropped
    from presence on `app` audio, and only there — on `mic` and `room` the
    user's own account is the one voice that *can* be present and nothing is
    dropped. With no account linked to "You", nothing is known to be inaudible
    and the rule does not fire.

    It reads as one line and it moved three quarters of this install's
    `overlap` pile: 1,791 `overlap` verdicts became 450, because 1,341 of them
    were the user talking over one other person — single-speaker audio wearing
    an `overlap` label. `single` went 1,474 → 2,466, `partial` 516 → 597,
    `nobody` 350 → 618. A `single` naming only the user becomes `nobody`, which
    is what `nobody` has always meant: truth covers this moment and none of the
    voices this recording can hold was in it.

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
  - **`segments.truth_overlap_frac` (0.11.6, schema v13)** — the same pass
    also stamps *how much of the turn had two or more users talking at the
    same time*, which is a different number from the verdict and from
    `truth_coverage`. The verdict asks whether two users each covered a fifth
    of the turn somewhere in it; this asks how much of it they overlapped.
    Each user's own spans are merged first (as coverage does), then a sweep
    totals the time at depth ≥ 2. Since 0.12.1 it takes the same audibility
    rule the verdict does — a mouth the stream cannot carry is not a second
    voice in it — so on `app` audio the user's own spans are dropped before the
    sweep. The v13 migration backfills it for verdicts
    already on disk, but **only where the speaking spans survive** — a purged
    span and a quiet turn would otherwise both read 0.0, so a row it cannot
    measure stays NULL. `nobody` and `unknown` are never stamped: there is no
    second speaker in either to measure. Clients may read it; nothing in the
    daemon gates on it. FINDINGS §26 is what it was added for, and its
    headline is that Discord's `overlap` really is collision (median 0.47 of
    the turn) while the segmentation gate is blind to it on this audio.

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

  Two rules the pass has to obey, both of them measured rather than argued:

  - **It ranks on the aggregate the ladder learned** (`settings
    .identity_aggregate`, 0.12.2), not on a hard-coded max over a voice's
    prototypes. The label half of its decision reads the per-voice thresholds,
    which are fitted on that aggregate's scale, and `enroll_threshold = 0.55`
    means a different thing on each scale. Same rule §32 wrote down for the
    learned projection; this was the caller it had missed. `[identity] learn =
    false` means max, exactly as it means the global thresholds.
  - **The bar stays where it is, and the switch stays off** (§36). Held out on
    777 rows, the bank this pass builds at the shipping bar is identical to the
    bank a control that never looks at Discord builds from the same rows:
    a turn that clears the enrol bar is a turn the live path in `analysis`
    already enrols. Ground truth's only distinct contribution would be at a
    *lower* bar, where it is also the mechanism by which one mis-linked account
    writes another person's voice into a bank permanently — and the nightly
    gate cannot catch that, because it scores the poisoned bank against the
    same wrong link and finds it an improvement.

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

### `status.asr.devices` (0.12.4)

Which device each model on the live path runs on, and why. Always present and
always this shape, for the same reason the two blocks above it are — "this
daemon measured the question and the answer is the CPU" and "this daemon is old
enough not to have been asked" are different states:

```json
"devices": {
  "live": "cpu",
  "night": "vulkan",
  "live_models": [
    { "model": "silero-vad", "runtime": "onnxruntime", "device": "cpu",
      "cpu_share_pct": 0.8, "why": "onnxruntime's ROCm provider was removed in 1.23, …" },
    { "model": "parakeet-tdt-0.6b-v3", "runtime": "sherpa-onnx", "device": "cpu",
      "cpu_share_pct": 87.5, "why": "sherpa-onnx accepts no AMD execution provider …" }
  ],
  // 0.13.x: which ASR export is actually live right now, and why. `null` on a
  // daemon with no analysis models resolved at all — there is no transcriber
  // for the question to be about. See "Light mode" below.
  "light": { "model": "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8", "why": "no game captured and the GPU is not sustained-busy" },
  "summary": "every model on the live path runs on the CPU. …"
}
```

- `live` — the device every live model is on. `"cpu"`, and on an AMD machine it
  is not going to be anything else; see below.
- `night` — `"vulkan"` where `models build-night` has been run, `"unavailable"`
  otherwise. Reported here as well as under `night` because the question a
  person asks is "is my graphics card doing anything for this", and an answer
  that omits the one thing that uses it is a misleading answer.
- `cpu_share_pct` — that model's measured share of the live path's CPU
  (FINDINGS §40, the user's own 22.8 minutes). A **constant**, not a live
  counter: it is a property of the models, and a per-turn timer maintaining a
  number nobody reads is exactly the cost this measurement was about. A client
  ordering the list by it is ordering by "what would be worth moving".
- `why` — one sentence per model, but only **two distinct sentences** across the
  list, because the models split by runtime and each runtime is blocked for its
  own reason. A UI should fold them rather than repeat them; `recalld status`
  does.

**What a client should not imply.** There is no setting here and there is not
going to be one. A UI must not offer a "use the GPU" toggle, or present the CPU
placement as a default that can be changed: 90% of the live cost is the
transcriber, the transcriber runs under sherpa-onnx, and sherpa-onnx's provider
enum has no AMD variant — that is upstream C++, not a flag this daemon withheld.
The night shift is the GPU feature, and it has its own block. `devices.light`
is a different question and it is a real setting — which *export* of the
transducer is live, still on the CPU either way — see "Light mode" below.

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
 "summary": "Aspen fragt nach dem Shader. Kira schickt morgen den Link.",
 "summary_raw": "…what the model wrote…",
 "open": ["Kira schickt Aspen morgen den Link"],
 "open_raw": ["…what the model wrote…"], "rendered": "names",
 "participants": [{"speaker_id": 3, "label": "Aspen"}],
 "started_ms": …, "started_ns": "…", "ended_ms": …, "ended_ns": "…",
 "turns": 12, "model_id": "qwen2.5-3b-instruct-q4_k_m@1", "created_ms": …}
```

**A digest says people's names** (0.11.6). `summary` and `open` are prose about
`Aspen` and `Speaker 07` and `You`, in the same words `participants[].label`
uses — the name the user gave the voice, else its auto label written as a name
(`Speaker 07`, not `Speaker_07`), and `You` for the microphone. The model is
handed those labels in the summary call and writes them back; the **verdict**
call still sees letters, byte for byte what its six traps were measured on.

`summary_raw` / `open_raw` are what the model wrote, so nothing is lost, and
`rendered` says how the prose was arrived at:

| `rendered` | |
|---|---|
| `names` | the model was given the labels and wrote them. |
| `legacy` | written before 0.11.6, with letters. The daemon substitutes names on the way out, from the conversation's own roster — only assigned letters, only standalone, and never a sentence-initial English `A` before a lowercase word, so `A meetup at eight` stays an article. No model is asked anything about an old row, which is also why there is no `digest rerender`: an old row has no `summary_raw` to re-render from. |

Rendering happens **on read**, so a rename moves the paragraph the same way it
already moves the participant chips.

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

0.11.6 added two numbers to the same ten cases — summaries in which **every**
participant is called by their label, and summaries containing a name nobody in
the room has — and measured both ways of getting a name into the paragraph:

| design | traps | summarised | everyone named | invented names | language |
|---|---:|---:|---:|---:|---:|
| (a) letters in the prompt, substituted in the daemon | 6/6 | 4/4 | 3/4 | 0 | 3/4 |
| **(b) the labels in the summary prompt** | **6/6** | **4/4** | **4/4** | **0** | 3/4 |

(b) ships. (a)'s loss is English: *"A asked for the recording"* has the same
shape as *"A meetup at eight"*, and a substitution rule that refuses the second
must refuse the first. Neither design can move the traps, because neither
touches the verdict call — and the run confirms it rather than assuming it.
(a)'s rule survives as the read-time renderer for `rendered: "legacy"` rows.

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
row back at the end of the queue. A turn whose source language the translator
has no code for is declined the same way with `translation_via:
"unsupported-language"` (0.12.0) rather than left for a retry — it fails
identically every pass, and before this two French rows the guesser had named
without confidence went to the model with an empty tag every five minutes for
an evening. The guesser's tag now reaches the model even when it is not
confident enough to be written onto the row.

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
## 0.10.0 — export and the room microphone

Two additive features and nothing else: no method, event, field or behaviour
described above this line changes, and `proto` stays `1`. Both put a second
thing on a page that already had one of its kind, and both are shaped by the
same rule — a client that has never heard of either goes on working, because
every new key is *present* rather than merely absent-when-off (a missing key
cannot tell "off" from "an older daemon").

### The local Markdown export

`export.run` writes files into **one directory on a local filesystem that the
user chose**, and that is its entire output surface. There is no upload, no
share, no clipboard, no link, no network target — see DESIGN §12, whose
amendment this section implements. The copy in every client must say the
sentence the daemon's own CLI says: *this writes files to your disk and nothing
else.*

- **`export.preview {dir, from?, to?, speaker?, thread?, include_translations?}`**
  → `{dir, days, conversations, turns, bytes, files: [{name, bytes, turns,
  conversations, exists, blocked}], blocked: [name]}`.

  The plan is **rendered**, not estimated: `bytes` is the exact length of the
  file that would be written, because the preview and the run are one code path
  and the preview is its first half. A preview writes nothing.

  `from` is inclusive and `to` exclusive, in any form the daemon reads a time in
  (ISO-8601, epoch ms, epoch ns — "Field conventions"). `speaker` is a canonical
  voice id, so a merged voice exports under the voice it was merged into.

- **`export.run {…same params}`** → `{op, files, dir}`, then `op.progress` /
  `op.done` / `op.failed` on the `ops` topic with `kind: "export.run"`, exactly
  like `delete.run`. `op.done` carries `{files, bytes, dir}`.

- **What is written.** One file per **local calendar day that has turns**,
  named `YYYY-MM-DD.md`, plus `people.md`. A day with nothing in it produces no
  file. Every file begins with the line `<!-- nx-recall export -->`.

  ```markdown
  <!-- nx-recall export -->
  # 2026-09-01

  ## 19:04 — Kira, You — in wrld_abc123
  - **19:04** Kira: hey, did you get the thing?
  - **19:05** You: yeah, it is on the desk
  - **19:06** Kira: _nice_[^shaky]

  [^shaky]: A second decoder read this turn differently, so the words are uncertain. The speaker is not in doubt; the transcript is.
  ```

  One `##` per conversation (`segments.thread_id`), ordered by when it started,
  with its participants and — when the roster covers that moment — the world.
  Turns with no thread become one final section marked as such. A turn whose
  `asr_confidence` is `"shaky"` is written in italics with a footnote reference;
  the footnote itself is defined **once** per file. With
  `include_translations: true` a turn's `translation` follows it as an indented
  quote (`  > …`). A turn with no text is written as `_(not transcribed)_`
  rather than dropped: hiding it would claim a silence that did not happen.

  `people.md` lists the **named** voices only — name, declared languages (or
  "any language"), and when each was last heard.

- **The overwrite guard.** A file this feature wrote is rewritten. A file it did
  not write is **never touched**: `export.run` fails with `err:refused` naming
  the file, before writing anything at all — not the blocked file, and not the
  files that would have preceded it. The test is the `<!-- nx-recall export -->`
  header in the first kilobyte; a file that cannot be read counts as not ours.
  `export.preview` reports the same decision per file (`blocked`) and in
  `blocked`, so a client can warn before the button is pressed.

- **The path guard**, all of it `err:refused` with a reason a person can act on:

  | refused | why |
  |---|---|
  | a relative path | it means a different folder depending on who expands it |
  | a path reached through `..` | a confirmation dialog has to be readable |
  | under `/run/user`, `/proc`, `/sys`, `/dev` | files there do not survive the session |
  | a path that does not exist | the export writes into a folder you already have |
  | not a directory | — |
  | a network mount | copying a transcript onto a share is the one thing the design does not do |

  Network mounts are identified by `statfs(2)`'s `f_type` against a list of
  magics: NFS `0x6969`, CIFS/SMB1 `0xff534d42`, SMB2 `0xfe534d42`, smbfs
  `0x517b`, 9P `0x01021997`, CephFS `0x00c36400`, AFS `0x5346414f` / `0x6b414653`,
  Coda `0x73757245`, OCFS2 `0x7461636f`, GFS2 `0x01161970`. FUSE (`0x65735546`)
  is deliberately **not** on the list — most FUSE mounts are local and the kernel
  cannot say which are not — and a `statfs` that fails lets the path through,
  because refusing every unknown would refuse ordinary disks.

- **CLI.** `recalld export <dir> [--from --to --speaker --thread] [--translations]
  [--dry-run]`. It runs in its own process against the database directly, so it
  works whether or not the daemon is running.

### The room microphone

A **second, physical** microphone: the desk mic that hears the people sitting in
the room, who never joined the instance. It is not `[mic]` with another device,
because the two devices mean opposite things. The headset mic is *provenance* —
whatever it hears is the user, pinned to "You" with no comparison made. A room
mic hears strangers to the voicebank, so its turns take the **ordinary** route:
VAD, the overlap gate, ASR, and identity as unknown voices that are matched,
minted and enrolled exactly like voices coming out of an application. **Nothing
on this device is ever labelled You.**

- **`sources.list` rows carry `kind: "room"`** (schema: `sources.kind` is
  free-text and has been since v4; no migration, no backfill). A client that
  does not know the kind must not render it as an application, and must not
  count it among allowed applications.
- **`sources.set` refuses `match_key: "room"`** with `err:refused` naming
  `room.set`, for the same reason it refuses `mic`: `[rules]` and `[room]` would
  otherwise disagree about a consent decision.
- **`room.get`** → `{enabled, mode, active, state, device}`. There is
  deliberately no `you_speaker` counterpart, and its absence is the feature.
- **`room.set {enabled?, mode?, device?}`** → the same block plus `persisted`.
  Each field applies independently. `mode` is `"follow"` or `"always"` and
  anything else is `err:params`; a call with no fields at all is `err:params`.
  `device` is a PipeWire `node.name`, or `null` to clear the pin — and unlike
  `mic`, it is settable over the wire, because a device with no default cannot
  be a config-file-only decision without making the feature unreachable.
  The change is live *and* written to `config.toml`.
- **The device is required, and the refusal says so.** A call that would leave
  the switch on with no device is `err:params` naming `devices.list`; the switch
  does not move. There is no sensible default for a second input — following
  `default.audio.source` would open the headset `[mic]` is already on and record
  the user twice under two identities.
- **`state`** is the microphone's five plus one:

  | state | meaning |
  |---|---|
  | `off` | not recording, and not listening for a reason to |
  | `needs-device` | on, and no device is pinned — nothing can open |
  | `following:idle` | on, waiting for an allowed application |
  | `following:active` | recording the room, because an allowed app is captured |
  | `always:active` | recording the room, whatever is running |
  | `always:idle` | on, but the pinned device is not on the graph |

  `needs-device` exists because `always:idle` would be a lie of the kind the
  microphone's own contract forbids: it says "waiting for a device", when the
  truth is "no device was ever chosen".
- **`room` event**, on the existing **`status`** topic, same block, published
  when the switch moves and when the tap opens or closes — the only way a client
  sees a `follow`-mode transition. No new topic, so no client changes its
  subscription and an older one ignores it under the versioning rule.
- **`status`** carries `room` (the same block) and the flat `room_state`, plus
  `counters.room_segments`.
- **Provenance.** Segments from this device carry `source: "room"` — the source's
  match key, on the wire since v1, needing no new field. `label_via` is whatever
  the identity ladder decided, never `"mic"`.
- **Threading.** Room turns bridge across sessions exactly as microphone turns
  do: a room turn may join a conversation that is live in **any** session, not
  only its own. The room and the headset are one physical evening, and somebody
  on the sofa answering somebody in the instance is in that conversation — which
  device carried the sound is a fact about cabling. Two *applications* speaking
  at once are still two conversations.
- **`devices.list`** → `{devices: [{node_name, description, is_default}]}`.
  Every `Audio/Source` on the graph, default first then by name; a node with no
  `node.name` is omitted because it cannot be pinned. `is_default` is on the
  wire so a client can warn about the one choice that is almost always wrong —
  the default input is the headset. It opens no stream. The daemon reads its own
  PipeWire registry and falls back to parsing `pw-dump` if a second connection
  cannot be made.
- **CLI.** `recalld devices`, and `recalld room [status|on|off|follow|always]
  [--device NODE_NAME]`.

### One addition to `status` for a 0.9.0 feature

`status` now also carries **`truth`**: `{enabled, listening, last_event_ms,
users}` — the Discord ground-truth ingest's four facts, so a client can draw its
state without polling `truth.status` every few seconds. `enabled` is the
intention and `listening` the address actually bound (they differ when the port
was taken); `last_event_ms` is when the plugin last sent anything, which is the
difference between "receiving" and "waiting for Discord"; `users` is how many
accounts it has heard. Everything else stays on `truth.status`, which remains
the method for the whole picture.

---

## 0.11.0 — source-aware identity

Where the audio came from is evidence about who is on it. A voice heard 2 798
times on Discord and never once in VRChat should not win a VRChat turn at 0.36,
and until now it could: the voicebank was asked one question about the whole
world at once.

This release adds **one new field to two existing methods**, **one CLI command**,
and **one optional change to the labelling ladder that is off by default**.
Nothing on the wire is removed or reshaped, and no schema version is bumped —
the source history is derived from `segments` and `sessions` on demand, so there
is no new column and no new table, only one index
(`idx_segments_speaker_session`).

### `sources` on `speakers.list` rows and on `person.get`

Both carry the same array, most-heard first:

```json
"sources": [
  {"source": "Discord",    "name": "Chromium", "kind": "app",
   "segments": 2798, "last_ms": 1788378123000, "last_ns": "1788378123000000000"},
  {"source": "VRChat.exe", "name": "VRChat",   "kind": "app",
   "segments": 12,   "last_ms": 1788291000000, "last_ns": "1788291000000000000"}
]
```

- **`source` is the match key**, not the display name — an application's
  executable name, or the literal `mic` / `room`. It is what the prior keys on
  and the thing that survives a display name changing under it. `name` is what a
  person should read; the two genuinely differ (a Discord client presents itself
  to PipeWire as `Chromium`).
- **`kind`** is `app`, `mic` or `room`, as on `sources.list`.
- **Both time forms**, per "Field conventions".
- **`[]`, never `null`.** A voice with no live turns has an *empty* history, not
  an unknown one, and a client must never have to guard the field. Soft-deleted
  turns are excluded, so the counts add up to the `segments` beside them.
- On `person.get` it sits at the **top level**, beside `languages`, for the same
  reason `languages` does: the page has a row of chips for it and should not
  have to know the field lives on the speaker row.

### The prior itself (`[identity]`, off by default)

| key | default | what it does |
|---|---|---|
| `source_prior` | `false` | whether any of this affects labelling at all |
| `foreign_source_margin` | `0.10` | added to `label_threshold` for a voice foreign to this source |
| `foreign_after_segments` | `20` | turns a voice needs before its *absence* from a source counts as evidence |
| `presence_hard` | `true` | may Discord's own events exclude a candidate outright |
| `vrchat_sources` | `["vrchat"]` | which sources are VRChat; the mirror of `[truth].sources` |

**It is off because it was measured, not because it is unfinished.** On this
install's 161 Discord turns with Discord's own ground truth the soft rules
removed 236 candidates and changed **zero** labels: every voice the ladder was
choosing between was already native to Discord. `recalld identity audit` finds
three cross-source labels in 9 039, all scoring 0.361–0.393 — the predicted
failure, at the predicted scores, in numbers too small to measure against.
FINDINGS §17 has the table and the gate.

Three rules, applied between ranking and deciding:

1. **Soft — foreign to this source.** A candidate with at least
   `foreign_after_segments` turns in total and **zero** on this source must
   score `label_threshold + foreign_source_margin` *and* beat the best native
   candidate by `enroll_margin`. Failing either, it is **removed** from the
   candidate list rather than demoted — so the mint rule can create a voice that
   genuinely belongs to this source instead of the turn being absorbed by a
   stranger from another app. The pinned "You" voice is never foreign: the
   microphone follows the user everywhere.
2. **Hard — Discord says they were not there.** On a Discord-sourced segment, a
   candidate linked to a Discord account (`discord_users.speaker_id`) with no
   speaking span within **±5 minutes** is excluded outright. It fires only when
   truth data exists in that window at all (no plugin is not absence), only for
   candidates that actually have a link, and only on Discord-sourced segments.
3. **Soft — the VRChat roster.** On a VRChat-sourced segment, a *named*
   candidate whose display name is not in the roster within **±10 minutes** gets
   the foreign treatment, never exclusion. Unnamed voices are untouched.

**The asymmetry between 2 and 3 is deliberate and is about what the two sources
know.** Discord's events are keyed on an account id that cannot be typed wrong,
and a turn on the Discord stream is by definition audio Discord decoded — so if
the account was not talking, the audio is not theirs. The roster is display
names scraped from VRChat's text log and matched to a voice by nothing but the
user having typed the same string; people rename themselves and the log rotates,
so a name that fails to match is more often our failure than their absence.
Evidence that weak may raise a bar; it may not slam a door.

The known cost of rule 2, stated rather than hidden: `truth_speaking` records
*speaking*, not membership, so an account that sat silently in the call for more
than five minutes either side of a turn is excluded from it. That is the trade —
an account that said nothing for ten minutes around a turn is a poor explanation
for that turn.

**No new `label_via`.** A label that survived a foreign check is still a match,
made by the same ladder on the same evidence at a higher bar, and a client that
saw `label_via: "foreign"` would have to decide what to do about a distinction it
cannot act on. What the daemon records instead is a log line naming every
candidate it removed and why, and three counters (`prior_foreign`,
`prior_absent`, `prior_foreign_kept`).

### `recalld identity audit`

A report; it writes nothing. Four parts: the **voice × source matrix**, the
**banks over the cap**, the **count of labels the rule questions**, and the
**twenty most recent** of them with their scores and `label_via`.

**Banks over the cap** lists every live voice holding more than
`[identity].max_prototypes` prototypes. `add_prototype` enforces that number on
every write, so a voice can only be over it because `merge_speakers` re-pointed
a collapsed voice's prototypes and nothing re-applied the cap. It reports and
does not repair: FINDINGS §44 measured every automatic trim held out and refused
all of them — pruning the most *redundant* prototype (the eviction rule with no
incoming vector) is catastrophic, because redundancy pruning keeps exactly the
outliers that do not belong, and pruning the most *outlying* one is safe and
immaterial. `identity repair --prototypes` stays the only command that deletes.

A past label is judged by **replaying the labels in the order they were made** and
asking the prior's question of each using only what was known before it. Any
other reading is circular — a voice that won ten VRChat turns has ten turns of
VRChat history *today*. The consequence is that a run of wrong labels is counted
once, at its head; each row carries `followed` saying how long its run got.

`recalld identity repair --foreign` takes those labels back to **unassigned** and
previews by default (`--apply` writes). It never re-points a row at another
voice, and must not grow that ability: the finding is that a label is not
supported by where the audio came from, which argues against the name the row
has and for no other name at all. The transcript, the audio and the embedding
all stay, so a later reassignment still has everything to argue from.
## 0.10.2 — translation controls

0.9.0 shipped translation as a config-file entry with one setting: `[assist]
translate_to`. In use it turned out to be three questions, and the person asked
all three in one breath — *everything but German and English should be
translated*, *the original should be subtext and the translated thing the main
thing*, and *translate it to English*. This round makes each of them a live
setting with a control in the Memory view, and teaches the daemon to recognise
a third language at all, because "everything but German and English" is not
implementable by a classifier that only knows German and English.

Everything here is additive. A client that has never heard of it renders 0.9.0's
layout, and a daemon that has never heard of it answers `unknown_method` to both
new methods — which a client treats as "this cannot be set from here", not as a
failure.

### `[assist]`, three keys

| key | default | meaning |
|---|---|---|
| `translate_to` | `""` | the target. `""` is off, and is still the shipped value |
| `read_languages` | `["de", "en"]` | languages a turn may be in without being translated |
| `translation_display` | `"main"` | `"main"` or `"under"` — which line leads on a row |

`read_languages` **always implicitly contains the target**: a target you would
then translate away from is not a setting anybody meant, and every read of this
value on the wire has the target folded in. It is *not* the same list as
`speakers.set_languages`, which says what one voice speaks and still accepts
`de`/`en` only.

`translation_display: "main"` is a deliberate reversal of 0.9.0's argument. That
round said the original must lead because the transcript is a record; the
correction is that somebody who cannot read the original is not reading a
record, they are reading a wall of text with a hint under each line. Both lines
are on the row in either mode, the translation always says which language it is
in and which model wrote it, and under `main` the original carries its own
language code.

### `assist.get` → the state and the selector's options

```json
{"translate_to": "en",
 "read_languages": ["de", "en"],
 "translation_display": "main",
 "languages": [{"code": "en", "name": "English"}, …]}
```

`languages` is on the wire for the same reason `graph.get` carries
`llm_threads_min`/`_max`: a client builds its selector out of what the daemon
will accept rather than out of a list in its own source that can drift. It is
`lang::OFFERED` — en de fr es it pt nl pl ru uk ja zh ko tr sv da no fi cs.

### `assist.set {translate_to?, read_languages?, translation_display?}`

At least one field, or `params`. Every code is validated against the list above
and a bad one is a **refusal**, not a silent drop — a client that asked for
`"gr"` and was answered `"en", "de"` would show a language it is not translating
into. `translate_to: ""` is the exception and is how translation is switched
off. Codes are case-folded and de-duplicated.

The reply is `assist.get`'s shape plus `persisted`. The three values are applied
to the running daemon *before* the reply, so the answer describes what is
already true, and written back to `config.toml` the way `graph.set` writes the
graph settings: re-read the file, move the fields, save.

- **`assist` event**, on the existing **`status`** topic, carrying the same
  shape. No new topic, so no client changes its subscription and an older one
  ignores it. It repaints the *transcript* as well as any settings card, which
  is unusual for a settings event and is the point: `translation_display`
  decides which of a row's two lines is the main one.
- **`status`** carries the three values under `assist` (not `languages` — a
  selector's options do not change and that block is polled every few seconds),
  beside the existing `assist.reminders` and `assist.digest`.

### Which turns are candidates

A turn is a candidate when its `lang` is **not** in `read_languages ∪
{translate_to}` — plus the existing rules: three words minimum, nothing already
looked at (`translation_via IS NULL`), and nothing in a language the user's own
voice is declared to speak.

`lang` is `de`, `en` or NULL, so before this round a French turn was NULL — the
same stamp a mumbled German line gets — and invisible to that query. 0.10.2 adds
`lang::guess_other`, which answers *which language other than German or English
is this*, and a NULL-language turn is a candidate **only when it answers**. That
asymmetry is the safety argument: the difference between "a language I do not
read" and "words nobody could read" is the only thing that stops "translate
everything I cannot read" from meaning "translate everything".

The rule is script first — Cyrillic (ru, or uk on `і ї є ґ`), kana → ja, hangul
→ ko, Han → zh, Arabic → ar, Greek → el — then a stopword vote over twelve
Latin-script tables, which fires only on ≥3 stopwords, strictly more than de+en
together, and an outright win.

Measured in `spike/guess_other_bench.py` over **4200 FLEURS sentences**: 200 per
language plus 200 German and 200 English as negatives, precision over the whole
mixed set. The gate was 90% precision. Shipped: **ar cs da el es fi fr it ja ko
nl pl pt ru sv tr uk zh** (93.1%–100%; **zero** of the 400 German and English
negatives was guessed to be anything at all). Norwegian did **not** ship — its
function words are Danish's, 64.8% precision — and stays in the vote table as a
blocker, so a Norwegian sentence is answered "I cannot tell" rather than
"Danish", which is what keeps Danish at 98.4%.

A **confident** guess — a non-Latin script, or four stopwords — is written onto
the row as `lang` with `lang_via: "guessed"`, a new value beside `model`,
`classified`, `re-decode`, `mismatch` and `context`. Like `context` it is an
inference and the words were not re-decoded. An unconfident guess is enough to
ask the model and not enough to claim anything in the database.

### The prompt

The few-shot example is now **in the target language**. It was German
hard-coded, from the 0.9.0 bench, and a prompt that says "translate into
English" under two worked examples answering in German is a prompt arguing with
itself. Only English and German have written examples, because those are the two
this project can check; every other target gets the instruction alone.

The wrong-language guard, which used to have no opinion about any target that
was not `de` or `en`, now guards those too: a confident `guess_other` reading of
a language that is not the target is a rejection, and where it cannot read one,
three German or English stopwords are. It deliberately does **not** use
`lang::classify` for a third-language target — that settles on German the moment
it sees an umlaut, and Swedish, Turkish and Finnish are full of them.

## 0.11.0 — learned identity

Everything in this section is **additive and inert until earned**. A database
where nothing has been calibrated behaves exactly as 0.10.2 did, byte for byte,
and a client that ignores the whole section is correct.

### The claim, and what measuring it found

The voicebank has one label threshold for every voice, chosen once on lab audio.
Now that Discord's ground truth names hundreds of the user's friends' turns, it
should be possible to do better: a bar per voice, and an embedding space
rescaled by how much one person's own turns wander.

Both were built and both were measured against held-out ground truth. **Neither
cleared its gate on the corpus this shipped against** — 239 usable rows over one
evening and three linked voices — so the shipped operating point is unchanged.
What ships is the mechanism, the gate and the report. Numbers, per row, in
`spike/FINDINGS.md` §18.

### The honesty rules

They are in the code, not in a decision somebody made once.

* **Chronological split.** Truth rows are ordered by time; the first 60% may be
  fitted on, the last 40% is the only thing any verdict reads. The split never
  cuts through a shared timestamp.
* **No self-derived prototype.** A row is never scored against a prototype that
  row produced. Without this, every number is a memory test.
* **Own account excluded.** A `single` verdict naming the user's own Discord
  account is not ground truth about audio from the user's own client (0.10.1).
* **Hyperparameters on an inner split.** The whitening intensity and the
  per-voice row bar are chosen on a split *of the fit split*.
* **Two hurdles to install.** `calib::swap_is_safe` vetoes any candidate that
  lowers held-out precision, whatever it does to recall — a wrong name corrupts
  what the user reads back as memory, a missed one costs a shrug.
  `calib::improvement_is_material` then refuses to move the operating point for
  less than **2 percentage points** of held-out recall or **a fifth** of the
  wrong labels. Both, or nothing changes.

### What can be learned

**A label threshold per voice.** Bounded to `[0.30, 0.60]`, fitted for voices
with at least 30 truth rows in the fit split, maximising F-0.5 (β = 0.5:
precision counts four times as much as recall) with ties broken towards fewer
wrong labels. A voice under the bar keeps the global. Only the **label**
decision is learned — enrolment keeps every global bar it had, because a wrong
prototype is permanent and nothing has measured that decision.

**A linear map before cosine.** Pooled within-class covariance, shrunk towards a
scaled identity, inverted to a chosen power. Square and full-rank on purpose: a
rank-reducing LDA would discard the subspace that separates every voice ground
truth has never named. 192×192 f32.

### Where it lives

`speakers` gains five nullable columns — `label_threshold`, `label_margin`,
`threshold_via`, `threshold_n`, `threshold_at` — and there is one single-row
`identity_projection` table. Absence means the global. The database rather than
the config file, because a fitted threshold is derived data with provenance and
a config file is the user's opinion; it also means a learned value travels with
its voice through a merge, a rename and a backup.

A merge tombstone never carries a learned threshold: the table the ladder reads
resolves through the live-speaker view, so a merged-away id cannot hold a bar
that applies to nothing while looking like it applies to something.

### `identity.calibrate`

```jsonc
// request
{"method": "identity.calibrate", "params": {"apply": false, "reset": false}}
```

`apply` is permission to install **what cleared the gate**, not permission to
install. `reset` puts every voice back on the globals and drops the learned
space. Without either, the call measures and returns; it writes nothing.

The reply is the whole report:

```jsonc
{
  "rows": 239, "fit_rows": 143, "eval_rows": 96,
  "per_voice":  [{"speaker": 25, "fit": 116, "held_out": 62}],
  "installed":  [{"speaker": 25, "threshold": 0.33, "margin": 0.0, "n": 102}],
  "proposed":   [{"speaker": 25, "threshold": 0.33, "margin": 0.0,
                  "n": 102, "f_beta": 1.0, "f_beta_global": 0.998}],
  "baseline":   {"n": 95, "correct": 80, "wrong": 7, "declined": 8,
                 "precision": 0.92, "recall": 0.842, "f_beta": 0.903},
  "candidate":  {"n": 95, "correct": 81, "wrong": 7, "declined": 7, "…": null},
  "projection": null,             // or {shrinkage, power, centred, score}
  "projection_installed": false,
  "thresholds_swap": false,       // did the gate approve?
  "projection_swap": false,
  "gate": {"shipping": {…}, "best": {…}, "curve": [{…}]},
  "written": 0, "cleared": 0,
  "note": null                    // why nothing could be measured, when nothing could
}
```

`precision`, `recall` and `f_beta` are `null` rather than a number when there was
nothing to divide by — an arm that named nothing is not perfectly precise.

`gate` is the **overlap** gate measured against Discord's `overlap` verdicts:
`curve` walks 0.05 to 0.30, `shipping` is the point `[identity].max_overlap`
currently sits on and `best` is the point the fit split preferred, looked up in
the held-out curve. On this corpus the fit split preferred 0.20 and held out it
was worse than the shipping 0.10, so 0.10 stands.

### `[identity].learn`

On by default. The truth pass refits at most every six hours, and only when the
truth corpus has grown by a fifth since the last run. Every write goes to
`operations` as `identity.calibrate` with the before/after table as its prior
state. Off means the globals, exactly as 0.10.2 used them; the CLI still
reports.

### What a client should show

Nothing is required. When it wants to:

* a **"calibrated on N turns"** chip on a voice with a learned threshold, from
  `installed[].n` — the number is the provenance, and a learned bar nobody can
  see is indistinguishable from a magic one;
* the **before/after precision pair** from `baseline` and `candidate`, which is
  the only honest way to render "is it getting better";
* the fit/held-out row counts per voice, because "this voice has no ground
  truth yet" is the answer to most questions about why nothing changed.

A client must not present a *proposed* threshold as an installed one:
`thresholds_swap` is the difference, and it is false far more often than true.

## 0.12.0 — how a voice's prototypes become one score, and a bank that can be repaired

Three changes to identity, all of them measured against this install's own
ground truth on the same held-out rows (`spike/FINDINGS.md` §32). Two are
learned and travel through `identity.calibrate`; one is an operator command.

### A third learnable: the scoring rule

A voice has up to twenty prototypes. Until 0.12.0 it scored the **best** of
them, which answers *could this be them?* and is generous in exactly the wrong
way: one recording of somebody that happens to sit near another person's turns
wins those turns forever, and nothing the voice's other nineteen prototypes say
can outvote it.

The alternative asks whether the voice's record **agrees**: the mean of its k
best. On this install that one change is worth more than everything 0.11.0
learned — held out, wrong labels 13 → 7 and F-0.5 0.947 → 0.972, and it wins at
the *global* threshold with no per-voice fitting at all, so it is the aggregate
and not a threshold artefact. The mean over *all* prototypes is measured and
catastrophic (F-0.5 0.774): prototypes are supposed to span a voice's range, so
averaging the bad ones in measures the spread rather than the match.

Stored as a `settings` row, not a column: it is one rule for the install rather
than a property of a voice. **Absent means `"max"`** — every version before
0.12.0, and every install that has learned nothing.

`identity.calibrate` gains three fields:

```jsonc
{
  "aggregate": {"rule": "top-3",
                "score": {"n": 460, "correct": 426, "wrong": 7, "declined": 27,
                          "precision": 0.984, "recall": 0.926, "f_beta": 0.972}},
  "aggregate_installed": "max",   // the rule every other arm was measured under
  "aggregate_swap": true          // did the gate approve it?
}
```

`rule` is `"max"` or `"top-<k>"` for k in 1..5. A client that does not know a
value must fall back to "the best prototype" rather than refuse to render.

**Changing the rule clears every learned threshold.** 0.41 under max cosine and
0.41 under a top-3 mean are not the same operating point, so a bar fitted
against the old scale is a number nothing stands behind. One run moves the
scale; the next calibrates to it. Clients should expect `cleared > 0` with
`written == 0` on the run that swaps the aggregate, and that is not a bug.

### A projection is now taken back, not merely refused

Through 0.11.8 the pass only ever *wrote* projections. One evening's `--apply`
installed a whitening; every later run measured it, refused it, and left it in
the table — and the daemon went on labelling every turn in a space its own
held-out numbers called worse. On this install that cost twenty-one correct
labels and 4.5 pp of held-out recall, silently, for two days.

A projection this run's held-out numbers do not re-earn is now dropped, and
`projection_cleared: true` says so. It only fires when the pass actually
measured something: a run that could not split has no verdict to refuse with,
and absence of evidence is not refusal.

### `identity.repair` — prototypes that are somebody else

```jsonc
// request
{"method": "identity.repair", "params": {"prototypes": true, "apply": false}}
```

A prototype enrolled from a turn that carries a `single` verdict — one linked
account covering at least `SINGLE_MIN` (0.8) of the audio — naming a **different**
voice than the prototype's owner is a recording of that other person filed
under this one. It is not a fit and it has no parameter: it is a consistency
check between the bank and Discord's own word.

```jsonc
{
  "condemned": [{"prototype": 2114, "owner": 55, "owner_name": "Speaker_55",
                 "truth_speaker": 25, "truth_name": "Aspen",
                 "segment": 17991, "coverage": 0.90}],
  "deleted": 0,
  "before": {"n": 458, "correct": 430, "wrong": 7, "…": null},
  "after":  {"n": 458, "correct": 432, "wrong": 5, "…": null},
  "note": null
}
```

Three kinds of evidence are deliberately **not** used, and a client explaining
the command should say so:

* **`partial` and `overlap` verdicts.** A `partial` verdict is one account under
  the coverage bar; an `overlap` turn has two mouths open and the embedder is
  captured by one of them, so the prototype may perfectly well be its owner.
  Measured: removing the twenty overlap-sourced prototypes on this install
  *raises* the wrong-label count.
* **The user's own account.** A `single` verdict naming the user's own Discord
  account is not ground truth about audio captured from the user's own client
  (0.10.1). The same rule that keeps those rows out of every headline keeps them
  from condemning a prototype — on this install it spares twenty-seven.
* **Golden prototypes.** Hand-enrolled audio is the user's own word about who
  this is, and it outranks a speaking ring.

`before` and `after` are measured on the held-out rows **minus** any row whose
own verdict the repair read. That is the same rule as "no row is scored against
a prototype it produced itself", one step further out.

Unlike `identity.calibrate` this **never runs itself**. Deleting a prototype is
permanent, and the gate that stops a six-hourly job churning an operating point
is not the right gate for a correctness fix an operator asked for. Writes go to
`operations` as `identity.repair`.
## 0.11.0 — live translation and short-line detection

Two changes, one complaint behind both: a French line — "Tu arrêtes
appartement." — sat in the transcript untranslated, and would have sat there for
up to five minutes even if the daemon had recognised it.

**No new methods, no new fields on the wire.** A `segment` event is published a
second time when its translation lands, exactly as 0.9.0 already did from the
idle pass; what changed is *when*.

### The language of a short line

`lang::guess_other` needed three function words. A lobby turn is three to eight
words and mostly content, so on real turns it answered "I cannot tell" more
often than it answered — `spike/short_lang_bench.py` measures 33.8% recall over
FLEURS test fragments of 2, 3, 4 and 6 words. Two stages are added below the
script check:

* **an exclusive character** — one that exactly one shippable Latin-script
  language of the set writes and neither German nor English does. The table is
  generated from the corpus, not written by hand: `ç` is Turkish and Portuguese
  before it is French, `ø` is Norwegian as well as Danish, and shipping those
  cost three languages the gate. What is left is `ñ` (es), `ãõ` (pt), `ąćęłńśż`
  (pl), `ğış` (tr), `ýčěřšůž` (cs). A line carrying two languages' characters is
  answered "I cannot tell", never split.
* **a character-trigram model** — `crate::lang_ngrams`, generated from the
  FLEURS dev text, 14 languages × 2 000 trigrams. A language wins by argmax with
  a margin over the runner-up *and* a margin over the better of German and
  English, both scaling as `1/sqrt(trigrams)` because the score is a mean.
  German, English and Norwegian are in the tables and cannot win: the first two
  are what a guess must beat, and Norwegian is 0.10.2's blocker, present so that
  its win can be refused rather than handed to Danish.

Recall over every shippable language and fragment length goes from 33.8% to
74.7%; German and English false positives stay at 0.00–0.25% per length against
a gate of 0.5%. Danish ships stopword-only — it has no exclusive character and
cannot be told from Norwegian at two words. FINDINGS §21 has the per-language
table, the gate verdict and the two pre-existing weaknesses it exposed.

A guess from either new stage is **confident**, so it is written to
`segments.lang` with `lang_via = "guessed"` exactly as 0.10.2's confident
stopword guess is. Clients need no change: `lang` and `lang_via` mean what they
already meant.

### A translation while the line is still on screen

Until now every translation came from the assistant's idle pass, and that pass
yields the model to the enrichment queue for five minutes at a time
(`assist::SHARE_EVERY_S`). For a paragraph about last night that is fine. For
the sentence somebody is reading it is not a caption at all.

When `write_segment` commits a turn whose language — stamped, or guessed at
commit time — is outside `read_languages ∪ {translate_to}` and outside the
languages the user's own voice is declared to speak, and translation is on, the
segment id goes onto a bounded queue and the assistant thread is woken. The
worker drains that queue **first**: before digests, before the fair-share
arithmetic, one model call per line.

* The queue holds 64 ids. Past that the **oldest** is dropped and counted — the
  newest line is the one being read, and a dropped id loses nothing permanently
  because the row still has a NULL `translation_via` and the ordinary pass will
  offer it again.
* `status`'s `assist` block gains two counters: `live_queued` and
  `live_dropped`. Both are read from the queue rather than from the rows, so
  they are zero on a fresh daemon and say nothing about history.
* The gates that are about the machine still apply in full and are re-checked
  between every line: **paused writes nothing, including this**, and a
  transcription backlog stands the pass down. Only the fair share is skipped.
* Changing `translate_to`, including switching translation off, empties the
  queue. A line queued for one target is not a line anybody asked to read in
  another.
* The three-word floor becomes **two words** for a line whose language was named
  confidently — a two-word French line is still a line the reader cannot read.
  One word stays out at any confidence.

Measured end to end on the fixture path, from the `write_segment` hook to the
`segment` event carrying the translation: **median 3.94 s** over five foreign
lines with qwen2.5-3b at `-t 4`, which is the model call and almost nothing
else.
## 0.11.0 — grounded answers

`search.answer {q, limit?}` → everything `search.ask` returns, plus **exactly
one** of `answer` and `refused`:

```json
{"q": "…", "interpretation": {…, "is_question": true}, "total": 4, "hits": [ … ],
 "answer": {"text": "The meetup is at eight in the evening.", "lang": "en",
            "citations": [204], "via": "qwen2.5-3b-instruct-q4_k_m", "took_ms": 11200},
 "refused": null}
```

or

```json
{"…": "…", "answer": null, "refused": {"reason": "the transcript does not say"}}
```

`interpretation` and `hits` are `search.ask`'s, field for field, because they
are the same code. **The hits come back either way** — that is the whole design:
a refusal is not an error page, it is the search results with an honest line
above them. Nothing is written to the store; `search.answer` is a read, and the
daemon does not "remember" answers.

`interpretation` gains one field, on **both** methods: `is_question`, true when
the query ends in `?` or opens with an interrogative. A client uses it to decide
which method to call, and it is reported rather than re-derived so the daemon's
reading and the client's cannot differ.

### Two model calls, verdict first

1. `{"answerable": true|false}` over the question and the top **k ≤ 12** hits,
   each shown as `[id] HH:MM Name: text`, oldest first, clipped to a budget of
   roughly 1 800 tokens. The grammar has no field in it that could hold an
   answer. The instruction that matters: a question is answerable **only if the
   lines state the answer**, not if they merely mention the subject.
2. Only if that said yes: `{"answer": "…", "citations": [id…]}`, under a grammar
   whose `id` rule **is the list of ids that were shown**, so citing a row that
   was not on the page is not something the decoder can emit. At least one
   citation is required; the answer is one or two sentences, at most 60 words,
   in the language of the question (`de`/`en`, by the question's own reading).

An empty hit list is refused before either call runs.

### Post-checks, all hard

A grammar can force the shape of an answer, never its honesty. Both of these
produce `refused` — never a repaired answer — and the hits are still returned:

- every citation must be an id that was shown;
- the sentence must share **at least two content words** with the rows it cited
  (normalised, with the de/en function words dropped). A sentence that cites row
  109 and has no word in common with row 109 was not read off row 109.

`refused.reason` is one of: `there is nothing in the archive about that`, `the
transcript does not say`, `the model's answer did not come from the cited
turns`, `the local model is switched off`, `answers need the local model —
\`recalld models fetch --graph\``, `answers are off until the bench passes`,
`the model did not answer`.

The last two were not written down before 0.11.x, and one of them did not
exist. **`the model did not answer` is a statement about the model, not about
the archive** — a timeout, a killed child, a non-zero exit — and a client must
not render it as a refusal that says anything about what was or was not said.
It used to be reported as `the model's answer did not come from the cited
turns`, which claims there was an answer; a client with no branch for that
string then fell through to whatever its default said, which in this project's
own GUI was "The transcript does not say." A dead model must never be able to
make a claim about somebody's transcript.

`answer.via` is the model id, the same string every other Tier 3 row records.

### What it measured

`spike/answer_bench`: 24 questions over a seeded 200-turn German/English
transcript, run against the real Qwen2.5-3B-Instruct Q4 on four pinned cores at
nice 19. Gate: ≥11/12 traps refused **and** ≥9/12 answerable answered with every
citation correct. **Shipped: 12/12 traps refused, 10/12 answered correctly**,
5.6 s median (a refusal is one short call; an answer is two). The two it loses
are compound questions whose halves sit in two different rows — the error in the
safe direction.

The finding worth keeping: the four refusal rules had to be in the **user**
prompt, immediately after the rows and immediately before generation. In the
system prompt alone — same words, same examples — the traps sat at 9/12; moved
to the end of the user turn they went to 12/12. A small model's attention is a
recency effect, and a system prompt is the least recent thing in the window.

### 0.11.x — questions asked in Japanese

Everything above holds, with three additions. Japanese turns have been in the
archive since the `lid` re-decode below; asking about them in Japanese did not
work, and it failed **quietly**, in the way that costs a person the feature
without ever telling them so.

**`is_question` reads a Japanese question.** The rule was "ends in `?` or opens
with an interrogative", and Japanese satisfies neither: the mark is `？`, and
Japanese does not front its interrogatives — 誰 and 何 sit wherever the clause
puts them. So a typed Japanese question was a keyword search, and because the
client picks the method off its own copy of this rule, the round trip that
could have refused never happened. Now: a trailing `？` counts, and so does a
Japanese-by-script sentence ending in one of `でしょうか`, `ですか`, `ますか`,
`かな`, `か`, `の` — with or without the mark. The script test is the guard, and
it is kana-first for `lang.guess_other`'s reason: kanji alone are Chinese's
characters too.

**`answer.lang` can be `"ja"`.** The question's own script decides it, ahead of
the de/en stopword vote, because a writing system is an answer where a
six-word stopword count is not. The system prompt for it is written **in**
Japanese with a Japanese worked example — including the sentence inside the
example's JSON, since an `answer` field in English is an instruction to write
English whatever the prose above it says. It asks for one or two sentences and
**at most 60 characters**, not 60 words: a Japanese sentence has no spaces to
count, so the word cap could never fire and the character cap is what is
enforced. The verdict call is unchanged, byte for byte — one boolean, the same
four rules repeated at the end of the user turn, which is the finding doing the
work here too. Naming Japanese in it was tried and cost a de/en positive while
changing nothing Japanese, so it was not kept.

**Grounding is counted in character bigrams.** The post-check is otherwise
unchanged and just as hard, but its unit is not: split on whitespace, a
Japanese sentence is one token, so the check had two possible outcomes — 1 if
the answer was character-for-character the row and 0 otherwise. Every honest
Japanese answer failed it. For an answer that is Japanese by script the answer
and the cited rows are compared as adjacent character pairs (compatibility
forms folded, punctuation a boundary rather than a character) and **at least
three** must be shared, rather than two content words; bigrams are commoner
than content words, and です alone hands any two Japanese sentences one. Latin
text is untouched, unit and threshold both.

`refused.reason` gains nothing: a Japanese question that cannot be answered is
refused with the same seven strings, for the same seven reasons.

**What it measured.** `spike/answer_bench --lang ja`: six questions over a
44-turn spoken-Japanese fixture, same binary, same four pinned cores. Gate: 3/3
traps refused **and** ≥2/3 answered with every citation correct. **Shipped: 3/3
traps refused (a price never stated, a person who never spoke, a specification
that is the world's knowledge and not the transcript's), 3/3 answered with
correct citations**, 10.5 s median — about twice a de/en case, which is the
tokeniser.

## 0.11.0 — audio-language routing

Two failures, one mechanism. Both are turns whose language the transcript
cannot reveal, so both are settled by a model that **listens**; they differ in
what is wrong with the decoder that produced the words in the first place, and
therefore in what it takes to replace them.

* **0.11.0, Japanese** — the decoder does not speak the language and does not
  say so. Below.
* **0.11.8, the other languages** — the decoder speaks the language perfectly
  well and, on a one-second fragment, hears a different one. Further down.

### The Japanese half

A turn spoken in Japanese did not come back wrong-looking. It came back
**wrong-looking-like-English**: Parakeet-TDT-0.6b-v3 covers 25 European
languages, does not cover Japanese, and does not say so — it transliterates.
"sumimasen, ogenki desu ka" was stored as `Sima Sen Okenki Deska.`

That is why this round adds a decision made from **audio** rather than from
text. Every language mechanism in the protocol before now — `lang_via: "model"`,
`"classified"`, `"guessed"`, `"context"`, `"re-decode"`, `"mismatch"` — reads
the transcript, and there was nothing in that transcript to read.

### `segments.lang_via: "lid"`

A sixth value, and it has to be its own. It means: **a model listened to the
audio and said which language it was**, and a decoder for that language then
re-read the turn. It is not `re-decode` (nothing disagreed with a speaker's
declaration) and not `classified` (the words said nothing — they could not).

A row carrying `lang_via: "lid"` has `lang: "ja"` (or, since 0.11.6, `"ko"` or
`"zh"`), an `asr_model_id` naming the decoder that produced it, and `text_via:
"lid"` — its own value since 0.11.4, because a person filtering for "what did
the language router change" should not have to separate it from the German flip
arbiter's rows by hand.

**A `lang_via: "lid"` row is settled.** The conversational language prior
(0.7.7) does not re-open it, the same way it does not re-open `re-decode` or
`mismatch`. This matters more than the other two: `lang::classify` reads kana as
`Unclear`, which is the *inherit* branch, so without the guard a correctly
re-decoded Japanese turn would be stamped `de` by the German conversation around
it seconds after being fixed.

### The prior words are kept

The replacement goes through the same path the context pass and the night shift
use, so an `operations` row is written with op `segments.redecode` and a
`prior_state` carrying the previous `text`, `asr_model_id` and `text_via`. A
Japanese re-decode is a machine's edit of a transcript and is comparable and
revertible exactly like any other. `asr_confidence` is cleared with the words it
was about.

### When it fires

Per turn, after the speaker is known and before the thread prior runs:

1. speaker tagged **exactly** `ja` → the Japanese decoder directly, no
   identifier call;
2. speaker tagged exactly something else → not this feature's business
   (`correct_language` owns that row);
3. transcript reads as German, English, or anything `guess_other` is confident
   about (Japanese included) → **nothing, and no identifier call**;
4. otherwise — `Unclear` or no words, which is what a transliterated Japanese
   turn looks like — the spoken-language identifier is asked, and if it says
   `ja` at or above `[asr].lid_min_confidence` the Japanese decoder re-reads
   the turn.

Turns shorter than `[lang].arbiter_min_duration_s` (1.5 s) are not offered to
either model.

The replacement is then judged before it may overwrite anything: it must be
non-empty and must **contain kana**. A Japanese-only decoder run over German
audio does not produce kana, so a false positive from step 4 costs one decode
and changes no row. The arbiter's `arbiter_min_words` floor deliberately does
not apply — Japanese has no spaces, so every correct answer would be one "word".

### Status counters

`status` gains two, and they are read as a pair:

* `lid_checked` — turns whose transcript nobody could read and which were
  therefore played to the identifier. This is what the feature **costs**.
* `routed_ja` — turns the Japanese decoder re-read and replaced. What it
  **buys**.

`lid_checked` climbing while `routed_ja` stays at zero is not a Japanese
problem; it means a lot of transcripts are unreadable and the thing to look at
is why.

### Models

Both optional, both installed by `recalld models fetch --japanese` (~605 MB),
and one group because either alone does nothing:

| role | export | bytes |
|------|--------|------:|
| `japanese.asr` | `sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8` | 489,389,564 |
| `lid` | `sherpa-onnx-whisper-tiny` | 116,204,861 |

`[asr].japanese` (default `true`) means "use them if they are there", so a
machine that never fetches them is unaffected. Without them a Japanese turn
behaves exactly as it did in 0.10.3.

Note for anyone reading the catalogue: whisper **tiny**, not base, and that is a
measurement rather than a saving. Base has the better Japanese recall and hears
3–5% of English as Japanese; tiny heard zero of 400 German and English
utterances as Japanese at any length. Numbers in `spike/FINDINGS.md` §22–§24.

## 0.11.6 — the same route, for Korean and Chinese

§24 wrote down what 0.11.0 did not cover: a Korean or Chinese speaker in the
same lobby got **precisely** the failure the Japanese round had just fixed. The
multilingual decoder does not speak either, does not say so, and transliterates.
This round closes that, and nothing above changes — the route, the guards, the
`lang_via: "lid"` provenance, the `segments.redecode` operations row and the
duration floor are all the same mechanism with two more arms.

### What is different: two decoders, not one

The obvious move was one model for all three, and the bench said no. Rule going
in: one decoder if SenseVoice-Small came within 2 CER points of the Japanese
Parakeet on Japanese at 3 s (`spike/asr_cjk.py`, 200 FLEURS utterances per
language, FINDINGS §27):

| decoder | lang | full | 3 s | RTF at 3 s |
|---------|------|-----:|----:|-----------:|
| `ja-parakeet-tdt_ctc-0.6b` int8 | ja | 7.5% | 11.3% | 0.026 |
| `sense-voice-small` int8 | ja | 7.6% | **15.3%** | 0.014 |
| `sense-voice-small` int8 | ko | 9.2% | 9.6% | 0.012 |
| `sense-voice-small` int8 | zh | 10.7% | 9.6% | 0.012 |

Level on whole utterances, 4.0 points behind on the 3 s fragment a lobby
actually speaks in — twice the bar. So **Japanese keeps the Parakeet** and
SenseVoice is catalogued for Korean and Chinese, where it has no competition at
all (there is no Korean Parakeet in the zoo).

### The identifier, re-measured

Adding a target adds a way for a German turn to be stolen, so the §22 gate was
re-run on five languages (`spike/lid_cjk.py`, 200 utterances each):

| length | ja | ko | zh | de correct | en correct | de/en → any of the three |
|--------|---:|---:|---:|-----------:|-----------:|-------------------------:|
| full | 100.0% | 100.0% | 100.0% | 99.5% | 100.0% | **0.0%** |
| 3 s | 96.5% | 97.5% | 100.0% | 91.0% | 100.0% | **0.0%** |

Zero of 400 German and English utterances were heard as any of the three, at
either length. The ja↔zh confusion the shared script made likely did not
appear (0.5% one way, 0.0% the other); the cross-talk that exists runs ja↔ko at
1.5–2.0%, and the judge below makes it free.

### The judge: the script decides the tag

Unchanged in shape, generalised in content. The replacement must be non-empty
and must read as one of the languages **the decoder that produced it can
write** — two kana for `ja`, two hangul for `ko`, a Han majority for `zh`, the
same bars `guess_other` already uses. A German turn that reaches either decoder
comes back in none of those and overwrites nothing.

One deliberate refinement: the tag written to `segments.lang` is taken off the
**script of the text**, not off the identifier's reading. Forcing SenseVoice's
language changes nothing about what it writes — a Japanese clip decoded as `ko`
still comes back in kana, measured — so on the 1.5% of Japanese turns the
identifier hands to the Korean arm, reading the script keeps a correct
transcript with a correct `ja` stamp where trusting the reading would have
thrown both away. The identifier still chooses the decoder; the writing system
chooses the tag.

SenseVoice also wraps its output in metadata tags — `<|ja|>`, `<|NEUTRAL|>`,
`<|HAPPY|>`, `<|Speech|>`, `<|BGM|>`, `<|woitn|>`. At the sherpa-onnx version
this daemon links, those arrive in the result struct's own fields and the text
comes back clean (0.0% of 1200 decodes carried one). They are stripped anyway,
before anything reads the string: a transcript beginning with a Latin `<|ja|>`
would be classified as English, which is the exact class of undetectable failure
this route exists to remove.

### Status counters

`routed_ja` keeps its meaning and is joined by **`routed_ko`** and
**`routed_zh`**. Three counters rather than one total, because the three arms
have different decoders, different downloads and different accuracies — a
single number could not answer "is Korean working". `lid_checked` is unchanged
and is still the cost against all three.

### Speaker tags

`speakers.languages` accepts `"ko"` and `"zh"` alongside `de`, `en` and `ja`, so
a voice can be pinned to one and skip the identifier entirely. `yue` is **not**
accepted even though SenseVoice writes it: a tag exists only if a decoder for it
is catalogued, and nothing routes Cantonese.

### Models

| role | export | tarball bytes | sha256 |
|------|--------|--------------:|--------|
| `japanese.asr` | `sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8` | 489,389,564 | `4b0a800e…6f8d1306b` |
| `cjk.asr` | `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17` | 1,047,870,769 | `f6b2a72e…4b9ea71a` |
| `lid` | `sherpa-onnx-whisper-tiny` | 116,204,861 | `c4611699…129e66b1` |

Installed files the fetch verifies:

| path | bytes |
|------|------:|
| `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/model.int8.onnx` | 239,233,841 |
| `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/tokens.txt` | 315,894 |

`recalld models fetch --japanese` is **unchanged**: the same 605 MB pair it has
always installed. `--cjk` is a superset — 1.65 GB — because Japanese is one of
the three languages it promises and its decoder is the Parakeet. The 1 GB
tarball installs 239 MB: it also carries an fp32 export and test WAVs this
daemon never opens.

`[asr].cjk` (default `true`) joins `[asr].japanese`, with the same "use it if it
is there" meaning. Two switches rather than one because there are two downloads
and two residents: a machine that only hears Korean should not hold 655 MB of
Japanese Parakeet, and turning Japanese off must not silently take Korean with
it.

Numbers in `spike/FINDINGS.md` §27.
utterances as Japanese at any length. Numbers in `spike/FINDINGS.md` §22–§24,
which also record what this does *not* cover — Korean and Chinese have the same
failure and no catalogued decoder yet.

## 0.11.8 — the other languages

Three rows from one evening, all from the user's own microphone:

```
"Mon petit chou."      1.01 s
"During apartments."   1.20 s   — French
"Wanky Daska."         1.30 s   — Japanese, "genki desu ka"
```

None of them reached the identifier, and the middle one would not have been
fixed if it had. Two separate gaps.

**Gap one: the floor.** The identifier was only asked about turns clearing
`[lang].arbiter_min_duration_s` (1.5 s). That number was measured for a
*replacement* — below it the German arbiter's own words are in the reference
only 28% of the time — and was reused for an *identification*, which is a
different question: a model can know what language it is hearing on audio too
short to transcribe usefully. **`[asr].lid_min_s` (new, default `1.0`)** is now
the floor for asking, and it governs the Japanese route too, which is what makes
"Wanky Daska." reachable.

**Gap two: French is not Japanese.** Parakeet-TDT-0.6b-v3 covers French — it
decodes FLEURS French at 18–22% WER. It did not fail to spell the language; on
a 1.2-second fragment it **committed to the wrong one**, which is the flip
`crate::arbiter` has handled for German since 0.7.7. The German arbiter never
saw this turn because it is reached from *text*, and "During apartments." is two
ordinary English words.

#### When it fires

After the Japanese route and only on a turn it left alone. It re-uses that
route's identifier reading rather than asking again — LID is the only cost
either feature has — so the pre-filter is identical: a transcript that already
reads as German, English, or anything `guess_other` is confident about is never
asked about at all.

If the reading names a language in `[asr].polyglot_languages` (default `fr`,
the one language with a WER gate behind it; `es` and `it` had their
identification half measured and their benefit half not, and join by being
named, as do `pt`, `nl` and `pl`, which were not measured at all) the turn is
re-decoded with the decoder forced to that language, and the result is judged
before it may overwrite anything. It must be:

1. caption-stripped and at least `[lang].arbiter_min_words` long;
2. **not an echo** of the words already on the row — an arbiter whose correction
   is the existing transcript has found nothing;
3. **confidently readable as the language the identifier named**, by
   `guess_other` — the bar `segments.lang` is written at, not the looser one
   that only queues a translation.

Guard 3 is what stands in for the Japanese route's kana test, and it is weaker
by nature: Whisper forced to French over German audio produces real French
words, and there is no script to appeal to. The safety therefore comes from all
three narrowings together. Measured end to end over 900 German and English
FLEURS cuts at 1.0/1.5/2.5 s, **one** survived every guard — 0.11%, all of it
German, all of it at 1.0 s (FINDINGS §28).

A row it settles is shaped exactly like a Japanese one: `lang_via: "lid"`,
`text_via: "lid"`, an `operations` row with op `segments.redecode` carrying the
prior text, and `lang` set to the code the identifier named — after which the
live translation queue picks it up with no knowledge that this route exists.

#### The decoder is the night shift's, and that is the finding

`whisper-large-v3-q5_0` on the GPU, one `whisper-cli` per turn, forced to the
language. The route is therefore **conditional on `models fetch --night` *and*
`models build-night`**, and on the GPU being under `[night].gpu_busy_max_pct`
at that moment; without either it does nothing and the turn keeps the words it
has.

That dependency was not the plan. The intended backend was the German arbiter's
whisper-base, already on disk, which would have cost zero new bytes — and it is
**worse than doing nothing**. On the turns this route actually hands it, base
moved WER by −54.7% (fr, 1.0 s), −63.7% (fr, 1.5 s), −195.0% (es, 1.5 s) and
−84.4% (it, 1.5 s), making 10–83% of touched rows worse. large-v3 moved it
+62.4% and +31.2% on French at 1.0 and 1.5 s, with **0%** made worse.

The sign flip is the difference between the two halves of this section. The
Japanese arbiter competes against a decoder that cannot spell the language at
all, so any kana wins. This one competes against v3 speaking good French, so an
arbiter has to beat a strong decoder having a bad second — and only the large
model does.

#### Status counters

`status` gains `routed_other` (turns re-decoded by this route) and a
per-language split beside it. `lid_checked` is unchanged and now covers both
halves: it is still what the identifier costs.

## 0.11.0 — partial turns

Provisional words on the glass while somebody is **still talking**. One new event,
no new topic, no new row, nothing stored.

```json
{"seq": 41830, "ev": "partial", "data": {
  "session": 3, "source": "VRChat.exe",
  "speaker": 7, "speaker_hint": "proximity",
  "t_start_ms": 1772486400123, "t_start_ns": "1772486400123456789",
  "elapsed_ms": 2400,
  "text": "but in his hands solitude",
  "seq_in_turn": 2,
  "final": false
}}
```

It rides on the **`segments`** topic, so no client changes its subscription and one
too old to know the event ignores it (Versioning rules).

### When one is emitted

While a turn is OPEN — from the first voiced frame, not from the moment the VAD
closes a span — subject to three rules, all of them about never costing a recording:

| rule | key | default |
|---|---|---|
| at most one partial per | `[asr] partial_every_ms` | 1000 |
| and only once the turn has lasted | `[asr] partial_min_ms` | 800 |
| and never while the inference queue holds more than | `[asr] partial_backlog_max_s` | 5 s |
| the feature at all | `[asr] partials` | see §20 |

The backlog rule is the same rule and the same reason as `[graph] max_queue_seconds`:
the capture queue drops the **oldest** audio when it overflows, so a partial that put
the inference thread behind would be paying for a caption with a lost turn.

**Pausing stops partials instantly.** The pipeline discards the session's whole state
on the first paused buffer, and the emit path re-checks the pause flag both before and
after the decode — the same discipline `write_segment` uses, for the same reason.

### Each decode is over the WHOLE open turn

Not the newest chunk — the whole turn so far, every time, so the text **converges**.
Parakeet v3 is an offline transducer; there is no streaming API and this feature does
not add one. (sherpa-onnx does ship online zipformer models. They are English-only and
would be a second model in RAM, which is exactly the cost this feature is not allowed
to have.) The reason for re-reading from the top is measured: FINDINGS §12 puts a 1.5 s
slice decoded alone at **56.7% WER** against **20.4%** for the same slice with context
around it. A partial built from the last second would be noise that never improved.

It runs **on the inference thread, through the same recogniser instance** the finished
turn will go through. No second model, no second thread.

### Replacing the provisional row

When the turn closes, the ordinary `segment` event follows. There is **no**
`partial_of` field and no id to match on: a client replaces the provisional row by
matching **`(session, t_start_ns)`** — the same turn has the same start, and the
`segment`'s `t_start_ns` is exactly the partial's. Both are strings, as every `_ns`
field on this protocol is.

`seq_in_turn` counts partials **within one turn**, from 0, and restarts on the next
turn. It is for a client that wants to say "still listening" versus "settling", not
for ordering — `seq` already orders everything.

### The speaker on a partial is a guess or nothing

**No embedding is ever computed for a partial.** The identity ladder only labels
finished turns: it needs an embedding, an embedding needs the overlap gate's verdict,
and both are decided over the completed audio. So:

| `speaker_hint` | means |
|---|---|
| `"proximity"` | the previous turn in this session ended **less than 2 s** ago; `speaker` repeats that turn's identity because the same person is probably still talking |
| `null` | `speaker` is `null` too — nothing recent enough to borrow |
| `"match"` | reserved. The daemon never emits it on a partial; a client should render it like a settled label if a future version does |

A client MUST render a proximity speaker as uncertain. It is the cheapest true thing
available, not a reading of the voice.

A partial carries no `speaker_name`, and since 0.12.0 it carries no `colour`/`icon`
either: a client resolves both from `speakers.list` against the `speaker` id. See
"One deliberate exception: `partial`" in the highlight section for why.

### Never stored, never replayed

A partial is not a row, is not in `segments.list`, and is **not in the `events.since`
replay buffer** — it is published ephemerally (`bus::publish_ephemeral`). It still
consumes a `seq`, because `seq` is the stream's clock and clients dedupe on it; a
replay therefore has a hole where a partial went, which is the same hole a subscription
filter has always been allowed to leave. Clients must not assume contiguous `seq`.

### What a client is expected to do with it

Show it as **provisional**: lighter ink, a trailing `…`, updated in place, replaced by
the final row without the row jumping. Never count it, never put it in history, never
export it. A provisional row with no update and no `segment` for several seconds should
be dropped — a pause, a discarded turn after an audio gap, or a daemon that went away
all end a turn with no event to say so.

## 0.11.0 — the translator

0.9.0 translated a turn by asking a chat model to. This round gives translation
a model of its own — **NLLB-200-distilled-600M**, int8 ONNX, in-process through
the `ort` the daemon already links — and makes the 0.9.0 path the alternative
rather than the only one.

**Nothing on the wire changes.** `translation` still carries `{lang, text, via}`
and `via` still names the model that wrote the row; a client that has never
heard of this round cannot tell which backend answered unless it reads `via`.
There is no new method, no new event and no new field. The whole of this section
is a config key, a fetch flag, and one function the live-translation path calls.

### `[assist]`, two keys

| key | default | meaning |
|---|---|---|
| `translator` | `"nllb"` | `"nllb"` or `"qwen"` — which backend translates |
| `translator_threads` | `4` | ONNX intra-op threads for the NLLB backend |

An unrecognised value reads as the default rather than switching the feature
off: a typo in a config file must not silently stop translating, and it must not
quietly select something nobody named.

`translator = "nllb"` needs the model. **A backend whose files are not on disk
is not selected** — the daemon falls back to the other one and logs the fetch
command once, because a person who turned translation on asked for translation,
not for a particular model. `translator = "qwen"` reproduces 0.9.0 exactly.

### `recalld models fetch --translator`

Three assets, 911 MB, one opt-in group (`Group::Translator`): an encoder, a
merged decoder-with-past, and the tokenizer, all pinned to an upstream commit
and byte-verified like every other catalogue entry.

**Licence: CC-BY-NC 4.0.** NLLB-200 is published non-commercially. That is fine
for a private personal install and it is a hard blocker for anything sold —
which is why this is a flag with the licence written next to the bytes rather
than a file added to the default set. `models status` says the same thing in its
group note.

### `via`, and what a client should do with it

`translation.via` becomes `nllb-200-distilled-600m-int8@1` for rows this backend
writes, against `qwen2.5-3b-instruct-q4_k_m` for rows the old one wrote. The
`@1` is a contract version and is bumped when anything about how a translation
is produced changes, so a row translated under one contract is never mistaken
for a row translated under another and the whole lot can be found and re-run.

A client needs no change. One that wants to may show the backend on a row's
detail; it must not present a row's `via` as a *setting*, because the setting is
what the next row will use and `via` is what this one did.

### `translate::translate_line(text, src, target) -> Option<Translation>`

The seam the live path calls, and the reason this round is worth having at all.

A translation was an **idle pass**: a worker woke up, took eight rows, and spent
3.7 seconds a row on a 1.9 GB child process. That is fine for filling in last
night and useless for a captions window. The NLLB backend translates a
two-to-five-word turn in a **median 0.85 s** in the same process with no
launch — measured over 180 short lines, against 2.48 s for the old path — which
is the difference between "translated eventually" and "translated".

`translate_line` is deliberately the dedicated translator **only**. When
`translator = "qwen"`, or when the model is not installed, it returns `None` and
the ordinary idle pass picks the row up later, exactly as 0.9.0 did. Spawning a
1.9 GB process per line is not a live path and pretending otherwise would make
the captions window worse, not better.

`src` is the row's language stamp when it has one; `None` asks the classifier,
and a line **nothing can name a language for is not translated**. That
asymmetry is the same one 0.10.2 built the third-language queue on: a mumbled
German turn and a French turn are the same NULL in the column, and the
difference between them is the only thing that stops "translate what I cannot
read" from meaning "translate everything".

### The guards are unchanged, plus one

`translate::judge` is shared by both backends: an echo is dropped, an answer
that reads as the wrong language is dropped, an empty answer is dropped. 0.11.0
adds a **length sanity check** — an answer longer than three times its source,
with a floor of twelve words, is `TooLong` and is dropped. A greedy decoder that
starts repeating itself does so at length, and a paragraph under a two-word row
reads as something that person said at length.

The length is measured in units that survive a change of script: words, or one
per six characters, whichever is larger. Japanese has no spaces, so a whole
sentence of it is one *word*, and a guard built on the word count alone would
have thrown away every honest translation of a Japanese turn.

### Why, with numbers

FINDINGS §23. 13 FLEURS directions, 100 parallel sentences each, both systems on
the same four pinned cores: NLLB wins on chrF on **13 of 13** pairs, mean 57.75
against 48.29, with **no regression anywhere**. The old path echoed its input
back — untranslated, and therefore dropped by the guard — **171 times in 1300**,
concentrated in the languages a 3B cannot recognise: 34/100 on Finnish, 36/100
on French into German. NLLB echoed **none**.

The gate to switch the default was: chrF wins on ≥ 80% of pairs, no pair worse
by more than 2 chrF, fewer empties and echoes, lower median latency. All five
passed.

## 0.11.4 — provenance, made consistent

- `text_via` gains `"lid"`: the row's words were re-decoded by the Japanese
  decoder after the spoken-language identifier heard Japanese. A row that says
  `"arbiter"` is a German/English flip re-read by a constrained decoder; the
  two are different claims about the audio and now read differently.
- The language arbiter's rewrite (`text_via: "arbiter"`) follows the same
  three rules as the context pass and the night shift: the words it replaces
  are kept in an `operations` row `op: "segments.redecode"` whose
  `prior_state` carries `{segment_id, text, asr_model_id, text_via, route}`;
  the cross-check verdict about the old words is cleared (`asr_confidence:
  null` until the pass looks again); and any translation of the old words is
  dropped. Up to 0.11.3 this path did none of the three.

## 0.11.6 — a digest names people

- **`digest.list` rows and the `digest` event** carry the paragraph as prose
  about people: `summary` and `open` say `Aspen`, `Speaker 07`, `You` — the
  same words `participants[].label` uses, which now renders a generated label
  the way a sentence has to (`Speaker 07`, not `Speaker_07`). Three new
  fields: **`summary_raw`** and **`open_raw`** (what the model wrote, so
  nothing is lost) and **`rendered`** — `"names"` for a digest written since
  this change, `"legacy"` for one written before it. See the digest section
  for the substitution rule the legacy path uses and why there is no
  `digest rerender`.
- Additive only. A client that reads `summary` and ignores the rest sees the
  same field it always did, with names in it.

## 0.12.0 — Discord's word, applied; and the second client

Three things, and the third is the reason the other two are shaped the way they
are. The daemon had 168 turns it could name and had not; it had 137 turns queued
for an enrolment pass that was switched off with nothing saying so; and it had a
verdict, `nobody`, that it had been reading as a fact about a *person* when on
this install it was a fact about *which of two Discord clients the plugin was
sitting in*. The first two ship. The third ends two features that were designed
and measured and then refused, and ships the one column that makes the question
answerable next time.

### `label_via = "truth"` — Discord names the blank turns

A fifth value for `segments.label_via`, beside `match`, `mic`, `proximity` and
`manual`. It means: the identity ladder declined to name this turn, Discord's
verdict for it was `single`, and the account that owned it was already linked to
a voice. `match_score` is NULL, as it is for `proximity`, because nothing was
compared.

**`recalld truth label`** lists what it would name; `--apply` writes; `--limit N`
stops after N. Also runs nightly inside the truth worker, after the auto-linker
and never before it (a user linked tonight has their backlog named tonight).

Four things it will not do, by construction rather than by flag:

- **It never overwrites.** A row that already has a speaker is not a candidate —
  ladder, person or proximity, it is left alone. So the pass cannot move the
  identity precision `truth report` prints: every row it writes was counted
  `unlabelled` before and none was counted `correct`.
- **It never enrols.** Not one prototype. The enrol bar is deliberately not
  learned, and a pass that added prototypes on Discord's word would be that bar
  learning itself through the side door. `[truth] enrol` remains the supervised
  route.
- **It never mints.** Only accounts already linked to a voice are read.
- **It never uses your own account.** A `single` verdict naming *you* is not
  evidence about audio captured from your own Discord client — that client never
  plays your microphone back to you, so yours is the one voice the stream cannot
  contain. 0.10.1 established this for scoring (FINDINGS §17, 73% → 88%); 0.12.0
  inherits it for labelling, where getting it wrong would have put the user's
  name on 17 turns of somebody else's voice, permanently.

There is **no duration bar**, which makes it the only truth pass without one.
Every other one is *measuring* the voicebank, where a sub-second turn measures
the floor rather than the model. This one measures nothing; it copies a name.
Discord's word about a one-second turn is as good as its word about a ten-second
one, and the short turns are the ones no other route can ever name.

Each `--apply` logs one **`truth.label`** operation per 200 rows, carrying every
segment's prior state in `speakers.split`'s shape, so the pass is reversible as a
class.

### `truth.summary` — the two queues, said out loud

Two new objects. Additive; a client that ignores them sees 0.11.8's reply.

```json
{"enrol": {"on": false, "waiting": 137,
           "min_duration_ms": 3000, "min_coverage": 0.95},
 "retro_label": {"waiting": 168}}
```

`enrol.waiting` is the count of turns that *would* be considered if `[truth]
enrol` were true. It exists because of the shape of §29's investigation: the
enrolment pass had never once fired, `segments.truth_enrol_ns` was NULL on all
1,461 `single` rows, and the cause was neither a bug nor a bar the data could not
reach — the feature was off by default and had never been turned on. Nothing
anybody could run said so. A switch nobody can see is indistinguishable from a
bug, and an evening went into telling them apart. `"off"` is a setting;
`"off, with 137 turns waiting"` is a decision. `recalld truth report` prints both
queues whenever they are non-empty.

### Schema v14 — `sessions.instance_key`

One nullable column, no backfill: for a session already on disk the answer is
genuinely unknown, and NULL is the only honest way to say so. NULL is **not**
"instance one".

It holds `serial:<object.serial>`, or `pid:<application.process.id>` where
PipeWire gave no serial — both already read by `capture::node_info_from_props`
and, until now, thrown away at `Shared::attach`.

**Why it is on the session and must never go on the source.** `sources.match_key`
comes from `application.process.binary`, so two copies of one application share
one source row. The obvious fix — suffix the key with the pid — is a trap:
`allowlist::decide` looks that key up in the `[rules]` table by exact string, so
`"vesktop#4711"` matches no rule, falls through to default-deny, and **capture
silently stops for an app the user explicitly allowed**. `[rules.X]` would grow
an entry per launch, `recalld allow <KEY>` would need a number a human cannot
know, and the GUI's source card and per-source search filter are keyed the same
way. A session, by contrast, is already per-PipeWire-node — two instances already
open two concurrent `sessions` rows against the one source — so the identity has
a home that costs nothing.

What the column does **not** do is say which instance the ground-truth plugin is
sitting in. That needs the plugin to name the call it is watching, which is a
wire change and is not in this round.

### The two-client failure mode

Read this before trusting a `nobody` verdict for anything.

`truth_verdict = 'nobody'` means *no account the RecallBridge plugin can see
reached 20% coverage of this turn, and the plugin was running*. It has always
been documented as "a real disagreement worth looking at". On an install running
**two Discord clients** it is frequently not a disagreement at all:

- Both clients are Vesktop, so PipeWire names both nodes `vesktop` and both
  collapse into one `sources` row and, on the evening §29 measured, into one
  session.
- Only one client carries the plugin. The other's call — different account,
  different channel, different people — arrives through the same source with no
  speaking spans behind it at all.
- The plugin's own call is live and noisy, so `truth_spans_between` finds spans
  within five minutes and the verdict is `nobody` rather than `unknown`.

The result is a confident-looking `nobody` on turns of real people the plugin was
never able to see. On the measured install that was **190 rows across four
voices, every one of them a person the user had named by hand within two minutes
of the voice being minted**. Neither of the two obvious daemon-side repairs
works, and both were measured before being dropped (§29): a session-level rule
("a session with no positive verdict is not the bridge's call") rescues 31 of 350
rows and not one of the 190, because both calls shared session 305; a voice-level
rule ("a voice that never once got a positive verdict") rescues 20 and not one of
the 190, because cross-talk from the local user gave all four voices `single`
verdicts of their own.

Consequences, which are contracts and not advice:

- **`nobody` is not evidence that no human spoke.** Nothing may unassign a label,
  refuse a mint, or downgrade a voice on the strength of it. 0.12.0 designed both
  a `identity repair --media` and a mint guard keyed on `nobody`, measured them,
  and shipped neither; §29 has the numbers and the reasoning.
- **`nobody` remains excluded from every score,** as it has been since 0.9.0.
  Nothing about the identity or overlap numbers changes.
- The honest fix is to stop merging the two instances, which is what
  `sessions.instance_key` begins and a plugin that names its call will finish.
## 0.12.0 — the archive sweep for language

Every language decision is made once, on the way in, by whatever was shipped
that evening. The spoken-language identifier arrived in 0.11.0, Korean and
Chinese in 0.11.6, French and a one-second floor in 0.11.8 — and none of it
reached a row captured before it. On the install this was measured against,
**7,428 non-deleted rows have `lang: null`**, 4,141 of them at least a second
long with their audio still on disk.

`recalld lang sweep` walks them, and the same pass runs nightly while
`[asr].lang_sweep` is on, in the night shift's clock window (`[night].window`
and `[night].also_when_idle_min`) but **not** behind `[night].enabled` — the
night shift needs a gigabyte of whisper and a local compile, and this needs a
13 MB identifier.

#### What it writes

- **`lang_via` gains `"sweep"`** — an eighth value, and it appears in **two
  shapes**:
  - with `lang` set to `"de"` or `"en"`, on a row the identifier heard as one
    of the two languages the routes deliberately never act on. The words are
    **not** touched: there was nothing to re-decode, so the honest record is
    the reading and no more.
  - with `lang` still `null`, on a row the identifier heard as something
    nothing acts on, or had no opinion about. Nothing is claimed; the mark is
    there so a bounded, resumable walk does not pay for the same model pass
    every night. The same shape as `"mismatch"`.
- It is **not** `"lid"`, and a client must not read it as one: `"lid"` promises
  that a decoder re-read the turn and its words are on the row, and a `"sweep"`
  row never had a re-decode.
- A `"sweep"` stamp is **excluded from the conversational language prior**
  (`Store::thread_language_stamps`), alongside `"context"`. One second of audio
  nobody could read, judged by nothing, is not evidence about what language a
  conversation is in.
- Rows marked `"mismatch"` are **never** swept: that backlog is
  `recalld lang repair`'s, and overwriting the mark would silently empty its
  queue.
- Rows a person has corrected (`operations` `op: "segments.correct"`) are never
  swept, the same guard the night shift's queue carries.

#### What it does not write, and why that is the finding

**Off by default, `[asr].lang_sweep_redecode = false`: the sweep does not
replace transcripts.** With `lang` written and the words left alone there is
nothing here a client has to re-render.

The routing half was measured and refused (FINDINGS §29). Run over 1,796
untagged rows at the live operating point, the identifier named a routed
language for 342 of them — 131 Korean, 67 Chinese, in an archive where nobody
has ever spoken either — and 44 survived every judge, 36 on voices declared
German/English-only. That is 2.30% against the **1% of de/en** gate the routes
shipped under. Asking the identifier over three overlapping windows that must
agree brings it to 0.97%, inside the gate — and a hand check of all nine
resulting rewrites says **eight are wrong**. `"Okay."` became `、お疲さ`;
`"Oh, she has this detected beim sonar."` became a fluent French sentence
nobody said.

The gate is not wrong, it is being asked the wrong question. On the live path
the rows that clear these guards are overwhelmingly real foreign turns and the
false positives are a residue. On an archive of German and English there are
almost no real foreign turns to be right about, so the residue is the whole
output. **A 1% false-positive budget is a tax on a benefit, not a substitute
for one.**

`recalld lang sweep --apply --redecode`, or `[asr].lang_sweep_redecode = true`,
turns it on. A row it settles is then shaped exactly like a live one:
`lang_via: "lid"`, `text_via: "lid"`, the decoder's `asr_model_id`, and an
`operations` row `op: "segments.redecode"` carrying the prior text — because it
is the same code, called in the same order.

#### The two knobs that make it stricter than the live path

Both are one-directional: an operator may make the sweep stricter than the live
route and never looser.

- **`[asr].lang_sweep_windows`** (default `3`, live `1`). Three overlapping
  identifier windows at `lid_min_confidence = 1.0`, so a reading has to survive
  being asked about three different parts of the same turn. Three model passes
  at RTF 0.027 is a price a nightly batch can pay and a live turn cannot.
- **`[asr].lang_sweep_min_s`** (default `1.5`, live `1.0`). Below 1.5 s nothing
  is ever kept anyway — `[lang].arbiter_min_duration_s` is the replacement floor
  and both decoders refuse under it — so the 974 archive rows in that band cost
  a model pass each and produced no rewrites at all.

`[asr].lang_sweep_rows_per_run` (default `400`) bounds a nightly run, and counts
**rows a model was spent on**, not rows looked at: 2,345 of the 4,141 are
declined by the text pre-filter for free, and a budget spent walking past those
would take a fortnight to reach the first row worth asking about.

#### The CLI

```text
recalld lang                     adds two lines: the sweep bar, and how much
                                 archive is owed against how much is swept
recalld lang sweep               preview. Runs the identifier; decodes nothing,
                                 writes nothing. Prints what it heard and what
                                 --redecode would have rewritten.
recalld lang sweep --apply       write the language, never the words
recalld lang sweep --apply --redecode    also let the decoders rewrite
```

Bounded (`--limit`, `--batch`), resumable, idle-priority, and safe to run while
the daemon is capturing: the work list is a query rather than a cursor, and
every row a model is spent on leaves it.

---

## 0.12.0 — the lobby is not FLEURS: three guards on the audio route, and a way back

The spoken-language route (0.11.0 for `ja`, 0.11.6 for `ko`/`zh`, 0.11.8 for
`fr`) shipped behind a false-positive gate measured on FLEURS: **zero of 400**
German and English utterances heard as any of the three, at any length. On the
install this was written for, it had rewritten **45 archive rows** — 32 `ja`,
12 `zh`, one `fr` — and **37 of them belong to one voice: the user's own
microphone, declared `["de","en"]**:

```text
"Mm-hmm."                    3.15 s  ->  うん
"Okay, yeah."                3.89 s  ->  ok看嗯
"Right."                     2.32 s  ->  可以嗯
"Yeah."                      2.38 s  ->  没
"Yeah, Gott was zu trinken." 2.32 s  ->  よしじゃあ。
"Uh"                         2.00 s  ->  Au revoir.
```

Two of the 45 look right. FLEURS is read news; a lobby is people saying
"Mm-hmm." at each other, and neither the identifier nor the judge had ever been
asked about that. FINDINGS §31 has the tables.

Nothing on the wire changes shape. `lang_via: "lid"` and `text_via: "lid"` mean
exactly what 0.11.0 said they mean; there are simply far fewer of them, and one
new `operations` op for taking the old ones back.

### The three guards

They are all in `asr_cjk::pre_route` and `asr_cjk::judge`, which both routes
share, so the French arm gets them without a second copy.

1. **A declared set with nothing routable in it is a declaration.** Before
   0.12.0 only a *sole* declaration stopped the route (`lang::sole_language`),
   so a voice declared `["de","en"]` fell through to the identifier on every
   unreadable turn. Now any non-empty declared set that contains no tag either
   route can decode — `ja`/`ko`/`zh`, or a tag in `[asr].polyglot_languages` —
   is left alone. **A person who names their languages has answered the
   question; two answers are still an answer.** The sole-language fast path to a
   decoder is unchanged, and a set that *does* contain a routable tag
   (`["en","ja"]`) still falls through, because a Japanese speaker's English
   turn is not a mistake.
2. **A back-channel is never worth a second decoder.** A turn whose transcript
   has fewer than **two** words outside `lang::FILLERS` — `mm`, `mhm`, `uh`,
   `yeah`, `okay`, `right`, `ja`, `ach`, `genau`, and thirty more built from the
   prior transcripts of those 45 rows — never reaches the identifier. Two, not
   three, because the three rows this feature exists for are "Sima Sen Okenki
   Deska." (4 content words), "Wanky Daska." (2) and "During apartments." (2).
   A turn with **no words at all** still reaches the identifier: a decoder that
   gave up entirely is exactly the turn worth re-reading, and both of the two
   genuine Japanese rows on this install are that shape.
3. **The script test is necessary and not sufficient.** `うん`, `没`, `嗯嗯`,
   `フフフフフフフ` and `ok看嗯` are all written in a script only the target
   languages use, which is all the 0.11.6 judge asked for. A re-decode must now
   also carry at least 4 letters **and** at least one per second of audio, have
   more than half its letters distinct (a repetition loop is what these decoders
   do with noise), not be a bare interjection in the target script, not be more
   than a third Latin, and not mix hangul with kana. There is no decoder
   confidence to lean on instead — sherpa's offline result carries `text`,
   `lang`, `emotion` and `event` and no score.

Rejections are logged with which guard fired.

### `[asr].lid_windows` now defaults to `3`

The identifier is asked about three overlapping windows of the turn and they
must all agree (`lid_min_confidence` is still `1.0`). Measured on the 45 rows
plus 200 German/English back-channels from the same voice, with guard 1
deliberately switched off so the question is about the identifier alone:

| windows | floor | asked | rewrites kept | false positives | rate |
|--------:|------:|------:|--------------:|----------------:|-----:|
| 1 | 1.0 s | 94 | 9 | 7 | 7.45% |
| 1 | 1.5 s | 59 | 9 | 7 | 11.86% |
| **3** | **1.0 s** | **94** | **2** | **1** | **1.06%** |
| 3 | 1.5 s | 59 | 2 | 1 | 1.69% |

One window misses the 1% gate by seven times; three windows lands on it. The
floor moves nothing — it drops rows out of the denominator and none out of the
numerator. `[asr].lid_min_s` therefore stays at `1.0`, and
`[asr].lang_sweep_windows` stays a separate field at `3`: the narrowing is
one-directional, and an operator who lowers `lid_windows` for a machine short of
cores must not thereby lower it for four hundred archive rows decided at 04:00.

Three passes cost three times one, and what pays for it is guard 1 and guard 2:
on that same data they take the turns the identifier is asked about at all from
245 to **one**.

### `operations` gains `segments.unroute`

Written once per row by the repair below. `prior_state` carries the state being
**discarded**, so the repair is itself undoable:

```json
{
  "segment_id": 15045,
  "text": "うん",
  "asr_model_id": "sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8@1",
  "text_via": "lid",
  "lang": "ja",
  "lang_via": "lid"
}
```

The row it is written for goes back to the `text`, `asr_model_id` and
`text_via` recorded in that row's own `segments.redecode` operation, and its
`lang` and `lang_via` both go to **null** — not to `"sweep"`. A `"sweep"` mark
would say "asked, nothing to say", and what happened is that the answer was
withdrawn; a null puts the row back on the sweep's work list, which is where a
row nobody has a reading for belongs. `asr_confidence` and any `translation`
are cleared with the words they were about, exactly as `set_segment_text_via`
clears them.

A client showing a row that has been unrouted sees the live transcript back,
`lang: null`, `lang_via: null` — the state it would have been in had the route
never run.

### `recalld lang unroute`

```text
recalld lang unroute            what it would put back, row by row, with the
                                guard that refused each one. Writes nothing.
recalld lang unroute --apply    do it.
```

A row goes back when **the code as it stands today would not have written it**:
the shipped guards are re-run against the speaker's declaration and the words
the route replaced, and — where the identifier is installed and the clip is
still on disk — the identifier is re-run over the audio as well. A row the new
guards still accept is not touched. A row with no recorded `segments.redecode`
operation is reported and left alone: there is nothing to restore and inventing
a prior transcript would be worse than the row it was fixing.

Without the identifier the pass still runs on the stored text and durations
alone, which can only ever put back **fewer** rows, never more.

**Order matters.** The decision reads the database as it is now, so a voice that
has since declared `ja` clears guard 1 and its rows are then judged on guards 2
and 3 alone — which on this install keeps ten rewrites, two of them right. Run
the repair **before** widening a declaration, not after.

## 0.12.1 — the sweep can finish (schema v16)

`recalld lang sweep --apply` never emptied its work list. On the live install it
printed the same two lines after every run:

```text
  1729  already readable, or a voice pinned to a language another pass owns
1730 still owed
```

The 53 rows that needed the identifier were done on the first pass and stayed
done. The 1,729 that [`asr_cjk::pre_route`] declines — a transcript that already
reads as something, or a voice pinned to a language `correct_language` owns —
were counted as owed every time, so the nightly pass re-walked all of them every
night and no output could ever say the archive was swept.

0.11.9 left those rows unmarked on purpose: they cost a string compare and no
model, a declaration can be added tomorrow, and marking them would freeze a
decision that was free to re-make. That reasoning was right about the future
decision and wrong about the arithmetic, and the cause of both is that
`lang_via` was being asked two questions — *how did this row get its language*
and *has this pass been here* — that only one column can answer at a time.

#### `segments.sweep_at_ns` (new, schema v16)

- **A visit, not a claim.** Nullable; set to the moment the sweep reached a
  conclusion about the row, whatever the conclusion was: re-decoded, stamped
  `de`/`en`, asked and nothing to act on, audio gone, **and declined by the
  pre-filter**. Nothing but `crate::sweep` reads it, which is what makes marking
  a declined row safe.
- **The one free outcome that is not a visit** is a clip under
  `[asr].lang_sweep_min_s`. What would change that answer is configuration, not
  data, so no write to the row could ever clear the mark; an operator who lowers
  the floor must get those rows back, and does.
- **It is cleared whenever something `pre_route` reads changes**, which is the
  0.11.9 guarantee kept by a different mechanism. The row's words
  (`set_segment_text_via`, `set_segment_analysis`), its voice
  (`set_segment_speaker_via`, `merge_speakers`), or that voice's declared
  languages (`set_speaker_languages`, i.e. `recalld languages`) each hand the
  rows concerned straight back to the next run. Declaring a voice Japanese now
  re-opens its whole history to the sweep, which is the retroactive behaviour
  `recalld lang repair` has always had.

#### `lang_via: "sweep"` now means one thing

It is written **only with `lang` set**, to `de` or `en`. The bare form 0.11.9
also wrote — `lang_via: "sweep"` with `lang: null`, as an "asked, nothing to
say" mark — is gone; that was the second question, and it lives in
`sweep_at_ns` now. **The v16 migration renames the existing ones**: every row
carrying the bare form gets `sweep_at_ns = 0` (the honest answer to *when* is
"before this migration") and its `lang_via` back to null. Rows with the stamp
form are untouched. A client that special-cased the bare form can drop that
branch; one that reads `lang_via` for provenance sees strictly fewer values.

#### The CLI says when it is done

- `recalld lang sweep` and `--apply` both end in `0 still owed — the archive is
  swept` once nothing a model pass could change remains. A **preview** writes
  nothing, so its own count cannot fall; it subtracts the rows it just resolved
  and reports what `--apply` would leave, rather than printing the number the
  operator started with.
- `recalld lang` reports `sweep   N never asked about, M already swept`, where
  `M` counts visits and therefore includes rows whose language the sweep set.

## 0.12.1 — the verdicts on disk, re-judged

The own-account rule above changed what a verdict *means*, and every verdict on
disk was written by the old meaning. Leaving them is not "old data": the overlap
gate's precision and recall, the calibration corpus and `identity calibrate`'s
held-out gate curve all read those rows, so a stale `overlap` pile is a wrong
answer key that keeps being marked against. FINDINGS §34 has the before/after.

**`recalld truth rejudge`** lists what would move; `--apply` writes; `--limit N`
stops after N. No schema change: the verdict is recomputed from the speaking
spans exactly as the nightly pass computes a fresh one, `truth_overlap_frac` is
re-measured under the same rule, and each `--apply` logs one `truth.rejudge`
operation per 200 rows carrying every segment's prior verdict.

Only `single`, `overlap` and `partial` are examined, and that is a closed
argument rather than an optimisation: the rule only ever *removes* presence, and
removing presence cannot turn a `nobody` into anything or make an `unknown`
known. The pass is therefore bounded by the verdicts that assert somebody was
talking — 3,781 rows on the install it was measured on, not the 9,770 with a
verdict of any kind.

Where the speaking spans have been purged:

| stored verdict | what the pass does |
|---|---|
| `single`/`partial` naming **you** | `nobody`, no spans needed — the verdict itself records that nobody else reached the presence bar, which is the whole question |
| `single`/`partial` naming anybody else | unchanged, and provably so: the same fact read the other way round |
| **`overlap`** | **left exactly as it is, and counted.** It records that two accounts were present and not which two; guessing here would invent the thing the pass exists to correct |

**It runs once by itself**, in the ground-truth worker, on the first start after
the upgrade — bounded to 10,000 rows, gated on `[truth] label` with everything
else in that worker (the switch that lets the daemon write verdicts is the
switch that lets it correct them), and marked done by the `settings` row it
writes. It is a pass and not a migration on purpose: it reads the speaking spans
and can take thousands of small queries, and a migration that does that runs
while the user is waiting for the daemon to come up. With `[truth] label = false`
the CLI is the route and the operator drives it.

**`truth.summary` carries `rejudge`** — `null` until the pass has run, otherwise
`{at_ms, examined, changed, overlap_reassigned, from_columns, unresolvable,
restamped, moves: [{from, to, n}]}`. `recalld truth report` prints it as a
`re-judged` block, because "450 `overlap` rows" and "450 `overlap` rows, and
1,341 more used to be counted here" are different facts about the same install
and every measurement made before the rule read the second number without
knowing it.
## 0.12.1 — per-user Discord audio

Every Discord turn this daemon has ever stored came off the **mixed** stream:
one tap on the client's output, carrying everybody at once. Whose voice is
whose has therefore been a recognition problem, and when two people talk over
each other, a separation problem too. FINDINGS §33 measured the separation
route and refused it — the artifact tax is five times the interference removed,
and it is charged by the embedder, so a better separator cannot pay it. Its
last question was whether the client could just hand the streams over
separately. On Vesktop it can.

This section is that path. It is additive: no method, event, field or behaviour
described above this line changes, `proto` stays `1`, and **there is no schema
migration** — `sources.kind` has been free text since v4 and both new
vocabulary values are strings.

- **The route.** A third on the loopback ingest, beside the two from 0.9.0:

  | route | body | replies |
  |---|---|---|
  | `POST /v1/discord/audio` | NDJSON, `{t_ms, user_id, name?, channel_id?, rate, seq, pcm}` | `204` |

  `pcm` is base64 mono PCM16 little-endian at `rate` (8 000–96 000 Hz;
  the plugin sends 16 000, and anything else is resampled by the daemon).
  `t_ms` is `Date.now()` for the frame's **first sample** — the same clock the
  speaking edges are on and the same clock `segments.t_start_ns` ends up on, so
  nothing is converted. `seq` is per stream and its only job is to make a hole
  visible.

  Base64 in NDJSON rather than a binary body: the other two routes are NDJSON,
  one batch carries several people at once, one bad line is skippable without
  poisoning the rest, and the 33% the encoding costs is 16 kB over a loopback
  socket. **Measured sizes** — one 500 ms frame of 16 kHz PCM16 is 16 000 bytes
  of PCM, 21 336 of base64, ~21.4 kB of JSON line; four people at 500 ms is
  ~86 kB per POST against `MAX_BODY` of 1 MiB, so a batch is budgeted in bytes
  and not in lines. Steady state is ~43 kB/s per speaking person.

  Auth, the `413`, the `204`-for-everything-else and the one-bad-line-is-counted
  rule are 0.9.0's and unchanged. A malformed or refused frame is counted in
  the audio counters, never raised: the client retries a whole batch on any
  non-2xx, so failing a batch over one frame would loop on it forever.

- **`[truth] audio = false`.** A third switch, and deliberately not folded into
  `enabled`. `enabled` opens a door for timestamps; this one lets recordings
  arrive over a TCP socket. **Both sides are off by default and neither trusts
  the other to have asked** — the plugin has its own switch too, because audio
  leaving the client and audio entering the recordings are two different
  people's decisions. While it is off the route answers `204` and counts the
  lines as rejected, so "I turned it on in the plugin and nothing happened" has
  a number attached to it. Three more keys: `audio_live_s` (4.0),
  `audio_idle_s` (10) and `audio_max_frame_ms` (5 000).

- **`sources.kind = "discord-user"`** — one row per Discord account whose own
  audio has arrived, `match_key` `discord:<user_id>`, `display_name`
  `Discord · <nickname>`. Rows are created **on demand**, by the first frame,
  so there are as many as there are people the user has been in a call with.

  - It has no rule and no device: `sources.set` **refuses** a `discord:` key
    the way it refuses `mic` and `room`, and `sources.list` reports `allowed`
    as `[truth].audio`. Deciding it by `[rules]` would be worse than wrong —
    the key is in nobody's allowlist, so every one of these would read as
    denied while plainly recording.
  - It **bridges threads**, like both microphones. Here it is not a nicety: one
    call is now one session per person, and a conversation that could not cross
    a session boundary would render a four-handed call as four monologues.
  - The Discord user id is on `sessions.instance_key`, not parsed back out of
    the match key.

- **`segments.label_via = "discord-stream"`**, `match_score` NULL. The audio is
  single-speaker **by construction** — the packets were decoded from one
  person's connection — so this is `"mic"`'s claim made about somebody else,
  and exactly as strong. It is not `"truth"`, which means the weaker,
  retroactive "Discord's speaking ring says this mixed turn was probably them".
  Three consequences, each a rule and not a heuristic:

  - **The overlap gate does not apply.** `overlap_frac` is still computed and
    still stored — it is information, and `enroll_max_overlap` reads it — but a
    positive reading cannot cost the embedding, because there is no second
    talker for it to be about. The duration floor is unchanged.
  - **The identity ladder does not run.** No comparison, no mint, no margin.
  - **Enrolment follows `[truth] enrol`** (off by default) **and the audio bar
    unchanged**: `overlap_frac ≤ enroll_max_overlap`, `duration_s ≥
    enroll_min_duration_s`. Ground truth says whose voice it is; it does not say
    the recording is worth keeping, and that has been the rule since 0.9.0. **No
    goldens are kept**: a golden outlives retention on the argument that it is
    the user's own voice and a future embedding model will need it, and that
    argument does not transfer to anybody else.

- **The voice, and the link.** The speaker is whatever `discord_users.speaker_id`
  points at. When the account is unlinked a voice is **minted and linked on the
  spot**, with `discord_users.via = "discord-stream"` — a fifth value beside
  `truth`, `manual` and `learned`. There is nothing to decide: the voice exists
  *for* that account and holds nothing else, which is why it is not the
  auto-linker's `truth` (20 turns at 90% agreement). A `manual` link is never
  overwritten, and **the Discord nickname is still never applied to the voice**
  — it is minted `Speaker_NN` like any other.

- **The de-duplication rule.** Both sources hear the same call, so:

  > While **any** per-user stream is live, the mixed Discord tap is **muted for
  > analysis**. Its audio is discarded at the head of the pipeline, exactly as a
  > global pause discards it, and the turn in progress goes with it rather than
  > being spliced across the boundary. A stream is live until `audio_live_s`
  > after its last frame. When the last one goes quiet the mixed tap resumes on
  > its next buffer, opening a fresh turn.

  Muting rather than de-duplicating afterwards, because the duplicate this
  prevents has no key to be found by: `segments` has no uniqueness constraint a
  second reading of the same speech would violate, and matching two turns by
  time overlap after the fact would be a guess about which transcript is real.
  The turn that is never written needs no rule for choosing. The cost of the
  rule is bounded and in the right direction: a plugin switched off mid-call,
  or a client that cannot do this at all, costs `audio_live_s` of transcript
  rather than the evening.

  "Mixed Discord tap" means a source whose match key matches `[truth].sources`
  the way everything else in this section matches it — and never a
  `discord:` key, which would be a source muting itself.

- **Sessions.** Opened by the first frame for an account, closed
  `audio_idle_s` after the last one, ended **at the last frame** and not at now
  — the last thing anybody knows is that they were being heard then, which is
  the rule `open_span_timeout_s` already follows for a speaking span.

- **Placing a frame in time.** Contiguous frames are placed by sample count
  from the run's anchor, not by their own `t_ms`: the wall clock is quantised to
  the millisecond and jitters by whole scheduler slices, and deriving each
  frame's position from it would wander past the pipeline's 100 ms gap
  threshold, which reads a wander as a hole and throws away the turn in
  progress. `t_ms` anchors the run; the samples carry it from there. A
  **sequence gap** ends the run, and the next frame anchors a new one from its
  own `t_ms` — so a hole is re-anchored and the half-built turn is discarded
  rather than spliced, which is what a hole should get.

- **`truth.status` gains `audio`**: `{enabled, live, live_s, idle_s, streams:
  [{user_id, name, channel_id, session_id, speaker_id, frames, quiet_ms,
  live}], counters: {frames, samples, rejected, gaps, sessions_opened,
  sessions_closed, voices_minted, clock_skew}}`. `null` only on a daemon that
  does not have the feature — a client must be able to tell that from "off",
  and a missing key cannot.

- **CLI.** `recalld truth audio on|off` edits the config (restart to apply) and
  says the rest of what has to be true — the plugin's own switch, that it is
  Vesktop only, and that `[truth] enrol` is separately off so these turns will
  be named but will not teach the voicebank. `recalld truth report` prints the
  live streams, the counters, and whether the mixed tap is muted right now,
  which is the single most surprising thing the daemon can be doing to a
  Discord recording.

- **Discord desktop cannot do this**, and the plugin says so rather than
  looking switched on: voice is decoded and mixed in `discord_voice.node`,
  whose JS surface offers per-user volume, mute and pan — parameters passed
  *into* the native mixer — and no way at all to receive a user's audio
  (FINDINGS §33.7). Vesktop and the web client use `MediaEngineWebRTC`, where
  every remote user is their own `MediaStream`.

## 0.12.2 — the mute aims at one client, not at Discord

0.12.1's de-duplication rule was written against a **source key**: while any
per-user stream is live, every session whose source matches `[truth].sources` is
muted for analysis. On an install with one Discord client that is exactly right.

The user runs two. Vesktop carries the RecallBridge plugin and is sending
per-user audio; a second client — the official Discord, on this machine — is in
a **different call**, with different people, and has no plugin and therefore no
streams. The rule muted both, and the second call went unrecorded for as long as
the first one lasted. This section aims the mute at one **instance** and says
who decides which. Additive: `proto` stays `1`, no schema migration, and the
one-client behaviour is unchanged.

### The rule, restated

> While any per-user stream is live, **at most one** mixed Discord instance is
> muted for analysis: the one whose speech the streams explain. Every other
> instance keeps recording, including a second instance of the same source.

"Instance" is a **session**. `sessions` has been per-PipeWire-node since Step 1
— two copies of an application already open two concurrent rows against the one
source — and v14 put `instance_key` (`serial:<object.serial>`, else
`pid:<pid>`, else NULL) on it. Nothing new is stored.

### How the instance is chosen

Time is quantised into 100 ms buckets. A bucket is marked for an instance when
that instance's audio was above an RMS floor of 0.005 in it, and marked globally
when *some* per-user stream was. Then

```text
share = |instance buckets within ±300 ms of a stream bucket| / |instance buckets|
```

and the bridge's client is the instance with the highest share, subject to three
guards, each of which exists because the measurement was run without it:

- **An evidence floor.** No share at all until the instance has 3 s of active
  audio inside the 25 s window. Under it, `share` is `null` — *not* zero, which
  a client must not render as a confident "not the bridge".
- **A bar**, 0.85. Loose on purpose: the mixed tap also carries join chimes,
  notification pings and a shared screen, and none of that is in the streams.
- **A margin**, 0.10, over the runner-up — and only when there *is* a runner-up.
  Two independent calls that are both busy score alike (FINDINGS §37 measured
  0.98 for a second call at 70% talk density), and a rule that guessed there
  would take a real call off the record on a coin flip.

Below the margin nothing is muted at all. **The trade is deliberate and it is
not symmetric:** a duplicate is two transcripts of one sentence — ugly, findable,
deletable — and a wrong mute is audio that was never recorded, with no row to
say it happened. The same asymmetry is why an instance that appears mid-call
starts with an empty window and **is never muted by inheritance**.

Across 144 synthetic two-call timelines §37 measured **108 right, 36 declined,
0 wrong**.

### `sources.instance_role {source, role}`

The override, because the automatic rule can be wrong and because the user asked
for one.

- `role` is `"bridge"`, `"other"` or `"auto"`. Anything else is a `params`
  error and never a silent `"auto"`: a typo that quietly meant "measure it"
  would be a mute somebody thought they had turned off.
- **`"other"` is never muted**, however sure the measurement is. **`"bridge"` is
  muted whenever streams are live**, with no evidence at all. `"auto"` **removes**
  the entry rather than storing a third value.
- Naming a `"bridge"` also stops the automatic rule naming a second one: there
  is one plugin, so there is one bridge client.
- It refuses `mic`, `room` and any `discord:` key, exactly as `sources.set`
  does and for the same reason — a role is a statement about a Discord *client*.
- Replies `{source, role, persisted, roles}`. Persisted to
  `[truth] bridge_roles` in the config, so it survives a restart.

**Keyed on the source match key, not on `instance_key`.** An override exists to
outlive a relaunch and both halves of an instance key are per-launch. On this
machine the two clients already differ there (FINDINGS §37): Vesktop's playback
node is `application.process.binary = vesktop`, `node.name = vesktop`; the
official client's is `application.process.binary = Discord` with
`node.name = application.name = WEBRTC VoiceEngine`. Two source rows. Two copies
of the *same* binary share one row and one role — that case is what the
automatic rule is for, and the override then speaks about both of them.

### `truth.status.audio` gains `mute`

```json
{"share_bar": 0.85, "share_margin": 0.1, "window_s": 25.0, "min_active_s": 3.0,
 "streams_live": true, "muted_sessions": [4211],
 "instances": [{"session_id": 4211, "source": "vesktop",
                "instance_key": "serial:44628", "role": "auto", "share": 0.96,
                "active_ms": 8700, "muted": true, "why": "…"}],
 "roles": {"vesktop": "bridge"}}
```

`why` is one sentence, written for a person: it is what `recalld truth report`
prints under each client and what the GUI's Sources card shows under each row.
`roles` carries only overrides somebody set. An instance is listed while it has
been heard in the last 50 s and is forgotten when its session ends.

### CLI and UI

`recalld role <MATCH_KEY> bridge|other|auto` moves it — over the socket when the
daemon is running, so it acts on the next buffer, and by editing the config when
it is not. `recalld truth report` prints one line per client with its role, its
share, and whether it is muted.

The Sources card's Discord panel lists every client heard in the window with a
three-way control on each. That is where the user answers the question they
asked: *which Discord client has the plugin installed, so not both inputs get
disabled.*

### What is unchanged

Neither microphone is ever a candidate, and neither is any non-Discord source: a
mic hears a room, not a call, and no amount of per-user audio makes it a
duplicate of anything. With one Discord client running, the behaviour is 0.12.1's
— the single candidate clears the bar and is muted.

## 0.12.3 — two bridges, and whose word is about which call

0.12.2 aimed the *mute* at one Discord client. It left the *verdict* aimed at
all of them, and on the install it was written for that was about to stop being
a theoretical problem.

The user runs two Discord clients with two accounts, often in two different
calls: the official desktop client patched with the NX Vencord build, and
Vesktop. Per-user audio (0.12.1) is Vesktop-only by construction, so to get it
the second client gets the same plugin — and then **both clients run RecallBridge
and both POST at the same daemon**. `truth_speaking` had no notion of which one
sent a span. `coverage()` reads every span overlapping a segment's time, so two
concurrent calls were mixed into every verdict; both bridges emitted roster
lines for different channels into one table; and a per-user frame from Vesktop
was attributed to whichever call the mute rule happened to be looking at.

This section gives a span a sender and a segment a scope. Additive: `proto`
stays `1`, one migration, and an install with one plugin — or with an older
plugin that sends no `client` at all — behaves exactly as 0.12.2 did.

### The wire: `client`

Every RecallBridge POST — `speaking`, `voice` and `audio` — carries one more
field on every line:

```json
"client": {"kind": "vesktop" | "discord" | "web",
           "account_id": "<the plugin's own Discord user id>",
           "instance": "<random, per plugin start>"}
```

- **`kind`** comes from Vencord's own `IS_VESKTOP` / `IS_DISCORD_DESKTOP` /
  `IS_WEB` build globals — the same ones the audio patch's `predicate` reads, so
  "the audio half is impossible here" and "this is the official client" can
  never disagree. It is what maps a bridge to the PipeWire node its client plays
  through without asking anybody: on this machine Vesktop's is
  `application.process.binary = vesktop` and the official client's is `Discord`
  / `WEBRTC VoiceEngine` (FINDINGS §37). `web` is accepted and never maps: a
  browser tab's audio comes out of the browser's node, which is not in
  `[truth].sources`.
- **`account_id`** is the bridge's identity, and the only thing that separates
  two clients of the *same* kind — two Vesktops share one `sources` row and can
  share nothing else.
- **`instance`** is random per plugin start and is **not stored**. It tells a
  reloaded plugin from a second one, which is a question about right now, and a
  column of it would be a column of churn. `truth.status` shows it.

**A malformed `client`, or none at all, never costs a line.** It is read as
absent, the line is stored unscoped, and the payload is taken. A refused line is
a span that never existed; provenance is not worth one.

### Schema v17 — `truth_speaking.account_id`, `.client_kind`

Two nullable columns, one index, **no backfill**.

> **NULL is a fact, not a placeholder.** It means the line came from a plugin
> that predates the field — from the only bridge there was — so it is evidence
> about whatever was being recorded then, and **every scope matches it**.

Without that last clause, upgrading the daemon would silently un-judge the whole
archive: 9,770 verdicts' worth of spans are unscoped on the install this was
written for, and a scope that excluded them would turn every one of those turns
`unknown`.

Two writes change with it, both to "per bridge":

- **A second `start` closes the first only within one bridge.** Two starts with
  no stop between them is a dropped batch (0.9.0) — but with two bridges in two
  calls the same account can genuinely be talking in both, and the official
  client's ring is not evidence that Vesktop's utterance ended. Matched with
  `IS`, so an old plugin's spans remain one stream of their own.
- **A `stop` (and a `leave`) closes its own bridge's span**, for the same reason.

### The scope: whose spans may judge this turn

A mixed Discord session belongs to a client, and only that client's bridge saw
the call in it. `crate::bridge::Scope` is the answer, in three values:

| scope | spans it reads | when |
|---|---|---|
| `Every` | all of them | one bridge, or an ambiguous pair |
| `Account(a)` | `account_id IS NULL OR account_id = a` | a bridge maps to this source |
| `Legacy` | `account_id IS NULL` | no bridge maps to this source |

and the ladder that picks one, in order of certainty:

1. **The user said so** — `[truth] bridge_roles` may name an account
   (`bridge:<account_id>`), and that beats every measurement. It is the only
   thing that can separate two clients of one kind.
2. **Nobody has ever scoped a span** — one bridge by construction. `Every`,
   which is 0.12.2 exactly.
3. **The kind maps** — exactly one bridge of this source's kind: that one.
4. **No bridge of this kind** — the spans on disk are somebody else's call.
   `Legacy`, so a scoped-only archive answers `unknown` rather than judging this
   audio against the wrong conversation. **`unknown`, never `nobody`**: the
   `truth_nearby` reach is scoped too, so "this bridge saw nothing here" cannot
   become "truth covered this moment and nobody was talking".
5. **Two bridges of one kind, no override** — ambiguous. `Every`, which is the
   pre-0.12.3 answer and therefore adds no new wrongness, and `truth.status`
   says out loud that it is guessing.

`coverage()`, `verdict()`, `truth_overlap_frac`, `truth label`, `truth rejudge`
and `truth report` all read through it.

### `Audible` gains the bridge's own account

0.12.1's rule was "every Discord account linked to the pinned You voice is
dropped from presence on `app` audio". Two accounts already reached it —
`discord_user_ids_for_speaker` resolves through `speaker_resolved`, so linking
the second account to the same "You" voice is the whole of the setup, merges
included — and that stays the way to say "these are both me".

0.12.3 adds a **stronger** clause that needs no link at all: **the bridge's own
account is silent on its own client's audio.** `own` is "an account the user has
told us is theirs"; this is "the account this recording is physically made
from", which is a fact about the stream and not a preference. A client never
plays your microphone back to you (§17), and the bridge account *is* that
client's local user.

### The mute, per bridge

0.12.1's rule read "is **any** per-user stream live". With two bridges that
question is true of the machine and false of the client in front of it, and
acting on the machine's answer is 0.12.1's bug with a second coat of paint. The
liveness the rule reads is now a *set of kinds*:

> A per-user stream can only be a duplicate of **its own bridge's client**. A
> Vesktop bridge's streams never mute the official client's session, and vice
> versa. An unscoped stream — an older plugin — explains every instance, which
> is 0.12.2 unchanged.

Three consequences, and all three fail towards recording:

- The share is computed only against the streams that could be about this
  instance. Two busy calls scored 0.98 alike (§37); a share computed against the
  wrong streams is not merely uninformative, it is confidently wrong.
- An instance no live stream is about is not a candidate, so it cannot win the
  margin and cannot be the runner-up that denies it to somebody else.
- **A `bridge` role cannot mute a client whose bridge is not sending.** The role
  answers "which of the candidates"; this answers "is there a candidate", and a
  fact outranks a preference.

`truth.status.audio.mute` gains `streams_kinds` (which bridges are arriving) and
each instance gains `explained` (whether any of them could be about it).

### `sources.instance_role` gains `account_id`

Optional, and only meaningful beside `role: "bridge"` — anything else is a
`params` error rather than a silently dropped field. It persists as
`bridge:<account_id>` in `[truth] bridge_roles`; a bare `bridge` is unchanged and
a trailing colon is a typo and is refused. On the CLI:

```
recalld role vesktop bridge --account 482913
```

Naming the account says **whose speaking spans this client's audio is judged
against**, which is a different question from whether it is muted, and the one
the source key cannot answer.

### `truth.status.bridges`

```json
{"bridges": [{"account_id": "482913", "kind": "vesktop", "instance": "a1b2c3d4",
              "last_span_ms": 1757000000000, "last_line_ms": 1757000000100,
              "spans": 412, "spans_per_min": 8.2, "source": "vesktop"}],
 "ambiguous": [], "recent_s": 300}
```

Always an object, never `null` on a daemon that has the feature: `{"bridges":
[], "ambiguous": []}` is "one plugin, or an older one", and a client must be able
to tell that from "no such field".

The rows come from `truth_speaking` and not from a live registry, because the
verdict pass judges segments recorded hours ago and has to know which bridges
existed *then*. `instance` and a bridge that has connected without anybody
speaking yet come from the registry, which is the only thing the database cannot
answer. `source` is resolved through the same ladder the verdict pass uses, so
the status can never disagree with the thing it describes.

`ambiguous` is a list of **kinds**, not a boolean: two Vesktops and one official
client is a real state and only half of it is broken. `recalld truth report`
prints one line per bridge and, when the list is non-empty, the warning and the
command that settles it. The Sources card prints the account under each client
("labels from the plugin signed in as …") and the warning above the list.

### Rejudge, and what does not move

**Historical verdicts stay as they are.** `recalld truth rejudge` runs against a
data directory with no daemon behind it, so it reads no live override, and every
span it re-judges predates the field — `scope_of` answers `Every` and the pass
re-derives exactly what 0.12.1 measured. Scoping begins **from the first scoped
span onward**: a verdict written before the plugin was updated is never
re-scoped by a bridge that arrived afterwards, and nothing in this section moves
a number 0.12.1 or §34 reported.

## 0.12.4 — every correction is word-level ground truth (schema v19)

`accuracy.summary` has always measured *how wrong* the transcripts were. It
could never say **which decoder to believe**, because the three readings of a
clip were never in the same place: the live pass's words and the context
re-decode's live inside `segments.redecode` operations, the night shift's in
`segments.night_text`, and the person's in a `segments.correct` operation. This
release puts them on one row, at the moment the truth is made.

Nothing above this line changes shape. `proto` stays `1`, `segments.correct`
takes and returns exactly what it did, and every field described here is
additive.

### Schema v19 — the `text_truth` table

| column | meaning |
|---|---|
| `segment_id` | the turn |
| `truth_text` | what the person typed. The reference |
| `live_text` | what the first pass read, when it is recoverable |
| `context_text` | what a re-decode read (`context`, `arbiter` and `lid` are one pass here) |
| `night_text` | what the night shift read, **whether or not the vote let it win** |
| `canary_text` | always `NULL` today — see below |
| `asr_confidence` | the cross-check verdict standing over the words being replaced |
| `speaker_id`, `source_kind`, `duration_ns` | the three facets a measurement is cut by, **as they are now** |
| `created_ns` | the instant of the correction. With `segment_id` it is the natural key |

Written by `segments.correct` after the row is rewritten, and **backfilled from
the operations history on migration** — every field already existed on disk, so
a v17 archive arrives with its whole correction history in the table. The
backfill runs on every open and is a no-op after the first: the natural key
makes replaying history idempotent, which `tests/truth.rs`'s three-open test now
also covers. The table is derived data. Nothing renders from it, and dropping it
loses no user-visible state.

Two things it deliberately does not claim:

- **`canary_text` is null, and that is the honest value.** The cross-check
  decoder stores its *verdict* and not its words (`crate::quality`); the night
  shift re-decodes the clip when it needs the actual sentence and throws it
  away again. The column exists so that the day the words are kept is a write
  rather than a migration.
- **A reading is filed by the route recorded with it, not guessed.** Every
  `segments.redecode` operation carries `{text, text_via}` and a timestamp, so
  for a correction at `T`: operations at or before `T` each contribute one
  reading under the pass their `text_via` names; the words the correction
  replaced are filed under the route named by the **first operation after
  `T`** (that operation replaced them, so its `prior_state.text_via` is how
  they got there), falling back to the row's current `text_via`; and nothing
  after `T` contributes anything else, because the text it kept is the
  corrected text and scoring the truth against itself is not a measurement.

### `accuracy.learn {apply?}` — which decoder wins, per cell

A **cell** is one voice, one source kind, one duration bucket (`short` under
2 s, `mid` under 6 s, `long`), keyed as `app/25/short`; an unlabelled voice is
`-` and is a real cell, not a missing one.

```json
{"id": 9, "method": "accuracy.learn", "params": {"apply": false}}
```

The reply carries `corrections`, `measurable`, `min_rows_per_cell`,
`margin_pp`, `fit_fraction`, a `global` cell report, one report per `cell`,
`short_by` (what each under-sampled cell still wants), the `rules` the pass
would install, `applied`, and `installed` — the rules actually in force. A cell
report is `{cell, source_kind, speaker_id, bucket, rows, held_out, decoders:
[{decoder, rows, wer}], rule, verdict}`; `wer` is the bounded corpus edit share
over normalised words, the same figure `edit_rate` is, and `null` where that
decoder read none of the held-out rows.

**Read-only unless `apply` is true.** Four gates, and a rule ships only if it
clears all of them:

1. **A minimum sample per cell** — 30 corrections, `calib::MIN_ROWS_PER_VOICE`
   for the same reason. A smaller cell inherits the global decision.
2. **A minimum sample per comparison** — 12 held-out rows *both* decoders read.
   This is not implied by the first and the archive is why it is written down:
   the first run of this pass over 37 corrections found a 37-row cell whose
   live-against-context comparison rested on four rows, and would have shipped
   a rule off it. Thirty corrections are not thirty measurements of every
   decoder.
3. **A chronological hold-out** — `calib::split_at` at `FIT_FRACTION`, so
   nothing the fit saw scores it.
4. **A margin** — 2 percentage points off held-out error, `calib`'s
   `improvement_is_material` restated. Without it every rounding-error
   improvement installs itself.

A cell that clears none of them ships **nothing**, and the 0.9.0 vote stands.

### What a rule can change

Exactly two things, stored in `settings` under `text.decoder_rules`:

- **`winner`** — `live` (the shipped answer), `context` or `night`.
- **`vote`** — what `crate::night` may do in that cell. `two_of_three` is the
  0.9.0 rule and the default everywhere. `night_wins` drops the requirement for
  a second voter **and nothing else** — every guard still runs, because the
  guards are about §12's hallucinations and no amount of held-out WER makes a
  Swedish sentence an acceptable replacement for a German one, and a reading
  that agrees with the words already there is still not a replacement.
  `keep_live` refuses the replacement outright and keeps the reading as an
  annotation.

The night shift reads them once per batch, under the same lock it gathers
under. An absent or malformed setting is "no rules", never an error: a night
shift must not stop because a setting could not be parsed.

### `accuracy.summary` gains `learned`

```json
{"learned": {"corrections": 37, "min_rows_per_cell": 30, "margin_pp": 2.0,
             "rules": 0, "learned_ns": null, "learned_ms": null,
             "ready": false, "needed": 0, "installed": {…}}}
```

`needed` is how many more corrections the smallest useful sample wants, and it
is the half of the block that matters on a real machine: the Memory view's
accuracy card renders one line — *"Learned from 37 corrections — N more in one
voice, source and turn length and it can start choosing between its decoders"*
— because a card that only said "nothing learned yet" would leave the reader
with nothing to do about it. `recalld accuracy` prints the same line, and
`recalld accuracy report` prints the whole per-cell table.

**On this install, today, nothing ships.** 37 corrections, 31 of them with a
decoder's reading beside them, spread over ten cells whose largest holds 11 —
and no night reading at all, because `[night].enabled` has never been on here.
The recording and the pass are in; the bar is unmet and says so.
## 0.12.4 — a turn cut where the speaker changes

**No wire change, and that is the design.** A split turn is two ordinary
segments: two `segment.new` events, two rows in `transcript.page`, two clips.
No client learns a new field, no client learns a new event, and a client that
predates this section renders a split turn correctly because there is nothing
about it to render specially. The only thing that changes is that some turns
that used to be one row are now two.

### What a piece is

The daemon has always ended a turn at silence and nowhere else
(`crate::turns`), so a fast exchange — "yeah" / "no it isn't" across half a
second — arrives as one row with one label. `crate::turnsplit` slides the
identity extractor over the turn at a 0.25 s hop, compares the window *ending*
at each boundary with the one *starting* there, and cuts where they disagree
most, subject to three refusals:

* no piece shorter than `[identity].split_turn_min_piece_s` (default
  `min_duration_s`, 1.0 s) — a piece exists to be labelled, and a piece the
  ladder must refuse is a row with no speaker where there used to be one;
* at most `split_turn_max_cuts` cuts, no two closer together than a piece;
* **no piece without words.** The detector reads the voice and not the words,
  so it will cut a laugh or a two-second "yeah" in half; a split that leaves
  any piece silent is refused whole.

Each piece then goes through the ordinary path — its own clip, its own row, its
own transcript, its own trip through the ladder — so `t_start_ns`/`t_end_ns`,
`overlap_frac`, `speaker_id`, `match_score`, `label_via`, `lang` and the truth
verdict on a piece all mean exactly what they mean on any other segment. The
pieces tile the turn: piece *n*'s `t_end_ns` is piece *n+1*'s `t_start_ns`, and
between them they hold every sample.

### The transcript is partitioned, never re-decoded

The whole turn is decoded once with word timestamps
(`crate::asr::TimedAsr`) and each piece takes the words that **start** inside
it. Concatenating the pieces' `text` in time order reproduces the turn's word
sequence exactly — no word is lost at a cut and none is spelled twice. That is
a property of the construction and not of the model: two independent decodes
could do neither, because a word straddling the cut belongs to whichever piece
got most of its audio, to both, or to nothing.

`asr_model_id` on every piece is the same decoder, because it was the same
decode.

### The switch

```toml
[identity]
split_turns = true             # on since 0.12.7, with the voicebank veto — FINDINGS §52
split_turn_window_s = 1.5
split_turn_hop_s = 0.25
split_turn_min_piece_s = 1.0
split_turn_distance = 0.80     # moved from 0.85 (§39) at 0.12.7 — see below
split_turn_max_cuts = 3
```

`split_turn_distance` is a **distance**, `1 - cos`, and is not comparable with
`label_threshold`: two 1.5 s windows of the *same* person on this audio already
score around 0.6, so anything that sounds like a sensible similarity bar is
below the noise floor.

**Shipped off at 0.12.4, on since 0.12.7 — see that section below for why.**
The 0.12.4 detector alone found 41.7% of the reachable change points within
±0.5 s at 67.9% precision and split 0.87% of turns Discord says are one
person: it cleared the false-split bar and missed the recall bar it was set
(≥50%). 0.12.7 added a second, independent refusal — a voicebank veto, not a
better distance curve — that clears both.

### `recalld turns resplit`

The same detector over the archive's `partial` and `overlap` rows — the two
verdicts that *mean* the row holds more than one person's audio. `single` is
never re-decided by this pass: Discord has already settled it.

```
recalld turns resplit           # what it would cut, turn by turn. Writes nothing.
recalld turns resplit --apply   # cut them.
recalld turns resplit --undo    # put back what the last run cut, newest first.
```

**The original row survives, shortened to its first piece**; the other pieces
become new rows in the same session. Nothing is deleted — not the row, whose id
`threads`, `commitments`, `time_refs`, `notes`,
`speaker_prototypes.source_segment_id`, `segment_vectors` and every stored
correction point at, and not the original clip, which is what makes `--undo`
work. Everything the analysis leg owns about audio the row no longer covers —
the words, the language, the speaker, the overlap reading, the vectors, the
verdict — is cleared and recomputed per piece.

Each cut turn writes one `turns.resplit` operation whose `prior_state` carries
the whole turn back:

```json
{"segment_id": 11050, "t_start_ns": 1788…, "t_end_ns": 1788…,
 "audio_path": "segments/000312/seg-000239-1788….wav",
 "text": "Yeah that's all Imano", "truth_verdict": "overlap",
 "minted": [17421]}
```

`--undo` restores the span and the clip and soft-deletes the minted pieces. It
does **not** restore the words: the span is right again and nothing has re-read
the audio, so the honest state is a turn waiting for the analysis leg, which is
the same state the split left it in. It is idempotent by the span — undoing
twice cannot delete a second generation of rows.

### What a client should expect

Nothing new to implement, and one thing not to assume: a segment id is no
longer a stable claim on a fixed span. It always could change (`segments.correct`
rewrites text, `segments.reassign` rewrites the speaker); after 0.12.4 an
applied resplit can also *shorten* an existing row and add a sibling beside it.
A client that re-reads a segment by id after a `segment.updated` event was
already doing the right thing.

## 0.12.7 — the voicebank veto, and the switch turns on

FINDINGS §39 shipped turn-splitting off: `adjacent` alone missed the ≥50%
recall bar by eight points. §52 asked five ways to close that gap without
moving G1 (false splits ≤1% on `single` turns) or G3 (held-out identity
precision) — an adaptive per-turn bar, combining `adjacent` with `contrast`, a
smaller hop with two window lengths voted, a voicebank veto, and boundary
refinement against the VAD. One cleared all three, chronologically held out:

**A candidate boundary is kept only if the voicebank's own top-1 speaker
actually differs across it.** `crate::turnsplit::proto_veto` asks the bank one
yes/no question per candidate `adjacent` already found — never a score of its
own, never a boundary of its own — and a candidate it disagrees with is
dropped before [`cuts`](../crates/recalld/src/turnsplit.rs) ever sees it. This
is not `named` from §39, which lost at every threshold because it asked the
bank a harder question (does each side name a voice *above the label bar*)
that a captured window answers wrong with total confidence; the argmax needs
no confidence at all.

| gate | bar | §39 (`adjacent` alone) | §52 (`adjacent` + veto) |
|---|---|---:|---:|
| G1 false splits on `single` | ≤ 1% | 0.87% | 0.83% held out |
| G2 change recall at ±0.5 s | ≥ 50% | 41.7% | **51.0%** held out |
| G3 held-out identity precision | not down | — | unmoved by a real `--apply` |

**The stated cost: a bank with fewer than two enrolled voices vetoes
everything.** There is nothing for the top-1 speaker to disagree with itself
about, so a fresh install with nobody enrolled yet cuts nothing until it has
two people in the voicebank. This is the measured shape, not a bug — the same
shape `named` had in §39 — and it is why `split_turns` turning on by default
costs a new install nothing it would have noticed: no bank, no cuts, exactly
as before.

`split_turn_distance` moved from 0.85 to 0.80 alongside the veto: with a
second, independent filter in force the distance bar can afford to let more
candidates through, and 0.80 is the fit split's best point once it does.
Nothing else about the switch, the archive pass, or the wire changed — see the
0.12.4 section above for the shape of a piece, the transcript-partition
guarantee, and `recalld turns resplit`.

## 0.12.4 — how a turn sounded (schema v18)

The user asked for one thing: *"colour in the text or tag the text with the mood
in the transcript"*. The honest answer turned out to be two features with two
different amounts of evidence behind them, and this section is mostly about
keeping those two apart on the wire.

**Nothing here is new inference.** SenseVoice — the decoder 0.11.6 catalogued
for Korean and Chinese (FINDINGS §27) — has always emitted four tags per decode:
language, emotion, audio event, and whether inverse text normalisation ran. The
daemon read `text` and dropped the rest. 0.12.4 reads two more of them.

### What is measured, and therefore what is drawn

`spike/mood_bench.py` ran SenseVoice-small int8 over the user's own archive on
four niced cores before a line of GUI was written. The table is FINDINGS §42; the
two sentences that decide the protocol are:

* **Events are drawn.** `laughter` and `music` are marks on the audio, and
  laughter agrees with the transcript's own laughter tokens far above the base
  rate.
* **Mood is stored and withheld.** The model declines to answer on most turns,
  and on the rest it did not clear the margin fixed before the run.

So the wire carries both and **one flag says which may be believed**. That is
deliberate: a daemon that quietly omitted a column would leave a future client
unable to tell "withheld" from "old daemon", and a client that decided for itself
would be telling somebody how their friend felt on evidence nobody checked.

### Schema v18 — `segments.mood`, `.events`, `.mood_at_ns`

Three nullable columns on `segments`, no backfill.

| column | value |
|---|---|
| `mood` | `happy`, `sad`, `angry`, `neutral`, or NULL |
| `events` | the sorted comma-joined subset of `laughter,music,applause,cry`, or NULL |
| `mood_at_ns` | when the pass looked |

**`mood_at_ns` is the queue and the other two are the answer.** There are three
columns and not two because the pass stamps every row it reaches, *including*
the ones it heard nothing on — a model that abstained and a clip retention has
taken both write NULL/NULL, and without the third column they would be
indistinguishable from a row nothing had visited. The pass would then re-read
them every night for the life of the archive.

NULL in `mood` therefore means one of two things and the row cannot tell them
apart: nothing has listened, or the model listened and declined. Both render
identically — as nothing — so the distinction stays off the wire.

### The segment shape gains two keys

Every surface that carries a segment carries them: the transcript, a search hit,
`thread.get`, replay, and the live `segment` event.

```json
"mood": "happy",
"events": ["laughter", "music"]
```

`events` is an **array of the closed set**, never the stored comma-joined
string, and it is `[]` — not `null` — on a turn that carried none and on every
row the pass has not reached. A client iterates it without a null check, because
"no event" and "not looked at" are the same thing to draw.

`mood` is a bare string or `null`. **A client must consult
`status.mood.rendered` before drawing it.**

### `status.mood`

Always present, always the same shape, so a client can tell "off", "the model is
not installed" and "an older daemon" apart:

```json
"mood": {
  "enabled": false,          // [mood].enabled — is the pass listening
  "available": true,         // is SenseVoice on disk
  "how": null,               // …and how to get it, when it is not
  "phase": "off",            // off | unavailable | blocked | idle | running
  "rendered": false,         // MAY THE MOOD BE DRAWN — a measurement, not a setting
  "why": "The mood tag is stored but not shown: …",
  "live": false,             // are the tags also read on the way in
  "backlog": 0,
  "read_total": 0,
  "counters": { "read": 0, "with_mood": 0, "with_event": 0, "no_audio": 0, "last_run_ms": 0 }
}
```

`rendered` is the load-bearing key and it is **not** `enabled`. The pass being on
says the tags are being written; `rendered` says whether the mood among them may
be believed, and it comes from `crate::mood::MOOD_IS_MEASURED` rather than from
config. There is no request that changes it, and that is on purpose: whether a
measurement came out is not the operator's opinion, and a switch there would be
an invitation to turn on a feature the evidence calls noise. **Laughter and music
are never gated by it** — they were measured separately and they passed.

`why` carries the daemon's own sentence for the refusal, so a client prints the
reason instead of inventing a friendlier one.

### `[assist] mood_display` — the fourth control on that card

`tags` (the default), `tint`, `both`, `off`. Set through `assist.set` beside the
three translation settings, carried on `assist.get`, on the `assist` event and on
`status.assist`, and live in all four — a control that needs a restart is not a
control.

**The setting governs the MOOD; the events are governed only by `off`.** A mood
can be a chip or a colour, and an event can only ever be a chip — there is no
such thing as the colour of laughter. So `tint` means "put the mood on the
words instead of on a chip", and laughter and music keep theirs. That is also
what keeps all four states acting on the daemon as it ships, where the mood is
withheld: without the split, `tint` would draw nothing at all and be a setting
waiting on a measurement.

It is on `[assist]` rather than on `[mood]` because it is a fact about a **page**
and `[mood].enabled` is a fact about the **pass**. Two questions, two switches: a
person who turns the display off has not turned the listening off.

```
assist.set {"mood_display": "both"}
```

Refused with `params` for anything outside the four, in the shape
`translation_display` is: a client that sends `tinted` is told, rather than
silently getting the default and wondering why its radio button will not stick.

`off` still leaves the laughter glyph on the headset overlay, which is another
surface with its own answer — see `docs/OVERLAY.md`.

### `speakers.palette` gains `mood_palette`

```json
{ "palette": [ … ten … ], "mood_palette": [
  {"token": "happy",  "hue": 44,  "hex": "#705d29"},
  {"token": "sad",    "hue": 232, "hex": "#293370"},
  {"token": "angry",  "hue": 350, "hex": "#702935"}
]}
```

Three, and they are three of the ten the person palette already spends, so the
suite turns one wheel. `neutral` is a mood and is deliberately **absent**: it is
what a transcript already looks like, and painting it would repaint the whole
archive to say nothing. A lookup that misses falls through to the ordinary ink,
which is the rule an unknown token already follows.

Tokens and not hex, for the reason `crate::palette`'s module note gives — see
`docs/DESIGN.md` §6.1 for the saturation and lightness a *sentence* is painted
at, which are not a name's.

### `person.get` and `thread.get` gain `mood`; so does a digest

One block, one function, three callers, so a digest and the conversation page it
opens can never say different things about one evening.

```json
"mood": {
  "read": 214,
  "counts": {"happy": 31, "sad": 4, "angry": 2, "neutral": 60, "laughter": 26, "music": 3},
  "last_ms": 1756000000000,
  "last_ns": "1756000000000000000",
  "summary": {
    "read": 214,
    "laughter": 26,
    "laughter_share": 0.121,
    "laughs": true,
    "mood": null
  }
}
```

`counts` is unconditional — they are facts about rows. `summary` is the block
that says what may be **said**, and it is `null` for nearly everybody: under
thirty read rows the daemon refuses to write one at all, because a person heard
twice is not somebody who "laughs half the time". `laughs` is the one claim this
daemon will make about a person from these tags, and `summary.mood` is the one it
will not until the measurement changes.

The daemon hands over counts and booleans and never prose. A daemon that shipped
English sentences would have to ship them in every language the GUI is read in.

**The digest's `mood` is beside the paragraph, never inside it.** Every clause
added to a prompt that both decides and writes made the deciding worse (the table
in `crate::digest`'s module note), and "say how it felt" is exactly such a clause.
The feeling is counted, not asked of the model.

### `search.answer` may read an event off a row

The rows the model is shown gain the event in parentheses after the name:

```
[301] 21:04 Kira (laughter): der Tank ist einfach explodiert
```

Parentheses and not brackets, because `[` is the citation syntax and a second
bracketed thing on the line is a second thing that looks like an id. `mood` is
**not** on the line — the events were measured and it was not, and a model handed
a tag nobody trusts will happily build a sentence on it.

**The grounding post-check is unchanged and does not see the mark.** The overlap
test runs against the rows' `text` alone, so an answer that says *"they laughed"*
and shares no content word with the turn is still refused. The event tells the
model which row to read; the words are what it has to read off it.

### `[mood]`

```toml
[mood]
enabled = false        # off by default, like every optional pass
rows_per_run = 2000    # per opening of the gate
batch_rows = 64        # rows fetched per query, NOT a decode batch
min_duration_s = 1.0
live = false           # read the tags on the capture path too
```

The clock is **borrowed**: `[night].window` and `[night].also_when_idle_min`, the
same borrow `[asr].lang_sweep` makes and for the same reason — "the hours this
machine is nobody's" is one fact about a household, and two copies of it would
eventually disagree. There is no GPU gate, because nothing here touches the card.

`batch_rows` is not the night shift's kind of batch. SenseVoice's tags are **per
clip**, so concatenating eight turns would ask which of them the laughter was on;
every clip is decoded on its own, and the number only bounds a query.

`live = false` was measured before it was offered rather than defaulted off out
of caution — see FINDINGS §42. A live mood chip is worth less than a dropped
turn, and the overnight pass reaches the same row within a day.

## 0.12.4 — every scoring rule, with the bars it earns for itself

`identity.calibrate` has chosen between prototype-aggregate rules since 0.12.0.
It chose them wrong in two ways, and this round fixes both
(`spike/FINDINGS.md` §45). Neither is a new feature; both are the same
correction, which is that **a rule and a bar are one decision**.

### The report carries every arm, twice

A learned threshold is a number on a score scale, and the aggregate *is* the
scale. Comparing a top-3 mean against per-voice bars fitted under max cosine
measures the scale and not the rule — the error §36 found in `truth::enrol_batch`,
still present in the one place §32 had left it. The pass now refits per-voice
thresholds **under each candidate rule** and reports both readings:

```jsonc
{
  "aggregates": [
    {"rule": "max",   "incumbent": true,
     "globals": {"n": 1113, "correct": 1015, "wrong": 49, "f_beta": 0.945},
     "fitted":  {"n": 1113, "correct": 939,  "wrong": 27, "f_beta": 0.943},
     "thresholds": [{"speaker": 2, "threshold": 0.32, "margin": 0.04, "n": 912}]},
    {"rule": "top-4", "incumbent": false,
     "globals": {"n": 1113, "correct": 1011, "wrong": 10, "f_beta": 0.973},
     "fitted":  {"n": 1113, "correct": 1033, "wrong":  9, "f_beta": 0.978},
     "thresholds": [ … ]}
  ],
  "aggregate": {"rule": "top-4", "score": { … the FITTED score … }},
  "aggregate_thresholds": [ … the winning arm's own bars … ],
  "aggregate_installed": "max",
  "aggregate_swap": true
}
```

Three rules a client can rely on:

* **`aggregates` contains the installed rule**, flagged `incumbent: true`. The
  loop used to skip it, so the rule the box was running never appeared in its
  own table and could only be compared against the globals row. The incumbent's
  two entries are the same two measurements as `baseline` and `candidate` —
  one answer per question, not two that can disagree.
* **`aggregate.score` is the `fitted` score**, because that is the operating
  point an install would actually put the box on. The gate compares
  (candidate rule + candidate's bars) against (installed rule + its bars).
* **`aggregate_thresholds` is installed with the rule.** Through 0.12.3 a swap
  cleared every learned bar and left the refit to the next pass — so between
  the two runs the box sat on an operating point nothing had measured. The pair
  is what the gate approved, so the pair is what is written: `cleared > 0`
  **and** `written == aggregate_thresholds.length` on a run that swaps.

Because the incumbent is now an arm, the pass can also go **home**: an install
that learned `top-3` on an earlier corpus and no longer earns it is put back on
`max`, through the same `swap_is_safe` + `improvement_is_material` gate as any
other change. This is not hypothetical — the box did exactly that on
2026-09-04, and §45 measures the reversal it should have made instead.

### `identity.repair` measures at the install's operating point

`repair_prototypes` scored its before/after table with `IdentityConfig::default()`.
On this install the daemon gates at `max_overlap = 0.06` against a default of
0.1, so the table that justifies a permanent deletion described a machine
nobody was running. It now takes the caller's `[identity]` config, which is the
same block `identity.calibrate` and the live ladder read. The wire shape does
not change; the numbers in it do.

## 0.12.5 — sliced turns

Words for a turn that is **still being spoken**, from a turn long enough that
waiting for it to end is the problem. One new event, no new topic, no new row,
nothing stored — and one row at the end, exactly as before.

```json
{"seq": 41871, "ev": "slice", "data": {
  "session": 3, "source": "VRChat.exe",
  "speaker": 7, "speaker_hint": "proximity",
  "t_start_ms": 1772486400123, "t_start_ns": "1772486400123456789",
  "elapsed_ms": 12400,
  "text": "and that is why the door behind the bar only works once",
  "text_so_far": "so the way the portal network actually works is that every instance keeps its own copy of the graph and that is why the door behind the bar only works once",
  "seq": 1,
  "final": false
}}
```

It rides on the **`segments`** topic, so no client changes its subscription and
one too old to know the event ignores it (Versioning rules).

### A slice is not a partial, and the difference is one word

The two events are deliberately the same shape, down to the field names, so a
client that already draws a provisional row draws this one with no new code.
What they do not share is what the words mean:

| | `partial` (0.11.0) | `slice` (0.12.5) |
|---|---|---|
| each event is a reading of | the **whole open turn**, again | **new audio**, once |
| the row is | **replaced** | **extended** |
| the words are | provisional, often taken back | final — they are the words the row will carry |
| cost over an N-second turn | O(N²) — **+553% to +702%** CPU (§20) | O(N) — the turn's audio is decoded once either way |
| what to draw | `text` | **`text_so_far`** |

**`text_so_far` is what a client renders.** `text` is this slice alone, offered
so a renderer can animate the newest words without diffing for them. The daemon
does the joining, and a client that accumulated `text` itself would double a
piece the moment an event was redelivered.

### Never cut mid-word

`[captions] slice_after_s` is a **floor, not a period**. Past it the daemon
waits for a dip the VAD has already scored as not-speech (120 ms — under the
VAD's own 500 ms span close, over its 32 ms frame) and cuts there. A turn with
no pause in it is never sliced; it ends on the VAD's 30 s cap exactly as it did
before this existed. Half a word decoded alone is not a worse reading of that
word, it is a different word, and it would be written down.

`slice_after_s = 0` turns the feature off, and off is byte-for-byte the
behaviour before it existed: no slice is offered, and the turn is decoded
whole. A client must still treat `slice` as an event it may never see, on an
install that has turned the floor down.

**On by default since 0.13.1, at `slice_after_s = 8`** (FINDINGS §48). §41
shipped this off: the row was built from the joined slices, and that text
disagreed with a whole-turn decode on 17.6% of words against a noise floor of
exactly 0.00% — a real cost with no way to tell which reading was actually
worse. 0.13.1 removes that cost rather than accepting it, by changing what the
row IS — see "One row at the end" below — so the CPU line is now the only
thing this knob trades off. At `8` a sliced turn's audio is read one extra
time in full, and that measured at **+6.4%** CPU per audio minute against a
+10% gate on the same 30-minute archive replay §41 used, while a word still
reaches the glass at the latency §41 measured (1.7–2.1 s sooner at the
median, depending on the floor). `6` — where §41's caption numbers were
taken — slices more of the archive's long turns but costs +11.3%, over the
+10% line though inside a +15% ceiling; `10` and `12` cost less (+3.7%,
+2.6%) and touch fewer turns. Every floor's row is worth exactly the same
words: see below.

### One row at the end

When the turn finishes, an ordinary **`segment`** arrives and replaces the
growing row by matching **`(session, t_start_ns)`** — the same key, the same
rule and the same reason as a partial: a slice has no `id`, because there is
no row yet. The wire contract has not changed since 0.12.5; what changed in
0.13.1 is what the daemon puts in that `segment`.

**Before 0.13.1, the row was every slice joined to the remainder** — the words
already on the glass, plus one more decode for whatever was left. That is
what made the caption cheap (§41's whole claim: a turn's audio is decoded
once, whether in one piece or six) and also what made it wrong 17.6% of the
time: a slice read without the rest of the turn around it is a different
reading of it, not a worse one, but a different one, and joining several
different readings into one row is not the same claim as the turn being
decoded whole.

**Since 0.13.1 (FINDINGS §48), the row is the WHOLE turn, read once more.**
The slices are still decoded exactly as before and still reach the glass at
the latency §41 measured — nothing about `slice` or `maybe_slice` changed —
but the joined text is discarded rather than written down. At turn close the
daemon decodes `samples`, the turn's own whole audio, the same call an
unsliced turn has always made (`Analyzer::prepare_maybe_said` with `said:
None`), and that reading becomes the row. Same function, same bytes: the two
readings are not merely close, they are the same string, because there is
only one decode of that audio for them to disagree about. That is the whole
proof behind "0.00% by construction" — measured at exactly 0.00% (bootstrap
0.00%–0.00%) over the 127 sliced turns of §41's own long-turn sample. The
price moved from words to CPU: one extra whole-turn decode, once per sliced
turn, which is the number `slice_after_s` now trades off (see above).

**`[identity] split_turns` wins where they meet.** A turn that was sliced is
never *also* cut at a speaker change: splitting needs a timed decode of the
whole turn, which is exactly the decode slicing exists to avoid, so doing both
would spend the turn twice and throw away the reading already on the glass.
`split_turns` is off by default; an install that turns it on together with
slicing gets split turns and no slicing of the turns that would be split.

There is no `continues` flag and there are no consecutive rows, and that is the
load-bearing decision in this feature rather than an implementation detail.
Everything downstream of a turn is a statement about a **whole turn**: the
speaker embedding is taken over the turn's whole audio (six vectors of six
fragments are six weaker claims about the same person), the overlap gate decides
over the whole turn, the context re-decode and the flip arbiter re-read the
whole turn, and threads, digests, truth verdicts, translation, search and export
all count turns. Slices that were rows would have made every one of those
count a monologue as six conversations' worth of evidence. So the pieces are
**wire events only**, the row is written once, and not one downstream pass
changes.

The cost of that choice, stated plainly: the words on the glass during a long
turn are **not durable and not searchable** until the turn ends. A crash
mid-turn loses them — which is exactly what a crash mid-turn already did.

### The speaker on a slice is a guess or nothing

Identical to a partial, and for the identical reason: the identity ladder only
labels finished turns, because it needs an embedding and an embedding needs the
overlap gate's approval over completed audio. So a slice carries the proximity
hint or nothing at all, `speaker_hint` says which, and the settled `segment`
carries the real answer.

### Never stored, never replayed

A slice is not a row, not an entry in the replay ring, and not part of
`events.since`. A client that reconnects mid-turn has missed the slices and will
get the `segment`, which is the whole turn.

### What a client is expected to do with it

Draw `text_so_far` as **one row that grows**, under the settled rows and outside
the last-N caption window, with a trailing ellipsis and **settled ink** — a
slice's words are not in doubt, only the sentence is unfinished. Replace it on
the matching `segment`. Do not count it, do not file it as a segment, and do not
let it age out on a partial's staleness rule: slices are minutes apart by
design, and the bound that is actually true of a growing row is the VAD's 30 s
turn cap.
## 0.12.5 — a fitted bar declines, it never mints

On the evening of 2026-09-04 the nightly pass installed a per-voice label bar of
0.41 for Rowan and 0.60/0.08 for two unnamed voices ground truth had never once
confirmed. Between 21:47 and 22:54 UTC the daemon minted twenty phantom
`Speaker_NN` voices, every one of them seeded with a recording of Rowan, and
filed 302 of Rowan's and Aspen's turns under them. Held-out identity fell to
F-0.5 0.31. The measurement is FINDINGS §46; this is what changed.

### The failure, in one paragraph

`decide_with` had one exit for two different claims. *Nothing in the bank is
close to this* and *this voice's own learned bar is higher than the one everybody
else answers to* both returned `Decision::Mint`, and `analysis` seeds a mint with
the turn's own audio. Because a same-evening recording of a person outscores that
person's two-day-old bank — 0.614 against 0.544 in the first mint of the burst —
the next turn of the same person matched the phantom, and either took its name or
failed the next fitted bar and minted again.

### The rule

A turn that clears the **global** operating point (`[identity].label_threshold`
and the global margin) and fails only its top candidate's **fitted** bar returns
the new `Decision::Declined { best_score, bar }`: no name, no new voice, and the
embedding and transcript are kept exactly as `Mint` kept them. `Mint` now means
what its doc comment always claimed: nothing in the bank came close.

The rule is deliberately narrow. A voice with no fitted bar cannot reach it, a
score under the global bar still mints, and the enrol bar is untouched. On the
2026-09-04 evening it prevents all ten mints the reconstruction reproduces and
costs zero correct labels; on the archive's two real cold starts it mints 7 of 8
and 8 of 8 of the voices the shipping path did, with *fewer* wrong labels than
shipping. Two rules that would also have stopped the cascade — a near-miss slack
of 0.10, and refusing a label to an unnamed voice with fewer than N prototypes —
were measured on those same cold starts and **refused**: they turn a stranger's
first evening into 120 mints.

A new counter, `declined_fitted`, appears alongside `too_slight` in the daemon's
stats line and in `stats`.

### `identity.calibrate` scores the mint path

`swap_is_safe` compared two label-only scores, in which a decline is a free
non-answer. It is not free — the live daemon turns one into a voice — and on
2026-09-04 that arithmetic is what installed the bar: label-only 0.848 → 0.897
(`may_install`: PASS), the same pair with the mint path in 0.771 → 0.123.

Every arm the pass measures now carries a **second** score, in which a decline
the ladder would have minted is counted as the wrong name, and a candidate must
clear both. Two new fields on the report and in `operations`:

```jsonc
{"baseline_minting": {"n": 1156, "correct": 928, "wrong": 181, "declined": 47, "…": null},
 "candidate_minting": {"n": 1156, "correct": 884, "wrong": 57, "declined": 215, "…": null}}
```

The second veto is `swap_is_safe` only, not `may_install`: materiality is a rule
about not churning an operating point for a row or two, and this is a rule about
not installing a bar that invents people. A mint-neutral candidate has an
identical pair and passes.

### `recalld identity audit` → mint bursts

A fifth section, and a report like the other four. It names every run of **three
or more** voices minted from one source, each within **ten minutes** of the last,
and says what Discord's verdict was on the turns they were minted from. A run
whose seed turns all name one voice the bank already has is the cascade and says
so; a run with no verdicts is reported without the accusation, because a room
filling up with strangers looks the same from here. Both numbers are a report's
and nothing is refused or written on them.

### `identity.repair` — now two modes, exactly one required

```jsonc
{"method": "identity.repair", "params": {"phantoms": true, "apply": false}}
```

`--prototypes` deletes the *vectors* a burst wrote and cannot touch the *labels*.
On the live archive that left nineteen unnamed voices holding 304 of two people's
turns with no prototypes at all — nothing to delete, nothing to re-score, and
held-out precision reading in the thirties purely from rows filed under a number.
`--phantoms` is the other half.

A voice is a phantom when all three hold, and each is a consistency check with no
free parameter:

* it is **unnamed** — `display_name = auto_label` and the label is the minted
  `Speaker_NN` form. The moment somebody types a name over it, it is a person's
  voice and this command has no opinion about it;
* **no prototype it still holds stands up**: every one is condemned by the same
  check `--prototypes` uses. A voice with zero prototypes passes vacuously, which
  is precisely the population `--prototypes` leaves behind;
* **its rows agree**: of the rows carrying a Discord `single` verdict, a strict
  majority name one voice, and that voice is the target.

Majority rather than unanimity, and the reason is in the write: a row that
carries **its own** verdict goes to that verdict's voice, not to the target's, so
a phantom holding 168 of one person's turns and 2 of another's returns 168 and 2
to their owners. A voice with no majority at all is refused and named in the
preview. Every relabelled row is stamped `label_via: "truth"` and its
`match_score` cleared, because nothing was compared.

```jsonc
{
  "found": [{"speaker": 71, "name": "Speaker_71", "prototypes": 2, "condemned": 2,
             "rows": 186, "covered": 170,
             "says": [{"speaker": 2, "name": "Rowan", "rows": 168},
                      {"speaker": 25, "name": "Aspen", "rows": 2}],
             "target": {"speaker": 2, "name": "Rowan"}, "refused": null}],
  "merged": 16, "relabelled": 302, "prototypes_moved": 2, "note": null
}
```

Preview by default, `--apply` writes, **never automatic** — the same rule as
`--prototypes`, for the same reason. It is reversible: every row's prior
`(speaker, label_via, match_score)` goes into `operations` under
`identity.repair_phantoms` before it changes, and the voice is tombstoned rather
than deleted. `identity.repair` now requires exactly one of `prototypes: true` or
`phantoms: true`; asking for both, or neither, is a refusal.
## 0.12.5 — the mood pass gets its own switch, and the enrichment queue its honest split

Two fixes that turned out to be the same bug wearing two faces: a queue count
that included work the worker was never going to do, and a feature 0.12.4
shipped with a switch that lived only in `config.toml`.

### `mood.set` / `mood.get`

`[mood].enabled` moves from "edit the file and restart" to a live socket
switch, `graph.set`'s pattern exactly:

```
mood.set {"enabled": true}
→ the same shape as status.mood, plus "persisted": true
mood.get {}
→ the same shape as status.mood
```

Live because `crate::mood::run` re-reads `Control::mood` at the top of every
loop and `crate::mood::gate` re-reads it between rows — turning the switch off
stops the pass within one row, and turning it on starts it at the next look,
at most a minute away. Persisted for the reason `graph.set`'s is: a switch
that forgets by morning is not a switch a person can rely on. Nothing else
about the pass moved — `rows_per_run`, `batch_rows`, `min_duration_s` and
`live` are still `config.toml`-only, same as before.

`recalld mood [on|off|status]` is the CLI surface, over the socket like
`recalld graph`:

```
$ recalld mood
the mood pass         off (the default)
backlog                0 read, 0 to go — newest first

$ recalld mood on
the mood pass         on — listening overnight, on the niced cores
backlog                0 read, 4213 to go — newest first
```

### The mood pass reads newest-first

`Store::segments_for_mood`'s queue used to walk `t_start_ns` ascending, which
is right for a sweep nobody looks at and wrong for this one: the reason to
know a turn carried laughter is that somebody is about to open tonight's
transcript, and oldest-first means the row captured five minutes ago is
stamped last — after every one of the twenty thousand before it. The queue now
reads `ORDER BY g.t_start_ns DESC`. It stays resumable exactly as before —
`mood_at_ns IS NULL` is the cursor, not an offset, so a batch stamped removes
itself from the next query whichever end the walk starts from, and a daemon
killed mid-archive resumes rather than restarting. A turn captured *while* the
pass is running is newer than anything it has read and goes to the head of the
queue, which is the behaviour the ordering is for.

`status.mood.backlog` / `.read_total` and `recalld mood`'s own "backlog" line
describe this same newest-first queue.

### `GraphCounts` splits `threads_pending` into `threads_waiting` and `threads_too_short`

On one real install, `threads_pending` — unenriched conversations with any
words at all — was 919. The enrichment worker's own queue
(`Store::unenriched_threads`, gated on `[graph].min_thread_segments`) was 12.
The other 907 were conversations too short for the worker to ever open, and a
card that called all 919 "waiting" was describing a backlog nobody was working
behind a chip that honestly said "idle" — 0.12.4's bug, not 0.12.5's; this is
the fix.

```json
"counts": {
  …
  "threads_pending": 919,       // kept, unchanged, for an older client
  "threads_waiting": 12,        // threads_pending, narrowed to unenriched_threads' own filter
  "threads_too_short": 907,     // the rest: unread, and never going to be read
  "min_thread_segments": 3      // the floor the split was measured at
}
```

`threads_waiting + threads_too_short == threads_pending`, always — the second
is computed by subtraction from the first rather than by a second WHERE
clause, so the identity holds by construction rather than by two queries
happening to agree. `min_thread_segments` travels alongside the pair so a
client can write "under 3 turns" without a number of its own that the config
could move out from under it.

`recalld graph`'s own line follows the split:

```
topics              4 label(s) over 12 of 23 conversation(s), 12 waiting, 907 too short to read (under 3 turns)
```

And two sentences that used to describe an earlier gate now describe the real
one (`enrich::gate`): the startup INFO line and `recalld graph`'s own summary
said the worker "runs only while nothing is being captured", which stopped
being true when the gate moved to standing down only while audio is still
queued for transcription, or while capture is paused — no captured
application in view enters into it at all.

### Follow-up: what still needs config.toml or the CLI

`mood.set` closed one gap — a switch with copy in the GUI and no way to move
it short of an edit and a restart. It is not the only one. This is a plain
inventory, not a plan: every `config.rs` section and every `recalld` verb,
classified so the next pass knows where to spend a socket method rather than
rediscovering the gap by reading a bug report.

**User-facing** — a person would reasonably expect a control in the GUI, or
does today: `truth on/off`, `truth audio on/off`. and `sources`/`allow`/`deny`
already have one and are not listed twice.

| config.rs section | CLI verb(s) | today | gap |
|---|---|---|---|
| `[mic]`, `[room]` | `recalld mic`, `recalld room` | live (`mic.set`/`room.set`), in the Sources card | none |
| `[graph]` | `recalld graph on\|off` | live (`graph.set`), in the Memory card | none |
| `[mood]` (`enabled`) | `recalld mood on\|off` | live (`mood.set`), in the Memory card | **closed this round** |
| `[mood]` (`rows_per_run`, `batch_rows`, `min_duration_s`, `live`) | — | `config.toml` only | no control anywhere; `live` in particular trades a live mood chip against dropped-turn risk on the capture path and deserves the same kind of explicit opt-in `[truth].audio` got, not a silent default |
| `[assist]` (`reminders`, `digest`, `translate_to`, …) | `recalld digest` (read-only) | live (`assist.set`) for the translation half; `reminders`/`digest` themselves have no socket switch | a person cannot turn digests or reminders off from the GUI, only from `config.toml` |
| `[night]` | — | `config.toml` only; read-only via `status` | the window and the idle threshold that `[mood]` and `[asr].lang_sweep` both borrow have no control at all — moving them means editing a file three features silently depend on |
| `[asr]` (accuracy round, `lang_sweep`) | `recalld accuracy` | mostly read/measurement; `lang_sweep`'s own enable is `config.toml` only | the switch exists in the daemon and nowhere for a person to flip it |
| `[asr]` (`light_mode`, `light_mode_games`, `light_mode_gpu_busy_pct`) | `recalld models fetch --fallback-asr`/`--light` (fetch only) | live (`asr.light.set`), in the Memory card | **closed by light mode, below** |
| `[truth]` | `recalld truth on\|off`, `recalld truth audio on\|off` | needs a restart to take effect (unlike `graph`/`mood`) — the one already-user-facing switch that is NOT live | the oldest inconsistency in this table; fixing it means finding what in `truthnet`'s listener setup assumes it only runs once at start |
| `[identity]` | `recalld identity calibrate`, `recalld identity repair` | maintenance: run-on-demand operations, not settings with a state | fine as is — these are one-shot passes over the voicebank, not switches |
| `[retention]` | — | `config.toml` only; `status.storage` reports what it did | a person cannot see or change how long audio is kept without editing the file; the numbers that decide it are invisible until a sweep happens to log them |
| `[roster]`, `[socket]`, `[runtime]`, `[vad]`, `[models]`, `[capture]` | `recalld models fetch/status`, `recalld probe` | maintenance / measurement-only: process wiring, model downloads, capture tuning that assumes expert judgement | correctly CLI-only — a GUI control here would be a foot-gun, not a feature |

**Maintenance** (CLI, deliberately not in the GUI): `recalld speakers merge/split/prune/delete`, `recalld identity calibrate/repair`, `recalld turns rejudge`, `recalld models fetch/build-night`. These are corrective operations on the voicebank or the archive, not settings — running one is a decision with a before/after, not a state a card should show as "on".

**Measurement-only** (no switch, by design — see `[mood].live`'s own note above for the shape of the argument): `recalld accuracy report/learn`, `crate::mood::MOOD_IS_MEASURED` (`status.mood.rendered`), the calibration bake-off's `aggregate`/`aggregate_thresholds`. A number here changing what it reports on the next code change is expected; a number here becoming a knob is not, unless a future measurement earns it the way `[mood].live` will if RTF or the live-path budget ever changes.

The two closest to worth doing next, on this inventory: `[truth]` on/off not being live (the inconsistency, since its own `audio` sibling flag reads live-vs-restart the same way `[graph]`/`[mood]` do and a person has no way to tell which is which without reading this table), and `reminders`/`digest` having no switch at all despite `[assist]`'s other half being fully live.

## 0.13.0 — a backup you can trust

Additive, like 0.10.0's export: no method, event, field or behaviour described
above this line changes, and `proto` stays `1`. Every new key is present
rather than merely absent-when-off, the same rule `status.backup` follows
below.

### What a snapshot is

`backup create` writes, into one directory on a local filesystem the caller
names:

- `recall.db` — copied through SQLite's own **online backup API**, against a
  **second, independent, read-only connection** to the live file. WAL is what
  makes this safe: a reader never blocks the writer and is never blocked by
  it, so capture is never paused for this, unlike a restore (below).
- `segments/`, `goldens/`, `probes/` — by **hard link** where the destination
  is the same filesystem (a segment clip is never modified after it is
  written, so a link is exactly as durable a copy as `cp` would make and costs
  no I/O), and by copy where it is not. Any of the three may simply not exist
  yet; an empty tier backs up to nothing.
- `manifest.json` — every file's SHA-256 and byte count, the schema version,
  and the row counts (`sessions`, `speakers`, `segments`) a restore is checked
  against.
- `manifest.sig` — a keyed signature over the manifest bytes, the key kept at
  `<data-dir>/backup_key` (mode 0600, minted once, never asked for and never
  transmitted). Its only job is making a hand-edited manifest provably
  different from a real one — a plain hash alone cannot do that, since
  anybody can recompute a plain hash over their tampered copy. The manifest's
  own SHA-256 is also returned to the caller and printed by the CLI, so a
  person who wants to eyeball it on paper can.

### `backup.create` / `backup.verify` / `backup.restore`

All three are **async operations**, `export.run`'s shape exactly, because
hashing or copying a multi-gigabyte `segments/` tree is not a request that
must finish inside the round trip that named it:

```
backup.create {dir} → {op}
backup.verify {dir} → {op}
backup.restore {dir} → {op}
```

then `op.progress` / `op.done` / `op.failed` on the `ops` topic, `kind` set to
the method name. Progress is file-granularity for `create`; `verify` and
`restore` report only the terminal event, since neither has a plan to render
ahead of time.

- **`backup.create`**'s `op.done` carries `{dir, files, bytes,
  manifest_sha256, counts: {sessions, speakers, segments} | null,
  created_at_utc_ns}`. Refuses `err:refused` on the same path guard
  `export.run` uses (`crate::export::check_dir`, reused rather than
  reimplemented) — relative, `..`, a volatile runtime directory, or a network
  mount, identified the same way (`statfs(2)` against the same magic list).
  Also runs at **idle scheduling priority** on a dedicated thread
  (`crate::pipeline::background_current_thread`, the night shift's own
  discipline: `SCHED_IDLE` plus `[runtime].inference_nice`/`inference_cpus`),
  so a scheduled backup can never win a contest against a live capture.

- **`backup.verify`** re-hashes every file the manifest names, re-checks the
  signature, opens the copied database **read-only** and runs `PRAGMA
  integrity_check`, and compares row counts. It changes nothing. `op.done`
  carries `{dir, ok, files_checked, files_bad: [path], files_missing: [path],
  integrity_check, signature_valid, counts_match, manifest_sha256}`. `ok` is
  the AND of all four checks; a client should show `files_bad` and
  `files_missing` by name rather than only the boolean, the same reason
  `delete.preview` names rows instead of just a count.

- **`backup.restore`** refuses outright, `err:refused`, unless capture is
  paused (`pause` / `resume` — the panic-path switch, DESIGN §8) — checked
  once at the request and again on the worker thread right before anything is
  copied, since a `resume` landing in the gap between the two must still stop
  it. A restore under a live writer would restore into a database something
  else is still appending to, which is not a race this feature is willing to
  lose quietly.

  It copies the snapshot into a staging directory next to the live one,
  **verifies the staged copy** (not the source snapshot — a corruption
  introduced by the copy step itself must be caught too), and only then
  swaps: the live data directory is renamed to `<data-dir>.bak` (replacing
  any earlier one) and the staged copy takes its name. Both renames are
  same-filesystem and therefore atomic. `op.done` carries `{restored_into,
  previous_kept_as}`; `previous_kept_as` is `null` only when there was
  nothing at `data-dir` to keep. A snapshot that does not verify clean is
  never swapped in at all — the daemon's own data directory is left
  untouched, and the error names which check failed.

### `backup.get` / `backup.set`

The scheduled backup's switch, `mood.set`'s pattern exactly — live and
persisted, an omitted key leaves that field alone:

```
backup.set {enabled?, dir?, every_days?, keep?} → the same shape as backup.get, plus persisted: true
backup.get {} → {enabled, dir, every_days, keep, persisted, last}
```

`dir` is the folder scheduled runs write timestamped generations into (a
manual `backup.create` can target any directory it likes and does not touch
this setting); `every_days` and `keep` are the schedule's period and how many
generations to retain. `last` is the same object `backup.create`'s `op.done`
carries, or `null` before any backup — scheduled or by hand — has ever run.

### `status.backup`

Always present, on the same three-second poll as every other block on this
page:

```json
"backup": {
  "enabled": false, "dir": null, "every_days": 7, "keep": 4,
  "persisted": true,
  "last": null
}
```

`last` becomes the `backup.create` result once one has run. A client renders
`null` as "no backup yet" — the same "measured, not assumed" discipline
`status.storage` and `status.last_sweep` already keep, for the same reason: a
zeroed block here would claim a backup that never happened.

### CLI

```
recalld backup create <dir>     a consistent snapshot, right now
recalld backup verify <dir>     re-check one without touching it
recalld backup restore <dir>    swap it in over the live data dir
```

`create` and `verify` run in their own process against the data directory
directly, exactly like `recalld export`, and work whether or not the daemon
is running. `restore` is the one that asks: if a daemon is reachable on the
control socket, it is queried for `status.paused` first and the CLI refuses
with the same sentence the socket method does if capture is not paused; with
no daemon reachable at all, the data directory is not in use by anything and
the restore proceeds directly. `restore` also prints its "this will replace…"
confirmation and requires `--yes`, the one command here that touches the live
data directory in a way nothing else in this protocol does.
## 0.13.x — light mode: a smaller decoder while a game is running

§40 measured the live path's cost and found the lever: 89.5% of it is one
model, Parakeet-TDT 0.6b-v3, on the one runtime with no route to this
machine's GPU (`status.asr.devices`, above). The only knob left is a smaller
model, and `[asr].light_mode` is that knob — see `crate::light` and FINDINGS
§48 for the measurement.

### `asr.light.set` / `status.asr.light_mode`

```
asr.light.set {"mode": "on"}
→ the same shape as status.asr, plus "persisted": true
```

`status.asr.light_mode`:

```json
"light_mode": {
  "mode": "auto",
  "games": ["vrchat"],
  "gpu_busy_pct_threshold": 70,
  "light": false,
  "reason": "no game captured and the GPU is not sustained-busy",
  "model": "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
}
```

- `mode` — the switch: `"off"` (the default — the 110m export reads English only and measured 104.5% WER on a German-heavy archive, FINDINGS §49; a person opts in from the card), `"auto"`, or `"on"`. `asr.light.set`
  also accepts `games` (an array of lower-cased substrings, matched against a
  captured source's match key the same way `[identity].vrchat_sources`
  matches VRChat) and `gpu_busy_pct` (1-100) to move the other two knobs; at
  least one of `mode`/`games`/`gpu_busy_pct` must be present.
- `light` / `reason` — what is actually true **right now**, independent of
  `mode`: whether the smaller decoder is the one loaded, and why. In `auto`
  this is decided by `crate::light::decide` — a captured source matching
  `games`, or `gpu_busy_percent`'s 30-second median clearing
  `gpu_busy_pct_threshold` (FINDINGS §12/13: a single reading swings ±20
  points at 1 Hz, which is why it is a median over a window and not the last
  sample) — and it changes on its own, with no request from any client, the
  moment a game starts or stops. A client that only reads `mode` cannot tell
  the difference between "auto, and nothing is happening" and "auto, and the
  transcriber just got smaller"; a client that renders this card should read
  both.
- `model` — the export's directory name. `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8`
  or `sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8`, the same
  strings `status.asr.devices.light.model` and a row's own `asr_model_id`
  carry — a client never has to hard-code either name to answer "which one is
  this daemon running".

Live like `graph.set`/`mood.set`: the inference thread re-checks the switch
about once a second (`Pipeline::maybe_update_light_mode`) and reloads the
decoder — a real model load, the same one start-up pays — only when the
answer changed. Persisted to `config.toml` the same way, so a choice a person
made does not revert at the next restart. An automatic swap — a game starting,
or the GPU crossing the threshold — is announced on `Topic::Status` as its own
`light` event (`{"light": bool, "reason": str}`), because that transition
happens with no request in flight for `asr.light.set`'s own reply to ride on;
`status`'s next poll would eventually show it too, but a game starting is
exactly the moment CPU headroom matters and "eventually" is the wrong answer.

### The gate this feature shipped against

FINDINGS §49: CPU ≥ -50% in light mode (measured -58.0%, on 24.9 minutes of
the user's own archive, interleaved per clip against the model it replaces).
The WER cost — 104.5% against the 0.6b-v3 reading as reference, on an archive
that is 83% non-English, matching the catalogue's own "103% German" note for
this export — is **reported, not gated**, and the reason it is acceptable
despite that number is `Store::segments_for_night`'s other half of this round:
every row `asr_model_id` says was decoded by the light export is queued for
the night shift's third reading regardless of the cross-check's confidence
verdict, so a row light mode wrote wrong on a German lobby is not wrong for
long — the archive is never permanently downgraded, only for the hours between
the turn and the next night shift window.

### Operator steps

`recalld models fetch --fallback-asr` (also accepts `--light`) installs the
110m export the switch needs, ~108 MB compressed. Without it, `mode = "on"` or
an automatic `auto` swap that would otherwise fire logs a warning and the
default decoder keeps running — light mode never trades a missing optional
download for losing transcription outright.

## 0.14.0 — saved recall and context (schema v22)

Additive methods; `proto` remains `1`. Schema v21 introduces semantic
bookkeeping; v22 introduces `saved_searches`, `saved_moments`, and ordered
`saved_moment_segments` source references. The desktop moves processing,
translation, mood, light mode, vocabulary, and accuracy controls from Memory to
Settings; their existing protocol methods are unchanged.

### `segments.context`

Parameters: `{ "id": 123, "before": 3, "after": 3 }`. `id` is a positive
visible segment ID. `before` and `after` default to 3 and each accept 0–10.
Returns `{ "anchor": <segment>, "segments": [<segment>, ...] }`, using the
ordinary segment wire shape. The chronological list includes the anchor and
nearby visible turns from its thread, or its source session when no thread is
assigned. Deleted or missing anchors return `not_found`. This reads existing
source text; it does not generate a summary or require a language model.

### Saved searches

| Method | Parameters | Reply |
|---|---|---|
| `saved.searches.list` | `{limit?: 100, offset?: 0}` | `{searches: [...], total: N}` |
| `saved.searches.save` | `{id?, name, query?, filters?}` | `{search: {...}}` |
| `saved.searches.delete` | `{id}` | `{removed: bool}` |

A search record contains `id`, `name`, `query`, `filters`, and `created_ms`.
Omitting `id` creates a record; including it updates an existing record without
changing creation time. A missing update target returns `not_found`. The name
is trimmed, required, and at most 120 characters; query text is trimmed and at
most 4096 characters. `filters` defaults to `{}` and must be a JSON object whose
serialized representation is at most 16 KiB. The daemon stores filters as
client data; it does not run the search when saving or validate GUI filter
semantics.

The desktop uses this filter shape:

```json
{
  "speaker": "",
  "source": "",
  "world": "",
  "worldLabel": "",
  "mode": "keyword",
  "date": { "kind": "rolling", "days": 7 },
  "asked": null
}
```

`date` may instead be `{ "kind": "all" }` or
`{ "kind": "fixed", "from": "2026-09-01", "to": "2026-09-11" }`.
Fixed boundaries are local calendar dates, with the selected final day included
by querying through the next local midnight. Rolling boundaries are resolved
when reopened; explicit dates from a natural-language interpretation stay
fixed. `asked` may carry the existing query interpretation. The client preserves
speaker/source/world facets and search mode when reopening.

### Saved moments

| Method | Parameters | Reply |
|---|---|---|
| `saved.moments.list` | `{limit?: 100, offset?: 0}` | `{moments: [...], total: N}` |
| `saved.moments.save` | `{id?, segment_ids: [...], title?, note?}` | `{moment: {...}}` |
| `saved.moments.delete` | `{id}` | `{removed: bool}` |

A moment contains `id`, `title`, `note`, `segment_ids`, `segments`,
`created_ms`, and `unavailable_count`. Source segments use the ordinary wire
shape; no captured text or audio is duplicated into the saved-item tables.
`title` and `note` default to empty strings, are trimmed, and are limited to
180 and 4096 characters respectively. The note is a user annotation, separate
from the original transcript.

Saving accepts 1–20 unique positive segment IDs, validated transactionally as a
consecutive range of visible turns in one conversation or source session, with
a total span no greater than ten minutes. Input IDs are sorted chronologically.
Missing/deleted source rows return `not_found`; invalid ranges return
`params`. Updating replaces the referenced range, title, and note and
preserves creation time. A failed save leaves the existing record intact.

Listing reads current source rows, so corrections appear immediately and
deleted turns are not exposed. `segment_ids` and `segments` contain only visible
sources; `unavailable_count` counts original references no longer available.
Moments with no visible source turns are omitted from both the list and total.
Hard deletion cascades source references and removes a moment when its last
reference is deleted. Removing a saved item never deletes the transcript.

Both list methods default to 100 records, accept `limit` 1–200 and a nonnegative
`offset`, and order by newest creation time then descending ID. The desktop
provides independent pagination for moments and searches. Delete replies are
idempotent: deleting an absent item returns `removed: false`.

### Semantic bookkeeping (schema v21)

Transactional state tracks vector writes and deletion generations, per-model
coverage, and dirty transcript text. Text changes invalidate obsolete vectors
and queue affected visible rows; metadata-only changes do not rewrite FTS text.
Search operates on immutable snapshots with inference, whitening, and ranking
outside the store lock, then validates candidate freshness, deletion state, and
facets against current rows. Status reads use cached index state rather than
loading or refitting the index.

The existing semantic backfill command consumes resumable archived dirty work;
new live turns retain their existing pipeline. There is no new automatic
background reindex scheduler. Cold/delta database reads still hold the store
lock, and concurrent snapshot replacement can temporarily duplicate resident
matrix memory. No query protocol shape or model accuracy guarantee changes.


## 0.16.0 — whole conversations (schema v23)

`proto` remains 1. Additive, local methods:

| Method | Parameters | Reply |
|---|---|---|
| `history.page` | `{from,to,limit?:100,cursor?}` | `{segments:[...],next_cursor:string or null}` |
| `saved.moments.get` | `{id}` | `{moment:{...}}` |
| `saved.moments.move` | `{id,collection_id:id or null}` | `{moment:{...}}` |
| `saved.collections.list` | `{}` | `{collections:[{id,name,created_ms,count}]}` |
| `saved.collections.save` | `{id?,name}` | `{collection:{...}}` |
| `saved.collections.delete` | `{id}` | `{removed:bool}` |
| `performance.get` | `{}` | process-local measurements described below |

History boundaries are ISO timestamps, inclusive `from` and exclusive `to`.
Limits accept 1–200. Treat the cursor as opaque and reuse the exact boundaries;
it fixes a maximum segment ID and advances by nanosecond timestamp plus ID.
Deleted rows are filtered on each read. Refresh without a cursor to include
later recordings. History returns the ordinary full segment wire shape.

Moments add `collection_id`. Saving with an omitted collection preserves the
existing membership; null unfiles. Listing moments accepts `collection_id`:
omitted means all, null means unfiled, a positive ID scopes to that collection.
Collection names are trimmed, required, up to 120 characters, unique under
SQLite NOCASE. Deleting a collection unfiles its moments without deleting them.
Moment detail returns visible full segments in saved order; a missing or wholly
deleted source range returns `not_found`. Existing retention rules apply.

`performance.get` returns `scope:"current_process"`, `capture` and `search`
latency objects `{samples,window,latest_ms,p50_ms,p95_ms,max_ms}`, nullable
`resident_bytes`, nullable `queue:{seconds,capacity_seconds,dropped_chunks,
dropped_seconds}`, and nullable `repair`. Latency samples are finite,
nonnegative and limited to the newest 256 per category. Empty statistics are
null rather than zero. Percentiles use nearest rank. Capture timing uses a
monotonic clock from finalized audio end to stored nonempty transcript, before
semantic enrichment. Search covers request dispatch including errors and
answer generation, excluding transport/rendering. Memory is recalld VmRSS,
excluding Electron and child models. A missing capture queue is null.

`repair` is also included in `status.semantic` when its model is loaded. It
reports state, sampled pending count and its age, completed/failed/discarded
attempt counts, and retry delay. It keeps no text. Repair yields to capture,
foreground inference and busy database access, backs off on failures, and
resumes the durable dirty queue after restart. An in-flight model call finishes
on pause/shutdown but its result is discarded.


## 0.17.0 — recognition review and related moments (schema v24)

`proto` remains 1. The review table stores source IDs, revisions and review state,
not copied transcript/audio. Changes to words, decoder/language confidence or
source visibility update the revision. Speaker metadata changes do not reopen
word review. Migration backfills currently flagged visible words.

| Method | Parameters | Reply |
|---|---|---|
| `review.list` | `{limit?:30,before_id?}` | `{items:[segment + review_revision + review_reasons],next_before_id}` |
| `review.get` | `{segment_id}` | `{item:segment + review_revision}` |
| `review.mark` | `{segment_id,revision}` | `{reviewed:true}` or `conflict` |
| `saved.moments.related` | `{id,limit?:5}` | `{available:true,method:"shared_words",moments:[...],candidate_limit:200}` |

Review limits are 1–100, newest IDs first. Reasons are `decoder_disagreement`
(`asr_confidence=shaky`) and `language_mismatch`. A reviewed item reappears only
when source text/evidence changes. Correction uses existing `segments.correct`,
which now accepts optional `expected_text`; stale words produce `conflict`
without changing the transcript or publishing an event. The comparison and
write share a transaction. After correction, clients may fetch the new review
revision before marking it; never mark a revision inferred from client state.

Related moments use up to 24 normalized original-word tokens of length≥4,
excluding a small common-word list. Indexed FTS retrieves at most 200 candidate
moments; exact source-token overlap then ranks them, requiring at least two
shared tokens. Overlapping saved ranges are excluded. Personal notes/titles
are not matching input. Each result includes the ordinary saved moment plus
`shared_words` (up to six) and `reason`. This is bounded lexical suggestion,
not exhaustive semantic similarity. Limits are 1–10; hidden/deleted sources
cannot contribute evidence or returned text. No extra model is required.

`performance.get.stages` maps `queue_wait`, `recognition`,
`partial_recognition`, `overlap`, `speaker_embedding`, `speaker_matching`,
`transcript_commit`, `refinement`, `audio_write`, and `semantic` to the existing
bounded latency shape. Queue wait measures captured buffer end to processing
entry using monotonic time. Recognition separates primary final passes from
live caption calls. Finalization includes nested speaker ranking; refinement
includes language/speaker follow-up work. Stages have different sample counts
and may overlap: their medians are not additive. Missing samples remain null.
