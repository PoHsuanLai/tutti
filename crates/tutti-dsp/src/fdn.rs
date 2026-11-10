//! [`FdnKernel`]: the kernel of [`FdnDesign`](crate::FdnDesign) (design 022
//! step 1c).
//!
//! `N` delay lines read with linear interpolation (so a length may move), a
//! three-band absorption in each loop, a reflection `A = I - 2 v v^T` to mix
//! them and a stereo output. The wet signal is the lines' own outputs (before
//! their absorption), so nothing sounds before the shortest line has gone
//! round, and the loop gain is exactly the network's.
//!
//! **Retargeting ramps.** `retarget` only sets targets. Every parameter that
//! sounds (the loop gains, the reflection vector, the input and output gains,
//! the wet gain, the width and the modulation depth) is a [`Slew`] that moves
//! to it in a fixed 20 ms of whole [`CHUNK`]s, landing on it exactly; a length
//! moves at a bounded half frame a chunk (about 1.6% of a frame a frame, a
//! pitch bend of 27 cents), and the pre-delay at two. The chunks are counted
//! from the first frame since the last reset, not from a block's edge, so the
//! output does not depend on how the input is cut into blocks.
//!
//! **The absorption** of a line is `y = g_high x + (g_mid - g_high) L2(x) +
//! (g_low - g_mid) L1(x)` with `L1` and `L2` two cascaded one-poles at the
//! network's two crossovers: `g_low` at DC, `g_high` far above the upper
//! crossover and `g_mid` between. It is linear in the gains, so a ramp of the
//! gains is a ramp of the filter; with every gain 1 it is the identity, and a
//! frozen loop loses nothing. A set of gains whose response would exceed 1
//! anywhere (a mid band that rings longer than both its neighbours, by a lot)
//! is scaled to peak at 0.9999 at most, so the loop cannot grow.

use tutti_types::Samples;

use crate::network::{Absorption, Band, Bounds, Effort, Kernel, Matrix, Network, Setup, Stage};

/// The frames between control updates: every ramp advances once a chunk.
const CHUNK: usize = 32;
/// The longest ramp of a gain, in seconds.
const RAMP: f64 = 0.020;
/// How far a line's length moves in a chunk, in frames.
const LENGTH_STEP: f32 = 0.5;
/// How far the pre-delay moves in a chunk, in frames.
const PRE_STEP: f32 = 2.0;
/// The crossovers a network that gives none is read at, in hertz.
const DEFAULT_CROSSOVER: [f32; 2] = [300.0, 4_000.0];

/// A value that moves to its target in whole chunks and lands on it.
#[derive(Clone, Copy, Debug, Default)]
struct Slew {
    cur: f32,
    target: f32,
    step: f32,
    left: u32,
}

impl Slew {
    fn at(v: f32) -> Self {
        Self {
            cur: v,
            target: v,
            step: 0.0,
            left: 0,
        }
    }

    fn snap(&mut self) {
        *self = Self::at(self.target);
    }

    /// Aims at `target`, reaching it in `chunks` chunks from here.
    fn aim(&mut self, target: f32, chunks: u32) {
        if target == self.target {
            return;
        }
        self.target = target;
        if chunks == 0 || self.cur == target {
            self.cur = target;
            self.left = 0;
        } else {
            self.left = chunks;
            self.step = (target - self.cur) / chunks as f32;
        }
    }

    /// Aims at `target`, no faster than `per_chunk` a chunk.
    fn aim_rate(&mut self, target: f32, per_chunk: f32) {
        let chunks = ((target - self.cur).abs() / per_chunk).ceil();
        self.aim(target, chunks as u32);
    }

    fn advance(&mut self) {
        if self.left > 0 {
            self.left -= 1;
            self.cur = if self.left == 0 {
                self.target
            } else {
                self.cur + self.step
            };
        }
    }
}

/// The kernel of a feedback delay network.
#[derive(Clone, Debug)]
pub struct FdnKernel {
    rate: f64,
    ramp_chunks: u32,
    max_lines: usize,
    cap: usize,
    pre_cap: usize,
    n: usize,
    initialised: bool,

    buf: Vec<f32>,
    w: usize,
    pre: Vec<f32>,
    pre_w: usize,

    len: Vec<Slew>,
    pre_len: Slew,
    depth: Slew,
    rate_hz: f32,
    phase: Vec<f64>,
    gain: Vec<[Slew; 3]>,
    v: Vec<Slew>,
    input: Vec<Slew>,
    out: Vec<[Slew; 2]>,
    wet: Slew,
    width: Slew,
    pole: [f32; 2],
    crossover: [f32; 2],

    /// One-pole states a line: `[L1 first, L1 second, L2 first, L2 second]`.
    state: Vec<[f32; 4]>,

    pos: usize,
    // The chunk's constants.
    c_len: Vec<f32>,
    c_mod_prev: Vec<f32>,
    c_mod_next: Vec<f32>,
    c_gain: Vec<[f32; 3]>,
    c_v: Vec<f32>,
    c_in: Vec<f32>,
    c_out: Vec<[f32; 2]>,
    c_wet: f32,
    c_width: f32,
    c_pre: f32,
}

impl FdnKernel {
    pub(crate) fn new() -> Self {
        Self {
            rate: 48_000.0,
            ramp_chunks: 1,
            max_lines: 0,
            cap: 0,
            pre_cap: 0,
            n: 0,
            initialised: false,
            buf: Vec::new(),
            w: 0,
            pre: Vec::new(),
            pre_w: 0,
            len: Vec::new(),
            pre_len: Slew::default(),
            depth: Slew::default(),
            rate_hz: 0.5,
            phase: Vec::new(),
            gain: Vec::new(),
            v: Vec::new(),
            input: Vec::new(),
            out: Vec::new(),
            wet: Slew::default(),
            width: Slew::default(),
            pole: [0.0; 2],
            crossover: [0.0; 2],
            state: Vec::new(),
            pos: 0,
            c_len: Vec::new(),
            c_mod_prev: Vec::new(),
            c_mod_next: Vec::new(),
            c_gain: Vec::new(),
            c_v: Vec::new(),
            c_in: Vec::new(),
            c_out: Vec::new(),
            c_wet: 0.0,
            c_width: 0.0,
            c_pre: 0.0,
        }
    }

    fn start_phase(&self, j: usize) -> f64 {
        std::f64::consts::TAU * j as f64 / self.max_lines.max(1) as f64
    }

    /// Back to silence with every ramp landed: what a fresh kernel is after its
    /// first retarget.
    fn settle(&mut self) {
        self.buf.fill(0.0);
        self.pre.fill(0.0);
        self.w = 0;
        self.pre_w = 0;
        self.pos = 0;
        self.state.fill([0.0; 4]);
        for s in &mut self.len {
            s.snap();
        }
        self.pre_len.snap();
        self.depth.snap();
        self.wet.snap();
        self.width.snap();
        for g in &mut self.gain {
            g.iter_mut().for_each(Slew::snap);
        }
        for s in self.v.iter_mut().chain(&mut self.input) {
            s.snap();
        }
        for o in &mut self.out {
            o.iter_mut().for_each(Slew::snap);
        }
        for j in 0..self.max_lines {
            self.phase[j] = self.start_phase(j);
            let m = (f64::from(self.depth.cur) * libm::sin(self.phase[j])) as f32;
            self.c_mod_prev[j] = m;
            self.c_mod_next[j] = m;
        }
    }

    fn set_poles(&mut self, crossover: [f32; 2]) {
        if crossover == self.crossover {
            return;
        }
        self.crossover = crossover;
        for (p, f) in self.pole.iter_mut().zip(crossover) {
            *p = libm::exp(-std::f64::consts::TAU * f64::from(f) / self.rate) as f32;
        }
    }

    /// The gains of line `j`'s absorption, scaled if their response would peak
    /// above 1.
    fn absorption_gains(&self, a: Absorption) -> [f32; 3] {
        let g = [Band::Low, Band::Mid, Band::High].map(|b| a.gain(b).get());
        if !g.iter().all(|x| x.is_finite() && *x >= 0.0) {
            return [0.0; 3];
        }
        let peak = self.peak_response(g);
        if peak > 1.0 {
            g.map(|x| (f64::from(x) * 0.9999 / peak) as f32)
        } else {
            g
        }
    }

    /// The largest `|y(w)|` of the absorption with gains `g = [low, mid,
    /// high]`, over a grid of frequencies.
    fn peak_response(&self, g: [f32; 3]) -> f64 {
        let (low, mid, high) = (f64::from(g[0]), f64::from(g[1]), f64::from(g[2]));
        let (c2, c1) = (mid - high, low - mid);
        // |L1|, |L2| <= 1, so the response is at most the sum of the magnitudes
        // of its three terms: when that is under 1 (every gain falling with
        // frequency, the usual case) there is nothing to measure.
        let bound = high.abs() + c2.abs() + c1.abs();
        if bound <= 1.0 {
            return bound;
        }
        let pole = self.pole.map(f64::from);
        // |L(w)|: ((1 - p) / (1 - p e^-jw))^2.
        let l = |p: f64, w: f64| -> (f64, f64) {
            let (re, im) = (1.0 - p * libm::cos(w), p * libm::sin(w));
            let den = re * re + im * im;
            let (hr, hi) = ((1.0 - p) * re / den, -(1.0 - p) * im / den);
            (hr * hr - hi * hi, 2.0 * hr * hi)
        };
        let mut peak = 0.0f64;
        for k in 0..=96 {
            let w = std::f64::consts::PI * (f64::from(k) / 96.0).powi(2);
            let (a, b) = l(pole[0], w);
            let (c, d) = l(pole[1], w);
            let (re, im) = (high + c2 * c + c1 * a, c2 * d + c1 * b);
            peak = peak.max(libm::hypot(re, im));
        }
        peak
    }

    fn begin_chunk(&mut self) {
        let n = self.n;
        for s in &mut self.len[..n] {
            s.advance();
        }
        self.pre_len.advance();
        self.depth.advance();
        self.wet.advance();
        self.width.advance();
        let dphi = std::f64::consts::TAU * f64::from(self.rate_hz) * CHUNK as f64 / self.rate;
        for j in 0..n {
            self.gain[j].iter_mut().for_each(Slew::advance);
            self.v[j].advance();
            self.input[j].advance();
            self.out[j].iter_mut().for_each(Slew::advance);
            self.phase[j] += dphi;
            self.c_len[j] = self.len[j].cur;
            self.c_mod_prev[j] = self.c_mod_next[j];
            self.c_mod_next[j] = (f64::from(self.depth.cur) * libm::sin(self.phase[j])) as f32;
            let [low, mid, high] = self.gain[j].map(|s| s.cur);
            self.c_gain[j] = [high, mid - high, low - mid];
            self.c_v[j] = self.v[j].cur;
            self.c_in[j] = self.input[j].cur;
            self.c_out[j] = self.out[j].map(|s| s.cur);
        }
        self.c_wet = self.wet.cur;
        self.c_width = self.width.cur;
        self.c_pre = self.pre_len.cur;
    }
}

/// `buf[(w - delay) mod cap]` interpolated to a fractional `delay` of at least 0.
#[inline]
fn read(buf: &[f32], w: usize, delay: f32) -> f32 {
    let cap = buf.len();
    let whole = delay as usize;
    let frac = delay - whole as f32;
    let a = buf[(w + cap - whole) % cap];
    if frac == 0.0 {
        return a;
    }
    let b = buf[(w + cap - whole - 1) % cap];
    a * (1.0 - frac) + b * frac
}

impl Kernel for FdnKernel {
    fn prepare(&mut self, bounds: &Bounds, setup: &Setup) {
        let rate = setup.rate.get();
        self.rate = rate;
        self.ramp_chunks = ((RAMP * rate / CHUNK as f64).ceil() as u32).max(1);
        let lines = bounds.lines;
        self.max_lines = lines;
        self.cap = bounds.line.ceil() as usize + 4;
        self.pre_cap = bounds.pre_delay.get() + 4;
        self.n = 0;
        self.initialised = false;
        self.buf = vec![0.0; lines * self.cap];
        self.pre = vec![0.0; self.pre_cap];
        self.len = vec![Slew::at(1.0); lines];
        self.pre_len = Slew::default();
        self.depth = Slew::default();
        self.wet = Slew::default();
        self.width = Slew::at(1.0);
        self.phase = vec![0.0; lines];
        self.gain = vec![[Slew::default(); 3]; lines];
        self.v = vec![Slew::default(); lines];
        self.input = vec![Slew::default(); lines];
        self.out = vec![[Slew::default(); 2]; lines];
        self.state = vec![[0.0; 4]; lines];
        self.c_len = vec![1.0; lines];
        self.c_mod_prev = vec![0.0; lines];
        self.c_mod_next = vec![0.0; lines];
        self.c_gain = vec![[0.0; 3]; lines];
        self.c_v = vec![0.0; lines];
        self.c_in = vec![0.0; lines];
        self.c_out = vec![[0.0; 2]; lines];
        self.crossover = [0.0; 2];
        self.set_poles(DEFAULT_CROSSOVER);
        self.settle();
    }

    fn retarget(&mut self, network: &Network) {
        let Some(Stage::Fdn(f)) = network.stages.iter().find(|s| matches!(s, Stage::Fdn(_))) else {
            return;
        };
        let n = f.lines.len().min(self.max_lines);
        if n == 0 {
            return;
        }
        let first = !self.initialised;
        self.n = n;
        let ramp = self.ramp_chunks;
        let max_len = (self.cap - 4) as f32;

        if let Some(Absorption::Bands { crossover, .. }) = f.lines.first().map(|l| l.absorption) {
            self.set_poles([crossover[0].get(), crossover[1].get()]);
        }
        let flat = 1.0 / (n as f32).sqrt();
        for j in 0..n {
            let line = &f.lines[j];
            let target = if line.length.is_finite() {
                line.length.clamp(1.0, max_len)
            } else {
                1.0
            };
            self.len[j].aim_rate(target, LENGTH_STEP);
            let g = self.absorption_gains(line.absorption);
            for (s, t) in self.gain[j].iter_mut().zip(g) {
                s.aim(t, ramp);
            }
            let v = match &f.matrix {
                Matrix::Diagonal => 0.0,
                Matrix::Householder => flat,
                Matrix::Reflection(v) if v.len() == n => v[j],
                _ => 0.0,
            };
            self.v[j].aim(if v.is_finite() { v } else { 0.0 }, ramp);
            let input = f.input.get(j).copied().unwrap_or(0.0);
            self.input[j].aim(if input.is_finite() { input } else { 0.0 }, ramp);
            let row = |r: usize| {
                let x = f
                    .output
                    .rows
                    .get(r)
                    .or_else(|| f.output.rows.first())
                    .and_then(|row| row.get(j))
                    .copied()
                    .unwrap_or(0.0);
                if x.is_finite() { x } else { 0.0 }
            };
            let (l, r) = (row(0), row(1));
            self.out[j][0].aim(l, ramp);
            self.out[j][1].aim(r, ramp);
        }
        for j in n..self.max_lines {
            self.input[j].aim(0.0, ramp);
            self.out[j].iter_mut().for_each(|s| s.aim(0.0, ramp));
        }
        let m = f.lines[0].modulation;
        let depth = if m.depth.is_finite() {
            m.depth.abs().min((self.cap as f32) * 0.25)
        } else {
            0.0
        };
        self.depth.aim(depth, ramp);
        let hz = m.rate.get();
        self.rate_hz = if hz.is_finite() {
            hz.clamp(0.0, 20.0)
        } else {
            0.5
        };
        let pre = network.pre_delay.get() as f32;
        self.pre_len
            .aim_rate(pre.clamp(0.0, (self.pre_cap - 4) as f32), PRE_STEP);
        let wet = network.wet.to_amplitude().get();
        self.wet.aim(if wet.is_finite() { wet } else { 0.0 }, ramp);
        let width = network.width.get();
        self.width.aim(
            if width.is_finite() {
                width.clamp(0.0, 4.0)
            } else {
                1.0
            },
            ramp,
        );

        if first {
            self.initialised = true;
            self.settle();
        }
    }

    // The per-line arrays (`c_len`, `c_out`, `state`, `buf` rows) are indexed by
    // one line number with the loop's scratch `d`; an iterator over any one of
    // them would only hide the others.
    #[allow(clippy::needless_range_loop)]
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let frames = output.first().map_or(0, |c| c.len());
        if self.max_lines == 0 || !self.initialised {
            output.iter_mut().for_each(|c| c.fill(0.0));
            return;
        }
        let n = self.n;
        let scale = 1.0 / input.len().max(1) as f32;
        let p = self.pole.map(|x| 1.0 - x);
        let mut d = [0.0f32; 16];
        for i in 0..frames {
            if self.pos == 0 {
                self.begin_chunk();
            }
            let t = self.pos as f32 / CHUNK as f32;
            let x: f32 = input.iter().map(|c| c[i]).sum::<f32>() * scale;

            self.pre[self.pre_w] = x;
            let xd = read(&self.pre, self.pre_w, self.c_pre);
            self.pre_w = (self.pre_w + 1) % self.pre_cap;

            let (mut out_l, mut out_r) = (0.0f32, 0.0f32);
            for j in 0..n {
                let delay = (self.c_len[j]
                    + self.c_mod_prev[j]
                    + (self.c_mod_next[j] - self.c_mod_prev[j]) * t)
                    .max(1.0);
                let line = &self.buf[j * self.cap..(j + 1) * self.cap];
                d[j] = read(line, self.w, delay);
                out_l += self.c_out[j][0] * d[j];
                out_r += self.c_out[j][1] * d[j];
            }

            let mut sum = 0.0f32;
            for j in 0..n {
                let s = &mut self.state[j];
                s[0] += p[0] * (d[j] - s[0]);
                s[1] += p[0] * (s[0] - s[1]);
                s[2] += p[1] * (d[j] - s[2]);
                s[3] += p[1] * (s[2] - s[3]);
                let [high, c2, c1] = self.c_gain[j];
                d[j] = high * d[j] + c2 * s[3] + c1 * s[1];
                sum += self.c_v[j] * d[j];
            }
            for j in 0..n {
                self.buf[j * self.cap + self.w] =
                    self.c_in[j] * xd + d[j] - 2.0 * self.c_v[j] * sum;
            }
            self.w = (self.w + 1) % self.cap;

            let (mid, side) = ((out_l + out_r) * 0.5, (out_l - out_r) * 0.5);
            let wet = self.c_wet;
            let (l, r) = (
                (mid + self.c_width * side) * wet,
                (mid - self.c_width * side) * wet,
            );
            if let Some(c) = output.get_mut(0) {
                c[i] = l;
            }
            if let Some(c) = output.get_mut(1) {
                c[i] = r;
            }
            self.pos = (self.pos + 1) % CHUNK;
        }
    }

    fn reset(&mut self) {
        self.settle();
    }

    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }

    fn fresh(&self) -> Self {
        let mut fresh = self.clone();
        fresh.settle();
        fresh
    }
}
