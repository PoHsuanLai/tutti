//! Noise gate with external sidechain: attenuates below a threshold.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{ChannelLayout, UnitParam};

use super::envelope::GateEnvelopeFollower;
use super::params::AttackRelease;
use super::utils::{amplitude_to_db, apply_gain_lane, compute_gate_gain, ramp_db, sidechain_level};
use tutti_core::{Db, Param, SampleRate, Seconds, Tail};

use crate::ramp::LastGood;

/// Shared gate state used by the per-sample gain computation.
#[derive(Clone)]
pub(super) struct GateCore {
    pub threshold_db: Param<Db>,
    pub timing: AttackRelease,
    pub hold: Param<Seconds>,
    pub range_db: Param<Db>,

    envelope: f32,
    follower: GateEnvelopeFollower,
    /// The range the previous block ended on, the start of this block's ramp.
    /// `None` until the first block, which starts on its own value.
    last_range: Option<Db>,
    /// Last finite threshold, range, attack, hold and release. The times
    /// become the follower's coefficients and hold count, and the range the
    /// start of every later ramp, so a non-finite write reads as unchanged
    /// (see [`LastGood`]).
    good: [LastGood; 5],
}

/// The controls one block runs on, read from their atomics **once** at the
/// top of `process`.
///
/// Two atomic loads per sample used to sit inside the gain computation. The
/// threshold only decides open/closed, and the attack/hold/release follower
/// already turns a changed decision into a ramp, so it is held for the block.
/// The range multiplies the output directly — the closed floor — so a step
/// would click; it ramps linearly from the previous block's value instead.
#[derive(Clone, Copy)]
pub(super) struct GateBlock {
    threshold: Db,
    range_from: Db,
    range_to: Db,
}

impl GateCore {
    /// Builds the shared core. The follower is seeded at the placeholder
    /// [`SampleRate::DEFAULT`]; `GateNode`'s `prepare` retunes it from
    /// `timing` and `hold`, which is why those are kept as [`Seconds`] rather
    /// than only as coefficients and a sample count — the seconds are the
    /// recoverable form, the derived values are not.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    pub fn new(
        threshold_db: impl Into<Db>,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        let threshold_db = threshold_db.into();
        let attack = attack.into();
        let hold = hold.into();
        let release = release.into();
        Self {
            threshold_db: Param::new(threshold_db),
            timing: AttackRelease::new(attack, release),
            hold: Param::new(hold),
            range_db: Param::new(Db(-80.0)),
            envelope: 0.0,
            follower: GateEnvelopeFollower::new(attack, hold, release, SampleRate::DEFAULT),
            last_range: None,
            good: [
                LastGood::new(threshold_db.get()),
                LastGood::new(-80.0),
                LastGood::new(attack.get()),
                LastGood::new(hold.get()),
                LastGood::new(release.get()),
            ],
        }
    }

    pub fn with_range(mut self, range_db: impl Into<Db>) -> Self {
        self.range_db = Param::new(Db(range_db.into().get().min(0.0)));
        self
    }

    pub fn is_open(&self) -> bool {
        self.follower.value() > 0.5
    }

    pub fn gate_level(&self) -> f32 {
        self.follower.value()
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.follower.reset();
        // Ramp history is state like the envelope; see `CompressorCore::reset`.
        self.last_range = None;
    }

    pub fn set_sample_rate(&mut self, sample_rate: impl Into<tutti_core::SampleRate>) {
        let (attack, hold, release) = self.times();
        self.follower
            .set_sample_rate(sample_rate, attack, hold, release);
    }

    #[inline]
    pub fn update_coefficients(&mut self) {
        let (attack, hold, release) = self.times();
        self.follower.update_coefficients(attack, hold, release);
    }

    /// Attack, hold and release, with non-finite writes held off.
    #[inline]
    fn times(&mut self) -> (Seconds, Seconds, Seconds) {
        (
            Seconds(self.good[2].read(self.timing.attack.load().get())),
            Seconds(self.good[3].read(self.hold.load().get())),
            Seconds(self.good[4].read(self.timing.release.load().get())),
        )
    }

    /// Read every block-rate control once, and move the range ramp's start to
    /// this block's end.
    #[inline]
    pub fn begin_block(&mut self) -> GateBlock {
        let range_to = Db(self.good[1].read(self.range_db.load().get()));
        let range_from = self.last_range.unwrap_or(range_to);
        self.last_range = Some(range_to);
        GateBlock {
            threshold: Db(self.good[0].read(self.threshold_db.load().get())),
            range_from,
            range_to,
        }
    }

    /// Gate gain (linear) for frame `i` of an `n`-frame block, given the
    /// sidechain level and an optional per-sample threshold override (dB).
    /// `None` uses the block's threshold (the fast path); `Some(db)` overrides
    /// it — the audio-rate modulation path, which stays per sample. No extra
    /// clamp — the setter stores threshold unclamped.
    #[inline]
    pub fn compute_gain(
        &mut self,
        block: &GateBlock,
        i: usize,
        n: usize,
        sc_level: f32,
        threshold_override: Option<Db>,
    ) -> f32 {
        let input_db = amplitude_to_db(sc_level);
        let threshold = threshold_override
            .filter(|t| t.get().is_finite())
            .unwrap_or(block.threshold);
        self.envelope = sc_level;
        self.follower.step(input_db >= threshold);
        let range = ramp_db(block.range_from, block.range_to, i, n);
        compute_gate_gain(self.follower.value(), range).get()
    }
}

/// GateNode with external sidechain. Channel-count is runtime-configurable:
/// `channels = N` means N audio inputs + N sidechain inputs + N outputs, with
/// a single linked gate level computed from the max-abs of the sidechain channels.
///
/// - `GateNode::mono(..)` — 2 inputs (audio + sidechain), 1 output.
/// - `GateNode::stereo(..)` — 4 inputs (L, R, SC-L, SC-R), 2 outputs, linked gate.
/// - `GateNode::with_channels(.., n)` — arbitrary N.
///
/// # Modulated threshold
///
/// The audio inputs (`0..ch`) come first, then the sidechain inputs
/// (`ch..2*ch`). The threshold is modulatable by the graph (design doc 013
/// item 6; [`GATE_PARAMS`]): a per-frame threshold in [`Db`] on the param
/// port ([`Io::param`](tutti_graph::Io::param)) overrides the threshold cell
/// per sample. Unmodulated, the node reads its cell once per block, which is
/// the common case; the arity never changes.
///
/// # In a graph
///
/// A native node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over threshold, attack and release, and a fork of it starts from the
/// values last set through that set. The graph prepares it at the device
/// rate before its first block.
pub struct GateNode {
    core: GateCore,
    channels: ChannelLayout,
    /// The per-frame gain lane, sized to the prepared `MaxBlock`.
    gains: Vec<f32>,
}

/// The params a [`GateNode`] lets the graph modulate, in port order.
pub const GATE_PARAMS: [UnitParam; 1] = [UnitParam::Threshold];

impl GateNode {
    /// Mono + mono sidechain: 2 inputs (audio, sidechain), 1 output.
    ///
    /// `threshold_db` is the sidechain level in [`Db`] at or above which the
    /// gate opens. `attack` is how fast it opens, `hold` how long it stays open
    /// after the signal falls back below the threshold, and `release` how fast
    /// it then closes — all [`Seconds`]. Hold is what stops a gate chattering
    /// on a signal hovering at the threshold.
    ///
    /// The closed floor is −80 dB; set it with [`with_range`](Self::with_range).
    ///
    /// The attack and release coefficients and the hold count are derived
    /// from the times at the rate [`Node::prepare`] hands it.
    pub fn mono(
        threshold_db: impl Into<Db>,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        Self::with_channels(threshold_db, attack, hold, release, 1)
    }

    /// Stereo + stereo sidechain: 4 inputs (L, R, SC-L, SC-R), 2 outputs.
    ///
    /// The gate is **linked** — one open/closed decision from the loudest
    /// sidechain channel, applied to both — so the two channels always gate
    /// together. Parameters are as [`mono`](Self::mono).
    pub fn stereo(
        threshold_db: impl Into<Db>,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        Self::with_channels(threshold_db, attack, hold, release, 2)
    }

    /// Arbitrary channel count, clamped to at least 1.
    ///
    /// `N` audio inputs, then `N` sidechain inputs, then `N` outputs, with one
    /// **linked** gate decision from the loudest sidechain channel. Parameters
    /// are as [`mono`](Self::mono).
    pub fn with_channels(
        threshold_db: impl Into<Db>,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
        channels: u8,
    ) -> Self {
        Self {
            core: GateCore::new(threshold_db, attack, hold, release),
            channels: ChannelLayout::from(channels.max(1) as u16),
            gains: Vec::new(),
        }
    }

    /// Sets how far the gate attenuates when closed, in [`Db`], clamped to at
    /// most `0.0`.
    ///
    /// This is a *floor*, not a mute: `-80.0` (the default) is effectively
    /// silent, while a gentler `-12.0` ducks the signal without removing it,
    /// which sounds more natural on drums and room mics. `0.0` disables the
    /// gate's effect entirely.
    pub fn with_range(mut self, range_db: impl Into<Db>) -> Self {
        self.core = self.core.with_range(range_db);
        self
    }

    /// The audio channel width this gate was built for.
    ///
    /// It has `2 * channels` inputs (audio then sidechain) and `channels`
    /// outputs, plus a threshold port if one was requested.
    pub fn channels(&self) -> u8 {
        self.channels.count() as u8
    }

    /// The width this gate was built for, as the engine's channel vocabulary.
    /// [`channels`](Self::channels) is the same number as a bare count, kept
    /// for callers doing port arithmetic.
    pub fn layout(&self) -> ChannelLayout {
        self.channels
    }

    /// The shared threshold cell in [`Db`] — the sidechain level at or above
    /// which the gate opens.
    ///
    /// **A threshold the graph feeds overrides this per sample.**
    /// Shared across clones.
    pub fn threshold(&self) -> Arc<AtomicF32> {
        self.core.threshold_db.as_atomic()
    }

    /// The shared attack-time cell in [`Seconds`] — how fast the gate opens
    /// once the sidechain crosses the threshold.
    ///
    /// Very short attacks can click on low-frequency material; longer ones
    /// soften the onset.
    pub fn attack_time(&self) -> Arc<AtomicF32> {
        self.core.timing.attack.as_atomic()
    }

    /// The shared hold-time cell in [`Seconds`] — how long the gate stays fully
    /// open after the sidechain falls back below the threshold.
    ///
    /// This is what stops chatter on a signal hovering at the threshold. The
    /// release only begins once hold expires.
    pub fn hold_time(&self) -> Arc<AtomicF32> {
        self.core.hold.as_atomic()
    }

    /// The shared release-time cell in [`Seconds`] — how fast the gate closes
    /// after the hold expires.
    pub fn release_time(&self) -> Arc<AtomicF32> {
        self.core.timing.release.as_atomic()
    }

    /// The shared range cell in [`Db`] — the attenuation floor when closed.
    ///
    /// At most `0.0`; `-80.0` is effectively silent, gentler values duck rather
    /// than mute.
    pub fn range(&self) -> Arc<AtomicF32> {
        self.core.range_db.as_atomic()
    }

    /// Whether the gate is currently more than half open — a **measurement**,
    /// for driving an open/closed indicator.
    ///
    /// A threshold over [`gate_level`](Self::gate_level), so it flips partway
    /// through the attack and release rather than at their edges.
    pub fn is_open(&self) -> bool {
        self.core.is_open()
    }

    /// The gate's open fraction, `0.0` (fully closed) to `1.0` (fully open) — a
    /// **measurement**, for driving a gate indicator.
    ///
    /// Unitless, and deliberately not an `Amplitude`: it is how far through its
    /// envelope the gate is, not a signal level. The applied attenuation is
    /// this fraction scaled into the range.
    pub fn gate_level(&self) -> f32 {
        self.core.gate_level()
    }

    /// Sets the threshold in [`Db`], unclamped.
    ///
    /// While the graph modulates the threshold this sets the *base* its
    /// modulation rides on.
    pub fn set_threshold(&self, db: impl Into<Db>) {
        self.core.threshold_db.store(db.into());
    }

    /// Sets the attack time in [`Seconds`], floored at 0.
    pub fn set_attack(&self, seconds: impl Into<Seconds>) {
        self.core
            .timing
            .attack
            .store(Seconds(seconds.into().get().max(0.0)));
    }

    /// Sets the release time in [`Seconds`], floored at 0.
    ///
    /// Takes effect only after the hold time expires.
    pub fn set_release(&self, seconds: impl Into<Seconds>) {
        self.core
            .timing
            .release
            .store(Seconds(seconds.into().get().max(0.0)));
    }
}

impl Node for GateNode {
    /// `2 * channels` in (audio, then sidechain), `channels` out, the
    /// threshold modulatable ([`GATE_PARAMS`]).
    ///
    /// No tail: the release envelope decays after the input goes silent,
    /// but it only scales (`output = input * gain`), so a silent input is a
    /// silent output whatever the envelope is doing.
    fn shape(&self) -> Shape {
        let n = self.channels.count();
        Shape::audio(ChannelLayout::from_count(2 * n), self.channels)
            .with_tail(Tail::None)
            .with_params(&GATE_PARAMS)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.core.set_sample_rate(p.sample_rate());
        self.gains = vec![0.0; p.max_block().get()];
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        self.core.update_coefficients();
        let block = self.core.begin_block();
        let ch = self.channels.count() as usize;
        // A modulated threshold overrides the cell per sample; the cell is
        // the base the graph's modulation rides on.
        let threshold_fed = io.param(0).frames();
        let (inputs, mut outputs) = io.split();

        // Detector sample-outer (a recursive envelope with a hold counter),
        // apply channel-outer over planar slices — see `CompressorNode::process`.
        let gains = &mut self.gains[..size];
        for (i, g) in gains.iter_mut().enumerate() {
            let sc = sidechain_level(&inputs, ch, i);
            let threshold = threshold_fed.map(|v| Db(v[i]));
            *g = self.core.compute_gain(&block, i, size, sc, threshold);
        }
        apply_gain_lane(gains, ch, &inputs, &mut outputs);
        Status::Modified
    }

    fn reset(&mut self) {
        self.core.reset();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        (k == 0).then(|| self.core.threshold_db.load().get())
    }
}

impl ParamNode for GateNode {
    /// Threshold, attack and release: the addresses `set(Setting)` took.
    /// Hold and range are reached through their cells.
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Threshold, self.threshold())
            .param(UnitParam::Attack, self.attack_time())
            .param(UnitParam::Release, self.release_time())
            .build()
    }

    /// A clone with every control cell detached (hold and range too), its
    /// envelope cleared.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.core.threshold_db.detach();
        fork.core.timing.detach();
        fork.core.hold.detach();
        fork.core.range_db.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for GateNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

/// Shares the control cells (the template [`tutti_graph::param_parts`]
/// forks from); the detector state is copied.
impl Clone for GateNode {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
            channels: self.channels,
            gains: self.gains.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{prepared_at, tick, tick_fed, RATE_44K};
    use core::sync::atomic::Ordering;
    use tutti_graph::contract::{assert_param_fork, drive};

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it.
    ///
    /// Mutation (run): drop `fork.core.timing.detach()` in `fork_fresh` →
    /// "a live write reached the fork" for `Attack` → fails.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(GateNode::stereo(-30.0, 0.001, 0.01, 0.1));
    }

    /// A block longer than the old 64-frame stack lane renders whole: the
    /// gain lane is sized from the prepared `MaxBlock`.
    ///
    /// Mutation (run): size the lane `vec![0.0; 64]` in `prepare` → the
    /// 1024-frame block panics indexing past it → fails.
    #[test]
    fn a_block_up_to_the_prepared_maximum_renders() {
        let mut gate = prepared_at(GateNode::mono(-20.0, 0.0001, 0.01, 0.1), RATE_44K);
        let audio = vec![0.5f32; 1024];
        let sc = vec![0.9f32; 1024];
        let out = drive(&mut gate, RATE_44K, &[&audio, &sc], &[]).remove(0);
        assert!(out[1023] > 0.3, "the loud sidechain opened the gate");
    }

    #[test]
    fn test_gate_starts_closed() {
        let gate = GateNode::mono(-30.0, 0.001, 0.01, 0.1);
        assert!(!gate.is_open());
        assert_eq!(gate.gate_level(), 0.0);
    }

    #[test]
    fn test_gate_opens_on_loud_sidechain() {
        let mut gate = prepared_at(GateNode::mono(-20.0, 0.0001, 0.01, 0.1), RATE_44K);

        let mut output = [0.0f32];

        for _ in 0..500 {
            tick(&mut gate, &[0.5, 0.9], &mut output);
        }

        assert!(gate.is_open());
        assert!(output[0] > 0.3);
    }

    #[test]
    fn test_gate_closes_on_quiet_sidechain() {
        let mut gate = prepared_at(
            GateNode::mono(-20.0, 0.001, 0.001, 0.001).with_range(-60.0),
            RATE_44K,
        );

        let mut output = [0.0f32];

        for _ in 0..500 {
            tick(&mut gate, &[0.5, 0.9], &mut output);
        }
        assert!(gate.is_open());

        for _ in 0..2000 {
            tick(&mut gate, &[0.5, 0.01], &mut output);
        }

        assert!(!gate.is_open());
        assert!(output[0] < 0.1);
    }

    #[test]
    fn test_gate_range_clamps_to_non_positive() {
        let gate = GateNode::mono(-20.0, 0.001, 0.01, 0.1).with_range(10.0);
        assert_eq!(gate.range().load(Ordering::Acquire), 0.0);
    }

    #[test]
    fn test_gate_range_attenuates_rather_than_mutes() {
        let mut gate = prepared_at(
            GateNode::mono(-20.0, 0.001, 0.001, 0.001).with_range(-12.0),
            RATE_44K,
        );

        let mut output = [0.0f32];

        for _ in 0..2000 {
            tick(&mut gate, &[0.5, 0.01], &mut output);
        }

        assert!(!gate.is_open());
        assert!(
            output[0] > 0.05,
            "With -12dB range, signal should be attenuated not muted: {}",
            output[0]
        );
        assert!(output[0] < 0.5);
    }

    #[test]
    fn test_gate_reset() {
        let mut gate = prepared_at(GateNode::mono(-20.0, 0.0001, 0.01, 0.1), RATE_44K);

        let mut output = [0.0f32];
        for _ in 0..500 {
            tick(&mut gate, &[0.5, 0.9], &mut output);
        }
        assert!(gate.is_open());

        gate.reset();
        assert!(!gate.is_open());
        assert_eq!(gate.gate_level(), 0.0);
    }

    #[test]
    fn test_gate_stereo_opens_on_loud_sidechain() {
        let mut gate = prepared_at(GateNode::stereo(-20.0, 0.0001, 0.01, 0.1), RATE_44K);

        let mut output = [0.0f32; 2];

        for _ in 0..500 {
            tick(&mut gate, &[0.5, 0.4, 0.9, 0.9], &mut output);
        }

        assert!(gate.is_open());
        assert!(output[0] > 0.3);
        assert!(output[1] > 0.2);
    }

    #[test]
    fn test_gate_stereo_channel_count() {
        let mono = GateNode::mono(-20.0, 0.001, 0.01, 0.1);
        assert_eq!(mono.channels(), 1);
        assert_eq!(mono.shape().audio_in.count(), 2);
        assert_eq!(mono.shape().audio_out.count(), 1);

        let stereo = GateNode::stereo(-20.0, 0.001, 0.01, 0.1);
        assert_eq!(stereo.channels(), 2);
        assert_eq!(stereo.shape().audio_in.count(), 4);
        assert_eq!(stereo.shape().audio_out.count(), 2);
    }

    #[test]
    fn test_gate_stereo_linking() {
        let mut gate = prepared_at(GateNode::stereo(-20.0, 0.0001, 0.01, 0.1), RATE_44K);

        let mut output = [0.0f32; 2];

        for _ in 0..500 {
            tick(&mut gate, &[0.8, 0.3, 0.9, 0.9], &mut output);
        }

        let ratio = output[1] / output[0];
        assert!(
            (ratio - 0.3 / 0.8).abs() < 0.15,
            "Both channels should have same gate gain, ratio: {}",
            ratio
        );
    }

    // ── Modulated threshold (the graph's param feed) ────────────────────────

    /// The shape declares the threshold, and it never changes the arity:
    /// audio plus sidechain, at every width.
    ///
    /// Mutation (run): declare no params in `shape` → the first assertion
    /// fails.
    #[test]
    fn gate_declares_its_threshold_param() {
        let m = GateNode::mono(-20.0, 0.0001, 0.01, 0.1);
        assert_eq!(m.shape().params.as_slice(), &[UnitParam::Threshold][..]);
        assert_eq!(m.shape().audio_in.count(), 2);
        assert_eq!(
            m.param_base(0),
            Some(-20.0),
            "the base is the threshold control"
        );
        let s = GateNode::stereo(-20.0, 0.0001, 0.01, 0.1);
        assert_eq!(s.shape().audio_in.count(), 4);
    }

    #[test]
    fn gate_unmodulated_matches_held_constant() {
        // A modulated mono gate whose fed threshold is held at the same value
        // as a plain gate's atomic must produce identical output.
        let mut plain = prepared_at(GateNode::mono(-20.0, 0.0001, 0.01, 0.1), RATE_44K);

        let mut modn = prepared_at(GateNode::mono(-20.0, 0.0001, 0.01, 0.1), RATE_44K);

        let mut plain_out = [0.0f32];
        let mut mod_out = [0.0f32];
        for n in 0..1000 {
            let audio = 0.5;
            // Alternate loud/quiet sidechain to exercise open + close.
            let sc = if n % 200 < 100 { 0.9 } else { 0.01 };
            tick(&mut plain, &[audio, sc], &mut plain_out);
            tick_fed(&mut modn, &[audio, sc], &[Some(-20.0)], &mut mod_out);
            assert!(
                (plain_out[0] - mod_out[0]).abs() < 1e-6,
                "modulated-held output diverges from plain at sample {n}: {} vs {}",
                plain_out[0],
                mod_out[0]
            );
        }
    }
}
