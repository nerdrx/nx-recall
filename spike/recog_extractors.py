"""Arm 1: is there a better speaker extractor than the one that ships?

Every extractor is given exactly the same deal, because a bake-off in which
the candidate gets a bank grown this afternoon and the incumbent keeps a year
of prototypes is not a bake-off:

  * the bank is the LIVE bank's 207 prototypes, re-embedded from the very
    segments they were enrolled from — same voices, same audio, five spaces;
  * no row is scored against a prototype it produced itself, exactly as the
    shipping calibration does it;
  * the operating point is chosen per extractor on the fit split, because a
    threshold is a number on *this* extractor's score scale, and handing a
    512-d model the 192-d model's 0.35 measures the scale, not the model;
  * the held-out 40% is scored, and nothing else is ever read.

Two numbers are reported per extractor. **EER** is bank-free: over every pair
of truth rows, how separable are same-voice pairs from different-voice pairs.
It is the property the ladder is really buying, and it cannot be flattered by
a lucky bank. **Identification** is the ladder's own question against the real
bank, and it is what would actually change for the user.

    python3 spike/recog_extractors.py
"""

import numpy as np

import recog_lib as L

SCRATCH = "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/recog"

# name, key, dim, file MB, licence, measured RTF on 4 cores, front end as the
# published ONNX declares it.
MODELS = [
    ("ERes2Net (ships)", "eres2net", 192, 25.3, "Apache-2.0", 0.0095, "global-mean"),
    ("TitaNet-small (NeMo)", "titanet_small", 192, 38.4, "CC-BY-4.0", 0.0048, "per_feature"),
    ("CAM++ (3D-Speaker)", "campplus_3ds", 512, 27.0, "Apache-2.0", 0.0050, "global-mean"),
    ("CAM++ (WeSpeaker LM)", "campplus_lm", 512, 27.9, "Apache-2.0", 0.0048, "NONE DECLARED"),
    ("ResNet34 (WeSpeaker LM)", "resnet34_lm", 256, 25.3, "Apache-2.0", 0.0053, "NONE DECLARED"),
]


def unit(v):
    n = np.linalg.norm(v)
    return v / n if n > 0 else v


def load(name):
    d = np.load(f"{SCRATCH}/emb_{name}.npz")
    vecs = {int(i): v.astype(np.float64) for i, v in zip(d["ids"], d["vecs"])}
    bank = L.Bank([
        dict(id=n, speaker=int(sp), src=int(src), vec=v.astype(np.float64), golden=0, created=0)
        for n, (src, sp, v) in enumerate(
            zip(d["proto_src"], d["proto_speakers"], d["proto_vecs"]))
    ])
    con = ({int(i): v.astype(np.float64) for i, v in zip(d["ids"], d["concat"])}
           if "concat" in d else None)
    return vecs, bank, con


def eer(rows, vecs):
    X = np.stack([unit(vecs[r.id]) for r in rows])
    y = np.array([r.truth for r in rows])
    iu = np.triu_indices(len(rows), 1)
    s = (X @ X.T)[iu]
    same = (y[:, None] == y[None, :])[iu]
    pos, neg = s[same], s[~same]
    ths = np.linspace(-1, 1, 4001)
    far = np.array([(neg >= t).mean() for t in ths])
    frr = np.array([(pos < t).mean() for t in ths])
    i = int(np.argmin(abs(far - frr)))
    return (far[i] + frr[i]) / 2, pos.mean(), neg.mean()


def choose_threshold(rows, bank, vecs, agg, topk, you):
    """One global label threshold, on this extractor's own scale, on fit rows."""
    best = (-1.0, L.LABEL_THRESHOLD)
    ranked = {r.id: L.rank(bank, unit(vecs[r.id]), drop_segment=r.id, agg=agg, topk=topk)
              for r in rows if r.truth != you}
    for t in np.arange(-0.20, 0.96, 0.01):
        s = L.Score()
        for r in rows:
            if r.truth == you:
                continue
            s.add(L.decide(ranked[r.id], r.overlap, r.dur, {}, (float(t), 0.0)), r.truth)
        if s.f_beta() > best[0]:
            best = (s.f_beta(), float(t))
    return best[1]


def run(rows, bank, vecs, t, agg, topk, you):
    s, wrong = L.Score(), []
    for r in rows:
        if r.truth == you:
            continue
        ranked = L.rank(bank, unit(vecs[r.id]), drop_segment=r.id, agg=agg, topk=topk)
        lab = L.decide(ranked, r.overlap, r.dur, {}, (t, 0.0))
        s.add(lab, r.truth)
        if lab is not None and lab != r.truth:
            wrong.append((r.id, lab, r.truth))
    return s, wrong


HDR = (f"  {'extractor':<26}{'dim':>5}{'MB':>6}{'RTF':>8}{'thr':>7}"
       f"{'n':>6}{'correct':>9}{'wrong':>7}{'decl':>6}{'prec':>8}{'recall':>8}{'F-0.5':>8}")


def main():
    c = L.conn()
    rows = L.load_rows(c)
    you = L.you_speaker_id(c)
    cut = L.split_at([r.t for r in rows])
    fit, ev = rows[:cut], rows[cut:]
    scored = [r for r in rows if r.truth != you]
    print(f"rows {len(rows)}: fit {len(fit)} / held out {len(ev)}; "
          f"bank = the live 207 prototypes, re-embedded per extractor")
    print(f"own account: speaker {you} (excluded)")

    loaded = {k: load(k) for _l, k, *_ in MODELS}

    print("\n=== 1a. verification, bank-free: every pair of truth rows ===")
    print(f"  {'extractor':<26}{'front end':<16}{'licence':<12}"
          f"{'same':>8}{'diff':>8}{'EER':>9}")
    for label, key, _d, _mb, lic, _rtf, fe in MODELS:
        e, p, n = eer(scored, loaded[key][0])
        print(f"  {label:<26}{fe:<16}{lic:<12}{p:>8.3f}{n:>8.3f}{e * 100:>8.2f}%")
    print("  NONE DECLARED: the published ONNX carries no `feature_normalize_type`, so"
          "\n  sherpa-onnx feeds it un-normalised fbank. Those two rows are a packaging"
          "\n  artefact, not a verdict on the model.")

    for agg, topk, title in [("max", 2, "max cosine (today's rule)"),
                             ("topk", 3, "top-3 mean")]:
        print(f"\n=== 1b. identification against the real bank, {title} ===")
        print(HDR)
        for label, key, dim, mb, _lic, rtf, _fe in MODELS:
            vecs, bank, _ = loaded[key]
            t = choose_threshold(fit, bank, vecs, agg, topk, you)
            s, _ = run(ev, bank, vecs, t, agg, topk, you)
            print(f"  {label:<26}{dim:>5}{mb:>6.1f}{rtf:>8.4f}{t:>7.2f}"
                  f"{s.n:>6}{s.correct:>9}{s.wrong:>7}{s.declined:>6}"
                  f"{s.precision * 100:>7.1f}%{s.recall * 100:>7.1f}%{s.f_beta():>8.3f}")

    print("\n=== 1c. 2-extractor score fusion (mean of the two cosines) ===")
    print(f"  {'pair':<40}{'thr':>7}{'n':>6}{'correct':>9}{'wrong':>7}{'decl':>6}"
          f"{'prec':>8}{'recall':>8}{'F-0.5':>8}")
    keys = [m[1] for m in MODELS if m[6] != "NONE DECLARED"]
    for i, a in enumerate(keys):
        for b in keys[i + 1:]:
            va, ba, _ = loaded[a]
            vb, bb, _ = loaded[b]

            def fuse(r, va=va, ba=ba, vb=vb, bb=bb):
                ra = dict(L.rank(ba, unit(va[r.id]), drop_segment=r.id, agg="topk", topk=3))
                rb = dict(L.rank(bb, unit(vb[r.id]), drop_segment=r.id, agg="topk", topk=3))
                out = [(sp, (ra.get(sp, -1.0) + rb.get(sp, -1.0)) / 2)
                       for sp in set(ra) | set(rb)]
                out.sort(key=lambda x: (-x[1], x[0]))
                return out

            cache = {r.id: fuse(r) for r in rows if r.truth != you}
            best = (-1.0, 0.35)
            for t in np.arange(-0.20, 0.96, 0.01):
                s = L.Score()
                for r in fit:
                    if r.truth == you:
                        continue
                    s.add(L.decide(cache[r.id], r.overlap, r.dur, {}, (float(t), 0.0)), r.truth)
                if s.f_beta() > best[0]:
                    best = (s.f_beta(), float(t))
            t = best[1]
            s = L.Score()
            for r in ev:
                if r.truth == you:
                    continue
                s.add(L.decide(cache[r.id], r.overlap, r.dur, {}, (t, 0.0)), r.truth)
            print(f"  {a + ' + ' + b:<40}{t:>7.2f}{s.n:>6}{s.correct:>9}{s.wrong:>7}"
                  f"{s.declined:>6}{s.precision * 100:>7.1f}%{s.recall * 100:>7.1f}%"
                  f"{s.f_beta():>8.3f}")

    print("\n=== 5b. a longer embedding window (previous turn glued on, gap <= 2 s) ===")
    print(f"  {'arm':<40}{'n':>6}{'correct':>9}{'wrong':>7}{'decl':>6}"
          f"{'prec':>8}{'recall':>8}{'F-0.5':>8}")
    vecs, bank, con = loaded["eres2net"]
    t = choose_threshold(fit, bank, vecs, "topk", 3, you)
    for name, vs in [("this turn only", vecs), ("previous turn glued on", con)]:
        for lo, hi, blab in [(0.0, 1e9, "all"), (0.0, 1.5, "< 1.5 s"), (1.5, 3.0, "1.5 - 3 s"),
                             (3.0, 1e9, "3 s +")]:
            sub = [r for r in ev if lo <= r.dur < hi]
            s, _ = run(sub, bank, vs, t, "topk", 3, you)
            print(f"  {name + ', ' + blab:<40}{s.n:>6}{s.correct:>9}{s.wrong:>7}{s.declined:>6}"
                  f"{s.precision * 100:>7.1f}%{s.recall * 100:>7.1f}%{s.f_beta():>8.3f}")


if __name__ == "__main__":
    main()
