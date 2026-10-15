//! Modulation without Bevy: an LFO and an envelope follower on a filter's
//! native params, declared, reconciled onto a plain `Editor` with
//! `tutti_nodes::Modulation`, and rendered by its executor.
//!
//! A saw runs through a low-pass SVF. A 2 Hz sine LFO sweeps the cutoff;
//! a follower of a kick (a decaying burst on every beat) raises the
//! resonance on each hit. Then a depth edit republishes the LFO's lane with
//! no commit. Prints the output's level per 50 ms window.
//!
//! ```text
//! cargo run -p tutti-nodes --example headless_modulation
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use tutti_core::graph::{Edge, InPort, OutPort, Source};
use tutti_graph::{Cx, Editor, Executor, ForkByClone, Io, Node, Prepare, Shape, Status, Transport};
use tutti_mod::{
    Clock, CurveType, Declarations, LfoShape, Polarity, RouteDecl, RouteKey, RouteTarget,
    Source as ModSource, SourceKey, TargetDecl, TargetKey, Wave,
};
use tutti_nodes::testing::Osc;
use tutti_nodes::{Feed, ModHost, Modulation, SvfFilterNode, SvfType, Target};
use tutti_types::{
    AtomicF32, Beat, Bpm, ChannelLayout, Depth, Hz, NodeKey, ParamAddr, PhaseIncrement, SampleRate,
    Samples, Seconds, UnitParam, Q,
};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 512;

/// A kick on the executor's own clock: a burst decaying over 50 ms at the
/// top of every half second.
#[derive(Clone)]
struct Kick;

impl Node for Kick {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let base = cx.env.frame.get();
        for (i, x) in io.output(0).iter_mut().enumerate() {
            let t = (base + i as u64) % 24_000;
            *x = (1.0 - t as f32 / 2_400.0).max(0.0);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// The host's keys: target node 1 is the filter; source 2 follows the kick.
struct Host {
    filter: NodeKey,
    knobs: BTreeMap<UnitParam, Arc<AtomicF32>>,
    kick: NodeKey,
}

impl ModHost for Host {
    fn target(&self, t: TargetKey) -> Option<Target> {
        let ParamAddr::Unit(param) = t.param else {
            return None;
        };
        (t.node == 1).then(|| Target::Native {
            node: self.filter,
            knob: self.knobs.get(&param).cloned(),
        })
    }
    fn feed(&self, s: SourceKey) -> Option<Feed> {
        (s == SourceKey(2)).then_some(Feed::Audio(self.kick))
    }
}

fn param(p: UnitParam) -> TargetKey {
    TargetKey {
        node: 1,
        param: ParamAddr::Unit(p),
    }
}

fn route(source: u64, to: UnitParam, depth: f32) -> RouteDecl {
    RouteDecl {
        source: SourceKey(source),
        to: RouteTarget::Param(param(to)),
        depth: Depth(depth),
        polarity: Polarity::Bipolar,
        curve: CurveType::Linear,
    }
}

/// `windows` 50 ms windows, printed as their RMS.
fn play(editor: &mut Editor, exec: &mut Executor, windows: usize) {
    let transport = Transport::new(false, Bpm(120.0), Beat(0.0), None);
    let window = 2_400;
    let mut out = vec![0.0f32; BLOCK];
    let (mut sum, mut n) = (0.0f64, 0usize);
    let mut line = String::new();
    while n < windows * window {
        exec.process(BLOCK, &transport, &[], &mut [&mut out[..]]);
        editor.collect();
        for &x in &out {
            sum += f64::from(x) * f64::from(x);
            n += 1;
            if n % window == 0 {
                line.push_str(&format!("{:.3} ", (sum / window as f64).sqrt()));
                sum = 0.0;
            }
        }
    }
    println!("{line}");
}

fn main() {
    let (mut editor, mut exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(BLOCK)));
    let [saw, filter, kick] = [1, 2, 3].map(NodeKey);
    editor.insert(saw, "saw", ForkByClone(Osc::saw(Hz(110.0))));
    let params = editor.insert(
        filter,
        "svf",
        SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(1_000.0), Q(0.7)),
    );
    editor.insert(kick, "kick", ForkByClone(Kick));
    let topology = &mut editor.spec_mut().topology;
    topology.edges.insert(
        InPort {
            node: filter,
            port: 0,
        },
        Edge::Direct(Source::Node(OutPort { node: saw, port: 0 })),
    );
    topology.outputs = vec![Source::Node(OutPort {
        node: filter,
        port: 0,
    })];
    // A native target's range cell exists once its node is placed.
    editor.commit().expect("commits");

    let host = Host {
        filter,
        knobs: [UnitParam::Cutoff, UnitParam::Q]
            .into_iter()
            .filter_map(|p| Some((p, params.cell(p)?)))
            .collect(),
        kick,
    };
    let sources = [
        (
            SourceKey(1),
            ModSource::Lfo {
                wave: Wave::Shape {
                    shape: LfoShape::Sine,
                    seed: 0,
                },
                clock: Clock::Seconds {
                    rate: Hz(2.0),
                    phase: PhaseIncrement(0.0),
                    anchor: None,
                },
            },
        ),
        (
            SourceKey(2),
            ModSource::Follower {
                attack: Seconds(0.001),
                release: Seconds(0.08),
            },
        ),
    ];
    let mut routes = [
        (RouteKey(1), route(1, UnitParam::Cutoff, 0.2)),
        (RouteKey(2), route(2, UnitParam::Q, 0.5)),
    ];
    let targets = [
        (
            param(UnitParam::Cutoff),
            TargetDecl {
                min: 100.0,
                max: 4_000.0,
                automation: None,
            },
        ),
        (
            param(UnitParam::Q),
            TargetDecl {
                min: 0.5,
                max: 8.0,
                automation: None,
            },
        ),
    ];

    let mut modulation = Modulation::new();
    let decl = Declarations {
        sources: &sources,
        routes: &routes,
        targets: &targets,
    };
    let applied = modulation
        .apply(&decl, &mut editor, &host)
        .expect("applies");
    assert!(applied.commit, "two lanes, a follower and its edges");
    modulation.commit(&mut editor).expect("commits");
    println!("LFO depth 0.2, kick on the resonance:");
    play(&mut editor, &mut exec, 20);

    // A depth edit: the LFO's lane republishes its program, keeping phase.
    // No plan, no commit.
    routes[0].1.depth = Depth(0.45);
    let decl = Declarations {
        sources: &sources,
        routes: &routes,
        targets: &targets,
    };
    let applied = modulation
        .apply(&decl, &mut editor, &host)
        .expect("applies");
    assert!(!applied.commit);
    assert_eq!(applied.republished, vec![param(UnitParam::Cutoff)]);
    println!("LFO depth 0.45:");
    play(&mut editor, &mut exec, 20);
}
