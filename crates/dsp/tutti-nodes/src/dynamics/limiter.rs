//! Two limiters: the lookahead [`LimiterNode`] and the hard-clipping
//! [`BrickwallLimiterNode`].
//!
//! They are not interchangeable. The lookahead limiter delays its audio so the
//! gain decision runs ahead of a peak, which costs latency and sounds
//! transparent; the brickwall clamps in place, which costs nothing and
//! distorts. Reach for the first to limit musically, the second as a safety
//! catch.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, ChannelLayout, SignalFrame};

use super::envelope::EnvelopeFollower;
use super::utils::{amplitude_to_db, compute_limiter_gain, db_to_amplitude, smooth_envelope};
use crate::buffer::{CircularBuffer, MonotonicMinDeque};
use tutti_core::{Db, Param, SampleRate, Samples, Seconds, Tail};

/// Lookahead ring buffers + sliding-window-minimum tracker for the limiter.
/// Split out so `LimiterNode` reads as a list of parameters plus a lookahead
/// block, not a flat field soup.
#[derive(Clone)]
struct LookaheadRing {
    /// One lookahead ring per audio channel; `buffers.len()` == width. Built at
    /// construction — never resized in the RT path.
    buffers: Vec<CircularBuffer<f32>>,
    min_deque: MonotonicMinDeque,
    sample_counter: u64,
    lookahead_samples: usize,
}

impl LookaheadRing {
    fn new(channels: usize, lookahead_samples: usize) -> Self {
        let n = lookahead_samples.max(1);
        Self {
            buffers: (0..channels.max(1))
                .map(|_| CircularBuffer::new(n))
                .collect(),
            min_deque: MonotonicMinDeque::new(n),
            sample_counter: 0,
            lookahead_samples: n,
        }
    }

    fn resize(&mut self, lookahead_samples: usize) {
        *self = Self::new(self.buffers.len(), lookahead_samples);
    }

    /// Frames still held when the input stops.
    ///
    /// The ring delays by `lookahead_samples - 1` (see `read_back` below), so
    /// that many frames outlive a silent input. The release envelope is not part
    /// of this: it shapes gain, and gain applied to silence is silence.
    fn ring_out(&self) -> Samples {
        Samples(self.lookahead_samples.saturating_sub(1))
    }

    fn clear(&mut self) {
        for b in &mut self.buffers {
            b.clear();
        }
        self.min_deque.clear();
        self.sample_counter = 0;
    }

    #[inline]
    fn footprint(&self) -> usize {
        self.buffers.iter().map(|b| b.len()).sum::<usize>() * core::mem::size_of::<f32>()
            + self.min_deque.capacity() * core::mem::size_of::<(u64, f32)>()
    }

    /// Read the lookahead-delayed sample for each channel into `delayed`, push
    /// the current `frame` into the rings, feed `gain` to the sliding minimum,
    /// and return the window-min gain (the linked gain reduction all channels
    /// share). `delayed` and `frame` are both `width` long.
    #[inline]
    fn step(&mut self, frame: &[f32], gain: f32, delayed: &mut [f32]) -> f32 {
        for (c, b) in self.buffers.iter_mut().enumerate() {
            delayed[c] = b.read_back(self.lookahead_samples - 1);
            b.push(frame[c]);
        }

        self.min_deque.push(self.sample_counter, gain);
        let window_start = self
            .sample_counter
            .saturating_sub(self.lookahead_samples as u64 - 1);
        self.min_deque.evict_older_than(window_start);
        self.sample_counter = self.sample_counter.wrapping_add(1);

        self.min_deque.min().unwrap_or(gain)
    }
}

/// Lookahead limiter with gain reduction linked across channels. 2 inputs, 2
/// outputs by default.
///
/// A limiter is a compressor at an effectively infinite ratio: nothing is
/// allowed past the ceiling. The **lookahead** is what makes that possible
/// without distortion — the audio is delayed by the lookahead window while the
/// gain decision runs ahead of it, so reduction is already in place when a peak
/// arrives rather than chasing it.
///
/// That delay is real latency: the node reports it via `AudioUnit`, and a graph
/// that mixes this against a dry path needs the delay compensated.
///
/// The minimum gain over the lookahead window is tracked with a
/// [`MonotonicMinDeque`], so each sample costs O(1) amortized regardless of
/// lookahead length. Nothing in the RT path allocates: the rings and scratch
/// frames are sized at construction.
///
/// # Port layout & audio-rate modulation
///
/// The default node is 2-in / 2-out (audio L/R on ports 0/1). For audio-rate
/// modulation it can grow optional param-input ports after the audio inputs
/// (see [`LimiterNode::with_param_inputs`]) in the order **ceiling, then
/// threshold** (both dB): ceiling at index 2 if present, threshold next. Each
/// present port overrides its atomic per sample; the atomics still hold the
/// base. Absent, the node is a plain 2-in/2-out node, which is the common case.
///
/// Ask [`ceiling_port`](Self::ceiling_port) /
/// [`threshold_port`](Self::threshold_port) rather than computing an index:
/// they move with the width.
pub struct LimiterNode {
    threshold_db: Param<Db>,
    ceiling_db: Param<Db>,
    release: Param<Seconds>,

    ring: LookaheadRing,
    /// The lookahead window **as requested**, which is the only form a rate
    /// change can be re-derived from.
    ///
    /// The ring's frame count is a lossy view of this: it is `_ceil`ed, so
    /// recovering seconds by dividing the count by the rate returns a slightly
    /// *longer* window than was asked for. Re-ceiling that at a new rate ratchets
    /// the lookahead up a little on every `set_sample_rate`, and the drift
    /// accumulates rather than cancelling — 5.000 ms becomes 5.011, then 5.021,
    /// then 5.034 across a few device changes. Keeping the request means every
    /// derivation starts from the same number, so the count is a pure function
    /// of (`lookahead`, `sample_rate`) and rate changes commute.
    ///
    /// Same reason `release` stays a `Param<Seconds>` and the compressor and gate
    /// keep their `AttackRelease` in [`Seconds`]: the derived coefficient is
    /// never the recoverable form.
    lookahead: Seconds,
    /// Audio channel width. Gain reduction is linked across all channels (peak
    /// = max-abs over the frame), matching the stereo-linked design.
    layout: ChannelLayout,
    /// [`layout`](Self::layout)'s count, cached as the interleave/iteration
    /// stride.
    ///
    /// The layout is the *declaration*; this is the arithmetic derived from it.
    /// They are kept as separate fields because `process_frame_with` reads the
    /// width per sample (`frame[..n]`, `for c in 0..n`), and deriving it there
    /// would put a `match` in the inner loop. Set once at construction, so the
    /// two can never disagree.
    channels: usize,
    /// Per-channel scratch frames, sized to `channels` at construction so the
    /// RT path builds an input frame + limited output without allocating.
    in_frame: Vec<f32>,
    out_frame: Vec<f32>,
    /// Per-channel lookahead-delayed scratch (filled by the ring each sample).
    delayed: Vec<f32>,
    envelope: f32,
    gain_reduction_db: Db,
    sample_rate: SampleRate,
    follower: EnvelopeFollower,
    /// When true, a ceiling param-input port (dB) follows the audio inputs and
    /// overrides the ceiling atomic per sample.
    mod_ceiling: bool,
    /// When true, a threshold param-input port (dB) follows the audio inputs
    /// (and the ceiling port if present) and overrides the threshold atomic
    /// per sample.
    mod_threshold: bool,
}

impl LimiterNode {
    /// Builds a stereo lookahead limiter with a 5 ms lookahead and 100 ms
    /// release.
    ///
    /// `threshold_db` is where reduction starts and `ceiling_db` the hard
    /// output bound, both in [`Db`] and typically negative — a mastering
    /// limiter often sits at a ceiling of `-0.3` to leave inter-sample
    /// headroom. Adjust the window with
    /// [`with_lookahead`](Self::with_lookahead) and the recovery with
    /// [`with_release`](Self::with_release).
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**; see
    /// [`with_channels`](Self::with_channels), which this delegates to, for what
    /// goes wrong if [`AudioUnit::set_sample_rate`] is not called before the
    /// first `process`.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
    pub fn new(threshold_db: impl Into<Db>, ceiling_db: impl Into<Db>) -> Self {
        Self::with_channels(ChannelLayout::STEREO, threshold_db, ceiling_db)
    }

    /// An `n`-channel lookahead limiter (clamped to at least mono), with gain
    /// reduction **linked** across all channels.
    ///
    /// The peak is the max-abs over the whole frame and one gain is applied to
    /// every channel, so a loud peak in one channel ducks them all together and
    /// the image does not shift — the surround generalization of the stereo
    /// design. Parameters are as [`new`](Self::new).
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**, and here that
    /// governs an *allocation*: the lookahead ring is sized in samples from the
    /// 5 ms window, and the envelope follower's attack/release coefficients are
    /// derived the same way. Call [`AudioUnit::set_sample_rate`] before the
    /// first `process`; it resizes the ring and recomputes the coefficients.
    ///
    /// Skipping it at 48 kHz gives a ring holding 4.6 ms where 5 ms was asked
    /// for, so the limiter sees a peak later than its own reported latency
    /// promises and lets the front edge through — a limiter that mostly limits,
    /// with occasional overs above the ceiling it guarantees. The release skews
    /// by the same 8.8%. See the crate-level "born at a placeholder rate"
    /// section.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
    pub fn with_channels(
        channels: impl Into<ChannelLayout>,
        threshold_db: impl Into<Db>,
        ceiling_db: impl Into<Db>,
    ) -> Self {
        let layout = channels.into();
        // An empty layout would leave every scratch `Vec` zero-length and make
        // `inputs()`/`outputs()` report 0, so clamp to at least mono — the same
        // floor the raw `channels.max(1)` used to provide.
        let n = (layout.count() as usize).max(1);
        let layout = ChannelLayout::from(n);
        let lookahead_secs = Seconds(0.005);
        // `_ceil`, the allocation form: the ring must hold at *least* the
        // lookahead, and nearest-rounding under-allocates for half of all
        // inputs.
        let lookahead_samples = lookahead_secs.to_samples_ceil(SampleRate::DEFAULT).get();

        Self {
            threshold_db: Param::new(threshold_db.into()),
            ceiling_db: Param::new(ceiling_db.into()),
            release: Param::new(Seconds(0.1)),
            ring: LookaheadRing::new(n, lookahead_samples),
            lookahead: lookahead_secs,
            layout,
            channels: n,
            in_frame: vec![0.0; n],
            out_frame: vec![0.0; n],
            delayed: vec![0.0; n],
            envelope: 0.0,
            gain_reduction_db: Db::UNITY,
            sample_rate: SampleRate::DEFAULT,
            follower: EnvelopeFollower::new(0.0, 0.1, SampleRate::DEFAULT),
            mod_ceiling: false,
            mod_threshold: false,
        }
    }

    /// A limiter with optional audio-rate ceiling / threshold param-input ports,
    /// appended after the audio inputs in that order (ceiling first). Each
    /// present port overrides its atomic per sample; the atomics still hold the
    /// base.
    ///
    /// Width and modulation are **independent axes**: `channels` says how wide
    /// the limiter is, the `mod_*` flags say which params it reads at audio
    /// rate. Collapsing them — building the modulated form at a fixed width 2 —
    /// turns a request for a modulated 5.1 limiter into a *stereo* one, and the
    /// only symptom is a `set_source` on a param port that resolves and carries
    /// the wrong signal.
    ///
    /// The param ports follow the audio inputs, so their indices **move with the
    /// width**. Ask [`ParamPorts::param_port`](crate::ParamPorts::param_port);
    /// never assume an index.
    pub fn with_param_inputs(
        channels: impl Into<ChannelLayout>,
        threshold_db: impl Into<Db>,
        ceiling_db: impl Into<Db>,
        mod_ceiling: bool,
        mod_threshold: bool,
    ) -> Self {
        let mut node = Self::with_channels(channels, threshold_db, ceiling_db);
        node.mod_ceiling = mod_ceiling;
        node.mod_threshold = mod_threshold;
        node
    }

    /// The audio width this limiter was built for.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Input-port index of the ceiling param input, if present (right after the
    /// audio inputs).
    #[inline]
    pub fn ceiling_port(&self) -> Option<usize> {
        self.mod_ceiling.then_some(self.channels)
    }

    /// Input-port index of the threshold param input, if present (after the
    /// audio inputs and the ceiling port).
    #[inline]
    pub fn threshold_port(&self) -> Option<usize> {
        self.mod_threshold
            .then_some(self.channels + self.mod_ceiling as usize)
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

    /// Sets the lookahead window in [`Seconds`], reallocating the rings.
    ///
    /// **This is the node's latency**: the audio is delayed by the window so
    /// the gain decision can run ahead of it. Longer catches peaks more
    /// transparently and delays more; the 5 ms default is a typical
    /// compromise. Clamped to at least one frame.
    ///
    /// Allocates and clears the rings, so call it during setup — never on a
    /// live node.
    pub fn with_lookahead(mut self, lookahead: impl Into<Seconds>) -> Self {
        // Keep the request, not just the count it derives: `set_sample_rate`
        // re-derives from this, and a count is `_ceil`ed and so cannot round-trip
        // back to the seconds that produced it.
        self.lookahead = lookahead.into();
        // Ceil: a lookahead ring must hold at least the requested window.
        let samples = self.lookahead.to_samples_ceil(self.sample_rate);
        self.ring.resize(samples.get().max(1));
        self
    }

    /// Sets how fast the limiter recovers after reduction, in [`Seconds`],
    /// floored at 1 ms.
    ///
    /// Short releases are louder but pump audibly on dense material; longer
    /// ones sound transparent and hold reduction through a passage.
    pub fn with_release(self, release_secs: impl Into<Seconds>) -> Self {
        self.release
            .store(Seconds(release_secs.into().get().max(0.001)));
        self
    }

    /// The shared threshold cell in [`Db`] — where reduction begins.
    ///
    /// **A present threshold param-input port overrides this per sample.**
    /// Shared across clones.
    pub fn threshold(&self) -> Arc<AtomicF32> {
        self.threshold_db.as_atomic()
    }

    /// The shared ceiling cell in [`Db`] — the hard bound the output is not
    /// allowed to exceed.
    ///
    /// **A present ceiling param-input port overrides this per sample.**
    pub fn ceiling(&self) -> Arc<AtomicF32> {
        self.ceiling_db.as_atomic()
    }

    /// The shared release-time cell in [`Seconds`].
    ///
    /// Read once per block to recompute the envelope coefficients. The attack
    /// is fixed at zero — a limiter must not let a peak through.
    pub fn release_time(&self) -> Arc<AtomicF32> {
        self.release.as_atomic()
    }

    /// Sets the threshold in [`Db`], unclamped.
    ///
    /// With a threshold param-input port present this sets the *base* the port
    /// overrides.
    pub fn set_threshold(&self, db: impl Into<Db>) {
        self.threshold_db.store(db.into());
    }

    /// Sets the output ceiling in [`Db`], unclamped.
    pub fn set_ceiling(&self, db: impl Into<Db>) {
        self.ceiling_db.store(db.into());
    }

    /// Sets the release time in [`Seconds`], floored at 1 ms.
    pub fn set_release(&self, secs: impl Into<Seconds>) {
        self.release.store(Seconds(secs.into().get().max(0.001)));
    }

    /// The gain reduction currently applied, in [`Db`] — a **measurement**, for
    /// driving a reduction meter.
    ///
    /// [`Db::UNITY`] means nothing is being reduced. This is the window
    /// minimum, so it reflects the lookahead decision rather than the
    /// instantaneous peak.
    pub fn gain_reduction_db(&self) -> Db {
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

    /// Process one sample-frame with explicit threshold/ceiling (the modulated
    /// path; the fast path passes the atomics). `frame` holds the `channels`
    /// audio inputs; the limited, lookahead-delayed output for each channel is
    /// written into `out` (also `channels` long). Gain reduction is linked: the
    /// peak is the max-abs across the frame, and one min-gain scales every
    /// channel.
    #[inline]
    fn process_frame_with(
        &mut self,
        frame: &[f32],
        threshold: tutti_core::Db,
        ceiling: tutti_core::Db,
        out: &mut [f32],
    ) {
        let peak = frame[..self.channels]
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        let peak_db = amplitude_to_db(peak);

        let target_gain = self.compute_gain(peak_db, threshold, ceiling);

        if target_gain < self.envelope {
            self.envelope = target_gain;
        } else {
            self.envelope =
                smooth_envelope(self.envelope, target_gain, self.follower.release_coeff());
        }

        // `delayed` is a per-instance scratch sized at construction — no alloc.
        let mut delayed = core::mem::take(&mut self.delayed);
        let min_gain = self.ring.step(frame, self.envelope, &mut delayed);
        for c in 0..self.channels {
            out[c] = delayed[c] * min_gain;
        }
        self.delayed = delayed;

        // Metering only, and reported as a positive magnitude — the sign
        // convention `CompressorNode::gain_reduction_db` also follows.
        //
        // `from_amplitude` owns the `log10(0)` guard, so silence pins at
        // `Db::FLOOR` rather than `-inf`. This site used to hand-roll the
        // conversion with a `96.0` floor of its own, which was the divergence
        // `amplitude_to_db`'s doc records as already removed — it had been
        // removed from the detector path just above and missed here. The old
        // floor was never a ceiling either: a merely tiny gain fell through to
        // the `log10` branch and reported far past 96 dB.
        self.gain_reduction_db = -amplitude_to_db(min_gain);
    }
}

impl AudioUnit for LimiterNode {
    fn inputs(&self) -> usize {
        self.channels + self.mod_ceiling as usize + self.mod_threshold as usize
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    fn reset(&mut self) {
        self.ring.clear();
        self.envelope = 0.0;
        self.gain_reduction_db = Db::UNITY;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        // Re-derive the frame count from the *requested* window, so the
        // wall-clock lookahead survives a rate change. Deriving it from the
        // ring's current count instead would re-ceil an already-ceiled value and
        // ratchet the window up on every call — see the `lookahead` field.
        self.sample_rate = sample_rate;
        self.follower
            .set_sample_rate(sample_rate, Seconds(0.0), self.release.load());
        let new_samples = self.lookahead.to_samples_ceil(sample_rate).get();
        self.ring.resize(new_samples.max(1));
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.update_coefficients();
        // A present ceiling/threshold port overrides its atomic.
        let (threshold, ceiling) = self.effective_params(|p| input[p]);
        // Build a full-width audio frame from `input`, tolerating a caller that
        // supplies fewer audio channels than the unit width: the last available
        // channel is duplicated (mirrors the pre-widen mono→stereo fallback), so
        // `process_frame_with`'s `frame[..channels]` never indexes out of range.
        let mut in_frame = core::mem::take(&mut self.in_frame);
        let audio = self.channels.min(input.len());
        for (c, slot) in in_frame.iter_mut().enumerate() {
            *slot = if c < audio {
                input[c]
            } else if audio > 0 {
                input[audio - 1]
            } else {
                0.0
            };
        }
        let mut out = core::mem::take(&mut self.out_frame);
        self.process_frame_with(&in_frame, threshold, ceiling, &mut out);
        let n = self.channels.min(output.len());
        output[..n].copy_from_slice(&out[..n]);
        self.in_frame = in_frame;
        self.out_frame = out;
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.update_coefficients();
        let in_ch = input.channels();
        let out_ch = output.channels();
        let ceiling_port = self.ceiling_port();
        let threshold_port = self.threshold_port();
        let base_threshold = self.threshold_db.load();
        let base_ceiling = self.ceiling_db.load();

        // Reuse the two fixed scratch frames (sized at construction). Taken so
        // `process_frame_with` can borrow `self` without aliasing them.
        let mut in_frame = core::mem::take(&mut self.in_frame);
        let mut out_frame = core::mem::take(&mut self.out_frame);
        for i in 0..size {
            for (c, slot) in in_frame.iter_mut().enumerate() {
                *slot = if c < in_ch { input.at_f32(c, i) } else { 0.0 };
            }
            let threshold = threshold_port.map_or(base_threshold, |p| Db(input.at_f32(p, i)));
            let ceiling = ceiling_port.map_or(base_ceiling, |p| Db(input.at_f32(p, i)));
            self.process_frame_with(&in_frame, threshold, ceiling, &mut out_frame);
            for (c, &y) in out_frame.iter().enumerate().take(self.channels.min(out_ch)) {
                output.set_f32(c, i, y);
            }
        }
        self.in_frame = in_frame;
        self.out_frame = out_frame;
    }

    fn set(&mut self, setting: tutti_core::Setting) {
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
        let mut out = SignalFrame::new(self.channels);
        let latency = self.ring.lookahead_samples as f64;
        for c in 0..self.channels {
            out.set(c, input.at(c).delay(latency));
        }
        out
    }

    /// The lookahead ring's contents, which outlive a silent input.
    ///
    /// Exactly known: the ring is a fixed delay, so when the input stops it
    /// still holds that many frames. The release envelope is deliberately not
    /// included — it shapes gain, and gain applied to silence is silence.
    fn tail(&mut self) -> Tail {
        match self.ring.ring_out() {
            s if s.is_zero() => Tail::None,
            s => Tail::Finite(s),
        }
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
            // Carried, not re-derived: `Net` clones a unit to probe it and the
            // clone may be handed a different rate, so a clone that lost the
            // request would re-derive its lookahead from the ceiled count.
            lookahead: self.lookahead,
            layout: self.layout,
            channels: self.channels,
            in_frame: self.in_frame.clone(),
            out_frame: self.out_frame.clone(),
            delayed: self.delayed.clone(),
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
/// [`BrickwallLimiterNode::with_param_inputs`]); present → overrides the ceiling
/// atomic per sample, absent → a plain 2-in/2-out node, bit-identical to the
/// unmodulated path.
pub struct BrickwallLimiterNode {
    ceiling_db: Param<Db>,
    ceiling_linear: f32,
    /// Audio channel width (`inputs()` audio ports == `outputs()`). The clip is
    /// stateless and per-channel, so widening is purely the port count.
    layout: ChannelLayout,
    /// [`layout`](Self::layout)'s count, cached as the iteration stride — see
    /// the note on [`LimiterNode::channels`]. Set once at construction.
    channels: usize,
    /// When true, a ceiling param-input port (dB) follows the audio inputs and
    /// overrides the ceiling atomic per sample.
    mod_ceiling: bool,
}

impl BrickwallLimiterNode {
    /// Builds a stereo brickwall limiter clamping at `ceiling_db`.
    ///
    /// **This is a hard clipper, not [`LimiterNode`].** It has no lookahead, no
    /// envelope and no latency — a sample past the ceiling is simply clamped,
    /// which distorts rather than ducking. Reach for it as a safety catch on an
    /// output, and for musical limiting use [`LimiterNode`].
    pub fn new(ceiling_db: impl Into<Db>) -> Self {
        Self::with_channels(ChannelLayout::STEREO, ceiling_db)
    }

    /// An `n`-channel brickwall limiter. The clip is stateless, so every
    /// channel is clamped to the same (linked) ceiling.
    /// `with_channels(ChannelLayout::STEREO, …)` is bit-identical to
    /// [`Self::new`].
    pub fn with_channels(channels: impl Into<ChannelLayout>, ceiling_db: impl Into<Db>) -> Self {
        let ceiling_db = ceiling_db.into();
        // At least mono: a zero-width unit would report 0 input and output
        // ports, which is not a limiter.
        let n = (channels.into().count() as usize).max(1);
        Self {
            ceiling_db: Param::new(ceiling_db),
            ceiling_linear: db_to_amplitude(ceiling_db).get(),
            layout: ChannelLayout::from(n),
            channels: n,
            mod_ceiling: false,
        }
    }

    /// A brickwall limiter with an optional audio-rate ceiling param-input port.
    /// When present it overrides the ceiling atomic per sample; the atomic still
    /// holds the base.
    pub fn with_param_inputs(ceiling_db: impl Into<Db>, mod_ceiling: bool) -> Self {
        let mut node = Self::new(ceiling_db);
        node.mod_ceiling = mod_ceiling;
        node
    }

    /// The audio width this limiter was built for.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Input-port index of the ceiling param input, if present (right after the
    /// audio inputs).
    #[inline]
    pub fn ceiling_port(&self) -> Option<usize> {
        self.mod_ceiling.then_some(self.channels)
    }

    /// The shared ceiling cell in [`Db`] — the level samples are clamped to.
    ///
    /// **A present ceiling param-input port overrides this per sample.**
    /// Writing the raw cell does *not* refresh the cached linear ceiling the
    /// unmodulated path clamps against; use
    /// [`set_ceiling`](Self::set_ceiling) for that.
    pub fn ceiling(&self) -> Arc<AtomicF32> {
        self.ceiling_db.as_atomic()
    }

    /// Sets the clamp ceiling in [`Db`], refreshing the cached linear
    /// amplitude.
    ///
    /// `&mut self` because of that cache, so this cannot reach a node already
    /// live in the graph — a live ceiling change needs the param-input port.
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

impl AudioUnit for BrickwallLimiterNode {
    fn inputs(&self) -> usize {
        self.channels + self.mod_ceiling as usize
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    fn reset(&mut self) {}

    /// Deliberately empty, and the only node in this crate that can honestly
    /// leave it so: a brickwall is a memoryless `clamp` with no lookahead, no
    /// envelope and therefore no time constant to re-derive. It is exempt from
    /// the crate-level "born at a placeholder rate" rule for that reason, not by
    /// oversight — unlike [`LimiterNode`], whose ring and follower both need
    /// the real rate.
    fn set_sample_rate(&mut self, _sample_rate: tutti_core::SampleRate) {}

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Guard against a graph handing fewer physical channels than the unit's
        // width (mirrors the old `input.len() > 1` checks, now general).
        let n = self.channels.min(input.len()).min(output.len());
        // Modulated path: a present ceiling port overrides the atomic; clip
        // against the per-sample linear ceiling without touching the cache.
        if let Some(p) = self.ceiling_port() {
            let ceiling_linear = db_to_amplitude(Db(input[p])).get();
            for c in 0..n {
                output[c] = Self::clip_at(input[c], ceiling_linear);
            }
            return;
        }
        let ceiling = self.ceiling_db.load();
        if (db_to_amplitude(ceiling).get() - self.ceiling_linear).abs() > 0.0001 {
            self.ceiling_linear = db_to_amplitude(ceiling).get();
        }
        for c in 0..n {
            output[c] = self.clip(input[c]);
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // Clip every channel the graph actually provides, up to the unit width.
        let n = self.channels.min(input.channels()).min(output.channels());

        // Modulated path: read the ceiling port per sample.
        if let Some(p) = self.ceiling_port() {
            for i in 0..size {
                let ceiling_linear = db_to_amplitude(Db(input.at_f32(p, i))).get();
                for c in 0..n {
                    output.set_f32(c, i, Self::clip_at(input.at_f32(c, i), ceiling_linear));
                }
            }
            return;
        }

        let ceiling = self.ceiling_db.load();
        if (db_to_amplitude(ceiling).get() - self.ceiling_linear).abs() > 0.0001 {
            self.ceiling_linear = db_to_amplitude(ceiling).get();
        }

        for i in 0..size {
            for c in 0..n {
                output.set_f32(c, i, self.clip(input.at_f32(c, i)));
            }
        }
    }

    fn set(&mut self, setting: tutti_core::Setting) {
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
        let mut out = SignalFrame::new(self.channels);
        for c in 0..self.channels {
            out.set(c, input.at(c).distort(0.0));
        }
        out
    }

    /// The clip is stateless, so it stops with its input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl Clone for BrickwallLimiterNode {
    fn clone(&self) -> Self {
        Self {
            ceiling_db: self.ceiling_db.handle(),
            ceiling_linear: self.ceiling_linear,
            layout: self.layout,
            channels: self.channels,
            mod_ceiling: self.mod_ceiling,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A limiter is **born at the placeholder rate**, and `set_sample_rate` is
    /// what makes its lookahead a wall-clock 5 ms rather than a frame count.
    ///
    /// This is the invariant the constructor docs promise, asserted as the
    /// *duration* the ring holds rather than as a frame count, because the
    /// count is supposed to change with the rate and only the duration is
    /// supposed to survive. Both halves matter and neither implies the other:
    ///
    /// - Uncorrected, the ring holds 221 frames — which is 5 ms at 44100 and
    ///   only 4.6 ms at 48000. That is the documented failure: a limiter that
    ///   sees a peak later than its reported latency promises.
    /// - Corrected, the count grows to 240 so the duration stays 5 ms.
    ///
    /// Asserted through `AudioUnit::latency`, the same figure PDC compensates
    /// against, so a regression here is a regression in what the graph is told —
    /// not merely in a private field.
    #[test]
    fn lookahead_is_a_duration_and_set_sample_rate_is_what_preserves_it() {
        const LOOKAHEAD: Seconds = Seconds(0.005);
        let device = tutti_core::SampleRate(48_000.0);

        let mut lim = LimiterNode::new(-6.0, -0.3);

        // Born at the placeholder: the count is right for 44100 and wrong for
        // the device the node is about to run on.
        let born = lim.ring.lookahead_samples;
        assert_eq!(
            born,
            LOOKAHEAD.to_samples_ceil(SampleRate::DEFAULT).get(),
            "construction must size the ring at the placeholder rate"
        );
        let born_secs = born as f32 / device.get() as f32;
        assert!(
            born_secs < LOOKAHEAD.get() * 0.95,
            "an uncorrected ring must be audibly short at {device:?} — that is \
             the hazard the constructor documents; got {born_secs:.5}s of the \
             {:.5}s asked for",
            LOOKAHEAD.get()
        );

        // The correction: same wall-clock window, more frames.
        lim.set_sample_rate(device);
        let fixed = lim.ring.lookahead_samples;
        assert_eq!(
            fixed,
            LOOKAHEAD.to_samples_ceil(device).get(),
            "set_sample_rate must re-derive the frame count at the new rate"
        );
        assert!(
            fixed > born,
            "a higher rate needs more frames for the same duration ({born} -> {fixed})"
        );
        let fixed_secs = fixed as f32 / device.get() as f32;
        assert!(
            (fixed_secs - LOOKAHEAD.get()).abs() < 1e-4,
            "the wall-clock lookahead must survive the rate change; got {fixed_secs:.5}s"
        );

        // And the graph is told the corrected figure, not the placeholder one.
        assert_eq!(
            lim.latency(),
            Some(fixed as f64),
            "reported latency must track the resized ring"
        );

        // Repeated rate changes must land on the same count each time. The
        // window is `_ceil`ed, so re-deriving it from the ring's *count* rather
        // than from the stored request re-rounds an already-rounded value and
        // ratchets the lookahead up a little on every call — a drift that
        // accumulates instead of cancelling. Going back and forth pins that the
        // derivation is a pure function of (request, rate).
        for _ in 0..4 {
            lim.set_sample_rate(SampleRate::DEFAULT);
            assert_eq!(
                lim.ring.lookahead_samples, born,
                "returning to the original rate must return the original count"
            );
            lim.set_sample_rate(device);
            assert_eq!(
                lim.ring.lookahead_samples, fixed,
                "a rate change must not accumulate rounding across calls"
            );
        }
    }

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
        assert_eq!(lim.gain_reduction_db(), Db::UNITY);
    }

    #[test]
    fn test_brickwall_clips_at_ceiling() {
        let mut bw = BrickwallLimiterNode::new(0.0);

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
        let mut bw = BrickwallLimiterNode::new(-6.0);
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
        let mut bw = BrickwallLimiterNode::new(0.0);

        let mut out = [0.0f32; 2];
        bw.tick(&[0.3, -0.2], &mut out);

        assert!((out[0] - 0.3).abs() < 0.001);
        assert!((out[1] - (-0.2)).abs() < 0.001);
    }

    // ── Audio-rate param-input ports ─────────────────────────────────────────

    #[test]
    fn limiter_param_port_arity_and_indices() {
        // The plain constructor declares NO param ports: the audio arity is
        // untouched and both accessors are absent.
        let d = LimiterNode::new(-6.0, -0.3);
        assert_eq!(d.inputs(), 2);
        assert_eq!(d.outputs(), 2);
        assert_eq!(d.ceiling_port(), None);
        assert_eq!(d.threshold_port(), None);
        // ceiling only → ceiling at 2 (right after the two audio inputs).
        let c = LimiterNode::with_param_inputs(ChannelLayout::STEREO, -6.0, -0.3, true, false);
        assert_eq!(c.inputs(), 3);
        assert_eq!(c.ceiling_port(), Some(2));
        assert_eq!(c.threshold_port(), None);
        // threshold only → threshold at 2 (no ceiling port before it).
        let t = LimiterNode::with_param_inputs(ChannelLayout::STEREO, -6.0, -0.3, false, true);
        assert_eq!(t.inputs(), 3);
        assert_eq!(t.ceiling_port(), None);
        assert_eq!(t.threshold_port(), Some(2));
        // both → ceiling at 2, threshold at 3 (ceiling first, documented order).
        let b = LimiterNode::with_param_inputs(ChannelLayout::STEREO, -6.0, -0.3, true, true);
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

        let mut modn =
            LimiterNode::with_param_inputs(ChannelLayout::STEREO, -6.0, -0.3, true, true);
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
    fn brickwall_param_port_arity_and_index() {
        // Plain constructor: no port, arity untouched.
        let d = BrickwallLimiterNode::new(0.0);
        assert_eq!(d.inputs(), 2);
        assert_eq!(d.outputs(), 2);
        assert_eq!(d.ceiling_port(), None);
        let c = BrickwallLimiterNode::with_param_inputs(0.0, true);
        assert_eq!(c.inputs(), 3);
        assert_eq!(c.ceiling_port(), Some(2));
        let off = BrickwallLimiterNode::with_param_inputs(0.0, false);
        assert_eq!(off.inputs(), 2);
        assert_eq!(off.ceiling_port(), None);
    }

    #[test]
    fn brickwall_unmodulated_matches_held_constant() {
        // A modulated brickwall whose ceiling port is held at the atomic value
        // must clip identically to a plain brickwall.
        let mut plain = BrickwallLimiterNode::new(-6.0);
        let mut modn = BrickwallLimiterNode::with_param_inputs(-6.0, true);

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
        let mut bw = BrickwallLimiterNode::with_param_inputs(0.0, true);
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

    // ── Width-native (N-channel) ─────────────────────────────────────────────

    #[test]
    fn limiter_tick_tolerates_short_input_frame() {
        // A stereo limiter handed a mono (len-1) tick frame must not panic; it
        // duplicates the last channel (the pre-widen mono→stereo fallback).
        let mut lim = LimiterNode::new(-6.0, -0.3);
        lim.set_sample_rate(tutti_core::SampleRate(44100.0));
        let mut out = [0.0f32; 2];
        // Run past the 5 ms lookahead so the delayed signal emerges.
        for _ in 0..500 {
            lim.tick(&[0.9], &mut out); // len 1 < channels 2 — must not panic
        }
        // Both outputs are driven (the mono input is duplicated to ch1), not
        // left silent — proves the short-frame fallback wired the second channel.
        assert!(out[0].abs() > 1e-4, "ch0 silent: {}", out[0]);
        assert!(
            out[1].abs() > 1e-4,
            "ch1 silent (fallback not applied): {}",
            out[1]
        );
    }

    #[test]
    fn limiter_with_channels_reports_arity() {
        let l = LimiterNode::with_channels(ChannelLayout::from(6u16), -6.0, -0.3);
        assert_eq!(l.inputs(), 6);
        assert_eq!(l.outputs(), 6);
    }

    #[test]
    fn limiter_with_channels_2_matches_new() {
        let mut a = LimiterNode::new(-6.0, -0.3);
        a.set_sample_rate(tutti_core::SampleRate(44100.0));
        let mut b = LimiterNode::with_channels(ChannelLayout::STEREO, -6.0, -0.3);
        b.set_sample_rate(tutti_core::SampleRate(44100.0));
        let mut oa = [0.0f32; 2];
        let mut ob = [0.0f32; 2];
        for i in 0..2000 {
            let s = if i % 400 < 200 { 0.95 } else { 0.05 };
            a.tick(&[s, s * 0.5], &mut oa);
            b.tick(&[s, s * 0.5], &mut ob);
            assert_eq!(oa[0].to_bits(), ob[0].to_bits(), "L bit-diff at {i}");
            assert_eq!(oa[1].to_bits(), ob[1].to_bits(), "R bit-diff at {i}");
        }
    }

    #[test]
    fn wide_limiter_gain_is_linked_across_all_channels() {
        // A loud transient on one channel must reduce ALL channels by the same
        // linked gain (max-abs across the frame), preserving inter-channel ratios.
        let mut lim = LimiterNode::with_channels(ChannelLayout::from(6u16), -6.0, -0.3);
        lim.set_sample_rate(tutti_core::SampleRate(44100.0));
        let mut out = [0.0f32; 6];
        // ch0 loud, others at half — the whole frame should be limited together.
        let inp = [1.0f32, 0.5, 0.5, 0.5, 0.5, 0.5];
        for _ in 0..500 {
            lim.tick(&inp, &mut out);
        }
        // Once limiting, every channel keeps its input ratio to ch0.
        if out[0].abs() > 1e-3 {
            for c in 1..6 {
                let ratio = out[c].abs() / out[0].abs();
                assert!(
                    (ratio - 0.5).abs() < 0.1,
                    "ch{c} not linked to ch0: ratio {ratio}"
                );
            }
        }
    }

    #[test]
    fn brickwall_with_channels_reports_arity_and_clips_all() {
        let mut bw = BrickwallLimiterNode::with_channels(ChannelLayout::from(6u16), 0.0);
        assert_eq!(bw.inputs(), 6);
        assert_eq!(bw.outputs(), 6);
        let mut out = [0.0f32; 6];
        bw.tick(&[2.0, -2.0, 3.0, -3.0, 0.5, -0.5], &mut out);
        for (c, &y) in out.iter().enumerate() {
            assert!(
                (-1.0 - 1e-6..=1.0 + 1e-6).contains(&y),
                "ch{c} not clipped: {y}"
            );
        }
        assert!((out[0] - 1.0).abs() < 1e-4);
        assert!((out[1] + 1.0).abs() < 1e-4);
    }

    #[test]
    fn test_limiter_sliding_minimum_releases_after_window() {
        let mut lim = LimiterNode::new(-12.0, -0.3).with_lookahead(0.002);
        lim.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 2];
        lim.tick(&[1.0, 1.0], &mut out);
        let reduction_after_transient = lim.gain_reduction_db();
        assert!(
            reduction_after_transient > Db::UNITY,
            "Loud input should trigger gain reduction"
        );

        for _ in 0..20000 {
            lim.tick(&[0.0, 0.0], &mut out);
        }
        assert!(
            lim.gain_reduction_db() < Db(0.5),
            "After long quiet, gain reduction should release: {}",
            lim.gain_reduction_db()
        );
    }

    /// Width and modulation are independent axes.
    ///
    /// The regression for the bug this constructor had: it delegated to
    /// `Self::new`, which is stereo, so a modulated 6-channel limiter came back
    /// *stereo*. The arity assertion fails against that version.
    #[test]
    fn a_modulated_limiter_is_as_wide_as_it_was_asked_for() {
        let l = LimiterNode::with_param_inputs(ChannelLayout::from(6u16), -6.0, -0.3, true, true);
        assert_eq!(l.outputs(), 6, "the width is what was asked for");
        assert_eq!(
            l.inputs(),
            8,
            "six audio inputs, then ceiling and threshold"
        );
        assert_eq!(
            l.ceiling_port(),
            Some(6),
            "param ports follow the audio inputs, so their indices move with the width"
        );
        assert_eq!(
            l.threshold_port(),
            Some(7),
            "and keep their documented order"
        );
    }
}
