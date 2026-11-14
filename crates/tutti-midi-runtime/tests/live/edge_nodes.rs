//! The nodes at the graph's MIDI border:
//! [`MidiIngestNode`] sends the events of its device inlets out of their channels' ports,
//! translated; an event tap hands what a port carries back to a control
//! thread; [`ClockNode`] ticks
//! Beat Clock on the frames the transport reaches, a start inside a block
//! included.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_types::MaxBlock;

use crate::common;
use tutti_graph::{Stamp, UmpAt};

use tutti_fixtures::midi::{
    continue_msg, m1_cc, note_off, note_on, pitch_bend, registered_rpn, song_position, start,
    timing_clock,
};
use tutti_graph::{
    Cx, Editor, EventCapacity, EventEdge, EventIn, EventInletNode, EventKind, EventOut, Executor,
    ForkMode, ForkTarget, Io, Node, Offset, Prepare, Shape, Status, TransportChanges,
    TransportPosition,
};
use tutti_graph::{IntoNode as _, NodeKey};
use tutti_midi_protocol::{MpeZoneConfig, MpeZones, PitchBendSensitivity, channel_voice};
use tutti_midi_runtime::{
    CHANNELLESS_PORT, ClockNode, DeviceInlets, MIDI_INPUT_EVENT_CAPACITY, MIDI_INPUT_PORTS,
    MidiIngestNode, MidiInputPorts, MpeTransformNode, SlotCountMismatch,
};
use tutti_types::{Beat, Bpm, Frame, SampleRate, Samples};
use tutti_ump::{
    CCNumber, KeyEvent, Message, MidiChannel, MidiGroup, NoteNumber, U7, U14, Ump, Voice2,
};

/// The note a channel voice message addresses, either protocol.
fn note_of(ump: Ump) -> Option<u8> {
    channel_voice(ump)?.1.note().map(NoteNumber::get)
}

const G: MidiGroup = MidiGroup::FIRST;

const RATE: f64 = 48_000.0;

/// `(sink port, absolute frame, event)` of every MIDI event a sink received.
type Seen = Arc<Mutex<Vec<(usize, u64, Ump)>>>;

/// Logs the MIDI on each of its `ports` event inputs; a silent mono output,
/// so a fork has somewhere to end.
#[derive(Clone)]
struct Sink {
    ports: u16,
    seen: Seen,
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(tutti_types::ChannelLayout::MONO).with_events(self.ports, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.seen.lock().unwrap();
        for p in 0..usize::from(self.ports) {
            for e in io.events(p) {
                if let EventKind::Midi(Ump(data)) = e.kind {
                    seen.push((p, cx.env.frame.get() + u64::from(e.offset.get()), Ump(data)));
                }
            }
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A source at key 1 whose `ports` event outputs each feed the sink's input
/// of the same number (key 2), blocks of at most `max`.
fn rig<N: tutti_graph::IntoNode>(
    source: N,
    ports: u16,
    max: usize,
) -> (Editor, Executor, N::Controls, Seen) {
    rig_with(source, ports, max, |_| {})
}

/// [`rig`], and `extra` adds what feeds the source before the commit.
fn rig_with<N: tutti_graph::IntoNode>(
    source: N,
    ports: u16,
    max: usize,
    extra: impl FnOnce(&mut Editor),
) -> (Editor, Executor, N::Controls, Seen) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples(max)),
    ));
    let controls = ed.insert(NodeKey(1), "source", source);
    extra(&mut ed);
    let seen = Seen::default();
    ed.insert(
        NodeKey(2),
        "sink",
        tutti_graph::ForkByClone(Sink {
            ports,
            seen: Arc::clone(&seen),
        }),
    );
    for p in 0..ports {
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(2),
                port: p,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(1),
                port: p,
            }),
        );
    }
    ed.spec_mut()
        .topology
        .set_outputs(vec![tutti_graph::Source::Node(tutti_graph::OutPort {
            node: NodeKey(2),
            port: 0,
        })]);
    ed.commit().expect("commits");
    (ed, exec, controls, seen)
}

/// An ingest node and the device inlets that feed it (keys 3 upward), over
/// `ports` sink inputs.
fn ingest(
    ingest: Ingest,
    ports: u16,
    max: usize,
) -> (
    Editor,
    Executor,
    tutti_midi_runtime::MidiIngestControls,
    Seen,
) {
    let Ingest { node, inlets } = ingest;
    rig_with(node, ports, max, |ed| {
        for (slot, inlet) in inlets.into_iter().enumerate() {
            let key = NodeKey(3 + slot as u64);
            ed.insert(key, "inlet", inlet);
            ed.spec_mut().connect_events(
                EventIn {
                    node: NodeKey(1),
                    port: slot as u16,
                },
                EventEdge::Direct(EventOut { node: key, port: 0 }),
            );
        }
    })
}

fn block(exec: &mut Executor, len: usize) {
    let mut out = vec![0.0f32; len];
    exec.process(len, &TransportPosition::default(), &[], &mut [&mut out[..]]);
}

/// What a device sends, one batch for each block: each event on its offset in
/// its block (the blocks are 512 frames).
struct Script(Vec<Vec<UmpAt>>);

impl Script {
    fn new(batches: Vec<Vec<UmpAt>>) -> Self {
        Self(batches)
    }
}

/// An ingest node with one device slot for each script, and the inlet each
/// device's thread has pushed its script into, every event for its frame; an
/// inlet holds it until its block.
struct Ingest {
    node: MidiIngestNode,
    inlets: Vec<EventInletNode>,
}

impl Ingest {
    fn new(script: Script) -> Self {
        Self::devices(vec![script])
    }

    fn devices(scripts: Vec<Script>) -> Self {
        let inlets = scripts
            .iter()
            .map(|script| {
                let (inlet, mut writer) = EventInletNode::pair(EventCapacity::of(4096));
                for (block, batch) in script.0.iter().enumerate() {
                    for &(offset, ump) in batch {
                        let frame = Frame::new(512 * block as u64 + u64::from(offset.get()));
                        writer.push(Stamp::Frame(frame), ump).expect("room");
                    }
                }
                inlet
            })
            .collect();
        Self {
            node: MidiIngestNode::new(NonZeroUsize::new(scripts.len()).expect("a script")),
            inlets,
        }
    }

    fn hearing(mut self, ports: MidiInputPorts) -> Self {
        self.node = self.node.hearing(ports);
        self
    }

    fn with_mpe(mut self, mode: tutti_midi_protocol::MpeMode) -> Self {
        self.node = self.node.with_mpe(mode);
        self
    }
}

/// `ump` at frame `offset`, which may lie past the 512-frame blocks these
/// tests run: it is built against a block long enough to hold it.
fn at(offset: usize, ump: Ump) -> UmpAt {
    (Offset::of(Samples(offset), Samples(1 << 20)), ump)
}

/// A note-on with 7-bit velocity 100, widened (M2-115), at `offset`.
fn note(channel: u8, note: u8, offset: usize) -> UmpAt {
    at(
        offset,
        note_on(
            MidiGroup::FIRST,
            MidiChannel::of(channel),
            note,
            tutti_fixtures::midi::wide_velocity(100),
        ),
    )
}

/// A MIDI 1.0 CC.
fn cc_ev(channel: u8, control: CCNumber, value: u8) -> UmpAt {
    cc_at(0, channel, control, value)
}

/// A MIDI 1.0 CC at frame `offset`.
fn cc_at(offset: usize, channel: u8, control: CCNumber, value: u8) -> UmpAt {
    at(
        offset,
        m1_cc(MidiChannel::of(channel), control, U7::of(value)),
    )
}

/// **Each event leaves on its channel's port, in offset order.** A note on
/// channel 9 before one on channel 0 in arrival order but after it in time,
/// and a Timing Clock (no channel): channel 0's on port 0 at 10, channel
/// 9's on port 9 at 40, the clock on the channelless port at 20.
#[test]
fn each_event_leaves_on_its_channels_port() {
    let clock = at(20, timing_clock(MidiGroup::FIRST));
    let wire = Script::new(vec![vec![note(9, 64, 40), note(0, 60, 10), clock]]);
    let (_ed, mut exec, _c, seen) = ingest(Ingest::new(wire), MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    let got: Vec<(usize, u64)> = seen.lock().unwrap().iter().map(|s| (s.0, s.1)).collect();
    assert_eq!(got, vec![(0, 10), (9, 40), (CHANNELLESS_PORT, 20)]);
}

/// **A Group's channels leave on ports of their own** (M2-104 section 2.1.2,
/// Appendix B.1): channel 1 of Group 1 and channel 1 of Group 2 arrive on
/// different ports, so a synth wired to one hears the other never; a Group
/// the node was not built to hear is not delivered; the fork of such a node
/// has the same ports.
/// Spec: m2-104/2.1.2#2
#[test]
fn a_groups_channels_leave_on_their_own_ports() {
    let group = |g: u8, channel: u8, n: u8| {
        at(
            0,
            note_on(
                MidiGroup::of(g),
                MidiChannel::of(channel),
                n,
                tutti_fixtures::midi::wide_velocity(100),
            ),
        )
    };
    let ports = MidiInputPorts::through(MidiGroup::of(1));
    let wire = Script::new(vec![vec![
        group(0, 1, 60),
        group(1, 1, 61),
        group(2, 1, 62),
    ]]);
    let (live, mut exec, _c, seen) =
        ingest(Ingest::new(wire).hearing(ports), ports.count() as u16, 512);
    block(&mut exec, 512);
    let got: Vec<(usize, u8)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| (s.0, note_of(s.2).unwrap()))
        .collect();
    assert_eq!(got, vec![(1, 60), (MIDI_INPUT_PORTS + 1, 61)]);
    let prepare = *live.prepare();
    assert!(
        live.fork(ForkTarget::Master, ForkMode::Live, prepare)
            .is_ok()
    );
}

/// **An (N)RPN run split across blocks assembles into one controller.**
/// The parameter select arrives in one block, its Data Entry (CC 6, then the
/// CC 38 that sends it, M2-104 D.3.3) in the next: one Registered Controller
/// arrives, on channel 3's port.
#[test]
fn an_rpn_run_split_across_blocks_is_assembled() {
    let wire = Script::new(vec![
        vec![
            cc_ev(3, CCNumber::RPN_MSB, 0),
            cc_ev(3, CCNumber::RPN_LSB, 6),
        ],
        vec![
            cc_ev(3, CCNumber::DATA_ENTRY, 10),
            cc_ev(3, CCNumber::DATA_ENTRY_LSB, 0),
        ],
    ]);
    let (_ed, mut exec, _c, seen) = ingest(Ingest::new(wire), MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let (port, frame, ev) = seen[0];
    assert_eq!((port, frame), (3, 512));
    // MIDI 2.0 channel voice (type 4), Registered Controller (status 0x2).
    assert!(
        matches!(
            ev.decode(),
            Ok(Message::Midi2(_, Voice2::Registered { .. }))
        ),
        "{ev:?}"
    );
}

/// A Registered Controller on channel 3, as a received `Voice2` says.
fn is_registered(ev: Ump) -> bool {
    matches!(
        ev.decode(),
        Ok(Message::Midi2(_, Voice2::Registered { .. }))
    )
}

/// **Two devices' (N)RPN runs do not meet** (M2-104 D.3.3 gives a run to the
/// one controller that sends it: a Data Entry completes the parameter *its*
/// sender selected). Device 0 selects RPN 0/6 on channel 3 and device 1 sends a
/// Data Entry on channel 3 with no select of its own: no Registered
/// Controller is made.
/// Alone on one device, the same two messages make one.
#[test]
fn two_devices_runs_do_not_meet() {
    let select = vec![vec![
        cc_ev(3, CCNumber::RPN_MSB, 0),
        cc_ev(3, CCNumber::RPN_LSB, 6),
    ]];
    let entry = vec![vec![
        cc_ev(3, CCNumber::DATA_ENTRY, 10),
        cc_ev(3, CCNumber::DATA_ENTRY_LSB, 0),
    ]];
    // One device sending both: the run completes.
    let together: Vec<Vec<UmpAt>> = vec![select[0].iter().chain(&entry[0]).copied().collect()];
    let (_ed, mut exec, _c, seen) = ingest(
        Ingest::new(Script::new(together)),
        MIDI_INPUT_PORTS as u16,
        512,
    );
    block(&mut exec, 512);
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|s| is_registered(s.2))
            .count(),
        1,
        "one device's select and entry make a run"
    );

    // Two devices, one sending each.
    let (_ed, mut exec, _c, seen) = ingest(
        Ingest::devices(vec![Script::new(select), Script::new(entry)]),
        MIDI_INPUT_PORTS as u16,
        512,
    );
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.iter().filter(|s| is_registered(s.2)).count(),
        0,
        "device 1's Data Entry did not complete device 0's select: {seen:?}"
    );
}

/// **A slot a new device takes starts empty**: device 0 selects an RPN in
/// block 0, a new device then opens the slot (the inlets bump its epoch, which
/// the node follows with no call from the owner), and the Data Entry in block 1
/// finds no select to complete, so no Registered Controller is made; with no
/// new device the same Data Entry completes the run.
#[test]
fn a_slot_a_new_device_takes_forgets_the_previous_devices_half_sent_run() {
    let script = || {
        Script::new(vec![
            vec![
                cc_ev(3, CCNumber::RPN_MSB, 0),
                cc_ev(3, CCNumber::RPN_LSB, 6),
            ],
            vec![
                cc_ev(3, CCNumber::DATA_ENTRY, 10),
                cc_ev(3, CCNumber::DATA_ENTRY_LSB, 0),
            ],
        ])
    };
    for reopened in [false, true] {
        let inlets = DeviceInlets::new(NonZeroUsize::MIN, EventCapacity::of(64), Duration::ZERO);
        let Ingest {
            node,
            inlets: nodes,
        } = Ingest::new(script());
        let node = node.following(&inlets).expect("one slot each");
        let (_ed, mut exec, _controls, seen) = ingest(
            Ingest {
                node,
                inlets: nodes,
            },
            MIDI_INPUT_PORTS as u16,
            512,
        );
        block(&mut exec, 512);
        let _device = reopened.then(|| inlets.open().expect("a free slot"));
        block(&mut exec, 512);
        let made = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| is_registered(s.2))
            .count();
        assert_eq!(made, usize::from(!reopened), "reopened: {reopened}");
    }
}

/// An ingest node that follows `inlets`, over the script's one slot.
fn following(script: Script, inlets: &DeviceInlets) -> Ingest {
    let Ingest {
        node,
        inlets: nodes,
    } = Ingest::new(script);
    Ingest {
        node: node.following(inlets).expect("one slot each"),
        inlets: nodes,
    }
}

/// A System message of `group`.
fn system(group: MidiGroup, message: tutti_ump::System) -> UmpAt {
    at(0, Ump::from(Message::System(group, message)))
}

/// A short description of what a sink heard, for an assertion that reads:
/// `(frame, what)`, `what` being `on N`, `off N`, `cc C V` or `bend V` of a
/// MIDI 2.0 channel voice message, `other` for the rest.
fn heard(seen: &Seen) -> Vec<(u64, String)> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|&(_, frame, ump)| {
            let what = match (ump.key_event(), ump.decode()) {
                (Some(KeyEvent::On(k)), _) => format!("on {}", k.note().get()),
                (Some(KeyEvent::Off(k)), _) => format!("off {}", k.note().get()),
                (_, Ok(Message::Midi2(_, Voice2::ControlChange { controller, value }))) => {
                    format!("cc {} {:#x}", controller.get(), value.get())
                }
                (_, Ok(Message::Midi2(_, Voice2::PitchBend(v)))) => format!("bend {:#x}", v.get()),
                _ => "other".to_string(),
            };
            (frame, what)
        })
        .collect()
}

/// **A device that lets go leaves nothing held downstream** (MIDI 1.0
/// Detailed Specification: p. 24, all notes are turned off by individual Note
/// Offs; A-5, a pedal down defeats them; p. 31 and RP-015, the pedals and the
/// pitch wheel return to normal). Device 0 holds a key, the sustain pedal and
/// a bend off centre, then drops its input: the next block's first frame
/// carries the Note Off, the pedal-up and the centred bend, in that order,
/// and its later events are not heard. When another device takes the slot,
/// the same release comes first and then the new device is heard.
#[test]
fn a_device_that_lets_go_has_what_it_held_released() {
    for replaced in [false, true] {
        let inlets = DeviceInlets::new(NonZeroUsize::MIN, EventCapacity::of(64), Duration::ZERO);
        let device = inlets.open().expect("a free slot");
        let bend = at(30, pitch_bend(G, MidiChannel::of(0), 0xC000_0000));
        let wire = Script::new(vec![
            vec![note(0, 60, 10), cc_at(20, 0, CCNumber::SUSTAIN, 127), bend],
            vec![note(0, 61, 5)],
        ]);
        let (_ed, mut exec, _c, seen) =
            ingest(following(wire, &inlets), MIDI_INPUT_PORTS as u16, 512);
        block(&mut exec, 512);
        assert_eq!(heard(&seen).len(), 3, "the key, the pedal, the bend");
        seen.lock().unwrap().clear();
        drop(device);
        let _next = replaced.then(|| inlets.open().expect("the slot is free again"));
        block(&mut exec, 512);
        let mut expected = vec![
            (512, "off 60".to_string()),
            (512, "cc 64 0x0".to_string()),
            (512, "bend 0x80000000".to_string()),
        ];
        if replaced {
            expected.push((517, "on 61".to_string()));
        }
        assert_eq!(heard(&seen), expected, "replaced: {replaced}");
    }
}

/// **Active Sensing: once an FE is seen, a message is expected within 330 ms
/// and the cable's notes end when none comes** (MIDI 1.0 Detailed
/// Specification p. 32). At 48 kHz the limit is 15,840 frames: an FE and a key
/// at frame 0, silence after, and the Note Off is at frame 15,840 exactly (the
/// frame the time ran out at, inside the block), not a block earlier; the FE
/// itself is not sent on. A message at frame 15,000 moves the lapse a limit on
/// from it.
#[test]
fn a_cable_that_goes_quiet_after_active_sensing_has_its_notes_ended() {
    let sense = system(G, tutti_ump::System::ActiveSensing);
    let blocks = |last: u64| (last / 512 + 2) as usize;
    for (extra, ran_out) in [(None, 15_840u64), (Some(15_000usize), 30_840)] {
        let mut batch = vec![sense, note(0, 60, 0)];
        batch.extend(extra.map(|f| cc_at(f, 0, CCNumber::of(1), 1)));
        let (_ed, mut exec, _c, seen) = ingest(
            Ingest::new(Script::new(vec![batch])),
            MIDI_INPUT_PORTS as u16,
            512,
        );
        for _ in 0..blocks(ran_out) {
            block(&mut exec, 512);
        }
        let got = heard(&seen);
        let off = got.iter().find(|(_, w)| w == "off 60");
        assert_eq!(off.map(|o| o.0), Some(ran_out), "{got:?}");
        assert_eq!(
            got.iter().filter(|(_, w)| w == "other").count(),
            0,
            "the FE is not sent on: {got:?}"
        );
    }
}

/// **A device that never sends an Active Sensing is never judged** (p. 32:
/// a device that never receives Active Sensing operates normally):
/// the same key and silence, no FE, and the key is still held after 40
/// blocks.
#[test]
fn a_cable_with_no_active_sensing_is_never_judged_gone() {
    let (_ed, mut exec, _c, seen) = ingest(
        Ingest::new(Script::new(vec![vec![note(0, 60, 0)]])),
        MIDI_INPUT_PORTS as u16,
        512,
    );
    for _ in 0..40 {
        block(&mut exec, 512);
    }
    assert_eq!(heard(&seen), vec![(0, "on 60".to_string())]);
}

/// **System Reset ends what the cable held, forgets its half-sent runs and is
/// not echoed** (p. 33: turn voices off, reset all controllers, clear running
/// status; the message is never echoed). A key, the
/// sustain pedal and an RPN select on Group 1 and a key on Group 2, then an FF
/// on Group 1 at frame 100: Group 1's key ends and its pedal lifts at frame
/// 100, the FF is sent on to no one, the Data Entry after it completes no
/// RPN, and Group 2's key is not touched.
#[test]
fn a_system_reset_ends_its_cables_notes_and_runs_and_is_not_echoed() {
    let g2 = MidiGroup::of(1);
    let two = MidiInputPorts::through(g2);
    let reset = at(100, Ump::from(Message::System(G, tutti_ump::System::Reset)));
    let other_note = at(
        15,
        note_on(
            g2,
            MidiChannel::of(0),
            70,
            tutti_fixtures::midi::wide_velocity(100),
        ),
    );
    let wire = Script::new(vec![vec![
        note(0, 60, 10),
        other_note,
        cc_at(20, 0, CCNumber::SUSTAIN, 127),
        cc_at(30, 0, CCNumber::RPN_MSB, 0),
        cc_at(31, 0, CCNumber::RPN_LSB, 6),
        reset,
        cc_at(200, 0, CCNumber::DATA_ENTRY, 10),
        cc_at(201, 0, CCNumber::DATA_ENTRY_LSB, 0),
    ]]);
    let (_ed, mut exec, _c, seen) = ingest(Ingest::new(wire).hearing(two), two.count() as u16, 512);
    block(&mut exec, 512);
    let got = heard(&seen);
    let after: Vec<_> = got.iter().filter(|(f, _)| *f >= 100).cloned().collect();
    assert_eq!(
        after,
        vec![(100, "off 60".to_string()), (100, "cc 64 0x0".to_string())],
        "{got:?}"
    );
    assert!(
        got.contains(&(15, "on 70".to_string())),
        "Group 2 untouched"
    );
    assert!(
        !seen.lock().unwrap().iter().any(|s| is_registered(s.2)),
        "the select did not survive the reset"
    );
}

/// A node follows inlets with the slots it has inputs for, and refuses others.
#[test]
fn a_node_refuses_inlets_with_another_number_of_slots() {
    let inlets = DeviceInlets::new(
        NonZeroUsize::new(2).unwrap(),
        EventCapacity::of(64),
        Duration::ZERO,
    );
    assert_eq!(
        MidiIngestNode::new(NonZeroUsize::new(3).unwrap())
            .following(&inlets)
            .unwrap_err(),
        SlotCountMismatch {
            node: NonZeroUsize::new(3).unwrap(),
            inlets: NonZeroUsize::new(2).unwrap()
        }
    );
    assert!(
        MidiIngestNode::new(NonZeroUsize::new(2).unwrap())
            .following(&inlets)
            .is_ok()
    );
}

/// **The devices merge in the listener's fan-in**: two devices' notes on one
/// channel arrive on that channel's port, in time order.
#[test]
fn two_devices_notes_merge_on_the_channels_port() {
    let (_ed, mut exec, _c, seen) = ingest(
        Ingest::devices(vec![
            Script::new(vec![vec![note(0, 60, 30)]]),
            Script::new(vec![vec![note(0, 64, 10)]]),
        ]),
        MIDI_INPUT_PORTS as u16,
        512,
    );
    block(&mut exec, 512);
    let got: Vec<(usize, u64, u8)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| (s.0, s.1, note_of(s.2).unwrap()))
        .collect();
    assert_eq!(got, vec![(0, 10, 64), (0, 30, 60)]);
}

/// **A requested MPE mode is adopted once and holds.** With the lower zone
/// requested, a note on member channel 1 binds it; a channel bend there in
/// each of the next two blocks folds into a per-note bend on the note. The
/// request latches, so it is read every block. Adopting it sends the zone's
/// default bend ranges once, in the first block (thirteen registered
/// controllers: the master's range, and on each of six members the channel's
/// and the per-note one), after the All Sound Off and Reset All
/// Controllers of the seven channels that entered MPE control (M1-100
/// section 2.2.3).
#[test]
fn a_requested_mpe_mode_is_adopted_once() {
    let member = MidiChannel::of(1);
    let bend = at(0, pitch_bend(MidiGroup::FIRST, member, 0xC000_0000));
    let wire = Script::new(vec![
        vec![at(
            0,
            note_on(
                MidiGroup::FIRST,
                member,
                60,
                tutti_fixtures::midi::wide_velocity(100),
            ),
        )],
        vec![bend],
        vec![bend],
    ]);
    let (_ed, mut exec, controls, seen) = ingest(Ingest::new(wire), MIDI_INPUT_PORTS as u16, 512);
    let mode = tutti_fixtures::midi::lower_zone(6);
    controls.set_mpe_mode(mode);
    for _ in 0..3 {
        block(&mut exec, 512);
    }
    let seen = seen.lock().unwrap();
    // MIDI 2.0 channel voice (type 4), Registered Controller (status 0x2).
    let is_rpn = |ev: &Ump| {
        matches!(
            (*ev).decode(),
            Ok(Message::Midi2(_, Voice2::Registered { .. }))
        )
    };
    let is_stop = |ev: &Ump| {
        matches!(
            (*ev).decode(),
            Ok(Message::Midi2(_, Voice2::ControlChange { controller, .. }))
                if controller == CCNumber::ALL_SOUND_OFF || controller == CCNumber::RESET_ALL
        )
    };
    let resets: Vec<u64> = seen.iter().filter(|s| is_rpn(&s.2)).map(|s| s.1).collect();
    assert_eq!(
        resets,
        vec![0; 13],
        "the ranges, once, at frame 0: {seen:?}"
    );
    let stops: Vec<u64> = seen.iter().filter(|s| is_stop(&s.2)).map(|s| s.1).collect();
    assert_eq!(stops, vec![0; 14], "the stops, once, at frame 0: {seen:?}");
    let music: Vec<_> = seen
        .iter()
        .filter(|s| !is_rpn(&s.2) && !is_stop(&s.2))
        .collect();
    let folded: Vec<Option<u8>> = music[1..].iter().map(|s| note_of(s.2)).collect();
    assert_eq!(folded, vec![Some(60), Some(60)], "{seen:?}");
}

/// **A device's MPE Profile zones switch that device's rules alone** (M2-120
/// section 3.3, Table 15). Two devices each send Reset All Controllers on
/// member channel 2 in three blocks. The Profile is enabled on the first
/// device before block 1 and disabled before block 2: the Profile makes
/// Reset All Controllers on a Member Channel prohibited, so the first device's
/// is ignored in block 1 alone, and the second device's, with no zones, is
/// heard in every block. Enabling sends the zone's channels All Sound Off and
/// Reset All Controllers once, ahead of the block's events.
#[test]
fn a_devices_profile_zones_switch_its_rules_alone() {
    let reset = |frame: usize| cc_at(frame, 2, CCNumber::RESET_ALL, 0);
    let device = || Script::new(vec![vec![reset(0)], vec![reset(1)], vec![reset(2)]]);
    let inlets = DeviceInlets::new(
        NonZeroUsize::new(2).unwrap(),
        EventCapacity::of(64),
        Duration::ZERO,
    );
    let (first, _second) = (inlets.open().unwrap(), inlets.open().unwrap());
    let Ingest {
        node,
        inlets: nodes,
    } = Ingest::devices(vec![device(), device()]);
    let node = Ingest {
        node: node.following(&inlets).expect("two slots each"),
        inlets: nodes,
    };
    let (_ed, mut exec, controls, seen) = ingest(node, MIDI_INPUT_PORTS as u16, 512);
    let zone = MpeZones::single(
        MpeZoneConfig::profile(
            MidiChannel::of(0),
            4,
            PitchBendSensitivity::MPE_PROFILE_DEFAULT,
        )
        .unwrap(),
    );
    let group = MidiGroup::FIRST;
    block(&mut exec, 512);
    controls.set_mpe_profile(first.slot(), group, Some(zone));
    block(&mut exec, 512);
    controls.set_mpe_profile(first.slot(), group, None);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let is_reset = |ev: &Ump| {
        matches!(
            (*ev).decode(),
            Ok(Message::Midi2(_, Voice2::ControlChange { controller, .. }))
                if controller == CCNumber::RESET_ALL
        )
    };
    // Block 1's frames are 512 to 1023: the Reset All Controllers of the
    // zone's four channels at its first frame, and the second device's own at
    // frame 513 alone.
    let block_one: Vec<(usize, u64)> = seen
        .iter()
        .filter(|s| (512..1024).contains(&s.1) && is_reset(&s.2))
        .map(|s| (s.0, s.1))
        .collect();
    assert_eq!(
        block_one,
        [(0, 512), (1, 512), (2, 512), (2, 513), (3, 512)],
        "{seen:?}"
    );
    let heard = |first: u64| {
        seen.iter()
            .filter(|s| (first..first + 512).contains(&s.1) && s.0 == 2 && is_reset(&s.2))
            .count()
    };
    assert_eq!(heard(0), 2, "both devices before the Profile");
    // Both devices again, and the Reset All Controllers that leaving the
    // Profile sends to the channel.
    assert_eq!(heard(1024), 3);
}

/// Clearing a slot's zones sets each of its Groups to none: the device's
/// Reset All Controllers on a Member Channel, ignored under the Profile, is
/// heard again.
#[test]
fn clearing_a_slots_profiles_returns_it_to_the_rules_it_had() {
    let inlets = DeviceInlets::new(NonZeroUsize::MIN, EventCapacity::of(64), Duration::ZERO);
    let device = inlets.open().unwrap();
    let reset = cc_at(1, 2, CCNumber::RESET_ALL, 0);
    let (_ed, mut exec, controls, seen) = ingest(
        following(Script::new(vec![vec![], vec![reset], vec![reset]]), &inlets),
        MIDI_INPUT_PORTS as u16,
        512,
    );
    let zone = MpeZones::single(
        MpeZoneConfig::profile(
            MidiChannel::of(0),
            4,
            PitchBendSensitivity::MPE_PROFILE_DEFAULT,
        )
        .unwrap(),
    );
    // The first block takes the device's slot over; the zones come after it.
    block(&mut exec, 512);
    controls.set_mpe_profile(device.slot(), MidiGroup::FIRST, Some(zone));
    block(&mut exec, 512);
    controls.clear_mpe_profiles(device.slot());
    block(&mut exec, 512);
    let heard_at = |frame: u64| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|s| {
                s.1 == frame
                    && matches!(
                        s.2.decode(),
                        Ok(Message::Midi2(_, Voice2::ControlChange { controller, .. }))
                            if controller == CCNumber::RESET_ALL
                    )
            })
            .count()
    };
    assert_eq!(heard_at(513), 0, "ignored while the Profile is enabled");
    assert_eq!(heard_at(1025), 1, "heard once the zones are cleared");
}

/// **A host-set mode's reset goes ahead of the block's own events.** The
/// lower zone of six members is requested, and the adopting block polls a
/// bend range (RPN #00/00, 12 semitones) on member 2 at frame 0. Both the
/// reset (±48 per member) and the wire's range (±12 per member) land on
/// frame 0; a receiver reading each member's port in order must end at the
/// wire's ±12, which came after the mode it was sent in.
#[test]
fn a_host_set_modes_reset_goes_ahead_of_the_blocks_events() {
    use tutti_fixtures::midi::BendRanges;
    use tutti_midi_protocol::PitchBendSensitivity;
    use tutti_ump::Rpn;
    let range = at(
        0,
        registered_rpn(
            MidiGroup::FIRST,
            MidiChannel::of(2),
            Rpn::PITCH_BEND_SENSITIVITY,
            PitchBendSensitivity::of(12).to_rpn_bits(),
        ),
    );
    let wire = Script::new(vec![vec![range]]);
    let (_ed, mut exec, controls, seen) = ingest(Ingest::new(wire), MIDI_INPUT_PORTS as u16, 512);
    controls.set_mpe_mode(tutti_fixtures::midi::lower_zone(6));
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let mut ranges = BendRanges::default();
    let mut per_member = 0;
    for (_, frame, ev) in seen.iter() {
        if ranges.observe(ev) {
            assert_eq!(*frame, 0);
            per_member += 1;
        }
    }
    assert_eq!(
        per_member, 12,
        "the reset's six and the wire's six: {seen:?}"
    );
    for channel in 1..=6 {
        assert_eq!(
            ranges.semitones(tutti_ump::MidiChannel::of(channel)),
            12.0,
            "channel {channel}"
        );
    }
}

/// **A member channel's bend range reaches every member's port as a per-note
/// range, without losing the events polled after it.** With a lower zone of
/// seven members, a MIDI 1.0 RPN 0 run (12 semitones) on member 2 and then a
/// note on member 1: ports 1 to 7 each receive RPN #00/07 at 12 semitones,
/// and port 1 the note too.
#[test]
fn a_member_bend_range_fans_out_over_the_zones_ports() {
    use tutti_fixtures::midi::BendRanges;
    let wire = Script::new(vec![vec![
        cc_ev(2, CCNumber::RPN_MSB, 0),
        cc_ev(2, CCNumber::RPN_LSB, 0),
        cc_ev(2, CCNumber::DATA_ENTRY, 12),
        cc_ev(2, CCNumber::DATA_ENTRY_LSB, 0),
        note(1, 60, 5),
    ]]);
    let node = Ingest::new(wire).with_mpe(tutti_fixtures::midi::lower_zone(7));
    let (_ed, mut exec, _c, seen) = ingest(node, MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let mut ranges = BendRanges::default();
    let mut range_ports = Vec::new();
    for (port, _, ev) in seen.iter() {
        if ranges.observe(ev) {
            assert_eq!(ev.channel().map(u8::from), Some(*port as u8));
            range_ports.push(*port);
        }
    }
    assert_eq!(range_ports, (1..=7).collect::<Vec<_>>(), "{seen:?}");
    for channel in 1..=7 {
        assert_eq!(
            ranges.semitones(tutti_ump::MidiChannel::of(channel)),
            12.0,
            "channel {channel}"
        );
    }
    let notes: Vec<(usize, u64)> = seen
        .iter()
        .filter(|s| matches!(s.2.key_event(), Some(KeyEvent::On(_))))
        .map(|s| (s.0, s.1))
        .collect();
    assert_eq!(notes, vec![(1, 5)]);
}

/// **A translation with no room is dropped whole, and counted.** With a
/// lower zone of seven members, a block polls 507 notes on the master, a
/// MIDI 1.0 RPN 0 run (12 semitones, sent at its CC 38) on member 2, and a
/// note on member 1: 512 polled events. The run fans out to seven, which do
/// not fit after the 507 notes (512 slots), so all seven are dropped and
/// counted, and the note after them still goes out: 508 events, no per-note
/// range.
#[test]
fn a_fan_out_with_no_room_is_dropped_whole_and_counted() {
    use tutti_fixtures::midi::BendRanges;
    // The run is sent after the notes: an inlet delivers in time order.
    let mut batch: Vec<UmpAt> = (0..507).map(|i| note(0, 60, i)).collect();
    batch.extend([
        cc_at(508, 2, CCNumber::RPN_MSB, 0),
        cc_at(508, 2, CCNumber::RPN_LSB, 0),
        cc_at(508, 2, CCNumber::DATA_ENTRY, 12),
        cc_at(508, 2, CCNumber::DATA_ENTRY_LSB, 0),
        note(1, 61, 509),
    ]);
    assert_eq!(batch.len(), MIDI_INPUT_EVENT_CAPACITY);
    let node = Ingest::new(Script::new(vec![batch])).with_mpe(tutti_fixtures::midi::lower_zone(7));
    let (_ed, mut exec, controls, seen) = ingest(node, MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let mut ranges = BendRanges::default();
    assert!(
        !seen.iter().any(|s| ranges.observe(&s.2)),
        "no range went out"
    );
    assert_eq!(seen.len(), 508);
    assert!(seen.iter().any(|s| s.0 == 1 && note_of(s.2) == Some(61)));
    assert_eq!(controls.losses().refused, 7);
}

/// **A note-off has room when a block is full of other messages.** 504
/// notes on the master, a bend-range run on member 2 (sent at its CC 38)
/// that fans out to seven (504 + 7 leave one of the 512 the other messages
/// may use) and four note-offs on
/// member 1: all four go out, in the room past the capacity, and nothing is
/// dropped or counted. A note-on in their place would be dropped.
#[test]
fn a_note_off_has_room_past_the_capacity() {
    let mut batch: Vec<UmpAt> = (0..504).map(|i| note(0, 60, i)).collect();
    batch.extend([
        cc_ev(2, CCNumber::RPN_MSB, 0),
        cc_ev(2, CCNumber::RPN_LSB, 0),
        cc_ev(2, CCNumber::DATA_ENTRY, 12),
        cc_ev(2, CCNumber::DATA_ENTRY_LSB, 0),
    ]);
    batch.extend((0..4).map(|k| {
        at(
            508 + k,
            note_off(MidiGroup::FIRST, MidiChannel::of(1), 61 + k as u8, 0),
        )
    }));
    assert_eq!(batch.len(), MIDI_INPUT_EVENT_CAPACITY);
    let node = Ingest::new(Script::new(vec![batch])).with_mpe(tutti_fixtures::midi::lower_zone(7));
    let (_ed, mut exec, controls, seen) = ingest(node, MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.iter().filter(|s| s.2.is_note_off()).count(), 4);
    // The run went out too: the block is full of other messages.
    assert_eq!(seen.len(), 504 + 7 + 4);
    assert_eq!(controls.losses().refused, 0);
}

/// **An input node's fork reads no wire.**
#[test]
fn an_input_nodes_fork_is_silent() {
    let wire = Script::new(vec![vec![note(0, 60, 0)]]);
    let (live, _exec, _c, seen) = ingest(Ingest::new(wire), MIDI_INPUT_PORTS as u16, 512);
    let prepare = *live.prepare();
    let (_fork, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect("forks");
    block(&mut fork_exec, 512);
    assert!(seen.lock().unwrap().is_empty());
}

/// **An event tap hands MIDI back on the graph's clock, and counts what a
/// full ring drops.** Five blocks of 256 queued notes into a ring of 1 024
/// nobody drains: 256 dropped; what is drained is the first 1 024, the
/// first on its frame (block 0, offset 7) and the last of them on block 3's
/// (frame 1 536 plus offset 8).
#[test]
fn an_event_tap_hands_midi_back_and_counts_drops() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples(512)),
    ));
    let (inlet, mut keys) = EventInletNode::pair(EventCapacity::of(256));
    ed.insert(NodeKey(1), "keys", inlet);
    let mut out = common::sink(&mut ed, 0, &[(1, 0)]);
    ed.commit().unwrap();
    for k in 0..5u64 {
        // The inlet holds each event for its frame: offset 7, then 8, in
        // block `k`.
        for i in 0..256u64 {
            let frame = Frame::new(512 * k + 7 + i / 128);
            keys.push(Stamp::Frame(frame), note(0, 60, 0).1).unwrap();
        }
        exec.process(512, &TransportPosition::default(), &[], &mut []);
    }
    assert_eq!(out.dropped(), 256);
    let mut buf = vec![(0, Ump([0; 4])); 2048];
    assert_eq!(out.poll_into(&mut buf), 1024);
    assert_eq!(buf[0].0, 7);
    assert_eq!(buf[1023].0, 3 * 512 + 8);
}

/// **A start inside a block clocks from its frame.** Stopped at beat 0 for
/// 100 frames, then rolling at 120 BPM: Start and the first tick at frame
/// 100, the next tick 1 000 frames on (a 24-PPQN tick at 48 kHz, the rate
/// `prepare` gave the master).
#[test]
fn a_start_inside_a_block_clocks_from_its_frame() {
    let (_ed, mut exec, clock, seen) = rig(
        ClockNode::new(tutti_ump::MidiGroup::FIRST, tutti_types::SampleRate::SR_48K),
        1,
        2048,
    );
    clock.set_enabled(true);
    let stopped = TransportPosition::new(false, Bpm::of(120.0), Beat(0.0), None);
    let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat(0.0), None);
    let mut changes = TransportChanges::NONE;
    changes
        .push(Offset::of(Samples(100), Samples(2048)), rolling)
        .unwrap();
    let mut out = vec![0.0f32; 2048];
    exec.process_with_changes(2048, &stopped, &changes, &[], &mut [&mut out[..]]);
    let got: Vec<(u64, Ump)> = seen.lock().unwrap().iter().map(|s| (s.1, s.2)).collect();
    assert_eq!(
        got,
        vec![
            (100, start(G)),
            (100, timing_clock(G)),
            (1_100, timing_clock(G))
        ]
    );
}

/// **A clock's fork sends nothing.**
#[test]
fn a_clocks_fork_sends_nothing() {
    let (live, _exec, clock, seen) = rig(
        ClockNode::new(tutti_ump::MidiGroup::FIRST, tutti_types::SampleRate::SR_48K),
        1,
        512,
    );
    clock.set_enabled(true);
    let prepare = *live.prepare();
    let (_fork, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect("forks");
    let mut out = vec![0.0f32; 512];
    fork_exec.process(
        512,
        &TransportPosition::new(true, Bpm::of(120.0), Beat(0.0), None),
        &[],
        &mut [&mut out[..]],
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// **A block with more clock events than the port holds sends the first
/// [`CLOCK_EVENT_CAPACITY`](tutti_midi_runtime::CLOCK_EVENT_CAPACITY) and
/// counts the rest.**
///
/// 300 BPM with MTC on at 48 kHz is 120 ticks and 100 quarter-frames a
/// second, so one 65 536-frame block (1.37 s) produces more events than the
/// port's 256. The same span in four quarter-length blocks fits every event,
/// so it is the reference total: what one long block sent plus what it
/// counted must equal it, exactly.
#[test]
fn a_clock_flood_past_the_port_capacity_is_counted() {
    use tutti_midi_runtime::CLOCK_EVENT_CAPACITY;
    const LONG: usize = 65_536;
    const BPM: f64 = 300.0;

    let run = |blocks: &[usize]| -> (usize, u64) {
        let (_ed, mut exec, clock, seen) = rig(
            ClockNode::new(tutti_ump::MidiGroup::FIRST, tutti_types::SampleRate::SR_48K),
            1,
            LONG,
        );
        clock.set_enabled(true);
        clock.set_send_mtc(true);
        let mut frame = 0usize;
        for &len in blocks {
            let beat = frame as f64 / RATE * BPM / 60.0;
            let rolling = TransportPosition::new(true, Bpm::of(BPM), Beat(beat), None);
            let mut out = vec![0.0f32; len];
            exec.process(len, &rolling, &[], &mut [&mut out[..]]);
            frame += len;
        }
        let sent = seen.lock().unwrap().len();
        (sent, clock.losses().refused)
    };

    let (whole, none) = run(&[LONG / 4; 4]);
    assert_eq!(none, 0, "a quarter-length block fits every event");
    assert!(
        whole > CLOCK_EVENT_CAPACITY,
        "the span produces more than one block can carry ({whole})"
    );

    let (sent, dropped) = run(&[LONG]);
    assert_eq!(sent, CLOCK_EVENT_CAPACITY, "one block sends up to the cap");
    assert_eq!(
        sent as u64 + dropped,
        whole as u64,
        "every event past the cap is counted"
    );
}

/// **A transport change on the frame of a loop wrap sends one Song Position,
/// on that frame.** Looping beats 1.0..1.1 at 120 BPM and 48 kHz (24 000
/// frames a beat), the loop wraps on frame 2 400; a change of the transport
/// (to the same rolling one, at the loop's start) cuts the block there. The
/// first piece ends on the wrap, the second starts on the wrapped beat and
/// has no wrap of its own. Block start: Song Position, Continue (beat 1.0 is
/// no Start) and the tick; ticks every 1 000 frames; then on 2 400 the
/// wrap's Song Position and the tick on the loop's start.
#[test]
fn a_segment_boundary_on_a_wrap_sends_the_song_position_once() {
    use tutti_graph::LoopRange;
    // The loop starts at beat 1, a quarter note: 4 MIDI beats (sixteenths).
    const LOOP_START: u16 = 4;
    let (_ed, mut exec, clock, seen) = rig(
        ClockNode::new(tutti_ump::MidiGroup::FIRST, tutti_types::SampleRate::SR_48K),
        1,
        4096,
    );
    clock.set_enabled(true);
    let looping = Some(LoopRange {
        start: Beat(1.0),
        end: Beat(1.1),
    });
    let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat(1.0), looping);
    let mut changes = TransportChanges::NONE;
    changes
        .push(Offset::of(Samples(2_400), Samples(4_000)), rolling)
        .unwrap();
    let mut out = vec![0.0f32; 4_000];
    exec.process_with_changes(4_000, &rolling, &changes, &[], &mut [&mut out[..]]);
    let got: Vec<(u64, Ump)> = seen.lock().unwrap().iter().map(|s| (s.1, s.2)).collect();
    assert_eq!(
        got,
        vec![
            (0, song_position(G, LOOP_START)),
            (0, continue_msg(G)),
            (0, timing_clock(G)),
            (1_000, timing_clock(G)),
            (2_000, timing_clock(G)),
            (2_400, song_position(G, LOOP_START)),
            (2_400, timing_clock(G)),
            (3_400, timing_clock(G)),
        ]
    );
}

/// **A short loop in a long block overflows the port, counted, and the
/// blocks it is cut into lose nothing.** A loop of 47.3 frames (beat 0 to
/// 47.3 / 24 000) at 120 BPM sends a Song Position and a tick on its start
/// at every wrap: about 340 wraps in 16 384 frames, so about 680 events
/// against the port's 256. One block sends the first 256 and counts the
/// rest, without panicking; the same span in blocks of 1 024 (each under the
/// cap) loses none, and what the one block sent plus what it counted is what
/// the blocks sent, exactly.
#[test]
fn a_short_loop_in_a_long_block_is_counted_past_the_cap() {
    use tutti_graph::LoopRange;
    use tutti_midi_runtime::CLOCK_EVENT_CAPACITY;
    use tutti_types::FrameClock;
    const TOTAL: usize = 16_384;
    let end = 47.3 / 24_000.0;

    let run = |len: usize| -> (usize, u64) {
        let (_ed, mut exec, clock, seen) = rig(
            ClockNode::new(tutti_ump::MidiGroup::FIRST, tutti_types::SampleRate::SR_48K),
            1,
            TOTAL,
        );
        clock.set_enabled(true);
        let looping = Some(LoopRange {
            start: Beat(0.0),
            end: Beat(end),
        });
        let region = tutti_types::LoopRange::new(Beat::new(0.0), Beat::new(end));
        let mut host = FrameClock::new(Beat(0.0), Bpm::of(120.0), SampleRate::of(RATE));
        for _ in 0..TOTAL / len {
            let rolling = TransportPosition::new(true, Bpm::of(120.0), host.beat(), looping);
            let mut out = vec![0.0f32; len];
            exec.process(len, &rolling, &[], &mut [&mut out[..]]);
            host.advance(Samples(len), region);
        }
        let sent = seen.lock().unwrap().len();
        (sent, clock.losses().refused)
    };

    let (cut, none) = run(1_024);
    assert_eq!(none, 0, "blocks of 1 024 fit every event");
    assert!(cut > 2 * CLOCK_EVENT_CAPACITY, "{cut} events in all");
    let (sent, dropped) = run(TOTAL);
    assert_eq!(sent, CLOCK_EVENT_CAPACITY);
    assert_eq!(sent as u64 + dropped, cut as u64);
}

/// **A clock node's master is built with a real rate and its Group, never a
/// placeholder of zero**: the controls read both back before any prepare, and a
/// fork keeps the Group.
#[test]
fn a_clock_node_holds_its_group_and_rate_from_construction() {
    let group = MidiGroup::masked(2);
    let node = ClockNode::new(group, SampleRate::SR_44K1);
    let master = node.into_parts().controls;
    assert_eq!(master.group(), group);
    assert_eq!(master.sample_rate(), SampleRate::SR_44K1);
}

/// The MPE transform node over a script: one batch of MIDI 1.0 or 2.0 events a
/// block, pushed through an event inlet into the node's input, and a sink that
/// logs what the node sends.
fn transform(
    mode: tutti_midi_protocol::MpeMode,
    script: Script,
) -> (
    Editor,
    Executor,
    tutti_midi_runtime::MpeTransformControls,
    Seen,
) {
    let (inlet, mut writer) = EventInletNode::pair(EventCapacity::of(4096));
    for (block, batch) in script.0.iter().enumerate() {
        for &(offset, ump) in batch {
            let frame = Frame::new(512 * block as u64 + u64::from(offset.get()));
            writer.push(Stamp::Frame(frame), ump).expect("room");
        }
    }
    rig_with(MpeTransformNode::new(mode), 1, 512, |ed| {
        ed.insert(NodeKey(3), "inlet", inlet);
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(1),
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(3),
                port: 0,
            }),
        );
    })
}

/// What a sink saw, as `(channel, message)`.
fn voices(seen: &Seen) -> Vec<(u8, Voice2)> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|s| match s.2.decode() {
            Ok(Message::Midi2(address, voice)) => (address.channel.get(), voice),
            other => panic!("{other:?} is not MIDI 2.0 channel voice"),
        })
        .collect()
}

/// **A clip's stream gets the transform**: a note on member channel 1 and a bend
/// there (both MIDI 1.0, as a file's are) come out as the note and a per-note
/// bend on it; the manager's Damper Pedal comes out on the manager and on each
/// of the zone's member channels (M1-100 section 2.3.1).
#[test]
fn the_transform_node_folds_a_member_and_fans_out_a_managers_pedal() {
    let member = MidiChannel::of(1);
    let wire = Script::new(vec![vec![
        at(
            0,
            tutti_fixtures::midi::m1_note_on(member, NoteNumber::of(60), U7::of(100)),
        ),
        at(
            10,
            tutti_fixtures::midi::m1(
                member,
                tutti_ump::Voice1::PitchBend(U14::new(0x3FFF).unwrap()),
            ),
        ),
        cc_at(20, 0, CCNumber::SUSTAIN, 127),
    ]]);
    let (_ed, mut exec, _c, seen) = transform(tutti_fixtures::midi::lower_zone(3), wire);
    block(&mut exec, 512);
    // The zone's default bend ranges go first, ahead of the Group's first event:
    // the master's, and on each of three members the channel's and the per-note one.
    let all = voices(&seen);
    let ranges = all
        .iter()
        .take_while(|(_, v)| matches!(v, Voice2::Registered { .. }))
        .count();
    assert_eq!(ranges, 7, "{all:?}");
    let got: Vec<_> = all
        .into_iter()
        .filter(|(_, v)| !matches!(v, Voice2::Registered { .. }))
        .collect();
    assert!(matches!(got[0], (1, Voice2::NoteOn { .. })), "{got:?}");
    assert!(
        matches!(got[1], (1, Voice2::PerNotePitchBend { note, .. }) if note.get() == 60),
        "{got:?}"
    );
    let pedals: Vec<u8> = got
        .iter()
        .filter(|(_, v)| matches!(v, Voice2::ControlChange { controller, .. } if *controller == CCNumber::SUSTAIN))
        .map(|(c, _)| *c)
        .collect();
    assert_eq!(pedals, [0, 1, 2, 3]);
}

/// **A mode set through the controls is adopted at the next block, once**: the
/// transform starts disabled, so a member's bend passes as a channel bend; the
/// lower zone then set, the zone's reset ranges go first (thirteen registered
/// controllers, as the ingest node sends), once however many blocks follow, and
/// the member's bend then folds per-note.
#[test]
fn a_mode_set_through_the_transforms_controls_is_adopted_once() {
    let member = MidiChannel::of(1);
    let bend = at(0, pitch_bend(MidiGroup::FIRST, member, 0xC000_0000));
    let note = at(
        0,
        note_on(
            MidiGroup::FIRST,
            member,
            60,
            tutti_fixtures::midi::wide_velocity(100),
        ),
    );
    let (_ed, mut exec, controls, seen) = transform(
        tutti_midi_protocol::MpeMode::Disabled,
        Script::new(vec![vec![bend], vec![note, bend], vec![bend]]),
    );
    block(&mut exec, 512);
    assert!(matches!(voices(&seen)[..], [(1, Voice2::PitchBend(_))]));
    seen.lock().unwrap().clear();
    controls.set_mode(tutti_fixtures::midi::lower_zone(6));
    assert_eq!(controls.mode(), tutti_fixtures::midi::lower_zone(6));
    block(&mut exec, 512);
    block(&mut exec, 512);
    let got = voices(&seen);
    let registered = got
        .iter()
        .filter(|(_, v)| matches!(v, Voice2::Registered { .. }))
        .count();
    assert_eq!(registered, 13, "{got:?}");
    let per_note: Vec<u8> = got
        .iter()
        .filter(|(_, v)| matches!(v, Voice2::PerNotePitchBend { .. }))
        .map(|(c, _)| *c)
        .collect();
    assert_eq!(per_note, [1, 1], "{got:?}");
}

/// **A reset transform starts its Groups at the zone's ranges again**: a reset
/// receiver is back at its own defaults (M1-100 section 2.2.5's ranges are the
/// zone's, not the receiver's), so the transform's next event in a Group goes
/// behind the seven ranges of a three-member lower zone again, as its first did;
/// a second event with no reset between goes alone.
#[test]
fn a_reset_transform_sends_the_zones_ranges_again() {
    use tutti_graph::{Event, Node as _, Offset, contract::Direct};
    let pedal = Event::midi(
        Offset::ZERO,
        tutti_fixtures::midi::cc(MidiGroup::FIRST, MidiChannel::FIRST, CCNumber::SUSTAIN, 0),
    );
    let mut d = Direct::new(
        tutti_midi_runtime::MpeTransformNode::new(tutti_fixtures::midi::lower_zone(3)),
        SampleRate::SR_48K,
        256,
    );
    let ranges = |d: &mut Direct<tutti_midi_runtime::MpeTransformNode>| {
        d.events(0, &[pedal]);
        let _ = d.block();
        d.events_out(0)
            .iter()
            .filter(|e| {
                matches!(e.kind, tutti_graph::EventKind::Midi(u)
                    if matches!(u.decode(), Ok(Message::Midi2(_, Voice2::Registered { .. }))))
            })
            .count()
    };
    assert_eq!(ranges(&mut d), 7, "the first event");
    assert_eq!(ranges(&mut d), 0, "told once");
    d.node.reset();
    assert_eq!(ranges(&mut d), 7, "after the reset");
}
