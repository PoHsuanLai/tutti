use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{dsp::DEFAULT_SR, AudioUnit, BufferMut, BufferRef, SignalFrame};

use super::envelope::EnvelopeFollower;
use super::utils::{amplitude_to_db, compute_limiter_gain, db_to_amplitude, smooth_envelope};
use crate::buffer::{CircularBuffer, MonotonicMinDeque};
use crate::delay::StereoPair;
use tutti_core::{Db, Param, Seconds};

/// Lookahead ring buffers + sliding-window-minimum tracker for the limiter.
/// Split out so `LimiterNode` reads as a list of parameters plus a lookahead
/// block, not a flat field soup.
#[derive(Clone)]
struct LookaheadRing {
    buffers: StereoPair<CircularBuffer<f32>>,
    min_deque: MonotonicMinDeque,
    sample_counter: u64,
    lookahead_samples: usize,
}

impl LookaheadRing {
    fn new(lookahead_samples: usize) -> Self {
        let n = lookahead_samples.max(1);
        Self {
            buffers: StereoPair::new(CircularBuffer::new(n), CircularBuffer::new(n)),
            min_deque: MonotonicMinDeque::new(n),
            sample_counter: 0,
            lookahead_samples: n,
        }
    }

    fn resize(&mut self, lookahead_samples: usize) {
        *self = Self::new(lookahead_samples);
    }

    fn clear(&mut self) {
        self.buffers.l.clear();
        self.buffers.r.clear();
        self.min_deque.clear();
        self.sample_counter = 0;
    }

    #[inline]
    fn footprint(&self) -> usize {
        (self.buffers.l.len() + self.buffers.r.len()) * core::mem::size_of::<f32>()
            + self.min_deque.capacity() * core::mem::size_of::<(u64, f32)>()
    }

    /// Write `(left, right)` into the lookahead, push `gain` into the sliding
    /// minimum, and return `(delayed_l, delayed_r, window_min_gain)`.
    #[inline]
    fn step(&mut self, left: f32, right: f32, gain: f32) -> (f32, f32, f32) {
        let delayed_l = self.buffers.l.read_back(self.lookahead_samples - 1);
        let delayed_r = self.buffers.r.read_back(self.lookahead_samples - 1);

        self.buffers.l.push(left);
        self.buffers.r.push(right);

        self.min_deque.push(self.sample_counter, gain);
        let window_start = self
            .sample_counter
            .saturating_sub(self.lookahead_samples as u64 - 1);
        self.min_deque.evict_older_than(window_start);
        self.sample_counter = self.sample_counter.wrapping_add(1);

        let min_gain = self.min_deque.min().unwrap_or(gain);
        (delayed_l, delayed_r, min_gain)
    }
}


/// Lookahead limiter with stereo-linked gain reduction.
/// 2 inputs (L/R), 2 outputs (L/R).
///
/// The minimum gain over the lookahead window is tracked with a monotonic
/// deque, so each sample costs O(1) amortized regardless of lookahead length.
///
/// # Port layout & audio-rate modulation
///
/// The default node is 2-in / 2-out (audio L/R on ports 0/1). For audio-rate
/// modulation it can grow optional param-input ports after the audio inputs
/// (see [`LimiterNode::with_param_inputs`]) in the order **ceiling, then
/// threshold** (both dB): ceiling at index 2 if present, threshold next. Each
/// present port overrides its atomic per sample; the atomics still hold the
/// base. Absent → a plain 2-in/2-out node, bit-identical output to the
/// unmodulated path (the common case).
pub struct LimiterNode {
    threshold_db: Param<Db>,
    ceiling_db: Param<Db>,
    release: Param<Seconds>,

    ring: LookaheadRing,
    envelope: f32,
    gain_reduction_db: f32,
    sample_rate: f64,
    follower: EnvelopeFollower,
    /// When true, a ceiling param-input port (dB) follows the two audio inputs
    /// at index 2 and overrides the ceiling atomic per sample.
    mod_ceiling: bool,
    /// When true, a threshold param-input port (dB) follows the audio inputs
    /// (and the ceiling port if present) and overrides the threshold atomic
    /// per sample.
    mod_threshold: bool,
}

impl LimiterNode {
    pub fn new(threshold_db: impl Into<Db>, ceiling_db: impl Into<Db>) -> Self {
        let lookahead_secs = 0.005;
        let lookahead_samples = (lookahead_secs * DEFAULT_SR as f32).ceil() as usize;

        Self {
            threshold_db: Param::new(threshold_db.into()),
            ceiling_db: Param::new(ceiling_db.into()),
            release: Param::new(Seconds(0.1)),
            ring: LookaheadRing::new(lookahead_samples),
            envelope: 0.0,
            gain_reduction_db: 0.0,
            sample_rate: DEFAULT_SR,
            follower: EnvelopeFollower::new(0.0, 0.1, DEFAULT_SR),
            mod_ceiling: false,
            mod_threshold: false,
        }
    }

    /// A limiter with optional audio-rate ceiling / threshold param-input ports,
    /// appended after the two audio inputs in that order (ceiling first). Each
    /// present port overrides its atomic per sample; the atomics still hold the
    /// base.
    pub fn with_param_inputs(
        threshold_db: impl Into<Db>,
        ceiling_db: impl Into<Db>,
        mod_ceiling: bool,
        mod_threshold: bool,
    ) -> Self {
        let mut node = Self::new(threshold_db, ceiling_db);
        node.mod_ceiling = mod_ceiling;
        node.mod_threshold = mod_threshold;
        node
    }

    /// Input-port index of the ceiling param input, if present (right after the
    /// two audio inputs).
    #[inline]
    pub fn ceiling_port(&self) -> Option<usize> {
        self.mod_ceiling.then_some(2)
    }

    /// Input-port index of the threshold param input, if present (after the
    /// audio inputs and the ceiling port).
    #[inline]
    pub fn threshold_port(&self) -> Option<usize> {
        self.mod_threshold.then_some(2 + self.mod_ceiling as usize)
    }

    /// Effective per-sample (threshold_db, ceiling_db): a present param port
    /// overrides the corresponding atomic. `read` reads input port `p`.
    /// No extra clamp — the dB setters store unclamped.
    #[inline]
    fn effective_params(&self, read: impl Fn(usize) -> f32) -> (tutti_core::Db, tutti_core::Db) {
        let threshold = self
            .threshold_port()
            .map_or_else(|| self.threshold_db.load(), |p| Db(read(p)));
        let ceiling = self
            .ceiling_port()
            .map_or_else(|| self.ceiling_db.load(), |p| Db(read(p)));
        (threshold, ceiling)
    }

    pub fn with_lookahead(mut self, lookahead: impl Into<Seconds>) -> Self {
        let secs = lookahead.into().get();
        let samples = (secs * self.sample_rate as f32).ceil() as usize;
        self.ring.resize(samples.max(1));
        self
    }

    pub fn with_release(self, release_secs: impl Into<Seconds>) -> Self {
        self.release
            .store(Seconds(release_secs.into().get().max(0.001)));
        self
    }

    pub fn threshold(&self) -> Arc<AtomicF32> {
        self.threshold_db.as_atomic()
    }

    pub fn ceiling(&self) -> Arc<AtomicF32> {
        self.ceiling_db.as_atomic()
    }

    pub fn release_time(&self) -> Arc<AtomicF32> {
        self.release.as_atomic()
    }

    pub fn set_threshold(&self, db: impl Into<Db>) {
        self.threshold_db.store(db.into());
    }

    pub fn set_ceiling(&self, db: impl Into<Db>) {
        self.ceiling_db.store(db.into());
    }

    pub fn set_release(&self, secs: impl Into<Seconds>) {
        self.release.store(Seconds(secs.into().get().max(0.001)));
    }

    pub fn gain_reduction_db(&self) -> f32 {
        self.gain_reduction_db
    }

    #[inline]
    fn update_coefficients(&mut self) {
        self.follower
            .update_coefficients(Seconds(0.0), self.release.load());
    }

    #[inline]
    fn compute_gain(
        &self,
        peak_db: tutti_core::Db,
        threshold: tutti_core::Db,
        ceiling: tutti_core::Db,
    ) -> f32 {
        compute_limiter_gain(peak_db, threshold, ceiling).get()
    }

    /// Process one sample using explicit threshold/ceiling (the modulated path;
    /// the fast path passes the atomics).
    #[inline]
    fn process_sample_with(
        &mut self,
        left: f32,
        right: f32,
        threshold: tutti_core::Db,
        ceiling: tutti_core::Db,
    ) -> (f32, f32) {
        let peak = left.abs().max(right.abs());
        let peak_db = amplitude_to_db(peak);

        let target_gain = self.compute_gain(peak_db, threshold, ceiling);

        if target_gain < self.envelope {
            self.envelope = target_gain;
        } else {
            self.envelope =
                smooth_envelope(self.envelope, target_gain, self.follower.release_coeff());
        }

        let (delayed_l, delayed_r, min_gain) = self.ring.step(left, right, self.envelope);

        // Metering only. Guard log10(0) so a fully-closed gain reports a large
        // finite reduction instead of +inf.
        self.gain_reduction_db = if min_gain > 0.0 {
            -20.0 * min_gain.log10()
        } else {
            96.0
        };
        (delayed_l * min_gain, delayed_r * min_gain)
    }
}

impl AudioUnit for LimiterNode {
    fn inputs(&self) -> usize {
        2 + self.mod_ceiling as usize + self.mod_threshold as usize
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.ring.clear();
        self.envelope = 0.0;
        self.gain_reduction_db = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate_f64: f64 = sample_rate.get();
        let lookahead_secs = self.ring.lookahead_samples as f64 / self.sample_rate;
        self.sample_rate = sample_rate_f64;
        self.follower
            .set_sample_rate(sample_rate, Seconds(0.0), self.release.load());
        let new_samples = (lookahead_secs * sample_rate_f64).ceil() as usize;
        self.ring.resize(new_samples.max(1));
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.update_coefficients();
        let left = input[0];
        let right = if input.len() > 1 { input[1] } else { input[0] };
        // A present ceiling/threshold port overrides its atomic.
        let (threshold, ceiling) = self.effective_params(|p| input[p]);
        let (out_l, out_r) = self.process_sample_with(left, right, threshold, ceiling);
        output[0] = out_l;
        if output.len() > 1 {
            output[1] = out_r;
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.update_coefficients();
        let has_stereo = input.channels() > 1;
        let ceiling_port = self.ceiling_port();
        let threshold_port = self.threshold_port();

        // Fast path: no param ports — read the atomics once per block.
        if ceiling_port.is_none() && threshold_port.is_none() {
            let threshold = self.threshold_db.load();
            let ceiling = self.ceiling_db.load();
            for i in 0..size {
                let left = input.at_f32(0, i);
                let right = if has_stereo { input.at_f32(1, i) } else { left };
                let (out_l, out_r) = self.process_sample_with(left, right, threshold, ceiling);
                output.set_f32(0, i, out_l);
                if output.channels() > 1 {
                    output.set_f32(1, i, out_r);
                }
            }
            return;
        }

        // Modulated path: read the active port(s) per sample.
        let base_threshold = self.threshold_db.load();
        let base_ceiling = self.ceiling_db.load();
        for i in 0..size {
            let left = input.at_f32(0, i);
            let right = if has_stereo { input.at_f32(1, i) } else { left };
            let threshold = threshold_port.map_or(base_threshold, |p| Db(input.at_f32(p, i)));
            let ceiling = ceiling_port.map_or(base_ceiling, |p| Db(input.at_f32(p, i)));
            let (out_l, out_r) = self.process_sample_with(left, right, threshold, ceiling);
            output.set_f32(0, i, out_l);
            if output.channels() > 1 {
                output.set_f32(1, i, out_r);
            }
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::Threshold => self.set_threshold(value),
                tutti_core::UnitParam::Ceiling => self.set_ceiling(value),
                tutti_core::UnitParam::Release => self.set_release(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::LIMITER_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(2);
        let latency = self.ring.lookahead_samples as f64;
        out.set(0, input.at(0).delay(latency));
        out.set(1, input.at(1).delay(latency));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>() + self.ring.footprint()
    }
}

impl Clone for LimiterNode {
    fn clone(&self) -> Self {
        Self {
            threshold_db: self.threshold_db.handle(),
            ceiling_db: self.ceiling_db.handle(),
            release: self.release.handle(),
            ring: self.ring.clone(),
            envelope: self.envelope,
            gain_reduction_db: self.gain_reduction_db,
            sample_rate: self.sample_rate,
            follower: self.follower.clone(),
            mod_ceiling: self.mod_ceiling,
            mod_threshold: self.mod_threshold,
        }
    }
}

/// Hard clipper at ceiling. No lookahead, zero latency.
/// 2 inputs (L/R), 2 outputs (L/R).
///
/// # Port layout & audio-rate modulation
///
/// The default node is 2-in / 2-out (audio L/R on ports 0/1). For audio-rate
/// ceiling modulation it can grow **one optional ceiling param-input port (dB)
/// after the audio inputs** at index 2 (see
/// [`BrickwallLimiter::with_param_inputs`]); present → overrides the ceiling
/// atomic per sample, absent → a plain 2-in/2-out node, bit-identical to the
/// unmodulated path.
pub struct BrickwallLimiter {
    ceiling_db: Param<Db>,
    ceiling_linear: f32,
    /// When true, a ceiling param-input port (dB) follows the two audio inputs
    /// at index 2 and overrides the ceiling atomic per sample.
    mod_ceiling: bool,
}

impl BrickwallLimiter {
    pub fn new(ceiling_db: impl Into<Db>) -> Self {
        let ceiling_db = ceiling_db.into();
        Self {
            ceiling_db: Param::new(ceiling_db),
            ceiling_linear: db_to_amplitude(ceiling_db).get(),
            mod_ceiling: false,
        }
    }

    /// A brickwall limiter with an optional audio-rate ceiling param-input port
    /// at index 2. When present it overrides the ceiling atomic per sample; the
    /// atomic still holds the base.
    pub fn with_param_inputs(ceiling_db: impl Into<Db>, mod_ceiling: bool) -> Self {
        let mut node = Self::new(ceiling_db);
        node.mod_ceiling = mod_ceiling;
        node
    }

    /// Input-port index of the ceiling param input, if present (right after the
    /// two audio inputs).
    #[inline]
    pub fn ceiling_port(&self) -> Option<usize> {
        self.mod_ceiling.then_some(2)
    }

    pub fn ceiling(&self) -> Arc<AtomicF32> {
        self.ceiling_db.as_atomic()
    }

    pub fn set_ceiling(&mut self, db: impl Into<Db>) {
        let db = db.into();
        self.ceiling_db.store(db);
        self.ceiling_linear = db_to_amplitude(db).get();
    }

    #[inline]
    fn clip(&self, sample: f32) -> f32 {
        sample.clamp(-self.ceiling_linear, self.ceiling_linear)
    }

    /// Clip against an explicit linear ceiling (the modulated path).
    #[inline]
    fn clip_at(sample: f32, ceiling_linear: f32) -> f32 {
        sample.clamp(-ceiling_linear, ceiling_linear)
    }
}

impl AudioUnit for BrickwallLimiter {
    fn inputs(&self) -> usize {
        2 + self.mod_ceiling as usize
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {}

    fn set_sample_rate(&mut self, _sample_rate: tutti_core::SampleRate) {}

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Modulated path: a present ceiling port overrides the atomic; clip
        // against the per-sample linear ceiling without touching the cache.
        if let Some(p) = self.ceiling_port() {
            let ceiling_linear = db_to_amplitude(Db(input[p])).get();
            output[0] = Self::clip_at(input[0], ceiling_linear);
            if output.len() > 1 && input.len() > 1 {
                output[1] = Self::clip_at(input[1], ceiling_linear);
            }
            return;
        }
        let ceiling = self.ceiling_db.load();
        if (db_to_amplitude(ceiling).get() - self.ceiling_linear).abs() > 0.0001 {
            self.ceiling_linear = db_to_amplitude(ceiling).get();
        }
        output[0] = self.clip(input[0]);
        if output.len() > 1 && input.len() > 1 {
            output[1] = self.clip(input[1]);
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let has_stereo = input.channels() > 1;

        // Modulated path: read the ceiling port per sample.
        if let Some(p) = self.ceiling_port() {
            for i in 0..size {
                let ceiling_linear = db_to_amplitude(Db(input.at_f32(p, i))).get();
                output.set_f32(0, i, Self::clip_at(input.at_f32(0, i), ceiling_linear));
                if has_stereo && output.channels() > 1 {
                    output.set_f32(1, i, Self::clip_at(input.at_f32(1, i), ceiling_linear));
                }
            }
            return;
        }

        let ceiling = self.ceiling_db.load();
        if (db_to_amplitude(ceiling).get() - self.ceiling_linear).abs() > 0.0001 {
            self.ceiling_linear = db_to_amplitude(ceiling).get();
        }

        for i in 0..size {
            output.set_f32(0, i, self.clip(input.at_f32(0, i)));
            if has_stereo && output.channels() > 1 {
                output.set_f32(1, i, self.clip(input.at_f32(1, i)));
            }
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((tutti_core::UnitParam::Ceiling, value)) =
            tutti_core::unit_param::from_setting(&setting)
        {
            self.set_ceiling(value);
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::BRICKWALL_LIMITER_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(2);
        out.set(0, input.at(0).distort(0.0));
        out.set(1, input.at(1).distort(0.0));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl Clone for BrickwallLimiter {
    fn clone(&self) -> Self {
        Self {
            ceiling_db: self.ceiling_db.handle(),
            ceiling_linear: self.ceiling_linear,
            mod_ceiling: self.mod_ceiling,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_limiter_reduces_loud_signal() {
        let mut lim = LimiterNode::new(-6.0, -0.3);
        lim.set_sample_rate(tutti_core::SampleRate(44100.0));

        let loud = 1.0f32;
        let mut out = [0.0f32; 2];

        for _ in 0..500 {
            lim.tick(&[loud, loud], &mut out);
        }

        let ceiling_lin = db_to_amplitude(-0.3).get();
        assert!(
            out[0].abs() <= ceiling_lin + 0.05,
            "Output {:.4} should be near ceiling {:.4}",
            out[0].abs(),
            ceiling_lin
        );
    }

    #[test]
    fn test_limiter_passes_quiet_signal() {
        let mut lim = LimiterNode::new(-6.0, -0.3);
        lim.set_sample_rate(tutti_core::SampleRate(44100.0));

        let quiet = 0.1f32;
        let mut out = [0.0f32; 2];

        for _ in 0..500 {
            lim.tick(&[quiet, quiet], &mut out);
        }

        assert!(
            out[0].abs() > 0.01,
            "Quiet signal should pass through, got {}",
            out[0]
        );
    }

    #[test]
    fn test_limiter_stereo_linked() {
        let mut lim = LimiterNode::new(-6.0, -0.3);
        lim.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 2];

        for _ in 0..500 {
            lim.tick(&[1.0, 0.1], &mut out);
        }

        if out[0].abs() > 0.001 && out[1].abs() > 0.001 {
            let in_ratio = 0.1 / 1.0;
            let out_ratio = out[1].abs() / out[0].abs();
            assert!(
                (in_ratio - out_ratio).abs() < 0.2,
                "Stereo link: in_ratio={in_ratio}, out_ratio={out_ratio}"
            );
        }
    }

    #[test]
    fn test_limiter_reset() {
        let mut lim = LimiterNode::new(-6.0, -0.3);
        lim.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 2];
        for _ in 0..500 {
            lim.tick(&[1.0, 1.0], &mut out);
        }

        lim.reset();
        assert_eq!(lim.gain_reduction_db(), 0.0);
    }

    #[test]
    fn test_brickwall_clips_at_ceiling() {
        let mut bw = BrickwallLimiter::new(0.0);

        let mut out = [0.0f32; 2];
        bw.tick(&[2.0, -3.0], &mut out);

        assert!(
            (out[0] - 1.0).abs() < 0.001,
            "Should clip to 1.0, got {}",
            out[0]
        );
        assert!(
            (out[1] - (-1.0)).abs() < 0.001,
            "Should clip to -1.0, got {}",
            out[1]
        );
    }

    #[test]
    fn test_brickwall_adjustable_ceiling() {
        let mut bw = BrickwallLimiter::new(-6.0);
        let ceiling_lin = db_to_amplitude(-6.0).get();

        let mut out = [0.0f32; 2];
        bw.tick(&[1.0, -1.0], &mut out);

        assert!(
            (out[0] - ceiling_lin).abs() < 0.001,
            "Should clip to {ceiling_lin}, got {}",
            out[0]
        );
    }

    #[test]
    fn test_brickwall_passes_quiet_signal() {
        let mut bw = BrickwallLimiter::new(0.0);

        let mut out = [0.0f32; 2];
        bw.tick(&[0.3, -0.2], &mut out);

        assert!((out[0] - 0.3).abs() < 0.001);
        assert!((out[1] - (-0.2)).abs() < 0.001);
    }

    #[test]
    fn test_limiter_footprint() {
        let lim = LimiterNode::new(-6.0, -0.3);
        assert!(lim.footprint() > core::mem::size_of::<LimiterNode>());
    }

    // ── Audio-rate param-input ports ─────────────────────────────────────────

    #[test]
    fn limiter_default_no_ports() {
        let u = LimiterNode::new(-6.0, -0.3);
        assert_eq!(u.inputs(), 2);
        assert_eq!(u.outputs(), 2);
        assert_eq!(u.ceiling_port(), None);
        assert_eq!(u.threshold_port(), None);
    }

    #[test]
    fn limiter_param_port_arity_and_indices() {
        // ceiling only → ceiling at 2 (right after the two audio inputs).
        let c = LimiterNode::with_param_inputs(-6.0, -0.3, true, false);
        assert_eq!(c.inputs(), 3);
        assert_eq!(c.ceiling_port(), Some(2));
        assert_eq!(c.threshold_port(), None);
        // threshold only → threshold at 2 (no ceiling port before it).
        let t = LimiterNode::with_param_inputs(-6.0, -0.3, false, true);
        assert_eq!(t.inputs(), 3);
        assert_eq!(t.ceiling_port(), None);
        assert_eq!(t.threshold_port(), Some(2));
        // both → ceiling at 2, threshold at 3 (ceiling first, documented order).
        let b = LimiterNode::with_param_inputs(-6.0, -0.3, true, true);
        assert_eq!(b.inputs(), 4);
        assert_eq!(b.ceiling_port(), Some(2));
        assert_eq!(b.threshold_port(), Some(3));
    }

    #[test]
    fn limiter_unmodulated_matches_held_constant() {
        // A modulated node whose ceiling+threshold ports are held at the same
        // values as a plain node's atomics must produce identical output.
        let mut plain = LimiterNode::new(-6.0, -0.3);
        plain.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut modn = LimiterNode::with_param_inputs(-6.0, -0.3, true, true);
        modn.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut plain_out = [0.0f32; 2];
        let mut mod_out = [0.0f32; 2];
        for n in 0..2000 {
            // Mix of loud and quiet to exercise gain reduction + release.
            let s = if n % 400 < 200 { 0.9 } else { 0.05 };
            plain.tick(&[s, s], &mut plain_out);
            // Held: ceiling at 2 = -0.3, threshold at 3 = -6.0.
            modn.tick(&[s, s, -0.3, -6.0], &mut mod_out);
            assert!(
                (plain_out[0] - mod_out[0]).abs() < 1e-6
                    && (plain_out[1] - mod_out[1]).abs() < 1e-6,
                "modulated-held output diverges from plain at sample {n}: {:?} vs {:?}",
                plain_out,
                mod_out
            );
        }
    }

    #[test]
    fn brickwall_default_no_ports() {
        let u = BrickwallLimiter::new(0.0);
        assert_eq!(u.inputs(), 2);
        assert_eq!(u.outputs(), 2);
        assert_eq!(u.ceiling_port(), None);
    }

    #[test]
    fn brickwall_param_port_arity_and_index() {
        let c = BrickwallLimiter::with_param_inputs(0.0, true);
        assert_eq!(c.inputs(), 3);
        assert_eq!(c.ceiling_port(), Some(2));
        let off = BrickwallLimiter::with_param_inputs(0.0, false);
        assert_eq!(off.inputs(), 2);
        assert_eq!(off.ceiling_port(), None);
    }

    #[test]
    fn brickwall_unmodulated_matches_held_constant() {
        // A modulated brickwall whose ceiling port is held at the atomic value
        // must clip identically to a plain brickwall.
        let mut plain = BrickwallLimiter::new(-6.0);
        let mut modn = BrickwallLimiter::with_param_inputs(-6.0, true);

        let mut plain_out = [0.0f32; 2];
        let mut mod_out = [0.0f32; 2];
        let samples = [2.0, -3.0, 0.1, -0.05, 1.5, -1.5];
        for &s in &samples {
            plain.tick(&[s, -s], &mut plain_out);
            modn.tick(&[s, -s, -6.0], &mut mod_out);
            assert!(
                (plain_out[0] - mod_out[0]).abs() < 1e-6
                    && (plain_out[1] - mod_out[1]).abs() < 1e-6,
                "modulated-held brickwall diverges from plain for input {s}: {:?} vs {:?}",
                plain_out,
                mod_out
            );
        }
    }

    #[test]
    fn brickwall_ceiling_port_modulates_clip() {
        // Holding the ceiling port low clips harder than holding it high.
        let mut bw = BrickwallLimiter::with_param_inputs(0.0, true);
        let mut out = [0.0f32; 2];
        // Ceiling -12 dB ≈ 0.251 linear: 1.0 clips to ~0.251.
        bw.tick(&[1.0, 1.0, -12.0], &mut out);
        let low_ceiling = out[0];
        // Ceiling 0 dB = 1.0 linear: 1.0 passes through.
        bw.tick(&[1.0, 1.0, 0.0], &mut out);
        let high_ceiling = out[0];
        assert!(
            high_ceiling > low_ceiling + 0.1,
            "higher ceiling via port should clip less: low={low_ceiling}, high={high_ceiling}"
        );
    }

    #[test]
    fn test_limiter_sliding_minimum_releases_after_window() {
        let mut lim = LimiterNode::new(-12.0, -0.3).with_lookahead(0.002);
        lim.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 2];
        lim.tick(&[1.0, 1.0], &mut out);
        let reduction_after_transient = lim.gain_reduction_db();
        assert!(
            reduction_after_transient > 0.0,
            "Loud input should trigger gain reduction"
        );

        for _ in 0..20000 {
            lim.tick(&[0.0, 0.0], &mut out);
        }
        assert!(
            lim.gain_reduction_db() < 0.5,
            "After long quiet, gain reduction should release: {}",
            lim.gain_reduction_db()
        );
    }
}
