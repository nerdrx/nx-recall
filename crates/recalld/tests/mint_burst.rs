//! The evening of 2026-09-04, reproduced from a fixture (FINDINGS §46).
//!
//! Twenty phantom voices in thirty-three minutes, and not one of them was a
//! stranger. The mechanism is four facts in a row, and the fixture below is
//! built to hold all four:
//!
//! 1. the nightly pass fitted a label bar for a voice **above what that voice's
//!    own turns typically score** — 0.60 for a phantom whose turns score 0.5,
//!    0.41 for Rowan whose tenth percentile is 0.417;
//! 2. so a turn of that person fails its own bar and, before 0.12.2, that is a
//!    `Mint`;
//! 3. a mint seeds the new voice with the turn's **own audio**, and a
//!    same-evening recording of a person outscores the older bank;
//! 4. so the *next* turn of the same person matches the phantom — and either
//!    takes its name or fails the next fitted bar and mints again.
//!
//! Nothing here needs a model or a database: it is the ladder, the enrol path
//! and the eviction rule, which is exactly the loop that ran on the box.
//!
//! Run: `cargo test -p recalld --test mint_burst`

use recalld::calib::Thresholds;
use recalld::config::IdentityConfig;
use recalld::embed::Embedding;
use recalld::identity::{self, Decision};

const MODEL: &str = "eres2net_en@1";

const DIM: usize = 4 + 8 + 400;
const VOICE_DIM: usize = 0;
const ERA_DIM: usize = 4;
const TAKE_DIM: usize = 12;

/// One recording, at the cosines this install actually produces.
///
/// The numbers are not decoration. §44 measured a voice against its own bank at
/// 0.34–0.60, and the mint of 21:47:11 on 2026-09-04 read **0.614 on a phantom
/// holding a fresh recording of Rowan, 0.544 on Rowan's own two-day-old bank**.
/// Those two numbers, 0.07 apart, are the entire failure: a fitted margin of
/// 0.08 between two banks of the same person.
///
/// So a recording is `a·(who) + b·(who, which evening) + c·(this take)`, with
/// the weights chosen to put same-person-same-evening at 0.61, same-person-
/// different-evening at 0.544, and different people at 0.
fn take(who: usize, era: usize, take: usize) -> Embedding {
    const A: f32 = 0.738; // shared by every recording of this person
    const B: f32 = 0.255; // shared by this person's recordings this evening
    const C: f32 = 0.625; // this recording alone
    let mut v = vec![0.0f32; DIM];
    v[VOICE_DIM + who] = A;
    v[ERA_DIM + who * 2 + era] = B;
    v[TAKE_DIM + take] = C;
    Embedding::new(MODEL, v)
}

const HELVO: usize = 0;
const EMBER: usize = 1;
const STRANGER: usize = 2;
/// Two days ago, and tonight.
const THEN: usize = 0;
const NOW: usize = 1;

/// The speaker ids the fixture uses. 1 and 2 are the two people the bank knows;
/// 68 is the phantom the nightly pass had already fitted a bar for, which is
/// where the real evening started.
const HELVO_ID: i64 = 1;
const EMBER_ID: i64 = 2;
const PHANTOM_68: i64 = 68;

struct Box_ {
    cfg: IdentityConfig,
    /// `(speaker, source segment, vector)`.
    bank: Vec<(i64, i64, Embedding)>,
    thresholds: Thresholds,
    next_speaker: i64,
    next_take: usize,
    minted: Vec<i64>,
    /// `(who really spoke, the name the ladder put on it)`.
    labels: Vec<(usize, Option<i64>)>,
    declined: usize,
}

impl Box_ {
    fn new(thresholds: Thresholds) -> Self {
        let cfg = IdentityConfig {
            max_overlap: 0.06, // the live box's
            ..IdentityConfig::default()
        };
        Self {
            cfg,
            bank: Vec::new(),
            thresholds,
            next_speaker: 100,
            next_take: 0,
            minted: Vec::new(),
            labels: Vec::new(),
            declined: 0,
        }
    }

    /// The bank as it stood: `n` recordings of each person from two days ago,
    /// and — this is the part that matters — one phantom already holding a
    /// recording of Rowan from *tonight*.
    fn with_the_bank_of_2026_09_04(mut self, n: usize) -> Self {
        for (who, id) in [(HELVO, HELVO_ID), (EMBER, EMBER_ID)] {
            for _ in 0..n {
                let t = self.next_take;
                self.next_take += 1;
                self.bank.push((id, -1 - t as i64, take(who, THEN, t)));
            }
        }
        let t = self.next_take;
        self.next_take += 1;
        self.bank
            .push((PHANTOM_68, -1 - t as i64, take(HELVO, NOW, t)));
        self
    }

    /// `store::add_prototype` plus `identity::prototype_to_evict`.
    fn enrol(&mut self, speaker: i64, segment: i64, e: &Embedding) {
        let mine: Vec<(i64, Embedding, bool)> = self
            .bank
            .iter()
            .enumerate()
            .filter(|(_, (sp, ..))| *sp == speaker)
            .map(|(i, (_, _, v))| (i as i64, v.clone(), false))
            .collect();
        if mine.len() >= self.cfg.max_prototypes {
            let victim = identity::prototype_to_evict(e, &mine).unwrap().unwrap();
            self.bank.remove(victim as usize);
        }
        self.bank.push((speaker, segment, e.clone()));
    }

    /// `analysis::commit`'s identity leg, minus the I/O.
    fn turn(&mut self, who: usize) {
        let t = self.next_take;
        self.next_take += 1;
        let segment = 1000 + t as i64;
        let e = take(who, NOW, t);
        let live: Vec<(i64, Embedding)> = self
            .bank
            .iter()
            .filter(|(_, src, _)| *src != segment)
            .map(|(sp, _, v)| (*sp, v.clone()))
            .collect();
        let ranked = identity::rank(&e, &live).unwrap();
        match identity::decide_with(&self.cfg, &self.thresholds, 0.0, 4.0, 8, &ranked) {
            Decision::Matched {
                speaker_id, enroll, ..
            } => {
                if enroll {
                    self.enrol(speaker_id, segment, &e);
                }
                self.labels.push((who, Some(speaker_id)));
            }
            Decision::Mint { .. } => {
                let id = self.next_speaker;
                self.next_speaker += 1;
                self.minted.push(id);
                self.enrol(id, segment, &e);
                self.labels.push((who, Some(id)));
            }
            Decision::Declined { .. } => {
                self.declined += 1;
                self.labels.push((who, None));
            }
            other => panic!("the fixture should not reach {other:?}"),
        }
    }

    /// An evening in a lobby: Rowan talking, with Aspen answering every third
    /// turn. Aspen is the control — no phantom is anywhere near her, so a rule
    /// that costs correct labels will cost hers.
    fn an_evening(&mut self, turns: usize) {
        for i in 0..turns {
            self.turn(if i % 3 == 2 { EMBER } else { HELVO });
        }
    }

    fn correct(&self) -> usize {
        self.labels
            .iter()
            .filter(|(who, l)| {
                *l == Some(match who {
                    &HELVO => HELVO_ID,
                    _ => EMBER_ID,
                })
            })
            .count()
    }

    fn wrong(&self) -> usize {
        self.labels
            .iter()
            .filter(|(who, l)| {
                l.is_some_and(|id| {
                    id != match who {
                        &HELVO => HELVO_ID,
                        _ => EMBER_ID,
                    }
                })
            })
            .count()
    }
}

/// The fitted table the pass installed on 2026-09-04: Rowan's own bar above the
/// bottom of Rowan's own score distribution, and a phantom pinned at the top of
/// `THRESHOLD_BOUNDS` because ground truth never confirmed it and the fitter's
/// tie-break prefers "turn its wrong labels into declines".
fn the_bars_of_2026_09_04() -> Thresholds {
    Thresholds::global(0.35, 0.0)
        .with(HELVO_ID, 0.41, 0.0)
        .with(PHANTOM_68, 0.60, 0.08)
}

/// The one number the whole round turns on, asserted so a change to the fixture
/// cannot quietly stop reproducing the failure.
#[test]
fn the_fixture_puts_the_scores_where_the_evening_had_them() {
    let a = take(HELVO, NOW, 300);
    let same_evening = take(HELVO, NOW, 301);
    let two_days_old = take(HELVO, THEN, 302);
    let somebody_else = take(EMBER, NOW, 303);
    let cos = |x: &Embedding| (a.cosine(x).unwrap() * 1000.0).round() / 1000.0;
    assert_eq!(
        cos(&same_evening),
        0.609,
        "a phantom holding tonight's audio"
    );
    assert_eq!(cos(&two_days_old), 0.544, "the person's own older bank");
    assert_eq!(cos(&somebody_else), 0.0, "and anybody else");
    // 0.609 − 0.544 = 0.065, under the fitted margin of 0.08. That is the mint.
}

#[test]
fn the_2026_09_04_cascade_reproduces_without_the_rule() {
    // The control, and the reason the rule exists. The same evening with the
    // fitted numbers read the way 0.12.1 read them — a bar that turns a label
    // down turns it into a new voice — expressed as a table where they are the
    // GLOBAL point, so the arithmetic is 0.12.1's and not a second code path.
    let mut b = Box_::new(Thresholds::global(0.60, 0.08)).with_the_bank_of_2026_09_04(6);
    b.an_evening(30);
    assert!(
        b.minted.len() >= 15,
        "the cascade should run away; it minted {} voices",
        b.minted.len()
    );
    assert_eq!(b.correct(), 0, "and not one turn came back with a name");
    assert_eq!(b.wrong(), 30, "every turn of the evening went to a phantom");
}

#[test]
fn a_fitted_bar_mints_nothing_and_costs_no_correct_label() {
    // The same thirty turns, the same bank, the same numbers — read as what
    // they are, a bar fitted for one voice. Every mint of the evening is gone.
    // Aspen is the control: no phantom is near her, her bar is the global, and
    // she keeps every turn she had.
    let mut b = Box_::new(the_bars_of_2026_09_04()).with_the_bank_of_2026_09_04(6);
    b.an_evening(30);
    assert_eq!(b.minted, Vec::<i64>::new(), "no phantom voices");
    assert_eq!(b.wrong(), 0, "and nothing filed under the wrong name");
    let aspen_turns = (0..30).filter(|i| i % 3 == 2).count();
    assert_eq!(
        b.correct(),
        aspen_turns,
        "Aspen keeps all {aspen_turns} of hers"
    );
    assert_eq!(b.declined + b.correct(), 30);
}

#[test]
fn the_rule_does_not_stop_a_stranger_getting_a_voice() {
    // The other half of the claim, and the one that would make the rule
    // unshippable if it failed: a person the bank has never heard still mints,
    // is enrolled, and is recognised from then on. §46 measured the same thing
    // against the archive's two real cold starts, where the rules that *did*
    // block a cold start (a near-miss slack of 0.10, a minimum prototype count)
    // were refused for it.
    let mut b = Box_::new(the_bars_of_2026_09_04()).with_the_bank_of_2026_09_04(6);
    for _ in 0..12 {
        b.turn(STRANGER);
    }
    assert_eq!(
        b.minted.len(),
        1,
        "one new voice, not twelve: {:?}",
        b.minted
    );
    let new = b.minted[0];
    let after = b
        .labels
        .iter()
        .skip(1)
        .filter(|(_, l)| *l == Some(new))
        .count();
    assert!(
        after >= 8,
        "and it was recognised again {after} times of 11"
    );
}
