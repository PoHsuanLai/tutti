//! `DesignNode<D>` over a design whose blend is [`NoBlend`] (doc 024 step 1b): a
//! node that is the wet signal alone, with no port and no cell for a mix, next to
//! the same design with [`DryWet`], which has both. A test-only echo (the input
//! `DELAY` frames late) stands for any family that is never blended: a filter,
//! a dynamics processor.

use tutti_dsp::{
    Bounds, Ceiling, Channels, Design, Effect, Effort, FirSummary, MaxDecay, Network, Realization,
    Setup, Sizes, Stage,
};
use tutti_graph::contract::{drive_input, prepared};
use tutti_graph::{Env, Node, ParamNode, PlayClock, Transport, TransportChanges};
use tutti_nodes::{Blend, ControlParams, DesignNode, DryWet, NoBlend, NodeDesign};
use tutti_types::{ChannelLayout, Frame, Mix, SampleRate, Samples, Seconds, UnitAddr, UnitParam};

const SR: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;
const DELAY: usize = 10;

/// An echo of the input `DELAY` frames late, with the blend `B`.
#[derive(Clone, Copy, Debug, Default)]
struct Echo<B>(std::marker::PhantomData<B>);

#[derive(Clone, Debug)]
struct EchoKernel {
    line: Vec<f32>,
    at: usize,
}

impl EchoKernel {
    fn new() -> Self {
        Self {
            line: vec![0.0; DELAY + 1],
            at: 0,
        }
    }
}

impl Realization<Network> for EchoKernel {
    type Bounds = Bounds;
    fn prepare(&mut self, _: &Bounds, _: &Setup) {}
    fn retarget(&mut self, _: &Network) {}
    fn reset(&mut self) {
        self.line.fill(0.0);
    }
    fn effort(&self) -> Effort {
        Effort::Light
    }
    fn fresh(&self) -> Self {
        Self::new()
    }
}

impl Effect<Network> for EchoKernel {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let n = self.line.len();
        for (o, &x) in output[0].iter_mut().zip(input[0]) {
            self.line[self.at] = x;
            *o = self.line[(self.at + n - DELAY) % n];
            self.at = (self.at + 1) % n;
        }
    }
}

impl<B: Blend> Design for Echo<B> {
    type Controls = ();
    type Model = Network;
    type Bounds = Bounds;
    type Range = MaxDecay;
    type Realization = EchoKernel;

    fn channels(&self) -> Channels {
        Channels {
            input: ChannelLayout::MONO,
            output: ChannelLayout::MONO,
        }
    }

    fn blank(&self, rate: SampleRate) -> Network {
        Network::new(rate)
    }

    fn design_into(&self, _: &(), rate: SampleRate, out: &mut Network) {
        out.clear(rate);
        let _ = out.stages.push(Stage::Fir(FirSummary {
            length: Samples(DELAY),
            onset: Samples(DELAY),
            latency: Samples(0),
            rt60: [Seconds::new(0.0); 3],
            energy: 1.0,
        }));
    }

    fn bounds(&self, ceiling: &Ceiling<MaxDecay>) -> Bounds {
        Bounds::measure(&self.design(&(), ceiling.rate()), ceiling.floor())
    }

    fn realization(&self) -> EchoKernel {
        EchoKernel::new()
    }
}

/// An echo has no cells.
#[derive(Clone, Debug)]
struct NoParams;

impl ControlParams for NoParams {
    type Controls = ();
    fn addrs(&self) -> &'static [UnitAddr] {
        &[]
    }
    fn bind(&self, set: tutti_graph::ParamSetBuilder) -> tutti_graph::ParamSetBuilder {
        set
    }
    fn base(&self, _: usize) -> Option<f32> {
        None
    }
    fn read(&mut self, _: &tutti_graph::Io<'_>, _: usize) {}
    fn detach(&mut self) {}
}

impl<B: Blend> NodeDesign for Echo<B> {
    type Params = NoParams;
    type Blend = B;

    fn params(&self, _: ()) -> NoParams {
        NoParams
    }
}

fn node<B: Blend>() -> DesignNode<Echo<B>> {
    prepared(DesignNode::new(Echo::<B>::default(), ()), SR, BLOCK)
}

fn render<B: Blend>(node: &mut DesignNode<Echo<B>>, input: &[f32]) -> Vec<f32> {
    let env = Env {
        frame: Frame(0),
        sample_rate: SR,
        block_len: Samples(input.len()),
        transport: Transport::default(),
        changes: TransportChanges::NONE,
        play: PlayClock::ZERO,
    };
    drive_input(node, &env, &[input], &[], &[]).audio[0].clone()
}

/// A node over [`NoBlend`] has no mix: no `Wet` port in its shape, no cell in its
/// `ParamSet`, no base for port 0; the same design over [`DryWet`] has all three
/// (and the mix is the first port).
///
/// Mutation (run): `NoBlend::ADDRS` written `&[UnitAddr::first(UnitParam::Wet)]`
/// puts a `Wet` port in the shape and fails the first assertion.
#[test]
fn a_noblend_node_has_no_wet_port() {
    let plain = node::<NoBlend>();
    assert!(plain.shape().params.as_slice().is_empty());
    assert!(plain.param_base(0).is_none());
    assert!(plain.param_set().get(UnitParam::Wet).is_none());

    let blended = node::<DryWet>();
    assert_eq!(
        blended.shape().params.as_slice(),
        [UnitAddr::first(UnitParam::Wet)]
    );
    assert_eq!(blended.param_base(0), Some(1.0));
    assert!(blended.param_set().get(UnitParam::Wet).is_some());
}

/// A node over [`NoBlend`] renders the wet signal alone: bit for bit what the
/// [`DryWet`] node renders at its default mix of 1 (the wet signal alone), and
/// the mix a `DryWet` node is given moves the other and never this one.
///
/// Mutation (run): a `NoBlend::finish` that adds the input to the output (the dry
/// path, unaligned) fails the first assertion.
#[test]
fn a_noblend_node_is_the_wet_signal_alone() {
    let input: Vec<f32> = (0..BLOCK).map(|i| 1.0 + i as f32).collect();
    let mut plain = node::<NoBlend>();
    let mut blended = node::<DryWet>();
    let wet = render(&mut blended, &input);
    assert_eq!(render(&mut plain, &input), wet);
    // The echo is the input `DELAY` frames late: the figure is the definition.
    assert_eq!(&wet[DELAY..], &input[..BLOCK - DELAY]);

    // Half the dry input in: the blended node changes, the plain one has no
    // mix to lower.
    blended.mix().store(Mix(0.5));
    assert_ne!(render(&mut blended, &input), wet);
}
