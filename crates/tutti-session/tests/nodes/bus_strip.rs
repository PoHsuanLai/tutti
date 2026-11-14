//! The mixer strip on a placed bus: a bus edge folds into and out of it by the
//! speakers it declares.
//!
//! Every figure is analytic: a source writes a constant exactly representable
//! value, and a fold's gain is the fold's stated rule, not another render's.

use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_nodes::BusStripNode;
use tutti_session::{OutPort, Root, Session, SessionBuilder};
use tutti_types::{BusTopology, ChannelLayout, ChannelTopology, InterleavedMut, MaxBlock, Samples};

/// Constant `values` on its output channels, declaring `out` as its bus.
struct Src {
    values: Vec<f32>,
    out: BusTopology,
}

impl Node for Src {
    fn shape(&self) -> Shape {
        Shape::source(self.out)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (c, &v) in self.values.iter().enumerate() {
            io.output(c).fill(v);
        }
        Status::Constant
    }
    fn reset(&mut self) {}
}

fn offline(builder: SessionBuilder, device: u16) -> Session {
    builder
        .outputs(ChannelLayout::from(device))
        .max_block(MaxBlock::of(Samples(64)))
        .build()
        .expect("builds")
}

/// Frame 0 of the second device block (the first installs the plan), after the
/// session commits. `device` channels wide.
fn heard(s: &mut Session, device: u16) -> Vec<f32> {
    let _ = s.update();
    let layout = ChannelLayout::from(device);
    let mut out = vec![-9.0f32; usize::from(device) * 64];
    for _ in 0..2 {
        s.render(&mut InterleavedMut::new(&mut out, layout));
    }
    out[..usize::from(device)].to_vec()
}

/// A mono source wired into a stereo strip's bus is heard on both sides, the lone
/// centre at unity into each (the engine's mono-to-stereo fold), not on the left
/// alone.
#[test]
fn a_mono_source_reaches_both_sides_of_a_stereo_strip() {
    let mut s = offline(Session::builder(), 2);
    let g = s.graph();
    let (src, _) = g.add(Unforkable(Src {
        values: vec![0.5],
        out: BusTopology::MONO,
    }));
    let (strip, _) = g.add(BusStripNode::stereo());
    g.connect(src.bus_out(0), strip.bus_in(0))
        .expect("a mono bus folds onto a stereo one")
        .hard();
    for c in 0..2 {
        g.set_sources_hard(Root::output(c), [OutPort::new(strip, c)])
            .unwrap();
    }
    assert_eq!(heard(&mut s, 2), [0.5, 0.5]);
}

/// A stereo strip's bus folds onto a declared 5.1 root: its pair lands on the
/// front left and right, and the other four speakers are silent.
#[test]
fn a_stereo_strip_feeds_a_declared_surround_root() {
    let five_one = ChannelTopology::smpte(ChannelLayout::from(6u16)).unwrap();
    let mut s = offline(Session::builder().root_topology(five_one), 6);
    let g = s.graph();
    let (src, _) = g.add(Unforkable(Src {
        values: vec![0.25, 0.75],
        out: BusTopology::STEREO,
    }));
    let (strip, _) = g.add(BusStripNode::stereo());
    g.connect(src, strip).expect("same width").hard();
    g.connect(strip.bus_out(0), Root)
        .expect("a placed stereo bus folds onto 5.1")
        .hard();
    assert_eq!(heard(&mut s, 6), [0.25, 0.75, 0.0, 0.0, 0.0, 0.0]);
}
