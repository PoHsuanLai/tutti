//! `DesignNode<D>` over a design that is not Freeverb: a test-only echo with a
//! realization latency and a tail the test can make lie. What Freeverb cannot show
//! (it has no latency and a tail longer than a block): the graph's mix aligned
//! to the realization's latency, the cut at `tail + latency` inside a block, and a
//! block-aligned park. Every figure is the definition of a delay.

use tutti_dsp::{
    Bounds, Ceiling, Channels, Design, Effect, Effort, FirSummary, MaxDecay, Network, Realization,
    Setup, Sizes, Stage,
};
use tutti_graph::contract::{Driven, assert_honest_tail, drive_input, prepared};
use tutti_graph::{Env, PlayClock, Status, Transport, TransportChanges};
use tutti_graph::{Node, ParamNode, Prepare, Solo};
use tutti_nodes::{ControlSet, DesignNode, NodeDesign};
use tutti_types::{ChannelLayout, Frame, Mix, SampleRate, Samples, Seconds, UnitAddr, UnitParam};

const SR: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;

/// A mono echo. The **network** says an echo `claimed` frames after the input
/// (a FIR of that length and onset, with the realization's `latency`). The realization
/// plays that echo, and when `actual` differs a second one `actual` frames after
/// the input, past the claim: an honest echo has `actual == claimed`, a lying one
/// rings past its tail, which the node must cut.
#[derive(Clone, Copy, Debug)]
struct Echo {
    claimed: usize,
    actual: usize,
    latency: usize,
}

#[derive(Clone, Debug)]
struct EchoKernel {
    line: Vec<f32>,
    at: usize,
    echo: Echo,
}

impl Realization<Network> for EchoKernel {
    type Bounds = Bounds;
    fn prepare(&mut self, _: &Bounds, _: &Setup) {}
    fn retarget(&mut self, _: &Network) {}
    fn reset(&mut self) {
        self.line.fill(0.0);
    }
    fn latency(&self) -> Samples {
        Samples(self.echo.latency)
    }
    fn effort(&self) -> Effort {
        Effort::Light
    }
    fn fresh(&self) -> Self {
        Self::new(self.echo)
    }
}

impl Effect<Network> for EchoKernel {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let n = self.line.len();
        let (first, second) = (
            self.echo.claimed + self.echo.latency,
            self.echo.actual + self.echo.latency,
        );
        for (o, &x) in output[0].iter_mut().zip(input[0]) {
            self.line[self.at] = x;
            let back = |d: usize| self.line[(self.at + n - d) % n];
            *o = back(first) + if second != first { back(second) } else { 0.0 };
            self.at = (self.at + 1) % n;
        }
    }
}

impl EchoKernel {
    fn new(echo: Echo) -> Self {
        Self {
            line: vec![0.0; echo.claimed.max(echo.actual) + echo.latency + 1],
            at: 0,
            echo,
        }
    }
}

impl Design for Echo {
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

    fn design_into(&self, _: &(), rate: SampleRate, out: &mut Network) {
        out.clear(rate);
        let _ = out.stages.push(Stage::Fir(FirSummary {
            length: Samples(self.claimed),
            onset: Samples(self.claimed),
            latency: Samples(self.latency),
            rt60: [Seconds(0.0); 3],
            energy: 1.0,
        }));
    }

    fn blank(&self, rate: SampleRate) -> Network {
        Network::new(rate)
    }

    fn bounds(&self, ceiling: &Ceiling<MaxDecay>) -> Bounds {
        Bounds::measure(&self.design(&(), ceiling.rate()), ceiling.floor())
    }

    fn realization(&self) -> EchoKernel {
        EchoKernel::new(*self)
    }
}

/// An echo has no cells.
#[derive(Clone, Debug)]
struct NoParams;

impl ControlSet for NoParams {
    type Controls = ();

    fn addrs(&self) -> &'static [UnitAddr] {
        &[]
    }

    fn defaults(&self) {}

    fn flatten(&self, _: &(), _: &mut [f32]) {}

    fn assemble(&self, _: &[f32]) {}
}

impl NodeDesign for Echo {
    type Set = NoParams;

    fn set(&self, _: &()) -> NoParams {
        NoParams
    }
}

fn node(echo: Echo) -> DesignNode<Echo> {
    prepared(DesignNode::live(echo, ()), SR, BLOCK)
}

struct Rig {
    node: DesignNode<Echo>,
    frame: u64,
}

impl Rig {
    fn block(&mut self, input: &[f32]) -> Driven {
        let env = Env {
            frame: Frame(self.frame),
            sample_rate: SR,
            block_len: Samples(input.len()),
            transport: Transport::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        self.frame += input.len() as u64;
        drive_input(&mut self.node, &env, &[input], &[], &[])
    }
}

fn impulse_block() -> Vec<f32> {
    let mut x = vec![0.0; BLOCK];
    x[0] = 1.0;
    x
}

/// The realization's latency and the network's tail are declared apart (a tail
/// excludes latency), the harness's honesty check holds for an honest echo, and
/// the node's shape says so.
///
/// Mutation (run): folding the latency into the tail (`ring + latency` reported)
/// fails the equality; dropping `with_latency` fails the first assertion.
#[test]
fn latency_and_tail_are_declared_apart() {
    let echo = Echo {
        claimed: 100,
        actual: 100,
        latency: 7,
    };
    let mut n = node(echo);
    let shape = n.shape();
    assert_eq!(shape.latency.samples(), Samples(7));
    let tail = shape.tail.samples().expect("finite").get();
    assert!(
        (100..=101).contains(&tail),
        "the FIR's length, not +7: {tail}"
    );
    assert_honest_tail(&mut n, SR);
}

/// A realization that rings past its tail is cut at `tail + latency` after the last
/// input frame, to the frame, inside the block that holds it: dense noise in,
/// and the last frame to sound is the last input frame plus the reported tail
/// plus the latency.
///
/// Mutation (run): a frame more or less in the node's `live` count (the `Some`
/// arm, since the cut falls in the block that holds the input), or the latency
/// left out of `owed`, fails.
#[test]
fn a_cut_inside_a_block_is_at_tail_plus_latency() {
    let (claimed, latency) = (10, 7);
    let mut rig = Rig {
        node: node(Echo {
            claimed,
            actual: claimed + 3,
            latency,
        }),
        frame: 0,
    };
    // Input at every frame of the first 20, in a 64-frame block: the last input
    // frame is 19, the cut inside the block.
    let mut x = vec![0.0f32; BLOCK];
    for (i, s) in x.iter_mut().enumerate().take(20) {
        *s = 1.0 + i as f32;
    }
    let d = rig.block(&x);
    let Some(tutti_types::Tail::Finite(ring)) = d.reported_tail else {
        panic!("finite")
    };
    let last = d.audio[0].iter().rposition(|&s| s != 0.0);
    // The second echo, 3 frames past the claim, would sound for 3 frames more; the
    // cut is at 19 + ring + 7, inside the block.
    assert_eq!(last, Some(19 + ring.get() + latency));
    assert!(19 + ring.get() + latency < BLOCK);

    // The cut reset the realization: what it held when cut (the second echo, still in
    // flight) never sounds. Faint input keeps the node running; a node that never
    // heard the noise answers it identically.
    let mut fresh = Rig {
        node: node(Echo {
            claimed,
            actual: claimed + 3,
            latency,
        }),
        frame: 0,
    };
    for _ in 0..4 {
        let whisper = vec![1e-20; BLOCK];
        assert_eq!(rig.block(&whisper).audio, fresh.block(&whisper).audio);
    }
}

/// A cut that falls on a block edge parks the node in the next block, and the
/// park resets the realization: the node that woke answers as a fresh one. The echo
/// rings far past its claim, so its line still holds the impulse when the cut
/// lands, and a node that parked without resetting would play it back.
///
/// Mutation (run): returning `Idle` without the reset in the `cut == 0` branch
/// fails the comparison.
#[test]
fn a_block_aligned_cut_parks_and_resets() {
    // The ring the node reports for an echo of 20 frames, to land the cut at 64.
    let ring = {
        let mut probe = Rig {
            node: node(Echo {
                claimed: 20,
                actual: 20,
                latency: 0,
            }),
            frame: 0,
        };
        match probe.block(&impulse_block()).reported_tail {
            Some(tutti_types::Tail::Finite(n)) => n.get(),
            other => panic!("{other:?}"),
        }
    };
    // Impulse at frame 0: the cut is at 0 + ring + latency + 1 = 64 frames.
    let latency = BLOCK - 1 - ring;
    let echo = Echo {
        claimed: 20,
        actual: 300,
        latency,
    };
    let mut rig = Rig {
        node: node(echo),
        frame: 0,
    };
    let first = rig.block(&impulse_block());
    assert_eq!(first.status, Some(Status::Modified), "sound to the edge");
    let second = rig.block(&[0.0; BLOCK]);
    assert_eq!(
        second.status,
        Some(Status::Idle),
        "parked in the next block"
    );
    // A node that never heard the impulse. The same silence then an impulse to
    // both: the woken node must not play back the one the cut left in its line.
    let mut fresh = Rig {
        node: node(echo),
        frame: 0,
    };
    let mut woken = Vec::new();
    let mut new = Vec::new();
    for b in 0..12 {
        // Silence, then input that is faint but never exactly silent (so the node
        // stays awake and the realization runs on): an impulse, then a whisper.
        let x = match b {
            0..=2 => vec![0.0; BLOCK],
            3 => impulse_block(),
            _ => vec![1e-20; BLOCK],
        };
        woken.extend(rig.block(&x).audio.remove(0));
        new.extend(fresh.block(&x).audio.remove(0));
    }
    assert_eq!(woken, new);
}

/// The graph's mix aligns the dry input to the realization's declared latency,
/// never to the tail or the echo: at a mix of 0.5 an impulse comes out
/// half-strength at `latency` (the input, delayed to line up) and half-strength at
/// `latency + echo` (the wet). A block of silence first, so the mix's ramp from
/// the unit's `WET` start is over before the impulse.
///
/// Mutation (run): the shape declaring `latency - 1` (the graph's aligned input
/// one frame early) moves the first spike and fails.
#[test]
fn the_graphs_mix_is_aligned_to_the_kernels_latency() {
    let (claimed, latency) = (25, 7);
    let mut solo = Solo::new(
        DesignNode::live(
            Echo {
                claimed,
                actual: claimed,
                latency,
            },
            (),
        ),
        Prepare::new(SR, Samples(BLOCK)),
    );
    let key = solo.key();
    solo.renderer_mut()
        .editor_mut()
        .set_mix(key, Mix(0.5))
        .expect("a mix");
    let mut x = vec![0.0; BLOCK];
    x.extend(impulse_block());
    let out = solo.render_input(&[&x]).remove(0);
    let spikes: Vec<(usize, f32)> = out
        .iter()
        .enumerate()
        .filter(|&(_, &s)| s != 0.0)
        .map(|(i, &s)| (i - BLOCK, s))
        .collect();
    assert_eq!(
        spikes,
        vec![(latency, 0.5), (latency + claimed, 0.5)],
        "the dry at the latency, the wet an echo later"
    );
}

/// The echo reports no cells and declares no port: its dry/wet is the graph's.
#[test]
fn a_design_without_cells_offers_no_ports() {
    let n = node(Echo {
        claimed: 4,
        actual: 4,
        latency: 0,
    });
    assert_eq!(n.shape().params.len(), 0);
    assert_eq!(n.param_set().params().count(), 0);
    assert!(n.param_set().get(UnitParam::Wet).is_none());
}
