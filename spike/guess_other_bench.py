#!/usr/bin/env python3
"""Which third language is this? — the measurement behind `lang::guess_other`.

`crate::lang::classify` knows exactly two languages, de and en, and a French
turn therefore comes out `Unclear` — the same stamp a mumbled German line gets.
0.10.2 needs to tell those apart, because "translate everything that is not
German or English" is only implementable if a French line can be recognised as
*not German or English* rather than as *unreadable*.

The rule measured here is the one shipped in `lang::guess_other`:

  1. script first — Cyrillic / kana / hangul / han / arabic / greek settle it
     outright, because a Latin-alphabet stopword vote cannot be wrong about a
     sentence with no Latin letters in it;
  2. then a stopword vote over the Latin-script candidates, which fires only
     when a language has >= MIN_VOTES stopwords AND strictly more than de+en
     together (`lang::stopword_votes`), and only when it beats the runner-up.

Corpus: FLEURS dev sentences (200 per language, the raw transcription column),
plus 400 German and English sentences as negatives — a guesser that reads
German as Dutch would send the reader's own language to a translator, which is
the failure this gate exists to price.

    python3 spike/guess_other_bench.py

Precision here is over the WHOLE mixed set, negatives included: of every
sentence the rule called `fr`, how many were French. A language ships only at
>= 90%.
"""

import csv
import random
import sys
import unicodedata
from collections import Counter
from pathlib import Path

SCRATCH = Path("/tmp/nx-recall-workspace/nx-scratch")
FLEURS = SCRATCH / "fleurs"
GERMAN = SCRATCH / "de" / "dev.tsv"

MIN_VOTES = 3
CONFIDENT_VOTES = 4
PER_LANG = 200
NEGATIVES = 200  # per negative language, so 400 in total

# ---- the tables, verbatim in crates/recalld/src/lang.rs ---------------------

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

# The languages the rule is allowed to answer with. Everything else in the
# table above votes but cannot win — see `guess_other`.
SHIP = set("fr es it pt nl pl tr sv da fi cs ru uk ja zh ko ar el".split())

DE = "der die das und ist nicht ich du wir ihr sie es ein eine einen dem den mit von für auf als auch aber wenn dann noch schon nur mal was wie wo ja nein doch beim vom zur zum über unter zwischen gegen ohne durch".split()
EN = "the a an and is are was were not i you we they it this that of to in for on with as at by from but if then just only what how where yes no about into over under between against without through".split()


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


def guess_other(text):
    """(tag, confident) or (None, False). The shipped rule."""
    counts, letters = script_counts(text)
    # Kana and hangul are checked by PRESENCE, not by dominance: Japanese
    # writes most of its content words in Han characters and its grammar in
    # kana, so a kanji-heavy sentence is dominated by a script Chinese also
    # uses — that mistake alone cost `zh` 13 points of precision before this
    # rule went in. Two characters, so one borrowed word is not a language.
    if counts["HIRAGANA"] + counts["KATAKANA"] >= 2:
        return ("ja", True)
    if counts["HANGUL"] >= 2:
        return ("ko", True)
    # A third of the letters, for the rest. A Russian world name inside a
    # German sentence is not a Russian sentence.
    for key, tag in (("CYRILLIC", "ru"), ("CJK", "zh"), ("ARABIC", "ar"), ("GREEK", "el")):
        if letters and counts[key] * 3 >= letters:
            if tag == "ru":
                # Ukrainian has four letters Russian does not. Any of them
                # settles it; otherwise Russian, the commoner case by far.
                return ("uk" if any(c in "їієґЇІЄҐ" for c in text) else "ru", True)
            return (tag, True)

    ws = words(text)
    if not ws:
        return (None, False)
    bag = Counter(ws)
    de = sum(bag[w] for w in DE)
    en = sum(bag[w] for w in EN)
    scores = sorted(
        ((sum(bag[w] for w in sw), tag) for tag, sw in STOPWORDS.items()),
        reverse=True,
    )
    best, tag = scores[0]
    runner = scores[1][0]
    if best < MIN_VOTES or best <= de + en or best == runner:
        return (None, False)
    if tag not in SHIP:
        # In the table but not shipped. Norwegian Bokmål is the case: its
        # function words are Danish's, it wins the vote on Danish sentences as
        # often as Danish does, and neither reaches the gate while both are
        # answers. Kept in the table as a BLOCKER — a Norwegian sentence
        # reaches here, wins, and is answered "I cannot tell" instead of being
        # confidently called Danish.
        return (None, False)
    return (tag, best >= CONFIDENT_VOTES)


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
}


def sentences(paths, n, seed=7):
    """`n` distinct sentences drawn from one or more FLEURS tsvs.

    A dev split holds each sentence once per speaker, so the ~350 rows of a
    dev.tsv are only ~150 distinct sentences — the negatives therefore draw
    from dev *and* test to reach the 200 each the gate is priced on.
    """
    if isinstance(paths, Path):
        paths = [paths]
    seen, out = set(), []
    for path in paths:
        with open(path, encoding="utf-8") as fh:
            for row in csv.reader(fh, delimiter="\t", quoting=csv.QUOTE_NONE):
                if len(row) < 3:
                    continue
                s = row[2].strip()
                if len(s) < 12 or s in seen:
                    continue
                seen.add(s)
                out.append(s)
    random.Random(seed).shuffle(out)
    return out[:n]


def main():
    corpus = []  # (true_tag, sentence)
    missing = []
    for tag, dirname in FLEURS_DIRS.items():
        paths = [p for p in (FLEURS / f"{dirname}.dev.tsv", FLEURS / f"{dirname}.test.tsv") if p.exists()]
        if not paths:
            missing.append(tag)
            continue
        n = NEGATIVES if tag == "en" else PER_LANG
        for s in sentences(paths, n):
            corpus.append((tag, s))
    de_paths = [p for p in (GERMAN, FLEURS / "de_de.test.tsv") if p.exists()]
    if de_paths:
        for s in sentences(de_paths, NEGATIVES):
            corpus.append(("de", s))
    else:
        missing.append("de")
    if missing:
        print(f"missing corpora: {missing}", file=sys.stderr)
    if not corpus:
        sys.exit("no corpus; download the FLEURS dev tsvs first")

    guessed = Counter()
    correct = Counter()
    total = Counter()
    confident = Counter()
    confident_ok = Counter()
    leaks = Counter()  # what the negatives were called
    for truth, s in corpus:
        total[truth] += 1
        tag, conf = guess_other(s)
        if tag is None:
            continue
        guessed[tag] += 1
        if conf:
            confident[tag] += 1
        if tag == truth:
            correct[tag] += 1
            if conf:
                confident_ok[tag] += 1
        elif truth in ("de", "en"):
            leaks[f"{truth}->{tag}"] += 1

    print(f"corpus: {sum(total.values())} sentences, {len(total)} languages\n")
    print(f"{'lang':>5} {'n':>5} {'guessed':>8} {'hit':>5} {'prec':>7} {'rec':>7}  ship")
    order = sorted(set(list(STOPWORDS) + ["ru", "uk", "ja", "zh", "ko", "ar", "el"]))
    shipped = []
    for tag in order:
        n = total.get(tag, 0)
        g = guessed.get(tag, 0)
        c = correct.get(tag, 0)
        prec = c / g if g else 0.0
        rec = c / n if n else 0.0
        ok = g > 0 and prec >= 0.90
        if ok:
            shipped.append(tag)
        print(f"{tag:>5} {n:>5} {g:>8} {c:>5} {prec:>6.1%} {rec:>6.1%}  {'yes' if ok else 'NO'}")

    print("\nnegatives (de/en) sent to a translator by a wrong guess:")
    for k, v in leaks.most_common():
        print(f"  {k}: {v}")
    if not leaks:
        print("  none")
    de_en = total.get("de", 0) + total.get("en", 0)
    print(f"  {sum(leaks.values())}/{de_en} = {sum(leaks.values()) / max(de_en, 1):.2%} of the negatives")
    print("\nship: " + " ".join(shipped))


if __name__ == "__main__":
    main()
