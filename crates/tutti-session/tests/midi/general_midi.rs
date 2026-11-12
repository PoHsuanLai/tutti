//! `Session::add_general_midi` wires what it says: read back from the graph's
//! own topology (which port feeds which), not from a render.

use std::sync::Arc;
use tutti_session::InPort;
use tutti_types::MaxBlock;

use tutti_graph::NodeKey;
use tutti_session::{GeneralMidi, NodeMode, OutPort, Root, Session};
use tutti_sf2::SoundFont;
use tutti_soundfont::{Settings, SoundFontUnit};
use tutti_types::{ChannelLayout, SampleRate, Samples, Tail};

const RATE: SampleRate = SampleRate(48_000.0);

fn offline_session() -> Session {
    Session::builder()
        .build_offline(RATE, ChannelLayout::STEREO, MaxBlock::of(Samples(128)))
        .expect("an offline session builds")
}

fn unit() -> SoundFontUnit {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path).expect("the test font is in the checkout");
    let font = Arc::new(SoundFont::new(&mut file).expect("the test font parses"));
    SoundFontUnit::new(font, &Settings::new(RATE)).expect("builds")
}

/// The one source a port holds (these tests wire one).
fn source(session: &Session, node: NodeKey, port: usize) -> OutPort {
    let held = session.graph_ref().sources(InPort::new(node, port));
    assert!(held.len() == 1, "one source a port: {held:?}");
    held[0]
}

/// The setup's nodes are the nodes it names, with the shapes the wiring needs:
/// the chorus stereo in and out, the reverb mono in and stereo out, the fader
/// stereo in and out (its sum is the graph's, so it is two inputs, not six).
#[test]
fn the_setup_adds_a_chorus_a_reverb_and_a_fader_of_the_right_widths() {
    let mut session = offline_session();
    let gm = session.add_general_midi(unit()).expect("a SoundFont unit");
    let graph = session.graph_ref();
    assert_eq!(graph.node_outputs(gm.instrument().node()), 5);
    assert_eq!(
        (
            graph.node_inputs(gm.chorus()),
            graph.node_outputs(gm.chorus())
        ),
        (2, 2)
    );
    assert_eq!(
        (
            graph.node_inputs(gm.reverb()),
            graph.node_outputs(gm.reverb())
        ),
        (1, 2)
    );
    assert_eq!(
        (graph.node_inputs(gm.out()), graph.node_outputs(gm.out())),
        (2, 2),
        "the fader"
    );
}

/// Each send goes to its effect, and each effect and the dry pair to the
/// fader's left and right inputs: the unit's outputs 2 and 3 are the chorus's
/// two inputs, 4 the reverb's one, and the fader's port 0 sums the dry strip's,
/// the chorus's and the reverb's left, port 1 their right (a set: the graph
/// sums it, in no order the test relies on).
#[test]
fn the_sends_feed_the_effects_and_the_effects_feed_the_fader() {
    let mut session = offline_session();
    let gm = session.add_general_midi(unit()).expect("a SoundFont unit");
    let unit = gm.instrument().node();
    let strip = gm.instrument().out();
    assert_eq!(
        source(&session, gm.chorus().erase(), 0),
        OutPort::new(unit, 2)
    );
    assert_eq!(
        source(&session, gm.chorus().erase(), 1),
        OutPort::new(unit, 3)
    );
    assert_eq!(
        source(&session, gm.reverb().erase(), 0),
        OutPort::new(unit, 4)
    );
    for (port, channel) in [(0, 0), (1, 1)] {
        let mut held = session.graph_ref().sources(InPort::new(gm.out(), port));
        let mut want = vec![
            OutPort::new(strip, channel),
            OutPort::new(gm.chorus(), channel),
            OutPort::new(gm.reverb(), channel),
        ];
        let key = |s: &OutPort| format!("{s:?}");
        held.sort_by_key(key);
        want.sort_by_key(key);
        assert_eq!(held, want, "fader input {port}");
    }
    // The strip carries the dry pair: outputs 0 and 1 of the unit.
    assert_eq!(source(&session, strip.erase(), 0), OutPort::new(unit, 0));
    assert_eq!(source(&session, strip.erase(), 1), OutPort::new(unit, 1));
}

/// Nothing is wired to the graph's outputs: the mix is the host's to place, so
/// a second setup does not take the first's.
#[test]
fn the_setup_does_not_take_the_graphs_outputs() {
    let mut session = offline_session();
    session.add_general_midi(unit()).expect("a SoundFont unit");
    for channel in 0..session.graph_ref().outputs() {
        assert_eq!(session.graph_ref().sources(Root::output(channel)), []);
    }
}

/// Every stage declares its own tail, so an export keeps the release and then
/// the room: the unit's is the release alone, the chorus's the 5 ms of its
/// line, the reverb's a bound over its ring-out (about 1.5 s at 48 kHz), and
/// the fader's none (the graph's sum has no tail of its own).
#[test]
fn every_stage_declares_its_own_tail() {
    let mut session = offline_session();
    let gm = session.add_general_midi(unit()).expect("a SoundFont unit");
    let _ = session.update();
    let graph = session.graph_ref();
    let frames = |node| match graph.node_tail(node) {
        Tail::Finite(n) => n.get(),
        other => panic!("{node:?}: {other:?}"),
    };
    let seconds = |node| frames(node) as f64 / RATE.get();
    assert!(
        seconds(gm.instrument().node()) > 50.0,
        "the longest release in the font"
    );
    assert_eq!(frames(gm.chorus().erase()), 240);
    assert!((1.0..2.0).contains(&seconds(gm.reverb().erase())));
    assert_eq!(graph.node_tail(gm.out()), Tail::None);
}

/// The ring-out of 3a holds against the per-node tails: a note let go through
/// the whole setup (unit, chorus, reverb, the fader's sum) is heard no later than the sum
/// of the tails along the path says, the unit's release, the longest of the
/// chorus's and reverb's, and no sooner than the release alone could end it (the
/// reverb rings on past the voice, so the room is in the figure).
///
/// The font is one zone with a 0.25 s release (-2400 timecents), the sends at
/// full level (CC 91 and CC 93 at 127) so both effects carry the note. "Heard"
/// is the last frame at or above 1e-4 (-80 dBFS) on either channel.
///
/// The unit's own tail is pinned by `tutti-soundfont`'s `ring_out` suite.
#[test]
fn a_note_through_the_setup_rings_out_within_the_summed_tails() {
    use tutti_fixtures::sf2::{Inst, Preset, bytes, generator};
    use tutti_types::{At, InterleavedMut};
    use tutti_ump::{
        Address, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, U7, Ump, Voice1,
    };

    let font = Arc::new(
        SoundFont::new(&mut std::io::Cursor::new(bytes(
            &[Preset {
                name: "p",
                bank: 0,
                program: 0,
                zones: vec![(vec![], 0)],
            }],
            &[Inst {
                name: "i",
                zones: vec![vec![(generator::RELEASE_VOL_ENV, -2400)]],
            }],
        )))
        .expect("the built font parses"),
    );
    let unit = SoundFontUnit::new(font, &Settings::new(RATE)).expect("builds");
    let mut session = offline_session();
    let gm = session.add_general_midi(unit).expect("a SoundFont unit");
    session
        .graph()
        .set_sources(Root::output(0), [OutPort::new(gm.out(), 0)])
        .unwrap();
    session
        .graph()
        .set_sources(Root::output(1), [OutPort::new(gm.out(), 1)])
        .unwrap();
    let _ = session.update();
    let tail = |node| match session.graph_ref().node_tail(node) {
        Tail::Finite(n) => n.get(),
        other => panic!("{node:?}: {other:?}"),
    };
    let summed =
        tail(gm.instrument().node()) + tail(gm.chorus().erase()).max(tail(gm.reverb().erase()));
    let release = tail(gm.instrument().node());
    assert!(
        release > 11_000 && release < 13_000,
        "about 0.25 s: {release}"
    );

    let channel = MidiChannel::FIRST;
    let handle = gm.instrument().clone();
    for controller in [91, 93] {
        let message = Message::Midi1(
            Address {
                group: MidiGroup::FIRST,
                channel,
            },
            Voice1::ControlChange {
                controller: CCNumber::of(controller),
                value: U7::of(127),
            },
        );
        handle
            .send(At::NextBlock, Ump::from(message))
            .expect("room");
    }
    handle
        .note_on(At::NextBlock, channel, NoteNumber::of(69), 0xC800)
        .expect("room");
    let mut heard = Vec::new();
    let block = |session: &mut Session, heard: &mut Vec<f32>| {
        let mut buf = vec![0.0f32; 128 * 2];
        session.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        let _ = session.update();
        heard.extend(buf.chunks(2).map(|f| f[0].abs().max(f[1].abs())));
    };
    while heard.len() < 48_000 {
        block(&mut session, &mut heard);
    }
    let off = heard.len();
    handle
        .note_off(At::NextBlock, channel, NoteNumber::of(69))
        .expect("room");
    while heard.len() < off + summed + 20_000 {
        block(&mut session, &mut heard);
    }
    let last = heard.iter().rposition(|&s| s >= 1e-4).expect("it sounds") + 1;
    let rang = last - off;
    println!("RINGOUT through the setup: rang {rang} frames, summed tails {summed}");
    assert!(rang <= summed + 128, "rang {rang}, declared {summed}");
    assert!(
        rang > release,
        "the room rings on past the voice: {rang} vs {release}"
    );
}

/// A setup on a font of one zone with a 0.25 s release, its mix on the graph's
/// outputs, and a function that renders a block of it and returns the peak of
/// both channels.
fn short_release_setup() -> (Session, GeneralMidi) {
    use tutti_fixtures::sf2::{Inst, Preset, bytes, generator};
    let font = Arc::new(
        SoundFont::new(&mut std::io::Cursor::new(bytes(
            &[Preset {
                name: "p",
                bank: 0,
                program: 0,
                zones: vec![(vec![], 0)],
            }],
            &[Inst {
                name: "i",
                zones: vec![vec![(generator::RELEASE_VOL_ENV, -2400)]],
            }],
        )))
        .expect("the built font parses"),
    );
    let unit = SoundFontUnit::new(font, &Settings::new(RATE)).expect("builds");
    let mut session = offline_session();
    let gm = session.add_general_midi(unit).expect("a SoundFont unit");
    for c in 0..2 {
        session
            .graph()
            .set_sources(Root::output(c), [OutPort::new(gm.out(), c)])
            .unwrap();
    }
    let _ = session.update();
    (session, gm)
}

fn peak_of_block(session: &mut Session) -> f32 {
    use tutti_types::InterleavedMut;
    let mut buf = vec![0.0f32; 128 * 2];
    session.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
    let _ = session.update();
    buf.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// The fader after the sum takes the effects with it: after the dry note has
/// ended and only the reverb rings (heard, before the mute), muting the setup
/// makes the mix exact silence from the second block on (`NodeMode::Muted` on
/// `out()`). The instrument's own gain is the dry level and does not touch the
/// effects.
#[test]
fn muting_the_setup_silences_the_wet_path_too() {
    use tutti_ump::{
        Address, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, U7, Ump, Voice1,
    };
    let (mut session, gm) = short_release_setup();
    let handle = gm.instrument().clone();
    let at = tutti_types::At::NextBlock;
    let cc = |controller: u8| {
        let message = Message::Midi1(
            Address {
                group: MidiGroup::FIRST,
                channel: MidiChannel::FIRST,
            },
            Voice1::ControlChange {
                controller: CCNumber::of(controller),
                value: U7::of(127),
            },
        );
        handle.send(at, Ump::from(message)).expect("room");
    };
    cc(91);
    handle
        .note_on(at, MidiChannel::FIRST, NoteNumber::of(69), 0xC800)
        .expect("room");
    for _ in 0..200 {
        peak_of_block(&mut session);
    }
    handle
        .note_off(at, MidiChannel::FIRST, NoteNumber::of(69))
        .expect("room");
    // 0.25 s release is 12000 frames; the room rings past it.
    for _ in 0..(12_000 * 3 / 2 / 128) {
        peak_of_block(&mut session);
    }
    let wet = peak_of_block(&mut session);
    assert!(
        wet > 1e-4,
        "only the reverb is left, and it is heard: {wet}"
    );

    // The instrument's gain is the dry level: zero leaves the reverb heard.
    handle.set_gain(tutti_types::Amplitude(0.0));
    for _ in 0..2 {
        peak_of_block(&mut session);
    }
    assert!(
        peak_of_block(&mut session) > 1e-4,
        "the dry strip is not the fader"
    );

    session
        .graph()
        .set_mode(gm.out(), NodeMode::Muted)
        .expect("the fader is in the graph");
    for _ in 0..2 {
        peak_of_block(&mut session);
    }
    for block in 0..8 {
        assert_eq!(peak_of_block(&mut session), 0.0, "muted, block {block}");
    }
}

/// Removing the setup takes every node it added, and the graph is as an empty
/// session's: none of the unit, queue, strip, chorus, reverb or fader is
/// left, and the node count is back to what it was.
#[test]
fn removing_the_setup_leaves_nothing() {
    let mut session = offline_session();
    let before = session.graph_ref().node_count();
    let gm = session.add_general_midi(unit()).expect("a SoundFont unit");
    let nodes = [
        gm.instrument().node(),
        gm.instrument().queue().erase(),
        gm.instrument().out().erase(),
        gm.chorus().erase(),
        gm.reverb().erase(),
        gm.out().erase(),
    ];
    assert!(session.graph_ref().node_count() > before);
    for node in nodes {
        assert!(session.graph_ref().contains(node), "{node:?} is in");
    }
    gm.remove(&mut session);
    for node in nodes {
        assert!(!session.graph_ref().contains(node), "{node:?} is left");
    }
    assert_eq!(session.graph_ref().node_count(), before);
}
