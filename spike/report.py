"""Render results.json as the three tables that answer the design questions."""
import json
import sys
from pathlib import Path

R = json.loads((Path(__file__).parent / "results.json").read_text())
res, cfg = R["results"], R["config"]

BR = ["clean", "32k", "24k", "16k", "12k", "8k"]
DUR = ["0.5s", "1.0s", "2.0s", "3.0s", "5.0s", "8.0s"]
TK = [1, 2, 3, 4, 6, 8, 10]
DOM = [0, 6, 12]


def row(m, key):
    d = res[m].get(key)
    if not d:
        return None
    return d


def table(title, header, rows):
    print(f"\n{title}")
    print("  " + header)
    print("  " + "-" * len(header))
    for r in rows:
        print("  " + r)


for m in res:
    print(f"\n{'=' * 78}\n{m.upper()}   ({cfg['speakers']} speakers, "
          f"{cfg['n_enroll']} clean prototypes each, {cfg['n_probe']} probes each)\n{'=' * 78}")

    rows = []
    for b in BR:
        d = row(m, f"codec/{b}")
        if d:
            rows.append(f"{b:>6}  {d['eer']*100:6.2f}%  {d['tar_at_far1']*100:6.1f}%  "
                        f"{d['rank1']*100:6.1f}%   {d['same_mean']:+.3f} / {d['diff_mean']:+.3f}")
    table("CODEC  (1 talker, 3s window)  -- how much does VRChat's voice codec cost?",
          f"{'bitrate':>6}  {'EER':>7}  {'TAR@1%':>7}  {'rank-1':>7}   same/diff cos", rows)

    rows = []
    for d_ in DUR:
        d = row(m, f"dur/{d_}")
        if d:
            rows.append(f"{d_:>6}  {d['eer']*100:6.2f}%  {d['tar_at_far1']*100:6.1f}%  "
                        f"{d['rank1']*100:6.1f}%   {d['same_mean']:+.3f} / {d['diff_mean']:+.3f}")
    table(f"DURATION  (1 talker, {cfg['ref_bitrate']}k opus)  -- are short utterances labelable?",
          f"{'window':>6}  {'EER':>7}  {'TAR@1%':>7}  {'rank-1':>7}   same/diff cos", rows)

    print(f"\nOVERLAP x DOMINANCE  ({cfg['ref_dur']}s, {cfg['ref_bitrate']}k opus)"
          "  -- the lobby question")
    print("  dominance = how many dB the target talker is above EACH interferer")
    for metric, label, pct in [("tar_at_far1", "TAR@FAR=1%  (fraction of speech labelled)", True),
                               ("rank1", "rank-1 accuracy (closed set, 40 speakers)", True),
                               ("steal", "steal rate (best match is another talker IN the mix)", True),
                               ("eer", "EER", True)]:
        print(f"\n  {label}")
        print("    talkers " + "".join(f"{f'+{d}dB':>10}" for d in DOM))
        for n in TK:
            cells = []
            for dm in DOM:
                d = row(m, f"overlap/{n}tk_{dm}dB")
                cells.append(f"{d[metric]*100:9.1f}%" if d else "        -")
            print(f"    {n:>7} " + "".join(cells))
