"""Is the bank's eviction policy right? Held-out, under §32's protocol.

Question 2 of the drift round. The shipping policy is: cap the bank at 20
prototypes per voice and, when it is full, evict the one the incoming vector is
**nearest** to (`identity::prototype_to_evict`). §36 found that rule is also the
bank's only structural defence against a mis-linked Discord account, so every
candidate here is measured twice: on accuracy, and under §36's one-wrong-link
poisoning simulation.

Arms: nearest (ships), oldest, a mixed policy (k oldest kept as anchors,
recency for the rest), per-source sub-banks, and caps of 20 / 30 / 40.

Protocol, identical to §32 and §36: chronological 60/40 split of the truth rows,
no row scored against a prototype it produced, the user's own Discord account
excluded, the operating point read off the live box rather than the defaults.

    python3 spike/policy_bench.py [path/to/copy.db]

Read-only, against a COPY of the database. Never the live one.
"""

import sys
from collections import Counter, defaultdict

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import drift_lib as D  # noqa: E402
import recog_lib as R  # noqa: E402

MAX_OVERLAP = 0.06  # config.toml
GLOBAL = (0.35, 0.0)
ENROL_SCORE = 0.55
ENROL_MARGIN = 0.06
ENROL_MAX_OVERLAP = 0.05
ENROL_MIN_DUR = 3.0
CAP = 20
ANCHORS = 5  # the mixed policy's k

R.MAX_OVERLAP = MAX_OVERLAP


def victim(vec, mine, how, anchors=ANCHORS):
    """Which prototype goes. `nearest` is `identity::prototype_to_evict`."""
    cand = [p for p in mine if not p["golden"]]
    if not cand or how == "none":
        return None
    if how == "nearest":
        v = D.normed(vec)
        return max(cand, key=lambda p: float(v @ D.normed(p["vec"])))
    if how == "oldest":
        return min(cand, key=lambda p: ((p["created"] or 0), p["id"]))
    if how == "mixed":
        # Keep the k oldest as anchors — the voice as first heard — and run
        # recency on everything above them.
        rest = sorted(cand, key=lambda p: ((p["created"] or 0), p["id"]))[anchors:]
        return min(rest, key=lambda p: ((p["created"] or 0), p["id"])) if rest else None
    if how == "nearest_same_source":
        # Sub-bank hygiene: spend the slot within the incoming vector's own
        # source kind, so a mic prototype never evicts an app one.
        raise AssertionError("handled by the caller")
    raise ValueError(how)


def trim(protos, cap, how):
    """Bring an over-cap voice back to the cap.

    `add_prototype` enforces the cap, but `merge_speaker` moves prototypes with
    a bare UPDATE and nothing re-applies it afterwards: on this install Rowan
    holds 47 against a cap of 20, all the excess inherited from twenty minted
    voices that were merged in. Two ways to spend the excess:

      * `redundant` — the shipping eviction rule with no incoming vector: drop
        the prototype nearest to one of its own siblings, repeatedly.
      * `outlier` — drop the prototype least like the rest of its own voice.
        §32 warns that a voice's prototypes are *supposed* to span its range,
        so this one has to earn its place against `redundant` rather than
        being assumed better.
    """
    if how == "none" or not cap:
        return list(protos)
    out = []
    by = defaultdict(list)
    for p in protos:
        by[p["speaker"]].append(p)
    for voice, mine in by.items():
        keep = list(mine)
        while len(keep) > cap:
            free = [p for p in keep if not p["golden"]]
            if not free:
                break
            m = np.stack([D.normed(p["vec"]) for p in keep])
            k = m @ m.T
            np.fill_diagonal(k, -np.inf)
            idx = {id(p): i for i, p in enumerate(keep)}
            if how == "redundant":
                victim_p = max(free, key=lambda p: float(k[idx[id(p)]].max()))
            elif how == "outlier":
                kk = m @ m.T
                np.fill_diagonal(kk, np.nan)
                victim_p = min(
                    free, key=lambda p: float(np.nanmean(kk[idx[id(p)]]))
                )
            else:
                raise ValueError(how)
            keep = [p for p in keep if p is not victim_p]
        out.extend(keep)
    return out


def replay(base, fit_rows, *, cap, evict, agg, topk, you, truth_of=None,
           sub_source=False, anchors=ANCHORS, forced=None, skip_you=True):
    """The LIVE enrolment path over the fit split, in time order.

    `analysis` enrols on every `Matched { enroll: true }` — the ladder's own
    label, the global enrol bar, no ground truth involved — so this is what the
    bank actually does to itself. `truth_of` overrides the label for the
    poisoning simulation, which is the only place Discord's word enters.

    `forced` names rows whose label is not the ladder's to choose: a microphone
    turn is the user by construction (§17), and the daemon that gets it wrong
    mints a new voice rather than filing it under somebody else. Replaying a
    mic turn through the ladder with no mint path would hand it to the nearest
    voice over 0.55 and the simulation would run away — so those rows carry
    their label, and the arm says so.
    """
    protos = [dict(p) for p in base]
    added, evicted, refused = Counter(), 0, 0
    for r in sorted(fit_rows, key=lambda x: (x.t, x.id)):
        if r.dur < ENROL_MIN_DUR or r.overlap > ENROL_MAX_OVERLAP:
            continue
        force = None if forced is None else forced.get(id(r))
        bank = R.Bank(protos)
        ranked = R.rank(bank, r.vec, drop_segment=r.id, agg=agg, topk=topk)
        if not ranked:
            continue
        top_sp, top_score = ranked[0]
        runner = ranked[1][1] if len(ranked) > 1 else -np.inf
        if force is None:
            if top_score < ENROL_SCORE or (top_score - runner) < ENROL_MARGIN:
                continue
            claim = top_sp if truth_of is None else truth_of(r, top_sp)
        else:
            own = [s for sp, s in ranked if sp == force]
            if not own or own[0] < ENROL_SCORE:
                continue
            claim = force
        if claim is None or (skip_you and claim == you):
            continue
        mine = [p for p in protos if p["speaker"] == claim]
        pool = mine
        if sub_source:
            same = [p for p in mine if p["kind"] == r.kind]
            # A voice is capped per source kind, and the slot is spent inside
            # the kind the new audio came from.
            pool = same
            mine = same
        if cap and len(mine) >= cap:
            v = victim(r.vec, pool, evict, anchors)
            if v is None:
                refused += 1
                continue
            protos = [p for p in protos if p is not v]
            evicted += 1
        protos.append(
            dict(id=-r.id, speaker=claim, src=r.id, vec=r.vec, golden=0,
                 created=r.t, kind=r.kind, app=r.app)
        )
        added[claim] += 1
    return protos, added, evicted, refused


def judge(rows, bank, thr, you, agg, topk, key=None):
    s, wrong = R.Score(), []
    for r in rows:
        truth = r.truth if key is None else key(r)
        if truth == you:
            continue
        ranked = R.rank(bank, r.vec, drop_segment=r.id, agg=agg, topk=topk)
        label = R.decide(ranked, r.overlap, r.dur, thr, GLOBAL)
        s.add(label, truth)
        if label is not None and label != truth:
            wrong.append((r.id, label, truth))
    return s, wrong


POLICIES = [
    ("nearest, cap 20  (ships)", 20, "nearest", False, "none"),
    ("oldest,  cap 20", 20, "oldest", False, "none"),
    ("mixed:   5 oldest anchors + recency, cap 20", 20, "mixed", False, "none"),
    ("nearest, cap 20, per-source sub-banks", 20, "nearest", True, "none"),
    ("nearest, cap 30", 30, "nearest", False, "none"),
    ("nearest, cap 40", 40, "nearest", False, "none"),
    ("oldest,  cap 40", 40, "oldest", False, "none"),
    ("no cap at all", 0, "nearest", False, "none"),
    ("nearest, cap 20, cap re-applied after merge (redundant)", 20, "nearest", False,
     "redundant"),
    ("nearest, cap 20, cap re-applied after merge (outlier)", 20, "nearest", False,
     "outlier"),
]


def suite(title, base, fit, held, mic_fit, mic_held, thr, you, nm, agg, topk, swaps):
    print(f"\n{'=' * 104}\n{title}   (aggregate {agg}-{topk if agg == 'topk' else ''})\n{'=' * 104}")
    counts = Counter(p["speaker"] for p in base)
    print(f"  starting bank {len(base)}: "
          + str({nm.get(k, k): v for k, v in sorted(counts.items(), key=lambda kv: -kv[1])[:8]}))
    kinds = defaultdict(set)
    for p in base:
        kinds[p["speaker"]].add(p["kind"])
    multi = [nm.get(k, k) for k, v in kinds.items() if len(v) > 1]
    print(f"  voices whose prototypes come from more than one source kind: {multi or 'none'}")

    arms = []

    def run(name, protos, added=None, ev=0, refused=0):
        s, wrong = judge(held, R.Bank(protos), thr, you, agg, topk)
        arms.append([name, s, None, added or Counter(), ev, refused, wrong, len(protos)])
        return s

    base_score = run("(a) the bank as of the split, no enrolment", base)
    for name, cap, ev, sub, tr in POLICIES:
        start = trim(base, cap, tr)
        protos, added, evicted, refused = replay(
            start, fit, cap=cap, evict=ev, agg=agg, topk=topk, you=you, sub_source=sub
        )
        run(f"({name})", protos, added, evicted, refused)

    print("\n  Discord voices: the live path enrols over the fit split, "
          f"held out on {len(held)} Discord truth rows")
    print(R.HEADER + "   bank   added / evicted")
    for name, s, _m, added, ev, refused, _w, n in arms:
        extra = f"{n:>7}"
        if added:
            extra += "   +" + ",".join(f"{nm.get(k, k)[:6]}:{v}" for k, v in added.most_common(4))
            extra += f" / {ev}" + (f" ({refused} refused)" if refused else "")
        print(s.row(name) + extra)

    # ---- the user's own voice, on its own -----------------------------------
    # Its truth is the microphone, not Discord, and a mic turn the ladder gets
    # wrong is minted rather than misfiled — so its bank is grown from mic rows
    # carrying their own label, and judged on held-out mic rows.
    print(f"\n  the user's own voice: mic turns enrol under `You` over the fit split, "
          f"held out on {len(mic_held)} mic turns")
    print(R.HEADER + "   You's bank / evicted")
    mb, _w = judge(mic_held, R.Bank(base), thr, None, agg, topk)
    print(mb.row("(a) the bank as of the split, no enrolment")
          + f"{sum(1 for p in base if p['speaker'] == you):>10}")
    for name, cap, ev, sub, tr in POLICIES:
        protos, added, evicted, refused = replay(
            trim(base, cap, tr), mic_fit, cap=cap, evict=ev, agg=agg, topk=topk, you=you,
            sub_source=sub, forced={id(r): you for r in mic_fit}, skip_you=False,
        )
        m, _w = judge(mic_held, R.Bank(protos), thr, None, agg, topk)
        print(m.row(f"({name})")
              + f"{sum(1 for p in protos if p['speaker'] == you):>10} / {evicted}")

    print("\n  gates against (a), the bar stated first: precision must not drop, "
          "F-0.5 +0.005, material")
    for name, s, _m, *_ in arms[1:]:
        print(f"    {name:<48} {R.verdict(base_score, s)}  "
              f"safe={R.swap_is_safe(base_score, s)!s:<5} "
              f"material={R.improvement_is_material(base_score, s)!s:<5} "
              f"dF={s.f_beta() - base_score.f_beta():+.4f} "
              f"dP={(s.precision - base_score.precision) * 100:+.2f}pp "
              f"dR={(s.recall - base_score.recall) * 100:+.2f}pp")

    print("\n  gates against the shipping policy (nearest, cap 20) — the comparison "
          "that decides a swap")
    ship = arms[1][1]
    for name, s, _m, *_ in arms[2:]:
        print(f"    {name:<48} {R.verdict(ship, s)}  "
              f"safe={R.swap_is_safe(ship, s)!s:<5} "
              f"material={R.improvement_is_material(ship, s)!s:<5} "
              f"dF={s.f_beta() - ship.f_beta():+.4f} "
              f"dP={(s.precision - ship.precision) * 100:+.2f}pp "
              f"dR={(s.recall - ship.recall) * 100:+.2f}pp")

    # ---- §36's poisoning simulation, per policy ----------------------------
    # A mis-linked Discord account writing into the bank. The live path enrols
    # on the ladder's own label, so the simulation is truth-as-enroller: the
    # arm §36 measured, run once per eviction policy.
    print("\n  §36's one-wrong-link simulation, per policy "
          "(truth enrols, ladder agreement NOT required — the unprotected case)")
    print(f"    {'policy':<46}{'foreign prototypes':>20}{'precision':>12}{'recall':>9}{'F-0.5':>8}")
    for tag, swap in swaps:
        print(f"    --- {tag} ---")
        for name, cap, ev, sub, tr in POLICIES:
            def claimed(r, top, swap=swap):
                return swap.get(r.user, r.truth) if r.user is not None else None

            protos, _a, _e, _r = replay(
                trim(base, cap, tr), fit, cap=cap, evict=ev, agg=agg, topk=topk, you=you,
                truth_of=claimed, sub_source=sub,
            )
            real = {r.id: r.truth for r in fit}
            poison = sum(
                1 for p in protos
                if p["id"] < 0 and real.get(p["src"], p["speaker"]) != p["speaker"]
            )
            s, _w = judge(held, R.Bank(protos), thr, you, agg, topk)
            print(f"    {name:<46}{poison:>20}{s.precision * 100:>11.1f}%"
                  f"{s.recall * 100:>8.1f}%{s.f_beta():>8.3f}")
    return arms


def main(db=D.DB):
    c = R.conn(db)
    you = R.you_speaker_id(c)
    nm = R.names(c)
    protos = D.load_bank(c)
    rows = D.load_truth_rows(c)
    mic = D.load_mic_rows(c, you)
    settings = dict(c.execute("SELECT key, value FROM settings"))
    installed = settings.get("identity_aggregate", "max")
    thr = {
        sid: (float(t), float(m))
        for sid, t, m in c.execute(
            "SELECT id, label_threshold, COALESCE(label_margin, 0.0) FROM speakers "
            "WHERE merged_into IS NULL AND label_threshold IS NOT NULL"
        )
    }
    cut = R.split_at([r.t for r in rows])
    fit, held = rows[:cut], rows[cut:]
    split_t = rows[cut].t
    base = [p for p in protos if (p["created"] or 0) < split_t]
    mic_fit = [r for r in mic if r.t < split_t]
    mic_held = [r for r in mic if r.t >= split_t]
    print(f"rows {len(rows)}  fit {len(fit)}  held {len(held)}  you={you}")
    print(f"mic rows {len(mic)}  fit {len(mic_fit)}  held {len(mic_held)}")
    print(f"installed aggregate {installed!r}, thresholds {thr or 'none (the globals)'}, "
          f"max_overlap {MAX_OVERLAP}, cap {CAP}")
    print(f"bank today {len(protos)}; as of the split {len(base)}")

    users = {}
    for r in rows:
        if r.user is not None:
            users.setdefault(r.user, [r.truth, 0])[1] += 1
    two = sorted(users, key=lambda u: -users[u][1])[:2]
    swaps = [
        ("one link wrong", {two[0]: users[two[1]][0]}),
        ("both links wrong", {two[0]: users[two[1]][0], two[1]: users[two[0]][0]}),
    ]

    for agg, topk, tag in (("max", 1, "as installed"), ("topk", 3, "§32's top-3 mean")):
        suite(f"Eviction policies, {tag}", base, fit, held, mic_fit, mic_held,
              thr, you, nm, agg, topk, swaps)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else D.DB)
