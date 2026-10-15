//! Regression gate for the **hardware** MIDI poll on the audio thread.
//!
//! `tutti-midi-runtime`'s own gate drives `MidiInputNode` through a fake wire,
//! which copies from a pre-built batch and can never allocate — so it proves
//! nothing about the production input. That is `HardwareMidiInputs`, and
//! reaching it needs this crate, because `tutti-midi-runtime` cannot depend on
//! `tutti-midi-hardware` (the dependency runs the other way).
//!
//! What the real path does that the fake does not: drain N port rings,
//! converting arrival `Instant`s to per-block `frame_offset`s, into a
//! fixed-capacity scratch buffer, then hand each event to the node. A buffer
//! that grew there would `realloc` inside the callback, and a port set or ring
//! released there would free inside it.
//!
//! Each gate runs the node in a graph, channel 0's port wired to a
//! `MidiOutNode` whose ring the test drains, so what was polled is counted.

use std::sync::Arc;
use std::time::Instant;

use assert_no_alloc::AllocDisabler;
use tutti_core::{NodeKey, SampleRate, Samples};
use tutti_graph::{Editor, EventEdge, EventIn, EventOut, Executor, Prepare, Transport};
use tutti_midi_hardware::{HardwareMidiInputs, InputProducerHandle};
use tutti_midi_runtime::{MidiInputNode, MidiOutControls, MidiOutNode};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// A graph holding an input node over `inputs`, its channel-0 port feeding a
/// MIDI-out node.
struct Rig {
    _editor: Editor,
    exec: Executor,
    out: MidiOutControls,
    drain: Vec<MidiEvent>,
    /// Events that reached the out node.
    carried: usize,
}

impl Rig {
    fn new(inputs: Arc<HardwareMidiInputs>) -> Self {
        let (mut editor, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(1024)));
        editor.insert(NodeKey(1), "in", MidiInputNode::new(Some(inputs)));
        let out = editor.insert(NodeKey(2), "out", MidiOutNode::new());
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
        editor.commit().expect("commits");
        Self {
            _editor: editor,
            exec,
            out,
            drain: vec![MidiEvent::noop(); 2048],
            carried: 0,
        }
    }

    /// One block of `frames`, and drain what reached the out node.
    fn run(&mut self, frames: usize) {
        self.exec
            .process(frames, &Transport::default(), &[], &mut []);
        self.carried += self.out.poll_into(&mut self.drain);
    }
}

fn note() -> MidiEvent {
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
}

/// Feed `count` events through a port's producer, as a driver callback
/// thread would.
fn feed(port: &mut InputProducerHandle, count: usize) {
    for _ in 0..count {
        port.push(note(), Instant::now());
    }
}

/// The steady state: events arriving every block, well under the cap.
#[test]
fn hardware_poll_is_allocation_free() {
    let inputs = Arc::new(HardwareMidiInputs::new(256));
    inputs.set_sample_rate(48_000.0);
    let mut port = inputs.create_input_port("Test Input");

    let mut pre = Rig::new(inputs.clone());

    // Warm up: the first poll primes the scratch, which is the engine's
    // cost rather than the audio thread's.
    feed(&mut port, 16);
    pre.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..500 {
            feed(&mut port, 16);
            pre.run(512);
        }
    });

    assert!(
        pre.carried > 0,
        "the poll must have carried events — an empty drain would prove nothing"
    );
}

/// The overflow case: more events queued than one block may carry.
///
/// This is the path that could grow the scratch buffer. Four ports of 256
/// events offer 1024 against a 256 cap, so the drain must stop at the cap and
/// leave the rest — without reallocating to hold them.
#[test]
fn hardware_poll_over_capacity_is_allocation_free() {
    let inputs = Arc::new(HardwareMidiInputs::new(256));
    inputs.set_sample_rate(48_000.0);
    let mut ports: Vec<InputProducerHandle> = (0..4)
        .map(|i| inputs.create_input_port(format!("Input {i}")))
        .collect();

    let mut pre = Rig::new(inputs.clone());

    // Warm with a *light* load — one event per port.
    //
    // Warming with the flood would defeat the test: a growable scratch would
    // grow to the flood's size outside the gate and have nothing left to grow
    // for inside it, so an unbounded drain would pass. The scratch must enter
    // the gate holding only its reserved capacity, exactly as it does on a
    // freshly started stream that then meets a burst.
    for port in &mut ports {
        feed(port, 1);
    }
    pre.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            // 4 ports x 256 events against a 256-event cap: the drain must stop
            // at the cap and leave the backlog, rather than growing to hold it.
            for port in &mut ports {
                feed(port, 256);
            }
            pre.run(512);
        }
    });
}

/// A block size sweep, since `frame_offset` conversion is driven by `nframes`.
#[test]
fn hardware_poll_across_block_sizes_is_allocation_free() {
    let inputs = Arc::new(HardwareMidiInputs::new(256));
    inputs.set_sample_rate(48_000.0);
    let mut port = inputs.create_input_port("Test Input");

    let mut pre = Rig::new(inputs.clone());

    const SIZES: [usize; 5] = [64, 128, 256, 512, 1024];
    for frames in SIZES {
        feed(&mut port, 8);
        pre.run(frames);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100 {
            for frames in SIZES {
                feed(&mut port, 8);
                pre.run(frames);
            }
        }
    });
}

/// Events whose arrival predates the block must not allocate on the clamp path.
///
/// `read_inputs` converts an arrival `Instant` into a `frame_offset` and clamps
/// anything that lands past the block. A stale timestamp drives that branch,
/// which the steady-state test above never reaches.
#[test]
fn hardware_poll_with_stale_timestamps_is_allocation_free() {
    let inputs = Arc::new(HardwareMidiInputs::new(256));
    inputs.set_sample_rate(48_000.0);
    let mut handle = inputs.create_input_port("Test Input");

    let mut pre = Rig::new(inputs.clone());

    // An arrival a full second ago converts to a `samples_ago` far past any
    // block, so every event takes the clamp branch.
    let stale = Instant::now() - std::time::Duration::from_secs(1);

    for _ in 0..16 {
        handle.push(note(), stale);
    }
    pre.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            for _ in 0..16 {
                handle.push(note(), stale);
            }
            pre.run(512);
        }
    });
}

/// A hotplug landing while a block drains frees nothing on the audio thread.
///
/// The drain borrows the port set; a device connecting mid-drain publishes a
/// new set on the control side. The old set must then be retired for the
/// *control* side to free, never released by the drain's borrow when it drops
/// inside the gate. The hotplug is run inside the drain's visitor, under
/// `permit_alloc`, so it lands exactly while the drain holds its borrow, and
/// only the drain's own code stays gated.
///
/// This pins the reader's code path, not the race: a single thread cannot
/// exhaust the interleavings. The reclamation protocol itself is checked by
/// `tutti-types`' loom model.
///
/// Mutation: putting the port set back on `ArcSwap` (`load()` in the drain,
/// `load` + clone + `store` in `create_input_port`) aborts this test on its
/// first block: the store turns the drain's guard into the old set's last
/// strong count, and the guard's drop frees that `Vec` inside the gate.
#[test]
fn hotplug_during_a_drain_frees_nothing_on_the_audio_thread() {
    let inputs = HardwareMidiInputs::new(256);
    inputs.set_sample_rate(48_000.0);
    let mut first = inputs.create_input_port("Input 0");

    // Warm the scratch outside the gate, as the other gates do.
    feed(&mut first, 4);
    inputs.cycle_start_read_all_inputs(512, |_, _| {});

    const BLOCKS: usize = 32;
    let mut visited = 0usize;
    assert_no_alloc::assert_no_alloc(|| {
        for block in 0..BLOCKS {
            feed(&mut first, 4);
            let mut plugged = false;
            visited += inputs.cycle_start_read_all_inputs(512, |_, _| {
                if !plugged {
                    plugged = true;
                    assert_no_alloc::permit_alloc(|| {
                        // The new port's producer is dropped at once: this
                        // test is about the set it lands in, not its ring.
                        let _ = inputs.create_input_port(format!("Hotplug {block}"));
                    });
                }
            });
        }
    });

    assert_eq!(
        visited,
        BLOCKS * 4,
        "every block drained its events, so every hotplug landed mid-drain"
    );
    assert_eq!(
        inputs.list_input_ports().len(),
        1 + BLOCKS,
        "every hotplug's port landed"
    );
}

/// A device unplugged while a block drains frees nothing on the audio thread.
///
/// The other half of hot-plug. Each block, a device is plugged in and fed on
/// the control side, then its connection closes (its producer handle drops), so
/// the port in the published set is the ring's last owner. The removal lands
/// inside the drain's visitor, under `permit_alloc`, while the drain still
/// borrows the set holding the port. The set is retired, so the port and its
/// ring must be freed by the next publish (the next block's plug-in, outside
/// the gate), never when the drain's borrow drops inside it.
///
/// Mutation: hold the port set in a `Mutex<Arc<_>>` that the drain reads as
/// an `Arc` clone (an owning snapshot, as the old `ArcSwap` guard became) →
/// the drain's snapshot is the old set's last owner, its drop frees the set,
/// the port and its ring inside the gate, and `assert_no_alloc` aborts. (The
/// same mutation aborts the plug-in gate above.)
#[test]
fn unplug_during_a_drain_frees_nothing_on_the_audio_thread() {
    let inputs = HardwareMidiInputs::new(256);
    inputs.set_sample_rate(48_000.0);

    // Warm the scratch outside the gate, as the other gates do.
    let mut warm = inputs.create_input_port("Warm");
    feed(&mut warm, 4);
    inputs.cycle_start_read_all_inputs(512, |_, _| {});
    assert!(inputs.remove_input_port(warm.port_id()));
    drop(warm);

    const BLOCKS: usize = 32;
    let mut visited = 0usize;
    for block in 0..BLOCKS {
        let mut device = inputs.create_input_port(format!("Device {block}"));
        feed(&mut device, 4);
        let id = device.port_id();
        // The connection closed: the published set now owns the ring alone.
        drop(device);

        assert_no_alloc::assert_no_alloc(|| {
            let mut unplugged = false;
            visited += inputs.cycle_start_read_all_inputs(512, |_, _| {
                if !unplugged {
                    unplugged = true;
                    assert_no_alloc::permit_alloc(|| {
                        assert!(inputs.remove_input_port(id), "the port was there");
                    });
                }
            });
        });
    }

    assert_eq!(
        visited,
        BLOCKS * 4,
        "every block drained its events, so every unplug landed mid-drain"
    );
    assert!(
        inputs.list_input_ports().is_empty(),
        "every unplugged port is gone"
    );
}
