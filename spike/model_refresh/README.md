# Model refresh — the standing procedure

NX Recall pins one ASR checkpoint (`parakeet-tdt-0.6b-v3-int8`, exported
2025-08-16). sherpa-onnx publishes new exports every few weeks. This directory
is how that pin gets re-examined on a schedule instead of on a hunch, and the
rule that decides it is written down *before* the numbers arrive, so a swap is
never argued into existence after the fact.

Everything lives in `spike/model_refresh.py`. Nothing here touches the live
store (`~/.local/share/nx-recall`, `~/.config/nx-recall`); the real audio is
the frozen lobby recording under `spike/clips/`, and downloads land in the
scratchpad.

## Running it

```sh
cd spike
taskset -c 16-19 nice -n 19 venv/bin/python model_refresh.py discover
taskset -c 16-19 nice -n 19 venv/bin/python model_refresh.py bench
taskset -c 16-19 nice -n 19 venv/bin/python model_refresh.py verdict
```

Four cores at nice 19 is not a suggestion. The box runs a VR session and other
work; more importantly, **a model that only clears the RTF bar when it owns the
machine has not cleared it** — the daemon decodes in the background while the
user is in VR.

- `discover` — lists sherpa-onnx ASR exports newer than the pinned v3 from the
  GitHub `asr-models` release and `csukuangfj/*` on Hugging Face. It filters
  against a hard-coded allowlist of *families* (`FAMILIES` in the script):
  multilingual covering **de and en**, offline, int8, ≤ 1.5 GB. Everything else
  is counted with the reason it was skipped, so the skips are auditable. Adding
  a family is a three-line edit; that is the intended way to widen the net.
  Writes `discover.json`.
- `bench` — downloads what `discover` picked (via `pardl.py`, 12 ranged
  connections — the uplink shapes per connection), then runs every candidate
  *and the incumbent* through the §11 measurements: lab WER on FLEURS German +
  LibriSpeech English through Opus 24k, RTF, empty-output rate, wrong-language
  rate on 1.5 s cuts, and — only for candidates that already beat the incumbent
  on lab WER — the real-audio LLM-judge A/B. Writes `<date>.json` (including
  every transcript) and `<date>.md` (the table).
- `verdict` — applies the rule to the newest `<date>.json` and names the
  criterion that failed. `bench` prints it too.

Expect hours. Cheap transducers finish in minutes; an LLM-decoder model such as
qwen3-asr is the long pole.

## The rule

A candidate replaces v3 only if **all five** hold:

1. **Pooled lab WER** (de + en full utterances, one pooled error rate) at least
   **10 % relative** better than v3's.
2. **RTF ≤ 0.50** on four cores, measured on the real lobby clips.
3. **Wrong-language rate on 1.5 s cuts ≤ v3's.** This is the criterion that
   disqualified whisper-turbo in §11: it hallucinated French for an English
   fragment. Short Denglisch turns are the product's actual input.
4. **Empty-output rate ≤ v3's + 1 pp.** canary-180m was fast and
   language-perfect and dropped 11 % of turns; a dropped turn is worse than a
   wrong one, because nothing in the UI says it happened.
5. **The real-audio judge prefers it on ≥ 55 % of decided pairs** (ties and
   identical decodes are not decided pairs).

Anything else: **keep v3**, with the failing criterion named.

Why these five: lab WER alone ranks models on read speech the product never
sees, so it is a gate and not the decision; RTF and the two failure-rate
criteria encode the two ways a "better" model has already made things worse
here; the judge is the only measurement taken on the audio the user actually
produces. The judge's sample is small and it is used last, as a veto on
candidates that already won on the lab set — not as a ranking.

## Quarterly

One line, no daemon, no unit file installed — a systemd user timer is the
tidiest form:

```sh
systemd-run --user --on-calendar=quarterly --unit=nx-recall-model-refresh \
  sh -c 'cd ~/…/nx-recall/spike && taskset -c 16-19 nice -n 19 venv/bin/python model_refresh.py discover && taskset -c 16-19 nice -n 19 venv/bin/python model_refresh.py bench'
```

or, as cron: `0 4 1 1,4,7,10 *`. Read the `verdict` line in the generated
`<date>.md` afterwards; it is the only output that needs a human.

## Where the numbers come from

- Lab set: `spike/model_refresh.py:lab_set()` — the same seeded 40 FLEURS German
  + 40 LibriSpeech English utterances as FINDINGS §11, plus 1.5 s / 3 s cuts, all
  through Opus 24k.
- Real set: `spike/clips/*.wav` — 193 single-speaker clips (≈472 s, mean 2.45 s)
  cut from the user's own 20-minute VRChat lobby recording in §9. §11 sampled
  240 segments out of the live product database instead; a re-runnable procedure
  must not depend on what happens to be in the database this quarter, so the
  refresh uses the frozen clips. Same lobby, same codec path, same crosstalk.
- Judge: `llama-cli` + Qwen2.5-3B from the scratchpad, grammar-constrained to
  `{"better": "A"|"B"|"same"}`, blind A/B with the sides flipped at random and
  the two preceding clips as context.
