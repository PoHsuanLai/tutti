//! Route rules through the session: what a rule wires from the hardware
//! input's ports, a union of rules, and a disabled rule.
//!
//! The session half of `bevy-tutti`'s `midi_routes` suite (its ECS half, the
//! rule components, the keyboard entity and the despawn observer, stayed
//! there). The rest of it is in `midi.rs`
//! (`route_rules_wire_the_hardware_input`: one channel's ports and the
//! channelless one, an any-channel rule, the fallback, unrouting) and in
//! `tutti-midi-runtime`'s `compile_routes` tests; what neither had is the
//! union of two rules on one target and a rule that is disabled, which are
//! here.

use std::collections::BTreeSet;
use tutti_types::MaxBlock;

use tutti_graph::NodeKey;
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_midi_runtime::{CHANNELLESS_PORT, MIDI_INPUT_PORTS};
use tutti_session::{Feeder, MidiRouteRule, Session};
use tutti_types::{ChannelLayout, SampleRate, Samples};
use tutti_ump::MidiChannel;

/// A stand-in synth: one MIDI event input.
#[derive(Clone)]
struct Sink;

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn session() -> (Session, NodeKey) {
    let mut s = Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    let input = s.midi_nodes().expect("the builder's MIDI nodes").input;
    let _ = s.update();
    (s, input)
}

/// The input ports that feed `node` by the routes.
fn ports(s: &Session, input: NodeKey, node: NodeKey) -> BTreeSet<u16> {
    s.graph_ref()
        .feed(node, Feeder::Routes)
        .iter()
        .map(|e| {
            assert_eq!(e.node(), input);
            u16::try_from(e.index()).unwrap()
        })
        .collect()
}

fn channelless() -> u16 {
    u16::try_from(CHANNELLESS_PORT).unwrap()
}

/// **Two channel rules on one target take the union of their ports and the
/// channelless one, beside an any-channel rule on another target taking all
/// seventeen.**
#[test]
fn two_rules_on_one_target_take_the_union() {
    let (mut s, input) = session();
    let (lead, _) = s.graph().add(ForkByClone(Sink));
    let (pad, _) = s.graph().add(ForkByClone(Sink));
    s.midi()
        .route(MidiRouteRule::for_channel(MidiChannel::of(2)).to(lead.erase()))
        .unwrap();
    s.midi()
        .route(MidiRouteRule::for_channel(MidiChannel::of(5)).to(lead.erase()))
        .unwrap();
    s.midi()
        .route(MidiRouteRule::any_channel().to(pad.erase()))
        .unwrap();
    assert_eq!(
        ports(&s, input, lead.erase()),
        BTreeSet::from([2, 5, channelless()])
    );
    assert_eq!(
        ports(&s, input, pad.erase()),
        (0..u16::try_from(MIDI_INPUT_PORTS).unwrap()).collect()
    );
}

/// **A disabled rule routes nothing, and arming it wires its target again**,
/// through the host's `set_routes`.
#[test]
fn a_disabled_rule_routes_nothing_until_it_is_armed() {
    let (mut s, input) = session();
    let (lead, _) = s.graph().add(ForkByClone(Sink));
    let armed = MidiRouteRule::for_channel(MidiChannel::FIRST).to(lead.erase());
    s.midi().set_routes(input, [&armed], None, Some).unwrap();
    assert!(!ports(&s, input, lead.erase()).is_empty(), "armed");

    let off = MidiRouteRule::for_channel(MidiChannel::FIRST)
        .to(lead.erase())
        .disabled();
    s.midi().set_routes(input, [&off], None, Some).unwrap();
    assert!(ports(&s, input, lead.erase()).is_empty(), "disabled");

    s.midi().set_routes(input, [&armed], None, Some).unwrap();
    assert!(!ports(&s, input, lead.erase()).is_empty(), "re-armed");
}
