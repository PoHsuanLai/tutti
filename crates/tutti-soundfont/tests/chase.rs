//! What a font swap keeps (`Node::chase`, doc 013): the outgoing unit's
//! program, bank, controllers and wheel reach the unit that replaces it, on
//! its first block, whatever that unit is.
//!
//! Against the committed `TimGM6mb.sf2`; a missing fixture fails.

use std::sync::{Arc, Mutex};

use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::{
    Buses, Chase, CrossfadeCurve, Cx, Editor, EventCapacity, EventEdge, EventIn, EventInletNode,
    EventInletWriter, EventKind, EventOut, Executor, Fade, Io, Node, OutPort, Prepare, Shape,
    Source, Status, Transport, Unforkable,
};
use tutti_polysynth::{PolySynth, SynthConfig};
use tutti_sf2::SoundFont;
use tutti_soundfont::{Settings, SoundFontUnit};
use tutti_types::{At, ChannelLayout, MaxBlock, SampleRate, Samples};
use tutti_ump::{Bank, CCNumber, Message, MidiChannel, MidiGroup, U7, Ump, Voice2};

const BLOCK: usize = 256;
const RATE: SampleRate = SampleRate::SR_48K;
const G: MidiGroup = MidiGroup::FIRST;

fn font() -> Arc<SoundFont> {
    let mut file = std::fs::File::open(tutti_fixtures::asset("soundfonts/TimGM6mb.sf2"))
        .expect("the test soundfont");
    Arc::new(SoundFont::new(&mut file).expect("parses"))
}

fn unit() -> SoundFontUnit {
    SoundFontUnit::new(font(), &Settings::new(RATE)).expect("builds")
}

fn cc(channel: u8, controller: u8, value: u8) -> Ump {
    midi::cc(
        G,
        MidiChannel::of(channel),
        CCNumber::of(controller),
        u32::from(value) << 25,
    )
}

fn note_on() -> Ump {
    midi::note_on(G, MidiChannel::of(0), 60, midi::wide_velocity(100))
}

/// An inlet at key 1 feeding the instrument at key 2, whose two main
/// outputs are the graph's.
struct Rig {
    ed: Editor,
    exec: Executor,
    tx: EventInletWriter,
}

impl Rig {
    fn new(instrument: impl tutti_graph::IntoNode) -> Self {
        let (mut ed, exec) = Editor::new(Prepare::new(RATE, MaxBlock::of(Samples(BLOCK))));
        let (inlet, tx) = EventInletNode::pair(EventCapacity::of(256));
        ed.insert(NodeKey(1), "inlet", Unforkable(inlet));
        ed.insert(NodeKey(2), "instrument", instrument);
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(2),
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(1),
                port: 0,
            }),
        );
        ed.spec_mut().topology.set_outputs((0..2).map(|port| {
            Source::Node(OutPort {
                node: NodeKey(2),
                port,
            })
        }));
        ed.commit().expect("commits");
        Self { ed, exec, tx }
    }

    fn send(&mut self, messages: &[Ump]) {
        for &m in messages {
            self.tx.push(At::NextBlock, m).expect("room");
        }
    }

    /// One block; the peak of what it rendered.
    fn block(&mut self) -> f32 {
        let (mut l, mut r) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
        self.exec.process(
            BLOCK,
            &Transport::default(),
            &[],
            &mut [&mut l[..], &mut r[..]],
        );
        l.iter().chain(&r).fold(0.0, |p, s| p.max(s.abs()))
    }

    fn swap(&mut self, instrument: impl tutti_graph::IntoNode) {
        self.ed.insert(NodeKey(2), "instrument", instrument);
        self.ed.commit().expect("commits");
    }
}

/// A successor that writes down what it is sent.
struct Recorder(Arc<Mutex<Vec<Ump>>>);

impl Node for Recorder {
    fn shape(&self) -> Shape {
        Shape::buses(Buses::NONE, Buses::unassigned(ChannelLayout::STEREO)).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for e in io.events(0).iter() {
            if let EventKind::Midi(u) = e.kind {
                self.0.lock().unwrap().push(u);
            }
        }
        for c in 0..2 {
            io.output(c).fill(0.0);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn chase(&self, _: &mut Chase<'_>) {}
}

fn decoded(u: Ump) -> (MidiChannel, Voice2) {
    match u.decode().expect("a packet") {
        Message::Midi2(address, voice) => {
            assert_eq!(address.group, G);
            (address.channel, voice)
        }
        other => panic!("a chase message is MIDI 2.0 channel voice, got {other:?}"),
    }
}

/// The unit hands its successor each touched channel's program and bank, its
/// sends, its controllers and its wheel, as MIDI 2.0 messages in its Group,
/// and nothing for a channel nobody touched.
#[test]
fn the_unit_chases_its_programs_banks_controllers_and_wheels() {
    let mut rig = Rig::new(unit());
    rig.send(&[
        midi::program_change(G, MidiChannel::of(3), 40, Some(2 << 7)),
        cc(3, 7, 90),
        cc(3, 91, 10),
        midi::pitch_bend(G, MidiChannel::of(3), 0xC000_0000),
        // A Bank Select waiting for the next Program Change on channel 5.
        cc(5, 0, 7),
    ]);
    rig.block();
    let seen = Arc::new(Mutex::new(Vec::new()));
    rig.swap(Unforkable(Recorder(Arc::clone(&seen))));
    rig.block();
    let got: Vec<_> = seen.lock().unwrap().iter().map(|&u| decoded(u)).collect();
    let on = |c: u8| MidiChannel::of(c);
    let wants = [
        (
            on(3),
            Voice2::ProgramChange {
                program: U7::of(40),
                bank: Some(Bank {
                    msb: U7::of(2),
                    lsb: U7::of(0),
                }),
            },
        ),
        (on(3), decoded(cc(3, 91, 10)).1),
        (on(3), decoded(cc(3, 7, 90)).1),
        (on(3), Voice2::PitchBend(0xC000_0000)),
        (on(5), decoded(cc(5, 0, 7)).1),
    ];
    for want in &wants {
        assert!(got.contains(want), "{want:?} missing from {got:?}");
    }
    assert_eq!(got.len(), wants.len(), "nothing else: {got:?}");
}

/// A volume of zero set on the old unit silences the new one from its first
/// block (GM: Channel Volume 0 is silence), though the new unit was never told;
/// without it the same note sounds.
#[test]
fn a_volume_set_on_the_old_unit_silences_the_new_one_on_its_first_block() {
    for (volume, sounds) in [(Some(0), false), (None, true)] {
        let mut rig = Rig::new(unit());
        rig.send(&volume.map(|v| cc(0, 7, v)).into_iter().collect::<Vec<_>>());
        rig.block();
        rig.swap(Unforkable(unit()));
        rig.send(&[note_on()]);
        let peak = rig.block();
        assert_eq!(peak > 0.0, sounds, "volume {volume:?}: peak {peak}");
    }
}

/// The same across kinds: a font swapped for a polysynth keeps the controllers
/// (the polysynth reads Channel Volume).
#[test]
fn a_font_swapped_for_a_polysynth_keeps_its_controllers() {
    for (volume, sounds) in [(Some(0), false), (None, true)] {
        let mut rig = Rig::new(unit());
        rig.send(&volume.map(|v| cc(0, 7, v)).into_iter().collect::<Vec<_>>());
        rig.block();
        let synth = PolySynth::new(SynthConfig::default()).expect("builds");
        rig.swap(synth);
        rig.send(&[note_on()]);
        let peak = rig.block();
        assert_eq!(peak > 0.0, sounds, "volume {volume:?}: peak {peak}");
    }
}

/// A crossfade chases too: the incoming unit starts from the outgoing one's
/// volume.
#[test]
fn a_crossfade_between_fonts_chases_the_incoming_unit() {
    let mut rig = Rig::new(unit());
    rig.send(&[cc(0, 7, 0)]);
    rig.block();
    rig.ed
        .replace(
            NodeKey(2),
            Unforkable(unit()),
            Fade::new(Samples(2 * BLOCK), CrossfadeCurve::EqualPower),
        )
        .expect("fits");
    rig.ed.commit().expect("commits");
    rig.send(&[note_on()]);
    // The outgoing unit hears nothing during the fade, and the incoming one
    // was chased to volume 0: the note is silent throughout.
    assert_eq!(rig.block(), 0.0);
    assert_eq!(rig.block(), 0.0);
}
