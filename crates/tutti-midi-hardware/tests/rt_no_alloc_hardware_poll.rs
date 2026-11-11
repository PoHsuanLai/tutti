//! Regression gate for the **hardware** MIDI input on the audio thread.
//!
//! `tutti-midi-runtime`'s own gate drives `MidiIngestNode` through a scripted
//! inlet, which cannot show what the production input does. That is a
//! `HardwareMidiInputs` port: a driver thread stamps each arrival with the
//! frame the engine's block clock puts it on and pushes it into an event
//! inlet; the graph's inlet node holds it until its block and the ingest node
//! translates it. Reaching it needs this crate, because `tutti-midi-runtime`
//! cannot depend on `tutti-midi-hardware` (the dependency runs the other way).
//!
//! Both halves run inside `assert_no_alloc`: the audio thread (the inlet node,
//! the ingest node, the block clock's write) **and** the driver's `push`,
//! which runs on a thread the OS owns and must not allocate either. A waiting
//! list that grew, a stamp that boxed, or an event held and dropped on the
//! wrong side would `realloc` or free inside a callback.
//!
//! Each gate runs the nodes in a graph, channel 0's port wired to an event tap
//! whose ring the test drains, so what was carried is counted.

use std::sync::Arc;
use std::time::{Duration, Instant};

use assert_no_alloc::AllocDisabler;
use tutti_fixtures::midi::{noop, note_on};
use tutti_graph::NodeKey;
use tutti_graph::{
    Editor, EventEdge, EventIn, EventOut, EventTapId, EventTapReader, Executor, Prepare, TapAlign,
    TapEvent, Transport,
};
use tutti_midi_hardware::{HardwareMidiInputs, InputProducerHandle};
use tutti_midi_runtime::{DeviceInlets, InletChange, MidiIngestNode};
use tutti_types::{Frame, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup, Ump};

/// A block clock a test writes by hand, as the engine does each render call.
struct BlockClock(
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

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const RATE: f64 = 48_000.0;
const INGEST: NodeKey = NodeKey(1);

/// A graph holding an ingest node over the inlets of `inputs`, its channel-0
/// port under an event tap, and the engine's part of the block clock.
struct Rig {
    _editor: Editor,
    exec: Executor,
    clock: BlockClock,
    origin: Instant,
    frames: u64,
    out: EventTapReader,
    drain: Vec<TapEvent>,
    /// Events that reached the tap.
    carried: usize,
}

impl Rig {
    /// `inputs`' ports must be opened first: their inlets are added here.
    fn new(inputs: &HardwareMidiInputs) -> Self {
        let inlets = inputs.inlets();
        let (mut editor, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(1024)));
        editor.insert(INGEST, "in", MidiIngestNode::new(inlets.slots()));
        for change in inlets.take_changes() {
            if let InletChange::Attach { slot, node } = change {
                let key = NodeKey(100 + slot as u64);
                editor.insert(key, "inlet", node);
                editor.spec_mut().connect_events(
                    EventIn {
                        node: INGEST,
                        port: slot as u16,
                    },
                    EventEdge::Direct(EventOut { node: key, port: 0 }),
                );
            }
        }
        let out = editor
            .open_event_tap(
                EventTapId(0),
                EventOut {
                    node: INGEST,
                    port: 0,
                },
                TapAlign::Raw,
                1024,
            )
            .expect("the ingest node is in the graph");
        editor.commit().expect("commits");
        let clock = BlockClock::new();
        inlets.set_clock(Some(clock.reader()));
        Self {
            _editor: editor,
            exec,
            clock,
            origin: Instant::now(),
            frames: 0,
            out,
            drain: vec![
                TapEvent {
                    frame: Frame::ZERO,
                    ump: noop()
                };
                2048
            ],
            carried: 0,
        }
    }

    /// When graph frame `frame` begins.
    fn time_of(&self, frame: u64) -> Instant {
        self.origin + Duration::from_secs_f64(frame as f64 / RATE)
    }

    /// Where the next block begins, in OS time.
    fn next_block(&self) -> Instant {
        self.time_of(self.frames)
    }

    /// One block of `frames`, preceded by the engine's clock reading, and
    /// drain what reached the tap.
    fn run(&mut self, frames: usize) {
        self.clock
            .write(self.next_block(), Frame(self.frames), SampleRate(RATE));
        self.exec
            .process(frames, &Transport::default(), &[], &mut []);
        self.frames += frames as u64;
        self.carried += self.out.pop_into(&mut self.drain);
    }
}

fn inputs(slots: usize, capacity: usize) -> Arc<HardwareMidiInputs> {
    Arc::new(HardwareMidiInputs::new(Arc::new(DeviceInlets::new(
        slots,
        capacity,
        Duration::ZERO,
    ))))
}

fn note() -> Ump {
    note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
}

/// Feed `count` events through a port's producer, as a driver callback
/// thread would, each arriving at `at`.
fn feed(port: &mut InputProducerHandle, count: usize, at: Instant) {
    for _ in 0..count {
        port.push(note(), at);
    }
}

/// The steady state: events arriving every block, well under the capacity,
/// each placed inside the block that follows.
#[test]
fn hardware_input_is_allocation_free() {
    let inputs = inputs(1, 256);
    let mut port = inputs.open_input_port("Test Input").unwrap();
    let mut rig = Rig::new(&inputs);

    // Warm up: the first block publishes the clock the stamp needs.
    rig.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..500 {
            let at = rig.time_of(rig.frames + 100);
            feed(&mut port, 16, at);
            rig.run(512);
        }
    });

    assert!(
        rig.carried > 0,
        "the input must have carried events: an empty one would prove nothing"
    );
}

/// The overflow case: more events pushed than an inlet holds.
///
/// Four ports of 64 events take 256 a block against rings of 64: the driver's
/// push must refuse the rest (counted), and the inlet's waiting list must
/// never grow past its capacity.
#[test]
fn hardware_input_over_capacity_is_allocation_free() {
    let inputs = inputs(4, 64);
    let mut ports: Vec<InputProducerHandle> = (0..4)
        .map(|i| inputs.open_input_port(format!("Input {i}")).unwrap())
        .collect();
    let mut rig = Rig::new(&inputs);
    rig.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            let at = rig.time_of(rig.frames + 100);
            for port in &mut ports {
                feed(port, 256, at);
            }
            rig.run(512);
        }
    });
    assert!(inputs.dropped_events() > 0, "the rings were full");
}

/// A block size sweep, since the inlet resolves each event against the block's
/// length.
#[test]
fn hardware_input_across_block_sizes_is_allocation_free() {
    let inputs = inputs(1, 256);
    let mut port = inputs.open_input_port("Test Input").unwrap();
    let mut rig = Rig::new(&inputs);

    const SIZES: [usize; 5] = [64, 128, 256, 512, 1024];
    rig.run(64);
    for frames in SIZES {
        feed(&mut port, 8, rig.time_of(rig.frames + 10));
        rig.run(frames);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100 {
            for frames in SIZES {
                feed(&mut port, 8, rig.time_of(rig.frames + 10));
                rig.run(frames);
            }
        }
    });
}

/// Events whose arrival predates the block take the late path: the inlet finds
/// them already past and emits them at the block's first frame, counting them.
/// The steady-state test above never reaches that branch.
#[test]
fn hardware_input_with_stale_timestamps_is_allocation_free() {
    let inputs = inputs(1, 256);
    let mut port = inputs.open_input_port("Test Input").unwrap();
    let mut rig = Rig::new(&inputs);
    rig.run(512);

    // An arrival at frame 0, long before every block that follows.
    let stale = rig.time_of(0);
    feed(&mut port, 16, stale);
    rig.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            feed(&mut port, 16, stale);
            rig.run(512);
        }
    });
    assert!(rig.carried > 0);
}

/// Before the engine has published a reading there is nothing to place an
/// arrival with: the event is pushed for the next block, and that path
/// allocates nothing either.
#[test]
fn hardware_input_before_the_first_reading_is_allocation_free() {
    let inputs = inputs(1, 256);
    let mut port = inputs.open_input_port("Test Input").unwrap();
    let mut rig = Rig::new(&inputs);
    // Nothing has run, so the clock holds no reading.
    assert_no_alloc::assert_no_alloc(|| {
        feed(&mut port, 16, Instant::now());
    });
    rig.run(512);
    assert_eq!(rig.carried, 16, "pushed for the next block, all of them");
}
