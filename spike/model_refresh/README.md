# Model refresh — public procedure

This directory documents a repeatable quarterly check for the pinned ASR
model. Refresh runs use local, privately configured inputs and write generated
reports to the local scratch area. Generated transcripts and model outputs are
private artifacts and are ignored by version control; no real snippets or
recordings are part of the public repository.

## Running it

From `spike/`, run the three stages with the host's preferred low-priority
settings:

```sh
python model_refresh.py discover
python model_refresh.py bench
python model_refresh.py verdict
```

`discover` selects eligible newer multilingual offline checkpoints. `bench`
records aggregate lab metrics such as pooled WER, real-time factor, empty
output rate, and wrong-language rate, then writes generated local artifacts.
`verdict` applies the replacement rule to the newest local result and names any
failed criterion.

## Replacement rule

A candidate replaces the incumbent only when all of these gates pass:

1. at least 10% relative improvement on pooled lab WER;
2. real-time factor at most 0.50 on the configured machine;
3. wrong-language rate on short cuts no worse than the incumbent;
4. empty-output rate no more than one percentage point above the incumbent;
5. a blind quality comparison prefers it on at least 55% of decided pairs.

The public README records the aggregate historical outcome: keep the incumbent,
with the nearest candidate at 3.6% versus 3.3% lab WER and slower on real audio.
Detailed runs, transcripts, prompts, recordings, and model downloads are
local private artifacts and are intentionally omitted here.

## Scheduling

Run the stages quarterly using an existing scheduler appropriate to the host.
Keep the generated reports local and review the final verdict line. A public
release should include only aggregate, non-identifying results and newly
authored synthetic examples.
