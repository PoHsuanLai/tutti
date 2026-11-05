//! Beat placement by dynamic programming (Ellis, "Beat Tracking by Dynamic
//! Programming", JNMR 2007).
//!
//! A beat sequence scores the novelty at each beat, minus a penalty on how far
//! each gap strays from the target period. The best sequence ending at frame
//! `i` is `local[i]` plus the best predecessor `j` in `[i - 2P, i - P/2]`, whose
//! score is cut by `tightness * ln(gap / P)^2`. Backtracking from the best
//! final frame gives the beats. The penalty is on the log ratio, so a gap of
//! half the period costs the same as one of double.
//!
//! Pure functions over a novelty slice; the public entry points are in
//! [`crate::tempo`].

/// The standard deviation of the smoothing kernel as a fraction of the period.
const SMOOTH_PERIOD_FRACTION: f64 = 1.0 / 32.0;

/// A leading or trailing beat is dropped when its smoothed score is below this
/// share of the RMS of the beats' smoothed scores.
const TRIM_FRACTION: f32 = 0.5;

/// A frame starts the chain once its score reaches this share of the maximum.
const START_FRACTION: f32 = 0.01;

/// A final candidate must reach this share of the median final-candidate score.
const TAIL_FRACTION: f32 = 0.5;

/// Beat frames over `novelty`, with beats `period` frames apart.
///
/// `novelty` is read as non-negative strength per frame. It is divided by its
/// own standard deviation first, so `tightness` means the same at any level.
/// `period` is in frames and may be fractional; `tightness` is the weight of
/// the gap penalty against the novelty score (0 turns the period off).
/// Returns ascending frame indices; empty for a flat or too short curve.
pub(crate) fn track(novelty: &[f32], period: f64, tightness: f32) -> Vec<usize> {
    let n = novelty.len();
    let local = local_score(novelty, period);
    if n == 0 || local.is_empty() || period.is_nan() || period < 2.0 {
        return Vec::new();
    }

    let lo = ((period / 2.0).round() as usize).max(1);
    let hi = ((period * 2.0).round() as usize).max(lo);
    // `penalty[t - lo]` is the cost of a gap of `t` frames.
    let penalty: Vec<f32> = (lo..=hi)
        .map(|t| -tightness * (t as f64 / period).ln().powi(2) as f32)
        .collect();

    let threshold = START_FRACTION * local.iter().copied().fold(0.0f32, f32::max);
    let mut cumulative = vec![0.0f32; n];
    let mut backlink = vec![usize::MAX; n];
    let mut chain_open = false;
    for i in 0..n {
        let mut best: Option<(usize, f32)> = None;
        for t in lo..=hi.min(i) {
            let score = cumulative[i - t] + penalty[t - lo];
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((i - t, score));
            }
        }
        // Until the first strong frame the chain restarts at every frame, so
        // leading silence cannot pull the first beat out of place.
        match best {
            Some((j, score)) if chain_open => {
                cumulative[i] = local[i] + score;
                backlink[i] = j;
            }
            _ => cumulative[i] = local[i],
        }
        chain_open |= local[i] >= threshold;
    }

    let mut beats = Vec::new();
    let mut at = last_beat(&cumulative);
    loop {
        beats.push(at);
        if backlink[at] == usize::MAX {
            break;
        }
        at = backlink[at];
    }
    beats.reverse();
    trim(&mut beats, &local);
    beats
}

/// The novelty scaled to unit standard deviation and smoothed by a Gaussian
/// of `period / 32` frames. Empty when the curve has no spread.
fn local_score(novelty: &[f32], period: f64) -> Vec<f32> {
    let n = novelty.len() as f64;
    let mean = novelty.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let var = novelty
        .iter()
        .map(|&v| (f64::from(v) - mean).powi(2))
        .sum::<f64>()
        / n;
    let std = var.sqrt();
    if std.is_nan() || std <= 0.0 || !std.is_finite() {
        return Vec::new();
    }

    let sigma = (period * SMOOTH_PERIOD_FRACTION).max(f64::MIN_POSITIVE);
    let half = (3.0 * sigma).ceil() as isize;
    let kernel: Vec<f64> = (-half..=half)
        .map(|k| (-0.5 * (k as f64 / sigma).powi(2)).exp())
        .collect();
    (0..novelty.len() as isize)
        .map(|i| {
            kernel
                .iter()
                .zip(-half..=half)
                .filter_map(|(w, k)| {
                    usize::try_from(i + k)
                        .ok()
                        .and_then(|j| novelty.get(j))
                        .map(|&v| w * f64::from(v) / std)
                })
                .sum::<f64>() as f32
        })
        .collect()
}

/// The last frame the backtrace should start from: the final local maximum of
/// the cumulative score that reaches half the median of all local maxima, so
/// a trailing stretch of silence does not drag the chain out to the end.
fn last_beat(cumulative: &[f32]) -> usize {
    let n = cumulative.len();
    let maxima: Vec<usize> = (0..n)
        .filter(|&i| {
            (i == 0 || cumulative[i] > cumulative[i - 1])
                && (i + 1 == n || cumulative[i] >= cumulative[i + 1])
        })
        .collect();
    let mut scores: Vec<f32> = maxima.iter().map(|&i| cumulative[i]).collect();
    scores.sort_by(f32::total_cmp);
    let median = scores.get(scores.len() / 2).copied().unwrap_or(0.0);
    let threshold = TAIL_FRACTION * median;
    maxima
        .iter()
        .rev()
        .copied()
        .find(|&i| cumulative[i] >= threshold)
        .unwrap_or(n - 1)
}

/// Drops weak beats from both ends, judged on the local score smoothed over
/// five beats by a Hann window.
fn trim(beats: &mut Vec<usize>, local: &[f32]) {
    const HANN5: [f32; 5] = [0.0, 0.5, 1.0, 0.5, 0.0];
    if beats.is_empty() {
        return;
    }
    let scores: Vec<f32> = beats.iter().map(|&b| local[b]).collect();
    let smooth: Vec<f32> = (0..scores.len())
        .map(|i| {
            HANN5
                .iter()
                .enumerate()
                .filter_map(|(k, w)| {
                    (i + k)
                        .checked_sub(2)
                        .and_then(|j| scores.get(j))
                        .map(|s| w * s)
                })
                .sum()
        })
        .collect();
    let rms = (smooth.iter().map(|s| s * s).sum::<f32>() / smooth.len() as f32).sqrt();
    let cut = TRIM_FRACTION * rms;
    let first = smooth.iter().position(|&s| s >= cut);
    let last = smooth.iter().rposition(|&s| s >= cut);
    match (first, last) {
        (Some(f), Some(l)) => {
            beats.truncate(l + 1);
            beats.drain(..f);
        }
        _ => beats.clear(),
    }
}
