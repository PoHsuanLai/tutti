//! What the loopback examples share: the graph's side of the input, played on
//! the wall clock.
//!
//! A port's events reach the graph through an event inlet, each on its frame.
//! An example has no engine, so [`Listener`] is one: it adds the inlets of the
//! ports opened, writes the block clock the way the engine does (the OS time a
//! block begins and its frame) and renders blocks as real time passes, logging
//! what the inlets emit.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tutti_graph::{
    BlockClock, Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Executor, Io, Node, NodeKey,
    OutPort, Prepare, Shape, Source, Status, Transport, Unforkable,
};
use tutti_midi_hardware::DeviceInlets;
use tutti_midi_runtime::InletChange;
use tutti_types::{ChannelLayout, Frame, SampleRate, Samples};
use tutti_ump::Ump;

const RATE: f64 = 44_100.0;
const BLOCK: usize = 64;
const SINK: NodeKey = NodeKey(1);

/// Logs every event its inputs receive.
struct Sink {
    slots: u16,
    seen: Arc<Mutex<Vec<Ump>>>,
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(self.slots, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.seen.lock().unwrap();
        for slot in 0..usize::from(self.slots) {
            for e in io.events(slot) {
                if let EventKind::Midi(ump) = e.kind {
                    seen.push(ump);
                }
            }
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

pub struct Listener {
    inlets: Arc<DeviceInlets>,
    seen: Arc<Mutex<Vec<Ump>>>,
    editor: Editor,
    exec: Executor,
    clock: BlockClock,
    origin: Instant,
    frames: u64,
}

impl Listener {
    /// A graph over `inlets`, whose clock reads the wall time now at frame 0.
    pub fn over(inlets: &Arc<DeviceInlets>) -> Self {
        let slots = inlets.slots();
        let (mut editor, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(BLOCK)));
        let seen = Arc::new(Mutex::new(Vec::new()));
        editor.insert(
            SINK,
            "sink",
            Unforkable(Sink {
                slots: slots as u16,
                seen: Arc::clone(&seen),
            }),
        );
        editor
            .spec_mut()
            .topology
            .set_outputs(vec![Source::Node(OutPort {
                node: SINK,
                port: 0,
            })]);
        editor.commit().expect("commits");
        let clock = BlockClock::new();
        inlets.set_clock(Some(clock.reader()));
        Self {
            inlets: Arc::clone(inlets),
            seen,
            editor,
            exec,
            clock,
            origin: Instant::now(),
            frames: 0,
        }
    }

    /// Wires the inlets of the ports opened since.
    pub fn apply(&mut self) {
        for change in self.inlets.take_changes() {
            if let InletChange::Attach { slot, node } = change {
                let key = NodeKey(100 + slot as u64);
                self.editor.insert(key, "inlet", node);
                self.editor.spec_mut().connect_events(
                    EventIn {
                        node: SINK,
                        port: slot as u16,
                    },
                    EventEdge::Direct(EventOut { node: key, port: 0 }),
                );
            }
        }
        self.editor.commit().expect("commits");
    }

    /// Renders the blocks the wall clock has reached.
    fn play(&mut self) {
        let due = (self.origin.elapsed().as_secs_f64() * RATE) as u64;
        while self.frames + BLOCK as u64 <= due {
            let start = self.origin + Duration::from_secs_f64(self.frames as f64 / RATE);
            self.clock
                .write(start, Frame(self.frames), SampleRate(RATE));
            let mut out = vec![0.0f32; BLOCK];
            self.exec
                .process(BLOCK, &Transport::default(), &[], &mut [&mut out[..]]);
            self.frames += BLOCK as u64;
        }
    }

    /// Plays for `span` of real time and returns what arrived.
    pub fn listen(&mut self, span: Duration) -> Vec<Ump> {
        let end = Instant::now() + span;
        while Instant::now() < end {
            self.play();
            std::thread::sleep(Duration::from_millis(1));
        }
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}
