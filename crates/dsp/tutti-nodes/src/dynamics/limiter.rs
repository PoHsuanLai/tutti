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
use tutti_core::ChannelLayout;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};

use super::envelope::EnvelopeFollower;
use super::utils::{amplitude_to_db, compute_limiter_gain, db_to_amplitude, smooth_envelope};
use crate::buffer::{CircularBuffer, MonotonicMinDeque};
use tutti_core::{Db, Param, SampleRate, Samples, Seconds, Tail};
use tutti_types::Latency;
use tutti_types::UnitParam;

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
/// That delay is real latency: the node declares it in its [`Shape`], and the
/// graph's PDC compensates a dry path mixed against it.
///
/// The minimum gain over the lookahead window is tracked with a
/// [`MonotonicMinDeque`], so each sample costs O(1) amortized regardless of
/// lookahead length. Nothing in the RT path allocates: the rings and scratch
/// frames are sized at construction, the ring when the graph prepares it.
///
/// # Modulated params
///
/// The default node is 2-in / 2-out (audio L/R on ports 0/1). The ceiling
/// and the threshold (both dB) are modulatable by the graph (design doc 013
/// item 6), in that port order ([`LIMITER_PARAMS`]): a per-frame value on
/// the param port ([`Io::param`](tutti_graph::Io::param)) overrides its cell
/// per sample. Unmodulated, the node reads its cells once per block, which
/// is the common case; the arity never changes.
///
/// # In a graph
///
/// A native node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over threshold, ceiling and release, and a fork of it starts from the
/// values last set through that set. The graph prepares it at the device
/// rate before its first block, which sizes the ring.
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
    /// the lookahead up a little on every re-`prepare`, and the drift
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
}

/// The params a [`LimiterNode`] lets the graph modulate, in port order.
pub const LIMITER_PARAMS: [UnitParam; 2] = [UnitParam::Ceiling, UnitParam::Threshold];

/// The params a [`BrickwallLimiterNode`] lets the graph modulate.
pub const BRICKWALL_PARAMS: [UnitParam; 1] = [UnitParam::Ceiling];

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
    /// The lookahead ring is sized at the rate [`Node::prepare`] hands it.
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
    /// The lookahead ring is sized in frames from the 5 ms window, and the
    /// envelope follower's coefficients are derived the same way, at the
    /// rate [`Node::prepare`] hands it; both are re-derived from the stored
    /// window whenever the graph re-prepares it at a new rate.
    pub fn with_channels(
        channels: impl Into<ChannelLayout>,
        threshold_db: impl Into<Db>,
        ceiling_db: impl Into<Db>,
    ) -> Self {
        let layout = channels.into();
        // An empty layout would leave every scratch `Vec` zero-length and make
        // the shape report no channels, so clamp to at least mono — the same
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
        }
    }

    /// The audio width this limiter was built for.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
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
        // Keep the request, not just the count it derives: `prepare`
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
    /// **A threshold the graph feeds overrides this per sample.** Shared
    /// across clones.
    pub fn threshold(&self) -> Arc<AtomicF32> {
        self.threshold_db.as_atomic()
    }

    /// The shared ceiling cell in [`Db`] — the hard bound the output is not
    /// allowed to exceed.
    ///
    /// **A ceiling the graph feeds overrides this per sample.**
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
    /// While the graph modulates the threshold this sets the *base* its
    /// modulation rides on.
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

impl Node for LimiterNode {
    /// `channels` in and out, ceiling then threshold modulatable
    /// ([`LIMITER_PARAMS`]). Its latency is the lookahead ring's, at the
    /// prepared rate: the audio is delayed by the window.
    ///
    /// Its tail is the ring's contents, which outlive a silent input: the
    /// ring is a fixed delay, so when the input stops it still holds that
    /// many frames. The release envelope is deliberately not included — it
    /// shapes gain, and gain applied to silence is silence.
    fn shape(&self) -> Shape {
        let tail = match self.ring.ring_out() {
            s if s.is_zero() => Tail::None,
            s => Tail::Finite(s),
        };
        Shape::audio(self.layout, self.layout)
            .with_latency(Latency::new(Samples(self.ring.lookahead_samples)))
            .with_tail(tail)
            .with_params(&LIMITER_PARAMS)
    }

    fn prepare(&mut self, p: &Prepare) {
        // Re-derive the frame count from the *requested* window, so the
        // wall-clock lookahead survives a rate change. Deriving it from the
        // ring's current count instead would re-ceil an already-ceiled value and
        // ratchet the window up on every call — see the `lookahead` field.
        let sample_rate = p.sample_rate();
        self.sample_rate = sample_rate;
        self.follower
            .set_sample_rate(sample_rate, Seconds(0.0), self.release.load());
        let new_samples = self.lookahead.to_samples_ceil(sample_rate).get();
        self.ring.resize(new_samples.max(1));
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        self.update_coefficients();
        let (ceiling_fed, threshold_fed) = (io.param(0).frames(), io.param(1).frames());
        let base_threshold = self.threshold_db.load();
        let base_ceiling = self.ceiling_db.load();
        let (inputs, mut outputs) = io.split();

        // Reuse the two fixed scratch frames (sized at construction). Taken so
        // `process_frame_with` can borrow `self` without aliasing them.
        let mut in_frame = core::mem::take(&mut self.in_frame);
        let mut out_frame = core::mem::take(&mut self.out_frame);
        for i in 0..size {
            for (c, slot) in in_frame.iter_mut().enumerate() {
                *slot = inputs.get(c)[i];
            }
            let threshold = threshold_fed.map_or(base_threshold, |v| Db(v[i]));
            let ceiling = ceiling_fed.map_or(base_ceiling, |v| Db(v[i]));
            self.process_frame_with(&in_frame, threshold, ceiling, &mut out_frame);
            for (c, &y) in out_frame.iter().enumerate() {
                outputs.get(c)[i] = y;
            }
        }
        self.in_frame = in_frame;
        self.out_frame = out_frame;
        Status::Modified
    }

    fn reset(&mut self) {
        self.ring.clear();
        self.envelope = 0.0;
        self.gain_reduction_db = Db::UNITY;
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.ceiling_db.load().get()),
            1 => Some(self.threshold_db.load().get()),
            _ => None,
        }
    }
}

impl ParamNode for LimiterNode {
    /// Threshold, ceiling and release.
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Threshold, self.threshold())
            .param(UnitParam::Ceiling, self.ceiling())
            .param(UnitParam::Release, self.release_time())
            .build()
    }

    /// A clone with its three cells detached, its ring and envelope
    /// cleared.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.threshold_db.detach();
        fork.ceiling_db.detach();
        fork.release.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for LimiterNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

/// Shares the control cells (the template [`tutti_graph::param_parts`]
/// forks from); the ring and envelope are copied.
impl Clone for LimiterNode {
    fn clone(&self) -> Self {
        Self {
            threshold_db: self.threshold_db.handle(),
            ceiling_db: self.ceiling_db.handle(),
            release: self.release.handle(),
            ring: self.ring.clone(),
            // Carried, not re-derived: a clone may be prepared at a different
            // rate, so a clone that lost the request would re-derive its
            // lookahead from the ceiled count.
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
        }
    }
}

/// Hard clipper at ceiling. No lookahead, zero latency.
/// 2 inputs (L/R), 2 outputs (L/R).
///
/// # Modulated ceiling
///
/// The default node is 2-in / 2-out (audio L/R on ports 0/1). The ceiling
/// (dB) is modulatable by the graph (design doc 013 item 6;
/// [`BRICKWALL_PARAMS`]): modulated → the param port overrides the ceiling
/// cell per sample, unmodulated → bit-identical to a node nothing modulates.
///
/// A native node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over the ceiling.
pub struct BrickwallLimiterNode {
    ceiling_db: Param<Db>,
    ceiling_linear: f32,
    /// Audio channel width (`inputs()` audio ports == `outputs()`). The clip is
    /// stateless and per-channel, so widening is purely the port count.
    layout: ChannelLayout,
    /// [`layout`](Self::layout)'s count, cached as the iteration stride — see
    /// the note on [`LimiterNode::channels`]. Set once at construction.
    channels: usize,
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
        }
    }

    /// The audio width this limiter was built for.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// The shared ceiling cell in [`Db`] — the level samples are clamped to.
    ///
    /// **A ceiling the graph feeds overrides this per sample.** Writing the
    /// raw cell does *not* refresh the cached linear ceiling the
    /// unmodulated path clamps against; use
    /// [`set_ceiling`](Self::set_ceiling) for that.
    pub fn ceiling(&self) -> Arc<AtomicF32> {
        self.ceiling_db.as_atomic()
    }

    /// Sets the clamp ceiling in [`Db`], refreshing the cached linear
    /// amplitude.
    ///
    /// `&mut self` because of that cache, so this cannot reach a node already
    /// live in the graph — a live ceiling change is a write through its
    /// [`ParamSet`] (the cell, which the node re-reads once per block) or the
    /// graph's modulation of it.
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

impl Node for BrickwallLimiterNode {
    /// `channels` in and out, the ceiling modulatable
    /// ([`BRICKWALL_PARAMS`]). The clip is stateless, so it has no latency
    /// and stops with its input.
    fn shape(&self) -> Shape {
        Shape::audio(self.layout, self.layout)
            .with_tail(Tail::None)
            .with_params(&BRICKWALL_PARAMS)
    }

    /// Nothing to prepare, and the only node in this crate that can honestly
    /// say so: a brickwall is a memoryless `clamp` with no lookahead, no
    /// envelope and therefore no time constant to derive from the rate.
    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let n = self.channels;
        let ceiling_fed = io.param(0).frames();
        let (inputs, mut outputs) = io.split();

        // Modulated path: read the ceiling per sample.
        if let Some(v) = ceiling_fed {
            for (i, &db) in v.iter().enumerate() {
                let ceiling_linear = db_to_amplitude(Db(db)).get();
                for c in 0..n {
                    outputs.get(c)[i] = Self::clip_at(inputs.get(c)[i], ceiling_linear);
                }
            }
            return Status::Modified;
        }

        let ceiling = self.ceiling_db.load();
        if (db_to_amplitude(ceiling).get() - self.ceiling_linear).abs() > 0.0001 {
            self.ceiling_linear = db_to_amplitude(ceiling).get();
        }

        for i in 0..size {
            for c in 0..n {
                outputs.get(c)[i] = self.clip(inputs.get(c)[i]);
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}

    fn param_base(&self, k: usize) -> Option<f32> {
        (k == 0).then(|| self.ceiling_db.load().get())
    }
}

impl ParamNode for BrickwallLimiterNode {
    /// The ceiling. A write through the set reaches the clip on the next
    /// block: it re-derives its cached linear ceiling from the cell.
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Ceiling, self.ceiling())
            .build()
    }

    /// A clone with its ceiling cell detached.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.ceiling_db.detach();
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the value
/// last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for BrickwallLimiterNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

/// Shares the ceiling cell (the template [`tutti_graph::param_parts`] forks
/// from).
impl Clone for BrickwallLimiterNode {
    fn clone(&self) -> Self {
        Self {
            ceiling_db: self.ceiling_db.handle(),
            ceiling_linear: self.ceiling_linear,
            layout: self.layout,
            channels: self.channels,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{prepared_at, tick, tick_fed, RATE_44K};
    use tutti_graph::contract::assert_param_fork;

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it.
    ///
    /// Mutation (run): drop `fork.release.detach()` in `fork_fresh` → "a
    /// live write reached the fork" for `Release` → fails.
    #[test]
    fn a_limiter_fork_shares_no_cell() {
        assert_param_fork(LimiterNode::new(-6.0, -0.3));
    }

    /// Mutation (run): drop `fork.ceiling_db.detach()` in `fork_fresh` → "a
    /// live write reached the fork" for `Ceiling` → fails.
    #[test]
    fn a_brickwall_fork_shares_no_cell() {
        assert_param_fork(BrickwallLimiterNode::new(-1.0));
    }

    /// A ceiling set through the brickwall's `ParamSet` reaches the clip on
    /// the next block, though `set_ceiling` is `&mut self`: the node
    /// re-derives its cached linear ceiling from the cell once per block.
    ///
    /// Mutation (run): drop the re-derive in `process` → the clip stays at
    /// 0 dB → fails.
    #[test]
    fn a_brickwall_ceiling_set_by_address_reaches_the_clip() {
        let mut bw = prepared_at(BrickwallLimiterNode::new(0.0), RATE_44K);
        let set = bw.param_set();
        assert!(set.set(UnitParam::Ceiling, -6.0));
        let mut out = [0.0f32; 2];
        tick(&mut bw, &[1.0, -1.0], &mut out);
        let ceiling = db_to_amplitude(-6.0).get();
        assert!((out[0] - ceiling).abs() < 1e-4, "clipped at {}", out[0]);
    }

    /// A limiter is **born at the placeholder rate**, and `prepare` is
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
    /// Asserted through the shape's declared latency, the same figure PDC
    /// compensates against, so a regression here is a regression in what the graph is told —
    /// not merely in a private field.
    #[test]
    fn lookahead_is_a_duration_and_prepare_is_what_preserves_it() {
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
        lim.prepare(&Prepare::new(device, Samples(64)));
        let fixed = lim.ring.lookahead_samples;
        assert_eq!(
            fixed,
            LOOKAHEAD.to_samples_ceil(device).get(),
            "prepare must re-derive the frame count at the new rate"
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
            lim.shape().latency,
            Latency::new(Samples(fixed)),
            "reported latency must track the resized ring"
        );

        // Repeated rate changes must land on the same count each time. The
        // window is `_ceil`ed, so re-deriving it from the ring's *count* rather
        // than from the stored request re-rounds an already-rounded value and
        // ratchets the lookahead up a little on every call — a drift that
        // accumulates instead of cancelling. Going back and forth pins that the
        // derivation is a pure function of (request, rate).
        for _ in 0..4 {
            lim.prepare(&Prepare::new(SampleRate::DEFAULT, Samples(64)));
            assert_eq!(
                lim.ring.lookahead_samples, born,
                "returning to the original rate must return the original count"
            );
            lim.prepare(&Prepare::new(device, Samples(64)));
            assert_eq!(
                lim.ring.lookahead_samples, fixed,
                "a rate change must not accumulate rounding across calls"
            );
        }
    }

    #[test]
    fn test_limiter_reduces_loud_signal() {
        let mut lim = prepared_at(LimiterNode::new(-6.0, -0.3), RATE_44K);

        let loud = 1.0f32;
        let mut out = [0.0f32; 2];

        for _ in 0..500 {
            tick(&mut lim, &[loud, loud], &mut out);
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
        let mut lim = prepared_at(LimiterNode::new(-6.0, -0.3), RATE_44K);

        let quiet = 0.1f32;
        let mut out = [0.0f32; 2];

        for _ in 0..500 {
            tick(&mut lim, &[quiet, quiet], &mut out);
        }

        assert!(
            out[0].abs() > 0.01,
            "Quiet signal should pass through, got {}",
            out[0]
        );
    }

    #[test]
    fn test_limiter_stereo_linked() {
        let mut lim = prepared_at(LimiterNode::new(-6.0, -0.3), RATE_44K);

        let mut out = [0.0f32; 2];

        for _ in 0..500 {
            tick(&mut lim, &[1.0, 0.1], &mut out);
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
        let mut lim = prepared_at(LimiterNode::new(-6.0, -0.3), RATE_44K);

        let mut out = [0.0f32; 2];
        for _ in 0..500 {
            tick(&mut lim, &[1.0, 1.0], &mut out);
        }

        lim.reset();
        assert_eq!(lim.gain_reduction_db(), Db::UNITY);
    }

    #[test]
    fn test_brickwall_clips_at_ceiling() {
        let mut bw = BrickwallLimiterNode::new(0.0);

        let mut out = [0.0f32; 2];
        tick(&mut bw, &[2.0, -3.0], &mut out);

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
        tick(&mut bw, &[1.0, -1.0], &mut out);

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
        tick(&mut bw, &[0.3, -0.2], &mut out);

        assert!((out[0] - 0.3).abs() < 0.001);
        assert!((out[1] - (-0.2)).abs() < 0.001);
    }

    // ── Modulated params (the graph's param feed) ───────────────────────────

    /// The shapes declare ceiling then threshold (and the brickwall its
    /// ceiling), and never change the arity: a modulatable limiter is as
    /// wide as it was built.
    ///
    /// Mutation (run): swap `LIMITER_PARAMS`' order → the first assertion
    /// fails.
    #[test]
    fn the_shapes_declare_ceiling_then_threshold() {
        let l = LimiterNode::with_channels(ChannelLayout::from(6u16), -6.0, -0.3);
        assert_eq!(
            l.shape().params.as_slice(),
            &[UnitParam::Ceiling, UnitParam::Threshold][..]
        );
        assert_eq!(
            (l.shape().audio_in.count(), l.shape().audio_out.count()),
            (6, 6)
        );
        assert_eq!(l.param_base(0), Some(-0.3), "the ceiling's base");
        assert_eq!(l.param_base(1), Some(-6.0), "the threshold's base");
        let b = BrickwallLimiterNode::new(-1.0);
        assert_eq!(b.shape().params.as_slice(), &[UnitParam::Ceiling][..]);
        assert_eq!(
            (b.shape().audio_in.count(), b.shape().audio_out.count()),
            (2, 2)
        );
        assert_eq!(b.param_base(0), Some(-1.0));
    }

    #[test]
    fn limiter_unmodulated_matches_held_constant() {
        // A node whose fed ceiling and threshold are held at the same values
        // as a plain node's atomics must produce identical output.
        let mut plain = prepared_at(LimiterNode::new(-6.0, -0.3), RATE_44K);

        let mut modn = prepared_at(LimiterNode::new(-6.0, -0.3), RATE_44K);

        let mut plain_out = [0.0f32; 2];
        let mut mod_out = [0.0f32; 2];
        for n in 0..2000 {
            // Mix of loud and quiet to exercise gain reduction + release.
            let s = if n % 400 < 200 { 0.9 } else { 0.05 };
            tick(&mut plain, &[s, s], &mut plain_out);
            // Held: ceiling = -0.3, threshold = -6.0.
            tick_fed(&mut modn, &[s, s], &[Some(-0.3), Some(-6.0)], &mut mod_out);
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
    fn brickwall_unmodulated_matches_held_constant() {
        // A brickwall whose fed ceiling is held at the atomic value must clip
        // identically to a plain brickwall.
        let mut plain = BrickwallLimiterNode::new(-6.0);
        let mut modn = BrickwallLimiterNode::new(-6.0);

        let mut plain_out = [0.0f32; 2];
        let mut mod_out = [0.0f32; 2];
        let samples = [2.0, -3.0, 0.1, -0.05, 1.5, -1.5];
        for &s in &samples {
            tick(&mut plain, &[s, -s], &mut plain_out);
            tick_fed(&mut modn, &[s, -s], &[Some(-6.0)], &mut mod_out);
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
    fn brickwall_fed_ceiling_modulates_clip() {
        // A fed ceiling held low clips harder than one held high.
        let mut bw = BrickwallLimiterNode::new(0.0);
        let mut out = [0.0f32; 2];
        // Ceiling -12 dB ≈ 0.251 linear: 1.0 clips to ~0.251.
        tick_fed(&mut bw, &[1.0, 1.0], &[Some(-12.0)], &mut out);
        let low_ceiling = out[0];
        // Ceiling 0 dB = 1.0 linear: 1.0 passes through.
        tick_fed(&mut bw, &[1.0, 1.0], &[Some(0.0)], &mut out);
        let high_ceiling = out[0];
        assert!(
            high_ceiling > low_ceiling + 0.1,
            "higher fed ceiling should clip less: low={low_ceiling}, high={high_ceiling}"
        );
    }

    // ── Width-native (N-channel) ─────────────────────────────────────────────

    /// A stereo limiter fed on one channel only limits that channel and
    /// leaves the other silent: the graph hands an unconnected input
    /// silence, which the linked detector reads as such. (What replaced the
    /// `tick` short-frame fallback, which duplicated the last channel: a
    /// native node is always handed every input it declares.)
    ///
    /// Mutation (run): read `inputs.get(0)` for every channel in `process`
    /// → ch1 carries ch0's signal → fails.
    #[test]
    fn limiter_leaves_an_unfed_channel_silent() {
        let mut lim = prepared_at(LimiterNode::new(-6.0, -0.3), RATE_44K);
        let mut out = [0.0f32; 2];
        // Run past the 5 ms lookahead so the delayed signal emerges.
        for _ in 0..500 {
            tick(&mut lim, &[0.9, 0.0], &mut out);
        }
        assert!(out[0].abs() > 1e-4, "ch0 silent: {}", out[0]);
        assert_eq!(out[1], 0.0, "ch1 carries a signal it was never fed");
    }

    #[test]
    fn limiter_with_channels_reports_arity() {
        let l = LimiterNode::with_channels(ChannelLayout::from(6u16), -6.0, -0.3);
        assert_eq!(l.shape().audio_in.count(), 6);
        assert_eq!(l.shape().audio_out.count(), 6);
    }

    #[test]
    fn limiter_with_channels_2_matches_new() {
        let mut a = prepared_at(LimiterNode::new(-6.0, -0.3), RATE_44K);
        let mut b = prepared_at(
            LimiterNode::with_channels(ChannelLayout::STEREO, -6.0, -0.3),
            RATE_44K,
        );
        let mut oa = [0.0f32; 2];
        let mut ob = [0.0f32; 2];
        for i in 0..2000 {
            let s = if i % 400 < 200 { 0.95 } else { 0.05 };
            tick(&mut a, &[s, s * 0.5], &mut oa);
            tick(&mut b, &[s, s * 0.5], &mut ob);
            assert_eq!(oa[0].to_bits(), ob[0].to_bits(), "L bit-diff at {i}");
            assert_eq!(oa[1].to_bits(), ob[1].to_bits(), "R bit-diff at {i}");
        }
    }

    #[test]
    fn wide_limiter_gain_is_linked_across_all_channels() {
        // A loud transient on one channel must reduce ALL channels by the same
        // linked gain (max-abs across the frame), preserving inter-channel ratios.
        let mut lim = prepared_at(
            LimiterNode::with_channels(ChannelLayout::from(6u16), -6.0, -0.3),
            RATE_44K,
        );
        let mut out = [0.0f32; 6];
        // ch0 loud, others at half — the whole frame should be limited together.
        let inp = [1.0f32, 0.5, 0.5, 0.5, 0.5, 0.5];
        for _ in 0..500 {
            tick(&mut lim, &inp, &mut out);
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
        assert_eq!(bw.shape().audio_in.count(), 6);
        assert_eq!(bw.shape().audio_out.count(), 6);
        let mut out = [0.0f32; 6];
        tick(&mut bw, &[2.0, -2.0, 3.0, -3.0, 0.5, -0.5], &mut out);
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
        let mut lim = prepared_at(
            LimiterNode::new(-12.0, -0.3).with_lookahead(0.002),
            RATE_44K,
        );

        let mut out = [0.0f32; 2];
        tick(&mut lim, &[1.0, 1.0], &mut out);
        let reduction_after_transient = lim.gain_reduction_db();
        assert!(
            reduction_after_transient > Db::UNITY,
            "Loud input should trigger gain reduction"
        );

        for _ in 0..20000 {
            tick(&mut lim, &[0.0, 0.0], &mut out);
        }
        assert!(
            lim.gain_reduction_db() < Db(0.5),
            "After long quiet, gain reduction should release: {}",
            lim.gain_reduction_db()
        );
    }
}
