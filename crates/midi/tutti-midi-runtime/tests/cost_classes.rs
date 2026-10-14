//! The MIDI nodes are event-only and declare `Cost::Light` (doc 013, "Phase
//! 6, part (b)"). Declaring it changes only the parallel schedule: sibling
//! clip-to-output lanes pack into fewer tasks than undeclared ones, and on a
//! pool every event leaves on the frame the serial walk sends it on.

use std::sync::{Arc, Mutex};

use tutti_core::graph::{OutPort, Source};
use tutti_core::{Beat, Bpm, ChannelLayout, NodeKey, SampleRate, Samples, WorkerPool};
use tutti_graph::{
    Cost, Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Executor, ForkByClone, Io, Node,
    Prepare, Schedule, Shape, Status, Transport, Ump, Unforkable,
};
use tutti_midi_runtime::{
    ClockNode, HarmonyNode, MidiClipNode, MidiInputNode, MidiOutControls, MidiOutNode,
    MidiQueueNode, TimedMidiEvent,
};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};

const SR: f64 = 48_000.0;
const BPM: f64 = 120.0;
const MAX_BLOCK: usize = 256;
/// Clip → out lanes, every node `Light`.
const LANES: u64 = 16;
/// Clip → recorder lanes, the recorder declaring nothing (`Heavy`), so the
/// plan still spreads.
const HEAVY: u64 = 2;

/// `node` as a node that declares nothing: the cost model's default.
struct Undeclared<N>(N);

impl<N: Node> Node for Undeclared<N> {
    fn shape(&self) -> Shape {
        self.0.shape().with_cost(Cost::Heavy)
    }
    fn prepare(&mut self, p: &Prepare) {
        self.0.prepare(p);
    }
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.0.process(cx, io)
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

/// `(absolute frame, event)` of every MIDI event a recorder received.
type Seen = Arc<Mutex<Vec<(u64, MidiEvent)>>>;

/// Logs the MIDI on its event input; a silent mono output. Declares
/// nothing.
#[derive(Clone)]
struct Recorder(Seen);

impl Node for Recorder {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            if let EventKind::Midi(Ump(data)) = e.kind {
                let ev = MidiEvent {
                    frame_offset: e.offset.get(),
                    data,
                };
                seen.push((cx.env.frame.get() + u64::from(e.offset.get()), ev));
            }
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Notes every 1/50 beat for lane `k`, each lane a little later.
fn clip(k: u64) -> MidiClipNode {
    MidiClipNode::new((0..20).map(|i| {
        TimedMidiEvent::new(
            Beat(0.02 * i as f64 + 0.000_3 * k as f64),
            MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                (36 + k + i) as u8,
                0xFFFF,
            ),
        )
    }))
}

fn connect(ed: &mut Editor, from: NodeKey, to: NodeKey) {
    ed.spec_mut().connect_events(
        EventIn { node: to, port: 0 },
        EventEdge::Direct(EventOut {
            node: from,
            port: 0,
        }),
    );
}

/// [`LANES`] clips each into an out node, and [`HEAVY`] clips each into a
/// recorder on a global output; on a pool of `workers` when more than one.
/// `declared`: the clip → out lanes as shipped, or wrapped to declare
/// nothing. The out nodes' controls (declared only) and the recorders' logs.
fn graph(workers: usize, declared: bool) -> (Executor, Vec<MidiOutControls>, Vec<Seen>) {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(MAX_BLOCK)));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(WorkerPool::new(workers))));
    }
    let (mut outs, mut seen, mut outputs) = (Vec::new(), Vec::new(), Vec::new());
    for k in 0..LANES + HEAVY {
        let (source, sink) = (NodeKey(100 + k), NodeKey(k + 1));
        if k >= LANES {
            ed.insert(source, "clip", clip(k));
            let log = Seen::default();
            ed.insert(sink, "recorder", ForkByClone(Recorder(Arc::clone(&log))));
            seen.push(log);
            outputs.push(Source::Node(OutPort {
                node: sink,
                port: 0,
            }));
        } else if declared {
            ed.insert(source, "clip", clip(k));
            outs.push(ed.insert(sink, "out", MidiOutNode::new()));
        } else {
            ed.insert(source, "clip", Unforkable(Undeclared(clip(k))));
            ed.insert(sink, "out", Unforkable(Undeclared(MidiOutNode::new())));
        }
        connect(&mut ed, source, sink);
    }
    ed.spec_mut().topology.outputs = outputs;
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (exec, outs, seen)
}

/// Blocks from one frame to the maximum, ragged, rolling from beat 0.
fn render(exec: &mut Executor) {
    let mut frame = 0usize;
    for i in 0..60 {
        let n = 1 + (i * 37) % MAX_BLOCK;
        let mut chunk = vec![vec![0.0f32; n]; HEAVY as usize];
        let mut outs: Vec<&mut [f32]> = chunk.iter_mut().map(Vec::as_mut_slice).collect();
        let beat = frame as f64 * BPM / 60.0 / SR;
        let t = Transport::new(true, Bpm(BPM), Beat(beat), None);
        exec.process(n, &t, &[], &mut outs);
        frame += n;
    }
}

/// Everything the graph sent: each out node's events (frame and words),
/// then each recorder's.
fn sent(outs: &[MidiOutControls], seen: &[Seen]) -> Vec<Vec<(u64, [u32; 4])>> {
    let mut all: Vec<Vec<(u64, [u32; 4])>> = outs
        .iter()
        .map(|o| {
            let mut buf = vec![MidiEvent::noop(); 1024];
            let n = o.poll_into(&mut buf);
            buf[..n]
                .iter()
                .map(|e| (u64::from(e.frame_offset), e.data))
                .collect()
        })
        .collect();
    all.extend(seen.iter().map(|s| {
        s.lock()
            .unwrap()
            .iter()
            .map(|(f, e)| (*f, e.data))
            .collect()
    }));
    all
}

/// Every MIDI node is `Light`.
///
/// Mutation (run): the clip node's `with_cost` removed → its assert fails.
#[test]
fn every_midi_node_is_light() {
    assert_eq!(clip(0).shape().cost, Cost::Light);
    assert_eq!(HarmonyNode::new([]).shape().cost, Cost::Light);
    assert_eq!(ClockNode::new().shape().cost, Cost::Light);
    assert_eq!(MidiQueueNode::new().shape().cost, Cost::Light);
    assert_eq!(MidiOutNode::new().shape().cost, Cost::Light);
    assert_eq!(MidiInputNode::new(None).shape().cost, Cost::Light);
}

/// Sixteen light clip → out lanes beside two heavy ones: the light lanes
/// pack into fewer tasks than the same lanes declaring nothing, the plan
/// still spreads, and on a pool of two and of four every event is sent on
/// the frame the serial walk sends it on.
///
/// Mutation (run): the clip node's `with_cost(Cost::Light)` removed → each
/// lane is a heavy task again → the task-count assert fails.
#[test]
#[cfg_attr(miri, ignore = "spawns a worker pool")]
fn light_lanes_pack_and_send_on_the_serial_frames() {
    let (declared, ..) = graph(4, true);
    let (undeclared, ..) = graph(4, false);
    let (plan, bare) = (
        declared.plan().expect("a plan"),
        undeclared.plan().expect("a plan"),
    );
    assert!(
        plan.tasks().len() < bare.tasks().len(),
        "declared {} tasks, undeclared {}",
        plan.tasks().len(),
        bare.tasks().len()
    );
    assert!(matches!(plan.schedule(4), Schedule::Parallel { .. }));

    let (mut exec, outs, seen) = graph(1, true);
    render(&mut exec);
    let serial = sent(&outs, &seen);
    assert!(serial.iter().all(|lane| !lane.is_empty()));
    for workers in [2, 4] {
        let (mut exec, outs, seen) = graph(workers, true);
        assert!(exec.is_parallel());
        render(&mut exec);
        assert_eq!(serial, sent(&outs, &seen), "{workers} workers");
    }
}
