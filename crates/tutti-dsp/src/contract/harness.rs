//! The measuring helpers the cases share: a realization built at a model, a
//! render in blocks, seeded noise, a tone burst and the energy of a span.
//!
//! Public so a plugin probe (design 023) can measure a hosted plugin with the
//! same ones.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{MaxBlock, SampleRate, Samples};

use super::DesignCase;
use crate::{Channels, Design, Effect, Realization, Setup};

/// The longest block the cases prepare for.
pub const MAX_BLOCK: usize = 512;
/// The window the energy of a decay is measured in, in frames.
pub const WINDOW: usize = 1024;

/// What a realization of `d` is prepared for at `rate`: its widths and the
/// longest block of the cases.
#[must_use]
pub fn setup<D: Design>(d: &D, rate: SampleRate) -> Setup {
    let Channels { input, output } = d.channels();
    Setup {
        rate,
        max_block: MaxBlock::of(Samples(MAX_BLOCK)),
        input,
        output,
    }
}

/// A prepared realization retargeted to `controls`, and its model.
pub fn built<D: DesignCase>(
    d: &D,
    controls: &D::Controls,
    rate: SampleRate,
) -> (D::Realization, D::Model) {
    let mut k = d.realization();
    k.prepare(&d.bounds(&d.ceiling(rate)), &setup(d, rate));
    let model = d.design(controls, rate);
    k.retarget(&model);
    (k, model)
}

/// The input and output widths of `d`.
#[must_use]
pub fn widths<D: Design>(d: &D) -> (usize, usize) {
    let Channels { input, output } = d.channels();
    (usize::from(input.count()), usize::from(output.count()))
}

/// Renders `input` (the same signal on every input channel) through `k` in
/// blocks of `block`, one `Vec` per output channel.
pub fn render<M, K: Effect<M>>(
    k: &mut K,
    (inputs, outputs): (usize, usize),
    input: &[f32],
    block: usize,
) -> Vec<Vec<f32>> {
    let mut out = vec![vec![0.0f32; input.len()]; outputs];
    let mut start = 0;
    while start < input.len() {
        let end = (start + block).min(input.len());
        let ins: Vec<&[f32]> = vec![&input[start..end]; inputs];
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[start..end]).collect();
        k.process(&ins, &mut outs);
        start = end;
    }
    out
}

/// `frames` of seeded uniform noise in `-level..=level`.
#[must_use]
pub fn noise(seed: u64, frames: usize, level: f32) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(seed);
    (0..frames).map(|_| rng.gen_range(-level..=level)).collect()
}

/// A tone burst at `freq` cycles a frame for `plateau` frames between
/// raised-cosine fades of 4096 frames, then `after` frames of silence. The
/// fades keep the burst to its band: an abrupt start or stop leaks energy into
/// the low frequencies, which decay slowest and would govern the deep part of a
/// high band's decay.
#[derive(Clone, Debug)]
pub struct Burst {
    /// The signal.
    pub signal: Vec<f32>,
    /// Where the plateau ends and the fade-out begins.
    pub plateau_end: usize,
    /// Where the input is silent for good.
    pub end: usize,
}

/// The length of a burst's fades, in frames.
const FADE: usize = 4096;

/// A [`Burst`] of `freq` cycles a frame, `plateau` frames long, then `after`
/// frames of silence.
#[must_use]
pub fn burst(freq: f32, plateau: usize, after: usize) -> Burst {
    let end = plateau + 2 * FADE;
    let mut signal = vec![0.0f32; end + after];
    for (n, s) in signal[..end].iter_mut().enumerate() {
        let tone = libm::cos(std::f64::consts::TAU * f64::from(freq) * n as f64) as f32;
        let fade = |k: usize| {
            let x = std::f32::consts::PI * k as f32 / FADE as f32;
            0.5 - 0.5 * x.cos()
        };
        let envelope = if n < FADE {
            fade(n)
        } else if n >= FADE + plateau {
            fade(end - n)
        } else {
            1.0
        };
        *s = tone * envelope;
    }
    Burst {
        signal,
        plateau_end: FADE + plateau,
        end,
    }
}

/// The energy of all channels over `len` frames from `from`.
#[must_use]
pub fn energy(out: &[Vec<f32>], from: usize, len: usize) -> f64 {
    out.iter()
        .flat_map(|c| &c[from..from + len])
        .map(|&s| f64::from(s) * f64::from(s))
        .sum()
}

/// The RMS of `out` over `len` frames from `from`, all channels together.
#[must_use]
pub fn rms(out: &[Vec<f32>], from: usize, len: usize) -> f64 {
    (energy(out, from, len) / (len * out.len()) as f64).sqrt()
}

/// The first rate a design is measured at.
pub(super) fn rate_of<D: DesignCase>(d: &D) -> SampleRate {
    d.rates()[0]
}
