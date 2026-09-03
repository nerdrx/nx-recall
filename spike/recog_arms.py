"""The recognition round, arms 2-5: hygiene, scoring, a roster prior, short turns.

Every arm is scored on the same held-out rows as `recalld identity calibrate`
(chronological 60/40, own account excluded, no row judged against a prototype
it produced itself), against the arm the live daemon runs today.

    python3 spike/recog_arms.py [hygiene|scoring|prior|short|all]
"""

import sys
from collections import defaultdict

import numpy as np

import recog_lib as L


def setup():
    c = L.conn()
    rows = L.load_rows(c)
    protos = L.load_bank(c)
    you = L.you_speaker_id(c)
    nm = L.names(c)
    cut = L.split_at([r.t for r in rows])
    return c, rows, protos, you, nm, cut


def refit(fit_rows, bank, **kw):
    """The per-voice thresholds this arm's own fit split produces."""
    return L.thresholds_from_fit(L.fit_thresholds(L.obs_for_fit(fit_rows, bank, **kw)))


def report(base, cand, label=""):
    v = L.verdict(base, cand)
    df = cand.f_beta() - base.f_beta()
    dp = (cand.precision - base.precision) * 100
    dw = (base.wrong - cand.wrong) / base.wrong * 100 if base.wrong else 0.0
    print(f"  gate{label}: precision {dp:+.1f} pp, F-0.5 {df:+.3f}, "
          f"wrong {base.wrong} -> {cand.wrong} ({-dw:+.0f}%)  => {v}"
          f"{'' if df >= 0.005 or v == 'FAIL' else '  (under the stated +0.005 F margin)'}")


# ---- prototype provenance ---------------------------------------------------

PROV = """
  SELECT p.id, g.truth_verdict, g.t_start_ns, d.speaker_id, g.truth_coverage
    FROM speaker_prototypes p
    LEFT JOIN segments g ON g.id = p.source_segment_id
    LEFT JOIN discord_users d ON d.user_id = g.truth_user_id
   WHERE p.embed_model_id = ?
"""


def provenance(c, model=L.MODEL):
    return {
        r[0]: dict(verdict=r[1], t=r[2], truth_sp=r[3], cov=r[4])
        for r in c.execute(PROV, (model,))
    }


def suspects(protos, prov, you, before_ns=None):
    """Prototypes ground truth condemns, by class.

    `before_ns` restricts the evidence to the fit split: a repair that reads a
    held-out row's verdict has read the exam paper, even though what it edits
    is the bank rather than the answer.
    """
    overlap, wrong_person = set(), set()
    for p in protos:
        pr = prov.get(p["id"])
        if not pr or pr["verdict"] is None:
            continue
        if before_ns is not None and (pr["t"] is None or pr["t"] >= before_ns):
            continue
        if pr["verdict"] == "overlap":
            overlap.add(p["id"])
        elif (
            pr["verdict"] == "single"
            and pr["truth_sp"] is not None
            and pr["truth_sp"] != p["speaker"]
            # The user's own account is not ground truth about audio captured
            # from the user's own Discord client (0.10.1, §17). The same rule
            # that keeps those rows out of the headline keeps them from
            # condemning a prototype.
            and pr["truth_sp"] != you
        ):
            wrong_person.add(p["id"])
    return overlap, wrong_person


def without(protos, drop):
    return L.Bank([p for p in protos if p["id"] not in drop])


def hygiene(c, rows, protos, you, nm, cut):
    fit, ev = rows[:cut], rows[cut:]
    t_cut = rows[cut].t
    prov = provenance(c)
    bank = L.Bank(protos)
    pv = refit(fit, bank)
    base, _ = L.judge(ev, bank, pv, you=you)

    print("\n=== 2. prototype hygiene ===")
    print(f"  {len(protos)} prototypes; fit-split evidence is everything before "
          f"t={t_cut} (segment {rows[cut].id})")

    fo, fw = suspects(protos, prov, you, before_ns=t_cut)
    ao, aw = suspects(protos, prov, you)
    print(f"  overlap-sourced:   {len(fo)} in the fit split, {len(ao)} in all of truth")
    print(f"  wrong-person:      {len(fw)} in the fit split, {len(aw)} in all of truth")
    for pid in sorted(aw):
        pr = prov[pid]
        owner = next(p["speaker"] for p in protos if p["id"] == pid)
        side = "fit" if pr["t"] < t_cut else "HELD OUT"
        print(f"    proto {pid}: {nm.get(owner)} ({owner}) but truth says "
              f"{nm.get(pr['truth_sp'])} ({pr['truth_sp']}) at cov {pr['cov']:.2f}  [{side}]")

    print()
    print(L.HEADER)
    print(base.row("shipping (per-voice, raw)"))
    arms = [
        ("H1 drop overlap-sourced (fit)", fo),
        ("H2 drop wrong-person (fit)", fw),
        ("H3 drop both (fit)", fo | fw),
    ]
    results = {}
    for name, drop in arms:
        b2 = without(protos, drop)
        s, w = L.judge(ev, b2, refit(fit, b2), you=you)
        print(s.row(f"{name} [-{len(drop)}]"))
        results[name] = (s, w, drop)
    print()
    print("  DIAGNOSTIC (leaks: reads held-out verdicts to edit the bank)")
    for name, drop in [("D1 drop overlap-sourced (all)", ao),
                       ("D2 drop wrong-person (all)", aw),
                       ("D3 drop both (all)", ao | aw)]:
        b2 = without(protos, drop)
        s, _ = L.judge(ev, b2, refit(fit, b2), you=you)
        print(s.row(f"{name} [-{len(drop)}]"))
    print()
    for name, (s, _w, _d) in results.items():
        report(base, s, f" [{name}]")

    # ---- the third framing, and the one the repair actually ships under.
    #
    # H1-H3 restrict the *evidence* to the fit split, which is the right rule
    # for anything fitted — a threshold, a matrix. Nothing here is fitted:
    # "this prototype's own source segment is a turn Discord says was a
    # different person" is a deterministic consistency check with no free
    # parameters, so a chronological split is not what protects it. What
    # protects it is not scoring a row whose own verdict the repair consumed.
    # So: the repair may read every verdict, and the three segments whose
    # verdicts condemned a prototype are struck from the held-out set.
    used = set()
    for pid in aw | ao:
        pr = prov[pid]
        used.add(next(
            (p["src"] for p in protos if p["id"] == pid), None))
    ev_clean = [r for r in ev if r.id not in used]
    print(f"\n  evidence-excluded: {len(ev) - len(ev_clean)} held-out rows struck "
          f"(their own verdict was the repair's evidence)")
    print(L.HEADER)
    b0, _ = L.judge(ev_clean, bank, refit(fit, bank), you=you)
    print(b0.row("shipping (per-voice, raw)"))
    for name, drop in [("E1 drop wrong-person", aw),
                       ("E2 drop overlap-sourced", ao),
                       ("E3 drop both", aw | ao)]:
        b2 = without(protos, drop)
        s, w = L.judge(ev_clean, b2, refit(fit, b2), you=you)
        print(s.row(f"{name} [-{len(drop)}]"))
        report(b0, s, f" [{name}]")
        results[name] = (s, w, drop)
    return base, results


# ---- multi-prototype scoring ------------------------------------------------


def scoring(c, rows, protos, you, nm, cut):
    fit, ev = rows[:cut], rows[cut:]
    bank = L.Bank(protos)
    base, _ = L.judge(ev, bank, refit(fit, bank), you=you)
    print("\n=== 3. multi-prototype scoring ===")
    print(L.HEADER)
    print(base.row("max cosine (ships)"))
    for name, agg, k in [("top-2 mean", "topk", 2), ("top-3 mean", "topk", 3),
                         ("top-4 mean", "topk", 4),
                         ("top-5 mean", "topk", 5), ("centroid (mean of all)", "mean", 0)]:
        pv = refit(fit, bank, agg=agg, topk=k)
        s, _ = L.judge(ev, bank, pv, you=you, agg=agg, topk=k)
        print(s.row(name))
        report(base, s, f" [{name}]")

    print("\n  is it the aggregate or the refitted threshold? both arms at the global 0.35:")
    print(L.HEADER)
    for name, agg, k in [("max cosine, global 0.35", "max", 2),
                         ("top-3 mean, global 0.35", "topk", 3)]:
        s, _ = L.judge(ev, bank, {}, you=you, agg=agg, topk=k)
        print(s.row(name))

    print("\n  who top-3 still gets wrong, held out:")
    pv3 = refit(fit, bank, agg="topk", topk=3)
    for v, (t, m) in sorted(pv3.items()):
        print(f"    fitted: voice {v:<3} {nm.get(v, '?'):<14} t={t} m={m}")
    _s, w3 = L.judge(ev, bank, pv3, you=you, agg="topk", topk=3)
    for seg, lab, sc, truth, dur in w3:
        print(f"    segment {seg}  named {nm.get(lab)} ({lab}) at {sc:.2f}, "
              f"really {nm.get(truth)} ({truth}), {dur:.1f} s")
    plda(c, rows, protos, you, cut, bank, base)


def fit_plda(train_rows, bank, lam):
    """A two-covariance PLDA, fitted on `train_rows` only.

    Within- and between-class scatter from length-normalised embeddings, both
    shrunk towards a scaled identity: with three classes in 192 dimensions
    each is rank-deficient by construction, and an unshrunk inverse promotes
    the directions the sample never saw to the loudest thing in the space
    (§18). Returns a scorer with the same shape `identity::rank` has.
    """
    X = np.stack([r.vec / np.linalg.norm(r.vec) for r in train_rows])
    y = np.array([r.truth for r in train_rows])
    dim = X.shape[1]
    Xc = X - X.mean(axis=0)
    Sw = np.zeros((dim, dim))
    means = []
    for cl in np.unique(y):
        Z = Xc[y == cl]
        m = Z.mean(axis=0)
        means.append((len(Z), m))
        Sw += (Z - m).T @ (Z - m)
    Sw /= max(1, len(X) - len(means))
    Sb = sum(n * np.outer(m, m) for n, m in means) / max(1, len(X))
    Swr = (1 - lam) * Sw + lam * np.trace(Sw) / dim * np.eye(dim)
    Sbr = (1 - lam) * Sb + lam * np.trace(Sb) / dim * np.eye(dim)
    Wi = np.linalg.inv(Swr)
    Q = Wi - np.linalg.inv(Swr + Sbr)
    P = Wi @ np.linalg.inv(Wi + 2 * np.linalg.inv(Sbr)) @ Wi
    pm = bank.m
    self_term = 0.5 * np.einsum("ij,jk,ik->i", pm, Q, pm)

    def raw(r):
        v = r.vec / np.linalg.norm(r.vec)
        return (pm @ P @ v) + 0.5 * float(v @ Q @ v) + self_term

    # The ladder's threshold grid is [0.30, 0.60] on a cosine scale. An LLR is
    # not on that scale, so it is squashed monotonically — no ranking changes —
    # with a centre and width read off the TRAINING rows. Without a fitted
    # width the squash saturates, every row clears every threshold, and the arm
    # has no refusal path at all; an arm that cannot decline is not comparable
    # to one that can.
    tops = np.array([raw(r).max() for r in train_rows])
    centre, width = float(np.median(tops)), float(tops.std()) or 1.0

    def scorer(r, candidates=None):
        s = raw(r)
        keep = bank.srcs != r.id
        if candidates is not None:
            keep &= np.isin(bank.speakers, list(candidates))
        out = []
        for sp in bank.voices:
            sel = bank._masks[sp] & keep
            if sel.any():
                out.append((sp, 0.45 + 0.15 * float(np.tanh((float(s[sel].max()) - centre) / width))))
        out.sort(key=lambda t: (-t[1], t[0]))
        return out

    return scorer


def plda(c, rows, protos, you, cut, bank, base):
    fit, ev = rows[:cut], rows[cut:]
    inner_cut = L.split_at([r.t for r in fit])
    inner_fit, inner_ev = fit[:inner_cut], fit[inner_cut:]
    print(f"\n  PLDA-lite: shrinkage on an inner split of the fit split "
          f"({len(inner_fit)} / {len(inner_ev)})")
    grid = (0.05, 0.2, 0.5, 0.8, 0.95)
    inner_base, _ = L.judge(inner_ev, bank, refit(inner_fit, bank), you=you)
    print(f"    inner baseline (max cosine): F-0.5 {inner_base.f_beta():.3f}")
    best = None
    for lam in grid:
        sc = fit_plda(inner_fit, bank, lam)
        pv = L.thresholds_from_fit(L.fit_thresholds(L.obs_for_fit(inner_fit, bank, scorer=sc)))
        s, _ = L.judge(inner_ev, bank, pv, you=you, scorer=sc)
        print(f"    shrink {lam:<5} inner  n={s.n} correct={s.correct} wrong={s.wrong} "
              f"declined={s.declined}  P {s.precision*100:.1f}%  F-0.5 {s.f_beta():.3f}")
        if best is None or s.f_beta() > best[0]:
            best = (s.f_beta(), lam)
    lam = best[1]
    print(f"    chosen shrinkage: {lam}")

    print()
    print(L.HEADER)
    print(base.row("max cosine (ships)"))
    sc = fit_plda(fit, bank, lam)
    pv = L.thresholds_from_fit(L.fit_thresholds(L.obs_for_fit(fit, bank, scorer=sc)))
    s, w = L.judge(ev, bank, pv, you=you, scorer=sc)
    print(s.row(f"PLDA-lite (shrink {lam})"))
    report(base, s, f" [PLDA-lite {lam}]")
    print("    DIAGNOSTIC, every shrinkage carried to the held-out rows:")
    for g in grid:
        scg = fit_plda(fit, bank, g)
        pvg = L.thresholds_from_fit(L.fit_thresholds(L.obs_for_fit(fit, bank, scorer=scg)))
        sg, _ = L.judge(ev, bank, pvg, you=you, scorer=scg)
        print(sg.row(f"  shrink {g}"))
    n = sum(1 for r in ev if r.truth != you)
    top1 = sum(1 for r in ev if r.truth != you and sc(r)[0][0] == r.truth)
    mx = sum(1 for r in ev if r.truth != you
             and L.rank(bank, r.vec, drop_segment=r.id)[0][0] == r.truth)
    t3 = sum(1 for r in ev if r.truth != you
             and L.rank(bank, r.vec, drop_segment=r.id, agg="topk", topk=3)[0][0] == r.truth)
    print(f"    ranking alone, no operating point: the true voice is top on "
          f"{mx}/{n} rows for max cosine, {t3}/{n} for top-3 mean, {top1}/{n} for PLDA-lite.")
    for seg, lab, scv, truth, dur in w:
        print(f"    segment {seg}  named {lab} at {scv:.2f}, really {truth}, {dur:.1f} s")


# ---- the roster prior -------------------------------------------------------


def prior(c, rows, protos, you, nm, cut):
    fit, ev = rows[:cut], rows[cut:]
    bank = L.Bank(protos)
    pv = refit(fit, bank)
    base, _ = L.judge(ev, bank, pv, you=you)
    print("\n=== 4. a presence prior ===")

    n_roster = c.execute("SELECT COUNT(*) FROM session_roster").fetchone()[0]
    vr = c.execute(
        "SELECT COUNT(*) FROM segments g JOIN sessions s ON s.id=g.session_id "
        "JOIN sources so ON so.id=s.source_id "
        "WHERE g.truth_verdict='single' AND so.kind='vrchat'"
    ).fetchone()[0]
    print(f"  session_roster holds {n_roster} row(s); {vr} of the truth rows are "
          f"VRChat-sourced. The VRChat roster is not measurable on this corpus.")

    # (a) the daemon's OWN labels: who it named in this session recently.
    #     Built from `segments.speaker_id`, which is model output, not truth —
    #     so it is not circular with the Discord verdicts, but it IS
    #     self-reinforcing: a voice the ladder wrongly named is then permitted.
    heard = defaultdict(list)
    for sid, sp, t in c.execute(
        "SELECT session_id, speaker_id, t_start_ns FROM segments "
        "WHERE speaker_id IS NOT NULL AND deleted_at IS NULL ORDER BY t_start_ns"
    ):
        heard[sid].append((t, sp))

    def recent(minutes):
        reach = int(minutes * 60 * 1e9)

        def f(r):
            seen = {sp for t, sp in heard.get(r.session_id, []) if abs(t - r.t) <= reach}
            return seen or None  # nobody heard: do not restrict, let it mint

        return f

    # (b) Discord's own speaking events. CIRCULAR: `truth_speaking` is the very
    #     table the `single` verdict is computed from, so a rule that reads it
    #     is being handed the answer. Measured, never shippable on this corpus.
    spans = list(c.execute(
        "SELECT ts.user_id, ts.t_start_ns, COALESCE(ts.t_end_ns, ts.t_start_ns), d.speaker_id "
        "FROM truth_speaking ts JOIN discord_users d ON d.user_id = ts.user_id "
        "WHERE d.speaker_id IS NOT NULL"
    ))
    reach5 = int(5 * 60 * 1e9)

    def discord_present(r):
        seen = {sp for _u, a, b, sp in spans if a - reach5 <= r.t_end and b + reach5 >= r.t}
        return seen or None

    # (c) source-native: voices ever heard on this segment's source. The
    #     shipping `identity_prior`'s soft rule, as a hard restriction.
    native = defaultdict(set)
    for src, sp in c.execute(
        "SELECT s.source_id, g.speaker_id FROM segments g JOIN sessions s ON s.id=g.session_id "
        "WHERE g.speaker_id IS NOT NULL AND g.deleted_at IS NULL GROUP BY 1,2"
    ):
        native[src].add(sp)

    def source_native(r):
        return native.get(r.source_id) or None

    # The window is a hyperparameter, so it is chosen on an inner split of the
    # fit split. Reading the held-out table and keeping whichever window won
    # there is the same leak this protocol exists to stop, wearing a hat.
    inner_cut = L.split_at([r.t for r in fit])
    inner_fit, inner_ev = fit[:inner_cut], fit[inner_cut:]
    print(f"\n  the window, chosen on an inner split ({len(inner_fit)} / {len(inner_ev)}):")
    best = None
    for mins in (1, 2, 5, 10, 20, 60):
        f = recent(mins)
        pvi = L.thresholds_from_fit(
            L.fit_thresholds(L.obs_for_fit(inner_fit, bank, candidates_for=f)))
        s, _ = L.judge(inner_ev, bank, pvi, you=you, candidates_for=f)
        print(f"    +/-{mins:<3} min  correct={s.correct} wrong={s.wrong} "
              f"declined={s.declined}  P {s.precision * 100:.1f}%  F-0.5 {s.f_beta():.3f}")
        if best is None or s.f_beta() > best[0]:
            best = (s.f_beta(), mins)
    chosen = best[1]
    print(f"    chosen window: +/-{chosen} min")

    print()
    print(L.HEADER)
    print(base.row("no prior (ships)"))
    for name, f in [
        (f"R1 same session, +/-{chosen} min (chosen)", recent(chosen)),
        ("R2 same session, +/-2 min (own labels)", recent(2)),
        ("R3 same session, +/-60 min (own labels)", recent(60)),
        ("R4 source-native voices only", source_native),
    ]:
        b_pv = L.thresholds_from_fit(
            L.fit_thresholds(L.obs_for_fit(fit, bank, candidates_for=f))
        )
        s, _ = L.judge(ev, bank, b_pv, you=you, candidates_for=f)
        print(s.row(name))
        report(base, s, f" [{name}]")
    print()
    print("  CIRCULAR (reads the table the verdict is computed from — never shippable):")
    b_pv = L.thresholds_from_fit(
        L.fit_thresholds(L.obs_for_fit(fit, bank, candidates_for=discord_present))
    )
    s, _ = L.judge(ev, bank, b_pv, you=you, candidates_for=discord_present)
    print(s.row("R5 Discord spoke within +/-5 min"))

    print("\n  does the prior still add anything once scoring is top-3 mean?")
    print(L.HEADER)
    pv3 = L.thresholds_from_fit(L.fit_thresholds(L.obs_for_fit(fit, bank, agg="topk", topk=3)))
    b3, _ = L.judge(ev, bank, pv3, you=you, agg="topk", topk=3)
    print(b3.row("top-3 mean, no prior"))
    f = recent(chosen)
    pv3p = L.thresholds_from_fit(
        L.fit_thresholds(L.obs_for_fit(fit, bank, agg="topk", topk=3, candidates_for=f)))
    s, _ = L.judge(ev, bank, pv3p, you=you, agg="topk", topk=3, candidates_for=f)
    print(s.row(f"top-3 mean + prior (+/-{chosen} min)"))
    report(b3, s, " [prior on top of top-3]")


# ---- short turns ------------------------------------------------------------

BUCKETS = [("< 1.5 s", 0.0, 1.5), ("1.5 - 3 s", 1.5, 3.0), ("3 s +", 3.0, 1e9)]


def short(c, rows, protos, you, nm, cut, extra=()):
    fit, ev = rows[:cut], rows[cut:]
    bank = L.Bank(protos)
    pv = refit(fit, bank)
    print("\n=== 5. short-turn recognition ===")
    print(L.HEADER)
    for name, lo, hi in BUCKETS:
        sub = [r for r in ev if lo <= r.dur < hi]
        s, _ = L.judge(sub, bank, pv, you=you)
        print(s.row(f"shipping, {name}"))
    print()
    print("  the wrong labels by bucket, and the declines:")
    for name, lo, hi in BUCKETS:
        sub = [r for r in ev if lo <= r.dur < hi]
        s, w = L.judge(sub, bank, pv, you=you)
        print(f"    {name:<12} n={s.n:<4} wrong={s.wrong:<3} declined={s.declined:<3} "
              f"share of all wrong={s.wrong}/{sum(1 for _ in ev)}")


def main():
    what = sys.argv[1] if len(sys.argv) > 1 else "all"
    c, rows, protos, you, nm, cut = setup()
    print(f"rows {len(rows)}  fit {cut}  held out {len(rows) - cut}  own={you}")
    if what in ("hygiene", "all"):
        hygiene(c, rows, protos, you, nm, cut)
    if what in ("scoring", "all"):
        scoring(c, rows, protos, you, nm, cut)
    if what in ("prior", "all"):
        prior(c, rows, protos, you, nm, cut)
    if what in ("short", "all"):
        short(c, rows, protos, you, nm, cut)


if __name__ == "__main__":
    main()
