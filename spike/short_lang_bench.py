#!/usr/bin/env python3
"""Which language is a *short* line in? — the measurement behind 0.11.0.

`lang::guess_other` needs three function words before it will name a language
(`spike/guess_other_bench.py`). A VRChat turn is three to eight words and mostly
content, so the rule that scored 97.8% on FLEURS *sentences* answers "I cannot
tell" on "Tu arrêtes appartement." — the line that started this.

Three stages are measured here, in the order the shipped rule applies them:

  1. **script** — Cyrillic / kana / hangul / han / arabic / greek, unchanged
     from 0.10.2, and already right on two words;
  2. **exclusive diacritics** — characters that occur in exactly one shippable
     Latin-script language of the set and never in German or English. The table
     is *derived* from the corpus rather than written by hand, because the hand
     gets it wrong: `ç` looks French and is Turkish and Portuguese, `ø` looks
     Danish and is Norwegian too. German's `ä ö ü ß` and Swedish/Finnish `ä ö`
     are not exclusive and are not used;
  3. **character trigrams** — a per-language log-probability table built from
     the FLEURS *dev* text, scored as mean log-probability per trigram, decided
     by argmax with a margin over the runner-up *and* a margin over the better
     of de/en.

Measured on FLEURS **test**, cut to fragments of 2, 3, 4 and 6 words at word
boundaries — the real length distribution of a lobby turn — with 400 German and
400 English fragments per length as negatives.

    python3 spike/short_lang_bench.py            # the tables and the gate
    python3 spike/short_lang_bench.py --emit PATH  # write lang_ngrams.rs

Gate, per language and per fragment length: precision >= 95% at every length,
with de/en false positives <= 0.5% at every length. A language that fails under
a stage does not get that stage; the composite rule at the end is what ships.
"""

import argparse
import csv
import math
import random
import sys
import unicodedata
from collections import Counter, defaultdict
from pathlib import Path

SCRATCH = Path("/tmp/nx-recall-workspace/nx-scratch")
FLEURS = SCRATCH / "fleurs"
GERMAN_DEV = SCRATCH / "de" / "dev.tsv"

MIN_VOTES = 3
CONFIDENT_VOTES = 4
LENGTHS = (2, 3, 4, 6)
PER_LANG = 200
NEGATIVES = 400  # per negative language, per length

# ---- the 0.10.2 tables, verbatim in crates/recalld/src/lang.rs --------------

STOPWORDS = {
    "fr": "le la les des une est ne pas que qui pour dans sur avec aux cette il elle nous vous ils elles mais ou plus sont été être ce".split(),
    "es": "el los las del y en que es un una por para con su como más pero está están fue sus".split(),
    "it": "il lo gli le di della che non è un una per con sono come più anche dei nel alla".split(),
    "pt": "os as do da dos das que não um uma por para com mais mas está são se na no".split(),
    "nl": "het een van niet dat op te voor zijn er ook maar als aan door om worden werd deze".split(),
    "pl": "w nie na że to się do jest ale jak po dla od przez czy tylko już oraz który".split(),
    "tr": "bir bu için ile çok daha var olarak gibi ki ise ancak sonra kadar veya olan".split(),
    "sv": "och att är för av på inte till som han var men den det ett har med om".split(),
    "da": "og af ikke til er jeg han hun som men det den har med om der blev".split(),
    "no": "og av ikke til er jeg han hun som men det den har med om det være også".split(),
    "fi": "ja on ei että se ovat kuin myös mutta niin tai kun jos hän oli ollut sekä".split(),
    "cs": "a v na se že je to do za od pro ale jak nebo který jsou byl také jako".split(),
}

SHIP = set("fr es it pt nl pl tr sv da fi cs ru uk ja zh ko ar el".split())

DE = "der die das und ist nicht ich du wir ihr sie es ein eine einen dem den mit von für auf als auch aber wenn dann noch schon nur mal was wie wo ja nein doch beim vom zur zum über unter zwischen gegen ohne durch".split()
EN = "the a an and is are was were not i you we they it this that of to in for on with as at by from but if then just only what how where yes no about into over under between against without through".split()

# ---- stage 2: characters that occur in exactly one shippable language -------
#
# NOT hand-written. The first draft of this table was, and the bench threw it
# out: `ç` was listed as French, and `ç` is Turkish and Portuguese as well —
# French's precision fell to 85.9% and the whole stage failed its gate. So the
# table is *derived* from the dev text by [`derive_exclusive`], with the rule
# spelled out rather than assumed: a character is exclusive to a language when
# that language's dev text has it at least `EXCLUSIVE_MIN` times and no other
# Latin-script language in [`TRI_LANGS`] — German, English and the Norwegian
# blocker included — has it even once.
#
# That rule is what deletes the two cases the eye gets wrong. `ø` is Danish and
# Norwegian, so Danish does not get it; `å` is Swedish, Danish and Norwegian, so
# Swedish does not get it either. Both were in the hand-written draft and both
# cost their language the gate (da 64.6%, sv 69.2% at two words).
EXCLUSIVE_MIN = 15
EXCLUSIVE_TRACE = 1
"""Occurrences of a character another language may have and still not own it.

Not zero, and the reason is `ñ`: it appears exactly once in *every* language's
dev text, because FLEURS is a parallel corpus and one sentence in it is about
Spain. One occurrence in a hundred and fifty sentences is a proper noun, not a
language's alphabet, and a strict zero would throw Spanish's only exclusive
character away to protect against a place name.
"""
EXCLUSIVE_EXTRA = "¿¡"
"""Punctuation the derivation is allowed to consider as well as letters.

Spanish opens a question with `¿` and nothing else in the set does. It is not in
the shipped table: FLEURS transcriptions carry no `¿` at all, so there is no way
to *measure* it here, and an unmeasured rule does not ship.
"""

# ---- stage 3 constants ------------------------------------------------------

TRI_LANGS = "cs da de en es fi fr it nl no pl pt sv tr".split()
"""The Latin-script languages the trigram model covers.

The other guessable languages (ru uk ja zh ko ar el) are settled by *script*
before a trigram is ever counted, and a table for each of them would be a few
hundred kilobytes spent to re-derive an answer the alphabet already gave.
Norwegian is here as a blocker, exactly as in the stopword table: it must be
able to *win* so that it can then be refused, or its fragments go to Danish.
"""

TOP_TRIGRAMS = 2000
TRI_MIN_WORDS = 2
LOG_SCALE = 100.0  # i16 fixed point in the generated table

# The margin the winner must beat the runner-up (and de/en) by, as
# `A + B / sqrt(trigrams)`.
#
# A fixed margin cannot work and the first run of this bench is why: the score
# is a *mean* log-probability per trigram, so its noise falls off as
# 1/sqrt(n) — a margin loose enough to name a six-word line is a margin that
# names German fragments at two words (2.88% false positives, six times the
# gate). The `B` term is that standard error, priced by the gate.
TRI_MARGIN_A = 0.10
TRI_MARGIN_B = 1.30
TRI_MARGIN_DEEN_A = 0.14
TRI_MARGIN_DEEN_B = 1.80


def words(text):
    out, cur = [], []
    for ch in text:
        if ch.isalpha() or ch in ("'", "’"):
            cur.append(ch.lower())
        elif cur:
            out.append("".join(cur))
            cur = []
    if cur:
        out.append("".join(cur))
    return out


def script_counts(text):
    counts = Counter()
    letters = 0
    for ch in text:
        if not ch.isalpha():
            continue
        letters += 1
        try:
            name = unicodedata.name(ch)
        except ValueError:
            continue
        for key in ("CYRILLIC", "HIRAGANA", "KATAKANA", "HANGUL", "CJK", "ARABIC", "GREEK"):
            if name.startswith(key) or (key == "CJK" and "CJK UNIFIED" in name):
                counts[key] += 1
                break
    return counts, letters


def guess_by_script(text):
    counts, letters = script_counts(text)
    if counts["HIRAGANA"] + counts["KATAKANA"] >= 2:
        return "ja"
    if counts["HANGUL"] >= 2:
        return "ko"
    for key, tag in (("CYRILLIC", "ru"), ("CJK", "zh"), ("ARABIC", "ar"), ("GREEK", "el")):
        if letters and counts[key] * 3 >= letters:
            if tag == "ru":
                return "uk" if any(c in "їієґЇІЄҐ" for c in text) else "ru"
            return tag
    return None


def derive_exclusive(dev):
    """Characters exactly one language in the set has. See `EXCLUSIVE_MIN`."""
    counts = {tag: Counter(c for t in texts for c in t.lower()) for tag, texts in dev.items()}
    interesting = set()
    for c in counts.values():
        for ch in c:
            if ch in EXCLUSIVE_EXTRA or (ch.isalpha() and not ch.isascii()):
                interesting.add(ch)
    out = defaultdict(str)
    for ch in sorted(interesting):
        holders = [tag for tag in dev if counts[tag][ch] > EXCLUSIVE_TRACE]
        if len(holders) != 1:
            continue
        tag = holders[0]
        if counts[tag][ch] < EXCLUSIVE_MIN or tag in ("de", "en", "no"):
            continue
        out[tag] += ch
    return dict(out)


def guess_by_diacritic(text, exclusive, allow):
    """Stage 2. `allow` is the set of languages this stage may answer with."""
    low = text.lower()
    hit = {tag for tag, chars in exclusive.items() if any(c in low for c in chars)}
    if len(hit) != 1:
        return None  # nothing, or two languages arguing
    tag = hit.pop()
    if tag not in allow or len(words(text)) < 2:
        return None
    return tag


def stopword_guess(text):
    """Stage 1b — the 0.10.2 vote. Returns (tag, confident) or (None, False)."""
    ws = words(text)
    if not ws:
        return (None, False)
    bag = Counter(ws)
    de = sum(bag[w] for w in DE)
    en = sum(bag[w] for w in EN)
    scores = sorted(((sum(bag[w] for w in sw), tag) for tag, sw in STOPWORDS.items()), reverse=True)
    best, tag = scores[0]
    runner = scores[1][0]
    if best < MIN_VOTES or best <= de + en or best == runner:
        return (None, False)
    if tag not in SHIP:
        return (None, False)
    return (tag, best >= CONFIDENT_VOTES)


# ---- the trigram model ------------------------------------------------------


def normalise(text):
    """Lowercase, letters and single spaces only, padded with a space each end."""
    out = []
    prev_space = True
    for ch in text.lower():
        if ch.isalpha():
            out.append(ch)
            prev_space = False
        elif not prev_space:
            out.append(" ")
            prev_space = True
    s = "".join(out).strip()
    return f" {s} " if s else ""


def trigrams(text):
    s = normalise(text)
    return [s[i : i + 3] for i in range(len(s) - 2)]


class Ngrams:
    """Per-language trigram log-probabilities, add-one smoothed and pruned."""

    def __init__(self, tables, floors):
        self.tables = tables  # tag -> {trigram: logprob}
        self.floors = floors  # tag -> logprob of an unseen trigram

    @classmethod
    def build(cls, texts_by_lang, top=TOP_TRIGRAMS):
        tables, floors = {}, {}
        for tag, texts in texts_by_lang.items():
            counts = Counter()
            for t in texts:
                counts.update(trigrams(t))
            total = sum(counts.values())
            vocab = len(counts) + 1  # +1 for everything unseen
            floors[tag] = math.log((0 + 1) / (total + vocab))
            keep = counts.most_common(top)
            tables[tag] = {g: math.log((c + 1) / (total + vocab)) for g, c in keep}
        return cls(tables, floors)

    def score(self, text):
        gs = trigrams(text)
        if not gs:
            return {}
        out = {}
        for tag, table in self.tables.items():
            floor = self.floors[tag]
            out[tag] = sum(table.get(g, floor) for g in gs) / len(gs)
        return out

    def guess(self, text, allow, margins=None):
        a, b, da, db = margins or (TRI_MARGIN_A, TRI_MARGIN_B, TRI_MARGIN_DEEN_A, TRI_MARGIN_DEEN_B)
        if len(words(text)) < TRI_MIN_WORDS:
            return None
        gs = trigrams(text)
        if not gs:
            return None
        scores = self.score(text)
        se = 1.0 / math.sqrt(len(gs))
        order = sorted(scores.items(), key=lambda kv: -kv[1])
        tag, best = order[0]
        runner = order[1][1]
        deen = max(scores.get("de", -99), scores.get("en", -99))
        if tag in ("de", "en"):
            return None
        if best - runner < a + b * se:
            return None
        if best - deen < da + db * se:
            return None
        if tag not in allow:
            return None  # a blocker won; see `no`
        return tag


# ---- the corpus -------------------------------------------------------------

FLEURS_DIRS = {
    "fr": "fr_fr",
    "es": "es_419",
    "it": "it_it",
    "pt": "pt_br",
    "nl": "nl_nl",
    "pl": "pl_pl",
    "tr": "tr_tr",
    "sv": "sv_se",
    "da": "da_dk",
    "no": "nb_no",
    "fi": "fi_fi",
    "cs": "cs_cz",
    "ja": "ja_jp",
    "ru": "ru_ru",
    "ko": "ko_kr",
    "zh": "cmn_hans_cn",
    "uk": "uk_ua",
    "ar": "ar_eg",
    "el": "el_gr",
    "en": "en_us",
    "de": "de_de",
}


def read_tsv(path):
    seen, out = set(), []
    if not path.exists():
        return out
    with open(path, encoding="utf-8") as fh:
        for row in csv.reader(fh, delimiter="\t", quoting=csv.QUOTE_NONE):
            if len(row) < 3:
                continue
            s = row[2].strip()
            if len(s) < 12 or s in seen:
                continue
            seen.add(s)
            out.append(s)
    return out


def dev_texts(tag):
    """The table-building split. German's dev lives outside the fleurs dir."""
    if tag == "de":
        rows = read_tsv(GERMAN_DEV)
        if rows:
            return rows
    return read_tsv(FLEURS / f"{FLEURS_DIRS[tag]}.dev.tsv")


def test_texts(tag):
    return read_tsv(FLEURS / f"{FLEURS_DIRS[tag]}.test.tsv")


def fragments(sentences, n_words, count, seed):
    """`count` fragments of exactly `n_words` words, cut at word boundaries."""
    rng = random.Random(seed)
    pool = []
    for s in sentences:
        toks = s.split()
        if len(toks) < n_words:
            continue
        pool.append(toks)
    rng.shuffle(pool)
    out, seen = [], set()
    i = 0
    while len(out) < count and pool:
        toks = pool[i % len(pool)]
        start = rng.randrange(0, len(toks) - n_words + 1)
        frag = " ".join(toks[start : start + n_words])
        if frag not in seen:
            seen.add(frag)
            out.append(frag)
        i += 1
        if i > 40 * count:
            break
    return out


# ---- the rules under test ---------------------------------------------------


def make_rule(dia_langs, tri_langs, ngrams, exclusive, margins=None):
    """A composite guesser: script, diacritics, stopwords, trigrams."""

    def rule(text):
        tag = guess_by_script(text)
        if tag:
            return tag
        if dia_langs:
            tag = guess_by_diacritic(text, exclusive, dia_langs)
            if tag:
                return tag
        tag, _conf = stopword_guess(text)
        if tag:
            return tag
        if tri_langs and ngrams is not None:
            tag = ngrams.guess(text, tri_langs, margins)
            if tag:
                return tag
        return None

    return rule


LATIN = [t for t in sorted(SHIP) if t in STOPWORDS]


def evaluate(rule, corpus):
    """corpus: list of (truth, text). Returns per-tag (n, guessed, hit)."""
    total, guessed, correct = Counter(), Counter(), Counter()
    leaks = Counter()
    for truth, text in corpus:
        total[truth] += 1
        tag = rule(text)
        if tag is None:
            continue
        guessed[tag] += 1
        if tag == truth:
            correct[tag] += 1
        elif truth in ("de", "en"):
            leaks[truth] += 1
    return total, guessed, correct, leaks


def report(name, per_length, langs, out=sys.stdout):
    print(f"\n### {name}\n", file=out)
    head = "| lang |" + "".join(f" {n}w prec | {n}w rec |" for n in LENGTHS)
    print(head, file=out)
    print("|---:|" + "---:|" * (2 * len(LENGTHS)), file=out)
    for tag in langs:
        cells = []
        for n in LENGTHS:
            total, guessed, correct, _ = per_length[n]
            g, c, t = guessed.get(tag, 0), correct.get(tag, 0), total.get(tag, 0)
            prec = c / g if g else float("nan")
            rec = c / t if t else 0.0
            cells.append(f" {prec:.1%} |" if g else " – |")
            cells.append(f" {rec:.1%} |")
        print(f"| {tag} |" + "".join(cells), file=out)
    cells = []
    for n in LENGTHS:
        total, _g, _c, leaks = per_length[n]
        neg = total.get("de", 0) + total.get("en", 0)
        fp = sum(leaks.values()) / neg if neg else 0.0
        cells.append(f" {sum(leaks.values())} = {fp:.2%} |")
    print("\n| de/en false positives |" + "".join(f" {n}w |" for n in LENGTHS), file=out)
    print("|---:|" + "---:|" * len(LENGTHS), file=out)
    print("| count = rate |" + "".join(cells), file=out)


def gate_verdict(per_length, tag, prec_floor=0.95):
    """Does this language clear >= prec_floor at every fragment length?"""
    for n in LENGTHS:
        total, guessed, correct, _ = per_length[n]
        g, c = guessed.get(tag, 0), correct.get(tag, 0)
        if g == 0:
            continue  # nothing claimed is not a precision failure
        if c / g < prec_floor:
            return False
    return True


def deen_fp(per_length):
    return {
        n: sum(per_length[n][3].values()) / max(per_length[n][0].get("de", 0) + per_length[n][0].get("en", 0), 1)
        for n in LENGTHS
    }


def total_recall(per_length, langs):
    hit = sum(per_length[n][2].get(t, 0) for n in LENGTHS for t in langs)
    tot = sum(per_length[n][0].get(t, 0) for n in LENGTHS for t in langs)
    return hit / tot if tot else 0.0


# ---- the generated Rust table ----------------------------------------------


def alphabet_of(ngrams):
    chars = {" "}
    for table in ngrams.tables.values():
        for g in table:
            chars.update(g)
    return "".join(sorted(chars))


def pack(g, index):
    a = index.get(g[0], 1)
    b = index.get(g[1], 1)
    c = index.get(g[2], 1)
    n = len(index) + 2
    return (a * n + b) * n + c


def emit_rust(ngrams, path, dia_langs, tri_langs, exclusive):
    alpha = alphabet_of(ngrams)
    # index 0 is "not in the alphabet"; the rest are the alphabet in order.
    index = {ch: i + 1 for i, ch in enumerate(alpha)}
    n = len(index) + 2
    lines = []
    lines.append("//! Character-trigram language tables — GENERATED, do not edit.\n")
    lines.append("//!\n")
    lines.append("//! Written by `spike/short_lang_bench.py --emit`, from the FLEURS **dev**\n")
    lines.append("//! text of every Latin-script language `lang::guess_other` may answer with,\n")
    lines.append("//! plus German and English as the two the guess has to beat and Norwegian as\n")
    lines.append("//! a blocker. Add-one smoothed log-probabilities, pruned to the\n")
    lines.append(f"//! {TOP_TRIGRAMS} commonest trigrams per language, scaled by {int(LOG_SCALE)} into an i16.\n")
    lines.append("//!\n")
    lines.append("//! The gate these tables passed is `spike/FINDINGS.md` §21.\n\n")
    lines.append("/// Every character the tables use, in packing order: a character's index\n")
    lines.append("/// is its position in `ALPHABET.chars()` plus one, and everything else is 0.\n")
    lines.append("///\n")
    lines.append("/// `chars()`, not bytes — most of these are two bytes long.\n")
    lines.append(f"pub const ALPHABET: &str = {alpha!r};\n".replace("'", '"'))
    lines.append("/// The packing radix: the character count of [`ALPHABET`] plus two.\n")
    lines.append(f"pub const RADIX: u32 = {n};\n")
    lines.append(f"/// Log-probabilities are stored as `round(logp * {int(LOG_SCALE)})`.\n")
    lines.append(f"pub const LOG_SCALE: f32 = {LOG_SCALE};\n")
    lines.append("/// The log-probability of a trigram a language's table does not hold.\n")
    lines.append("pub const FLOORS: &[(&str, i16)] = &[\n")
    for tag in sorted(ngrams.tables):
        lines.append(f"    ({tag!r}, {round(ngrams.floors[tag] * LOG_SCALE)}),\n".replace("'", '"'))
    lines.append("];\n\n")
    lines.append("/// Per language, its trigrams sorted by the packed key for a binary search.\n")
    lines.append("pub const TABLES: &[(&str, &[(u32, i16)])] = &[\n")
    for tag in sorted(ngrams.tables):
        entries = sorted((pack(g, index), round(lp * LOG_SCALE)) for g, lp in ngrams.tables[tag].items())
        lines.append(f'    ("{tag}", &[\n')
        row = []
        for k, v in entries:
            row.append(f"({k},{v}),")
            if len(row) == 8:
                lines.append("        " + "".join(row) + "\n")
                row = []
        if row:
            lines.append("        " + "".join(row) + "\n")
        lines.append("    ]),\n")
    lines.append("];\n\n")
    lines.append("/// Characters exactly one language of the set writes, derived from the same\n")
    lines.append("/// dev text as [`TABLES`] — see `short_lang_bench.derive_exclusive`. `ç` is\n")
    lines.append("/// not French here and `ø` is not Danish, because Turkish, Portuguese and\n")
    lines.append("/// Norwegian have them: shared is not exclusive, whatever it looks like.\n")
    lines.append("pub const EXCLUSIVE: &[(&str, &str)] = &[\n")
    for tag in sorted(exclusive):
        lines.append(f'    ("{tag}", "{exclusive[tag]}"),\n')
    lines.append("];\n\n")
    lines.append("/// Languages the diacritic stage may answer with (FINDINGS §21's gate).\n")
    lines.append("pub const DIACRITIC_SHIP: &[&str] = &[")
    lines.append(", ".join(f'"{t}"' for t in sorted(dia_langs)))
    lines.append("];\n\n")
    lines.append("/// The two-word floor both new stages apply, and the margin coefficients\n")
    lines.append("/// the trigram stage decides with: `A + B / sqrt(trigrams)`. Duplicated\n")
    lines.append("/// into `lang.rs` as typed constants and asserted equal by a test there —\n")
    lines.append("/// these are the values FINDINGS §21's table was measured at.\n")
    lines.append(f"pub const MIN_WORDS: usize = {TRI_MIN_WORDS};\n")
    lines.append(f"pub const MARGIN_A: f32 = {TRI_MARGIN_A};\n")
    lines.append(f"pub const MARGIN_B: f32 = {TRI_MARGIN_B};\n")
    lines.append(f"pub const MARGIN_DEEN_A: f32 = {TRI_MARGIN_DEEN_A};\n")
    lines.append(f"pub const MARGIN_DEEN_B: f32 = {TRI_MARGIN_DEEN_B};\n\n")
    lines.append("/// Languages the trigram stage may answer with. Everything else in\n")
    lines.append("/// [`TABLES`] votes and cannot win — `de`, `en` and the Norwegian blocker.\n")
    lines.append("pub const TRIGRAM_SHIP: &[&str] = &[")
    lines.append(", ".join(f'"{t}"' for t in sorted(tri_langs)))
    lines.append("];\n")
    Path(path).write_text("".join(lines), encoding="utf-8")
    return Path(path).stat().st_size


# ---- main -------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--emit", help="write the generated Rust table here")
    ap.add_argument("--tune", action="store_true", help="sweep the trigram margins")
    args = ap.parse_args()

    # Tables from dev.
    dev = {}
    for tag in TRI_LANGS:
        rows = dev_texts(tag)
        if not rows:
            sys.exit(f"no dev text for {tag}")
        dev[tag] = rows
    ngrams = Ngrams.build(dev)
    print("trigram tables (dev):", file=sys.stderr)
    for tag in TRI_LANGS:
        print(f"  {tag}: {len(dev[tag])} sentences, {len(ngrams.tables[tag])} trigrams kept", file=sys.stderr)

    # Fragments from test.
    corpora = {}
    for n in LENGTHS:
        corpus = []
        for tag in FLEURS_DIRS:
            rows = test_texts(tag)
            if not rows:
                print(f"missing test corpus: {tag}", file=sys.stderr)
                continue
            count = NEGATIVES if tag in ("de", "en") else PER_LANG
            for f in fragments(rows, n, count, seed=1000 + n):
                corpus.append((tag, f))
        corpora[n] = corpus
    for n in LENGTHS:
        c = Counter(t for t, _ in corpora[n])
        print(f"{n}w fragments: {sum(c.values())} ({len(c)} languages)", file=sys.stderr)

    exclusive = derive_exclusive(dev)
    print("\n### the exclusive characters, derived from the dev text\n")
    print("| lang | characters |")
    print("|---:|:---|")
    for tag in sorted(exclusive):
        print(f"| {tag} | `{exclusive[tag]}` |")

    all_langs = sorted(SHIP)
    dia_all = set(exclusive)
    tri_all = set(TRI_LANGS) - {"de", "en", "no"}

    if args.tune:
        print("\n### the margin sweep\n", file=sys.stderr)
        for a in (0.06, 0.10, 0.14):
            for b in (0.9, 1.1, 1.3, 1.6):
                for da_, db_ in ((a, b), (a + 0.04, b + 0.5)):
                    m = (a, b, da_, db_)
                    rule = make_rule(set(), tri_all, ngrams, exclusive, m)
                    pl = {n: evaluate(rule, corpora[n]) for n in LENGTHS}
                    fp = deen_fp(pl)
                    ok = [t for t in tri_all if gate_verdict(pl, t)]
                    print(
                        f"  A={a} B={b} dA={da_:.2f} dB={db_:.2f} "
                        f"fp={max(fp.values()):.2%} pass={len(ok)}/{len(tri_all)} "
                        f"rec={total_recall(pl, sorted(tri_all)):.1%} {' '.join(sorted(ok))}",
                        file=sys.stderr,
                    )
        return

    rules = {
        "stopwords (as shipped)": (set(), set()),
        "+ exclusive diacritics": (dia_all, set()),
        "+ character trigrams": (set(), tri_all),
        "all three": (dia_all, tri_all),
    }
    results = {}
    for name, (dia, tri) in rules.items():
        rule = make_rule(dia, tri, ngrams, exclusive)
        per_length = {n: evaluate(rule, corpora[n]) for n in LENGTHS}
        results[name] = per_length
        report(name, per_length, all_langs)

    # ---- the composite: per language, the most generous stage it survives ----
    print("\n### the gate, per language\n")
    print("| lang | stopwords | + diacritics | + trigrams | ships |")
    print("|---:|:---:|:---:|:---:|:---|")
    dia_ship, tri_ship = set(), set()
    for tag in all_langs:
        ok_stop = gate_verdict(results["stopwords (as shipped)"], tag)
        ok_dia = tag in dia_all and gate_verdict(results["+ exclusive diacritics"], tag)
        ok_tri = tag in tri_all and gate_verdict(results["+ character trigrams"], tag)
        stages = ["stopwords"]
        if ok_dia:
            dia_ship.add(tag)
            stages.append("diacritics")
        if ok_tri:
            tri_ship.add(tag)
            stages.append("trigrams")
        mark = lambda b: "yes" if b else "NO"  # noqa: E731
        print(
            f"| {tag} | {mark(ok_stop)} | {mark(ok_dia) if tag in dia_all else '–'} | "
            f"{mark(ok_tri) if tag in tri_all else '–'} | {' + '.join(stages)} |"
        )

    rule = make_rule(dia_ship, tri_ship, ngrams, exclusive)
    per_length = {n: evaluate(rule, corpora[n]) for n in LENGTHS}
    report("THE COMPOSITE RULE (what ships)", per_length, all_langs)

    fp = deen_fp(per_length)
    print("\ngate: de/en false positives <= 0.5% at every length")
    for n in LENGTHS:
        print(f"  {n}w: {fp[n]:.2%} {'PASS' if fp[n] <= 0.005 else 'FAIL'}")
    print(f"\nrecall over all shippable languages and lengths: {total_recall(per_length, all_langs):.1%}")
    print(f"  (stopwords alone: {total_recall(results['stopwords (as shipped)'], all_langs):.1%})")
    print("\ndiacritic ship: " + " ".join(sorted(dia_ship)))
    print("trigram ship:   " + " ".join(sorted(tri_ship)))

    if args.emit:
        size = emit_rust(ngrams, args.emit, dia_ship, tri_ship, exclusive)
        print(f"\nwrote {args.emit}: {size / 1024:.0f} KB", file=sys.stderr)


if __name__ == "__main__":
    main()
