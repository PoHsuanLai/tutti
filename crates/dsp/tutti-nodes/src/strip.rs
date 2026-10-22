//! The mixer strip — volume, stereo balance, mute — as one addressable unit.
//!
//! A mixer bus applies three things to a summed signal: a fader, a balance, and
//! a mute. [`BusStripNode`] is all three in one node, addressed by
//! [`UnitParam`] through the [`ParamSet`] it is inserted with, so a host sets
//! it with the same generic machinery it uses for a filter cutoff and needs no
//! downcast.
//!
//! Every scalar lives in a cell the node's [`ParamSet`] addresses by
//! [`UnitParam`] — the same shape every other node in this crate uses — so a
//! host has one param path to the strip, not a second set of handles beside
//! it.
//!
//! # Balance, not panning
//!
//! A panner is **mono**-to-stereo: one signal placed between two speakers. A
//! mixer strip is stereo-to-stereo — it *rebalances* a signal that is already
//! stereo, and must leave it untouched at centre. See
//! [`BusStripNode::balance_gains`].
//!
//! # Every control change is a ramp, never a step
//!
//! Volume, balance and mute are read **once per block** and the gain they imply
//! is reached by a linear ramp across that block, starting from the gain the
//! previous block ended on. A step in gain is a step in the waveform — an
//! audible click on a mute, and zipper noise on a dragged fader — and the ramp
//! is what removes it. The ramp ends exactly on the new gain at the block's last
//! sample, so a mute is silent from the next block on. See
//! `BusStripNode::render`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tutti_core::{Amplitude, AtomicF32, ChannelLayout, Pan, Param, Tail, UnitParam};
use tutti_graph::{
    Cx, Inputs, IntoNode, Io, Node, NodeParts, Outputs, ParamNode, ParamSet, Prepare, Shape, Status,
};

/// The params a [`BusStripNode`] lets the graph modulate, in port order.
pub const STRIP_PARAMS: [UnitParam; 2] = [UnitParam::Volume, UnitParam::Pan];

/// A mixer strip: volume, stereo balance and mute over `channels` audio ports.
///
/// Ports are `channels` audio inputs → `channels` outputs. Volume and pan
/// are modulatable by the graph, in that port order
/// ([`STRIP_PARAMS`]): a per-frame value on the param port
/// ([`Io::param`](tutti_graph::Io::param)) overrides its cell per sample.
/// There is deliberately no modulatable mute: a per-sample boolean is a
/// gate, not a mute, and gating is [`crate::GateNode`]'s job.
///
/// Params, by address through its [`ParamSet`]: [`UnitParam::Volume`] (linear
/// amplitude), [`UnitParam::Pan`] (`-1..1`) and [`UnitParam::Mute`] (`>= 0.5`
/// is muted). The set refuses anything else, which is what lets a host push
/// params without knowing the node type.
///
/// Every control is read **once per block**; no cell is loaded per sample.
///
/// `Clone` shares the three cells: it is the template
/// [`tutti_graph::param_parts`] forks from ([`ParamNode::fork_fresh`] is the
/// copy that shares nothing).
#[derive(Clone)]
pub struct BusStripNode {
    volume: Param<Amplitude>,
    pan: Param<Pan>,
    /// The mute, as [`UnitParam::Mute`] encodes it: `>= 0.5` is muted. A bare
    /// cell rather than a [`Param`] — deliberately *not* given a unit newtype,
    /// since per the units rule a new type needs a distinct range or algebra,
    /// and a bool-as-float has neither — so the [`ParamSet`] addresses it as
    /// it does volume and pan, and a mute set by address lands on the next
    /// block.
    muted: Arc<AtomicF32>,
    /// The width this strip runs at — its declared audio layout (as many
    /// inputs as outputs).
    ///
    /// Balance is only meaningful on a stereo pair, so it applies to channels 0
    /// and 1 and leaves any others at unity — a 5.1 strip fades and mutes whole
    /// but does not "balance" its surrounds, which have no left/right axis.
    ///
    /// Stored as the layout alone, with the count derived at use via
    /// [`channels`](Self::channels). There is no cached `usize` beside it: the
    /// count is a loop *bound*, so it is loop-invariant and hoisted — `count()`
    /// is a `const fn` over a four-variant `Copy` enum, and the match lands in
    /// the prologue rather than the inner loop. A second field would only be a
    /// way for the two to disagree.
    layout: ChannelLayout,
    /// Which params the previous block was modulated on (bit `k`). A change is not
    /// ramped by the strip: the graph declicks the fed value itself, and
    /// ramping the control gain across the same block would apply the gain
    /// twice for its length.
    last_fed: u8,
    /// The control-driven gains the previous block ended on — where this
    /// block's ramp starts. `None` before the first block and after
    /// [`reset`](Node::reset): with no previous output there is nothing to
    /// be continuous with, so the first block starts at its target.
    ///
    /// Per-node state, not shared with clones: it describes what *this* node
    /// last emitted.
    ramp_from: Option<StripGains>,
}

/// The gain on each kind of channel: the balanced pair, and every channel past
/// it (which takes volume and mute but no balance).
///
/// The unit a ramp interpolates. Ramping the *gains* rather than the three
/// controls is what makes the ramp linear in the signal: `balance × volume ×
/// live` with each factor ramped separately would be a cubic across the block.
#[derive(Clone, Copy, Debug, PartialEq)]
struct StripGains {
    left: Amplitude,
    right: Amplitude,
    rest: Amplitude,
}

impl StripGains {
    /// The gain for channel `c`.
    #[inline]
    fn channel(self, c: usize) -> Amplitude {
        match c {
            0 => self.left,
            1 => self.right,
            _ => self.rest,
        }
    }
}

impl BusStripNode {
    /// A stereo strip at unity volume, centred, unmuted.
    pub fn new() -> Self {
        Self::with_channels(ChannelLayout::STEREO)
    }

    /// A strip at the given width, unity volume, centred, unmuted.
    /// `with_channels(ChannelLayout::STEREO)` is identical to [`new`](Self::new).
    ///
    /// A zero-wide strip is meaningless — there would be nothing to fade — so an
    /// empty layout is clamped to mono, matching
    /// [`ChannelSumNode::new`](crate::ChannelSumNode::new).
    pub fn with_channels(channels: impl Into<ChannelLayout>) -> Self {
        let layout = channels.into();
        Self {
            volume: Param::new(Amplitude::UNITY),
            pan: Param::new(Pan::CENTER),
            muted: Arc::new(AtomicF32::new(0.0)),
            layout: ChannelLayout::from(layout.count().max(1)),
            last_fed: 0,
            ramp_from: None,
        }
    }

    /// The width this strip runs at, as the engine's channel vocabulary.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// The audio channel count (its output count).
    #[inline]
    pub fn channels(&self) -> usize {
        self.layout.count() as usize
    }

    /// Atomic handle for the UI / automation to share the volume cell.
    ///
    /// Untyped by the shape of the contract, not by omission: this is the
    /// cell the strip's `ParamSet` addresses as `Volume`, and a control-rate
    /// route's target speaks `Arc<AtomicF32>`. To *read* the value, use
    /// [`volume_value`](Self::volume_value), which keeps the unit.
    pub fn volume(&self) -> Arc<tutti_core::AtomicF32> {
        self.volume.as_atomic()
    }

    /// Atomic handle for the UI / automation to share the pan cell. See
    /// [`volume`](Self::volume) on why this one is untyped; the typed read is
    /// [`pan_value`](Self::pan_value).
    pub fn pan(&self) -> Arc<tutti_core::AtomicF32> {
        self.pan.as_atomic()
    }

    /// The current fader position.
    pub fn volume_value(&self) -> Amplitude {
        self.volume.load()
    }

    /// The current balance position.
    pub fn pan_value(&self) -> Pan {
        self.pan.load()
    }

    /// Sets the fader position as a linear [`Amplitude`], unclamped.
    ///
    /// Linear, not [`Db`](tutti_core::Db): `1.0` is unity, `0.0` silent, and
    /// values above 1.0 amplify. Read once per block and ramped to across it.
    pub fn set_volume(&self, volume: impl Into<Amplitude>) {
        self.volume.store(volume.into());
    }

    /// Sets the balance, clamped to `-1..1` — an out-of-range value would
    /// otherwise amplify one channel past unity rather than saturating.
    pub fn set_pan(&self, pan: impl Into<Pan>) {
        self.pan.store(Pan(pan.into().get().clamp(-1.0, 1.0)));
    }

    /// Mutes or unmutes the strip.
    ///
    /// A gate on the output, applied after volume and pan — muting does not
    /// disturb the fader position, so unmuting restores the previous level.
    /// Shared across clones, so the write reaches a live node.
    ///
    /// Not a hard gate: the next block ramps to (or from) silence across its
    /// length, so the output is silent from the end of that block on. A step to
    /// zero is an audible click on anything but silence.
    ///
    /// The same cell [`UnitParam::Mute`] addresses through the [`ParamSet`].
    pub fn set_muted(&self, muted: bool) {
        self.muted
            .store(if muted { 1.0 } else { 0.0 }, Ordering::Release);
    }

    /// Whether the strip is currently muted and emitting silence.
    pub fn is_muted(&self) -> bool {
        Self::is_mute(self.muted.load(Ordering::Acquire))
    }

    /// [`UnitParam::Mute`]'s encoding: `>= 0.5` is muted (so a NaN is not).
    #[inline]
    fn is_mute(v: f32) -> bool {
        v >= 0.5
    }

    /// Per-channel gains for a stereo **balance** at position `pan`.
    ///
    /// Attenuates the channel you pan away from and leaves the other at unity:
    /// `(1, 1)` at centre, `(1, 0)` hard left, `(0, 1)` hard right. Unity at
    /// centre is the property that matters — a strip must be transparent until
    /// somebody moves it, and this is what
    /// [`unity_at_defaults`](self::tests::unity_at_defaults) pins.
    ///
    /// This is **balance, not panning**. A panner *places* a mono signal, so it spreads
    /// one input across two outputs with an equal-power (`cos`/`sin`) law that
    /// reads `-3 dB` on each side at centre. Applying that here would attenuate
    /// an already-stereo signal by 3 dB just for existing. A balance instead
    /// *rebalances* an existing pair, so centre must be exactly unity.
    /// Typed in and out: the argument is a *position* and the results are
    /// *gains*, which is the confusion worth preventing here — both are `-1..1`-
    /// ish floats, and multiplying a sample by a pan position instead of by its
    /// derived gain is silent.
    #[inline]
    fn balance_gains(pan: Pan) -> (Amplitude, Amplitude) {
        (
            Self::balance_side(pan, false),
            Self::balance_side(pan, true),
        )
    }

    /// One side of [`balance_gains`](Self::balance_gains): the right
    /// channel's gain if `right`, else the left's.
    #[inline(always)]
    fn balance_side(pan: Pan, right: bool) -> Amplitude {
        let p = pan.get().clamp(-1.0, 1.0);
        Amplitude(if right { 1.0 + p } else { 1.0 - p }.min(1.0))
    }

    /// Gains for a fader at `volume`, balance at `pan`, and `live` (the mute as
    /// a factor).
    ///
    /// Mute is folded in as a **factor rather than a branch**, so the per-sample
    /// cost does not depend on the mute state and a muted strip cannot take a
    /// cheaper path that diverges from the live one — and so a mute can be
    /// *ramped*, which a branch cannot be.
    #[inline]
    fn gains_for(volume: Amplitude, pan: Pan, live: f32) -> StripGains {
        // `Amplitude * f32` is the scaling the unit grants; cascading two gain
        // stages multiplies them, which is exactly what balance × fader is.
        let fader = volume * live;
        let (l, r) = Self::balance_gains(pan);
        StripGains {
            left: l * fader.get(),
            right: r * fader.get(),
            rest: fader,
        }
    }

    /// `1.0` when the strip is passing audio, `0.0` when muted — the mute as a
    /// multiplicand rather than a branch.
    #[inline]
    fn live_factor(&self) -> f32 {
        !Self::is_mute(self.muted.load(Ordering::Relaxed)) as u8 as f32
    }

    /// The gains the **control cells** ask for — the ramp's target.
    ///
    /// A control the graph modulates (`fed`: volume, pan) contributes unity
    /// here, and its value comes from [`fed_gain`](Self::fed_gain) per
    /// sample instead. Mute is never modulated, so it is always here.
    ///
    /// Reads three atomics; called once per block, never per sample.
    #[inline]
    fn control_gains(&self, fed: (bool, bool)) -> StripGains {
        let volume = match fed.0 {
            true => Amplitude::UNITY,
            false => self.volume.load(),
        };
        let pan = match fed.1 {
            true => Pan::CENTER,
            false => self.pan.load(),
        };
        Self::gains_for(volume, pan, self.live_factor())
    }

    /// Render one block: the one gain path.
    ///
    /// Reads the control cells **once**, then ramps linearly from the gains the
    /// previous call ended on to the ones just read, landing exactly on them at
    /// the last frame. A one-frame block's ramp is therefore a step.
    ///
    /// # The ramp is one block long, so its length is the caller's block size
    ///
    /// At the engine's 64-frame chunks that is ~1.3 ms at 48 kHz: long enough to
    /// turn a mute's step into a slope with no broadband click, short enough
    /// that the mute is silent from the end of that block on. A caller that
    /// renders one frame at a time gets a step.
    ///
    /// # Channel-major, so it vectorises along time
    ///
    /// Each channel is one pass over its two slices with a gain that is a
    /// function of the frame alone: no per-sample branch, channel lookup or
    /// slice fetch, so the compiler runs it in SIMD lanes. Every sample takes
    /// the same arithmetic in the same order as one gain per frame applied to
    /// every channel would, so the order of the loops changes no bit.
    #[inline]
    fn render(
        &mut self,
        inputs: &Inputs<'_>,
        outputs: &mut Outputs<'_, '_>,
        size: usize,
        volume: Option<&[f32]>,
        pan: Option<&[f32]>,
    ) {
        let fed = (volume.is_some(), pan.is_some());
        let fed_bits = u8::from(fed.0) | u8::from(fed.1) << 1;
        let target = self.control_gains(fed);
        let from = self.ramp_from.replace(target).unwrap_or(target);
        // A param that started or stopped being fed is not ramped here: the
        // graph declicks the fed value, and ramping the control gain from its
        // old share too would apply the gain twice across the block.
        let from = if fed_bits == self.last_fed {
            from
        } else {
            target
        };
        self.last_fed = fed_bits;
        // Per-block facts; each channel picks its loop once. The steady case skips the
        // interpolation outright, which also keeps its output bit-identical
        // to the unramped arithmetic.
        let ramping = from != target;
        let (volume, pan) = (volume.map(|v| &v[..size]), pan.map(|p| &p[..size]));

        for c in 0..self.channels() {
            // The one place a gain meets a sample. A sample is a signal
            // value, not a gain, so the unit comes off the gain here rather
            // than the sample being wrapped.
            let (x, y) = (&inputs.get(c)[..size], &mut outputs.get(c)[..size]);
            let (start, end) = (from.channel(c), target.channel(c));
            // `(i + 1) / size`, not `(i + 1) * (1 / size)`: the division is
            // exact at the last frame (`size / size == 1`), so the ramp lands
            // on the target rather than an ulp beside it. Through `i32`, which
            // has a SIMD conversion; it rounds as `usize` would.
            debug_assert!(i32::try_from(size).is_ok());
            let n = size as i32 as f32;
            let ramp = |i: usize| lerp(start, end, (i + 1) as i32 as f32 / n).get();
            let steady = end.get();
            match (ramping, volume.is_some() || pan.is_some()) {
                (false, false) => scale(x, y, |_| steady),
                (true, false) => scale(x, y, ramp),
                (false, true) => Self::scale_fed(x, y, |_| steady, volume, pan, c),
                (true, true) => Self::scale_fed(x, y, ramp, volume, pan, c),
            }
        }
    }

    /// [`scale`] by `base(i)` times channel `c`'s [`fed_gain`](Self::fed_gain),
    /// with `c` a constant in each loop so only its side is computed.
    #[inline(always)]
    fn scale_fed(
        x: &[f32],
        y: &mut [f32],
        base: impl Fn(usize) -> f32,
        volume: Option<&[f32]>,
        pan: Option<&[f32]>,
        c: usize,
    ) {
        match c {
            0 => scale(x, y, |i| base(i) * Self::fed_gain(volume, pan, i, 0)),
            1 => scale(x, y, |i| base(i) * Self::fed_gain(volume, pan, i, 1)),
            _ => scale(x, y, |i| base(i) * Self::fed_gain(volume, pan, i, 2)),
        }
    }

    /// Channel `c`'s gain from the fed params at frame `i`, unity for one
    /// that is not fed: [`gains_for`](Self::gains_for) with the mute factor
    /// at 1, computing only what channel `c` takes (past the pair, the
    /// fader alone).
    ///
    /// The param port carries raw values, so this is the boundary where an
    /// untyped float becomes a typed quantity again. Not ramped: a fed value
    /// is already a signal, and the graph that feeds it owns its continuity.
    #[inline(always)]
    fn fed_gain(volume: Option<&[f32]>, pan: Option<&[f32]>, i: usize, c: usize) -> f32 {
        let fader = volume.map_or(Amplitude::UNITY, |v| Amplitude(v[i])) * 1.0;
        if c >= 2 {
            return fader.get();
        }
        let pan = pan.map_or(Pan::CENTER, |p| Pan(p[i]));
        (Self::balance_side(pan, c == 1) * fader.get()).get()
    }
}

/// `from` at `t = 0`, `to` at `t = 1` — and *exactly* `to` there.
///
/// Written as `from·(1−t) + to·t` rather than `from + (to−from)·t` for that
/// endpoint: the second form rounds, so a ramp to silence could end a few
/// ulps above zero and a muted strip would leak. Not an `Amplitude` operator
/// because `Amplitude` deliberately has no `Add` (cascading gains multiply);
/// an interpolation between two gains is a different operation, and this is
/// its one home.
#[inline(always)]
fn lerp(from: Amplitude, to: Amplitude, t: f32) -> Amplitude {
    Amplitude(from.get() * (1.0 - t) + to.get() * t)
}

/// `y = x · gain(frame)`, over one channel's block.
///
/// `#[inline(always)]` so each caller's `gain` is compiled into the loop and
/// the loop vectorises with it.
#[inline(always)]
fn scale(x: &[f32], y: &mut [f32], gain: impl Fn(usize) -> f32) {
    for (i, (y, &x)) in y.iter_mut().zip(x).enumerate() {
        *y = x * gain(i);
    }
}

impl Default for BusStripNode {
    fn default() -> Self {
        Self::new()
    }
}

impl Node for BusStripNode {
    /// `channels` in and out, volume then pan modulatable
    /// ([`STRIP_PARAMS`]). Volume, pan and mute are per-frame gains, so it
    /// stops with its input.
    fn shape(&self) -> Shape {
        Shape::audio(self.layout, self.layout)
            // Measured (doc 013, "The cost model and coarsening"): 0.06 ns
            // per frame and channel steady, 0.4 ramping, 0.6-1.0 with the
            // volume fed per frame; under the `Light`/`Moderate` boundary
            // (about 2.8), where the frame-major kernel it replaced measured
            // 2.5-4.7 on the same box.
            .with_cost(tutti_graph::Cost::Light)
            .with_tail(Tail::None)
            .with_params(&STRIP_PARAMS)
    }

    /// Nothing is rate-dependent: the ramp is one block long, whatever the
    /// rate.
    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let (volume, pan) = (io.param(0).frames(), io.param(1).frames());
        let (inputs, mut outputs) = io.split();
        self.render(&inputs, &mut outputs, size, volume, pan);
        Status::Modified
    }

    /// Forget the previous block's gains, so the next block starts at its
    /// target rather than ramping from a signal that is no longer playing.
    fn reset(&mut self) {
        self.ramp_from = None;
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.volume.load().get()),
            1 => Some(self.pan.load().get()),
            _ => None,
        }
    }
}

impl ParamNode for BusStripNode {
    /// Volume, pan and mute (`>= 0.5` is muted).
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Volume, self.volume())
            .param(UnitParam::Pan, self.pan())
            .param(UnitParam::Mute, Arc::clone(&self.muted))
            .build()
    }

    /// A clone with its three cells detached (at their values now), so a
    /// fork renders the strip as it was set, not a fader ride or a mute made
    /// while it runs; its ramp history cleared.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.volume.detach();
        fork.pan.detach();
        fork.muted = Arc::new(AtomicF32::new(self.muted.load(Ordering::Acquire)));
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for BusStripNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{tick, RATE};
    use tutti_graph::contract::{assert_param_fork, drive};

    /// The strip measured as a `Light` node, at every width.
    ///
    /// Mutation (run): back to `Cost::Moderate` → fails.
    #[test]
    fn the_strip_is_light() {
        for layout in [ChannelLayout::MONO, ChannelLayout::STEREO] {
            let strip = BusStripNode::with_channels(layout);
            assert_eq!(strip.shape().cost, tutti_graph::Cost::Light);
        }
    }

    /// Set a param the way a host does — by address, through the node's
    /// `ParamSet` — rather than by calling the setter, so the test covers the
    /// path `AudioParam<U, P>` uses. `false` if the strip refused it.
    fn set_param(strip: &BusStripNode, param: UnitParam, value: f32) -> bool {
        strip.param_set().set(param, value)
    }

    fn tick2(strip: &mut BusStripNode, l: f32, r: f32) -> (f32, f32) {
        let mut out = [0.0f32; 2];
        tick(strip, &[l, r], &mut out);
        (out[0], out[1])
    }

    /// A fork starts from the values last set through the strip's
    /// `ParamSet` — the mute included — and shares no cell with it.
    ///
    /// Mutation (run): keep `fork.muted` shared in `fork_fresh` → "a live
    /// write reached the fork" for `Mute` → fails.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(BusStripNode::new());
    }

    /// A mute set by address lands on the cell the strip reads, and renders
    /// silence from the next block on.
    ///
    /// Mutation (run): leave `Mute` out of `param_set` → the set refuses it
    /// → fails.
    #[test]
    fn a_mute_set_by_address_reaches_the_render() {
        let mut s = BusStripNode::new();
        assert!(set_param(&s, UnitParam::Mute, 1.0));
        assert!(s.is_muted());
        assert_eq!(tick2(&mut s, 1.0, 1.0), (0.0, 0.0));
    }

    /// A strip must be transparent until somebody moves it. An equal-power
    /// pan law would attenuate a centred signal by 3 dB.
    #[test]
    fn unity_at_defaults() {
        let mut s = BusStripNode::new();
        assert_eq!(tick2(&mut s, 0.5, -0.25), (0.5, -0.25));
    }

    #[test]
    fn volume_scales_both_channels() {
        let mut s = BusStripNode::new();
        s.set_volume(Amplitude(0.5));
        assert_eq!(tick2(&mut s, 1.0, 1.0), (0.5, 0.5));
    }

    #[test]
    fn balance_attenuates_the_far_channel_only() {
        let mut s = BusStripNode::new();
        s.set_pan(Pan(-1.0)); // hard left
        assert_eq!(tick2(&mut s, 1.0, 1.0), (1.0, 0.0));
        s.set_pan(Pan(1.0)); // hard right
        assert_eq!(tick2(&mut s, 1.0, 1.0), (0.0, 1.0));
        // Halfway: the near channel stays at unity, the far one is halved.
        s.set_pan(Pan(-0.5));
        let (l, r) = tick2(&mut s, 1.0, 1.0);
        assert!((l - 1.0).abs() < 1e-6);
        assert!((r - 0.5).abs() < 1e-6);
    }

    /// Out-of-range balance saturates rather than amplifying: without the clamp,
    /// `pan = -2` would give the left channel a gain of 3.
    #[test]
    fn balance_saturates_out_of_range() {
        let mut s = BusStripNode::new();
        s.set_pan(Pan(-2.0));
        assert_eq!(tick2(&mut s, 1.0, 1.0), (1.0, 0.0));
        s.set_pan(Pan(2.0));
        assert_eq!(tick2(&mut s, 1.0, 1.0), (0.0, 1.0));
    }

    /// The reversibility half is the point: a mute implemented as a destructive
    /// write to the volume cell would pass the silencing assertion alone and then
    /// come back at the wrong level.
    #[test]
    fn mute_silences_and_is_reversible() {
        let mut s = BusStripNode::new();
        s.set_volume(Amplitude(0.75));
        s.set_muted(true);
        assert_eq!(tick2(&mut s, 1.0, 1.0), (0.0, 0.0));
        s.set_muted(false);
        assert_eq!(tick2(&mut s, 1.0, 1.0), (0.75, 0.75));
    }

    #[test]
    fn set_dispatches_each_unit_param() {
        let s = BusStripNode::new();

        assert!(set_param(&s, UnitParam::Volume, 0.25));
        assert_eq!(s.volume_value(), Amplitude(0.25));

        assert!(set_param(&s, UnitParam::Pan, -1.0));
        assert_eq!(s.pan_value(), Pan(-1.0));

        assert!(set_param(&s, UnitParam::Mute, 1.0));
        assert!(s.is_muted());
        assert!(set_param(&s, UnitParam::Mute, 0.0));
        assert!(!s.is_muted());
    }

    /// A strip refuses params it does not own, and they change nothing — the
    /// property that lets the generic reconciler push any param without
    /// dispatching on node type.
    #[test]
    fn ignores_params_it_does_not_own() {
        let mut s = BusStripNode::new();
        assert!(!set_param(&s, UnitParam::Cutoff, 8000.0));
        assert!(!set_param(&s, UnitParam::Drive, 4.0));
        assert_eq!(tick2(&mut s, 1.0, 1.0), (1.0, 1.0));
    }

    /// The shape declares volume then pan, and never changes the arity.
    ///
    /// Mutation (run): swap `STRIP_PARAMS`' order → the first assertion
    /// fails, and `a_fed_volume_overrides_the_atomic` reads the pan.
    #[test]
    fn the_shape_declares_volume_then_pan() {
        let s = BusStripNode::with_channels(ChannelLayout::from(6u16));
        assert_eq!(
            s.shape().params.as_slice(),
            &[UnitParam::Volume, UnitParam::Pan][..]
        );
        assert_eq!(
            (s.shape().audio_in.count(), s.shape().audio_out.count()),
            (6, 6)
        );
        s.set_volume(Amplitude(0.5));
        assert_eq!(s.param_base(0), Some(0.5), "volume's base is its control");
        assert_eq!(s.param_base(1), Some(0.0), "pan's base is centre");
    }

    /// A fed volume overrides the atomic per sample.
    #[test]
    fn a_fed_volume_overrides_the_atomic() {
        let mut s = BusStripNode::new();
        s.set_volume(Amplitude(1.0));
        let mut out = [0.0f32; 2];
        crate::test_support::tick_fed(&mut s, &[1.0, 1.0], &[Some(0.5)], &mut out);
        assert_eq!(out, [0.5, 0.5]);
    }

    /// Starting or stopping a feed is not ramped by the strip, so the gain
    /// is never applied twice across the block: the fed value takes over
    /// from its first frame, where the graph's declick puts it at the base.
    ///
    /// Mutation (run): drop the `last_fed` guard (ramp from the previous control
    /// share) → the first fed block starts at volume² (0.25) → fails.
    #[test]
    fn starting_a_feed_does_not_apply_the_gain_twice() {
        let mut s = BusStripNode::new();
        s.set_volume(Amplitude(0.5));
        let x = [1.0f32; 16];
        let steady = drive(&mut s, RATE, &[&x, &x], &[None, None]);
        assert!(steady[0].iter().all(|&y| y == 0.5));
        let held = [0.5f32; 16];
        let fed = drive(&mut s, RATE, &[&x, &x], &[Some(&held), None]);
        assert!(
            fed[0].iter().all(|&y| y == 0.5),
            "the fed volume alone, from its first frame: {:?}",
            fed[0]
        );
        let back = drive(&mut s, RATE, &[&x, &x], &[None, None]);
        assert!(back[0].iter().all(|&y| y == 0.5), "and the control again");
    }

    /// A clone shares the cells: it is the template `param_parts` forks from,
    /// and a fork taken later must read the cells as they are then.
    #[test]
    fn clone_shares_atomics() {
        let original = BusStripNode::new();
        let clone = original.clone();
        clone.set_volume(Amplitude(0.1));
        clone.set_muted(true);
        assert_eq!(original.volume_value(), Amplitude(0.1));
        assert!(original.is_muted());
    }

    /// Channels past the stereo pair are faded and muted but not balanced —
    /// a surround channel has no left/right axis to sit on.
    #[test]
    fn extra_channels_are_faded_but_not_balanced() {
        let mut s = BusStripNode::with_channels(ChannelLayout::from(3u16));
        s.set_volume(Amplitude(0.5));
        s.set_pan(Pan(-1.0));
        let mut out = [0.0f32; 3];
        tick(&mut s, &[1.0, 1.0, 1.0], &mut out);
        assert_eq!(out[0], 0.5); // near channel: unity balance × volume
        assert_eq!(out[1], 0.0); // far channel: balanced away
        assert_eq!(out[2], 0.5); // no balance applied, volume only
    }

    /// A muted strip renders silence on **every** channel — including the
    /// ones past the stereo pair, which take the unbalanced path. (This was
    /// `route`'s test, a hand-written copy of the gain arithmetic answering a
    /// control-thread query; with `route` gone the render is the one copy,
    /// and the property is pinned on it.)
    ///
    /// Mutation (run): `rest: volume` instead of `rest: fader` in `gains_for`
    /// → the third channel passes signal while muted → fails.
    #[test]
    fn mute_silences_every_channel() {
        let mut s = BusStripNode::with_channels(ChannelLayout::from(3u16));
        s.set_muted(true);
        let mut out = [1.0f32; 3];
        tick(&mut s, &[1.0, 1.0, 1.0], &mut out);
        for (c, &y) in out.iter().enumerate() {
            assert_eq!(y, 0.0, "channel {c} must be silent while muted");
        }
    }

    /// One block renders what the same frames do one at a time, while the
    /// controls hold: the ramp is the only thing block length changes.
    ///
    /// Mutation (run): read frame 0 for every frame in `render` →
    /// a frame at a time is unchanged, the block is not → fails.
    #[test]
    fn a_block_matches_its_frames() {
        let mut framed = BusStripNode::new();
        framed.set_volume(Amplitude(0.6));
        framed.set_pan(Pan(0.25));
        let mut block = framed.fork_fresh();

        const N: usize = 8;
        let samples: [(f32, f32); N] = [
            (0.4, -0.8),
            (1.0, 1.0),
            (-0.5, 0.5),
            (0.0, 0.0),
            (0.25, 0.75),
            (-1.0, -1.0),
            (0.1, -0.1),
            (0.9, 0.3),
        ];

        // Per-frame reference.
        let mut ticked = [[0.0f32; 2]; N];
        for (i, &(l, r)) in samples.iter().enumerate() {
            tick(&mut framed, &[l, r], &mut ticked[i]);
        }

        // One block over the same input.
        let l: Vec<f32> = samples.iter().map(|s| s.0).collect();
        let r: Vec<f32> = samples.iter().map(|s| s.1).collect();
        let out = drive(&mut block, RATE, &[&l, &r], &[]);
        for (i, frame) in ticked.iter().enumerate() {
            assert!((out[0][i] - frame[0]).abs() < 1e-6, "L @ {i}");
            assert!((out[1][i] - frame[1]).abs() < 1e-6, "R @ {i}");
        }
    }

    /// One `process` call over a constant (DC) stereo input, returned as frames.
    ///
    /// DC because it makes the output *be* the gain curve: any step in the gain
    /// is a step of the same size in the waveform.
    fn process_dc(strip: &mut BusStripNode, size: usize, l: f32, r: f32) -> Vec<[f32; 2]> {
        let (lv, rv) = (vec![l; size], vec![r; size]);
        let out = drive(strip, RATE, &[&lv, &rv], &[]);
        (0..size).map(|i| [out[0][i], out[1][i]]).collect()
    }

    /// Largest sample-to-sample jump across `frames`, on either channel.
    fn largest_step(frames: &[[f32; 2]]) -> f32 {
        frames
            .windows(2)
            .flat_map(|w| [(w[1][0] - w[0][0]).abs(), (w[1][1] - w[0][1]).abs()])
            .fold(0.0, f32::max)
    }

    /// Muting ramps to silence across one block instead of stepping to it, and
    /// is silent from the end of that block on; unmuting ramps back the same
    /// way.
    ///
    /// A mute that was a hard gate would make toggling it on a full-scale
    /// signal a full-scale step — a click.
    ///
    /// Mutation: rendering `target` unconditionally in `render` (no ramp) makes
    /// the toggle a step of 1.0 and fails the step bound on both edges; ramping
    /// with `from + (to - from) * t` instead of the exact-endpoint form is caught
    /// by the `== 0.0` assertion only when the rounding bites, so it is not
    /// claimed as covered here.
    #[test]
    fn mute_toggle_ramps_instead_of_clicking() {
        const BLOCK: usize = 64;
        // A linear ramp from 1 to 0 over 64 frames moves 1/64 per frame. The
        // bound leaves room for rounding and nothing near a real step.
        const MAX_STEP: f32 = 1.5 / BLOCK as f32;

        let mut s = BusStripNode::new();
        let steady = process_dc(&mut s, BLOCK, 1.0, 1.0);
        assert!(steady.iter().all(|f| *f == [1.0, 1.0]));

        s.set_muted(true);
        let fading = process_dc(&mut s, BLOCK, 1.0, 1.0);
        let mut seam = vec![*steady.last().unwrap()];
        seam.extend_from_slice(&fading);
        assert!(
            largest_step(&seam) <= MAX_STEP,
            "mute must ramp, not step: largest jump {}",
            largest_step(&seam)
        );
        assert_eq!(
            *fading.last().unwrap(),
            [0.0, 0.0],
            "the ramp ends on silence at the block's last frame, not near it"
        );
        let muted = process_dc(&mut s, BLOCK, 1.0, 1.0);
        assert!(
            muted.iter().all(|f| *f == [0.0, 0.0]),
            "silent from the next block on"
        );

        s.set_muted(false);
        let rising = process_dc(&mut s, BLOCK, 1.0, 1.0);
        let mut seam = vec![*muted.last().unwrap()];
        seam.extend_from_slice(&rising);
        assert!(
            largest_step(&seam) <= MAX_STEP,
            "unmute must ramp too: largest jump {}",
            largest_step(&seam)
        );
        assert_eq!(*rising.last().unwrap(), [1.0, 1.0]);
    }

    /// A fader move is reached by a linear ramp across one block, starting from
    /// where the last block ended — not by a step, and not per sample from the
    /// atomic.
    ///
    /// Mutation: rendering `target` unconditionally (no ramp) makes the first
    /// frame 0.5 and fails the "first frame is still near the old level"
    /// assertion; starting the ramp at `t = 0` instead of `t = 1/size` never
    /// reaches the target in-block and fails the last-frame assertion.
    #[test]
    fn volume_change_ramps_across_one_block() {
        const BLOCK: usize = 32;
        let mut s = BusStripNode::new();
        let _ = process_dc(&mut s, BLOCK, 1.0, 1.0);

        s.set_volume(Amplitude(0.5));
        let ramp = process_dc(&mut s, BLOCK, 1.0, 1.0);
        let first = ramp[0][0];
        assert!(
            (first - (1.0 - 0.5 / BLOCK as f32)).abs() < 1e-6,
            "the first frame moves one step from the old level, got {first}"
        );
        for pair in ramp.windows(2) {
            assert!(pair[1][0] < pair[0][0], "monotone ramp down");
            assert_eq!(pair[1][0], pair[1][1], "both channels ramp together");
        }
        assert_eq!(ramp[BLOCK - 1], [0.5, 0.5], "lands exactly on the target");

        let settled = process_dc(&mut s, BLOCK, 1.0, 1.0);
        assert!(settled.iter().all(|f| *f == [0.5, 0.5]));
    }

    /// The first block after construction — or after `reset` — has no previous
    /// output to be continuous with, so it starts *at* its target. A strip built
    /// muted must not fade in from unity for a block before going silent.
    ///
    /// Mutation: seeding `ramp_from` with unity gains instead of `None` (in the
    /// constructor or in `reset`) makes the first frame of each render below
    /// non-zero and fails.
    #[test]
    fn a_fresh_or_reset_strip_starts_at_its_target() {
        let mut s = BusStripNode::new();
        s.set_muted(true);
        assert!(process_dc(&mut s, 16, 1.0, 1.0)
            .iter()
            .all(|f| *f == [0.0, 0.0]));

        s.set_muted(false);
        s.reset();
        s.set_volume(Amplitude(0.25));
        assert!(process_dc(&mut s, 16, 1.0, 1.0)
            .iter()
            .all(|f| *f == [0.25, 0.25]));
    }

    /// Every channel, the ones past the balanced pair included, follows the
    /// linear ramp `from·(1−t) + to·t` at `t = (i+1)/n`, at a block length
    /// no SIMD width divides (so a vectorised loop's tail is covered), and
    /// lands exactly on the target.
    ///
    /// Mutations (run): the ramping branch of `render` applying the steady
    /// target alone → frame 0 is 0.5, not ≈1 → fails; the ramp's `t` as
    /// `i / n` → the last frame misses the target → fails.
    #[test]
    fn every_channel_ramps_linearly_past_the_simd_width() {
        const N: usize = 67;
        let mut s = BusStripNode::with_channels(ChannelLayout::from(3u16));
        s.set_pan(Pan(-0.5));
        let ones = [1.0f32; N];
        let _ = drive(&mut s, RATE, &[&ones, &ones, &ones], &[]);
        s.set_volume(Amplitude(0.5));
        let out = drive(&mut s, RATE, &[&ones, &ones, &ones], &[]);
        // Balance at -0.5: left takes the fader, right half of it, the rest
        // the fader.
        let (from, to) = ([1.0f32, 0.5, 1.0], [0.5f32, 0.25, 0.5]);
        for (c, y) in out.iter().enumerate() {
            for (i, &y) in y.iter().enumerate() {
                let t = (i + 1) as f32 / N as f32;
                let want = from[c] + (to[c] - from[c]) * t;
                assert!(
                    (y - want).abs() < 1e-6,
                    "channel {c} frame {i}: {y} against {want}"
                );
            }
            assert_eq!(y[N - 1], to[c], "channel {c} lands on the target");
        }
    }

    /// A mute ramp under fed volume and pan is the product of the three:
    /// `(1 − t) · volume[i] · balance(pan[i])` on each channel, frame by
    /// frame.
    ///
    /// Mutations (run): the ramping-and-fed branch of `render` dropping the
    /// ramp (`steady · fed`) → every frame is silent → fails; `fed_gain`
    /// taking the left side for both channels of the pair → the right is
    /// halved → fails; `fed_gain` without its past-the-pair case → the
    /// third channel takes the left side's balance → fails.
    #[test]
    fn a_mute_ramp_under_fed_params_is_their_product() {
        const N: usize = 37;
        let mut s = BusStripNode::with_channels(ChannelLayout::from(3u16));
        let ones = [1.0f32; N];
        let volume: Vec<f32> = (0..N).map(|i| 0.25 + i as f32 / N as f32).collect();
        let pan = [0.5f32; N];
        let fed = [Some(&volume[..]), Some(&pan[..])];
        let _ = drive(&mut s, RATE, &[&ones, &ones, &ones], &fed);
        s.set_muted(true);
        let out = drive(&mut s, RATE, &[&ones, &ones, &ones], &fed);
        // Balance at 0.5: left halved, right at unity, the rest unbalanced.
        let balance = [0.5f32, 1.0, 1.0];
        for (c, (y, &bal)) in out.iter().zip(&balance).enumerate() {
            for (i, (&y, &v)) in y.iter().zip(&volume).enumerate() {
                let live = 1.0 - (i + 1) as f32 / N as f32;
                let want = live * v * bal;
                assert!(
                    (y - want).abs() < 1e-6,
                    "channel {c} frame {i}: {y} against {want}"
                );
            }
            assert_eq!(y[N - 1], 0.0, "channel {c} ends silent");
        }
    }
}
