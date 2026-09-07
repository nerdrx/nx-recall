# NX Recall — public measurement overview

This file intentionally contains a short, privacy-safe summary of historical
measurements. Detailed notes, raw transcripts, recordings, and per-example
evidence are withheld from the public tree because they were collected during
private research. The figures below are aggregate results also reported in the
project README; they are not a substitute for the withheld evidence.

## Aggregate results

| Area | Aggregate result |
|---|---|
| English transcription | 1.4% WER |
| German transcription | 8.4% WER |
| Speaker identification from one second | 96% coverage, 2.5% EER |
| Overlapped speech in one measured lobby | 9.8% of speech |
| Daily digest gate | 6/6 low-value traps refused; 4/4 substantive examples summarised |
| Translation embeddings | 0.948 cosine against the reference set |
| Quarterly ASR refresh | Keep the incumbent; the nearest candidate measured 3.6% vs 3.3% lab WER; qwen3-asr tied on real audio and lost on speed |

These results establish the broad engineering direction: short speech can be
useful for identity, overlap is a distinct operating concern, and the digest
needs a refusal decision before prose generation. Exact datasets, utterances,
quotes, machine paths, and run-by-run measurements remain private.

## Reproducibility boundary

The public repository retains executable harnesses and synthetic fixtures where
they are useful for understanding schemas. It does not publish private audio,
transcripts, personally identifying speaker labels, or detailed historical
research commentary. Any future public benchmark should use newly authored
synthetic data or a separately licensed, documented corpus.
