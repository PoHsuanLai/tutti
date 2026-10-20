//! Modulation without Bevy (design 015 §5.1): an LFO and an envelope
//! follower on a filter's native params, declared on an offline session
//! ([`Session::mods`]) and applied by its update.
//!
//! A saw runs through a low-pass SVF. A 2 Hz sine LFO sweeps the cutoff;
//! a follower of a kick (a decaying burst on every half second) raises the
//! resonance on each hit. Then a depth edit republishes the LFO's lane with
//! no plan sent. Prints the output's level per 50 ms window.
//!
//! ```text
//! cargo run -p tutti-session --features modulation --example headless_modulation
//! ```

use std::sync::Arc;

use tutti_core::{ChannelLayout, InterleavedMut, SampleRate, Samples};
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_mod::{Follower, Lfo};
use tutti_nodes::testing::Osc;
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_session::{GraphSource, ParamRef, Session};
use tutti_types::{Depth, Hz, UnitParam, Q};

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

/// `windows` 50 ms windows, printed as their RMS. The session updates
/// between blocks, as a host's frame loop would.
fn play(s: &mut Session, windows: usize) {
    let window = 2_400;
    let mut out = vec![0.0f32; BLOCK];
    let (mut sum, mut n) = (0.0f64, 0usize);
    let mut line = String::new();
    while n < windows * window {
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
        s.update();
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
    let mut s = Session::builder()
        .build_offline(SampleRate(RATE), ChannelLayout::MONO, Samples(BLOCK))
        .expect("builds");
    let (saw, _) = s.graph().insert(ForkByClone(Osc::saw(Hz(110.0))));
    let (filter, _) = s.graph().add(SvfFilterNode::<f64>::new(
        SvfType::LowPass,
        Hz(1_000.0),
        Q(0.7),
    ));
    let (kick, _) = s.graph().insert(ForkByClone(Kick));
    s.graph().set_source(filter, 0, GraphSource::Node(saw, 0));
    s.graph().set_output_source(0, GraphSource::Node(filter, 0));

    let cutoff = ParamRef::unit(filter, UnitParam::Cutoff);
    let resonance = ParamRef::unit(filter, UnitParam::Q);
    let mut m = s.mods();
    m.range(cutoff, 100.0, 4_000.0);
    m.range(resonance, 0.5, 8.0);
    let wobble = m.lfo(Lfo::sine().hz(Hz(2.0)));
    let sweep = m.route(wobble, cutoff, Depth(0.2));
    let hits = m.follower(Follower::default(), kick);
    m.route(hits, resonance, Depth(0.5));

    // The nodes commit on the first update, the lanes on the re-apply after
    // it (their targets resolve once the filter is in a plan).
    s.update();
    assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
    assert!(s.mod_state().unresolved_targets().is_empty());
    println!("LFO depth 0.2, kick on the resonance:");
    play(&mut s, 20);

    // A depth edit: the LFO's lane republishes its program, keeping phase.
    // No plan is sent.
    let sent = s.graph().sent_plan();
    s.mods().route_mut(sweep).expect("the route").depth = Depth(0.45);
    s.update();
    assert!(
        sent.zip(s.graph().sent_plan())
            .is_some_and(|(a, b)| Arc::ptr_eq(&a, &b)),
        "a depth edit sends no plan"
    );
    println!("LFO depth 0.45:");
    play(&mut s, 20);
}
