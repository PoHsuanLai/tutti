//! The mixer strip — volume, stereo balance, mute — as one addressable unit.
//!
//! A mixer bus applies three things to a summed signal: a fader, a balance, and
//! a mute. [`BusStripNode`] is all three in one node, driven through the ordinary
//! [`UnitParam`] setting path, so a host reconciles it with the same generic
//! machinery it uses for a filter cutoff and needs no downcast.
//!
//! # Why this is a unit rather than a fundsp graph
//!
//! The DSP is a handful of multiplies, and fundsp can express it: `shared`/`var`
//! give a lock-free scalar, `pan`/`panner` an equal-power law, `mul` a gain. What
//! that composition does *not* give is **addressing**, and addressing is the
//! whole point.
//!
//! Every scalar here lives in a [`Param<T>`] and is written through
//! [`AudioUnit::set`], decoded by
//! [`from_setting`](tutti_core::unit_param::from_setting) — the same shape every
//! other node in this crate uses. A `Shared`-based strip would instead need its
//! host to hold the handles and write them directly, which is a second param path
//! running alongside the declared one, invisible to the reconcilers that own the
//! first. Two writers, one port, no way to see the conflict.
//!
//! fundsp's `Panner` is unreachable from the declared path for a second reason:
//! it answers only `Parameter::Pan`, while `node_setting` emits
//! `Setting::value(..).index(..)`. And `Panner<U2>` takes its pan as an *audio
//! input port*, which would sit in the same index space `PortSources` declares
//! into — where `Net::pipe_input` silently overwrites it.
//!
//! # The balance law is this crate's own
//!
//! fundsp's `Panner` is **mono**-to-stereo: one signal placed between two
//! speakers. A mixer strip is stereo-to-stereo — it *rebalances* a signal that is
//! already stereo, and must leave it untouched at centre. Those are different
//! functions, and nothing in tutti or fundsp implemented the second one. See
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tutti_core::{Amplitude, ChannelLayout, Pan, Param, ParamAddr, Tail, UnitParam};
use tutti_core::{AudioUnit, BufferMut, BufferRef, ParamFeed, Setting, SignalFrame};
use tutti_mod::{AtomicTarget, ModParams, ModTarget};

/// A mixer strip: volume, stereo balance and mute over `channels` audio ports.
///
/// Ports are `channels` audio inputs → `channels` outputs. Volume and pan
/// are modulatable by the graph (design doc 013 item 6), in that port order
/// ([`STRIP_PARAMS`]): a per-frame value fed to the strip's
/// [`ParamFeed`](tutti_core::ParamFeed) overrides its atomic per sample.
/// There is deliberately no modulatable mute: a per-sample boolean is a
/// gate, not a mute, and gating is [`crate::GateNode`]'s job.
///
/// Params: [`UnitParam::Volume`] (linear amplitude), [`UnitParam::Pan`] (`-1..1`)
/// and [`UnitParam::Mute`] (`>= 0.5` is muted). Settings for anything else are
/// ignored, which is what lets a host push params without knowing the node type.
/// The params a [`BusStripNode`] lets the graph modulate, in port order.
pub const STRIP_PARAMS: [UnitParam; 2] = [UnitParam::Volume, UnitParam::Pan];

pub struct BusStripNode {
    volume: Param<Amplitude>,
    pan: Param<Pan>,
    /// Not a [`Param`]: that requires `Unit<Raw = f32>` and a mute is a boolean.
    /// Deliberately *not* given a unit newtype either — per the units rule a new
    /// type needs a distinct range or algebra, and a bool-as-float has neither.
    muted: Arc<AtomicBool>,
    /// The width this strip runs at — its declared audio layout
    /// (`inputs()` audio ports == `outputs()`).
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
    /// Per-frame volume and pan from the graph, when it modulates them
    /// ([`STRIP_PARAMS`]).
    feed: ParamFeed,
    /// Which params the previous block was fed (bit `k`). A change is not
    /// ramped by the strip: the graph declicks the fed value itself, and
    /// ramping the control gain across the same block would apply the gain
    /// twice for its length.
    last_fed: u8,
    /// The control-driven gains the previous block ended on — where this
    /// block's ramp starts. `None` before the first block and after
    /// [`reset`](AudioUnit::reset): with no previous output there is nothing to
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
    /// `from` at `t = 0`, `to` at `t = 1` — and *exactly* `to` there.
    ///
    /// Written as `from·(1−t) + to·t` rather than `from + (to−from)·t` for that
    /// endpoint: the second form rounds, so a ramp to silence could end a few
    /// ulps above zero and a muted strip would leak. Not an `Amplitude` operator
    /// because `Amplitude` deliberately has no `Add` (cascading gains multiply);
    /// an interpolation between two gains is a different operation, and this is
    /// its one home.
    #[inline]
    fn lerp(from: Self, to: Self, t: f32) -> Self {
        let mix = |a: Amplitude, b: Amplitude| Amplitude(a.get() * (1.0 - t) + b.get() * t);
        Self {
            left: mix(from.left, to.left),
            right: mix(from.right, to.right),
            rest: mix(from.rest, to.rest),
        }
    }

    /// Two gain stages in series. They multiply — see [`Amplitude`].
    #[inline]
    fn cascade(self, other: Self) -> Self {
        Self {
            left: self.left * other.left.get(),
            right: self.right * other.right.get(),
            rest: self.rest * other.rest.get(),
        }
    }

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
            muted: Arc::new(AtomicBool::new(false)),
            layout: ChannelLayout::from(layout.count().max(1)),
            feed: ParamFeed::new(&STRIP_PARAMS),
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
    /// Untyped by the shape of the contract, not by omission: this is what
    /// [`ModParams::mod_target`] hands to an [`AtomicTarget`], and that boundary
    /// speaks `Arc<AtomicF32>`. To *read* the value, use
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

    /// Set the fader position as a linear [`Amplitude`], unclamped.
    ///
    /// Linear, not [`Db`](tutti_core::Db): `1.0` is unity, `0.0` silent, and
    /// values above 1.0 amplify. Read once per block and ramped to across it.
    pub fn set_volume(&self, volume: impl Into<Amplitude>) {
        self.volume.store(volume.into());
    }

    /// Set the balance, clamped to `-1..1` — an out-of-range value would
    /// otherwise amplify one channel past unity rather than saturating.
    pub fn set_pan(&self, pan: impl Into<Pan>) {
        self.pan.store(Pan(pan.into().get().clamp(-1.0, 1.0)));
    }

    /// Mute or unmute the strip.
    ///
    /// A gate on the output, applied after volume and pan — muting does not
    /// disturb the fader position, so unmuting restores the previous level.
    /// Shared across clones, so the write reaches a live node.
    ///
    /// Not a hard gate: the next block ramps to (or from) silence across its
    /// length, so the output is silent from the end of that block on. A step to
    /// zero is an audible click on anything but silence.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    /// Whether the strip is currently muted and emitting silence.
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    /// Per-channel gains for a stereo **balance** at position `pan`.
    ///
    /// Attenuates the channel you pan away from and leaves the other at unity:
    /// `(1, 1)` at centre, `(1, 0)` hard left, `(0, 1)` hard right. Unity at
    /// centre is the property that matters — a strip must be transparent until
    /// somebody moves it, and this is what
    /// [`unity_at_defaults`](self::tests::unity_at_defaults) pins.
    ///
    /// This is **balance, not panning**, and the distinction is why fundsp's
    /// `Panner` cannot be reused. A panner *places* a mono signal, so it spreads
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
        let p = pan.get().clamp(-1.0, 1.0);
        (Amplitude((1.0 - p).min(1.0)), Amplitude((1.0 + p).min(1.0)))
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
        !self.muted.load(Ordering::Relaxed) as u8 as f32
    }

    /// The gains the **control cells** ask for — the ramp's target.
    ///
    /// A control the graph feeds (`fed`: volume, pan) contributes unity here,
    /// and its value comes from [`fed_gains`](Self::fed_gains) per sample
    /// instead. Mute is never fed, so it is always here.
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

    /// The gains the fed params ask for at one sample — unity for one that
    /// is not fed.
    ///
    /// The feed carries raw values, so this is the boundary where an untyped
    /// float becomes a typed quantity again — named rather than inlined so there
    /// is one place that decision happens.
    ///
    /// Not ramped: a fed value is already a signal, one value per sample, and
    /// the graph that feeds it owns its continuity.
    #[inline]
    fn fed_gains(volume: Option<f32>, pan: Option<f32>) -> StripGains {
        Self::gains_for(
            volume.map_or(Amplitude::UNITY, Amplitude),
            pan.map_or(Pan::CENTER, Pan),
            1.0,
        )
    }

    /// Render `size` frames: the one gain path, shared by `tick` and `process`.
    ///
    /// Reads the control cells **once**, then ramps linearly from the gains the
    /// previous call ended on to the ones just read, landing exactly on them at
    /// the last frame. `tick` is this with `size == 1` — a one-frame block, whose
    /// ramp is therefore a step — which is what keeps the two paths one
    /// implementation instead of two that can drift.
    ///
    /// # The ramp is one block long, so its length is the caller's block size
    ///
    /// At the engine's 64-frame chunks that is ~1.3 ms at 48 kHz: long enough to
    /// turn a mute's step into a slope with no broadband click, short enough
    /// that the mute is silent from the end of that block on. A caller that
    /// renders one frame at a time gets a step, exactly as `tick` does.
    #[inline]
    fn render(
        &mut self,
        size: usize,
        get: impl Fn(usize, usize) -> f32,
        volume: Option<&[f32]>,
        pan: Option<&[f32]>,
        mut put: impl FnMut(usize, usize, f32),
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
        // Hoisted: whether to ramp and whether to read the feed are both
        // per-block facts. The steady case skips the interpolation outright,
        // which also keeps its output bit-identical to the unramped arithmetic.
        let ramping = from != target;
        let has_fed = fed_bits != 0;
        let channels = self.channels();

        for i in 0..size {
            let mut gains = match ramping {
                // `(i + 1) / size`, not `(i + 1) * (1 / size)`: the division is
                // exact at the last frame (`size / size == 1`), so the ramp
                // lands on the target rather than an ulp beside it.
                true => StripGains::lerp(from, target, (i + 1) as f32 / size as f32),
                false => target,
            };
            if has_fed {
                gains = gains.cascade(Self::fed_gains(volume.map(|v| v[i]), pan.map(|v| v[i])));
            }
            for c in 0..channels {
                // The one place a gain meets a sample. `get(c, i)` is a raw
                // sample, not an `Amplitude` — a sample is a signal value, not a
                // gain — so the unit comes off here rather than the sample being
                // wrapped.
                put(c, i, get(c, i) * gains.channel(c).get());
            }
        }
    }
}

impl Default for BusStripNode {
    fn default() -> Self {
        Self::new()
    }
}

/// Clones share the atomics rather than forking them — a cloned strip is another
/// handle on the same fader, which is what `Net`'s node cloning needs.
impl Clone for BusStripNode {
    fn clone(&self) -> Self {
        Self {
            volume: self.volume.handle(),
            pan: self.pan.handle(),
            muted: Arc::clone(&self.muted),
            layout: self.layout,
            feed: self.feed.clone(),
            last_fed: self.last_fed,
            ramp_from: self.ramp_from,
        }
    }
}

impl AudioUnit for BusStripNode {
    fn inputs(&self) -> usize {
        self.channels()
    }

    fn outputs(&self) -> usize {
        self.channels()
    }

    /// Forget the previous block's gains, so the next block starts at its
    /// target rather than ramping from a signal that is no longer playing.
    /// Detach volume, pan and mute (see `Param::detach`), so a fork renders
    /// the strip as it was set when it was taken, not a fader ride or a mute
    /// made while it runs. Values are kept.
    fn isolate(&mut self) {
        self.volume.detach();
        self.pan.detach();
        self.muted = Arc::new(AtomicBool::new(self.muted.load(Ordering::Acquire)));
    }

    fn reset(&mut self) {
        self.ramp_from = None;
    }

    /// A one-frame block — see `BusStripNode::render`.
    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let feed = ParamFeed::take(&mut self.feed);
        self.render(
            1,
            |c, _| input[c],
            feed.get(0, 1),
            feed.get(1, 1),
            |c, _, v| output[c] = v,
        );
        self.feed = feed;
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // Moved out for the render, which takes `&mut self`; moving it
        // allocates nothing.
        let feed = ParamFeed::take(&mut self.feed);
        self.render(
            size,
            |c, i| input.at_f32(c, i),
            feed.get(0, size),
            feed.get(1, size),
            |c, i, v| output.set_f32(c, i, v),
        );
        self.feed = feed;
    }

    fn param_feed(&mut self) -> Option<&mut ParamFeed> {
        Some(&mut self.feed)
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.volume.load().get()),
            1 => Some(self.pan.load().get()),
            _ => None,
        }
    }

    fn set(&mut self, setting: Setting) {
        let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) else {
            return;
        };
        match param {
            UnitParam::Volume => self.set_volume(Amplitude(value)),
            UnitParam::Pan => self.set_pan(Pan(value)),
            // The `>= 0.5` threshold is `UnitParam::Mute`'s documented encoding:
            // `Setting` carries an f32, so the boolean has to ride one.
            UnitParam::Mute => self.set_muted(value >= 0.5),
            // A unit ignores params it does not own — this is what lets a host
            // push a setting without dispatching on the node type.
            _ => {}
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::BUS_STRIP_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // A gain stage: each output is its input scaled, so propagate with the
        // gains the controls settle on. Read once here — `route` is a
        // control-thread query, not the audio path. The target, not a point on
        // the ramp: a ramp lasts one block, and `route` answers for the steady
        // state.
        let gains = Self::gains_for(self.volume.load(), self.pan.load(), self.live_factor());
        let channels = self.channels();
        let mut out = SignalFrame::new(channels);
        for c in 0..channels {
            // `gains.rest` past the pair, not the bare fader: a muted strip
            // propagates silence on every channel, not just the balanced pair.
            out.set(c, input.at(c).scale(gains.channel(c).get() as f64));
        }
        out
    }

    /// Volume, pan and mute are per-frame gains, so this stops with its input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl ModParams for BusStripNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match p {
            ParamAddr::Unit(UnitParam::Volume) => self.volume(),
            ParamAddr::Unit(UnitParam::Pan) => self.pan(),
            _ => return None,
        };
        Some(Arc::new(AtomicTarget::with_mirror(base, min, max, atomic)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::unit_param::node_setting;

    /// Drive a param the way a host does — through a `Setting` — rather than by
    /// calling the setter, so the test covers the path `AudioParam<U, P>` uses.
    fn set_param(strip: &mut BusStripNode, param: UnitParam, value: f32) {
        let node = tutti_core::dsp::NodeId::new();
        strip.set(node_setting(node, param, value).peel());
    }

    fn tick2(strip: &mut BusStripNode, l: f32, r: f32) -> (f32, f32) {
        let mut out = [0.0f32; 2];
        strip.tick(&[l, r], &mut out);
        (out[0], out[1])
    }

    /// A strip must be transparent until somebody moves it. This is the property
    /// that rules out reusing fundsp's equal-power `Panner`, which would attenuate
    /// a centred signal by 3 dB.
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
        let mut s = BusStripNode::new();

        set_param(&mut s, UnitParam::Volume, 0.25);
        assert_eq!(s.volume_value(), Amplitude(0.25));

        set_param(&mut s, UnitParam::Pan, -1.0);
        assert_eq!(s.pan_value(), Pan(-1.0));

        set_param(&mut s, UnitParam::Mute, 1.0);
        assert!(s.is_muted());
        set_param(&mut s, UnitParam::Mute, 0.0);
        assert!(!s.is_muted());
    }

    /// A unit ignores params it does not own — the property that lets the generic
    /// reconciler push any param without dispatching on node type.
    #[test]
    fn ignores_params_it_does_not_own() {
        let mut s = BusStripNode::new();
        set_param(&mut s, UnitParam::Cutoff, 8000.0);
        set_param(&mut s, UnitParam::Drive, 4.0);
        assert_eq!(tick2(&mut s, 1.0, 1.0), (1.0, 1.0));
    }

    /// The feed declares volume then pan, and never changes the arity.
    ///
    /// Mutation (run): swap `STRIP_PARAMS`' order → the first assertion
    /// fails, and `a_fed_volume_overrides_the_atomic` reads the pan.
    #[test]
    fn the_feed_declares_volume_then_pan() {
        let mut s = BusStripNode::with_channels(ChannelLayout::from(6u16));
        assert_eq!(
            s.param_feed().map(|f| f.params()),
            Some(&[UnitParam::Volume, UnitParam::Pan][..])
        );
        assert_eq!((s.inputs(), s.outputs()), (6, 6));
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
        s.param_feed().expect("fed").feed(0, &[0.5]);
        s.tick(&[1.0, 1.0], &mut out);
        assert_eq!(out, [0.5, 0.5]);
    }

    /// Starting or stopping a feed is not ramped by the strip, so the gain
    /// is never applied twice across the block: the fed value takes over
    /// from its first frame, where the graph's declick puts it at the base.
    ///
    /// Mutation (run): drop the `last_fed` guard (ramp from the old control
    /// share) → the first fed block starts at volume² (0.25) → fails.
    #[test]
    fn starting_a_feed_does_not_apply_the_gain_twice() {
        let mut s = BusStripNode::new();
        s.set_volume(Amplitude(0.5));
        let x = [1.0f32; 16];
        let steady = crate::testing::process_fed(&mut s, &[&x, &x], &[None, None]);
        assert!(steady[0].iter().all(|&y| y == 0.5));
        let held = [0.5f32; 16];
        let fed = crate::testing::process_fed(&mut s, &[&x, &x], &[Some(&held), None]);
        assert!(
            fed[0].iter().all(|&y| y == 0.5),
            "the fed volume alone, from its first frame: {:?}",
            fed[0]
        );
        let back = crate::testing::process_fed(&mut s, &[&x, &x], &[None, None]);
        assert!(back[0].iter().all(|&y| y == 0.5), "and the control again");
    }

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
        s.tick(&[1.0, 1.0, 1.0], &mut out);
        assert_eq!(out[0], 0.5); // near channel: unity balance × volume
        assert_eq!(out[1], 0.0); // far channel: balanced away
        assert_eq!(out[2], 0.5); // no balance applied, volume only
    }

    /// `route` reports what the strip does to a signal, so a muted strip must
    /// report silence on **every** channel — including the ones past the stereo
    /// pair, which take the unbalanced path.
    ///
    /// Worth a test because `route` is a second, hand-written copy of the gain
    /// arithmetic (it answers a control-thread query rather than processing
    /// samples), so it can drift from `tick`/`process` without any audio changing.
    /// An earlier draft applied the bare fader there and let a muted surround
    /// channel report itself as passing signal.
    #[test]
    fn route_reports_mute_on_every_channel() {
        use tutti_core::Signal;

        let mut s = BusStripNode::with_channels(ChannelLayout::from(3u16));
        s.set_muted(true);
        let mut input = SignalFrame::new(3);
        for c in 0..3 {
            input.set(c, Signal::Value(1.0));
        }
        // `Signal` implements neither `PartialEq` nor `Debug`, so match the
        // variant rather than comparing.
        let out = s.route(&input, 48_000.0);
        for c in 0..3 {
            assert!(
                matches!(out.at(c), Signal::Value(v) if v == 0.0),
                "channel {c} must report silence while muted"
            );
        }
    }

    /// The block path must agree with the per-sample one. They are separate
    /// implementations, so a change to the gain arithmetic that touches only one
    /// would otherwise show up as "it sounds right in tests and wrong live".
    #[test]
    fn process_matches_tick() {
        use tutti_core::BufferVec;

        let mut strip = BusStripNode::new();
        strip.set_volume(Amplitude(0.6));
        strip.set_pan(Pan(0.25));

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

        // Per-sample reference.
        let mut ticked = [[0.0f32; 2]; N];
        for (i, &(l, r)) in samples.iter().enumerate() {
            strip.tick(&[l, r], &mut ticked[i]);
        }

        // Block path over the same input.
        let mut input = BufferVec::new(2);
        {
            let mut inb = input.buffer_mut();
            for (i, &(l, r)) in samples.iter().enumerate() {
                inb.set_f32(0, i, l);
                inb.set_f32(1, i, r);
            }
        }
        let mut output = BufferVec::new(2);
        {
            let mut outb = output.buffer_mut();
            strip.process(N, &input.buffer_ref(), &mut outb);
            for (i, frame) in ticked.iter().enumerate() {
                assert!((outb.at_f32(0, i) - frame[0]).abs() < 1e-6, "L @ {i}");
                assert!((outb.at_f32(1, i) - frame[1]).abs() < 1e-6, "R @ {i}");
            }
        }
    }

    /// One `process` call over a constant (DC) stereo input, returned as frames.
    ///
    /// DC because it makes the output *be* the gain curve: any step in the gain
    /// is a step of the same size in the waveform.
    fn process_dc(strip: &mut BusStripNode, size: usize, l: f32, r: f32) -> Vec<[f32; 2]> {
        use tutti_core::BufferVec;

        let mut input = BufferVec::new(2);
        {
            let mut inb = input.buffer_mut();
            for i in 0..size {
                inb.set_f32(0, i, l);
                inb.set_f32(1, i, r);
            }
        }
        let mut output = BufferVec::new(2);
        let mut outb = output.buffer_mut();
        strip.process(size, &input.buffer_ref(), &mut outb);
        (0..size)
            .map(|i| [outb.at_f32(0, i), outb.at_f32(1, i)])
            .collect()
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
    /// D7 in design doc 013: the mute was a hard gate, so toggling it on a
    /// full-scale signal was a full-scale step — a click.
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
}
