# German speech fixtures

Three 3.5-second clips of German read speech, 16 kHz mono 16-bit — the format
every other fixture and every stored segment uses.

They exist for one test: the wrong-language re-decode
(`crates/recalld/tests/fixtures.rs`). The multilingual ASR export decodes these
as German, which is what makes them usable as the "this transcript disagrees
with the speaker's declared language" case — a case that cannot be produced by
injecting text, because the whole point is that the *audio* is re-decoded.

They are also the shape the flip was measured on: `spike/lang_flip.py` cut
FLEURS German utterances to lobby-sized windows and found 12% of 1 s fragments
and 5% of 2 s ones coming back as English.

## Provenance

Cut from the FLEURS dev set (`google/fleurs`, `de_de`), trimmed to 3.5 s from
0.4 s in and converted from 32-bit float to 16-bit PCM. FLEURS is published
under **CC-BY 4.0** (Google Research), which is why these three are small enough
and free enough to live in the repository; the full corpus is not, and is not
here.

Committed rather than downloaded because the test must run on a machine with no
network — the same rule the rest of the acceptance suite follows.
