//! The voice bank: every sub-voice's DSP state, stored as structure-of-arrays
//! across eight-wide SIMD lanes, and the one render loop that advances it.
//!
//! # Layout
//!
//! One lane per sub-voice. Voice `v`'s `U` unison sub-voices occupy lanes
//! `v*U .. v*U + U`, so a note and its unison stack sit next to each other and
//! share lane groups; eight lanes form a group, one `f32x8` per field. A
//! group none of whose lanes belongs to a sounding voice is skipped outright,
//! so the cost is proportional to the sounding sub-voices rounded up to a
//! group, not to `max_voices`.
//!
//! This avoids a boxed processor per sub-voice ticked per sample through a
//! virtual call. Surge's `QuadFilterChain` and Vital's `poly_float` are the
//! same idea.
//!
//! # What is uniform and what is per lane
//!
//! The oscillator waveform, the filter topology (and, for the SVF, its `Q` and
//! tap) and the envelope shape come from the `SynthConfig` and are the same for
//! every lane — so the whole bank runs one monomorphized loop, and "grouping by
//! filter type" is a single group. Pitch, cutoff, resonance, envelope position
//! and the two output gains are per lane.
//!
//! # Control rate
//!
//! The synth writes lane targets once per control step (at most
//! [`CONTROL_BLOCK`] frames apart, and at every MIDI event boundary). The render
//! ramps each lane linearly from where it is to its target across the step, so
//! pitch — including a portamento glide — changes every sample rather than in
//! stairs, and a cutoff sweep recomputes `tan` once per step instead of per
//! sample. The envelope is the exception: its stages are sample-accurate.

use crate::kernel::{self, V};
use crate::{Adsr, FilterType, OscillatorType, SvfMode};
use tutti_dsp::{LadderCoeffs, SvfCoeffs, SvfType};
use tutti_types::{Amplitude, Db, Hz, Phase, PhaseIncrement, Q, Resonance, SampleRate, Seconds};
use wide::u32x8;

/// The longest run of frames rendered between two control steps.
///
/// Sixteen frames is 0.33 ms at 48 kHz: short enough that a linear ramp
/// between control points is inaudible as a ramp, long enough that the
/// per-step work (a `tan` per moving cutoff, two `sqrt`s per lane for the pan
/// law) is amortized sixteen-fold.
pub(crate) const CONTROL_BLOCK: usize = 16;

/// Below these deltas a cutoff/resonance change does not recompute the filter
/// coefficients — the same guard `SvfFilterNode`/`LadderFilterNode` use.
const FREQ_EPS: f32 = 0.01;
const RES_EPS: f32 = 0.0001;

/// Where a lane's amplitude envelope is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvStage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// The envelope shape, converted to per-sample slopes at the current rate.
///
/// Every stage is a straight line, as `adsr_live`'s were: attack rises at a
/// fixed slope, decay falls from 1 to sustain, release falls from wherever the
/// level was to zero. A stage authored as zero seconds takes one sample rather
/// than zero, which keeps every slope finite.
#[derive(Debug, Clone, Copy)]
struct EnvShape {
    /// Rise per sample during attack; a full 0→1 attack takes `attack` seconds.
    attack_step: f32,
    /// Change per sample during decay, from 1.0 to `sustain`. Zero when
    /// `sustain` is 1.0, which makes decay a no-op.
    decay_step: f32,
    sustain: f32,
    /// Length of the release in samples. The slope is derived per lane from the
    /// level the release starts at, so a release always takes this long.
    release_frames: f32,
}

impl EnvShape {
    fn new(env: &Adsr, sample_rate: SampleRate) -> Self {
        // Where the types stop: these are per-sample slopes and a frame count
        // used as a divisor inside the SIMD loop, not quantities any unit
        // covers. `Seconds`/`Amplitude` come off here, once.
        let frames = |s: tutti_types::Seconds| s.to_samples(sample_rate).get().max(1) as f32;
        // Clamped to unity: the attack peaks at 1.0 and decay runs from there
        // to sustain. A sustain above 1.0 would make "decay" a rise, and a
        // retrigger from a held level above 1.0 would drop the note to the
        // attack peak — a click. Level above unity is the voice gain's job.
        let sustain = env.sustain.get().clamp(0.0, 1.0);
        Self {
            attack_step: 1.0 / frames(env.attack),
            decay_step: (sustain - 1.0) / frames(env.decay),
            sustain,
            release_frames: frames(env.release),
        }
    }

    /// Enter `stage` from `level`, skipping any stage that is already complete.
    /// Returns the stage actually entered and its `(level, slope, target)`.
    fn enter(&self, mut stage: EnvStage, level: f32) -> (EnvStage, f32, f32, f32) {
        loop {
            match stage {
                EnvStage::Attack if level >= 1.0 => stage = EnvStage::Decay,
                EnvStage::Attack => return (stage, level, self.attack_step, 1.0),
                // Decay starts from the attack's peak, so the level is 1.
                EnvStage::Decay if self.decay_step == 0.0 => stage = EnvStage::Sustain,
                EnvStage::Decay => return (stage, 1.0, self.decay_step, self.sustain),
                EnvStage::Sustain => return (stage, self.sustain, 0.0, self.sustain),
                EnvStage::Release if level <= 0.0 => stage = EnvStage::Idle,
                EnvStage::Release => {
                    return (stage, level, -level / self.release_frames, 0.0);
                }
                EnvStage::Idle => return (stage, 0.0, 0.0, 0.0),
            }
        }
    }

    /// The stage after `stage` reaches its target.
    fn next(stage: EnvStage) -> EnvStage {
        match stage {
            EnvStage::Attack => EnvStage::Decay,
            EnvStage::Decay => EnvStage::Sustain,
            EnvStage::Release => EnvStage::Idle,
            s => s,
        }
    }
}

/// The oscillator every lane runs.
#[derive(Debug, Clone, Copy)]
enum Osc {
    Sine,
    Saw,
    Pulse { width: f32 },
    Triangle,
    Noise,
}

/// The filter every lane runs.
#[derive(Debug, Clone, Copy)]
enum Filter {
    None,
    /// Simper SVF (`SvfFilterNode`'s topology). `q` and the output tap are
    /// fixed for the synth, so `m` is uniform; only `a1..a3` vary per lane.
    Svf {
        ty: SvfType,
        q: Q,
        m: [f32; 3],
    },
    /// Four-stage ladder with `tanh` feedback (`LadderFilterNode`'s topology),
    /// read at the fourth stage: the 24 dB/octave Moog response.
    Ladder,
}

/// The narrowest pulse the oscillator renders, as a fraction of the cycle.
const MIN_PULSE_WIDTH: f32 = 0.01;

const OSC_SINE: u8 = 0;
const OSC_SAW: u8 = 1;
const OSC_PULSE: u8 = 2;
const OSC_TRIANGLE: u8 = 3;
const OSC_NOISE: u8 = 4;
const FILTER_NONE: u8 = 0;
const FILTER_SVF: u8 = 1;
const FILTER_LADDER: u8 = 2;

/// Every sub-voice's DSP state, eight lanes to a group.
#[derive(Clone)]
pub(crate) struct VoiceBank {
    osc: Osc,
    filter: Filter,
    env_config: Adsr,
    shape: EnvShape,
    /// Per-sample one-pole coefficient the output gains glide with.
    gain_glide: f32,
    sample_rate: SampleRate,
    /// Sub-voices per voice: lane stride between consecutive voices.
    stride: usize,
    voices: usize,
    /// Next noise seed to hand out. Every lane array allocation draws fresh
    /// seeds from here, so a lane added by a unison resize never starts on a
    /// seed another lane already holds — seeding by lane index did, since a
    /// surviving sub-voice keeps its old stream after moving to a new index.
    next_seed: u32,

    // Oscillator.
    phase: Vec<V>,
    inc: Vec<V>,
    inc_target: Vec<V>,
    rng: Vec<u32x8>,
    pink: Vec<[V; 3]>,

    // Filter: integrator state (SVF uses two, the ladder four) and the
    // coefficients (SVF `a1..a3`; ladder `g1`, `k`), current and target.
    z: Vec<[V; 4]>,
    coef: Vec<[V; 3]>,
    coef_target: Vec<[V; 3]>,
    cutoff_seen: Vec<f32>,
    res_seen: Vec<f32>,

    // Amplitude envelope.
    env: Vec<V>,
    env_slope: Vec<V>,
    env_target: Vec<V>,
    stage: Vec<EnvStage>,

    // Output gains (pan law × unison gain × voice gain), current and target.
    // Unlike pitch and cutoff these glide by a one-pole at a fixed time
    // constant rather than ramping across the control step: `tick` takes a
    // control step every frame, so a step-long ramp there would be a one-sample
    // jump — a click on every velocity or pressure change.
    gain: Vec<[V; 2]>,
    gain_target: Vec<[V; 2]>,

    /// Groups holding at least one sounding lane this control step. Cleared by
    /// every render and re-marked by the next control step.
    live: Vec<bool>,
    out: [[f32; CONTROL_BLOCK]; 2],
}

impl std::fmt::Debug for VoiceBank {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceBank")
            .field("osc", &self.osc)
            .field("filter", &self.filter)
            .field("env_config", &self.env_config)
            .field("shape", &self.shape)
            .field("gain_glide", &self.gain_glide)
            .field("sample_rate", &self.sample_rate)
            .field("stride", &self.stride)
            .field("voices", &self.voices)
            .field("next_seed", &self.next_seed)
            .finish_non_exhaustive()
    }
}

impl VoiceBank {
    /// Build a bank for `voices` voices of `stride` sub-voices each.
    ///
    /// Allocates every lane array; control-thread only.
    pub(crate) fn new(
        oscillator: OscillatorType,
        filter: &FilterType,
        envelope: &Adsr,
        sample_rate: SampleRate,
        voices: usize,
        stride: usize,
    ) -> Self {
        let osc = match oscillator {
            OscillatorType::Sine => Osc::Sine,
            OscillatorType::Saw => Osc::Saw,
            // Clamped short of both ends: at 0 or 1 the two edges coincide and
            // their PolyBLEP corrections overlap into a spike rather than
            // cancelling, and the "pulse" is DC.
            OscillatorType::Square { pulse_width } => Osc::Pulse {
                width: pulse_width.clamp(MIN_PULSE_WIDTH, 1.0 - MIN_PULSE_WIDTH),
            },
            OscillatorType::Triangle => Osc::Triangle,
            OscillatorType::Noise => Osc::Noise,
        };
        let filter = match *filter {
            FilterType::None => Filter::None,
            FilterType::Moog { .. } => Filter::Ladder,
            FilterType::Svf { q, mode, .. } => {
                let ty = match mode {
                    SvfMode::Lowpass => SvfType::LowPass,
                    SvfMode::Highpass => SvfType::HighPass,
                    SvfMode::Bandpass => SvfType::BandPass,
                    SvfMode::Notch => SvfType::Notch,
                };
                // The output mix depends on the tap and `Q` alone, so any
                // cutoff gives the same `m`.
                let c = SvfCoeffs::new(ty, Hz(1000.0), q, Db(0.0), sample_rate);
                Filter::Svf {
                    ty,
                    q,
                    m: [c.m0 as f32, c.m1 as f32, c.m2 as f32],
                }
            }
        };
        let mut bank = Self {
            osc,
            filter,
            env_config: *envelope,
            shape: EnvShape::new(envelope, sample_rate),
            gain_glide: GAIN_GLIDE.one_pole_coeff(sample_rate),
            sample_rate,
            stride: stride.max(1),
            voices,
            next_seed: 1,
            phase: Vec::new(),
            inc: Vec::new(),
            inc_target: Vec::new(),
            rng: Vec::new(),
            pink: Vec::new(),
            z: Vec::new(),
            coef: Vec::new(),
            coef_target: Vec::new(),
            cutoff_seen: Vec::new(),
            res_seen: Vec::new(),
            env: Vec::new(),
            env_slope: Vec::new(),
            env_target: Vec::new(),
            stage: Vec::new(),
            gain: Vec::new(),
            gain_target: Vec::new(),
            live: Vec::new(),
            out: [[0.0; CONTROL_BLOCK]; 2],
        };
        bank.allocate_lanes();
        bank
    }

    fn lanes(&self) -> usize {
        self.voices * self.stride
    }

    fn groups(&self) -> usize {
        self.lanes().div_ceil(kernel::LANES)
    }

    /// (Re)size every lane array to the current `voices * stride`, zeroed.
    fn allocate_lanes(&mut self) {
        let groups = self.groups();
        let lanes = groups * kernel::LANES;
        let zero = V::ZERO;
        self.phase = vec![zero; groups];
        self.inc = vec![zero; groups];
        self.inc_target = vec![zero; groups];
        // Distinct nonzero seeds per lane: xorshift cannot leave zero, and two
        // lanes with one seed would play the same noise in both ears.
        let mut seeds = (self.next_seed..).map(seed_from);
        self.rng = (0..groups)
            .map(|_| u32x8::new(core::array::from_fn(|_| seeds.next().unwrap_or(1))))
            .collect();
        self.next_seed = self.next_seed.wrapping_add(lanes as u32);
        self.pink = vec![[zero; 3]; groups];
        self.z = vec![[zero; 4]; groups];
        self.coef = vec![[zero; 3]; groups];
        self.coef_target = vec![[zero; 3]; groups];
        // NaN never compares within epsilon, so the first `set_filter` always
        // computes coefficients.
        self.cutoff_seen = vec![f32::NAN; lanes];
        self.res_seen = vec![f32::NAN; lanes];
        self.env = vec![zero; groups];
        self.env_slope = vec![zero; groups];
        self.env_target = vec![zero; groups];
        self.stage = vec![EnvStage::Idle; lanes];
        self.gain = vec![[zero; 2]; groups];
        self.gain_target = vec![[zero; 2]; groups];
        self.live = vec![false; groups];
    }

    /// Change the unison stride, keeping every surviving sub-voice's state.
    ///
    /// Allocates; control-thread only (it backs `set_unison_voice_count`).
    pub(crate) fn resize_stride(&mut self, stride: usize) {
        let stride = stride.max(1);
        if stride == self.stride {
            return;
        }
        let old = self.clone();
        self.stride = stride;
        self.allocate_lanes();
        for v in 0..self.voices {
            for s in 0..stride {
                let to = v * stride + s;
                if s < old.stride {
                    self.copy_lane_from(&old, v * old.stride + s, to);
                } else {
                    // A sub-voice added under a sounding note joins it where
                    // its first sub-voice is — same envelope position, same
                    // phase — rather than sitting idle until the next note-on.
                    // Only the noise seed stays its own, or the new lane would
                    // replay the first one's noise.
                    let (g, k) = split(to);
                    let seed = self.rng[g].as_array()[k];
                    self.copy_lane_from(&old, v * old.stride, to);
                    self.rng[g].as_mut_array()[k] = seed;
                }
            }
        }
    }

    fn copy_lane_from(&mut self, src: &Self, from: usize, to: usize) {
        let (fg, fk) = split(from);
        let (tg, tk) = split(to);
        macro_rules! copy {
            ($($field:ident),*) => {
                $(self.$field[tg].as_mut_array()[tk] = src.$field[fg].as_array()[fk];)*
            };
        }
        copy!(phase, inc, inc_target, env, env_slope, env_target);
        self.rng[tg].as_mut_array()[tk] = src.rng[fg].as_array()[fk];
        for i in 0..3 {
            self.pink[tg][i].as_mut_array()[tk] = src.pink[fg][i].as_array()[fk];
            self.coef[tg][i].as_mut_array()[tk] = src.coef[fg][i].as_array()[fk];
            self.coef_target[tg][i].as_mut_array()[tk] = src.coef_target[fg][i].as_array()[fk];
        }
        for i in 0..4 {
            self.z[tg][i].as_mut_array()[tk] = src.z[fg][i].as_array()[fk];
        }
        for i in 0..2 {
            self.gain[tg][i].as_mut_array()[tk] = src.gain[fg][i].as_array()[fk];
            self.gain_target[tg][i].as_mut_array()[tk] = src.gain_target[fg][i].as_array()[fk];
        }
        self.cutoff_seen[to] = src.cutoff_seen[from];
        self.res_seen[to] = src.res_seen[from];
        self.stage[to] = src.stage[from];
    }

    /// Lane of voice `voice`'s sub-voice `sub`.
    #[inline]
    pub(crate) fn lane(&self, voice: usize, sub: usize) -> usize {
        voice * self.stride + sub
    }

    pub(crate) fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Re-derive everything rate-dependent. Pitch increments and filter
    /// coefficients are rederived at the next control step, from the `Hz` the
    /// voices hold; the envelope slopes are rederived here.
    pub(crate) fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.sample_rate = sample_rate;
        self.shape = EnvShape::new(&self.env_config, sample_rate);
        self.gain_glide = GAIN_GLIDE.one_pole_coeff(sample_rate);
        // The SVF's output mix depends on the tap and `Q`, not the rate, so
        // only the per-lane coefficients need recomputing.
        self.cutoff_seen.fill(f32::NAN);
        self.res_seen.fill(f32::NAN);
    }

    /// Zero every lane's state: silence, idle envelopes, cleared filters.
    ///
    /// Fills in place rather than reallocating, so it is safe wherever the
    /// host calls `reset`. The noise generators keep running: a reset that
    /// reseeded them would replay the same noise after every transport stop.
    pub(crate) fn reset(&mut self) {
        let zero = V::ZERO;
        for field in [
            &mut self.phase,
            &mut self.inc,
            &mut self.inc_target,
            &mut self.env,
            &mut self.env_slope,
            &mut self.env_target,
        ] {
            field.fill(zero);
        }
        self.pink.fill([zero; 3]);
        self.z.fill([zero; 4]);
        self.coef.fill([zero; 3]);
        self.coef_target.fill([zero; 3]);
        self.gain.fill([zero; 2]);
        self.gain_target.fill([zero; 2]);
        self.cutoff_seen.fill(f32::NAN);
        self.res_seen.fill(f32::NAN);
        self.stage.fill(EnvStage::Idle);
        self.live.fill(false);
    }

    // --- Control-step writes, one lane at a time -------------------------

    /// Aim the lane's oscillator at `freq`.
    pub(crate) fn set_pitch(&mut self, lane: usize, freq: Hz) {
        let inc = PhaseIncrement::per_sample(freq, self.sample_rate).get();
        set(
            &mut self.inc_target,
            lane,
            inc.clamp(0.0, kernel::MAX_INCREMENT),
        );
    }

    /// Aim the filters of `lanes` consecutive lanes from `first` — one voice's
    /// sub-voices, which share a cutoff — at `cutoff` (and, for the ladder,
    /// `resonance`). Computes the coefficients once for all of them, and only
    /// when either value moved past its epsilon.
    pub(crate) fn set_filter(
        &mut self,
        first: usize,
        lanes: usize,
        cutoff: Hz,
        resonance: Resonance,
    ) {
        let (c, r) = (cutoff.get(), resonance.get());
        let unchanged = (c - self.cutoff_seen[first]).abs() <= FREQ_EPS
            && (r - self.res_seen[first]).abs() <= RES_EPS;
        if unchanged {
            return;
        }
        let coef = match self.filter {
            Filter::None => return,
            Filter::Svf { ty, q, .. } => {
                let s = SvfCoeffs::new(ty, cutoff, q, Db(0.0), self.sample_rate);
                [s.a1 as f32, s.a2 as f32, s.a3 as f32]
            }
            Filter::Ladder => {
                let l = LadderCoeffs::new(cutoff, resonance, self.sample_rate);
                [(l.g / (1.0 + l.g)) as f32, l.k as f32, 0.0]
            }
        };
        for lane in first..first + lanes {
            self.cutoff_seen[lane] = c;
            self.res_seen[lane] = r;
            let (g, k) = split(lane);
            for (i, c) in coef.into_iter().enumerate() {
                self.coef_target[g][i].as_mut_array()[k] = c;
            }
        }
    }

    /// Set the lane's left and right output gains.
    pub(crate) fn set_gains(&mut self, lane: usize, left: Amplitude, right: Amplitude) {
        let (g, k) = split(lane);
        self.gain_target[g][0].as_mut_array()[k] = left.get();
        self.gain_target[g][1].as_mut_array()[k] = right.get();
    }

    /// Jump the lane's pitch and filter straight to their targets, so a new
    /// note starts on its own pitch and cutoff rather than gliding in from the
    /// previous note's. Changes the waveform's slope, never its value, so it
    /// cannot click.
    pub(crate) fn snap_tone(&mut self, lane: usize) {
        let (g, k) = split(lane);
        let inc = self.inc_target[g].as_array()[k];
        self.inc[g].as_mut_array()[k] = inc;
        for i in 0..3 {
            let c = self.coef_target[g][i].as_array()[k];
            self.coef[g][i].as_mut_array()[k] = c;
        }
    }

    /// Jump the lane's output gains straight to their targets.
    ///
    /// Only for a silent lane: on a sounding one this is a step in the output
    /// — a click — which is why a stolen voice ramps its gain instead.
    pub(crate) fn snap_gain(&mut self, lane: usize) {
        let (g, k) = split(lane);
        for i in 0..2 {
            let c = self.gain_target[g][i].as_array()[k];
            self.gain[g][i].as_mut_array()[k] = c;
        }
    }

    /// Prepare a silent lane for a new note: oscillator at `phase`, filter
    /// cleared.
    ///
    /// Only for a lane whose envelope is at zero — resetting a sounding lane's
    /// phase is an audible click, which is why a stolen or retriggered voice
    /// skips this and keeps running.
    pub(crate) fn start(&mut self, lane: usize, phase: Phase) {
        let (g, k) = split(lane);
        self.phase[g].as_mut_array()[k] = phase.get();
        for i in 0..4 {
            self.z[g][i].as_mut_array()[k] = 0.0;
        }
        for i in 0..3 {
            self.pink[g][i].as_mut_array()[k] = 0.0;
        }
    }

    /// Open the lane's gate: attack from wherever the envelope is.
    pub(crate) fn gate_on(&mut self, lane: usize) {
        self.enter(lane, EnvStage::Attack);
    }

    /// Close the lane's gate: release from wherever the envelope is.
    pub(crate) fn gate_off(&mut self, lane: usize) {
        self.enter(lane, EnvStage::Release);
    }

    /// Silence the lane immediately (All Sound Off): envelope to zero, filter
    /// cleared.
    pub(crate) fn kill(&mut self, lane: usize) {
        self.enter_at(lane, EnvStage::Idle, 0.0);
        self.start(lane, Phase::START);
        self.set_gains(lane, Amplitude::SILENT, Amplitude::SILENT);
        self.snap_gain(lane);
    }

    /// [`kill`](Self::kill) every sub-voice of voice `voice`.
    pub(crate) fn kill_voice(&mut self, voice: usize) {
        for sub in 0..self.stride {
            self.kill(self.lane(voice, sub));
        }
    }

    fn enter(&mut self, lane: usize, stage: EnvStage) {
        let (g, k) = split(lane);
        let level = self.env[g].as_array()[k];
        self.enter_at(lane, stage, level);
    }

    fn enter_at(&mut self, lane: usize, stage: EnvStage, level: f32) {
        let (stage, level, slope, target) = self.shape.enter(stage, level);
        let (g, k) = split(lane);
        self.stage[lane] = stage;
        self.env[g].as_mut_array()[k] = level;
        self.env_slope[g].as_mut_array()[k] = slope;
        self.env_target[g].as_mut_array()[k] = target;
    }

    /// Include this lane's group in the next render.
    #[inline]
    pub(crate) fn mark_live(&mut self, lane: usize) {
        self.live[lane / kernel::LANES] = true;
    }

    /// Whether the lane's envelope has finished its release.
    pub(crate) fn is_idle(&self, lane: usize) -> bool {
        self.stage[lane] == EnvStage::Idle
    }

    /// The lane's envelope level, `0.0..=1.0`.
    pub(crate) fn envelope(&self, lane: usize) -> Amplitude {
        let (g, k) = split(lane);
        Amplitude(self.env[g].as_array()[k])
    }

    /// The lane's oscillator phase. Observability for tests.
    #[cfg(test)]
    pub(crate) fn phase(&self, lane: usize) -> f32 {
        let (g, k) = split(lane);
        self.phase[g].as_array()[k]
    }

    /// Each lane's current noise-generator state. Observability for tests.
    #[cfg(test)]
    pub(crate) fn noise_states(&self) -> Vec<u32> {
        self.rng.iter().flat_map(|g| g.to_array()).collect()
    }

    // --- Render ----------------------------------------------------------

    /// Render `n <= CONTROL_BLOCK` frames of every live group and return the
    /// summed `(left, right)` mix.
    ///
    /// Clears the live set, so a lane only sounds in the step after the
    /// control code marks it.
    pub(crate) fn render(&mut self, n: usize) -> (&[f32], &[f32]) {
        debug_assert!(n <= CONTROL_BLOCK);
        let mut acc = [[V::ZERO; 2]; CONTROL_BLOCK];
        let osc = match self.osc {
            Osc::Sine => OSC_SINE,
            Osc::Saw => OSC_SAW,
            Osc::Pulse { .. } => OSC_PULSE,
            Osc::Triangle => OSC_TRIANGLE,
            Osc::Noise => OSC_NOISE,
        };
        let filter = match self.filter {
            Filter::None => FILTER_NONE,
            Filter::Svf { .. } => FILTER_SVF,
            Filter::Ladder => FILTER_LADDER,
        };
        for g in 0..self.live.len() {
            if !self.live[g] {
                continue;
            }
            self.live[g] = false;
            // One monomorphized loop per (oscillator, filter) pair: the match
            // runs once per group per step, never per sample.
            macro_rules! dispatch {
                ($($o:ident),*; $($f:ident),*) => {
                    dispatch!(@o [$($o),*] [$($f),*])
                };
                (@o [$($o:ident),*] $fs:tt) => {
                    match osc {
                        $($o => dispatch!(@f $o $fs),)*
                        _ => unreachable!(),
                    }
                };
                (@f $o:ident [$($f:ident),*]) => {
                    match filter {
                        $($f => self.render_group::<$o, $f>(g, n, &mut acc),)*
                        _ => unreachable!(),
                    }
                };
            }
            dispatch!(OSC_SINE, OSC_SAW, OSC_PULSE, OSC_TRIANGLE, OSC_NOISE;
                      FILTER_NONE, FILTER_SVF, FILTER_LADDER);
        }
        for (i, frame) in acc.iter().enumerate().take(n) {
            self.out[0][i] = frame[0].reduce_add();
            self.out[1][i] = frame[1].reduce_add();
        }
        (&self.out[0][..n], &self.out[1][..n])
    }

    #[inline(always)]
    fn render_group<const OSC: u8, const FILTER: u8>(
        &mut self,
        g: usize,
        n: usize,
        acc: &mut [[V; 2]; CONTROL_BLOCK],
    ) {
        let ramp = V::splat(1.0 / n as f32);

        let mut phase = self.phase[g];
        let mut inc = self.inc[g];
        let inc_target = self.inc_target[g];
        let inc_step = (inc_target - inc) * ramp;
        let width = match self.osc {
            Osc::Pulse { width } => V::splat(width),
            _ => V::splat(0.5),
        };
        let mut rng = self.rng[g];
        let mut pink = self.pink[g];

        let [mut z0, mut z1, mut z2, mut z3] = self.z[g];
        let mut c = self.coef[g];
        let c_target = self.coef_target[g];
        let c_step = [
            (c_target[0] - c[0]) * ramp,
            (c_target[1] - c[1]) * ramp,
            (c_target[2] - c[2]) * ramp,
        ];
        let m = match self.filter {
            Filter::Svf { m, .. } => m.map(V::splat),
            _ => [V::ZERO; 3],
        };

        let mut env = self.env[g];
        let mut slope = self.env_slope[g];
        let mut target = self.env_target[g];

        let [mut gain_l, mut gain_r] = self.gain[g];
        let [gain_l_target, gain_r_target] = self.gain_target[g];
        let glide = V::splat(self.gain_glide);

        let two = V::splat(2.0);
        for frame in acc.iter_mut().take(n) {
            inc += inc_step;
            // `max` keeps an idle lane's zero increment from dividing by zero;
            // the corrections it feeds are masked to zero there anyway.
            let dt = inc.max(V::splat(1e-9));
            let x = match OSC {
                OSC_SINE => kernel::sine(phase),
                OSC_SAW => kernel::saw(phase, dt, V::ONE / dt),
                OSC_PULSE => kernel::pulse(phase, width, dt, V::ONE / dt),
                OSC_TRIANGLE => kernel::triangle(phase, dt, V::ONE / dt),
                _ => kernel::pink(kernel::white(&mut rng), &mut pink),
            };
            phase = kernel::wrap_once(phase + inc);

            let y = match FILTER {
                FILTER_SVF => {
                    c[0] += c_step[0];
                    c[1] += c_step[1];
                    c[2] += c_step[2];
                    let v3 = x - z1;
                    let v1 = c[0] * z0 + c[1] * v3;
                    let v2 = z1 + c[1] * z0 + c[2] * v3;
                    z0 = two * v1 - z0;
                    z1 = two * v2 - z1;
                    m[0] * x + m[1] * v1 + m[2] * v2
                }
                FILTER_LADDER => {
                    c[0] += c_step[0];
                    c[1] += c_step[1];
                    let g1 = c[0];
                    let u = kernel::tanh(x - c[1] * z3);
                    let v = g1 * (u - z0);
                    let lp1 = v + z0;
                    z0 = lp1 + v;
                    let v = g1 * (lp1 - z1);
                    let lp2 = v + z1;
                    z1 = lp2 + v;
                    let v = g1 * (lp2 - z2);
                    let lp3 = v + z2;
                    z2 = lp3 + v;
                    let v = g1 * (lp3 - z3);
                    let lp4 = v + z3;
                    z3 = lp4 + v;
                    lp4
                }
                _ => x,
            };

            env += slope;
            // A lane reached its stage's target when it has moved onto or past
            // it in the direction of travel; a flat stage never "reaches".
            let reached = ((env - target) * slope).simd_ge(V::ZERO) & slope.simd_ne(V::ZERO);
            if reached.any() {
                env = reached.select(target, env);
                (slope, target) = self.advance_envelopes(g, env, slope, target, reached);
            }

            gain_l = (gain_l_target - gain_l).mul_add(glide, gain_l);
            gain_r = (gain_r_target - gain_r).mul_add(glide, gain_r);
            let out = y * env;
            frame[0] += out * gain_l;
            frame[1] += out * gain_r;
        }

        self.phase[g] = phase;
        // Land exactly on the targets rather than on the accumulated ramp, so
        // rounding in the ramp never builds up across steps.
        self.inc[g] = inc_target;
        self.rng[g] = rng;
        self.pink[g] = pink;
        self.z[g] = [z0, z1, z2, z3];
        self.coef[g] = c_target;
        self.env[g] = env;
        self.env_slope[g] = slope;
        self.env_target[g] = target;
        self.gain[g] = [gain_l, gain_r];
    }

    /// Move every lane in `reached` on to its next envelope stage.
    ///
    /// Out of line and scalar: a lane changes stage a handful of times per
    /// note, so this runs on a vanishing fraction of samples.
    #[cold]
    #[inline(never)]
    fn advance_envelopes(&mut self, g: usize, env: V, slope: V, target: V, reached: V) -> (V, V) {
        let reached = reached.to_bitmask();
        let level = env.to_array();
        let mut slope = slope.to_array();
        let mut target = target.to_array();
        for k in 0..kernel::LANES {
            if reached & (1 << k) == 0 {
                continue;
            }
            let lane = g * kernel::LANES + k;
            let next = EnvShape::next(self.stage[lane]);
            let (stage, _, s, t) = self.shape.enter(next, level[k]);
            self.stage[lane] = stage;
            slope[k] = s;
            target[k] = t;
        }
        (V::new(slope), V::new(target))
    }
}

/// Time constant of the output-gain glide. Half a millisecond settles a
/// velocity change to 1% in about 2.3 ms — inaudible as a fade, long enough
/// that no single sample steps by more than 4% of the change at 48 kHz.
const GAIN_GLIDE: Seconds = Seconds(0.0005);

/// The `n`th noise seed: a bijective mix (odd multiply, xorshift) of the
/// counter, so distinct counters give distinct seeds, forced nonzero.
fn seed_from(n: u32) -> u32 {
    let mut x = n.wrapping_mul(0x9E37_79B9);
    x ^= x >> 16;
    x = x.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 13;
    if x == 0 { 0x6D2B_79F5 } else { x }
}

/// Group and lane-within-group of a flat lane index.
#[inline(always)]
fn split(lane: usize) -> (usize, usize) {
    (lane / kernel::LANES, lane % kernel::LANES)
}

#[inline(always)]
fn set(field: &mut [V], lane: usize, value: f32) {
    let (g, k) = split(lane);
    field[g].as_mut_array()[k] = value;
}

#[cfg(test)]
mod tests;
