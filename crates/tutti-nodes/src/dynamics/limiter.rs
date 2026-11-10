//! Two limiters: the lookahead [`LimiterNode`] and the hard-clipping
//! [`BrickwallLimiterNode`].
//!
//! They are not interchangeable. The lookahead limiter delays its audio so the
//! gain decision runs ahead of a peak, which costs latency and sounds
//! transparent; the brickwall clamps in place, which costs nothing and
//! distorts. Reach for the first to limit musically, the second as a safety
//! catch.

use std::sync::Arc;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::ChannelLayout;
use tutti_types::{AtomicF32, AtomicF64};

use crate::ramp::LastGood;
use tutti_dsp::{
    EnvelopeFollower, LimiterCurve, LookaheadRing, amplitude_to_db, brickwall_clip,
    compute_limiter_gain, db_to_amplitude, fold_peak, smooth_envelope,
};
use tutti_types::Latency;
use tutti_types::UnitParam;
use tutti_types::{Db, Param, SampleRate, Samples, Seconds, Tail};

/// The lane's length before the graph prepares the node.
const DEFAULT_LANE: usize = 512;

/// Lookahead limiter with gain reduction linked across channels. 2 inputs, 2
/// outputs by default.
///
/// A limiter is a compressor at an effectively infinite ratio: nothing is
/// allowed past the ceiling: the output is bounded by it (a gain divide and a
/// product can in principle round one ulp past).
///
/// # The curve
///
/// The threshold drives the input. With `drive = ceiling - threshold` in dB,
/// the gain at a linked peak is `min(drive, ceiling / peak)`, so the output
/// is `min(peak * drive, ceiling)`: under the threshold the signal is
/// moved by the drive (boosted when the ceiling is above the threshold,
/// attenuated when below), and a louder one settles at the ceiling. The curve
/// is continuous and monotonic, with its knee at the threshold. With the
/// defaults (-6 dB threshold, -0.3 dB ceiling) a quiet signal is raised by
/// 5.7 dB and the output reaches -0.3 dB when the input reaches -6 dB. The
/// gain is then smoothed (instant attack, `release`) and its minimum over
/// the lookahead window applied. The **lookahead** is what makes that possible
/// without distortion — the audio is delayed by the lookahead window while the
/// gain decision runs ahead of it, so reduction is already in place when a peak
/// arrives rather than chasing it.
///
/// That delay is real latency: the node declares it in its [`Shape`], and the
/// graph's PDC compensates a dry path mixed against it.
///
/// The minimum gain over the lookahead window is tracked with a
/// [`MonotonicMinDeque`](tutti_dsp::MonotonicMinDeque), so each sample costs O(1) amortized regardless of
/// lookahead length. Nothing in the RT path allocates: the rings and the gain
/// lane are sized at construction, and again when the graph prepares it.
///
/// # Modulated params
///
/// The default node is 2-in / 2-out (audio L/R on ports 0/1). The ceiling
/// and the threshold (both dB) are modulatable by the graph,
/// in that port order ([`LIMITER_PARAMS`]): a per-frame value on
/// the param port ([`Io::param`](tutti_graph::Io::param)) overrides its cell
/// per sample. Unmodulated, the node reads its cells once per block, which
/// is the common case; the arity never changes.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over threshold, ceiling and release, and a fork of it starts from the
/// values last set through that set. The graph prepares it at the device
/// rate before its first block, which sizes the ring.
pub struct LimiterNode {
    threshold_db: Param<Db>,
    ceiling_db: Param<Db>,
    release: Param<Seconds>,
    /// The last finite release, driven or written: a NaN release would
    /// make the envelope's coefficient NaN, and the envelope NaN for good
    /// (see [`LastGood`]).
    good_release: LastGood,

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
    /// Same reason `release` is a `Param<Seconds>` and the compressor and gate
    /// keep their `AttackRelease` in [`Seconds`]: the derived coefficient is
    /// never the recoverable form.
    lookahead: Seconds,
    /// Audio channel width. Gain reduction is linked across all channels (peak
    /// = max-abs over the frame), matching the stereo-linked design.
    layout: ChannelLayout,
    /// [`layout`](Self::layout)'s count, cached as the iteration stride.
    ///
    /// The layout is the *declaration*; this is the arithmetic derived from it.
    /// They are kept as separate fields because `process` loops over the
    /// channels, and deriving the count there would put a `match` in the loop.
    /// Set once at construction, so the two can never disagree.
    channels: usize,
    /// The per-frame lane a block works in (peak level, then window-min
    /// gain), sized to the prepared `MaxBlock`; a longer block runs in
    /// lane-sized chunks.
    lane: Vec<f32>,
    envelope: f32,
    gain_reduction_db: Db,
    sample_rate: SampleRate,
    follower: EnvelopeFollower,
}

impl std::fmt::Debug for LimiterNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LimiterNode")
            .field("threshold_db", &self.threshold_db)
            .field("ceiling_db", &self.ceiling_db)
            .field("release", &self.release)
            .field("good_release", &self.good_release)
            .field("ring", &self.ring)
            .field("lookahead", &self.lookahead)
            .field("layout", &self.layout)
            .field("channels", &self.channels)
            .field("envelope", &self.envelope)
            .field("gain_reduction_db", &self.gain_reduction_db)
            .field("sample_rate", &self.sample_rate)
            .field("follower", &self.follower)
            .finish_non_exhaustive()
    }
}

/// The params a [`LimiterNode`] lets the graph modulate, in port order.
/// The ceiling and threshold are read per sample; the release is
/// block-rate in the node, so a driven port is read once per block
/// (`block_driven`).
pub const LIMITER_PARAMS: [UnitParam; 3] =
    [UnitParam::Ceiling, UnitParam::Threshold, UnitParam::Release];

/// The params a [`BrickwallLimiterNode`] lets the graph modulate.
pub const BRICKWALL_PARAMS: [UnitParam; 1] = [UnitParam::Ceiling];

impl LimiterNode {
    /// Builds a stereo lookahead limiter with a 5 ms lookahead and 100 ms
    /// release.
    ///
    /// `ceiling_db` is the hard output bound and `threshold_db` the input
    /// level that reaches it, both in [`Db`] and typically negative. The
    /// threshold drives the input: signal under it is moved by
    /// `ceiling_db - threshold_db` (a boost when the ceiling is above the
    /// threshold, as with the usual `-6` and `-0.3`), and signal over it is
    /// held at the ceiling. A mastering limiter often sits at a ceiling of
    /// `-0.3` to leave inter-sample headroom. Adjust the window with
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
        // An empty layout would leave the rings empty and make
        // the shape report no channels, so clamp to at least mono.
        let layout = ChannelLayout::from_count(layout.count().max(1));
        let n = usize::from(layout.count());
        // Narrowed, as `with_lookahead` stores it: the ring is sized by a `_ceil`.
        let lookahead_secs = Seconds(0.005).narrowed();
        // `_ceil`, the allocation form: the ring must hold at *least* the
        // lookahead, and nearest-rounding under-allocates for half of all
        // inputs.
        let lookahead_samples = lookahead_secs.to_samples_ceil(SampleRate::DEFAULT).get();

        Self {
            threshold_db: Param::new(threshold_db.into()),
            ceiling_db: Param::new(ceiling_db.into()),
            release: Param::new(Seconds(0.1)),
            good_release: LastGood::fixed(0.1),
            ring: LookaheadRing::new(n, lookahead_samples),
            lookahead: lookahead_secs,
            layout,
            channels: n,
            lane: vec![0.0; DEFAULT_LANE],
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
    #[must_use]
    pub fn with_lookahead(mut self, lookahead: impl Into<Seconds>) -> Self {
        // Keep the request, not just the count it derives: `prepare`
        // re-derives from this, and a count is `_ceil`ed and so cannot round-trip
        // back to the seconds that produced it.
        self.lookahead = lookahead.into().narrowed();
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
    #[must_use]
    pub fn with_release(self, release_secs: impl Into<Seconds>) -> Self {
        self.release.store(Self::floor_release(release_secs.into()));
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
    pub fn release_time(&self) -> Arc<AtomicF64> {
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
        self.release.store(Self::floor_release(secs.into()));
    }

    /// A release time as the setters store it: floored at 1 ms. Shared by
    /// the setters and a driven port's value, so a driven negative release
    /// releases at the floor rather than instantly.
    #[inline]
    fn floor_release(secs: Seconds) -> Seconds {
        Seconds(secs.get().max(0.001))
    }

    /// [`floor_release`](Self::floor_release) for a driven port's `f32`.
    #[inline]
    fn clamp_release(secs: f32) -> f32 {
        secs.max(0.001)
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
    /// `driven` is the block's release from the node's param port when a
    /// lane drives it; `None` reads the cell.
    fn update_coefficients(&mut self, driven: Option<f32>) {
        // The setter's floor at the point of use, so a driven value (or a
        // raw `ParamSet` write) gets it too; after the `LastGood` read,
        // which needs to see a NaN that `max` would hide.
        let release = Self::clamp_release(
            self.good_release
                .read(driven.unwrap_or_else(|| self.release.load().to_f32())),
        );
        self.follower
            .update_coefficients(Seconds(0.0), Seconds::from_f32(release));
    }
}

impl Node for LimiterNode {
    /// `channels` in and out, ceiling, threshold and release modulatable
    /// ([`LIMITER_PARAMS`]). Its latency is the lookahead ring's, at the
    /// prepared rate: the audio is delayed by the window.
    ///
    /// It has no tail. A tail excludes latency, and the ring is a pure
    /// delay of exactly that latency: when the input stops it holds the last
    /// `lookahead` frames, which come out within the latency PDC compensates.
    /// The release envelope adds nothing after them: it shapes gain, and gain
    /// applied to silence is silence.
    fn shape(&self) -> Shape {
        Shape::audio(self.layout, self.layout)
            .with_cost(tutti_graph::Cost::Moderate)
            .with_latency(Latency::new(Samples(self.ring.lookahead_samples())))
            .with_tail(Tail::None)
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
        let lane_len = p.max_block().get().max(1);
        if self.lane.len() != lane_len {
            self.lane = vec![0.0; lane_len];
        }
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        self.update_coefficients(crate::ramp::block_driven(&io, 2));
        let (ceiling_fed, threshold_fed) = (io.param(0).frames(), io.param(1).frames());
        let base_threshold = self.threshold_db.load();
        let base_ceiling = self.ceiling_db.load();
        let (inputs, mut outputs) = io.split();
        let release_coeff = self.follower.release_coeff();
        // Unfed, the curve is constant across the block: its constants are a
        // few `powf`s here, and a frame is a compare and a divide.
        let curve = (threshold_fed.is_none() && ceiling_fed.is_none())
            .then(|| LimiterCurve::new(base_threshold, base_ceiling));

        // Borrowed in place, so a panic caught outside leaves the lane intact.
        let lane = &mut self.lane;
        let mut envelope = self.envelope;
        let mut last_gain = None;
        let mut start = 0;
        while start < size {
            let end = (start + lane.len()).min(size);
            let gains = &mut lane[..end - start];

            // Linked peak: the max-abs over the frame, taken a channel at a
            // time (`max` is exact, so the order does not matter).
            gains.fill(0.0);
            for c in 0..self.channels {
                for (p, &x) in gains.iter_mut().zip(&inputs.get(c)[start..end]) {
                    *p = fold_peak(*p, x);
                }
            }

            // The recursive half: the envelope and the sliding minimum, one
            // frame at a time; each frame's peak becomes its window-min gain.
            for (k, p) in gains.iter_mut().enumerate() {
                let target_gain = match &curve {
                    Some(curve) => curve.gain(*p),
                    None => {
                        let peak_db = amplitude_to_db(*p);
                        let threshold = threshold_fed.map_or(base_threshold, |v| Db(v[start + k]));
                        let ceiling = ceiling_fed.map_or(base_ceiling, |v| Db(v[start + k]));
                        compute_limiter_gain(peak_db, threshold, ceiling).get()
                    }
                };
                if target_gain < envelope {
                    envelope = target_gain;
                } else {
                    envelope = smooth_envelope(envelope, target_gain, release_coeff);
                }
                *p = self.ring.push_gain(envelope);
            }
            last_gain = gains.last().copied();

            for c in 0..self.channels {
                self.ring.delay_scaled(
                    c,
                    &inputs.get(c)[start..end],
                    gains,
                    &mut outputs.get(c)[start..end],
                );
            }
            start = end;
        }
        self.envelope = envelope;

        // Metering only, read after the block, so the block's last frame is
        // the only one that is converted. Reported as a positive magnitude,
        // the sign convention `CompressorNode::gain_reduction_db` also
        // follows.
        //
        // `amplitude_to_db` owns the `log10(0)` guard, so silence pins at
        // `Db::FLOOR` rather than `-inf`, the same floor the detector path
        // above uses.
        if let Some(min_gain) = last_gain {
            self.gain_reduction_db = -amplitude_to_db(min_gain);
        }
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
            2 => Some(self.release.load().to_f32()),
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
            good_release: self.good_release,
            ring: self.ring.clone(),
            // Carried, not re-derived: a clone may be prepared at a different
            // rate, so a clone that lost the request would re-derive its
            // lookahead from the ceiled count.
            lookahead: self.lookahead,
            layout: self.layout,
            channels: self.channels,
            lane: self.lane.clone(),
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
/// The default node is 2-in / 2-out (audio L/R on ports 0/1). The ceiling (dB)
/// is modulatable by the graph ([`BRICKWALL_PARAMS`]): modulated → the param
/// port overrides the ceiling cell per sample, unmodulated → bit-identical to a
/// node nothing modulates.
///
/// A graph node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over the ceiling.
///
/// # A NaN ceiling is ignored
///
/// A NaN ceiling has no level to clamp to (and `f32::clamp` panics on a NaN
/// bound), so it is ignored wherever it comes from: a NaN in the cell keeps
/// the last ceiling the node clamped at, a NaN frame on the ceiling port
/// clamps at the cell's ceiling, and a NaN given to
/// [`new`](Self::new) starts the node at 0 dB. `±∞` dB are ceilings: `+∞`
/// passes everything, `-∞` mutes.
#[derive(Debug)]
pub struct BrickwallLimiterNode {
    ceiling_db: Param<Db>,
    /// The linear ceiling the unmodulated path clamps at: never NaN.
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
        let layout = ChannelLayout::from_count(channels.into().count().max(1));
        Self {
            ceiling_db: Param::new(ceiling_db),
            ceiling_linear: linear_ceiling(ceiling_db).unwrap_or(1.0),
            layout,
            channels: usize::from(layout.count()),
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
    /// amplitude. A NaN is stored but ignored: the clip keeps its ceiling.
    ///
    /// `&mut self` because of that cache, so this cannot reach a node already
    /// live in the graph — a live ceiling change is a write through its
    /// [`ParamSet`] (the cell, which the node re-reads once per block) or the
    /// graph's modulation of it.
    pub fn set_ceiling(&mut self, db: impl Into<Db>) {
        let db = db.into();
        self.ceiling_db.store(db);
        if let Some(linear) = linear_ceiling(db) {
            self.ceiling_linear = linear;
        }
    }

    #[inline]
    fn clip(&self, sample: f32) -> f32 {
        brickwall_clip(sample, self.ceiling_linear)
    }

    /// Clip against an explicit linear ceiling (the modulated path).
    #[inline]
    fn clip_at(sample: f32, ceiling_linear: f32) -> f32 {
        brickwall_clip(sample, ceiling_linear)
    }
}

/// `db` as a linear clamp bound, or `None` for a NaN, which has none.
#[inline]
fn linear_ceiling(db: Db) -> Option<f32> {
    let linear = db_to_amplitude(db).get();
    (!linear.is_nan()).then_some(linear)
}

impl Node for BrickwallLimiterNode {
    /// `channels` in and out, the ceiling modulatable
    /// ([`BRICKWALL_PARAMS`]). The clip is stateless, so it has no latency
    /// and stops with its input.
    fn shape(&self) -> Shape {
        Shape::audio(self.layout, self.layout)
            .with_cost(tutti_graph::Cost::Light)
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
        // The cell's ceiling, re-derived once per block: the unmodulated
        // clip, and a fed NaN frame's fallback. A NaN cell keeps the last.
        if let Some(linear) = linear_ceiling(self.ceiling_db.load())
            && (linear - self.ceiling_linear).abs() > 0.0001
        {
            self.ceiling_linear = linear;
        }
        // Channel-major: the clip is memoryless, so a sample's value depends
        // only on its input and its frame's ceiling, not on the loop order.

        // Modulated path: read the ceiling per frame. Each frame's linear
        // ceiling is converted once and lands in channel 0's output, which
        // every channel then clips against; channel 0 is clipped last, over
        // its own ceiling.
        if let Some(v) = ceiling_fed {
            let mut outs = outputs.iter_mut();
            let Some(first) = outs.next() else {
                return Status::Modified;
            };
            let first = &mut first[..size];
            let fallback = self.ceiling_linear;
            for (o, &db) in first.iter_mut().zip(v) {
                *o = linear_ceiling(Db(db)).unwrap_or(fallback);
            }
            for (c, out) in outs.enumerate() {
                let input = &inputs.get(c + 1)[..size];
                for ((o, &x), &ceiling) in out[..size].iter_mut().zip(input).zip(&*first) {
                    *o = Self::clip_at(x, ceiling);
                }
            }
            for (o, &x) in first.iter_mut().zip(&inputs.get(0)[..size]) {
                *o = Self::clip_at(x, *o);
            }
            return Status::Modified;
        }

        for c in 0..n {
            let (input, out) = (&inputs.get(c)[..size], &mut outputs.get(c)[..size]);
            for (o, &x) in out.iter_mut().zip(input) {
                *o = self.clip(x);
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
    use crate::test_support::{RATE_44K, prepared_at, tick, tick_fed};
    use tutti_graph::contract::{assert_param_fork, drive};

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
        let device = tutti_types::SampleRate(48_000.0);

        let mut lim = LimiterNode::new(-6.0, -0.3);

        // Born at the placeholder: the count is right for 44100 and wrong for
        // the device the node is about to run on.
        let born = lim.ring.lookahead_samples();
        assert_eq!(
            born,
            LOOKAHEAD
                .narrowed()
                .to_samples_ceil(SampleRate::DEFAULT)
                .get(),
            "construction must size the ring at the placeholder rate"
        );
        let born_secs = born as f64 / device.get();
        assert!(
            born_secs < LOOKAHEAD.get() * 0.95,
            "an uncorrected ring must be audibly short at {device:?} — that is \
             the hazard the constructor documents; got {born_secs:.5}s of the \
             {:.5}s asked for",
            LOOKAHEAD.get()
        );

        // The correction: same wall-clock window, more frames.
        lim.prepare(&Prepare::new(device, Samples(64)));
        let fixed = lim.ring.lookahead_samples();
        assert_eq!(
            fixed,
            LOOKAHEAD.narrowed().to_samples_ceil(device).get(),
            "prepare must re-derive the frame count at the new rate"
        );
        assert!(
            fixed > born,
            "a higher rate needs more frames for the same duration ({born} -> {fixed})"
        );
        let fixed_secs = fixed as f64 / device.get();
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
                lim.ring.lookahead_samples(),
                born,
                "returning to the original rate must return the original count"
            );
            lim.prepare(&Prepare::new(device, Samples(64)));
            assert_eq!(
                lim.ring.lookahead_samples(),
                fixed,
                "a rate change must not accumulate rounding across calls"
            );
        }
    }

    /// The limiter's output ends exactly its latency after its input:
    /// nothing rings once the lookahead is compensated, so it declares no
    /// tail. A tone over the threshold (so the release envelope is mid-swing
    /// when the input stops) leaves its last non-zero frame `lookahead`
    /// frames after the last input frame, and silence after that.
    ///
    /// Mutation (run): declaring the ring's `lookahead - 1` as the tail (the
    /// old figure) → the tail assertion fails. Mutation (run): each channel's
    /// ring one slot longer than the declared latency
    /// (`CircularBuffer::new(n + 1)`) → the last non-zero frame is one past
    /// the latency and the second assertion fails.
    #[test]
    fn the_limiter_stops_at_its_latency_and_declares_no_tail() {
        let mut lim = prepared_at(LimiterNode::new(-6.0, -0.3), RATE_44K);
        let shape = lim.shape();
        let n = shape.latency.samples().get();
        assert!(n > 1, "a lookahead to compensate");
        assert_eq!(shape.tail, Tail::None, "a tail excludes latency");

        const SOUNDING: usize = 1_000;
        let mut out = [0.0f32; 2];
        let mut last = None;
        for i in 0..SOUNDING + 4 * n {
            let x = if i < SOUNDING { 0.9 } else { 0.0 };
            tick(&mut lim, &[x, x], &mut out);
            if out.iter().any(|&y| y != 0.0) {
                last = Some(i);
            }
        }
        assert_eq!(
            last,
            Some(SOUNDING - 1 + n),
            "the last input frame leaves exactly the latency later, and nothing after"
        );
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

    /// The shapes declare ceiling, threshold and release (and the brickwall
    /// its ceiling), and never change the arity: a modulatable limiter is as
    /// wide as it was built. (Doc 014 §12.6 widened the list: a route onto
    /// any of these was refused once the frame-rate driver went.)
    ///
    /// Mutation (run): swap `LIMITER_PARAMS`' order → the first assertion
    /// fails. Mutation (run): `param_base(2)` answering the threshold
    /// cell → fails.
    #[test]
    fn the_shapes_declare_ceiling_threshold_and_release() {
        let l = LimiterNode::with_channels(ChannelLayout::from(6u16), -6.0, -0.3);
        assert_eq!(
            l.shape().params.as_slice(),
            &[UnitParam::Ceiling, UnitParam::Threshold, UnitParam::Release][..]
        );
        assert_eq!(
            (l.shape().audio_in.count(), l.shape().audio_out.count()),
            (6, 6)
        );
        assert_eq!(l.param_base(0), Some(-0.3), "the ceiling's base");
        assert_eq!(l.param_base(1), Some(-6.0), "the threshold's base");
        assert_eq!(
            l.param_base(2),
            Some(l.release_time().load(std::sync::atomic::Ordering::Acquire) as f32),
            "the release's base"
        );
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

    /// A NaN fed on the ceiling port clamps at the cell's ceiling rather
    /// than panicking in `f32::clamp`, and the next finite frame clamps at
    /// its own.
    ///
    /// Mutation (run): use the fed frame's linear ceiling unchecked (the old
    /// `db_to_amplitude(Db(db)).get()`) → `f32::clamp` panics on the NaN
    /// bound → fails.
    #[test]
    fn brickwall_a_fed_nan_ceiling_clamps_at_the_cell() {
        let mut bw = prepared_at(BrickwallLimiterNode::new(-6.0), RATE_44K);
        let cell = db_to_amplitude(-6.0).get();
        let mut out = [0.0f32; 2];
        tick_fed(&mut bw, &[1.0, -1.0], &[Some(f32::NAN)], &mut out);
        assert_eq!(out, [cell, -cell]);
        tick_fed(&mut bw, &[1.0, -1.0], &[Some(-12.0)], &mut out);
        let fed = db_to_amplitude(-12.0).get();
        assert_eq!(out, [fed, -fed]);
    }

    /// A NaN written to the ceiling cell keeps the ceiling the node last
    /// clamped at, and a later finite write takes effect; a NaN at
    /// construction starts at 0 dB.
    ///
    /// Mutations (run): re-derive the cell's ceiling unchecked
    /// (`self.ceiling_linear = db_to_amplitude(..).get()`) → the clamp
    /// panics → fails; build with `db_to_amplitude(ceiling_db).get()` in
    /// `with_channels` → the NaN-built node panics → fails.
    #[test]
    fn brickwall_a_nan_cell_keeps_the_last_ceiling() {
        let mut bw = prepared_at(BrickwallLimiterNode::new(-6.0), RATE_44K);
        let set = bw.param_set();
        let mut out = [0.0f32; 2];
        assert!(set.set(UnitParam::Ceiling, f32::NAN));
        tick(&mut bw, &[1.0, -1.0], &mut out);
        let held = db_to_amplitude(-6.0).get();
        assert_eq!(out, [held, -held]);
        assert!(set.set(UnitParam::Ceiling, -12.0));
        tick(&mut bw, &[1.0, -1.0], &mut out);
        let next = db_to_amplitude(-12.0).get();
        assert!((out[0] - next).abs() < 1e-6, "{}", out[0]);

        let mut born_nan = prepared_at(BrickwallLimiterNode::new(f32::NAN), RATE_44K);
        tick(&mut born_nan, &[2.0, -2.0], &mut out);
        assert_eq!(out, [1.0, -1.0], "a NaN-built brickwall clamps at 0 dB");
        born_nan.set_ceiling(f32::NAN);
        tick(&mut born_nan, &[2.0, -2.0], &mut out);
        assert_eq!(out, [1.0, -1.0], "set_ceiling(NaN) keeps the ceiling");
    }

    // ── Width-native (N-channel) ─────────────────────────────────────────────

    /// A stereo limiter fed on one channel only limits that channel and
    /// leaves the other silent: the graph hands an unconnected input
    /// silence, which the linked detector reads as such (a graph node is
    /// always handed every input it declares).
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

    /// A block of every channel clips each sample against its own frame's
    /// fed ceiling: frame `i` is fed `-i/4` dB and every channel holds
    /// `±2`, so each output is exactly that frame's linear ceiling, signed,
    /// and a quiet sample passes through.
    ///
    /// Mutation (run): skip channel 0's final clip → channel 0 outputs the
    /// bare ceiling, unsigned → fails; read the ceiling of frame `i - 1` for
    /// the other channels → fails; unfed, read `inputs.get(0)` for every
    /// channel → channel 1's sign is wrong → fails.
    #[test]
    fn brickwall_clips_each_frame_at_its_own_ceiling() {
        use crate::test_support::RATE;
        use tutti_graph::contract::drive;
        const N: usize = 64;
        let fed: Vec<f32> = (0..N).map(|i| -(i as f32) / 4.0).collect();
        let ins: Vec<Vec<f32>> = (0..3)
            .map(|c| {
                (0..N)
                    .map(|i| match (i + c) % 3 {
                        0 => 2.0,
                        1 => -2.0,
                        _ => 0.0625,
                    })
                    .collect()
            })
            .collect();
        let refs: Vec<&[f32]> = ins.iter().map(|c| &c[..]).collect();
        let mut bw = BrickwallLimiterNode::with_channels(ChannelLayout::from(3u16), 0.0);
        let out = drive(&mut bw, RATE, &refs, &[Some(&fed)]);
        for (c, (o, x)) in out.iter().zip(&ins).enumerate() {
            for (i, (&y, &x)) in o.iter().zip(x).enumerate() {
                let ceiling = Db(fed[i]).to_amplitude().get();
                let want = if x.abs() > ceiling {
                    ceiling.copysign(x)
                } else {
                    x
                };
                assert_eq!(y, want, "fed: channel {c}, frame {i}");
            }
        }
        // Unfed: one ceiling, 0 dB, for the whole block.
        let out = drive(&mut bw, RATE, &refs, &[None]);
        for (c, (o, x)) in out.iter().zip(&ins).enumerate() {
            for (i, (&y, &x)) in o.iter().zip(x).enumerate() {
                let want = if x.abs() > 1.0 { x.signum() } else { x };
                assert_eq!(y, want, "unfed: channel {c}, frame {i}");
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

    /// The gain-reduction meter reports the block's last frame, and a block
    /// longer than the lane runs in lane-sized chunks with the same result.
    ///
    /// The input steps from 0.2 to 0.8 at frame 600 of 1024, so the window
    /// minimum differs between the block's first and last frame. The output
    /// is the lookahead-delayed input times the window-min gain, so the last
    /// frame's gain is `out / delayed input`, and the meter is that gain in
    /// dB.
    ///
    /// Mutation (run): report `gains.first()` in `process` -> the meter is the
    /// first frame's reduction -> fails. Read each chunk's input from
    /// `[..end - start]` -> a chunk past the first sees the block's start ->
    /// fails with the lane at 100.
    #[test]
    fn the_meter_reports_the_blocks_last_frame_whatever_the_lane() {
        for lane in [None, Some(100)] {
            let mut lim = prepared_at(LimiterNode::new(-12.0, -0.3), RATE_44K);
            if let Some(n) = lane {
                lim.lane.truncate(n);
            }
            let latency = lim.ring.lookahead_samples();
            let input: Vec<f32> = (0..1024).map(|i| if i < 600 { 0.2 } else { 0.8 }).collect();
            let out = drive(&mut lim, RATE_44K, &[&input, &input], &[]).remove(0);
            let gain = out[1023] / input[1023 - latency];
            let expected = -amplitude_to_db(gain).get();
            assert!(expected > 1.0, "the loud half is being reduced: {expected}");
            assert!(
                (lim.gain_reduction_db().get() - expected).abs() < 1e-3,
                "lane {lane:?}: meter {} against the last frame's {expected}",
                lim.gain_reduction_db().get()
            );
        }
    }

    /// Each frame of output is the input delayed by the declared latency,
    /// times the minimum of the envelope over the window that starts at that
    /// delayed sample's own frame and ends at the current one (`latency + 1`
    /// frames).
    ///
    /// The window used to be `latency` frames, ending one frame short of
    /// the delayed sample's own target: by then the envelope had released one
    /// step, and a peak could pass the ceiling. Correcting the window changes
    /// this property, it does not weaken it: the expected gain is still
    /// computed independently and compared at 1e-6.
    ///
    /// The test computes the envelope itself from the documented math: it
    /// starts at 0, a lower target is taken at once (no attack), a higher
    /// one is approached by the one-pole release, and the target is
    /// `compute_limiter_gain` of the frame's peak. The input is quiet, steps
    /// above the threshold at frame 300 and falls back at 700, so the gain
    /// changes at the step, along the window and through the release.
    ///
    /// Mutation (run): in `delay_scaled`, apply each frame's gain one frame
    /// late (`gains.iter().cycle().skip(len - 1)`) -> the gain at the step and
    /// along the release is the previous frame's -> fails, at lane 512 and 100.
    /// Mutation (run): `MonotonicMinDeque::new(n)` in `LookaheadRing::new` (the
    /// old window) -> the frame at the end of the step's release is scaled by
    /// a gain one release step too high -> fails.
    #[test]
    fn each_frame_is_scaled_by_its_own_window_minimum_gain() {
        let release = 0.001;
        let (threshold, ceiling) = (Db(-12.0), Db(-0.3));
        for lane in [None, Some(100)] {
            let mut lim = prepared_at(
                LimiterNode::new(threshold, ceiling).with_release(release),
                RATE_44K,
            );
            if let Some(n) = lane {
                lim.lane.truncate(n);
            }
            let latency = lim.ring.lookahead_samples();
            let input: Vec<f32> = (0..1200)
                .map(|i| if (300..700).contains(&i) { 0.8 } else { 0.05 })
                .collect();
            let out = drive(&mut lim, RATE_44K, &[&input, &input], &[]).remove(0);

            let coeff = Seconds(release).one_pole_coeff(RATE_44K);
            let mut envelope = 0.0f32;
            let envelopes: Vec<f32> = input
                .iter()
                .map(|&x| {
                    let target = compute_limiter_gain(amplitude_to_db(x), threshold, ceiling).get();
                    envelope = if target < envelope {
                        target
                    } else {
                        envelope + coeff * (target - envelope)
                    };
                    envelope
                })
                .collect();
            for (i, &y) in out.iter().enumerate() {
                let delayed = if i < latency { 0.0 } else { input[i - latency] };
                let window = &envelopes[i.saturating_sub(latency)..=i];
                let gain = window.iter().copied().fold(f32::INFINITY, f32::min);
                assert!(
                    (y - delayed * gain).abs() <= 1e-6,
                    "lane {lane:?}, frame {i}: {y} against {delayed} x {gain}"
                );
            }
        }
    }
    /// How close the one-pole envelope gets to a steady target: it stops when
    /// a step rounds away, about `1 / coeff` ulps short (44 ulps at 1 ms).
    const SETTLED: f32 = 2e-5;

    /// A block of `n` frames of deterministic noise in `[-amp, amp]`.
    fn noise(seed: &mut u32, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// Whatever loud noise goes in, the output never exceeds the ceiling:
    /// the gain window includes the delayed sample's own frame, so its gain is
    /// at most `ceiling / |x|`. The bound is exact, with no tolerance: this
    /// noise never rounds a product past the ceiling. Threshold at the
    /// ceiling, so the quiet gain is unity.
    ///
    /// Mutation (run): drop the `/ peak` in `LimiterCurve::gain` (return
    /// `self.over`) -> a peak of 4 comes out at 4 x the ceiling -> fails.
    /// Mutation (run): `MonotonicMinDeque::new(n)` in `LookaheadRing::new` (the
    /// old window) -> the output reaches 1.0016 x the ceiling -> fails.
    #[test]
    fn loud_noise_stays_at_the_ceiling() {
        let ceiling = Db(-6.0);
        let limit = db_to_amplitude(ceiling).get();
        let mut lim = prepared_at(LimiterNode::new(ceiling, ceiling), RATE_44K);
        let latency = lim.ring.lookahead_samples();
        let mut seed = 7;
        let (mut input, mut output) = (Vec::new(), Vec::new());
        for _ in 0..40 {
            let l = noise(&mut seed, 1024, 4.0);
            let r = noise(&mut seed, 1024, 4.0);
            output.extend(drive(&mut lim, RATE_44K, &[&l, &r], &[]).remove(0));
            input.extend(l);
        }
        let mut limited = 0;
        for (i, &y) in output.iter().enumerate().skip(latency) {
            assert!(y.abs() <= limit, "frame {i}: {y} over {limit}");
            limited += usize::from(y.abs() > 0.9 * limit);
        }
        assert!(limited > 1000, "the limiter was exercised: {limited}");
    }

    /// A steady signal under the threshold passes at unity once the lookahead
    /// has filled; one over it settles at `ceiling / peak`.
    ///
    /// Mutation (run): `over: ... * threshold_lin` -> `over: ... ` (drop the
    /// threshold factor in `LimiterCurve::new`) -> the loud gain is off by
    /// the threshold -> fails; return `self.over` for the quiet branch ->
    /// the quiet signal is not unity -> fails.
    #[test]
    fn a_steady_signal_passes_under_and_settles_at_ceiling_over_peak() {
        let ceiling = Db(-6.0);
        let limit = db_to_amplitude(ceiling).get();
        for (level, want) in [(0.25f32, 1.0f32), (1.0, limit), (3.0, limit / 3.0)] {
            let mut lim = prepared_at(
                LimiterNode::new(ceiling, ceiling).with_release(0.001),
                RATE_44K,
            );
            let x = vec![level; 1024];
            let mut out = Vec::new();
            for _ in 0..16 {
                out = drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0);
            }
            let want = level * want;
            for &y in &out[512..] {
                assert!(
                    (y - want).abs() <= SETTLED * want,
                    "level {level}: {y} against {want}"
                );
            }
        }
    }

    /// With the ceiling under the threshold the reduction is still
    /// `ceiling / peak` over the threshold, and the constant
    /// `ceiling / threshold` under it.
    ///
    /// Mutation (run): in `LimiterCurve::new`, build `over` from a gap of
    /// `0.0` -> the loud gain is `threshold / peak` -> fails.
    #[test]
    fn a_ceiling_under_the_threshold_scales_by_the_gap_under_it() {
        let (threshold, ceiling) = (Db(-3.0), Db(-9.0));
        let (t, c) = (
            db_to_amplitude(threshold).get(),
            db_to_amplitude(ceiling).get(),
        );
        for (level, gain) in [(0.1f32, c / t), (2.0, c / 2.0)] {
            let mut lim = prepared_at(
                LimiterNode::new(threshold, ceiling).with_release(0.001),
                RATE_44K,
            );
            let x = vec![level; 1024];
            let mut out = Vec::new();
            for _ in 0..16 {
                out = drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0);
            }
            let want = level * gain;
            assert!(
                (out[1023] - want).abs() <= SETTLED * want,
                "level {level}: {} against {want}",
                out[1023]
            );
        }
    }

    /// The steady-state output level in dB of a constant input at `in_db`.
    fn steady_out_db(threshold: Db, ceiling: Db, in_db: f32) -> f32 {
        let mut lim = prepared_at(
            LimiterNode::new(threshold, ceiling).with_release(0.001),
            RATE_44K,
        );
        let level = db_to_amplitude(Db(in_db)).get();
        let x = vec![level; 1024];
        let mut out = Vec::new();
        for _ in 0..16 {
            out = drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0);
        }
        let ceiling_lin = db_to_amplitude(ceiling).get();
        // The output never exceeds the ceiling (to a rounding step), in
        // every frame, including the release that rises from zero.
        assert!(
            out.iter().all(|y| y.abs() <= ceiling_lin * (1.0 + 1e-6)),
            "threshold {threshold:?}, ceiling {ceiling:?}, in {in_db}: past the ceiling"
        );
        amplitude_to_db(out[1023]).get()
    }

    /// Threshold, ceiling pairs covering a boost, a cut, and no drive.
    const CURVES: [(f32, f32); 5] = [
        (-6.0, -0.3),
        (-12.0, -1.0),
        (-3.0, -9.0),
        (-6.0, -6.0),
        (-20.0, 0.0),
    ];

    /// The threshold drives the input: under the knee the output is the
    /// input plus `ceiling - threshold` dB, and over it the output sits at
    /// the ceiling. This is the curve for a ceiling above the threshold as
    /// well as below; it replaces a curve that boosted quiet signals by the
    /// drive but held loud ones at the threshold (a jump of the drive at the
    /// knee, and a louder input giving a quieter output).
    ///
    /// Mutation (run): `gap.min(0.0)` for `over` in `LimiterCurve::new` -> at
    /// (-6, -0.3) the input -5.9 dB comes out at -6.0 -> fails the settle
    /// assertion.
    /// Mutation (run): `below: 1.0` -> a quiet input comes out unmoved ->
    /// fails the drive assertion.
    #[test]
    fn the_threshold_drives_the_input_and_loud_signals_settle_at_the_ceiling() {
        for (t, c) in CURVES {
            let (threshold, ceiling) = (Db(t), Db(c));
            let mut in_db = -80.0f32;
            while in_db <= 12.0 {
                let out_db = steady_out_db(threshold, ceiling, in_db);
                let want = if in_db <= t { in_db + (c - t) } else { c };
                assert!(
                    (out_db - want).abs() <= 2e-3,
                    "({t}, {c}) in {in_db}: {out_db} dB against {want}"
                );
                in_db += 1.0;
            }
        }
    }

    /// The steady-state transfer curve never decreases and never jumps: a
    /// 1 dB step of input moves the output by between 0 and 1 dB (the
    /// slope is the drive's unity under the knee and zero over it).
    ///
    /// Mutation (run): as above, `.min(0.0)` back on the gap in
    /// `LimiterCurve::new` -> at (-6, -0.3) the output falls 5.7 dB at the
    /// knee -> fails the monotonic assertion.
    /// Mutation (run): `below: 1.0` -> the step into the knee is not the
    /// drive's -> fails.
    #[test]
    fn the_transfer_curve_is_monotonic_and_continuous() {
        for (t, c) in CURVES {
            let mut prev: Option<f32> = None;
            let mut in_db = -80.0f32;
            while in_db <= 12.0 {
                let out_db = steady_out_db(Db(t), Db(c), in_db);
                if let Some(prev) = prev {
                    let step = out_db - prev;
                    assert!(step >= -2e-3, "({t}, {c}) at {in_db}: fell by {}", -step);
                    assert!(step <= 1.0 + 2e-3, "({t}, {c}) at {in_db}: jumped {step}");
                }
                prev = Some(out_db);
                in_db += 1.0;
            }
        }
    }

    /// A NaN sample passes through as a NaN and does not poison the state:
    /// the peak detector ignores it (`max`), so the gain is untouched and the
    /// frames after it limit as before.
    ///
    /// Two layers keep a NaN out: the peak fold's `max` ignores it, and
    /// `LimiterCurve::gain` sends one to the quiet gain. Either alone is
    /// absorbed (the envelope attacks straight back), so the mutation breaks
    /// both.
    ///
    /// Mutation (run): in the peak fold, `*p = if x.is_nan() { x } else {
    /// p.max(x.abs()) }`, and in `LimiterCurve::gain`, `if !(peak <=
    /// self.threshold)` -> a NaN gain enters the envelope and the minimum, and
    /// every later frame is NaN -> fails.
    #[test]
    fn a_nan_input_does_not_poison_the_gain() {
        let ceiling = Db(-6.0);
        let limit = db_to_amplitude(ceiling).get();
        let mut lim = prepared_at(
            LimiterNode::new(ceiling, ceiling).with_release(0.001),
            RATE_44K,
        );
        let latency = lim.ring.lookahead_samples();
        let mut x = vec![1.0f32; 1024];
        x[10] = f32::NAN;
        let out = drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0);
        assert!(
            out[10 + latency].is_nan(),
            "the NaN is delayed, not dropped"
        );
        let x = vec![1.0f32; 1024];
        let out = drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0);
        for (i, &y) in out.iter().enumerate().skip(latency + 1) {
            assert!(
                (y - limit).abs() <= SETTLED * limit,
                "frame {i}: {y} against {limit}"
            );
        }
        assert!(lim.envelope.is_finite());
    }

    /// A NaN fed to the peak fold yields a non-NaN peak. Separate from the
    /// `LimiterCurve::gain` guard, which has its own test in `utils.rs`.
    ///
    /// Mutation (run): `if x.is_nan() { x } else { peak.max(x.abs()) }` in
    /// `fold_peak` -> the peak is NaN -> fails.
    #[test]
    fn a_nan_fed_to_the_peak_fold_is_ignored() {
        assert_eq!(fold_peak(0.5, f32::NAN), 0.5);
        assert_eq!(fold_peak(0.0, f32::NAN), 0.0);
        assert_eq!(fold_peak(0.5, -0.75), 0.75);
    }

    /// Silence, a burst, silence, at thresholds so low that `10^((ceiling -
    /// threshold)/20)` overflows `f32` without the clamp in
    /// `LimiterCurve::new`: `0 * inf` would put a NaN on every silent sample.
    ///
    /// Mutation (run): in `LimiterCurve::new`, `gap = ceiling - threshold`
    /// (drop the `.min(MAX_GAP_DB)`) -> `below` is inf at -inf and -800 dB ->
    /// NaN output -> fails.
    #[test]
    fn a_very_low_threshold_never_produces_a_nan() {
        for threshold in [f32::NEG_INFINITY, -800.0, -150.0] {
            let mut lim = prepared_at(
                LimiterNode::new(Db(threshold), Db(-6.0)).with_release(0.001),
                RATE_44K,
            );
            let mut x = vec![0.0f32; 1024];
            x[300..400].fill(2.0);
            x[400..500].fill(-0.001);
            let mut output = Vec::new();
            for _ in 0..4 {
                output.extend(drive(&mut lim, RATE_44K, &[&x, &x], &[]).remove(0));
            }
            for (i, &y) in output.iter().enumerate() {
                assert!(y.is_finite(), "threshold {threshold}: frame {i} is {y}");
            }
        }
    }
}
