"""The mint path, replayed. Loaders and the forward simulation for §46.

§32, §36 and §44 all replayed the *label* decision and treated a decline as a
free non-answer. The live path does not: `analysis` turns a decline into a
`Mint`, and a mint enrols the turn as the new voice's first prototype. On the
evening of 2026-09-04 that difference produced twenty phantom voices in
thirty-three minutes, so this round replays the whole ladder — rank, decide,
mint, enrol, cap, evict — forward in time over the real segments.

Read-only, against a COPY of the database. Never the live one.
"""

import sys

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import recog_lib as R  # noqa: E402

DB = "/tmp/nx-recall-workspace/nx-scratch/agents-2026-09-04/mintburst/work.db"

# The live box's `[identity]`, not the crate defaults.
MAX_OVERLAP = 0.06
MIN_DURATION_S = 1.0
LABEL_THRESHOLD = 0.35
ENROL_SCORE = 0.55
ENROL_MARGIN = 0.06
ENROL_MAX_OVERLAP = 0.05
ENROL_MIN_DUR = 3.0
MINT_MIN_DUR = 2.0
MINT_MIN_WORDS = 2
CAP = 20
GLOBAL = (LABEL_THRESHOLD, 0.0)

MIN_NS = 60_000_000_000


def word_count(text):
    """`lang::word_count` — `asr::normalise_words` then a length."""
    if not text:
        return 0
    n = 0
    for w in text.split():
        if any(ch.isalnum() or ch == "'" for ch in w):
            n += 1
    return n


class Seg:
    __slots__ = (
        "id", "t", "t_end", "overlap", "dur", "words", "vec",
        "kind", "truth", "user", "coverage", "verdict", "speaker_now",
    )


SEG_QUERY = """
  SELECT g.id, g.t_start_ns, g.t_end_ns, COALESCE(g.overlap_frac, 0.0),
         g.text, e.vector, e.embed_model_id, so.kind,
         d.speaker_id, g.truth_user_id, COALESCE(g.truth_coverage, 0.0),
         g.truth_verdict, g.speaker_id
    FROM segments g
    JOIN sessions ss ON ss.id = g.session_id
    JOIN sources so ON so.id = ss.source_id
    JOIN embeddings e ON e.id = (
         SELECT MAX(x.id) FROM embeddings x WHERE x.segment_id = g.id)
    LEFT JOIN discord_users d ON d.user_id = g.truth_user_id
   WHERE g.deleted_at IS NULL
     AND g.t_start_ns >= ?
   ORDER BY g.t_start_ns ASC, g.id ASC
"""


def load_segments(c, since_ns, model=R.MODEL):
    """Every turn the daemon analysed from `since_ns`, in the order it saw them.

    Not only the truth rows: a phantom is minted from whatever turn happened to
    be under the bar, and the bank the *next* turn is scored against contains
    everything that came before it.
    """
    out = []
    for r in c.execute(SEG_QUERY, (since_ns,)):
        if r[6] != model:
            continue
        s = Seg()
        s.id, s.t, s.t_end, s.overlap = r[0], r[1], r[2], float(r[3])
        s.dur = (r[2] - r[1]) / 1e9
        s.words = word_count(r[4])
        s.vec = R.blob_to_vec(r[5])
        s.kind = r[7]
        s.truth = r[8]
        s.user, s.coverage, s.verdict = r[9], float(r[10]), r[11]
        s.speaker_now = r[12]
        out.append(s)
    return out


def deleted_prototypes(c):
    """The prototypes `identity repair --prototypes` removed, from the audit trail.

    A prototype's vector *is* its source segment's embedding — `add_prototype`
    stores the same vector the ladder scored — so a deleted prototype can be
    put back for a replay without keeping a second copy of the bank around.
    """
    import json

    out = []
    for (blob,) in c.execute(
        "SELECT prior_state FROM operations WHERE op = 'identity.repair' ORDER BY at_utc_ns"
    ):
        d = json.loads(blob)
        for row in d.get("condemned", []):
            out.append(row)
    return out


def bank_at(c, t_ns, model=R.MODEL):
    """The voicebank as it stood at `t_ns`, as best the archive can say.

    Surviving prototypes created before the instant, plus the ones tonight's
    repair deleted whose source turn was already over by then. Prototypes that
    existed then and were *evicted* since cannot be recovered — the eviction
    keeps no copy — so this is a lower bound on the bank, and the replay's
    validation against what actually happened is what says whether that matters.
    """
    live = {
        r[0]: dict(id=r[0], speaker=r[1], src=r[2], vec=R.blob_to_vec(r[3]),
                   golden=r[4], created=r[5])
        for r in c.execute(
            "SELECT p.id, p.speaker_id, p.source_segment_id, p.vector, p.is_golden, "
            "p.created_at FROM speaker_prototypes p WHERE p.embed_model_id = ?", (model,)
        )
    }
    out = [dict(p) for p in live.values() if (p["created"] or 0) < t_ns]
    have = {p["id"] for p in out}
    for row in deleted_prototypes(c):
        pid, seg = row["prototype"], row["segment"]
        if pid in have or pid in live:
            continue
        e = c.execute(
            "SELECT e.vector, g.t_end_ns FROM segments g JOIN embeddings e "
            "ON e.id = (SELECT MAX(x.id) FROM embeddings x WHERE x.segment_id = g.id) "
            "WHERE g.id = ?", (seg,)
        ).fetchone()
        if not e or e[1] >= t_ns:
            continue
        out.append(dict(id=pid, speaker=row["owner"], src=seg,
                        vec=R.blob_to_vec(e[0]), golden=0, created=e[1]))
        have.add(pid)
    return out


def installed_thresholds(c):
    return {
        sid: (float(t), float(m))
        for sid, t, m in c.execute(
            "SELECT id, label_threshold, COALESCE(label_margin, 0.0) FROM speakers "
            "WHERE merged_into IS NULL AND label_threshold IS NOT NULL"
        )
    }


def normed(v):
    n = np.linalg.norm(v)
    return v / n if n > 0 else v


# ---- the rules under test ---------------------------------------------------


class Rules:
    """Every proposed brake, off by default. `Rules()` is what shipped."""

    def __init__(self, near_miss=None, near_miss_labels=False, bar_ceiling=None,
                 fitted_never_mints=False, min_prototypes=0):
        # (a) never mint when the best voice missed its own bar by `near_miss`
        #     or less, on either leg.
        self.near_miss = near_miss
        self.near_miss_labels = near_miss_labels
        # (b, at the ladder) a *fitted* bar declines, it never mints: a turn
        #     that clears the global operating point and fails only the voice's
        #     own learned bar is not a stranger.
        self.fitted_never_mints = fitted_never_mints
        # (b, at calibrate) no per-voice bar above this.
        self.bar_ceiling = bar_ceiling
        # (c) a voice with fewer than N prototypes and no name cannot win a label.
        self.min_prototypes = min_prototypes

    def bar_for(self, sp, thresholds):
        t, m = thresholds.get(sp, GLOBAL)
        if self.bar_ceiling is not None:
            t = min(t, self.bar_ceiling)
        return t, m


class Sim:
    """One forward run of the live path.

    `analysis::commit` in Python: gate, rank, decide, then mint-and-seed or
    label-and-maybe-enrol, with `add_prototype`'s cap and
    `identity::prototype_to_evict`'s nearest-first eviction.
    """

    def __init__(self, base, thresholds, names, you, rules=None, agg="max", topk=3,
                 next_speaker_id=1000):
        self.protos = [dict(p) for p in base]
        self.thresholds = dict(thresholds)
        self.names = dict(names)
        self.you = you
        self.rules = rules or Rules()
        self.agg, self.topk = agg, topk
        self.next_id = next_speaker_id
        self.next_proto = -1
        self.minted = {}          # new speaker id -> seed segment id
        self.mint_events = []     # the reconstruction table
        self.labels = {}          # segment id -> speaker id or None
        self.near_missed = 0

    # -- the bank -----------------------------------------------------------

    def _rank(self, seg):
        bank = [p for p in self.protos if p["src"] != seg.id]
        if self.rules.min_prototypes > 1:
            count = {}
            for p in self.protos:
                count[p["speaker"]] = count.get(p["speaker"], 0) + 1
            bank = [
                p for p in bank
                if count.get(p["speaker"], 0) >= self.rules.min_prototypes
                or not self._is_unnamed(p["speaker"])
            ]
        if not bank:
            return []
        m = np.stack([normed(p["vec"]) for p in bank])
        cos = m @ normed(seg.vec)
        out = {}
        for p, s in zip(bank, cos):
            out.setdefault(p["speaker"], []).append(float(s))
        ranked = []
        for sp, ss in out.items():
            if self.agg == "max":
                ranked.append((sp, max(ss)))
            else:
                k = min(self.topk, len(ss))
                ranked.append((sp, float(np.mean(sorted(ss)[-k:]))))
        ranked.sort(key=lambda t: (-t[1], t[0]))
        return ranked

    def _is_unnamed(self, sp):
        n = self.names.get(sp, "")
        return sp in self.minted or n.startswith("Speaker_")

    def _add_prototype(self, speaker, seg):
        mine = [p for p in self.protos if p["speaker"] == speaker]
        if len(mine) >= CAP:
            free = [p for p in mine if not p["golden"]]
            if not free:
                return
            v = normed(seg.vec)
            victim = max(free, key=lambda p: float(v @ normed(p["vec"])))
            self.protos = [p for p in self.protos if p is not victim]
        self.protos.append(dict(id=self.next_proto, speaker=speaker, src=seg.id,
                                vec=seg.vec, golden=0, created=seg.t_end))
        self.next_proto -= 1

    # -- one turn -----------------------------------------------------------

    def step(self, seg):
        # The microphone leg: provenance, not a match (`commit_pinned`).
        if seg.kind == "mic":
            self.labels[seg.id] = self.you
            if (self.you is not None and seg.overlap <= ENROL_MAX_OVERLAP
                    and seg.dur >= ENROL_MIN_DUR):
                self._add_prototype(self.you, seg)
            return
        if seg.overlap > MAX_OVERLAP or seg.dur < MIN_DURATION_S:
            self.labels[seg.id] = None
            return
        ranked = self._rank(seg)
        mints = seg.dur >= MINT_MIN_DUR and seg.words >= MINT_MIN_WORDS
        if not ranked:
            self.labels[seg.id] = None
            if mints:
                self._mint(seg, None, None, None, None, "empty bank")
            return
        top_sp, top_score = ranked[0]
        runner = ranked[1][1] if len(ranked) > 1 else float("-inf")
        margin = top_score - runner
        bar, bar_margin = self.rules.bar_for(top_sp, self.thresholds)
        if top_score < bar or margin < bar_margin:
            near = (self.rules.near_miss is not None
                    and top_score >= bar - self.rules.near_miss
                    and margin >= bar_margin - self.rules.near_miss)
            if self.rules.fitted_never_mints and top_sp in self.thresholds:
                # The turn cleared the operating point every voice without a
                # fitted bar answers to. Whatever it is, it is not a stranger.
                g_t, g_m = GLOBAL
                if top_score >= g_t and margin >= g_m:
                    near = True
            if near:
                self.near_missed += 1
                if self.rules.near_miss_labels:
                    self.labels[seg.id] = top_sp
                    return
                self.labels[seg.id] = None
                return
            self.labels[seg.id] = None
            if mints:
                self._mint(seg, top_sp, top_score, bar, margin, "under the bar")
            return
        self.labels[seg.id] = top_sp
        if (top_score >= ENROL_SCORE and margin >= ENROL_MARGIN
                and seg.overlap <= ENROL_MAX_OVERLAP and seg.dur >= ENROL_MIN_DUR):
            self._add_prototype(top_sp, seg)

    def _mint(self, seg, top_sp, top_score, bar, margin, why):
        sid = self.next_id
        self.next_id += 1
        self.minted[sid] = seg.id
        self.names[sid] = f"Speaker_{sid}"
        self.labels[seg.id] = sid
        self._add_prototype(sid, seg)
        self.mint_events.append(dict(
            speaker=sid, segment=seg.id, t=seg.t, dur=seg.dur, words=seg.words,
            best=top_sp, best_name=self.names.get(top_sp, "-"), score=top_score,
            bar=bar, margin=margin, truth=seg.truth, coverage=seg.coverage,
            verdict=seg.verdict, why=why,
        ))

    def run(self, segs):
        for s in segs:
            self.step(s)
        return self
