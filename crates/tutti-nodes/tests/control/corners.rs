//! A family node solves at the corners of the lanes that drive its slow
//! controls (doc 024 §4.5, "Solve at corners"), as well as on its grid.

use tutti_curve::Law;
use tutti_graph::{
    Cx, Event, EventOut, GraphBuilder, IntoNode, Io, Node, NodeParts, ParamIn, ParamSegment,
    Prepare, Shape, Status,
};
use tutti_nodes::{CompressorControls, CompressorNode, KeySource};
use tutti_types::{
    BusTopology, CompressionRatio, Db, MaxBlock, SampleRate, Samples, Seconds, UnitParam,
};

/// Sends each `(absolute frame, segment)` on its frame.
#[derive(Clone)]
struct Sends(Vec<(u64, ParamSegment)>);

impl Node for Sends {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1).with_event_capacity(16)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        let n = io.frames() as u64;
        for &(at, seg) in &self.0 {
            if at >= start && at < start + n {
                let offset = io.offset((at - start) as usize).expect("inside");
                io.event_out(0)
                    .push(Event::segment(offset, seg))
                    .expect("room");
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

const FRAMES: usize = 200;

/// A compressor on its own signal (threshold -20 dB, hard knee, instant
/// attack and release) over a full-scale DC input, its ratio driven by
/// `sends`, rendered in blocks of `block`.
fn render(sends: Vec<(u64, ParamSegment)>, block: usize) -> Vec<f32> {
    let controls = CompressorControls {
        threshold: Db(-20.0),
        knee: Db(0.0),
        ratio: CompressionRatio(1.0),
        attack: Seconds(0.0),
        release: Seconds(0.0),
        makeup: Db(0.0),
    };
    let mut g = GraphBuilder::new(
        tutti_types::ChannelLayout::MONO,
        tutti_types::ChannelLayout::MONO,
    );
    let NodeParts { node, fork, .. } =
        CompressorNode::new(BusTopology::MONO, KeySource::Own, controls).into_parts();
    let c = g.add(NodeParts {
        node,
        controls: (),
        fork,
    });
    g.connect_input(0, c, 0);
    g.connect_output(c, 0, 0);
    let s = g.add(tutti_graph::Unforkable(Sends(sends)));
    let mut r = g
        .renderer(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples(block)),
        ))
        .expect("builds");
    let at = ParamIn {
        node: c,
        param: UnitParam::Ratio.into(),
    };
    r.editor_mut()
        .spec_mut()
        .connect_param(at, EventOut { node: s, port: 0 });
    r.editor_mut().commit().expect("commits");
    let input = vec![1.0f32; FRAMES];
    r.render_input(&[&input]).remove(0)
}

fn step(to: f32) -> ParamSegment {
    ParamSegment::unit(UnitParam::Ratio, to, to, Samples(0), Law::Linear)
}

/// **A ratio stepped between grid points lands on its own frame, at any
/// block length, and a re-send of it changes nothing.** The ratio steps to 2
/// at frame 0 and to 4 at frame 37, between the compressor's 16-frame grid
/// points (32 and 48). A full-scale DC input 20 dB over the threshold is
/// reduced by `20 · (1 − 1/r)` dB: 10 dB through frame 36, 15 dB from frame
/// 37 on (within 1e-6), the same at blocks of 1, 64 and 100 frames. On the
/// grid alone the 4 would be read at frame 48. The same 4 sent again at 70
/// (a source's re-send) renders the same bits.
#[test]
fn a_step_on_a_slow_control_lands_on_its_frame() {
    let sends = vec![(0, step(2.0)), (37, step(4.0))];
    let want = |f: usize| {
        let reduction = if f < 37 { 10.0f64 } else { 15.0 };
        10f64.powf(-reduction / 20.0)
    };
    let one = render(sends.clone(), 1);
    for (f, &y) in one.iter().enumerate() {
        assert!(
            (f64::from(y) - want(f)).abs() < 1e-6,
            "frame {f}: {y} vs {}",
            want(f)
        );
    }
    for block in [64, 100] {
        let other = render(sends.clone(), block);
        assert!(
            one.iter()
                .zip(&other)
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "blocks of {block}"
        );
    }
    let mut resent = sends;
    resent.push((
        70,
        step(4.0).resumed(
            tutti_types::SamplePosition(0.0),
            tutti_types::SamplePosition(0.0),
        ),
    ));
    let again = render(resent, 64);
    assert!(
        one.iter()
            .zip(&again)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "a re-send changes nothing"
    );
}
