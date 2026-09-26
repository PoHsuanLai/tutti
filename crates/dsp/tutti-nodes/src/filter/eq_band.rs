//! Parametric EQ band: an SVF plus a zero-cost bypass.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::Real;
use tutti_core::{ChannelLayout, Db, Hz, Q};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};

use super::svf::{SvfFilterNode, SvfType};

/// Enable/bypass state for an EQ band. Replaces a raw `bool` so callers
/// cannot confuse "active" with unrelated boolean flags, and so the two
/// modes have names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BandState {
    /// The band filters its input. The default.
    #[default]
    Active,
    /// The band passes its input through untouched, at zero DSP cost.
    ///
    /// Filter state is retained while bypassed, so re-enabling resumes from
    /// stale integrator contents — reset the node first if that matters.
    Bypassed,
}

impl BandState {
    /// Returns whether the band is [`Active`](Self::Active) — i.e. whether it
    /// filters rather than passing through.
    #[inline]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Maps a plain `enabled` flag onto the two states: `true` is
    /// [`Active`](Self::Active), `false` [`Bypassed`](Self::Bypassed).
    #[inline]
    pub fn from_enabled(enabled: bool) -> Self {
        if enabled {
            Self::Active
        } else {
            Self::Bypassed
        }
    }
}

impl From<bool> for BandState {
    #[inline]
    fn from(enabled: bool) -> Self {
        Self::from_enabled(enabled)
    }
}

/// Parametric EQ band: an [`SvfFilterNode`] plus a zero-cost bypass. 1 input, 1
/// output.
///
/// The band adds nothing to the filter but [`BandState`] — cutoff, [`Q`] and
/// gain are the inner SVF's live params, reached through the accessors here and
/// by [`UnitParam`](tutti_core::UnitParam) through the [`ParamSet`] it is
/// inserted with. Stack several to build a parametric EQ, one band
/// per [`SvfType`].
///
/// Bypass is a branch around the filter, not a dry/wet blend: it costs one
/// `match` per block and passes samples through bit-exactly.
///
/// `F` is the internal state precision; defaults to `f64`.
pub struct EqBandNode<F: Real = f64> {
    svf: SvfFilterNode<F>,
    state: BandState,
}

impl<F: Real> EqBandNode<F> {
    /// Builds an [`Active`](BandState::Active) band of `filter_type` at
    /// `frequency`, `q` and `gain_db`.
    ///
    /// `gain_db` is read only by [`Bell`](SvfType::Bell),
    /// [`LowShelf`](SvfType::LowShelf) and [`HighShelf`](SvfType::HighShelf) —
    /// the usual EQ-band types. On the others it is stored and ignored.
    ///
    pub fn new(
        filter_type: SvfType,
        frequency: impl Into<Hz>,
        q: impl Into<Q>,
        gain_db: impl Into<Db>,
    ) -> Self {
        Self {
            svf: SvfFilterNode::<F>::new(filter_type, frequency, q).with_gain_db(gain_db),
            state: BandState::Active,
        }
    }

    /// The inner filter's shared centre-frequency cell in [`Hz`]. Read once per
    /// block; shared across clones.
    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.svf.frequency()
    }

    /// The inner filter's shared [`Q`] cell — the band's width. Higher is
    /// narrower.
    pub fn q(&self) -> Arc<AtomicF32> {
        self.svf.q()
    }

    /// The inner filter's shared gain cell in [`Db`]: positive boosts, negative
    /// cuts, `0.0` is flat.
    pub fn gain_db(&self) -> Arc<AtomicF32> {
        self.svf.gain_db()
    }

    /// Enables or bypasses the band.
    ///
    /// `&mut self`, so it cannot reach a node already live in the graph — drive
    /// a live bypass from the host instead.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.state = BandState::from_enabled(enabled);
    }

    /// Returns whether the band is filtering rather than passing through.
    pub fn is_enabled(&self) -> bool {
        self.state.is_active()
    }

    /// Returns the band's [`BandState`].
    pub fn state(&self) -> BandState {
        self.state
    }

    /// Sets the band's [`BandState`] — the named form of
    /// [`set_enabled`](Self::set_enabled).
    pub fn set_state(&mut self, state: BandState) {
        self.state = state;
    }

    /// Switches the inner filter's response, forcing a coefficient recompute on
    /// the next sample.
    ///
    /// Filter state is retained, so the switch is continuous rather than a
    /// click.
    pub fn set_filter_type(&mut self, filter_type: SvfType) {
        self.svf.set_filter_type(filter_type);
    }
}

impl<F: Real + 'static> Node for EqBandNode<F> {
    /// Mono in and out. Unlike the bare filter it declares no modulatable
    /// params: a band is set, not swept.
    /// Its tail is the inner filter's: [`Tail::Unknown`](tutti_core::Tail::Unknown).
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO).with_tail(self.svf.shape().tail)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.svf.prepare(p);
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        match self.state {
            BandState::Active => self.svf.process(cx, io),
            BandState::Bypassed => {
                let (inputs, mut outputs) = io.split();
                outputs.get(0).copy_from_slice(inputs.get(0));
                Status::Modified
            }
        }
    }

    fn reset(&mut self) {
        Node::reset(&mut self.svf);
    }
}

impl<F: Real + 'static> ParamNode for EqBandNode<F> {
    /// The inner filter's: cutoff, Q and gain.
    fn param_set(&self) -> ParamSet {
        self.svf.param_set()
    }

    fn fork_fresh(&self) -> Self {
        Self {
            svf: self.svf.fork_fresh(),
            state: self.state,
        }
    }
}

/// Inserted as the inner filter is: its [`ParamSet`] as its controls, and a
/// fork from the values last set through it.
impl<F: Real + 'static> IntoNode for EqBandNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

impl<F: Real> Clone for EqBandNode<F> {
    fn clone(&self) -> Self {
        Self {
            svf: self.svf.clone(),
            state: self.state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, rms};
    use tutti_core::SampleRate;
    use tutti_graph::contract::{assert_param_fork, drive, prepared};

    /// `eq`, prepared at 44.1 kHz, over `input` in one block.
    fn render<F: Real + 'static>(eq: EqBandNode<F>, input: &[f32]) -> Vec<f32> {
        let rate = SampleRate(44_100.0);
        let mut eq = prepared(eq, rate, input.len());
        drive(&mut eq, rate, &[input], &[]).remove(0)
    }

    /// **A fork reaches the inner SVF's param cells**: it starts from the
    /// authored values and shares none of them.
    ///
    /// Mutation (run): `fork_fresh` cloning `svf` instead of forking it →
    /// "a live write reached the fork" → fails.
    #[test]
    fn a_fork_severs_the_inner_filters_cells() {
        assert_param_fork(EqBandNode::<f64>::new(SvfType::Bell, 1000.0, 1.0, 6.0));
    }

    /// The band declares the inner filter's tail, not the shape default.
    ///
    /// Mutation (run): drop the band's `.with_tail(..)` → `Tail::None` →
    /// fails.
    #[test]
    fn the_band_rings_on_as_its_filter_does() {
        let band = EqBandNode::<f64>::new(SvfType::Bell, 1000.0, 8.0, 12.0);
        assert_eq!(band.shape().tail, tutti_core::Tail::Unknown);
    }

    #[test]
    fn test_eq_band_f32_variant_compiles_and_runs() {
        let input = generate_sine(1000.0, 44100.0, 1024);
        let out = render(
            EqBandNode::<f32>::new(SvfType::Bell, 1000.0, 1.0, 6.0),
            &input,
        );
        assert!(
            rms(&out[256..]) > 0.0,
            "eq f32 variant should produce signal"
        );
    }

    #[test]
    fn test_eq_band_bypass() {
        let mut eq = EqBandNode::<f64>::new(SvfType::Bell, 1000.0, 1.0, 6.0);
        eq.set_enabled(false);
        let sine = generate_sine(1000.0, 44100.0, 1024);
        let out = render(eq, &sine);
        for i in 0..sine.len() {
            assert!(
                (sine[i] - out[i]).abs() < 0.0001,
                "Bypassed EQ should pass through at sample {i}"
            );
        }
    }

    #[test]
    fn test_eq_band_bell_boost() {
        let sine = generate_sine(1000.0, 44100.0, 4096);
        let out = render(
            EqBandNode::<f64>::new(SvfType::Bell, 1000.0, 1.0, 12.0),
            &sine,
        );
        let rms_in = rms(&sine[512..]);
        let rms_out = rms(&out[512..]);
        assert!(
            rms_out > rms_in,
            "Bell boost should increase level: in={rms_in}, out={rms_out}"
        );
    }

    #[test]
    fn test_eq_band_filter_type_change() {
        let high = generate_sine(5000.0, 44100.0, 2048);
        let out_lp = render(
            EqBandNode::<f64>::new(SvfType::LowPass, 500.0, 0.707, 0.0),
            &high,
        );
        let mut eq = EqBandNode::<f64>::new(SvfType::LowPass, 500.0, 0.707, 0.0);
        eq.set_filter_type(SvfType::HighPass);
        let out_hp = render(eq, &high);
        let rms_lp = rms(&out_lp[512..]);
        let rms_hp = rms(&out_hp[512..]);
        assert!(
            rms_hp > rms_lp * 2.0,
            "HP should pass more high freq than LP: lp={rms_lp}, hp={rms_hp}"
        );
    }
}
