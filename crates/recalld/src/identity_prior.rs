//! Where the audio came from is evidence about who is on it (0.11.0).
//!
//! The ladder in [`crate::identity`] asks one question — *which prototype is
//! this vector nearest* — and asks it of the whole voicebank at once. That is
//! the right question when every voice is equally likely to be present, and
//! this install is not that: the user's own database has 2 798 turns of one
//! voice on Discord and two on a second Discord client, 4 547 of their own
//! voice on the microphone, and eight VRChat turns in total. A voice heard
//! thousands of times on Discord and never once in VRChat should not win a
//! VRChat segment at 0.36, and at 0.35 (`label_threshold`) it can — the audit
//! finds three such labels in 9 039, all scoring 0.361-0.393 (FINDINGS §17).
//!
//! So this module is a **prior on presence**, applied to the candidate list
//! before the ladder sees it. Nothing here scores audio; it only decides which
//! candidates the ladder is allowed to consider.
//!
//! # The three rules, and why they are three
//!
//! ### 1. Soft — foreign to this source
//!
//! A candidate with at least `foreign_after_segments` turns in total and
//! **zero** on the source being labelled is *foreign* to it. A foreign
//! candidate has to clear a raised bar to win:
//!
//! * `score >= label_threshold + foreign_source_margin`, and
//! * it must beat the best **native** candidate by `enroll_margin` — the
//!   ladder's only margin constant, reused rather than duplicated.
//!
//! A foreign candidate that fails either test is **removed**, not merely
//! demoted. That is the load-bearing half: the ladder then proceeds as if the
//! voice were not in the bank at all, so a genuinely new voice on this source
//! can mint instead of being absorbed into a stranger from another app.
//!
//! `foreign_after_segments` is what keeps the rule from eating brand-new
//! voices. A voice with four turns has no source history to speak of; calling
//! it foreign to a source it has simply not been heard on yet would freeze the
//! bank at whatever app happened to hear each person first.
//!
//! **The You voice is never foreign.** The microphone follows the user
//! everywhere and its label is provenance rather than a match ([`crate::analysis`]
//! `commit_mic`), so the one voice that is genuinely present at every source
//! is exempt by construction.
//!
//! ### 2. Hard — Discord says they were not there
//!
//! On a **Discord-sourced** segment, a candidate linked to a Discord account
//! (`discord_users.speaker_id`) that has no speaking span within ±5 minutes is
//! **excluded outright** — not demoted, not margin-raised. Removed.
//!
//! This is allowed to be hard because the evidence is categorical and comes
//! from Discord itself rather than from a model: the 0.9.0 bridge sees every
//! speaking ring in the call, so "this account said nothing for ten minutes
//! either side of this turn" is Discord's word, not an inference of ours. A
//! turn on the Discord stream is by definition audio Discord decoded, so if the
//! account was not talking, the audio is not theirs.
//!
//! Three guards keep it honest:
//!
//! * It only fires when truth data exists in the window at all. No plugin means
//!   no evidence, and no evidence is not absence.
//! * It only fires for candidates that have a Discord link. An unlinked voice
//!   is not claimed to be absent — nothing knows where it was.
//! * It only fires on Discord-sourced segments. Discord's speaking events say
//!   nothing about who is audible in VRChat.
//!
//! The known cost, stated rather than hidden: a linked account that sat in the
//! call silently for more than five minutes either side of a turn is excluded
//! from that turn. `truth_speaking` records *speaking*, not membership, so
//! silence is the only membership signal there is. The trade is defensible
//! precisely because it is the same window: an account that said nothing for
//! ten minutes around a turn is a poor explanation for that turn.
//!
//! ### 3. Soft — the VRChat roster
//!
//! On a **VRChat-sourced** segment, a *named* candidate whose display name is
//! not in the roster within ±10 minutes gets the **foreign treatment**, never
//! exclusion.
//!
//! The asymmetry with rule 2 is the point, and it is about what the two
//! sources actually know. Discord's events are keyed on an account id that
//! cannot be typed wrong. The roster is display names scraped out of VRChat's
//! text log, matched to a voice by nothing more than the user having typed the
//! same string ([`crate::store`] `roster_intervals`) — people rename
//! themselves, the log rotates, and a name that fails to match is far more
//! often our failure than their absence. Evidence that weak may raise a bar; it
//! may not slam a door. Unnamed voices are untouched for the same reason: they
//! have no name to look up, and *not knowing* is not *not there*.
//!
//! # What this does not do
//!
//! It does not invent a `label_via` value. A label that survived a foreign
//! check is still a match — the ladder made it, on the same evidence, at a
//! higher bar — and a client that saw `label_via = "foreign"` would have to
//! decide what to do about a distinction it cannot act on. What the daemon
//! records instead is a log line and a counter ([`Applied::note`]).

use anyhow::Result;

use crate::config::IdentityConfig;
use crate::identity::Candidate;
use crate::store::{SegmentSource, Store};

/// How far either side of a segment Discord's speaking events are consulted.
///
/// The same reach [`crate::truth`] uses to decide whether the plugin was
/// running at all, and deliberately the same number: this rule is an
/// interpretation of that subsystem's data, and reading it over a different
/// window would make "no truth nearby" mean two different things in one file.
pub const PRESENCE_REACH_NS: i64 = crate::truth::TRUTH_REACH_NS;

/// How far either side of a segment the VRChat roster is consulted.
///
/// Twice Discord's, because the evidence is different in kind. A speaking ring
/// is an instant; a roster line is a join or a leave, minutes or hours apart,
/// and a window as tight as five minutes would call somebody absent for the
/// gap between the log's entries rather than for having left.
pub const ROSTER_REACH_NS: i64 = 10 * 60 * 1_000_000_000;

/// Which family a capture source belongs to, as far as this module cares.
///
/// Not stored anywhere: it is decided per segment from the source's match key
/// and display name against the configured patterns, exactly as
/// [`crate::store::Store::segments_for_truth`] decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFamily {
    Discord,
    VrChat,
    /// Any other application, the microphone, or the room — no hard evidence of
    /// presence exists for these, so only the soft source-history rule applies.
    Other,
}

/// Decide a source's family from its identifiers.
///
/// Lower-case substring matching over both the match key and the display name,
/// which is what [`crate::truth`] already does; a fork of either client under
/// another name is a config edit rather than a rebuild. Discord is tested
/// first: nothing sensible matches both, and a source that somehow did is
/// better treated as the one with hard evidence behind it.
pub fn family_of(
    match_key: &str,
    display_name: &str,
    discord: &[String],
    vrchat: &[String],
) -> SourceFamily {
    let hit = |pats: &[String]| {
        let key = match_key.to_lowercase();
        let name = display_name.to_lowercase();
        pats.iter().any(|p| {
            let p = p.to_lowercase();
            !p.is_empty() && (key.contains(&p) || name.contains(&p))
        })
    };
    if hit(discord) {
        SourceFamily::Discord
    } else if hit(vrchat) {
        SourceFamily::VrChat
    } else {
        SourceFamily::Other
    }
}

/// What is known about one candidate voice's relationship to the source being
/// labelled. Gathered from the store; the rules below read nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    pub speaker_id: i64,
    /// Turns this voice has on **this** source.
    pub on_source: i64,
    /// Turns this voice has anywhere.
    pub total: i64,
    /// This is the pinned "You" voice: native everywhere, because the
    /// microphone follows the user everywhere.
    pub is_you: bool,
    /// Discord's word, on a Discord-sourced segment only: this voice is linked
    /// to at least one Discord account and **none** of them spoke within
    /// [`PRESENCE_REACH_NS`]. Only ever true when truth data existed in the
    /// window at all.
    pub discord_absent: bool,
    /// The roster's word, on a VRChat-sourced segment only: this voice has a
    /// name and that name is not in the roster within [`ROSTER_REACH_NS`].
    /// Only ever true when the roster had any entry in the window at all.
    pub roster_absent: bool,
}

impl Standing {
    /// A bare standing for a voice nothing is known about — every rule off.
    /// Used by the tests and by callers that could not reach the store.
    pub fn native(speaker_id: i64) -> Self {
        Self {
            speaker_id,
            on_source: 1,
            total: 1,
            is_you: false,
            discord_absent: false,
            roster_absent: false,
        }
    }
}

/// Why a candidate was taken off the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// Rule 2: Discord says the linked account was not speaking anywhere near
    /// this turn.
    HardAbsent,
    /// Rule 1 or 3: foreign to this source and below the raised bar.
    ForeignBelowBar,
    /// Rule 1 or 3: foreign to this source, over the raised bar, but not far
    /// enough ahead of a candidate that actually belongs here.
    ForeignBehindNative,
}

impl Dropped {
    pub fn as_str(&self) -> &'static str {
        match self {
            Dropped::HardAbsent => "not in the channel",
            Dropped::ForeignBelowBar => "foreign to this source, below the raised bar",
            Dropped::ForeignBehindNative => "foreign to this source, behind a native voice",
        }
    }
}

/// The candidate list the ladder should see, and what was taken out of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// Sorted descending, exactly as [`crate::identity::rank`] returns.
    pub kept: Vec<Candidate>,
    /// `(speaker_id, score, why)`, in the order they were ranked.
    pub dropped: Vec<(i64, f32, Dropped)>,
    /// Candidates that were foreign and **survived** the raised bar. Worth
    /// counting separately: these are the labels the prior nearly prevented.
    pub foreign_kept: Vec<i64>,
}

impl Applied {
    /// Nothing was changed.
    pub fn untouched(ranked: &[Candidate]) -> Self {
        Self {
            kept: ranked.to_vec(),
            dropped: Vec::new(),
            foreign_kept: Vec::new(),
        }
    }

    pub fn changed(&self) -> bool {
        !self.dropped.is_empty()
    }

    /// One line for the daemon log, or `None` when there is nothing to say.
    pub fn note(&self) -> Option<String> {
        if self.dropped.is_empty() {
            return None;
        }
        Some(
            self.dropped
                .iter()
                .map(|(id, score, why)| format!("{id} @ {score:.3} ({})", why.as_str()))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

/// Is this candidate foreign to the source being labelled?
///
/// Split out because it is the one predicate the whole feature turns on, and
/// because the audit command has to ask it about rows that have already been
/// labelled, where no scores are involved at all.
pub fn is_foreign(cfg: &IdentityConfig, st: &Standing) -> bool {
    if st.is_you {
        return false;
    }
    // The roster's verdict makes a voice foreign even where its history says
    // otherwise: a name that is not in the instance is evidence about THIS
    // turn, and turns on this source are evidence about the past.
    if st.roster_absent {
        return true;
    }
    st.on_source == 0 && st.total >= cfg.foreign_after_segments
}

/// Apply the prior to a ranked candidate list.
///
/// `ranked` must be sorted descending, as [`crate::identity::rank`] returns it;
/// `standings` may be in any order and may omit voices, which are then treated
/// as native (the honest reading of "nothing is known about this one").
///
/// Pure: no store, no clock, no model. Everything the rules need has already
/// been gathered into `standings`, which is what makes the table test below a
/// real test of the operating point rather than of a query.
pub fn apply(cfg: &IdentityConfig, standings: &[Standing], ranked: &[Candidate]) -> Applied {
    if !cfg.source_prior {
        return Applied::untouched(ranked);
    }
    let standing = |id: i64| standings.iter().find(|s| s.speaker_id == id);

    // Pass one: the hard rule. Done first and separately because an excluded
    // candidate must not be able to act as the "best native" that a foreign
    // candidate is then measured against — Discord has said it was not there,
    // so it is not there for any purpose.
    let mut dropped: Vec<(i64, f32, Dropped)> = Vec::new();
    let mut surviving: Vec<(&Candidate, bool)> = Vec::new();
    for c in ranked {
        let st = standing(c.speaker_id);
        if cfg.presence_hard && st.is_some_and(|s| s.discord_absent) {
            dropped.push((c.speaker_id, c.score, Dropped::HardAbsent));
            continue;
        }
        let foreign = st.is_some_and(|s| is_foreign(cfg, s));
        surviving.push((c, foreign));
    }

    // The bar a foreign candidate has to clear on its own, and the one it has
    // to clear against the best voice that actually belongs to this source.
    let raised = cfg.label_threshold + cfg.foreign_source_margin;
    let best_native = surviving
        .iter()
        .filter(|(_, foreign)| !*foreign)
        .map(|(c, _)| c.score)
        .fold(f32::NEG_INFINITY, f32::max);

    let mut kept = Vec::with_capacity(surviving.len());
    let mut foreign_kept = Vec::new();
    for (c, foreign) in surviving {
        if !foreign {
            kept.push(*c);
            continue;
        }
        if c.score < raised {
            dropped.push((c.speaker_id, c.score, Dropped::ForeignBelowBar));
            continue;
        }
        if best_native.is_finite() && c.score < best_native + cfg.enroll_margin {
            dropped.push((c.speaker_id, c.score, Dropped::ForeignBehindNative));
            continue;
        }
        foreign_kept.push(c.speaker_id);
        kept.push(*c);
    }

    Applied {
        kept,
        dropped,
        foreign_kept,
    }
}

// ---- the gathering half ---------------------------------------------------
//
// Everything above this line is pure. Everything below it reads the store and
// hands the pure half a `Standing` per candidate — which is the whole reason
// the split exists: the operating point is testable exhaustively without a
// database, and the queries are testable on a seeded one.

/// The two pattern lists the family test needs, carried together so the
/// callers — the analyser, the audit and the bench — cannot disagree about
/// which config field is which.
#[derive(Debug, Clone, Copy)]
pub struct Sources<'a> {
    /// `[truth].sources`. Reused rather than duplicated into `[identity]`:
    /// there must be exactly one answer to "which capture source is Discord",
    /// and the truth subsystem already owns that question.
    pub discord: &'a [String],
    /// `[identity].vrchat_sources`.
    pub vrchat: &'a [String],
}

/// Gather what the rules need about every candidate for one segment.
///
/// Three queries at most, and the last two only when the segment's source has
/// hard or soft presence evidence behind it. A candidate the store says
/// nothing about comes back as native, because "not known" is not "not here".
pub fn standings_for_segment(
    store: &Store,
    cfg: &IdentityConfig,
    sources: Sources<'_>,
    seg: &SegmentSource,
    candidates: &[i64],
) -> Result<Vec<Standing>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let family = family_of(
        &seg.match_key,
        &seg.display_name,
        sources.discord,
        sources.vrchat,
    );
    let you = store.you_speaker_id()?;
    let counts = store.source_standings(seg.source_id)?;

    // Rule 2's evidence, gathered once for the whole candidate list. Skipped
    // entirely off Discord and when the rule is off, so an install that never
    // ran the bridge pays for none of it.
    let spoke_nearby = if cfg.presence_hard && family == SourceFamily::Discord {
        Some(store.discord_users_speaking_between(
            seg.t_start_ns - PRESENCE_REACH_NS,
            seg.t_end_ns + PRESENCE_REACH_NS,
        )?)
    } else {
        None
    };
    // Rule 3's, likewise.
    let roster_nearby = if family == SourceFamily::VrChat {
        Some(store.roster_names_between(
            seg.t_start_ns - ROSTER_REACH_NS,
            seg.t_end_ns + ROSTER_REACH_NS,
        )?)
    } else {
        None
    };

    let mut out = Vec::with_capacity(candidates.len());
    for &speaker_id in candidates {
        let counted = counts.iter().find(|s| s.speaker_id == speaker_id);
        let is_you = you == Some(speaker_id);

        // "Nobody spoke anywhere near here" means the plugin was off, not that
        // the room was empty — so an empty set disarms the rule rather than
        // condemning everybody in it.
        let discord_absent = match &spoke_nearby {
            Some(spoke) if !spoke.is_empty() && !is_you => {
                let linked = store.discord_user_ids_for_speaker(speaker_id)?;
                !linked.is_empty() && !linked.iter().any(|u| spoke.contains(u))
            }
            _ => false,
        };
        // Same guard, same reason: an empty roster window is a log that was not
        // being read, and a voice with no name has nothing to look up.
        let roster_absent = match &roster_nearby {
            Some(present) if !present.is_empty() && !is_you => {
                match store.named_speaker_name(speaker_id)? {
                    Some(name) => !present.contains(&name),
                    None => false,
                }
            }
            _ => false,
        };

        out.push(Standing {
            speaker_id,
            on_source: counted.map(|s| s.on_source).unwrap_or(0),
            total: counted.map(|s| s.total).unwrap_or(0),
            is_you,
            discord_absent,
            roster_absent,
        });
    }
    Ok(out)
}

/// `standings_for_segment` then [`apply`], for the one caller that wants both:
/// the analysis leg, which has a segment id and a ranked list and no interest
/// in the middle.
pub fn for_segment(
    store: &Store,
    cfg: &IdentityConfig,
    sources: Sources<'_>,
    segment_id: i64,
    ranked: &[Candidate],
) -> Result<Applied> {
    if !cfg.source_prior || ranked.is_empty() {
        return Ok(Applied::untouched(ranked));
    }
    let Some(seg) = store.segment_source(segment_id)? else {
        return Ok(Applied::untouched(ranked));
    };
    let ids: Vec<i64> = ranked.iter().map(|c| c.speaker_id).collect();
    let standings = standings_for_segment(store, cfg, sources, &seg, &ids)?;
    Ok(apply(cfg, &standings, ranked))
}

// ---- the audit ------------------------------------------------------------
//
// `recalld identity audit`. A report and nothing else: it reads, it prints, it
// changes not one row.
//
// **How a past label is judged foreign.** Not by today's counts — a voice that
// won ten VRChat segments has ten turns of VRChat history today, and asking
// "does it have history there" of a row that IS that history answers itself.
// The audit instead replays the labels in the order they were made and asks the
// prior's question of each one using only what was known *before* it: had this
// voice ever been heard on this source yet? That is the user's own phrasing —
// "labels that point at voices with zero prior history on this source" — and it
// is the only version of the question that is not circular.
//
// The consequence, stated because it changes how the number reads: a *run* of
// wrong labels is counted once. The first is foreign; by the second the voice
// has history there, put there by the first. `followed_by` says how long each
// run got, so a single flag standing for forty turns is visible rather than
// hidden.

/// One past label the rule would have questioned.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignLabel {
    pub segment_id: i64,
    pub speaker_id: i64,
    pub speaker_name: String,
    pub source: String,
    pub match_score: Option<f64>,
    pub label_via: Option<String>,
    pub t_start_ns: i64,
    /// Labels for the same voice on the same source that came *after* this one.
    /// The size of the run this row opened.
    pub followed_by: i64,
}

/// What `recalld identity audit` found.
#[derive(Debug, Clone, Default)]
pub struct Audit {
    /// Voice × source, in the order `list_speakers` returns the voices.
    pub matrix: Vec<(i64, String, Vec<crate::store::SpeakerSource>)>,
    /// Live labelled turns considered.
    pub considered: i64,
    /// Every label the rule would have questioned, oldest first.
    pub foreign: Vec<ForeignLabel>,
}

/// Read the whole report. No writes, and no model.
pub fn audit(store: &Store, cfg: &IdentityConfig) -> Result<Audit> {
    let speakers = store.list_speakers()?;
    let matrix_by_id = store.speaker_source_matrix()?;
    let sources = store.sources_by_id()?;
    let you = store.you_speaker_id()?;

    let matrix = speakers
        .iter()
        .map(|s| {
            (
                s.id,
                s.name().unwrap_or(&s.auto_label).to_string(),
                matrix_by_id.get(&s.id).cloned().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    let name_of = |id: i64| {
        matrix
            .iter()
            .find(|(sid, ..)| *sid == id)
            .map(|(_, n, _)| n.clone())
            .unwrap_or_else(|| format!("speaker {id}"))
    };

    // The replay. `seen` is (voice, source) → turns before the row being
    // judged; `total` is the voice's turns anywhere before it.
    let rows = store.labelled_segments_in_order()?;
    let mut seen: std::collections::HashMap<(i64, i64), i64> = std::collections::HashMap::new();
    let mut total: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut foreign: Vec<ForeignLabel> = Vec::new();
    for row in &rows {
        let key = (row.speaker_id, row.source_id);
        let before_here = *seen.get(&key).unwrap_or(&0);
        let before_anywhere = *total.get(&row.speaker_id).unwrap_or(&0);
        let is_you = you == Some(row.speaker_id);
        if !is_you && before_here == 0 && before_anywhere >= cfg.foreign_after_segments {
            foreign.push(ForeignLabel {
                segment_id: row.id,
                speaker_id: row.speaker_id,
                speaker_name: name_of(row.speaker_id),
                source: sources
                    .get(&row.source_id)
                    .map(|(key, ..)| key.clone())
                    .unwrap_or_else(|| format!("source {}", row.source_id)),
                match_score: row.match_score,
                label_via: row.label_via.clone(),
                t_start_ns: row.t_start_ns,
                followed_by: 0,
            });
        }
        *seen.entry(key).or_insert(0) += 1;
        *total.entry(row.speaker_id).or_insert(0) += 1;
    }
    // How long each flagged run got: everything on that (voice, source) pair
    // today, minus the row that opened it.
    for f in &mut foreign {
        let here = matrix_by_id
            .get(&f.speaker_id)
            .and_then(|rows| {
                rows.iter()
                    .find(|s| s.match_key == f.source)
                    .map(|s| s.segments)
            })
            .unwrap_or(1);
        f.followed_by = (here - 1).max(0);
    }

    Ok(Audit {
        matrix,
        considered: rows.len() as i64,
        foreign,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Decision, decide};

    fn cfg() -> IdentityConfig {
        IdentityConfig {
            source_prior: true,
            ..IdentityConfig::default()
        }
    }

    fn c(id: i64, score: f32) -> Candidate {
        Candidate {
            speaker_id: id,
            score,
        }
    }

    /// A voice with plenty of history, none of it on this source.
    fn foreign(id: i64) -> Standing {
        Standing {
            speaker_id: id,
            on_source: 0,
            total: 800,
            is_you: false,
            discord_absent: false,
            roster_absent: false,
        }
    }

    /// A voice at home on this source.
    fn native(id: i64) -> Standing {
        Standing {
            speaker_id: id,
            on_source: 120,
            total: 800,
            is_you: false,
            discord_absent: false,
            roster_absent: false,
        }
    }

    // ---- source families -------------------------------------------------

    #[test]
    fn a_source_is_placed_by_either_of_its_names() {
        let d = vec!["discord".to_string(), "vesktop".to_string()];
        let v = vec!["vrchat".to_string()];
        // The live database's own rows: one Discord client whose match key says
        // so, one whose DISPLAY name does not (it reads "Chromium"), and VRChat.
        assert_eq!(
            family_of("Discord", "Chromium", &d, &v),
            SourceFamily::Discord
        );
        assert_eq!(
            family_of("vesktop", "Chromium", &d, &v),
            SourceFamily::Discord
        );
        assert_eq!(
            family_of("VRChat.exe", "VRChat", &d, &v),
            SourceFamily::VrChat
        );
        assert_eq!(family_of("mic", "Microphone", &d, &v), SourceFamily::Other);
        assert_eq!(family_of("firefox", "Firefox", &d, &v), SourceFamily::Other);
    }

    #[test]
    fn an_empty_pattern_matches_nothing_rather_than_everything() {
        // `"".contains("")` is true, so an empty entry in the list would make
        // every source Discord and turn the hard rule on everywhere.
        let d = vec![String::new()];
        assert_eq!(
            family_of("VRChat.exe", "VRChat", &d, &[]),
            SourceFamily::Other
        );
    }

    // ---- the table the whole feature is ----------------------------------

    #[test]
    fn a_native_candidate_is_never_touched() {
        let a = apply(&cfg(), &[native(1)], &[c(1, 0.36)]);
        assert_eq!(a.kept, vec![c(1, 0.36)]);
        assert!(!a.changed());
        assert!(a.note().is_none());
    }

    #[test]
    fn a_foreign_candidate_over_the_raised_bar_survives_alone() {
        // 0.35 + 0.10 = 0.45. Nothing native to lose to.
        let a = apply(&cfg(), &[foreign(1)], &[c(1, 0.45)]);
        assert_eq!(a.kept, vec![c(1, 0.45)]);
        assert_eq!(a.foreign_kept, vec![1]);
        assert!(!a.changed());
    }

    #[test]
    fn a_foreign_candidate_under_the_raised_bar_is_removed_not_demoted() {
        // 0.36 would have been a label at `label_threshold`. It is not one here,
        // and — the point of removing rather than demoting — the ladder is then
        // free to mint a voice that genuinely belongs to this source.
        let a = apply(&cfg(), &[foreign(1)], &[c(1, 0.36)]);
        assert!(a.kept.is_empty());
        assert_eq!(a.dropped, vec![(1, 0.36, Dropped::ForeignBelowBar)]);
        assert_eq!(
            decide(&cfg(), 0.0, 5.0, 5, &a.kept),
            Decision::Mint { best_score: None }
        );
    }

    #[test]
    fn a_foreign_candidate_must_also_beat_the_best_native_by_the_margin() {
        // Over the raised bar, ahead of the native voice, but only by 0.02 —
        // under `enroll_margin` (0.06). The native voice wins.
        let a = apply(&cfg(), &[foreign(1), native(2)], &[c(1, 0.50), c(2, 0.48)]);
        assert_eq!(a.kept, vec![c(2, 0.48)]);
        assert_eq!(a.dropped, vec![(1, 0.50, Dropped::ForeignBehindNative)]);
        assert!(matches!(
            decide(&cfg(), 0.0, 5.0, 5, &a.kept),
            Decision::Matched { speaker_id: 2, .. }
        ));

        // Clear of it by 0.06 exactly: inclusive, and the foreign voice wins.
        let a = apply(&cfg(), &[foreign(1), native(2)], &[c(1, 0.54), c(2, 0.48)]);
        assert_eq!(a.kept, vec![c(1, 0.54), c(2, 0.48)]);
        assert_eq!(a.foreign_kept, vec![1]);
    }

    #[test]
    fn a_voice_with_too_little_history_is_not_foreign_anywhere() {
        // Nineteen turns and none on this source: not enough history for its
        // absence to mean anything (`foreign_after_segments` is 20).
        let young = Standing {
            speaker_id: 1,
            on_source: 0,
            total: 19,
            ..foreign(1)
        };
        assert!(!is_foreign(&cfg(), &young));
        assert_eq!(
            apply(&cfg(), std::slice::from_ref(&young), &[c(1, 0.36)]).kept,
            vec![c(1, 0.36)]
        );

        let old = Standing { total: 20, ..young };
        assert!(is_foreign(&cfg(), &old));
    }

    #[test]
    fn the_you_voice_is_native_everywhere() {
        let you = Standing {
            is_you: true,
            ..foreign(1)
        };
        assert!(!is_foreign(&cfg(), &you));
        assert_eq!(apply(&cfg(), &[you], &[c(1, 0.36)]).kept, vec![c(1, 0.36)]);
    }

    #[test]
    fn a_voice_nothing_is_known_about_is_treated_as_native() {
        let a = apply(&cfg(), &[], &[c(7, 0.36)]);
        assert_eq!(a.kept, vec![c(7, 0.36)]);
    }

    // ---- the hard rule ---------------------------------------------------

    #[test]
    fn discord_saying_they_were_not_there_excludes_them_at_any_score() {
        let absent = Standing {
            discord_absent: true,
            ..native(1)
        };
        let a = apply(&cfg(), &[absent, native(2)], &[c(1, 0.99), c(2, 0.40)]);
        assert_eq!(a.kept, vec![c(2, 0.40)]);
        assert_eq!(a.dropped, vec![(1, 0.99, Dropped::HardAbsent)]);
    }

    #[test]
    fn an_excluded_candidate_cannot_stand_in_as_the_native_to_beat() {
        // Without the two passes, the excluded voice at 0.99 would still be the
        // "best native" and would take the foreign candidate down with it.
        let absent = Standing {
            discord_absent: true,
            ..native(1)
        };
        let a = apply(&cfg(), &[absent, foreign(2)], &[c(1, 0.99), c(2, 0.50)]);
        assert_eq!(a.kept, vec![c(2, 0.50)]);
        assert_eq!(a.dropped, vec![(1, 0.99, Dropped::HardAbsent)]);
    }

    #[test]
    fn the_hard_rule_can_be_turned_off_without_touching_the_soft_one() {
        let cfg = IdentityConfig {
            presence_hard: false,
            ..cfg()
        };
        let absent = Standing {
            discord_absent: true,
            ..native(1)
        };
        let a = apply(&cfg, &[absent, foreign(2)], &[c(1, 0.99), c(2, 0.50)]);
        assert_eq!(a.kept, vec![c(1, 0.99)]);
        assert_eq!(a.dropped, vec![(2, 0.50, Dropped::ForeignBehindNative)]);
    }

    // ---- the roster rule -------------------------------------------------

    #[test]
    fn a_name_missing_from_the_roster_is_soft_not_excluded() {
        // The asymmetry, in one assertion: the same shape of evidence that
        // EXCLUDES on Discord only raises the bar here. At 0.99 the voice still
        // wins; at 0.36 it would not.
        let away = Standing {
            roster_absent: true,
            ..native(1)
        };
        let a = apply(&cfg(), std::slice::from_ref(&away), &[c(1, 0.99)]);
        assert_eq!(a.kept, vec![c(1, 0.99)]);
        assert_eq!(a.foreign_kept, vec![1]);

        let a = apply(&cfg(), &[away], &[c(1, 0.36)]);
        assert!(a.kept.is_empty());
        assert_eq!(a.dropped, vec![(1, 0.36, Dropped::ForeignBelowBar)]);
    }

    #[test]
    fn the_roster_overrides_a_history_that_says_the_voice_belongs_here() {
        // 120 turns on this very source, and still foreign for this ONE turn:
        // the roster is about now, the history is about before.
        let away = Standing {
            roster_absent: true,
            ..native(1)
        };
        assert!(is_foreign(&cfg(), &away));
    }

    // ---- the switch ------------------------------------------------------

    #[test]
    fn with_the_prior_off_nothing_is_ever_dropped() {
        let cfg = IdentityConfig {
            source_prior: false,
            ..cfg()
        };
        let absent = Standing {
            discord_absent: true,
            roster_absent: true,
            ..foreign(1)
        };
        let a = apply(&cfg, &[absent], &[c(1, 0.36)]);
        assert_eq!(a.kept, vec![c(1, 0.36)]);
        assert!(!a.changed());
    }

    // ---- the gathering half, against a real store ------------------------

    mod gathered {
        use super::*;
        use crate::store::{KIND_APP, Store, label_via};

        const SEC: i64 = 1_000_000_000;

        /// A store with a Discord source, a VRChat source, a named voice with
        /// plenty of Discord history and a linked account, and one turn on each
        /// source to ask about.
        struct Rig {
            store: Store,
            voice: i64,
            discord_seg: i64,
            vrchat_seg: i64,
        }

        fn rig() -> Rig {
            let store = Store::open_in_memory().unwrap();
            let discord = store
                .upsert_source_kind("Discord", "Chromium", KIND_APP, 0)
                .unwrap();
            let vrchat = store
                .upsert_source_kind("VRChat.exe", "VRChat", KIND_APP, 0)
                .unwrap();
            let sd = store.begin_session(discord, 0).unwrap();
            let sv = store.begin_session(vrchat, 0).unwrap();

            let voice = store.create_speaker("Rowan", 0).unwrap();
            store.rename_speaker(voice, "Rowan", 0).unwrap();
            store.upsert_discord_user("u1", "Rowan", 0).unwrap();
            store
                .set_discord_link("u1", Some(voice), Some("manual"), 0)
                .unwrap();

            // Thirty Discord turns, so the voice is well past
            // `foreign_after_segments` everywhere.
            for i in 0..30 {
                let at = i * 10 * SEC;
                let id = store.insert_segment(sd, at, at + SEC, "x.wav", at).unwrap();
                store
                    .set_segment_speaker_via(id, Some(voice), Some(0.9), Some(label_via::MATCH))
                    .unwrap();
            }
            let base = 10_000 * SEC;
            let discord_seg = store
                .insert_segment(sd, base, base + SEC, "x.wav", base)
                .unwrap();
            let vrchat_seg = store
                .insert_segment(sv, base, base + SEC, "x.wav", base)
                .unwrap();
            Rig {
                store,
                voice,
                discord_seg,
                vrchat_seg,
            }
        }

        fn sources() -> (Vec<String>, Vec<String>) {
            (vec!["discord".into()], vec!["vrchat".into()])
        }

        fn gather(r: &Rig, segment: i64, cfg: &IdentityConfig) -> Vec<Standing> {
            let (d, v) = sources();
            let seg = r.store.segment_source(segment).unwrap().unwrap();
            standings_for_segment(
                &r.store,
                cfg,
                Sources {
                    discord: &d,
                    vrchat: &v,
                },
                &seg,
                &[r.voice],
            )
            .unwrap()
        }

        #[test]
        fn a_voice_at_home_on_discord_is_foreign_to_vrchat_and_not_to_discord() {
            let r = rig();
            let cfg = cfg();
            assert!(!is_foreign(&cfg, &gather(&r, r.discord_seg, &cfg)[0]));
            let away = &gather(&r, r.vrchat_seg, &cfg)[0];
            assert_eq!((away.on_source, away.total), (0, 30));
            assert!(is_foreign(&cfg, away));
        }

        #[test]
        fn discord_silence_excludes_a_linked_account_but_only_with_data_to_read() {
            let r = rig();
            let cfg = cfg();
            // No truth data at all: the plugin was off, and off is not absent.
            assert!(!gather(&r, r.discord_seg, &cfg)[0].discord_absent);

            // Somebody else spoke through the window; our account did not.
            let base = 10_000 * SEC;
            r.store
                .truth_speaking_start("u2", "Somebody", None, base - 60 * SEC)
                .unwrap();
            r.store.truth_speaking_stop("u2", base - 30 * SEC).unwrap();
            assert!(gather(&r, r.discord_seg, &cfg)[0].discord_absent);

            // And once the account itself is heard, the rule stands down.
            r.store
                .truth_speaking_start("u1", "Rowan", None, base - 10 * SEC)
                .unwrap();
            r.store.truth_speaking_stop("u1", base).unwrap();
            assert!(!gather(&r, r.discord_seg, &cfg)[0].discord_absent);
        }

        #[test]
        fn discord_evidence_never_reaches_a_vrchat_segment() {
            // The asymmetry's other half: Discord's speaking events say nothing
            // about who is audible in a VRChat instance, so the hard rule is
            // not merely soft there — it does not run.
            let r = rig();
            let cfg = cfg();
            let base = 10_000 * SEC;
            r.store
                .truth_speaking_start("u2", "Somebody", None, base - 60 * SEC)
                .unwrap();
            r.store.truth_speaking_stop("u2", base - 30 * SEC).unwrap();
            assert!(!gather(&r, r.vrchat_seg, &cfg)[0].discord_absent);
        }

        #[test]
        fn a_roster_that_was_never_read_accuses_nobody() {
            let r = rig();
            let cfg = cfg();
            assert!(!gather(&r, r.vrchat_seg, &cfg)[0].roster_absent);
        }

        #[test]
        fn a_name_absent_from_a_live_roster_is_marked_and_a_present_one_is_not() {
            let r = rig();
            let cfg = cfg();
            let base = 10_000 * SEC;
            r.store
                .roster_join(None, None, "Somebody Else", base - 60 * SEC)
                .unwrap();
            assert!(gather(&r, r.vrchat_seg, &cfg)[0].roster_absent);

            r.store
                .roster_join(None, None, "Rowan", base - 30 * SEC)
                .unwrap();
            assert!(!gather(&r, r.vrchat_seg, &cfg)[0].roster_absent);
        }

        #[test]
        fn an_unnamed_voice_has_no_name_for_the_roster_to_miss() {
            let r = rig();
            let cfg = cfg();
            let anon = r.store.create_speaker("Speaker_99", 0).unwrap();
            let base = 10_000 * SEC;
            r.store
                .roster_join(None, None, "Somebody Else", base - 60 * SEC)
                .unwrap();
            let (d, v) = sources();
            let seg = r.store.segment_source(r.vrchat_seg).unwrap().unwrap();
            let st = standings_for_segment(
                &r.store,
                &cfg,
                Sources {
                    discord: &d,
                    vrchat: &v,
                },
                &seg,
                &[anon],
            )
            .unwrap();
            assert!(!st[0].roster_absent, "not knowing is not not-there");
        }

        #[test]
        fn for_segment_is_the_two_halves_together_and_is_off_when_the_prior_is() {
            let r = rig();
            let ranked = [c(r.voice, 0.36)];
            let (d, v) = sources();
            let s = Sources {
                discord: &d,
                vrchat: &v,
            };
            // On: foreign to VRChat at 0.36, so it goes.
            let a = for_segment(&r.store, &cfg(), s, r.vrchat_seg, &ranked).unwrap();
            assert!(a.kept.is_empty());
            // Off: the ladder sees exactly what 0.10.0 handed it.
            let a = for_segment(
                &r.store,
                &IdentityConfig::default(),
                s,
                r.vrchat_seg,
                &ranked,
            )
            .unwrap();
            assert_eq!(a.kept, ranked.to_vec());
        }
    }

    #[test]
    fn the_note_names_every_voice_it_took_out_and_why() {
        let a = apply(&cfg(), &[foreign(1), foreign(2)], &[c(1, 0.40), c(2, 0.38)]);
        let note = a.note().unwrap();
        assert!(note.contains("1 @ 0.400"), "{note}");
        assert!(note.contains("2 @ 0.380"), "{note}");
        assert!(note.contains("foreign to this source"), "{note}");
    }
}
