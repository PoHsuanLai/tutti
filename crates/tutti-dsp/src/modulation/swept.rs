//! The swept (chorus and flanger) delay kernel: lines read at a delay an LFO
//! moves, the LFO sine solved at control points and interpolated between.

use crate::{COEFF_INTERVAL, DelayLine, InterpolationMode};
use tutti_types::{Phase, PhaseIncrement};

/// The delay lines and the block's LFO phases, borrowed apart from the node so
/// the per-block control closures can be chosen outside the loop.
#[derive(Debug)]
pub struct SweptLines<'a> {
    /// One line per channel.
    pub delays: &'a mut [DelayLine],
    /// Each channel's LFO phase offset (channel 0's is zero).
    pub offsets: &'a [PhaseIncrement],
    /// The LFO's phase at each frame of the block.
    pub phases: &'a [Phase],
    /// The sample rate, narrowed once to `f32`.
    pub sr: f32,
    /// The centre delay in seconds.
    pub base_delay: f32,
    /// Each channel's pending control points, `[prev, next]`.
    pub points: &'a mut [[f32; 2]],
    /// Samples since the last control point, in `0..COEFF_INTERVAL`.
    pub seg_pos: usize,
    /// `points` already hold the current segment's sines.
    pub primed: bool,
    /// This block's step differs from the one that stepped on the pending
    /// control point, so it is solved again from the block's own phases.
    pub restep: bool,
    /// The drive's phase after the block, and the step it ran at: the control
    /// point a segment ends on may lie past the block.
    pub after: Phase,
    /// The per-sample phase step the block ran at.
    pub inc: PhaseIncrement,
}

impl SweptLines<'_> {
    /// The LFO's phase `j` samples into the block, `j` at most one control
    /// interval past its end.
    #[inline]
    fn phase_at(&self, j: usize) -> Phase {
        match self.phases.get(j) {
            Some(&p) => p,
            None => {
                let mut p = self.after;
                for _ in 0..j - self.phases.len() {
                    p = p.advance(self.inc);
                }
                p
            }
        }
    }

    /// Renders the block, channel-outer over the lines.
    ///
    /// `depth_at` / `fb_at` give the
    /// controls at sample `i`; constant closures compile to constants.
    ///
    /// The LFO's sine is evaluated only at control points, every
    /// [`COEFF_INTERVAL`] samples counted across blocks, and interpolated
    /// linearly between (the phaser's mechanism for its coefficients). The
    /// chord of a sine over a step of `h` radians misses it by at most
    /// `h^2 / 8`; with `h = 2*pi * rate * 16 / sr` that is 1.4e-5 of the
    /// depth at 5 Hz and 48 kHz and 2.2e-4 at 20 Hz. Against the deepest
    /// chorus sweep (40 ms) the worst delay error is then 0.03 samples at
    /// 5 Hz (0.6 us), and at the default 5 ms depth 0.003 samples.
    ///
    /// A segment's end point past a block's edge is stepped on at the
    /// block's own rate. When the next block's rate differs, a segment it
    /// begins in the middle of has both its points solved again from that
    /// block's phases (`restep`), so a rate change reaches the delay at its
    /// first sample and not at the next control point; at a constant rate
    /// nothing is re-solved.
    ///
    /// The chord also kinks the delay's slope at every control point, a
    /// step of about 0.05 samples/sample^2 at 20 Hz and 10 ms: a small
    /// ripple on the pitch modulation.
    #[inline(always)]
    pub fn run(
        self,
        x: impl Fn(usize, usize) -> f32,
        mut y: impl FnMut(usize, usize, f32),
        depth_at: impl Fn(usize) -> f32,
        fb_at: impl Fn(usize) -> f32,
    ) {
        const STEP: f32 = 1.0 / COEFF_INTERVAL as f32;
        let sr = self.sr;
        let base_delay = self.base_delay * sr;
        let n = self.phases.len();
        for c in 0..self.delays.len() {
            let offset = self.offsets[c];
            // Channel 0's offset is zero; `offset_by(0)` returns the phase
            // unchanged.
            let sine = |p: Phase| p.offset_by(offset).to_radians().get().sin();
            let [mut prev, mut next] = self.points[c];
            let mut valid = self.primed;
            let mut pos = self.seg_pos;
            let mut i = 0;
            if self.restep && pos != 0 {
                // The block starts mid-segment at a new rate: both of the
                // segment's points move to where the new rate puts them, the
                // start by stepping back from the block's first phase.
                let back = PhaseIncrement(-self.inc.get() * pos as f32);
                prev = sine(self.phase_at(0).advance(back));
                next = sine(self.phase_at(COEFF_INTERVAL - pos));
            }
            while i < n {
                if pos == 0 {
                    prev = if valid { next } else { sine(self.phase_at(i)) };
                    valid = true;
                    next = sine(self.phase_at(i + COEFF_INTERVAL));
                }
                let end = i + (COEFF_INTERVAL - pos).min(n - i);
                let slope = (next - prev) * STEP;
                let line = &mut self.delays[c];
                for k in i..end {
                    let lfo = prev + slope * pos as f32;
                    pos += 1;
                    let delay = (base_delay + lfo * depth_at(k) * sr).max(1.0);
                    let input = x(c, k);
                    let fb_tap = line.read_sample(delay, InterpolationMode::Linear);
                    line.push_sample(input + fb_tap * fb_at(k));
                    y(c, k, line.read_sample(delay, InterpolationMode::Linear));
                }
                if pos == COEFF_INTERVAL {
                    pos = 0;
                }
                i = end;
            }
            self.points[c] = [prev, next];
        }
    }
}
