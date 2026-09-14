//! Conservative waveform proof. No words, voice embeddings, or speaker labels.
const RATE: usize = 16_000;
const BLOCK: usize = 16;
const WINDOW: usize = 512;

fn correlation(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut aa, mut bb, mut ab) = (0.0f64, 0.0f64, 0.0f64);
    for (&a, &b) in a.iter().zip(b) {
        aa += (a as f64).powi(2);
        bb += (b as f64).powi(2);
        ab += a as f64 * b as f64;
    }
    (aa > 1e-9 && bb > 1e-9).then(|| (ab / (aa * bb).sqrt()).clamp(-1.0, 1.0) as f32)
}
fn activity(audio: &[f32]) -> Option<(usize, usize)> {
    let energies: Vec<f32> = audio
        .chunks(WINDOW)
        .map(|chunk| chunk.iter().map(|x| x * x).sum::<f32>() / chunk.len() as f32)
        .collect();
    let peak = energies.iter().copied().fold(0.0, f32::max);
    if peak < 0.000064 {
        return None;
    }
    let threshold = (peak * 0.004).max(0.000004);
    let start = energies.iter().position(|e| *e > threshold)? * WINDOW;
    let end = ((energies.iter().rposition(|e| *e > threshold)? + 1) * WINDOW).min(audio.len());
    let amplitude = audio[start..end]
        .iter()
        .map(|x| x.abs())
        .fold(0.0, f32::max)
        * 0.01;
    let first = start + audio[start..end].iter().position(|x| x.abs() > amplitude)?;
    let last = start
        + audio[start..end]
            .iter()
            .rposition(|x| x.abs() > amplitude)?
        + 1;
    (last - first >= RATE * 3 / 5).then_some((first, last))
}
fn decimate(audio: &[f32]) -> Vec<f32> {
    audio
        .chunks_exact(BLOCK)
        .map(|b| b.iter().sum::<f32>() / BLOCK as f32)
        .collect()
}
fn anchors(audio: &[f32], start: usize, end: usize) -> Vec<usize> {
    let width = (end - start) / 3;
    (0..3)
        .filter_map(|third| {
            let from = start + third * width;
            let to = (from + width).min(end);
            (from..to.saturating_sub(WINDOW))
                .step_by(WINDOW)
                .max_by(|&a, &b| {
                    let energy = |at| audio[at..at + WINDOW].iter().map(|x| x * x).sum::<f32>();
                    energy(a).total_cmp(&energy(b))
                })
        })
        .collect()
}

/// Near-complete matching voiced intervals in both recordings, with gain allowed.
/// Coarse three-anchor search bounds work; only four candidates reach full PCM.
pub fn confirmed_mic_copy(
    mic: &[f32],
    mic_start_ns: i64,
    app: &[f32],
    app_start_ns: i64,
) -> Option<f32> {
    if mic.len() > 15 * RATE
        || app.len() > 15 * RATE
        || mic.len() < RATE * 3 / 5
        || app.len() < RATE * 3 / 5
        || mic_start_ns.abs_diff(app_start_ns) > 2_000_000_000
        || mic.iter().chain(app).any(|x| !x.is_finite())
    {
        return None;
    }
    let (ms, me) = activity(mic)?;
    let (as_, ae) = activity(app)?;
    if (me - ms).abs_diff(ae - as_) > RATE / 8 {
        return None;
    }
    let anchors = anchors(app, as_, ae);
    if anchors.len() != 3 {
        return None;
    }
    let md = decimate(mic);
    let ad = decimate(app);
    let mut best: Vec<(f32, isize)> = Vec::new();
    let center = (ms as isize - as_ as isize) / BLOCK as isize;
    for shift in center - 125..=center + 125 {
        let mut score = 1.0f32;
        let mut valid = true;
        for &anchor in &anchors {
            let ai = anchor / BLOCK;
            let mi = ai as isize + shift;
            if mi < 0 || mi as usize + WINDOW / BLOCK > md.len() || ai + WINDOW / BLOCK > ad.len() {
                valid = false;
                break;
            }
            score = score.min(
                correlation(
                    &ad[ai..ai + WINDOW / BLOCK],
                    &md[mi as usize..mi as usize + WINDOW / BLOCK],
                )
                .unwrap_or(-1.0),
            );
        }
        if valid && score > 0.70 {
            best.push((score, shift * BLOCK as isize));
            best.sort_by(|a, b| b.0.total_cmp(&a.0));
            best.truncate(4);
        }
    }
    for (_, coarse) in best {
        let mut refined = None;
        let mut best_score = 0.90;
        for shift in coarse - 16..=coarse + 16 {
            let mut score = 1.0f32;
            for &anchor in &anchors {
                let mi = anchor as isize + shift;
                if mi < 0 || mi as usize + WINDOW > mic.len() {
                    score = -1.0;
                    break;
                }
                score = score.min(
                    correlation(
                        &app[anchor..anchor + WINDOW],
                        &mic[mi as usize..mi as usize + WINDOW],
                    )
                    .unwrap_or(-1.0),
                );
            }
            if score > best_score {
                best_score = score;
                refined = Some(shift);
            }
        }
        let Some(shift) = refined else {
            continue;
        };
        let aligned_start = as_ as isize + shift;
        let aligned_end = ae as isize + shift;
        if aligned_start < 0
            || aligned_end > mic.len() as isize
            || aligned_start.abs_diff(ms as isize) > RATE / 16
            || aligned_end.abs_diff(me as isize) > RATE / 16
        {
            continue;
        }
        let lag = app_start_ns as i128
            - mic_start_ns as i128
            - shift as i128 * 1_000_000_000 / RATE as i128;
        if lag.abs() > 1_000_000_000 {
            continue;
        }
        let mut min_score = 1.0f32;
        let mut energetic = 0;
        let peak = app[as_..ae].iter().map(|x| x * x).fold(0.0, f32::max);
        for ai in (as_..ae).step_by(WINDOW) {
            let end = (ai + WINDOW).min(ae);
            let mi = (ai as isize + shift) as usize;
            let a = &app[ai..end];
            let b = &mic[mi..mi + a.len()];
            let ea = a.iter().map(|x| x * x).sum::<f32>() / a.len() as f32;
            let eb = b.iter().map(|x| x * x).sum::<f32>() / b.len() as f32;
            if ea < 0.000002 && eb < 0.000002 {
                continue;
            }
            if ea > peak * 0.001 || eb > 0.000008 {
                energetic += 1;
                // Correlation alone accepts a quiet second voice. Fit only gain,
                // then require near-quantization-level residual in every window.
                let dot: f64 = a.iter().zip(b).map(|(a, b)| *a as f64 * *b as f64).sum();
                let energy_b: f64 = b.iter().map(|b| (*b as f64).powi(2)).sum();
                if energy_b <= 1e-12 {
                    min_score = -1.0;
                    break;
                }
                let gain = dot / energy_b;
                let residual: f64 = a
                    .iter()
                    .zip(b)
                    .map(|(a, b)| (*a as f64 - gain * *b as f64).powi(2))
                    .sum::<f64>()
                    / a.len() as f64;
                if residual > 0.00000025 || residual > ea as f64 * 0.000025 {
                    min_score = -1.0;
                    break;
                }
                min_score = min_score.min(correlation(a, b).unwrap_or(-1.0));
            }
        }
        if energetic >= 12 && min_score >= 0.98 {
            return Some(min_score);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn fixture(n: usize) -> Vec<f32> {
        let mut state = 17u32;
        (0..n)
            .map(|i| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let x = (state as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32;
                x * 0.15 * (0.3 + 0.7 * (i as f32 / 1300.0).sin().abs())
            })
            .collect()
    }
    #[test]
    fn exact_gain_delayed_copies_only() {
        let mic = fixture(RATE * 2);
        let mut app = vec![0.0; 1600];
        app.extend(mic.iter().map(|x| x * 0.6));
        assert!(confirmed_mic_copy(&mic, 0, &app, 200_000_000).is_some());
        assert!(confirmed_mic_copy(&mic, 0, &mic, 3_000_000_000).is_none());
        let other: Vec<_> = mic.iter().rev().copied().collect();
        assert!(confirmed_mic_copy(&mic, 0, &other, 0).is_none());
        let mixed: Vec<_> = mic.iter().zip(&other).map(|(a, b)| a + b * 0.4).collect();
        assert!(confirmed_mic_copy(&mic, 0, &mixed, 0).is_none());
        let quiet_mix: Vec<_> = mic.iter().zip(&other).map(|(a, b)| a + b * 0.05).collect();
        assert!(confirmed_mic_copy(&mic, 0, &quiet_mix, 0).is_none());
        let very_quiet_mix: Vec<_> = mic.iter().zip(&other).map(|(a, b)| a + b * 0.01).collect();
        assert!(confirmed_mic_copy(&mic, 0, &very_quiet_mix, 0).is_none());
        assert!(confirmed_mic_copy(&mic, 0, &mic[..RATE], 0).is_none());
        assert!(confirmed_mic_copy(&mic[..RATE], 0, &mic, 0).is_none());
        assert!(confirmed_mic_copy(&vec![0.0; RATE], 0, &vec![0.0; RATE], 0).is_none());
        let mut bad = mic.clone();
        bad[4] = f32::NAN;
        assert!(confirmed_mic_copy(&mic, 0, &bad, 0).is_none());
    }
    #[test]
    fn bounded_worst_case_nonmatch() {
        let mic = fixture(15 * RATE);
        let app: Vec<_> = mic.iter().rev().copied().collect();
        let start = std::time::Instant::now();
        assert!(confirmed_mic_copy(&mic, 0, &app, 0).is_none());
        eprintln!("15-second nonmatch: {:?}", start.elapsed());
        let mut near_copy = mic.clone();
        for x in near_copy.iter_mut().rev().take(64) {
            *x += 0.02;
        }
        let start = std::time::Instant::now();
        for _ in 0..8 {
            assert!(confirmed_mic_copy(&mic, 0, &near_copy, 0).is_none());
        }
        eprintln!(
            "Eight 15-second near-copy rejections: {:?}",
            start.elapsed()
        );
    }
}
