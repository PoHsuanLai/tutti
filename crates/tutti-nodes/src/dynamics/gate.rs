//! Noise gate with external sidechain: attenuates below a threshold.

use std::sync::Arc;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{AtomicF32, AtomicF64};
use tutti_types::{ChannelLayout, UnitParam};

use super::params::AttackRelease;
use super::utils::{apply_gain_lane, sidechain_levels};
use tutti_dsp::{GateBlock, GateKernel};
use tutti_types::{Db, Param, SampleRate, Seconds, Tail};

use crate::ramp::{LastGood, block_driven};

/// Shared gate state used by the per-sample gain computation.
#[derive(Clone, Debug)]
pub(super) struct GateCore {
    pub threshold_db: Param<Db>,
    pub timing: AttackRelease,
    pub hold: Param<Seconds>,
    pub range_db: Param<Db>,

    kernel: GateKernel,
    /// The range the previous block ended on, the start of this block's ramp.
    /// `None` until the first block, which starts on its own value.
    last_range: Option<Db>,
    /// Last finite threshold, range, attack, hold and release. The times
    /// become the follower's coefficients and hold count, and the range the
    /// start of every later ramp, so a non-finite write reads as unchanged
    /// (see [`LastGood`]).
    good: [LastGood; 5],
}

impl GateCore {
    /// Builds the shared core. The follower is seeded at the placeholder
    /// [`SampleRate::DEFAULT`]; `GateNode`'s `prepare` retunes it from
    /// `timing` and `hold`, which is why those are kept as [`Seconds`] rather
    /// than only as coefficients and a sample count — the seconds are the
    /// recoverable form, the derived values are not.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_types::SampleRate::DEFAULT
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
            kernel: GateKernel::new(attack, hold, release, SampleRate::DEFAULT),
            last_range: None,
            good: [
                LastGood::new(threshold_db.get(), 0.0),
                LastGood::fixed(-80.0),
                LastGood::new(attack.to_f32(), 0.0),
                LastGood::new(hold.to_f32(), 0.0),
                LastGood::new(release.to_f32(), 0.0),
            ],
        }
    }

    pub fn with_range(mut self, range_db: impl Into<Db>) -> Self {
        self.range_db = Param::new(Db(range_db.into().get().min(0.0)));
        self
    }

    pub fn is_open(&self) -> bool {
        self.kernel.is_open()
    }

    pub fn gate_level(&self) -> f32 {
        self.kernel.gate_level()
    }

    pub fn reset(&mut self) {
        self.kernel.reset();
        // Ramp history is state like the envelope; see `CompressorCore::reset`.
        self.last_range = None;
    }

    pub fn set_sample_rate(&mut self, sample_rate: impl Into<tutti_types::SampleRate>) {
        let (attack, hold, release) = self.times([None, None]);
        self.kernel
            .set_sample_rate(sample_rate, attack, hold, release);
    }

    /// `driven` is the block's attack and release from the node's param
    /// ports where a lane drives them; `None` reads the cell.
    #[inline]
    pub fn update_coefficients(&mut self, driven: [Option<f32>; 2]) {
        let (attack, hold, release) = self.times(driven);
        self.kernel.update_coefficients(attack, hold, release);
    }

    /// Attack, hold and release, with non-finite writes held off; attack
    /// and release floored as the setters floor them
    /// ([`AttackRelease::clamp_time`]), driven or not.
    #[inline]
    fn times(&mut self, [attack, release]: [Option<f32>; 2]) -> (Seconds, Seconds, Seconds) {
        (
            Seconds::from_f32(AttackRelease::clamp_time(
                self.good[2].read(attack.unwrap_or_else(|| self.timing.attack.load().to_f32())),
            )),
            Seconds::from_f32(self.good[3].read(self.hold.load().to_f32())),
            Seconds::from_f32(AttackRelease::clamp_time(
                self.good[4].read(release.unwrap_or_else(|| self.timing.release.load().to_f32())),
            )),
        )
    }

    /// Reads every block-rate control once, and moves the range ramp's start to
    /// this block's end.
    #[inline]
    pub fn begin_block(&mut self) -> GateBlock {
        let range_to = Db(self.good[1].read(self.range_db.load().get()));
        let range_from = self.last_range.unwrap_or(range_to);
        self.last_range = Some(range_to);
        let threshold = Db(self.good[0].read(self.threshold_db.load().get()));
        GateBlock::new(threshold, range_from, range_to)
    }

    /// Gate gain (linear) for frame `i` of an `n`-frame block; see
    /// [`GateKernel::gain`].
    #[inline]
    pub fn gain(
        &mut self,
        block: &GateBlock,
        i: usize,
        n: usize,
        sc_level: f32,
        threshold_override: Option<Db>,
    ) -> f32 {
        self.kernel.gain(block, i, n, sc_level, threshold_override)
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
/// (`ch..2*ch`). The threshold is modulatable by the graph
/// ([`GATE_PARAMS`]): a per-frame threshold in [`Db`] on the param
/// port ([`Io::param`](tutti_graph::Io::param)) overrides the threshold cell
/// per sample. Unmodulated, the node reads its cell once per block, which is
/// the common case; the arity never changes.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over threshold, attack and release, and a fork of it starts from the
/// values last set through that set. The graph prepares it at the device
/// rate before its first block.
pub struct GateNode {
    core: GateCore,
    channels: ChannelLayout,
    /// The per-frame gain lane, sized to the prepared `MaxBlock`.
    gains: Vec<f32>,
}

impl std::fmt::Debug for GateNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateNode")
            .field("core", &self.core)
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

/// The params a [`GateNode`] lets the graph modulate, in port order.
pub const GATE_PARAMS: [UnitParam; 3] =
    [UnitParam::Threshold, UnitParam::Attack, UnitParam::Release];

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
            channels: ChannelLayout::from(channels.max(1)),
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
    #[must_use]
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
    pub fn attack_time(&self) -> Arc<AtomicF64> {
        self.core.timing.attack.as_atomic()
    }

    /// The shared hold-time cell in [`Seconds`] — how long the gate stays fully
    /// open after the sidechain falls back below the threshold.
    ///
    /// This is what stops chatter on a signal hovering at the threshold. The
    /// release only begins once hold expires.
    pub fn hold_time(&self) -> Arc<AtomicF64> {
        self.core.hold.as_atomic()
    }

    /// The shared release-time cell in [`Seconds`] — how fast the gate closes
    /// after the hold expires.
    pub fn release_time(&self) -> Arc<AtomicF64> {
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
    #[must_use]
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
            .store(AttackRelease::floor(seconds.into()));
    }

    /// Sets the release time in [`Seconds`], floored at 0.
    ///
    /// Takes effect only after the hold time expires.
    pub fn set_release(&self, seconds: impl Into<Seconds>) {
        self.core
            .timing
            .release
            .store(AttackRelease::floor(seconds.into()));
    }
}

impl Node for GateNode {
    /// `2 * channels` in (audio, then sidechain), `channels` out, the
    /// threshold, attack and release modulatable ([`GATE_PARAMS`]).
    ///
    /// No tail: the release envelope decays after the input goes silent,
    /// but it only scales (`output = input * gain`), so a silent input is a
    /// silent output whatever the envelope is doing.
    fn shape(&self) -> Shape {
        let n = self.channels.count();
        Shape::audio(ChannelLayout::from_count(2 * n), self.channels)
            .with_cost(tutti_graph::Cost::Moderate)
            .with_tail(Tail::None)
            .with_params(&GATE_PARAMS)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.core.set_sample_rate(p.sample_rate());
        self.gains = vec![0.0; p.max_block().get()];
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        self.core
            .update_coefficients([block_driven(&io, 1), block_driven(&io, 2)]);
        let block = self.core.begin_block();
        let ch = self.channels.count() as usize;
        // A modulated threshold overrides the cell per sample; the cell is
        // the base the graph's modulation rides on.
        let threshold_fed = io.param(0).frames();
        let (inputs, mut outputs) = io.split();

        // Detector sample-outer (a recursive envelope with a hold counter),
        // apply channel-outer over planar slices — see `CompressorNode::process`.
        let gains = &mut self.gains[..size];
        sidechain_levels(&inputs, ch, gains);
        for (i, g) in gains.iter_mut().enumerate() {
            let sc = *g;
            let threshold = threshold_fed.map(|v| Db(v[i]));
            *g = self.core.gain(&block, i, size, sc, threshold);
        }
        apply_gain_lane(gains, ch, &inputs, &mut outputs);
        Status::Modified
    }

    fn reset(&mut self) {
        self.core.reset();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.core.threshold_db.load().get()),
            1 => Some(self.core.timing.attack.load().to_f32()),
            2 => Some(self.core.timing.release.load().to_f32()),
            _ => None,
        }
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
    use crate::test_support::{RATE_44K, prepared_at, tick, tick_fed};
    use core::sync::atomic::Ordering;
    use tutti_graph::contract::{assert_param_fork, drive};

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it.
    ///
    /// Mutation (run): drop `fork.core.timing.detach()` in `fork_fresh` →
    /// "a live write reached the fork" for `Attack` → fails.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(GateNode::stereo(
            Db::new(-30.0),
            Seconds::new(0.001),
            Seconds::new(0.01),
            Seconds::new(0.1),
        ));
    }

    /// A block longer than 64 frames renders whole: the
    /// gain lane is sized from the prepared `MaxBlock`.
    ///
    /// Mutation (run): size the lane `vec![0.0; 64]` in `prepare` → the
    /// 1024-frame block panics indexing past it → fails.
    #[test]
    fn a_block_up_to_the_prepared_maximum_renders() {
        let mut gate = prepared_at(
            GateNode::mono(
                Db::new(-20.0),
                Seconds::new(0.0001),
                Seconds::new(0.01),
                Seconds::new(0.1),
            ),
            RATE_44K,
        );
        let audio = vec![0.5f32; 1024];
        let sc = vec![0.9f32; 1024];
        let out = drive(&mut gate, RATE_44K, &[&audio, &sc], &[]).remove(0);
        assert!(out[1023] > 0.3, "the loud sidechain opened the gate");
    }

    #[test]
    fn test_gate_starts_closed() {
        let gate = GateNode::mono(
            Db::new(-30.0),
            Seconds::new(0.001),
            Seconds::new(0.01),
            Seconds::new(0.1),
        );
        assert!(!gate.is_open());
        assert_eq!(gate.gate_level(), 0.0);
    }

    #[test]
    fn test_gate_opens_on_loud_sidechain() {
        let mut gate = prepared_at(
            GateNode::mono(
                Db::new(-20.0),
                Seconds::new(0.0001),
                Seconds::new(0.01),
                Seconds::new(0.1),
            ),
            RATE_44K,
        );

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
            GateNode::mono(
                Db::new(-20.0),
                Seconds::new(0.001),
                Seconds::new(0.001),
                Seconds::new(0.001),
            )
            .with_range(Db::new(-60.0)),
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
        let gate = GateNode::mono(
            Db::new(-20.0),
            Seconds::new(0.001),
            Seconds::new(0.01),
            Seconds::new(0.1),
        )
        .with_range(Db::new(10.0));
        assert_eq!(gate.range().load(Ordering::Acquire), 0.0);
    }

    #[test]
    fn test_gate_range_attenuates_rather_than_mutes() {
        let mut gate = prepared_at(
            GateNode::mono(
                Db::new(-20.0),
                Seconds::new(0.001),
                Seconds::new(0.001),
                Seconds::new(0.001),
            )
            .with_range(Db::new(-12.0)),
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
        let mut gate = prepared_at(
            GateNode::mono(
                Db::new(-20.0),
                Seconds::new(0.0001),
                Seconds::new(0.01),
                Seconds::new(0.1),
            ),
            RATE_44K,
        );

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
        let mut gate = prepared_at(
            GateNode::stereo(
                Db::new(-20.0),
                Seconds::new(0.0001),
                Seconds::new(0.01),
                Seconds::new(0.1),
            ),
            RATE_44K,
        );

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
        let mono = GateNode::mono(
            Db::new(-20.0),
            Seconds::new(0.001),
            Seconds::new(0.01),
            Seconds::new(0.1),
        );
        assert_eq!(mono.channels(), 1);
        assert_eq!(mono.shape().audio_in.count(), 2);
        assert_eq!(mono.shape().audio_out.count(), 1);

        let stereo = GateNode::stereo(
            Db::new(-20.0),
            Seconds::new(0.001),
            Seconds::new(0.01),
            Seconds::new(0.1),
        );
        assert_eq!(stereo.channels(), 2);
        assert_eq!(stereo.shape().audio_in.count(), 4);
        assert_eq!(stereo.shape().audio_out.count(), 2);
    }

    #[test]
    fn test_gate_stereo_linking() {
        let mut gate = prepared_at(
            GateNode::stereo(
                Db::new(-20.0),
                Seconds::new(0.0001),
                Seconds::new(0.01),
                Seconds::new(0.1),
            ),
            RATE_44K,
        );

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

    /// The shape declares the threshold, attack and release, and it never
    /// changes the arity: audio plus sidechain, at every width. (Doc 014
    /// §12.6 widened the list: a route onto any of these was refused once
    /// the frame-rate driver went.)
    ///
    /// Mutation (run): declare no params in `shape` → the first assertion
    /// fails. Mutation (run): `param_base(2)` answering the attack
    /// cell → fails.
    #[test]
    fn gate_declares_its_params() {
        let m = GateNode::mono(
            Db::new(-20.0),
            Seconds::new(0.0001),
            Seconds::new(0.01),
            Seconds::new(0.1),
        );
        assert_eq!(
            m.shape().params.as_slice(),
            &[UnitParam::Threshold, UnitParam::Attack, UnitParam::Release][..]
        );
        assert_eq!(m.shape().audio_in.count(), 2);
        assert_eq!(
            m.param_base(0),
            Some(-20.0),
            "the base is the threshold control"
        );
        assert_eq!(m.param_base(1), Some(0.0001), "the attack's base");
        assert_eq!(m.param_base(2), Some(0.1), "the release's base");
        let s = GateNode::stereo(
            Db::new(-20.0),
            Seconds::new(0.0001),
            Seconds::new(0.01),
            Seconds::new(0.1),
        );
        assert_eq!(s.shape().audio_in.count(), 4);
    }

    // ── The threshold compares in the linear domain ─────────────────────────

    /// A mono gate at `-20 dB` (0.1 linear), 44.1 kHz, fast attack, no range
    /// floor beyond the default.
    fn gate_at_minus_20() -> GateNode {
        prepared_at(
            GateNode::mono(
                Db::new(-20.0),
                Seconds::new(0.0001),
                Seconds::new(0.0102),
                Seconds::new(0.001),
            ),
            RATE_44K,
        )
    }

    /// Renders `frames` of a steady sidechain `sc` (audio 1.0) in one block.
    fn run_steady(gate: &mut GateNode, sc: f32, frames: usize) -> Vec<f32> {
        let audio = vec![1.0f32; frames];
        let side = vec![sc; frames];
        drive(gate, RATE_44K, &[&audio, &side], &[]).remove(0)
    }

    /// A steady sidechain a hair above the threshold opens the gate, a hair
    /// below leaves it closed. The margin is 0.1 percent (0.009 dB), far
    /// outside the rounding the comparison carries.
    ///
    /// Mutation (run): compare `sc_level >= block.threshold_linear * 1.01` →
    /// the "above" half fails; `* 0.99` → the "below" half fails.
    #[test]
    fn a_steady_level_just_either_side_of_the_threshold_opens_or_closes_the_gate() {
        let thr = 0.1f32; // -20 dB
        let mut above = gate_at_minus_20();
        run_steady(&mut above, thr * 1.001, 1024);
        assert!(above.is_open(), "just above the threshold must open");
        assert!(above.gate_level() > 0.999);

        let mut below = gate_at_minus_20();
        run_steady(&mut below, thr * 0.999, 1024);
        assert!(
            !below.is_open(),
            "just below the threshold must stay closed"
        );
        assert_eq!(below.gate_level(), 0.0);
    }

    /// A threshold above 0 dB compares the same way: at `+6 dB` (1.995
    /// linear) a steady level 0.1 percent over opens the gate and 0.1 percent
    /// under leaves it closed.
    ///
    /// Mutation (run): clamp `threshold_linear` to at most 1.0 in
    /// `begin_block` (`db_to_amplitude(threshold).get().min(1.0)`) → the
    /// "below" half opens the gate and fails.
    #[test]
    fn a_threshold_above_zero_db_opens_above_and_stays_closed_below() {
        let thr = 10f32.powf(6.0 / 20.0);
        let at_plus_6 = || {
            prepared_at(
                GateNode::mono(
                    Db::new(6.0),
                    Seconds::new(0.0001),
                    Seconds::new(0.0102),
                    Seconds::new(0.001),
                ),
                RATE_44K,
            )
        };

        let mut above = at_plus_6();
        run_steady(&mut above, thr * 1.001, 1024);
        assert!(above.is_open(), "just above +6 dB must open");

        let mut below = at_plus_6();
        run_steady(&mut below, thr * 0.999, 1024);
        assert!(!below.is_open(), "just below +6 dB must stay closed");
        assert_eq!(below.gate_level(), 0.0);
    }

    /// The hold keeps an opened gate fully open for exactly `floor(hold *
    /// rate)` frames after the level falls under the threshold; the release
    /// starts on the next one.
    ///
    /// Mutation (run): reload the counter with `hold_samples + 1` in
    /// `GateEnvelopeFollower::step` → the gate still holds on the 450th frame
    /// and the last assertion fails.
    #[test]
    fn the_hold_keeps_the_gate_open_for_its_whole_length() {
        let mut gate = gate_at_minus_20();
        run_steady(&mut gate, 0.5, 1024);
        let open = gate.gate_level();
        assert!(open > 0.999);
        // 0.0102 s at 44.1 kHz is 449.82 frames, floored.
        run_steady(&mut gate, 0.001, 449);
        assert_eq!(gate.gate_level(), open, "held for the whole hold");
        run_steady(&mut gate, 0.001, 1);
        assert!(
            gate.gate_level() < open,
            "the release starts after the hold"
        );
    }

    /// Closed, the gain is the range in dB as a linear factor: `10^(dB/20)`.
    ///
    /// Mutation (run): build `range_to_linear` from `range_to * 0.5` dB →
    /// every range misses by far more than the tolerance.
    #[test]
    fn closed_the_gain_is_the_range_in_decibels() {
        for (range_db, linear) in [
            (-12.0f32, 0.251_188_64f32),
            (-80.0, 1.0e-4),
            (-6.0, 0.501_187_2),
        ] {
            let mut gate = prepared_at(
                GateNode::mono(
                    Db::new(-20.0),
                    Seconds::new(0.001),
                    Seconds::new(0.001),
                    Seconds::new(0.001),
                )
                .with_range(Db::new(range_db)),
                RATE_44K,
            );
            // Silent sidechain: closed from the first frame; a second block
            // runs on the settled (held) range.
            run_steady(&mut gate, 0.0, 256);
            let out = run_steady(&mut gate, 0.0, 256);
            for y in out {
                assert!(
                    (y - linear).abs() <= linear * 1e-5,
                    "{range_db} dB: gain {y}, expected {linear}"
                );
            }
        }
    }

    /// Silence is pinned at [`Db::FLOOR`] (-144 dB) for the comparison, so a
    /// threshold at or under the floor opens on a silent sidechain and one
    /// above it does not. In the linear domain a silent `0.0` is under every
    /// positive threshold, so this is carried by `silence_opens`.
    ///
    /// Mutation (run): build the block with `silence_opens: false` → the
    /// `-200 dB` and `-144 dB` cases stay closed and fail.
    #[test]
    fn a_threshold_at_or_under_the_floor_opens_on_silence() {
        for (threshold, opens) in [(-200.0f32, true), (-144.0, true), (-143.0, false)] {
            let mut gate = prepared_at(
                GateNode::mono(
                    Db::new(threshold),
                    Seconds::new(0.0001),
                    Seconds::new(0.01),
                    Seconds::new(0.1),
                ),
                RATE_44K,
            );
            run_steady(&mut gate, 0.0, 1024);
            assert_eq!(gate.is_open(), opens, "threshold {threshold} dB on silence");
        }
        // A level well above a floor-pinned threshold still decides on its
        // own: -150 dB is 3.2e-8, so 1e-10 (-200 dB) is below it.
        let mut gate = prepared_at(
            GateNode::mono(
                Db::new(-150.0),
                Seconds::new(0.0001),
                Seconds::new(0.01),
                Seconds::new(0.1),
            ),
            RATE_44K,
        );
        run_steady(&mut gate, 1.0e-10, 1024);
        assert!(!gate.is_open(), "-200 dB is under a -150 dB threshold");
        run_steady(&mut gate, 1.0e-7, 1024);
        assert!(gate.is_open(), "-140 dB is over a -150 dB threshold");
    }

    /// A threshold written between blocks is converted again for the next
    /// block: a level that was under it is over the lowered one.
    ///
    /// Mutation (run): build `threshold_linear` from a fixed `-20 dB` rather
    /// than the cell in `begin_block` → the lowered threshold never opens →
    /// fails.
    #[test]
    fn a_changed_threshold_applies_on_the_next_block() {
        let mut gate = gate_at_minus_20();
        run_steady(&mut gate, 0.05, 512); // -26 dB
        assert!(!gate.is_open());
        gate.set_threshold(Db::new(-30.0));
        run_steady(&mut gate, 0.05, 512);
        assert!(gate.is_open());
        gate.set_threshold(Db::new(-20.0));
        run_steady(&mut gate, 0.05, 1024);
        assert!(!gate.is_open(), "raising it closes the gate again");
    }

    /// A NaN detector level never makes the gate's state NaN: it compares as
    /// not over the threshold (closed), and the gate recovers on the next
    /// finite level. An infinite level is over every finite threshold.
    ///
    /// Mutation (run): in `gain` open the gate on
    /// `!(sc_level < block.threshold_linear)` → a NaN level opens it and the
    /// `gate_level() == 0.0` assertion fails.
    #[test]
    fn a_nan_detector_level_leaves_the_state_finite_and_closed() {
        let mut core = GateCore::new(
            Db::new(-20.0),
            Seconds::new(0.0001),
            Seconds::new(0.01),
            Seconds::new(0.1),
        );
        core.set_sample_rate(RATE_44K);
        core.update_coefficients([None, None]);
        let block = core.begin_block();
        for i in 0..64 {
            let g = core.gain(&block, i, 64, f32::NAN, None);
            assert!(g.is_finite(), "gain {g} at {i}");
            assert_eq!(core.gate_level(), 0.0);
        }
        let block = core.begin_block();
        for i in 0..2048 {
            core.gain(&block, i % 64, 64, 0.5, None);
        }
        assert!(core.is_open(), "recovers on a finite level");
        let g = core.gain(&block, 0, 64, f32::INFINITY, None);
        assert!(g.is_finite() && core.gate_level().is_finite());
    }

    /// Through the node: NaN on the sidechain is ignored by the max-abs
    /// detector (reads as silent), so the closed gain applies and the gate's
    /// own state stays finite. Audio NaN passes through, as a gain does.
    #[test]
    fn a_nan_sidechain_block_does_not_poison_the_gate() {
        let mut gate = gate_at_minus_20();
        let audio = vec![1.0f32; 64];
        let nan = vec![f32::NAN; 64];
        let out = drive(&mut gate, RATE_44K, &[&audio, &nan], &[]).remove(0);
        assert!(out.iter().all(|y| y.is_finite()));
        assert_eq!(gate.gate_level(), 0.0);
        run_steady(&mut gate, 0.5, 1024);
        assert!(gate.is_open());
    }

    #[test]
    fn gate_unmodulated_matches_held_constant() {
        // A modulated mono gate whose fed threshold is held at the same value
        // as a plain gate's atomic must produce identical output.
        let mut plain = prepared_at(
            GateNode::mono(
                Db::new(-20.0),
                Seconds::new(0.0001),
                Seconds::new(0.01),
                Seconds::new(0.1),
            ),
            RATE_44K,
        );

        let mut modn = prepared_at(
            GateNode::mono(
                Db::new(-20.0),
                Seconds::new(0.0001),
                Seconds::new(0.01),
                Seconds::new(0.1),
            ),
            RATE_44K,
        );

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
