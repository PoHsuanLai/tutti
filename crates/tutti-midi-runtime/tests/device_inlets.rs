//! `DeviceInlets` and `DeviceInput` (doc 017 stage E5): a device's thread places
//! each arrival on a frame with a `ClockMap` fed the engine's block clock, and
//! pushes it into its own event inlet. Figures are analytic: a reading says OS
//! time `T` is frame `F`, so an arrival `d` after `T` is frame `F + d * rate`,
//! plus the lead (doc 017 section 5, "Timing at the edge").

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tutti_types::MaxBlock;

use tutti_fixtures::midi::note_on;
use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Executor, Io, Node, NodeKey, OutPort,
    Prepare, Shape, Source, Status, Transport, Unforkable,
};
use tutti_midi_runtime::{DeviceInlets, DeviceInput, InletChange, NoFreeSlot};
use tutti_types::{ChannelLayout, Frame, SampleRate, Samples};
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

const RATE: f64 = 48_000.0;
const BLOCK: usize = 512;

fn note(n: u8) -> Ump {
    note_on(MidiGroup::FIRST, MidiChannel::FIRST, n, 0x8000)
}

type Seen = Arc<Mutex<Vec<(u64, Ump)>>>;

struct Sink(Seen);

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for e in io.events(0) {
            if let EventKind::Midi(ump) = e.kind {
                self.0
                    .lock()
                    .unwrap()
                    .push((cx.env.frame.get() + u64::from(e.offset.get()), ump));
            }
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// One device's inlet in a graph that logs what it emits, played in blocks.
struct Rig {
    exec: Executor,
    _editor: Editor,
    seen: Seen,
    input: DeviceInput,
}

impl Rig {
    fn new(inlets: &DeviceInlets) -> Self {
        let input = inlets.open().expect("a free slot");
        let Some(InletChange::Attach { node, .. }) = inlets.take_changes().pop() else {
            panic!("an open queues its inlet");
        };
        let (mut editor, exec) =
            Editor::new(Prepare::new(SampleRate(RATE), MaxBlock::of(Samples(BLOCK))));
        let seen = Seen::default();
        editor.insert(NodeKey(1), "inlet", node);
        editor.insert(NodeKey(2), "sink", Unforkable(Sink(Arc::clone(&seen))));
        editor.spec_mut().connect_events(
            EventIn {
                node: NodeKey(2),
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(1),
                port: 0,
            }),
        );
        editor
            .spec_mut()
            .topology
            .set_outputs(vec![Source::Node(OutPort {
                node: NodeKey(2),
                port: 0,
            })]);
        editor.commit().expect("commits");
        Self {
            exec,
            _editor: editor,
            seen,
            input,
        }
    }

    fn run(&mut self, blocks: usize) {
        for _ in 0..blocks {
            let mut out = vec![0.0f32; BLOCK];
            self.exec
                .process(BLOCK, &Transport::default(), &[], &mut [&mut out[..]]);
        }
    }

    fn take(&self) -> Vec<(u64, Ump)> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

/// `d` after `t`, in frames at `RATE`.
fn after(t: Instant, frames: u64) -> Instant {
    t + Duration::from_secs_f64(frames as f64 / RATE)
}

fn inlets(lead: Duration) -> DeviceInlets {
    DeviceInlets::new(2, 64, lead)
}

/// **An arrival is the frame the reading puts it on, plus the lead**: the
/// engine says OS time `t0` is frame 1000; arrivals 100, 250 and 700 frames
/// after `t0` sound on frames 1100, 1250 and 1700, and with a lead of 480
/// frames (10 ms), 1580, 1730 and 2180: the same spacing, the same lead.
#[test]
fn an_arrival_sounds_on_the_frame_the_clock_puts_it_on_plus_the_lead() {
    for lead_frames in [0u64, 480] {
        let inlets = inlets(Duration::from_secs_f64(lead_frames as f64 / RATE));
        let mut rig = Rig::new(&inlets);
        let t0 = Instant::now();
        let mut clock = BlockClock::new();
        inlets.set_clock(Some(clock.reader()));
        clock.write(t0, Frame(1000), SampleRate(RATE));
        for (n, d) in [(60u8, 100u64), (61, 250), (62, 700)] {
            assert!(rig.input.push(note(n), after(t0, d)));
        }
        rig.run(6);
        let frames: Vec<u64> = rig.take().iter().map(|e| e.0).collect();
        assert_eq!(
            frames,
            vec![1100 + lead_frames, 1250 + lead_frames, 1700 + lead_frames],
            "lead {lead_frames}"
        );
    }
}

/// **The map follows the clock across readings**: a second reading puts the
/// OS time `t0 + 10 ms` at frame 1480 (480 frames on, on the nominal rate), and
/// an arrival after it is placed from it.
#[test]
fn a_later_reading_places_later_arrivals() {
    let inlets = inlets(Duration::ZERO);
    let mut rig = Rig::new(&inlets);
    let t0 = Instant::now();
    let mut clock = BlockClock::new();
    inlets.set_clock(Some(clock.reader()));
    clock.write(t0, Frame(1000), SampleRate(RATE));
    assert!(rig.input.push(note(60), after(t0, 100)));
    clock.write(after(t0, 480), Frame(1480), SampleRate(RATE));
    assert!(rig.input.push(note(61), after(t0, 580)));
    rig.run(6);
    let frames: Vec<u64> = rig.take().iter().map(|e| e.0).collect();
    assert_eq!(frames, vec![1100, 1580]);
}

/// **Before any reading an event is pushed for the next block**: the first
/// frame of the block that finds it, as `At::NextBlock` is.
#[test]
fn before_a_reading_an_event_is_for_the_next_block() {
    let inlets = inlets(Duration::from_millis(5));
    let mut rig = Rig::new(&inlets);
    assert!(rig.input.push(note(60), Instant::now()));
    rig.run(2);
    assert_eq!(rig.take(), vec![(0, note(60))]);
}

/// **An arrival the clock puts before frame 0 is pushed for the next block**,
/// not wrapped to a huge frame.
#[test]
fn an_arrival_before_frame_zero_is_for_the_next_block() {
    let inlets = inlets(Duration::ZERO);
    let mut rig = Rig::new(&inlets);
    let t0 = Instant::now();
    let mut clock = BlockClock::new();
    inlets.set_clock(Some(clock.reader()));
    clock.write(t0, Frame(100), SampleRate(RATE));
    // 5,000 frames before the reading: frame -4,900.
    let early = t0 - Duration::from_secs_f64(5000.0 / RATE);
    assert!(rig.input.push(note(60), early));
    rig.run(2);
    assert_eq!(rig.take(), vec![(0, note(60))]);
}

/// **A restarted clock re-anchors the map**: an engine that restarts begins its
/// frames at 0 again, and the map, which refuses a reading that goes back, is
/// built afresh from it. Arrivals after the restart are placed from the new
/// clock, not from the old anchor.
#[test]
fn a_restarted_clock_places_arrivals_from_the_new_anchor() {
    let inlets = inlets(Duration::ZERO);
    let mut rig = Rig::new(&inlets);
    let t0 = Instant::now();
    let mut first = BlockClock::new();
    inlets.set_clock(Some(first.reader()));
    first.write(t0, Frame(1000), SampleRate(RATE));
    assert!(rig.input.push(note(60), after(t0, 480)));

    // The engine restarts a second later; its frames begin at 0 again.
    let t1 = t0 + Duration::from_secs(1);
    let mut second = BlockClock::new();
    inlets.set_clock(Some(second.reader()));
    second.write(t1, Frame(100), SampleRate(RATE));
    assert!(rig.input.push(note(61), after(t1, 480)));
    rig.run(6);
    let frames: Vec<u64> = rig.take().iter().map(|e| e.0).collect();
    assert_eq!(frames, vec![580, 1480]);
}

/// **An arrival is a moment at the reading's rate**: a clock that counts
/// frames at 96 kHz puts an arrival 10 ms after its reading 960 frames of its
/// own on, and the graph (48 kHz) plays it 480 frames on from the reading's
/// instant.
#[test]
fn a_reading_at_another_rate_is_rescaled_by_the_inlet() {
    let inlets = inlets(Duration::ZERO);
    let mut rig = Rig::new(&inlets);
    let t0 = Instant::now();
    let mut clock = BlockClock::new();
    inlets.set_clock(Some(clock.reader()));
    clock.write(t0, Frame(2000), SampleRate(96_000.0));
    assert!(rig.input.push(note(60), t0 + Duration::from_millis(10)));
    rig.run(6);
    // 2,000 frames at 96 kHz is 1,000 at 48 kHz; 10 ms is 480 more.
    assert_eq!(rig.take(), vec![(1480, note(60))]);
}

/// **A device takes a slot and gives it back; a stale slot names nothing.**
/// With two slots, a third open is refused; closing one frees it for the next,
/// and closing it again, after another device took it, changes nothing.
#[test]
fn slots_are_taken_and_given_back() {
    let inlets = inlets(Duration::ZERO);
    let a = inlets.open().unwrap();
    let _b = inlets.open().unwrap();
    assert_eq!(inlets.open().unwrap_err(), NoFreeSlot { slots: 2 });
    let slot = a.slot();
    assert_eq!(slot.index(), 0);
    assert!(inlets.close(slot));
    let c = inlets.open().unwrap();
    assert_eq!(c.slot().index(), 0, "the freed slot is taken");
    assert_ne!(c.slot(), slot);
    assert!(!inlets.close(slot), "a stale slot closes nothing");
    assert!(inlets.close(c.slot()));
}

/// **The owner takes the changes in order**: an attach for each open, a detach
/// for each close of an inlet the owner has taken.
#[test]
fn the_owner_takes_attaches_and_detaches_in_order() {
    let inlets = inlets(Duration::ZERO);
    let a = inlets.open().unwrap();
    let b = inlets.open().unwrap();
    let first = inlets.take_changes();
    assert!(matches!(
        first.as_slice(),
        [
            InletChange::Attach { slot: 0, .. },
            InletChange::Attach { slot: 1, .. }
        ]
    ));
    assert!(inlets.close(b.slot()));
    assert!(inlets.close(a.slot()));
    let second = inlets.take_changes();
    assert!(matches!(
        second.as_slice(),
        [
            InletChange::Detach { slot: 1 },
            InletChange::Detach { slot: 0 }
        ]
    ));
    assert!(inlets.take_changes().is_empty(), "taken once");
}

/// **A device that comes and goes before the owner looks leaves nothing**: its
/// attach and detach cancel, so a connect and disconnect loop with no owner
/// draining does not pile up inlets.
#[test]
fn an_inlet_closed_before_it_was_taken_is_never_queued() {
    let inlets = inlets(Duration::ZERO);
    for _ in 0..100 {
        let device = inlets.open().unwrap();
        assert!(inlets.close(device.slot()));
    }
    assert!(inlets.take_changes().is_empty());
}

/// **The counts are readable from the control side while the thread pushes**:
/// a push into a full inlet is refused and counted as lost.
#[test]
fn a_full_inlet_refuses_and_counts() {
    let inlets = DeviceInlets::new(1, 2, Duration::ZERO);
    let mut device = inlets.open().unwrap();
    let counts = device.counts();
    let pushed: Vec<bool> = (0..4)
        .map(|i| device.push(note(60 + i), Instant::now()))
        .collect();
    assert_eq!(pushed, vec![true, true, false, false]);
    assert_eq!(counts.lost(), 2);
}
