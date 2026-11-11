//! What this crate's tests share: the inlets a port list is built over, and a
//! graph that plays the audio thread, so a test sees an event where the graph
//! would: on its frame, after the inlet has held it for its block.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tutti_types::MaxBlock;

use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Executor, Io, Node, NodeKey, OutPort,
    Prepare, Shape, Source, Status, Transport, Unforkable,
};
use tutti_midi_runtime::{DeviceInlets, InletChange};
use tutti_types::{Frame, SampleRate, Samples};
use tutti_ump::Ump;

use crate::HardwareMidiInputs;

/// A block clock a test writes by hand, as the engine does each render call.
pub(crate) struct BlockClock(
    std::sync::Arc<std::sync::Mutex<Option<(std::time::Instant, tutti_types::Moment)>>>,
);

impl BlockClock {
    fn new() -> Self {
        Self(Default::default())
    }

    fn reader(&self) -> tutti_midi_runtime::ArrivalClock {
        let cell = std::sync::Arc::clone(&self.0);
        std::sync::Arc::new(move || *cell.lock().unwrap())
    }

    fn write(
        &mut self,
        os: std::time::Instant,
        frame: tutti_types::Frame,
        rate: tutti_types::SampleRate,
    ) {
        *self.0.lock().unwrap() = Some((os, tutti_types::Moment::new(frame, rate)));
    }
}

pub(crate) const RATE: f64 = 44_100.0;

/// A list of ports over `slots` inlets of 64 events, with no lead.
pub(crate) fn ports(slots: usize) -> Arc<HardwareMidiInputs> {
    Arc::new(HardwareMidiInputs::new(Arc::new(DeviceInlets::new(
        slots,
        64,
        Duration::ZERO,
    ))))
}

/// `(slot, absolute frame, event)` of every event the sink received.
pub(crate) type Seen = Arc<Mutex<Vec<(usize, u64, Ump)>>>;

/// Logs the MIDI on each of its event inputs; a silent mono output.
struct Sink {
    slots: u16,
    seen: Seen,
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(tutti_types::ChannelLayout::MONO).with_events(self.slots, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.seen.lock().unwrap();
        for slot in 0..usize::from(self.slots) {
            for e in io.events(slot) {
                if let EventKind::Midi(ump) = e.kind {
                    seen.push((slot, cx.env.frame.get() + u64::from(e.offset.get()), ump));
                }
            }
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// The graph's owner and its audio thread in one: it applies the inlets'
/// changes as a session's `Poll` phase does, writes the block clock and renders
/// blocks of 64 frames.
pub(crate) struct Rig {
    pub(crate) inlets: Arc<DeviceInlets>,
    pub(crate) seen: Seen,
    clock: BlockClock,
    /// When frame 0 began.
    pub(crate) origin: Instant,
    editor: Editor,
    exec: Executor,
    frames: u64,
    /// The sink's slot inputs, and the inlet node each is fed from.
    inlet_keys: Vec<Option<NodeKey>>,
}

impl Rig {
    pub(crate) const SINK: NodeKey = NodeKey(1);
    pub(crate) const BLOCK: usize = 64;

    /// A rig over `inlets`, whose clock reads `origin` at frame 0.
    pub(crate) fn over(inlets: &Arc<DeviceInlets>, origin: Instant) -> Self {
        let slots = inlets.slots();
        let (mut editor, exec) = Editor::new(Prepare::new(
            SampleRate(RATE),
            MaxBlock::of(Samples(Self::BLOCK)),
        ));
        let seen = Seen::default();
        editor.insert(
            Self::SINK,
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
                node: Self::SINK,
                port: 0,
            })]);
        editor.commit().expect("commits");
        let clock = BlockClock::new();
        inlets.set_clock(Some(clock.reader()));
        Self {
            inlets: Arc::clone(inlets),
            seen,
            clock,
            origin,
            editor,
            exec,
            frames: 0,
            inlet_keys: vec![None; slots],
        }
    }

    /// Adds and wires the inlets of the devices that opened since, and takes
    /// out those that closed.
    pub(crate) fn apply(&mut self) {
        for change in self.inlets.take_changes() {
            match change {
                InletChange::Attach { slot, node } => {
                    let key = NodeKey(100 + slot as u64);
                    self.editor.insert(key, "inlet", node);
                    self.editor.spec_mut().connect_events(
                        EventIn {
                            node: Self::SINK,
                            port: slot as u16,
                        },
                        EventEdge::Direct(EventOut { node: key, port: 0 }),
                    );
                    self.inlet_keys[slot] = Some(key);
                }
                InletChange::Detach { slot } => {
                    if let Some(key) = self.inlet_keys[slot].take() {
                        self.editor.remove(key);
                    }
                }
            }
        }
        self.editor.commit().expect("commits");
    }

    /// The OS time at which graph frame `frame` begins.
    pub(crate) fn time_of(&self, frame: u64) -> Instant {
        self.origin + Duration::from_secs_f64(frame as f64 / RATE)
    }

    /// Renders `blocks` blocks, each preceded by its clock reading as the
    /// engine writes it.
    pub(crate) fn run(&mut self, blocks: usize) {
        for _ in 0..blocks {
            self.clock.write(
                self.time_of(self.frames),
                Frame(self.frames),
                SampleRate(RATE),
            );
            let mut out = vec![0.0f32; Self::BLOCK];
            self.exec
                .process(Self::BLOCK, &Transport::default(), &[], &mut [&mut out[..]]);
            self.frames += Self::BLOCK as u64;
        }
    }

    /// Renders blocks until the graph's clock reaches OS time `now`: the audio
    /// thread of a test that runs on the wall clock.
    #[cfg(feature = "sequencer-tests")]
    pub(crate) fn run_to(&mut self, now: Instant) {
        let due = (now.saturating_duration_since(self.origin).as_secs_f64() * RATE) as u64;
        while self.frames + Self::BLOCK as u64 <= due {
            self.run(1);
        }
    }

    /// Everything the sink has received, taken.
    pub(crate) fn take(&self) -> Vec<(usize, u64, Ump)> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}
