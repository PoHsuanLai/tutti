//! An `InletNode` holds a tap reader, whose drop is a debug panic on the
//! audio thread. Replacing or removing the node must retire it to the control
//! side (the executor hands the old unit back; `Editor::collect` drops it), as
//! a tap's lanes are.

use tutti_graph::{CrossfadeCurve, Editor, Fade, InletNode, Prepare, TapReader, Transport};
use tutti_graph::{NodeKey, TapId};
use tutti_graph::{OutPort, Source};
use tutti_types::{Beat, Bpm, ChannelLayout, SampleRate, Samples};

const KEY: NodeKey = NodeKey(7);

fn inlet() -> (InletNode, tutti_graph::TapWriter) {
    InletNode::pair(ChannelLayout::STEREO, Samples(256))
}

fn render(exec: &mut tutti_graph::Executor, blocks: usize) {
    let t = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
    for _ in 0..blocks {
        let mut chans = vec![vec![0.0f32; 64]; 2];
        let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
        exec.process(64, &t, &[], &mut outs);
    }
}

fn outputs(ed: &mut Editor) {
    ed.spec_mut()
        .topology
        .set_outputs((0..2).map(|port| Source::Node(OutPort { node: KEY, port })));
}

/// Replacing an inlet through a cut and through a crossfade, then removing
/// it, drops each ring on the control side: no block drops a tap reader, which
/// would panic in a debug build.
#[test]
fn replacing_and_removing_an_inlet_drops_it_on_the_control_side() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(64)));
    let (first, mut w1) = inlet();
    ed.insert(KEY, "inlet", first);
    outputs(&mut ed);
    ed.commit().expect("compiles");
    exec.apply_pending();
    w1.push_interleaved(&[0.5; 128]);
    render(&mut exec, 2);
    ed.collect();

    // A cut, then a crossfade, each over a fresh ring.
    for fade in [
        Fade::new(Samples(0), CrossfadeCurve::default()),
        Fade::new(Samples(96), CrossfadeCurve::default()),
    ] {
        let (next, mut w) = inlet();
        ed.replace(KEY, next, fade).expect("replaces");
        ed.commit().expect("compiles");
        exec.apply_pending();
        w.push_interleaved(&[0.25; 128]);
        render(&mut exec, 4);
        ed.collect();
    }

    ed.remove(KEY);
    ed.spec_mut().topology.set_outputs(vec![Source::Zero; 2]);
    ed.commit().expect("compiles");
    exec.apply_pending();
    render(&mut exec, 2);
    ed.collect();
}

/// A node that drops its tap reader inside `process`.
struct DropsInBlock(Option<TapReader>);

impl tutti_graph::Node for DropsInBlock {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::STEREO)
    }
    fn process(
        &mut self,
        _cx: &tutti_graph::Cx<'_>,
        _io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        self.0.take();
        tutti_graph::Status::Modified
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn reset(&mut self) {}
}

impl tutti_graph::IntoNode for DropsInBlock {
    type Controls = ();
    fn into_parts(self) -> tutti_graph::NodeParts<()> {
        tutti_graph::NodeParts {
            node: Box::new(self),
            controls: (),
            fork: None,
        }
    }
}

/// The control: a reader dropped in a block does panic here (debug builds), so
/// the test above would see a drop on the audio thread.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "tap reader")]
fn a_reader_dropped_in_a_block_panics() {
    let (reader, _writer) = TapReader::pair(TapId(0), 2, Samples(8));
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(64)));
    ed.insert(KEY, "drops", DropsInBlock(Some(reader)));
    outputs(&mut ed);
    ed.commit().expect("compiles");
    exec.apply_pending();
    render(&mut exec, 1);
}
