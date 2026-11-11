//! Parametric EQ band: an SVF with a mono layout and an EQ band's constructor.

use tutti_dsp::Real;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{ChannelLayout, Db, Hz, Q};

use super::svf::SvfFilterNode;
use tutti_dsp::{SvfControls, SvfType};

/// Parametric EQ band: an [`SvfFilterNode`] at one channel. 1 input, 1
/// output.
///
/// Cutoff, [`Q`] and gain are the inner SVF's live params, reached by
/// [`UnitParam`](tutti_types::UnitParam) through the [`ParamSet`] it is
/// inserted with. Stack several to build a parametric EQ, one band
/// per [`SvfType`]. A band's bypass is not here: it is `NodeMode::Bypassed`
/// (`Graph::set_mode`), the graph's for every node, bit-exact once its
/// declick is over (doc 013, "Bypass and mute are graph state").
///
/// `F` is the internal state precision; defaults to `f64`.
#[derive(Clone, Debug)]
pub struct EqBandNode<F: Real = f64> {
    svf: SvfFilterNode<F>,
}

impl<F: Real> EqBandNode<F> {
    /// Builds a band of `filter_type` at
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
            svf: SvfFilterNode::<F>::with_controls(
                ChannelLayout::MONO,
                SvfControls {
                    ty: filter_type,
                    freq: frequency.into(),
                    q: q.into(),
                    gain: gain_db.into(),
                },
            ),
        }
    }
}

impl<F: Real + 'static> Node for EqBandNode<F> {
    /// Mono in and out. Unlike the bare filter it declares no modulatable
    /// params: a band is set, not swept.
    /// Its tail is the inner filter's: [`Tail::Unknown`](tutti_types::Tail::Unknown).
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_cost(tutti_graph::Cost::Moderate)
            .with_tail(self.svf.shape().tail)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.svf.prepare(p);
    }

    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.svf.process(cx, io)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, rms};
    use tutti_graph::contract::{assert_param_fork, drive, prepared};
    use tutti_types::SampleRate;

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
        assert_param_fork(EqBandNode::<f64>::new(
            SvfType::Bell,
            Hz::new(1000.0),
            Q::new(1.0),
            Db::new(6.0),
        ));
    }

    /// The band declares the inner filter's tail, not the shape default.
    ///
    /// Mutation (run): drop the band's `.with_tail(..)` → `Tail::None` →
    /// fails.
    #[test]
    fn the_band_rings_on_as_its_filter_does() {
        let band =
            EqBandNode::<f64>::new(SvfType::Bell, Hz::new(1000.0), Q::new(8.0), Db::new(12.0));
        assert_eq!(band.shape().tail, tutti_types::Tail::Unknown);
    }

    #[test]
    fn test_eq_band_f32_variant_compiles_and_runs() {
        let input = generate_sine(1000.0, 44100.0, 1024);
        let out = render(
            EqBandNode::<f32>::new(SvfType::Bell, Hz::new(1000.0), Q::new(1.0), Db::new(6.0)),
            &input,
        );
        assert!(
            rms(&out[256..]) > 0.0,
            "eq f32 variant should produce signal"
        );
    }

    #[test]
    fn test_eq_band_bell_boost() {
        let sine = generate_sine(1000.0, 44100.0, 4096);
        let out = render(
            EqBandNode::<f64>::new(SvfType::Bell, Hz::new(1000.0), Q::new(1.0), Db::new(12.0)),
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
            EqBandNode::<f64>::new(
                SvfType::LowPass,
                Hz::new(500.0),
                Q::new(0.707),
                Db::new(0.0),
            ),
            &high,
        );
        let out_hp = render(
            EqBandNode::<f64>::new(
                SvfType::HighPass,
                Hz::new(500.0),
                Q::new(0.707),
                Db::new(0.0),
            ),
            &high,
        );
        let rms_lp = rms(&out_lp[512..]);
        let rms_hp = rms(&out_hp[512..]);
        assert!(
            rms_hp > rms_lp * 2.0,
            "HP should pass more high freq than LP: lp={rms_lp}, hp={rms_hp}"
        );
    }
}
