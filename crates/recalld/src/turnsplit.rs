//! Cutting one turn into two where the person talking changes.
//!
//! A turn ends at silence and nowhere else (`crate::turns`), so a fast exchange
//! — "yeah" / "no it isn't" across half a second — is one row with one label.
//! Discord's own spans say how often: on this install 409 of 515 `overlap`
//! turns contain at least one point where the solo speaker changes, 594 change
//! points in all (FINDINGS §39).
//!
//! The detector is the cheapest thing that could work, and it is deliberately
//! *not* a diarizer: slide the daemon's own ERes2Net over the turn, compare the
//! window ending at `t` with the window starting at `t`, and cut at the biggest
//! disagreement — if it is big enough, and if neither piece would come out
//! shorter than the identity ladder's own floor.
//!
//! Three refusals shape it, and each one exists because a false split is worse
//! than a missed one. A missed change leaves the row exactly as it is today; a
//! false split **duplicates a row** — two transcripts where there was one
//! utterance, two rows in the digest, two chances to enrol half a voice.
//!
//! * **A turn too short to yield two labellable pieces is never cut.** Below
//!   `2 * min_piece` there is no cut that leaves both sides scorable, and a
//!   piece the ladder must refuse is a row with no speaker where there used to
//!   be one.
//! * **A handful of peaks per turn, strongest first.** The curve has a peak
//!   everywhere the voice merely changes register; taking every local maximum
//!   over the bar shreds a long turn. Peaks are taken in order of strength, no
//!   two closer together than a piece, and the count is capped — measured at
//!   three, where the recall is still rising and the false-split rate has not
//!   moved (FINDINGS §39).
//! * **The bar is a *distance*, not a likelihood.** No threshold is learned
//!   here and none is fitted per voice. It is one number, measured once against
//!   Discord's spans, and it is high enough that the false-split rate on turns
//!   Discord says are one person stays under 1%.
//!
//! Pure: no model, no I/O, no database. The caller hands in the window
//! embeddings it already had to compute and gets back sample ranges.

use anyhow::{Context, Result};

use crate::embed::Embedding;

/// Where one window sits in the turn's samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from: usize,
    pub to: usize,
}

/// One piece of a cut turn: a half-open range of the turn's own samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    pub from: usize,
    pub to: usize,
}

impl Piece {
    pub fn len(&self) -> usize {
        self.to.saturating_sub(self.from)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The detector's operating point, in samples, so nothing here has to know the
/// sample rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shape {
    /// Length of each comparison window.
    pub window: usize,
    /// Distance between window starts.
    pub hop: usize,
    /// No piece may be shorter than this.
    pub min_piece: usize,
    /// `1 - cos` a boundary must reach before it is a cut at all.
    pub distance: f32,
    /// At most this many cuts in one turn.
    pub max_cuts: usize,
}

impl Shape {
    /// The shape a config asks for, at a sample rate.
    pub fn from_config(cfg: &crate::config::IdentityConfig, rate: u32) -> Self {
        let s = |secs: f32| (secs.max(0.0) * rate as f32) as usize;
        Self {
            window: s(cfg.split_turn_window_s).max(1),
            hop: s(cfg.split_turn_hop_s).max(1),
            min_piece: s(cfg.split_turn_min_piece_s).max(1),
            distance: cfg.split_turn_distance,
            max_cuts: cfg.split_turn_max_cuts,
        }
    }
}

impl Shape {
    /// The windows to embed, left to right. Empty when the turn is shorter
    /// than one window — there is nothing to compare it against.
    pub fn windows_of(&self, samples: usize) -> Vec<Window> {
        let (w, hop) = (self.window.max(1), self.hop.max(1));
        if samples < w {
            return Vec::new();
        }
        (0..=(samples - w))
            .step_by(hop)
            .map(|from| Window { from, to: from + w })
            .collect()
    }

    /// Is this turn long enough that a cut could leave two labellable pieces?
    pub fn cuttable(&self, samples: usize) -> bool {
        self.max_cuts > 0 && samples >= 2 * self.min_piece && samples >= self.window + self.hop
    }
}

/// `1 - cos` between the window ending at a boundary and the one starting
/// there, for every boundary the hop grid can express.
///
/// The two windows never overlap, which is the whole point: overlapping halves
/// share the audio that would tell them apart, and the curve flattens exactly
/// where the answer is.
pub fn curve(
    shape: &Shape,
    windows: &[Window],
    vectors: &[Embedding],
) -> Result<Vec<(usize, f32)>> {
    debug_assert_eq!(windows.len(), vectors.len());
    let step = shape.window.div_ceil(shape.hop.max(1));
    if windows.len() <= step || vectors.len() <= step {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(windows.len() - step);
    for i in 0..(windows.len() - step) {
        let d = 1.0 - vectors[i].cosine(&vectors[i + step])?;
        // The boundary is where the right window starts. With `step` chosen so
        // the two are adjacent that is also where the left one ends, and when
        // the hop does not divide the window it is the earlier of the two —
        // the conservative reading, because it never claims the cut is later
        // than the audio supports.
        out.push((windows[i + step].from, d));
    }
    Ok(out)
}

/// Where to cut, ascending. Empty when the turn stands as it is.
///
/// Boundaries at or above `shape.distance`, taken strongest first, no two
/// closer together than a piece and none closer than a piece to either edge,
/// stopping at `shape.max_cuts`. Ties go to the earlier boundary so the answer
/// never depends on how a float happened to round.
pub fn cuts(shape: &Shape, samples: usize, curve: &[(usize, f32)]) -> Vec<usize> {
    if !shape.cuttable(samples) {
        return Vec::new();
    }
    let mut cand: Vec<(usize, f32)> = curve
        .iter()
        .copied()
        .filter(|(at, d)| {
            *d >= shape.distance && *at >= shape.min_piece && *at <= samples - shape.min_piece
        })
        .collect();
    // Strongest first; equal strengths earliest first.
    cand.sort_by(|(a_at, a), (b_at, b)| {
        b.partial_cmp(a)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a_at.cmp(b_at))
    });
    let mut taken: Vec<usize> = Vec::new();
    for (at, _) in cand {
        if taken.len() >= shape.max_cuts {
            break;
        }
        if taken.iter().all(|t| at.abs_diff(*t) >= shape.min_piece) {
            taken.push(at);
        }
    }
    taken.sort_unstable();
    taken
}

/// The pieces a turn becomes. One piece when there is no cut.
///
/// Always at least one piece, and the pieces always tile the turn exactly —
/// the caller may hand any of them to the transcriber and to the embedder
/// knowing that between them they hold every sample and every word.
pub fn pieces(samples: usize, at: &[usize]) -> Vec<Piece> {
    let mut edges: Vec<usize> = at
        .iter()
        .copied()
        .filter(|a| *a > 0 && *a < samples)
        .collect();
    edges.sort_unstable();
    edges.dedup();
    let mut out = Vec::with_capacity(edges.len() + 1);
    let mut from = 0usize;
    for e in edges {
        out.push(Piece { from, to: e });
        from = e;
    }
    out.push(Piece { from, to: samples });
    out
}

/// Each piece with the second range its words are taken from.
///
/// The outer edges are opened deliberately — the first piece takes everything
/// from the start of time and the last everything to the end of it — so that
/// the spans **partition the word list** rather than merely covering the
/// samples. `words_in_span` keeps a word whose start is `>= from` and `< to`;
/// with closed edges a word timestamped a rounding error past the final sample
/// would belong to no piece, and a split that loses a word is not a split, it
/// is a deletion.
pub fn spans(pieces: &[Piece], rate: u32) -> Vec<(Piece, f32, f32)> {
    let secs = |n: usize| n as f32 / rate as f32;
    let last = pieces.len().saturating_sub(1);
    pieces
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let from = if i == 0 {
                f32::NEG_INFINITY
            } else {
                secs(p.from)
            };
            let to = if i == last { f32::INFINITY } else { secs(p.to) };
            (*p, from, to)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// the archive pass
// ---------------------------------------------------------------------------

/// One turn, decoded once and cut where the person talking changes.
///
/// `pieces.len() == 1` is the ordinary answer and the only one
/// `[identity].split_turns = false` can give. The pieces always tile the turn
/// and their transcripts always partition its words.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub pieces: Vec<(Piece, String)>,
    /// A cut was found and then refused because it left a piece with no words.
    /// Reported rather than swallowed: it is a fifth of everything the
    /// detector finds on this archive, and a number that large belongs in the
    /// operator's table rather than in a silent `continue`.
    pub wordless: bool,
}

impl Plan {
    pub fn cut(&self) -> bool {
        self.pieces.len() > 1
    }
}

/// **A piece that says nothing is not a turn.**
///
/// The detector reads the voice and not the words, so it will happily cut a
/// laugh, a breath or a two-second "yeah" in half. The archive says how often:
/// on a real preview of this install, cuts that left a wordless piece were a
/// fifth of everything the detector found (FINDINGS §39).
///
/// The whole split is refused rather than the offending cut, which is the same
/// bet [`crate::split`] makes about k-means: if all the words sit on one side,
/// the evidence for two turns is not there, whichever boundary scored highest.
pub fn every_piece_speaks(pieces: &[(Piece, String)]) -> bool {
    pieces
        .iter()
        .all(|(_, t)| !crate::asr::normalise_words(t).is_empty())
}

/// The operation one applied resplit writes, one per turn it cut.
pub const OP_RESPLIT: &str = "turns.resplit";

/// One archive turn the pass looked at.
#[derive(Debug, Clone, PartialEq)]
pub struct Cut {
    pub segment_id: i64,
    pub t_start_ns: i64,
    pub verdict: String,
    /// Seconds into the turn, ascending.
    pub at_s: Vec<f32>,
    /// What each piece says, in order. `pieces.len() == at_s.len() + 1`.
    pub said: Vec<String>,
    /// The row that stays and the rows that are new. Empty until `--apply`.
    pub new_segment_ids: Vec<i64>,
}

/// What one run did, or would do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub examined: usize,
    /// Turns whose clip retention has already taken. Counted rather than
    /// skipped silently: on an archive older than `[retention].audio_days`
    /// this is the whole answer, and an operator should be told that rather
    /// than shown an empty table.
    pub no_audio: usize,
    /// Turns too short to hold two labellable pieces.
    pub too_short: usize,
    /// Turns where a cut was found and refused because it left a piece with
    /// no words.
    pub wordless: usize,
    pub cuts: Vec<Cut>,
    pub applied: bool,
}

impl Report {
    /// New rows this run made, or would make.
    pub fn new_rows(&self) -> usize {
        self.cuts.iter().map(|c| c.at_s.len()).sum()
    }

    pub fn to_json(&self, at_ns: i64) -> serde_json::Value {
        serde_json::json!({
            "at_ns": at_ns,
            "examined": self.examined,
            "no_audio": self.no_audio,
            "too_short": self.too_short,
            "wordless": self.wordless,
            "turns_cut": self.cuts.len(),
            "new_rows": self.new_rows(),
            "applied": self.applied,
        })
    }
}

/// What the pass needs of one turn, gathered under the store lock and judged
/// without it — [`crate::quality`]'s rule, and for the same reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub segment_id: i64,
    pub session_id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    pub audio_path: String,
    pub verdict: String,
}

/// Where each piece of a cut turn begins and ends on the session's clock.
///
/// The turn's own `t_start_ns` plus the piece's offset at the sample rate:
/// the stored clip is exactly `[t_start_ns, t_end_ns)` of audio (the archive
/// says so — every clip's frame count matches its row to the sample), so a
/// sample offset *is* a timestamp and no re-anchoring is needed.
pub fn piece_times(t_start_ns: i64, piece: &Piece, rate: u32) -> (i64, i64) {
    let ns = |n: usize| (n as i64 * 1_000_000_000) / rate as i64;
    (t_start_ns + ns(piece.from), t_start_ns + ns(piece.to))
}

/// Where a piece's clip goes. Beside the turn's own file, with the piece
/// index in the name, so the original clip is never overwritten — which is
/// what makes `unsplit` able to put the turn back.
pub fn piece_path(original: &str, index: usize) -> String {
    let p = std::path::Path::new(original);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    let name = format!("{stem}-p{index}.wav");
    match p.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(name).to_string_lossy().into_owned(),
        _ => name,
    }
}

/// Walk the archive's `partial` and `overlap` turns, cutting the ones that
/// change speaker. `apply` false computes everything and writes nothing.
///
/// The lock discipline is [`crate::quality`]'s and it is not negotiable:
/// gather under the store lock, run the models without it, write under it
/// again. A worker that held the mutex across a model call blocked the capture
/// pipeline's inserts once already, and the evening it happened has 128
/// dropped-buffer warnings to show for it.
pub fn resplit(
    store: &std::sync::Mutex<crate::store::Store>,
    analyzer: &mut crate::analysis::Analyzer,
    stats: &crate::analysis::AnalysisStats,
    data_dir: &std::path::Path,
    limit: usize,
    apply: bool,
    at_utc_ns: i64,
) -> Result<Report> {
    let candidates = {
        let guard = store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?;
        guard.segments_for_resplit(limit)?
    };
    let mut report = Report {
        examined: candidates.len(),
        applied: apply,
        ..Default::default()
    };
    let rate = crate::config::SAMPLE_RATE;

    for c in candidates {
        // ---- no lock: read the clip and run the models ----
        let Ok(samples) = crate::ingest::read_wav(&data_dir.join(&c.audio_path)) else {
            report.no_audio += 1;
            continue;
        };
        if samples.is_empty() {
            report.no_audio += 1;
            continue;
        }
        let plan = analyzer.plan_split(&samples)?;
        if plan.wordless {
            report.wordless += 1;
        }
        if !plan.cut() {
            if !Shape::from_config(analyzer.identity_config(), rate).cuttable(samples.len()) {
                report.too_short += 1;
            }
            continue;
        }
        let plan = plan.pieces;

        let mut cut = Cut {
            segment_id: c.segment_id,
            t_start_ns: c.t_start_ns,
            verdict: c.verdict.clone(),
            at_s: plan
                .iter()
                .skip(1)
                .map(|(p, _)| p.from as f32 / rate as f32)
                .collect(),
            said: plan.iter().map(|(_, t)| t.clone()).collect(),
            new_segment_ids: Vec::new(),
        };
        if !apply {
            report.cuts.push(cut);
            continue;
        }

        // ---- write: the clips first, the rows second ----
        //
        // A clip with no row is a stray file the next retention sweep
        // reconciles away; a row with no clip is a turn nobody can ever
        // re-read. If one of the two has to exist first it is the file.
        let mut rows = Vec::with_capacity(plan.len());
        for (i, (piece, _)) in plan.iter().enumerate() {
            let rel = piece_path(&c.audio_path, i);
            crate::pipeline::write_wav(&data_dir.join(&rel), &samples[piece.from..piece.to])
                .with_context(|| format!("writing {rel}"))?;
            let (start, end) = piece_times(c.t_start_ns, piece, rate);
            rows.push((start, end, rel));
        }
        let minted = {
            let guard = store
                .lock()
                .map_err(|_| anyhow::anyhow!("store poisoned"))?;
            guard.resplit_segment(c.segment_id, &rows, at_utc_ns)?
        };
        if minted.is_empty() {
            // The row moved under us. The clips written above are strays and
            // retention reconciles them; nothing has been damaged.
            continue;
        }
        let mut ids = vec![c.segment_id];
        ids.extend(&minted);
        cut.new_segment_ids = minted;

        // ---- the analysis leg, per piece, exactly as the live path runs it ----
        for (id, (piece, said)) in ids.iter().zip(&plan) {
            crate::analysis::analyse_or_log(
                analyzer,
                store,
                stats,
                *id,
                &samples[piece.from..piece.to],
                Some(said.clone()),
                at_utc_ns,
            );
        }
        report.cuts.push(cut);
    }
    Ok(report)
}

/// Undo the most recent applied resplits, newest first.
pub fn unsplit(
    store: &std::sync::Mutex<crate::store::Store>,
    limit: usize,
    at_utc_ns: i64,
) -> Result<usize> {
    let ops = {
        let guard = store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?;
        guard.resplit_operations(limit)?
    };
    let mut undone = 0;
    for op in ops {
        let Ok(prior) = serde_json::from_str::<serde_json::Value>(&op.prior_state) else {
            continue;
        };
        let guard = store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?;
        if guard.unsplit_segment(&prior, at_utc_ns)? {
            undone += 1;
        }
    }
    Ok(undone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::{Word, words_in_span};

    const RATE: usize = 16_000;

    fn shape() -> Shape {
        Shape {
            window: RATE,
            hop: RATE / 4,
            min_piece: RATE,
            distance: 0.5,
            max_cuts: 3,
        }
    }

    fn e(v: &[f32]) -> Embedding {
        Embedding::new("m@1", v.to_vec())
    }

    /// `n` windows, the first `flip` of them one voice and the rest another.
    fn two_voices(n: usize, flip: usize) -> Vec<Embedding> {
        (0..n)
            .map(|i| {
                if i < flip {
                    e(&[1.0, 0.0])
                } else {
                    e(&[0.0, 1.0])
                }
            })
            .collect()
    }

    /// The curve for a turn whose windows change voice at index `flip`.
    fn curve_of(s: &Shape, samples: usize, flip: usize) -> Vec<(usize, f32)> {
        let w = s.windows_of(samples);
        curve(s, &w, &two_voices(w.len(), flip)).unwrap()
    }

    #[test]
    fn a_turn_shorter_than_one_window_has_nothing_to_compare() {
        assert!(shape().windows_of(RATE / 2).is_empty());
        assert!(!shape().cuttable(RATE / 2));
    }

    #[test]
    fn windows_tile_the_turn_at_the_hop() {
        let w = shape().windows_of(2 * RATE);
        assert_eq!(w.first(), Some(&Window { from: 0, to: RATE }));
        assert_eq!(w.len(), 5, "0.00 0.25 0.50 0.75 1.00 s");
        assert_eq!(
            w.last(),
            Some(&Window {
                from: RATE,
                to: 2 * RATE
            })
        );
        for pair in w.windows(2) {
            assert_eq!(pair[1].from - pair[0].from, RATE / 4);
        }
    }

    #[test]
    fn the_compared_windows_are_adjacent_and_never_overlap() {
        let s = shape();
        let w = s.windows_of(4 * RATE);
        let v: Vec<_> = (0..w.len()).map(|_| e(&[1.0, 0.0])).collect();
        let c = curve(&s, &w, &v).unwrap();
        let step = s.window / s.hop;
        for (i, (at, _)) in c.iter().enumerate() {
            assert_eq!(*at, w[i + step].from);
            assert_eq!(*at, w[i].to, "the left window ends where the right begins");
        }
    }

    #[test]
    fn one_voice_all_the_way_through_is_never_cut() {
        let s = shape();
        let w = s.windows_of(4 * RATE);
        let v: Vec<_> = (0..w.len()).map(|_| e(&[1.0, 0.01])).collect();
        let c = curve(&s, &w, &v).unwrap();
        assert!(c.iter().all(|(_, d)| *d < 0.01), "{c:?}");
        assert!(cuts(&s, 4 * RATE, &c).is_empty());
    }

    #[test]
    fn a_clean_change_is_cut_where_it_happens() {
        let s = shape();
        let samples = 4 * RATE;
        // The voice changes at 2.0 s, which is window index 8.
        let c = curve_of(&s, samples, 8);
        assert_eq!(cuts(&s, samples, &c), vec![2 * RATE]);
        assert_eq!(
            pieces(samples, &cuts(&s, samples, &c)),
            vec![
                Piece {
                    from: 0,
                    to: 2 * RATE
                },
                Piece {
                    from: 2 * RATE,
                    to: samples
                }
            ]
        );
    }

    #[test]
    fn a_change_too_close_to_an_edge_is_refused() {
        let s = shape();
        let samples = 3 * RATE;
        // The only disagreement in the turn is half a second in, and half a
        // second is less than the ladder can label. Cutting there would make a
        // row with no speaker where there used to be one, so there is no cut.
        assert!(cuts(&s, samples, &[(RATE / 2, 1.0)]).is_empty());
        // And the same at the other end.
        assert!(cuts(&s, samples, &[(samples - RATE / 2, 1.0)]).is_empty());
        // Exactly `min_piece` from either edge is allowed: the piece is as
        // short as a piece may be, which is not the same as too short.
        assert_eq!(cuts(&s, samples, &[(RATE, 1.0)]), vec![RATE]);
        assert_eq!(
            cuts(&s, samples, &[(samples - RATE, 1.0)]),
            vec![samples - RATE]
        );
    }

    #[test]
    fn a_change_the_hop_grid_cannot_express_snaps_to_the_nearest_legal_cut() {
        // The voice changes at 0.5 s of a 3 s turn. No boundary exists before
        // 1.0 s — the left window would not fit — so the earliest the detector
        // can put the cut is 1.0 s, and it does rather than losing the change.
        let s = shape();
        let samples = 3 * RATE;
        assert_eq!(cuts(&s, samples, &curve_of(&s, samples, 2)), vec![RATE]);
    }

    #[test]
    fn a_turn_that_cannot_hold_two_pieces_is_never_cut() {
        let s = shape();
        let samples = 2 * RATE - 1;
        assert!(!s.cuttable(samples));
        assert!(cuts(&s, samples, &[(RATE, 1.0)]).is_empty());
    }

    #[test]
    fn the_distance_bar_is_the_whole_decision() {
        let samples = 4 * RATE;
        let s = shape();
        assert_eq!(cuts(&s, samples, &curve_of(&s, samples, 8)).len(), 1);
        let strict = Shape { distance: 1.1, ..s };
        assert!(
            cuts(&strict, samples, &curve_of(&strict, samples, 8)).is_empty(),
            "nothing scores above 1.0"
        );
    }

    #[test]
    fn two_cuts_never_land_closer_together_than_a_piece() {
        let s = shape();
        let samples = 8 * RATE;
        // Every boundary is a maximum: without the spacing rule this would
        // shred the turn into hop-sized crumbs.
        let flat: Vec<(usize, f32)> = (0..30).map(|i| (RATE + i * RATE / 4, 1.0)).collect();
        let got = cuts(&s, samples, &flat);
        assert_eq!(got.len(), s.max_cuts);
        for pair in got.windows(2) {
            assert!(pair[1] - pair[0] >= s.min_piece, "{got:?}");
        }
        for p in pieces(samples, &got) {
            assert!(p.len() >= s.min_piece, "{p:?}");
        }
    }

    #[test]
    fn the_cut_count_is_capped() {
        let flat: Vec<(usize, f32)> = (0..20).map(|i| (RATE + i * RATE, 1.0)).collect();
        for max in 0..=5 {
            let s = Shape {
                max_cuts: max,
                ..shape()
            };
            assert_eq!(cuts(&s, 30 * RATE, &flat).len(), max);
        }
    }

    #[test]
    fn cuts_come_back_in_time_order_however_strong_they_are() {
        let s = shape();
        // The strongest boundary is the last one; the list is still ascending.
        let c = [(2 * RATE, 0.6f32), (5 * RATE, 0.9), (8 * RATE, 1.0)];
        assert_eq!(cuts(&s, 10 * RATE, &c), vec![2 * RATE, 5 * RATE, 8 * RATE]);
    }

    #[test]
    fn the_strongest_peaks_win_when_the_cap_bites() {
        let s = Shape {
            max_cuts: 1,
            ..shape()
        };
        let c = [(2 * RATE, 0.6f32), (5 * RATE, 0.9), (8 * RATE, 0.7)];
        assert_eq!(cuts(&s, 10 * RATE, &c), vec![5 * RATE]);
    }

    #[test]
    fn ties_take_the_earlier_boundary() {
        let s = Shape {
            max_cuts: 1,
            ..shape()
        };
        let c = [(2 * RATE, 0.9f32), (5 * RATE, 0.9), (8 * RATE, 0.9)];
        assert_eq!(cuts(&s, 10 * RATE, &c), vec![2 * RATE]);
    }

    #[test]
    fn the_pieces_tile_the_turn_exactly() {
        for at in [vec![], vec![2 * RATE], vec![RATE, 2 * RATE, 3 * RATE]] {
            let p = pieces(4 * RATE, &at);
            assert_eq!(p.len(), at.len() + 1);
            assert_eq!(p.first().unwrap().from, 0);
            assert_eq!(p.last().unwrap().to, 4 * RATE);
            for pair in p.windows(2) {
                assert_eq!(pair[0].to, pair[1].from, "no sample is lost or doubled");
            }
            assert!(p.iter().all(|x| !x.is_empty()));
        }
    }

    #[test]
    fn a_cut_at_an_edge_is_no_cut_at_all() {
        assert_eq!(pieces(4 * RATE, &[0]).len(), 1);
        assert_eq!(pieces(4 * RATE, &[4 * RATE]).len(), 1);
        assert_eq!(pieces(4 * RATE, &[2 * RATE, 2 * RATE]).len(), 2);
    }

    #[test]
    fn embeddings_from_two_models_never_produce_a_curve() {
        let s = shape();
        let w = s.windows_of(4 * RATE);
        let mut v: Vec<_> = (0..w.len()).map(|_| e(&[1.0, 0.0])).collect();
        v[6] = Embedding::new("other@1", vec![1.0, 0.0]);
        assert!(curve(&s, &w, &v).is_err());
    }

    fn w(text: &str, start_s: f32) -> Word {
        Word {
            text: text.to_string(),
            start_s,
        }
    }

    #[test]
    fn the_pieces_words_partition_the_turns_words() {
        // Every word of the turn lands in exactly one piece, in order, and
        // none is lost or spelled twice. This is the whole argument for taking
        // the words by time instead of decoding each piece again.
        let words = vec![
            w("yeah", 0.10),
            w("no", 1.05),
            w("it", 1.30),
            w("isn't", 1.55),
            w("really", 3.10),
        ];
        let p = pieces(4 * RATE, &[RATE, 3 * RATE]);
        let got: Vec<String> = spans(&p, 16_000)
            .into_iter()
            .map(|(_, from, to)| words_in_span(&words, from, to))
            .collect();
        assert_eq!(got, vec!["yeah", "no it isn't", "really"]);
        assert_eq!(
            got.join(" ").split_whitespace().collect::<Vec<_>>(),
            words.iter().map(|x| x.text.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_word_outside_the_turns_own_samples_still_belongs_to_a_piece() {
        // The decoder's timestamps are frame indices, not sample counts, so a
        // word can be stamped a hair before zero or past the last sample. The
        // outer edges are open so that neither is dropped.
        let words = vec![w("before", -0.01), w("after", 4.02)];
        let p = pieces(4 * RATE, &[2 * RATE]);
        let got: Vec<String> = spans(&p, 16_000)
            .into_iter()
            .map(|(_, from, to)| words_in_span(&words, from, to))
            .collect();
        assert_eq!(got, vec!["before", "after"]);
    }

    #[test]
    fn an_uncut_turn_gets_every_word() {
        let words = vec![w("one", 0.0), w("two", 9.9)];
        let p = pieces(4 * RATE, &[]);
        let (_, from, to) = spans(&p, 16_000)[0];
        assert_eq!(words_in_span(&words, from, to), "one two");
    }

    #[test]
    fn a_split_that_leaves_a_piece_silent_is_refused() {
        let p = |from: usize, to: usize| Piece { from, to };
        assert!(every_piece_speaks(&[
            (p(0, RATE), "yeah".into()),
            (p(RATE, 2 * RATE), "no it isn't".into()),
        ]));
        // The detector heard two voices in a laugh. There is one turn here.
        assert!(!every_piece_speaks(&[
            (p(0, RATE), "yeah".into()),
            (p(RATE, 2 * RATE), String::new()),
        ]));
        // Punctuation is not speech, and neither is whitespace.
        assert!(!every_piece_speaks(&[
            (p(0, RATE), "yeah".into()),
            (p(RATE, 2 * RATE), "  ... ".into()),
        ]));
        // An uncut turn with words is not a split and is never refused.
        assert!(every_piece_speaks(&[(p(0, RATE), "yeah".into())]));
    }

    #[test]
    fn the_shipped_shape_is_the_measured_one() {
        let cfg = crate::config::IdentityConfig::default();
        assert!(!cfg.split_turns, "measured off — FINDINGS §39");
        let s = Shape::from_config(&cfg, 16_000);
        assert_eq!(s.window, 24_000, "1.5 s");
        assert_eq!(s.hop, 4_000, "0.25 s");
        assert_eq!(s.min_piece, 16_000, "the identity gate's own floor");
        assert_eq!(s.distance, 0.85);
        assert_eq!(s.max_cuts, 3);
    }
}
