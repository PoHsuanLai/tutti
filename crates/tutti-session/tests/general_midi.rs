//! `Session::add_general_midi` wires what it says: read back from the graph's
//! own topology (which port feeds which), not from a render.

use std::sync::Arc;

use tutti_graph::NodeKey;
use tutti_session::{GeneralMidi, GraphSource, Session};
use tutti_sf2::SoundFont;
use tutti_soundfont::{Settings, SoundFontUnit};
use tutti_types::{ChannelLayout, SampleRate, Samples, Tail};

const RATE: SampleRate = SampleRate(48_000.0);

fn offline_session() -> Session {
    Session::builder()
        .build_offline(RATE, ChannelLayout::STEREO, Samples(128))
        .expect("an offline session builds")
}

fn unit() -> SoundFontUnit {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path).expect("the test font is in the checkout");
    let font = Arc::new(SoundFont::new(&mut file).expect("the test font parses"));
    SoundFontUnit::new(font, &Settings::new(RATE)).expect("builds")
}

fn source(session: &Session, node: NodeKey, port: usize) -> GraphSource {
    session.graph_ref().source(node, port)
}

/// The setup's nodes are the nodes it names, with the shapes the wiring needs:
/// the chorus stereo in and out, the reverb mono in and stereo out, the sum six
/// in (three stereo sources) and two out.
///
/// Mutation (run): `ChannelSumNode::new(2, ..)` for the sum fails the sum's
/// input count.
#[test]
fn the_setup_adds_a_chorus_a_reverb_and_a_sum_of_the_right_widths() {
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
        (graph.node_inputs(gm.sum()), graph.node_outputs(gm.sum())),
        (6, 2)
    );
    assert_eq!(
        (graph.node_inputs(gm.out()), graph.node_outputs(gm.out())),
        (2, 2),
        "the fader"
    );
}

/// Each send goes to its effect, and each effect and the dry pair to its own
/// source of the sum, left before right: the unit's outputs 2 and 3 are the
/// chorus's two inputs, 4 the reverb's one, and the sum reads the dry strip,
/// the chorus and the reverb in that order.
///
/// Mutations (run): the chorus fed from outputs 0 and 1 (the dry pair) fails the
/// first two rows; the sum's sources 1 and 2 swapped fails the later rows.
#[test]
fn the_sends_feed_the_effects_and_the_effects_feed_the_sum() {
    let mut session = offline_session();
    let gm = session.add_general_midi(unit()).expect("a SoundFont unit");
    let unit = gm.instrument().node();
    let strip = gm.instrument().out();
    assert_eq!(source(&session, gm.chorus(), 0), GraphSource::node(unit, 2));
    assert_eq!(source(&session, gm.chorus(), 1), GraphSource::node(unit, 3));
    assert_eq!(source(&session, gm.reverb(), 0), GraphSource::node(unit, 4));
    let expected = [
        GraphSource::node(strip, 0),
        GraphSource::node(strip, 1),
        GraphSource::node(gm.chorus(), 0),
        GraphSource::node(gm.chorus(), 1),
        GraphSource::node(gm.reverb(), 0),
        GraphSource::node(gm.reverb(), 1),
    ];
    for (port, want) in expected.into_iter().enumerate() {
        assert_eq!(source(&session, gm.sum(), port), want, "sum input {port}");
    }
    // The fader reads the sum, which is `out()`.
    assert_eq!(
        source(&session, gm.out(), 0),
        GraphSource::node(gm.sum(), 0)
    );
    assert_eq!(
        source(&session, gm.out(), 1),
        GraphSource::node(gm.sum(), 1)
    );
    // The strip carries the dry pair: outputs 0 and 1 of the unit.
    assert_eq!(source(&session, strip, 0), GraphSource::node(unit, 0));
    assert_eq!(source(&session, strip, 1), GraphSource::node(unit, 1));
}

/// Nothing is wired to the graph's outputs: the mix is the host's to place, so
/// a second setup does not take the first's.
///
/// Mutation (run): `set_output_source(0, ..)` in the helper fails.
#[test]
fn the_setup_does_not_take_the_graphs_outputs() {
    let mut session = offline_session();
    session.add_general_midi(unit()).expect("a SoundFont unit");
    for channel in 0..session.graph_ref().outputs() {
        assert_eq!(
            session.graph_ref().output_source(channel),
            GraphSource::Silence
        );
    }
}

/// Every stage declares its own tail, so an export keeps the release and then
/// the room: the unit's is the release alone, the chorus's the 5 ms of its
/// line, the reverb's a bound over its ring-out (about 1.5 s at 48 kHz), and
/// the sum's none.
///
/// Mutation (run): the reverb's tail quartered fails the reverb row.
#[test]
fn every_stage_declares_its_own_tail() {
    let mut session = offline_session();
    let gm = session.add_general_midi(unit()).expect("a SoundFont unit");
    session.update();
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
    assert_eq!(frames(gm.chorus()), 240);
    assert!((1.0..2.0).contains(&seconds(gm.reverb())));
    assert_eq!(graph.node_tail(gm.sum()), Tail::None);
    assert_eq!(graph.node_tail(gm.out()), Tail::None);
}

/// The ring-out of 3a holds against the per-node tails: a note let go through
/// the whole setup (unit, chorus, reverb, sum) is heard no later than the sum
/// of the tails along the path says, the unit's release, the longest of the
/// chorus's and reverb's, and no sooner than the release alone could end it (the
/// reverb rings on past the voice, so the room is in the figure).
///
/// The font is one zone with a 0.25 s release (-2400 timecents), the sends at
/// full level (CC 91 and CC 93 at 127) so both effects carry the note. "Heard"
/// is the last frame at or above 1e-4 (-80 dBFS) on either channel.
///
/// Mutation (run): the reverb's tail quartered in `ReverbNode::shape` leaves the
/// note ringing past the sum and fails the upper bound. The unit's own tail is
/// pinned by `tutti-soundfont`'s `ring_out` suite.
#[test]
fn a_note_through_the_setup_rings_out_within_the_summed_tails() {
    use tutti_fixtures::sf2::{Inst, Preset, bytes, generator};
    use tutti_types::{At, InterleavedMut};
    use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, U7, Ump, Voice1};

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
        .set_output_source(0, GraphSource::node(gm.out(), 0));
    session
        .graph()
        .set_output_source(1, GraphSource::node(gm.out(), 1));
    session.update();
    let tail = |node| match session.graph_ref().node_tail(node) {
        Tail::Finite(n) => n.get(),
        other => panic!("{node:?}: {other:?}"),
    };
    let summed = tail(gm.instrument().node()) + tail(gm.chorus()).max(tail(gm.reverb()));
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
                controller: CCNumber::new(controller).unwrap(),
                value: U7::new(127).unwrap(),
            },
        );
        handle
            .send(At::NextBlock, Ump::from(message))
            .expect("room");
    }
    handle
        .note_on(At::NextBlock, channel, 69, 100)
        .expect("room");
    let mut heard = Vec::new();
    let block = |session: &mut Session, heard: &mut Vec<f32>| {
        let mut buf = vec![0.0f32; 128 * 2];
        session.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        session.update();
        heard.extend(buf.chunks(2).map(|f| f[0].abs().max(f[1].abs())));
    };
    while heard.len() < 48_000 {
        block(&mut session, &mut heard);
    }
    let off = heard.len();
    handle.note_off(At::NextBlock, channel, 69).expect("room");
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
            .set_output_source(c, GraphSource::node(gm.out(), c));
    }
    session.update();
    (session, gm)
}

fn peak_of_block(session: &mut Session) -> f32 {
    use tutti_types::InterleavedMut;
    let mut buf = vec![0.0f32; 128 * 2];
    session.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
    session.update();
    buf.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// The fader after the sum takes the effects with it: after the dry note has
/// ended and only the reverb rings (heard, before the mute), muting the setup
/// makes the mix exact silence from the second block on; unmuting brings it
/// back. The instrument's own gain is the dry level and does not touch the
/// effects.
///
/// Mutations (run): `out()` returning the sum (the fader bypassed) fails the
/// silence; the mute written to the volume param instead fails it too.
#[test]
fn muting_the_setup_silences_the_wet_path_too() {
    use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, U7, Ump, Voice1};
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
                controller: CCNumber::new(controller).unwrap(),
                value: U7::new(127).unwrap(),
            },
        );
        handle.send(at, Ump::from(message)).expect("room");
    };
    cc(91);
    handle
        .note_on(at, MidiChannel::FIRST, 69, 100)
        .expect("room");
    for _ in 0..200 {
        peak_of_block(&mut session);
    }
    handle.note_off(at, MidiChannel::FIRST, 69).expect("room");
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

    gm.set_muted(true);
    for _ in 0..2 {
        peak_of_block(&mut session);
    }
    for block in 0..8 {
        assert_eq!(peak_of_block(&mut session), 0.0, "muted, block {block}");
    }
}

/// Removing the setup takes every node it added, and the graph is as an empty
/// session's: none of the unit, queue, strip, chorus, reverb, sum or fader is
/// left, and the node count is back to what it was.
///
/// Mutations (run): leaving the fader out of `GeneralMidi::remove` fails the
/// fader row; leaving out `remove_instrument` fails the queue and strip rows.
#[test]
fn removing_the_setup_leaves_nothing() {
    let mut session = offline_session();
    let before = session.graph_ref().node_count();
    let gm = session.add_general_midi(unit()).expect("a SoundFont unit");
    let nodes = [
        gm.instrument().node(),
        gm.instrument().queue(),
        gm.instrument().out(),
        gm.chorus(),
        gm.reverb(),
        gm.sum(),
        gm.out(),
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
