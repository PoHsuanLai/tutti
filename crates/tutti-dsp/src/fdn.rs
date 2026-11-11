//! [`FdnKernel`]: the [`Kernel`] of [`FdnDesign`](crate::FdnDesign), run by
//! [`Realized`](crate::Realized) (design 022 step 1c, design 024 section 4.3a).
//!
//! `N` delay lines read with linear interpolation (so a length may move), a
//! three-band absorption in each loop, a mix `A = I - 2 m v v^T` (`m` is 1 for a
//! reflection and 0 for the identity of `Matrix::Diagonal`) and a stereo output.
//! The wet signal is the lines' own outputs (before their absorption), so
//! nothing sounds before the shortest line has gone round, and the loop gain is
//! exactly the network's.
//!
//! **Retargeting ramps.** `retarget` only sets targets. Every parameter that
//! sounds (the loop gains, the reflection vector (renormalised to its length
//! every chunk) and its mix `m`, the input and output gains, the wet gain, the
//! width and the modulation depth) is a [`Glide`] that moves to it in a fixed
//! 20 ms of whole [`CHUNK`]s, landing on it exactly; a length moves at a
//! bounded half frame a chunk (about 1.6% of a frame a frame, a pitch bend of 27 cents), and the pre-delay at two. The chunks are counted
//! from the first frame since the last reset, not from a block's edge, so the
//! output does not depend on how the input is cut into blocks.
//!
//! **The mix.** The identity of a diagonal network and the reflection of a
//! mixing one have determinants +1 and -1, so no path of orthogonal matrices
//! joins them, and a straight line `I - 2 m v v^T` between them has eigenvalue
//! `1 - 2m` along `v`, a contraction that would lose a frozen tail's energy.
//! The matrix is therefore never between the two: `m` is 0 or 1, and it
//! switches at the end of a ramp that leaves a mixing matrix (when `v` has
//! reached line 0, where `I - 2 v v^T` flips one line's sign) and at the start
//! of one that reaches it. Every step of a ramp is then orthogonal, `v` moves
//! between unit vectors (a diagonal network aims it at line 0), and a mixing
//! network is bit-identical to a kernel without `m`. The switch is a step in
//! the sign of line 0's loop, as the model's own matrix steps between
//! `Diagonal` and the first `Reflection` near line 0.
//!
//! **The absorption** of a line is `y = g_high x + (g_mid - g_high) L2(x) +
//! (g_low - g_mid) L1(x)` with `L1` and `L2` two cascaded one-poles at the
//! network's two crossovers: `g_low` at DC, `g_high` far above the upper
//! crossover and `g_mid` between. It is linear in the gains, so a ramp of the
//! gains is a ramp of the filter; with every gain 1 it is the identity, and a
//! frozen loop loses nothing. A set of gains whose response would exceed 1
//! anywhere (a mid band that rings longer than both its neighbours, by a lot)
//! is scaled to peak at 0.9999 at most, so the loop cannot grow.

use core::ops::Range;

use crate::network::{Absorption, Band, Bounds, Matrix, Network, Stage};
use crate::{Approach, Effort, Glide, Kernel, Setup, Steps};

/// The frames between control updates: every ramp advances once a chunk.
pub(crate) const CHUNK: usize = 32;
/// The longest ramp of a gain, in seconds.
const RAMP: f64 = 0.020;
/// How far a line's length moves in a chunk, in frames.
const LENGTH_STEP: f32 = 0.5;
/// How far the pre-delay moves in a chunk, in frames.
const PRE_STEP: f32 = 2.0;
/// The crossovers a network that gives none is read at, in hertz.
const DEFAULT_CROSSOVER: [f32; 2] = [300.0, 4_000.0];

/// A 20 ms ramp at `rate`, in whole [`CHUNK`]s (at least one).
pub(crate) fn ramp(rate: f64) -> Steps {
    Steps::Fixed(((RAMP * rate / CHUNK as f64).ceil() as u32).max(1))
}

/// The kernel of a feedback delay network: what [`prepare`](Kernel::prepare) sized,
/// and the math of a chunk. Its coefficients are `FdnCoeffs`, its words
/// `FdnState`.
#[derive(Clone, Debug)]
pub struct FdnKernel {
    rate: f64,
    ramp: Steps,
    max_lines: usize,
    cap: usize,
    pre_cap: usize,
}

/// What an [`FdnKernel`] approaches: every sounding parameter a [`Glide`], and the
/// few that jump (the line count, the modulation rate, the absorption's poles).
#[derive(Clone, Debug)]
pub struct FdnCoeffs {
    /// The lines in use: 0 until the first valid retarget, and the block is silent.
    n: usize,
    rate_hz: f32,
    pole: [f32; 2],
    crossover: [f32; 2],

    len: Vec<Glide<f32>>,
    pre_len: Glide<f32>,
    depth: Glide<f32>,
    gain: Vec<[Glide<f32>; 3]>,
    v: Vec<Glide<f32>>,
    /// The length of `v` the network asks for (1 for every design here): the
    /// reflection is renormalised to it every chunk, so it is orthogonal at
    /// every step of a ramp and not only at its ends.
    v_len: Glide<f32>,
    /// `m` of `I - 2 m v v^T`: 0 for a diagonal network, 1 for a reflection. Only
    /// its target and whether it is moving are read (see the module docs): the
    /// kernel's `m` is 1 while it ramps or aims at 1, else 0.
    mix: Glide<f32>,
    input: Vec<Glide<f32>>,
    out: Vec<[Glide<f32>; 2]>,
    wet: Glide<f32>,
    width: Glide<f32>,
}

impl FdnCoeffs {
    fn new(lines: usize) -> Self {
        let rest = Glide::landed(0.0);
        Self {
            n: 0,
            rate_hz: 0.5,
            pole: [0.0; 2],
            crossover: [0.0; 2],
            len: vec![Glide::landed(1.0); lines],
            pre_len: rest,
            depth: rest,
            gain: vec![[rest; 3]; lines],
            v: vec![rest; lines],
            v_len: rest,
            mix: rest,
            input: vec![rest; lines],
            out: vec![[rest; 2]; lines],
            wet: rest,
            width: Glide::landed(1.0),
        }
    }

    fn glides(&mut self) -> impl Iterator<Item = &mut Glide<f32>> {
        self.len
            .iter_mut()
            .chain(self.gain.iter_mut().flatten())
            .chain(&mut self.v)
            .chain(&mut self.input)
            .chain(self.out.iter_mut().flatten())
            .chain([
                &mut self.pre_len,
                &mut self.depth,
                &mut self.v_len,
                &mut self.mix,
                &mut self.wet,
                &mut self.width,
            ])
    }
}

impl Approach for FdnCoeffs {
    fn step(&mut self) -> bool {
        // Every glide steps: no short circuit.
        self.glides().fold(false, |moved, g| g.step() | moved)
    }

    fn settle(&mut self) {
        self.glides().for_each(Approach::settle);
    }
}

/// What a network's chunks change: the lines, the pre-delay, the modulation phases,
/// the absorption's one-poles, and the constants of the chunk being run.
#[derive(Clone, Debug)]
pub struct FdnState {
    buf: Vec<f32>,
    w: usize,
    pre: Vec<f32>,
    pre_w: usize,
    phase: Vec<f64>,
    /// One-pole states a line: `[L1 first, L1 second, L2 first, L2 second]`.
    pole: Vec<[f32; 4]>,
    /// Whether the modulation has not started: its first chunk reads the phase's
    /// own value as the previous one.
    fresh: bool,

    // The chunk's constants.
    c_len: Vec<f32>,
    c_mod_prev: Vec<f32>,
    c_mod_next: Vec<f32>,
    c_gain: Vec<[f32; 3]>,
    c_v: Vec<f32>,
    /// 0 or 1: the chunk's `m`.
    c_mix: f32,
    c_in: Vec<f32>,
    c_out: Vec<[f32; 2]>,
    c_wet: f32,
    c_width: f32,
    c_pre: f32,
}

impl FdnState {
    fn new(k: &FdnKernel) -> Self {
        let lines = k.max_lines;
        let mut s = Self {
            buf: vec![0.0; lines * k.cap],
            w: 0,
            pre: vec![0.0; k.pre_cap],
            pre_w: 0,
            phase: vec![0.0; lines],
            pole: vec![[0.0; 4]; lines],
            fresh: true,
            c_len: vec![1.0; lines],
            c_mod_prev: vec![0.0; lines],
            c_mod_next: vec![0.0; lines],
            c_gain: vec![[0.0; 3]; lines],
            c_v: vec![0.0; lines],
            c_mix: 0.0,
            c_in: vec![0.0; lines],
            c_out: vec![[0.0; 2]; lines],
            c_wet: 0.0,
            c_width: 0.0,
            c_pre: 0.0,
        };
        k.clear(&mut s);
        s
    }
}

impl FdnKernel {
    pub(crate) fn new() -> Self {
        Self {
            rate: 48_000.0,
            ramp: Steps::Fixed(1),
            max_lines: 0,
            cap: 0,
            pre_cap: 0,
        }
    }

    fn start_phase(&self, j: usize) -> f64 {
        std::f64::consts::TAU * j as f64 / self.max_lines.max(1) as f64
    }

    fn set_poles(&self, c: &mut FdnCoeffs, crossover: [f32; 2]) {
        if crossover == c.crossover {
            return;
        }
        c.crossover = crossover;
        for (p, f) in c.pole.iter_mut().zip(crossover) {
            *p = libm::exp(-std::f64::consts::TAU * f64::from(f) / self.rate) as f32;
        }
    }

    /// The gains of a line's absorption, scaled if their response would peak
    /// above 1.
    fn absorption_gains(a: Absorption, pole: [f32; 2]) -> [f32; 3] {
        let g = [Band::Low, Band::Mid, Band::High].map(|b| a.gain(b).get());
        rescale_bands(g, pole)
    }

    /// The constants of the chunk that starts now, from the coefficients' step.
    fn begin_chunk(&self, c: &FdnCoeffs, s: &mut FdnState) {
        let n = c.n;
        let depth = f64::from(c.depth.now());
        if s.fresh {
            s.fresh = false;
            for j in 0..self.max_lines {
                s.c_mod_next[j] = (depth * libm::sin(s.phase[j])) as f32;
            }
        }
        let dphi = std::f64::consts::TAU * f64::from(c.rate_hz) * CHUNK as f64 / self.rate;
        for j in 0..n {
            s.phase[j] += dphi;
            s.c_len[j] = c.len[j].now();
            s.c_mod_prev[j] = s.c_mod_next[j];
            s.c_mod_next[j] = (depth * libm::sin(s.phase[j])) as f32;
            let [low, mid, high] = c.gain[j].map(|g| g.now());
            s.c_gain[j] = [high, mid - high, low - mid];
            s.c_v[j] = c.v[j].now();
            s.c_in[j] = c.input[j].now();
            s.c_out[j] = c.out[j].map(|g| g.now());
        }
        // A straight line between two unit vectors is shorter than either, and
        // `I - 2 v v^T` of a short `v` is a contraction: bring it back to length.
        let len: f32 = s.c_v[..n].iter().map(|x| x * x).sum::<f32>().sqrt();
        if len > 1.0e-6 {
            let k = c.v_len.now() / len;
            s.c_v[..n].iter_mut().for_each(|x| *x *= k);
        }
        s.c_mix = f32::from(u8::from(c.mix.target() > 0.5 || c.mix.moving()));
        s.c_wet = c.wet.now();
        s.c_width = c.width.now();
        s.c_pre = c.pre_len.now();
    }
}

/// The energy the lines hold: the last `length` samples of each, which is the
/// whole state of a frozen network (the absorption's one-poles are not read
/// while every gain is 1). For the tests.
#[cfg(test)]
impl crate::Realized<FdnKernel> {
    pub(crate) fn state_energy(&mut self) -> f64 {
        let (k, c, s) = self.parts_mut();
        (0..c.n)
            .map(|j| {
                let len = c.len[j].now() as usize;
                (1..=len)
                    .map(|i| {
                        let x = s.buf[j * k.cap + (s.w + k.cap - i) % k.cap];
                        f64::from(x) * f64::from(x)
                    })
                    .sum::<f64>()
            })
            .sum()
    }
}

/// The gains `g = [low, mid, high]` of a three-band absorption whose one-poles are
/// `pole` (`exp(-2 pi fc / fs)` for the two crossovers), scaled to peak at 0.9999
/// if their response would exceed 1 anywhere; NaN, infinite or negative gains
/// are silence. The model's design applies it too, so the gains a network
/// carries are the ones this kernel runs.
pub(crate) fn rescale_bands(g: [f32; 3], pole: [f32; 2]) -> [f32; 3] {
    if !g.iter().all(|x| x.is_finite() && *x >= 0.0) {
        return [0.0; 3];
    }
    let peak = peak_response(g, pole);
    if peak > 1.0 {
        g.map(|x| (f64::from(x) * 0.9999 / peak) as f32)
    } else {
        g
    }
}

/// The largest `|y(w)|` of the absorption with gains `g = [low, mid, high]`,
/// over a grid of frequencies.
pub(crate) fn peak_response(g: [f32; 3], pole: [f32; 2]) -> f64 {
    let (low, mid, high) = (f64::from(g[0]), f64::from(g[1]), f64::from(g[2]));
    let (c2, c1) = (mid - high, low - mid);
    // |L1|, |L2| <= 1, so the response is at most the sum of the magnitudes
    // of its three terms: when that is under 1 (every gain falling with
    // frequency, the usual case) there is nothing to measure.
    let bound = high.abs() + c2.abs() + c1.abs();
    if bound <= 1.0 {
        return bound;
    }
    let pole = pole.map(f64::from);
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
    type Model = Network;
    type Bounds = Bounds;
    type Coeffs = FdnCoeffs;
    type State = FdnState;

    const STEP: usize = CHUNK;

    fn start(&self, _: &Network) -> (FdnCoeffs, FdnState) {
        (FdnCoeffs::new(self.max_lines), FdnState::new(self))
    }

    fn prepare(&mut self, bounds: &Bounds, setup: &Setup, c: &mut FdnCoeffs, s: &mut FdnState) {
        self.rate = setup.rate.get();
        self.ramp = ramp(self.rate);
        self.max_lines = bounds.lines;
        self.cap = bounds.line.ceil() as usize + 4;
        self.pre_cap = bounds.pre_delay.get() + 4;
        *c = FdnCoeffs::new(self.max_lines);
        self.set_poles(c, DEFAULT_CROSSOVER);
        *s = FdnState::new(self);
    }

    fn aim(&self, c: &mut FdnCoeffs, network: &Network) {
        let Some(Stage::Fdn(f)) = network.stages.iter().find(|s| matches!(s, Stage::Fdn(_))) else {
            return;
        };
        let n = f.lines.len().min(self.max_lines);
        if n == 0 {
            return;
        }
        c.n = n;
        let ramp = self.ramp;
        let max_len = (self.cap - 4) as f32;

        if let Some(Absorption::Bands { crossover, .. }) = f.lines.first().map(|l| l.absorption) {
            self.set_poles(c, [crossover[0].get(), crossover[1].get()]);
        }
        let flat = 1.0 / (n as f32).sqrt();
        for j in 0..n {
            let line = &f.lines[j];
            let target = if line.length.is_finite() {
                line.length.clamp(1.0, max_len)
            } else {
                1.0
            };
            c.len[j].aim_by(target, Steps::Rate(LENGTH_STEP));
            let g = Self::absorption_gains(line.absorption, c.pole);
            for (s, t) in c.gain[j].iter_mut().zip(g) {
                s.aim_by(t, ramp);
            }
            // A diagonal network is `m = 0`; its `v` is a unit vector (line 0)
            // like the rest, so a ramp to a mixing matrix moves between unit
            // vectors and `v_len` stays 1.
            let v = match &f.matrix {
                Matrix::Diagonal => f32::from(u8::from(j == 0)),
                Matrix::Householder => flat,
                Matrix::Reflection(v) if v.len() == n => v[j],
                _ => 0.0,
            };
            c.v[j].aim_by(if v.is_finite() { v } else { 0.0 }, ramp);
            let input = f.input.get(j).copied().unwrap_or(0.0);
            c.input[j].aim_by(if input.is_finite() { input } else { 0.0 }, ramp);
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
            c.out[j][0].aim_by(row(0), ramp);
            c.out[j][1].aim_by(row(1), ramp);
        }
        let want: f32 = c.v[..n]
            .iter()
            .map(|v| v.target() * v.target())
            .sum::<f32>()
            .sqrt();
        c.v_len.aim_by(want, ramp);
        let mix = f32::from(u8::from(!matches!(f.matrix, Matrix::Diagonal)));
        c.mix.aim_by(mix, ramp);
        for j in n..self.max_lines {
            c.input[j].aim_by(0.0, ramp);
            c.out[j].iter_mut().for_each(|s| s.aim_by(0.0, ramp));
        }
        let m = f.lines[0].modulation;
        let depth = if m.depth.is_finite() {
            m.depth.abs().min((self.cap as f32) * 0.25)
        } else {
            0.0
        };
        c.depth.aim_by(depth, ramp);
        let hz = m.rate.get();
        c.rate_hz = if hz.is_finite() {
            hz.clamp(0.0, 20.0)
        } else {
            0.5
        };
        let pre = network.pre_delay.get() as f32;
        c.pre_len.aim_by(
            pre.clamp(0.0, (self.pre_cap - 4) as f32),
            Steps::Rate(PRE_STEP),
        );
        let wet = network.wet.to_amplitude().get();
        c.wet.aim_by(if wet.is_finite() { wet } else { 0.0 }, ramp);
        let width = network.width.get();
        c.width.aim_by(
            if width.is_finite() {
                width.clamp(0.0, 4.0)
            } else {
                1.0
            },
            ramp,
        );
    }

    fn clear(&self, s: &mut FdnState) {
        s.buf.fill(0.0);
        s.pre.fill(0.0);
        s.w = 0;
        s.pre_w = 0;
        s.pole.fill([0.0; 4]);
        for (j, p) in s.phase.iter_mut().enumerate() {
            *p = self.start_phase(j);
        }
        s.fresh = true;
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }

    // The per-line arrays (`c_len`, `c_out`, `pole`, `buf` rows) are indexed by
    // one line number with the loop's scratch `d`; an iterator over any one of
    // them would only hide the others.
    #[allow(clippy::needless_range_loop)]
    fn run(
        &self,
        c: &FdnCoeffs,
        s: &mut FdnState,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        into: usize,
    ) {
        let n = c.n;
        if n == 0 {
            output
                .iter_mut()
                .for_each(|ch| ch[frames.clone()].fill(0.0));
            return;
        }
        let scale = 1.0 / input.len().max(1) as f32;
        let p = c.pole.map(|x| 1.0 - x);
        let mut d = [0.0f32; 16];
        let mut pos = into;
        for i in frames {
            if pos == 0 {
                self.begin_chunk(c, s);
            }
            let t = pos as f32 / CHUNK as f32;
            let x: f32 = input.iter().map(|ch| ch[i]).sum::<f32>() * scale;

            s.pre[s.pre_w] = x;
            let xd = read(&s.pre, s.pre_w, s.c_pre);
            s.pre_w = (s.pre_w + 1) % self.pre_cap;

            let (mut out_l, mut out_r) = (0.0f32, 0.0f32);
            for j in 0..n {
                let delay =
                    (s.c_len[j] + s.c_mod_prev[j] + (s.c_mod_next[j] - s.c_mod_prev[j]) * t)
                        .max(1.0);
                let line = &s.buf[j * self.cap..(j + 1) * self.cap];
                d[j] = read(line, s.w, delay);
                out_l += s.c_out[j][0] * d[j];
                out_r += s.c_out[j][1] * d[j];
            }

            let mut sum = 0.0f32;
            for j in 0..n {
                let q = &mut s.pole[j];
                q[0] += p[0] * (d[j] - q[0]);
                q[1] += p[0] * (q[0] - q[1]);
                q[2] += p[1] * (d[j] - q[2]);
                q[3] += p[1] * (q[2] - q[3]);
                let [high, c2, c1] = s.c_gain[j];
                d[j] = high * d[j] + c2 * q[3] + c1 * q[1];
                sum += s.c_v[j] * d[j];
            }
            // `m` scales the projection, so `m = 1` is the reflection to the bit.
            let sum = s.c_mix * sum;
            for j in 0..n {
                s.buf[j * self.cap + s.w] = s.c_in[j] * xd + d[j] - 2.0 * s.c_v[j] * sum;
            }
            s.w = (s.w + 1) % self.cap;

            let (mid, side) = ((out_l + out_r) * 0.5, (out_l - out_r) * 0.5);
            let wet = s.c_wet;
            let (l, r) = (
                (mid + s.c_width * side) * wet,
                (mid - s.c_width * side) * wet,
            );
            if let Some(ch) = output.get_mut(0) {
                ch[i] = l;
            }
            if let Some(ch) = output.get_mut(1) {
                ch[i] = r;
            }
            pos = (pos + 1) % CHUNK;
        }
    }
}
