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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tutti_core::dsp::{AudioUnit, BufferMut, BufferRef, Setting, SignalFrame};
use tutti_core::{Amplitude, ChannelLayout, Pan, Param, ParamAddr, Tail, UnitParam};
use tutti_mod::{AtomicTarget, ModParams, ModTarget};

use crate::ParamPorts;

/// A mixer strip: volume, stereo balance and mute over `channels` audio ports.
///
/// Ports are `channels` audio inputs → `channels` outputs, plus the optional
/// param-input ports described in
/// [`with_param_inputs`](Self::with_param_inputs).
///
/// Params: [`UnitParam::Volume`] (linear amplitude), [`UnitParam::Pan`] (`-1..1`)
/// and [`UnitParam::Mute`] (`>= 0.5` is muted). Settings for anything else are
/// ignored, which is what lets a host push params without knowing the node type.
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
    /// When true, a volume param-input port follows the audio inputs.
    mod_volume: bool,
    /// When true, a pan param-input port follows the volume one.
    mod_pan: bool,
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
            mod_volume: false,
            mod_pan: false,
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

    /// A strip with optional audio-rate param-input ports.
    ///
    /// Present ports follow the audio inputs **in the order volume, then pan**,
    /// and each overrides its atomic per sample. The atomics still hold the base
    /// (they feed the upstream param-sum's base port), so the UI handle path is
    /// unchanged. The order is positional and unrecoverable from the value, which
    /// is why it is stated here and answered by
    /// [`param_port`](ParamPorts::param_port) rather than assumed at call sites.
    /// The ports follow the audio inputs, so their indices **move with the
    /// width** — ask `param_port`, never assume an index.
    ///
    /// Width and modulation are **independent axes**: `channels` says how wide
    /// the strip is, the `mod_*` flags say which params it reads at audio rate.
    /// They were not independent — this constructor hardcoded
    /// [`ChannelLayout::STEREO`] — so asking for a modulated 5.1 strip silently
    /// returned a *stereo* one, and the only symptom was a `set_source` on a
    /// param port that resolved and carried the wrong signal.
    ///
    /// There is deliberately no audio-rate mute port: a per-sample boolean is a
    /// gate, not a mute, and gating is [`crate::Gate`]'s job.
    pub fn with_param_inputs(
        channels: impl Into<ChannelLayout>,
        mod_volume: bool,
        mod_pan: bool,
    ) -> Self {
        Self {
            mod_volume,
            mod_pan,
            ..Self::with_channels(channels)
        }
    }

    /// Input-port index of the audio-rate volume input, if present.
    #[inline]
    pub fn volume_port(&self) -> Option<usize> {
        self.mod_volume.then_some(self.channels())
    }

    /// Input-port index of the audio-rate pan input, if present. Sits after the
    /// volume port when that one is also present.
    #[inline]
    pub fn pan_port(&self) -> Option<usize> {
        self.mod_pan
            .then_some(self.channels() + self.mod_volume as usize)
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
    /// values above 1.0 amplify. Read once per block.
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
    /// A hard gate on the output, applied after volume and pan — muting does
    /// not disturb the fader position, so unmuting restores the previous level.
    /// Shared across clones, so the write reaches a live node.
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

    /// The gains actually applied this sample: balance × volume, or zero when
    /// muted.
    ///
    /// Mute is folded in as a **factor rather than a branch**, so the per-sample
    /// cost does not depend on the mute state and a muted strip cannot take a
    /// cheaper path that diverges from the live one.
    #[inline]
    fn gains(&self, volume: Amplitude, pan: Pan) -> (Amplitude, Amplitude) {
        // `Amplitude * f32` is the scaling the unit grants; cascading two gain
        // stages multiplies them, which is exactly what balance × fader is.
        let live = self.live_factor();
        let (l, r) = Self::balance_gains(pan);
        (l * volume.get() * live, r * volume.get() * live)
    }

    /// `1.0` when the strip is passing audio, `0.0` when muted — the mute as a
    /// multiplicand rather than a branch.
    #[inline]
    fn live_factor(&self) -> f32 {
        !self.muted.load(Ordering::Relaxed) as u8 as f32
    }

    /// Volume/pan for this sample: a present param port overrides the atomic.
    ///
    /// The port carries a raw sample, so this is the boundary where an untyped
    /// float becomes a typed quantity again — named rather than inlined so there
    /// is one place that decision happens.
    #[inline]
    fn effective(&self, at: impl Fn(usize) -> f32) -> (Amplitude, Pan) {
        let volume = match self.volume_port() {
            Some(p) => Amplitude(at(p)),
            None => self.volume.load(),
        };
        let pan = match self.pan_port() {
            Some(p) => Pan(at(p)),
            None => self.pan.load(),
        };
        (volume, pan)
    }

    /// Apply `(left, right)` to a frame, leaving channels past the stereo pair
    /// scaled by volume/mute alone — they have no left/right axis to balance on.
    #[inline]
    fn apply(
        &self,
        gains: (Amplitude, Amplitude),
        volume: Amplitude,
        get: impl Fn(usize) -> f32,
        mut put: impl FnMut(usize, f32),
    ) {
        let unbalanced = volume * self.live_factor();
        for c in 0..self.channels() {
            let g = match c {
                0 => gains.0,
                1 => gains.1,
                _ => unbalanced,
            };
            // The one place a gain meets a sample. `get(c)` is a raw sample, not
            // an `Amplitude` — a sample is a signal value, not a gain — so the
            // unit comes off here rather than the sample being wrapped.
            put(c, get(c) * g.get());
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
            mod_volume: self.mod_volume,
            mod_pan: self.mod_pan,
        }
    }
}

impl AudioUnit for BusStripNode {
    fn inputs(&self) -> usize {
        self.channels() + self.mod_volume as usize + self.mod_pan as usize
    }

    fn outputs(&self) -> usize {
        self.channels()
    }

    fn reset(&mut self) {}

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let (volume, pan) = self.effective(|p| input[p]);
        let gains = self.gains(volume, pan);
        self.apply(gains, volume, |c| input[c], |c, v| output[c] = v);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            let (volume, pan) = self.effective(|p| input.at_f32(p, i));
            let gains = self.gains(volume, pan);
            self.apply(
                gains,
                volume,
                |c| input.at_f32(c, i),
                |c, v| output.set_f32(c, i, v),
            );
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
        // gains that are actually in force. Read once here — `route` is a
        // control-thread query, not the audio path.
        let volume = self.volume.load();
        let (l, r) = self.gains(volume, self.pan.load());
        let unbalanced = volume * self.live_factor();
        let channels = self.channels();
        let mut out = SignalFrame::new(channels);
        for c in 0..channels {
            let g = match c {
                0 => l,
                1 => r,
                // `unbalanced`, not the bare fader: a muted strip propagates
                // silence on every channel, not just the balanced pair.
                _ => unbalanced,
            };
            out.set(c, input.at(c).scale(g.get() as f64));
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

impl ParamPorts for BusStripNode {
    fn param_port(&self, param: UnitParam) -> Option<usize> {
        match param {
            UnitParam::Volume => self.volume_port(),
            UnitParam::Pan => self.pan_port(),
            _ => None,
        }
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

    #[test]
    fn param_ports_are_absent_unless_built_with_them() {
        let plain = BusStripNode::new();
        assert_eq!(plain.inputs(), 2);
        assert_eq!(plain.param_port(UnitParam::Volume), None);
        assert_eq!(plain.param_port(UnitParam::Pan), None);

        // Volume then pan, after the two audio inputs.
        let both = BusStripNode::with_param_inputs(ChannelLayout::STEREO, true, true);
        assert_eq!(both.inputs(), 4);
        assert_eq!(both.param_port(UnitParam::Volume), Some(2));
        assert_eq!(both.param_port(UnitParam::Pan), Some(3));

        // Pan alone still lands directly after the audio inputs — the index is
        // derived, not a fixed slot.
        let pan_only = BusStripNode::with_param_inputs(ChannelLayout::STEREO, false, true);
        assert_eq!(pan_only.inputs(), 3);
        assert_eq!(pan_only.param_port(UnitParam::Volume), None);
        assert_eq!(pan_only.param_port(UnitParam::Pan), Some(2));
    }

    /// A present port overrides the atomic per sample.
    #[test]
    fn param_port_overrides_the_atomic() {
        let mut s = BusStripNode::with_param_inputs(ChannelLayout::STEREO, true, false);
        s.set_volume(Amplitude(1.0));
        let mut out = [0.0f32; 2];
        // Ports: [L, R, volume]
        s.tick(&[1.0, 1.0, 0.5], &mut out);
        assert_eq!(out, [0.5, 0.5]);
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
        use tutti_core::dsp::Signal;

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

    /// Width and modulation are independent axes.
    ///
    /// The regression for the bug this constructor had: it hardcoded
    /// `ChannelLayout::STEREO`, so a modulated 6-channel strip came back
    /// *stereo*. The arity assertion fails against that version.
    #[test]
    fn a_modulated_strip_is_as_wide_as_it_was_asked_for() {
        let s = BusStripNode::with_param_inputs(ChannelLayout::from(6u16), true, true);
        assert_eq!(s.outputs(), 6, "the width is what was asked for");
        assert_eq!(s.inputs(), 8, "six audio inputs, then volume and pan");
        assert_eq!(
            s.volume_port(),
            Some(6),
            "param ports follow the audio inputs, so their indices move with the width"
        );
        assert_eq!(s.pan_port(), Some(7), "and keep their documented order");
    }
}
