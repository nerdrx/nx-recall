# NX Recall — Step 0 measurement spike

Before building capture plumbing, answer the question the whole design rests on:

> **Does timbre-based speaker labelling survive a VRChat lobby?**

The design brief estimates "good on 2–3 concurrent speakers, degrading past that."
This spike replaces that estimate with numbers.

## What it measures

Enrollment is **clean** audio; probes are **degraded**. That asymmetry is the real
deployment case — you enroll people from good sources (Discord, close mic) and then
try to recognise them through VRChat's lossy, many-talker mix.

Three sweeps, all against the same 40-speaker gallery:

| Sweep | Question |
|---|---|
| **Codec** — Opus 32k…8k | How much does VRChat's voice codec cost? |
| **Duration** — 0.5s…8s | Are short utterances (`"yeah"`, `"wait what"`) labelable? |
| **Overlap × dominance** — 1…10 talkers × 0/+6/+12 dB | The lobby question |

**Dominance** is the variable the brief doesn't have: how many dB the target talker
sits above *each* interferer. Ten people at equal loudness and one person beside you
over nine distant voices are completely different problems, and real lobbies are
mostly the second one.

## Signal chain

Each remote talker is encoded **individually** before the mix, because that is what
VRChat does — every person's mic is codec'd on their machine and your client mixes
the decoded streams. Encoding the finished mix instead would flatter the codec and
understate the damage.

```
per talker:  clean mic -> Opus (voip) -> decode -> gain (near/far)
             sum -> captured mix -> speaker embedding
```

## Metrics

- **EER** — equal error rate; the standard speaker-verification number.
- **TAR@FAR=1%** — how much speech gets labelled when the threshold is set tight
  enough to keep impostors out. Since "false merges are poison," this is the
  operating point that decides whether the tool is usable.
- **rank-1** — closed-set: is the best match in the whole gallery the right person?
- **steal rate** — overlap only: how often the best match is another talker who is
  *actually in the mix*. This is the realistic lobby mislabel, and it is the number
  that predicts false merges.

Impostor scores exclude anyone audible in the mix — scoring against a talker who is
present would count a correct detection as a false accept.

## Models

Both are the real shipping candidates, run through `sherpa-onnx` (the brief's chosen
dependency), so the numbers transfer directly:

- `3dspeaker_speech_eres2net_sv_en_voxceleb_16k` — ERes2Net, English, 26 MB, 192-dim
- `nemo_en_titanet_small` — 40 MB, 192-dim; stands in for §4's
  "VRChat running → downgrade to the smaller model" rule

## Corpus

LibriSpeech `dev-clean` (40 speakers). Read speech, not conversational — separability
is measured fairly, but real lobby speech is shorter and less articulate, so treat
these as an **optimistic ceiling**.

## Measuring a real lobby (the remaining open question)

Everything above is synthetic. The one thing that still gates the design is whether
real lobbies deliver enough dominance. Self-contained — `venv/` and `models/` ship
here, no setup:

```bash
./venv/bin/python record_lobby.py 20
```

Records the output monitor (what you hear, not your mic) to 16 kHz mono FLAC, then
runs the analysis automatically. `Ctrl+C` stops early; pass no argument to record
until interrupted. To analyse an existing file from any recorder:

```bash
./venv/bin/python measure_lobby.py lobby.flac
```

It reports speech/overlap split, unsupervised identity recovery, and the headline —
*what fraction of speech time is single-speaker and confidently identified* — placed
against the lab reference table.

## Re-running the synthetic sweeps

These additionally need LibriSpeech `dev-clean` unpacked under `$NXR_SCRATCH/corpus`:

```bash
export NXR_SCRATCH=/path/with/corpus
./venv/bin/python run_spike.py && ./venv/bin/python report.py
./venv/bin/python score_ops.py       # fixed-threshold operating point
./venv/bin/python overlap_detect.py  # the validated gate
```

`smoke.py` validates each stage independently first. `vecs_*.npz` holds every probe
embedding, so new metrics never require re-running inference.

## Known gaps

- **No HRTF.** Spatialisation applies a direction-dependent filter per source. Given
  the codec at 24 kbps costs only ~0.02 cosine, this is expected to be a second-order
  effect next to overlap — but it is untested, not proven negligible.
- **Read speech, not lobby speech.** Real utterances are shorter, more disfluent, and
  overlap more raggedly.
- **No reverb / world audio / music bleed.**
- Interferers are drawn from the same enrolled gallery, which is realistic for a
  friend group but makes the steal rate a *worst case* for strangers.
