//! A held key and a rewire through the session: `Midi::retarget` and
//! `Midi::set_routes` (whose compile `unroute` shares) change a sink's feed in
//! a commit, and the sink that loses the feed hears the note end. The one that
//! gains it hears the later off of a note it never started, which ends nothing.

use std::sync::{Arc, Mutex};
use tutti_types::MaxBlock;

use crate::common::{Seen, blocks, offline, tap};
use tutti_graph::{Cx, Event, IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, Status};
use tutti_midi_runtime::{MIDI_INPUT_PORTS, MidiInputPorts, TimedMidiEvent};
use tutti_session::{MidiRouteRule, Session, SessionNode};
use tutti_transport::MotionEvent;
use tutti_types::{Beat, BeatDuration, ChannelLayout, SampleRate, Samples};
use tutti_ump::{Message, MidiChannel, MidiGroup, NoteNumber, NoteVelocity, Ump, Voice2};

const MF: u16 = 0x8000;
/// Stands in for the hardware input: sends what it is mailed on port 0.
struct Src(Arc<Mutex<Vec<[u32; 4]>>>);
impl Node for Src {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, MIDI_INPUT_PORTS as u16)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for w in self.0.lock().unwrap().drain(..) {
            let _ = io.event_out(0).push(Event::midi(Offset::ZERO, Ump(w)));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}
impl IntoNode for Src {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        IntoNode::into_parts(tutti_graph::Unforkable(self))
    }
}
impl SessionNode for Src {}

fn is_off(w: &[u32; 4]) -> bool {
    Ump(*w).is_note_off()
}
fn ons(seen: &Seen) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, Ump(w))| !is_off(w) && matches!(w[0] >> 28, 2 | 4) && (w[0] >> 20) & 0xf == 9)
        .count()
}
fn offs(seen: &Seen) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, Ump(w))| is_off(w))
        .count()
}

/// A clip holds note 60 for a long time; mid-note the host re-aims it.
#[test]
fn retarget_with_a_held_clip_note() {
    let mut s = offline();
    let (a, a_seen) = tap(&mut s);
    let (b, b_seen) = tap(&mut s);
    s.midi()
        .play_clip(
            a,
            Beat(0.0),
            [
                TimedMidiEvent::new(
                    Beat(1.0 / 3.0),
                    Message::midi2(
                        MidiChannel::FIRST,
                        Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(MF)),
                    ),
                )
                .unwrap(),
                TimedMidiEvent::new(
                    Beat(1.0 / 3.0) + BeatDuration(8.0),
                    Message::midi2(
                        MidiChannel::FIRST,
                        Voice2::note_off(NoteNumber::of(60), NoteVelocity::MIN),
                    ),
                )
                .unwrap(),
            ],
        )
        .unwrap();
    let clip = s.midi().clip(a).expect("the clip a target has");
    let _ = s.update();
    s.transport().motion().try_send(MotionEvent::Play).unwrap();
    blocks(&s, 20); // the note starts at about frame 2,400
    assert_eq!(ons(&a_seen), 1, "A heard the note start");
    s.midi().retarget(clip, b).unwrap();
    let _ = s.update();
    blocks(&s, 20);
    assert_eq!(offs(&a_seen), 1, "A is told the note ended");
    // The clip's own off comes at beat 8.1 (about 194,000 frames): render on.
    blocks(&s, 400);
    assert_eq!(offs(&a_seen), 1, "A is told once");
    assert_eq!(ons(&b_seen), 0, "B never started it");
    assert_eq!(
        offs(&b_seen),
        1,
        "B hears the clip's off for a note it never saw"
    );
}

/// The host's routes move a held key from one sink to another, then to none.
#[test]
fn set_routes_with_a_held_key() {
    let mut s = offline();
    let mail = Arc::new(Mutex::new(Vec::new()));
    let (src, ()) = s.graph().add(Src(Arc::clone(&mail)));
    let (a, a_seen) = tap(&mut s);
    let (b, b_seen) = tap(&mut s);
    let resolve = |k: u8| match k {
        1 => Some(a),
        2 => Some(b),
        _ => None,
    };
    let to = |k: u8| [MidiRouteRule::<u8>::any_channel().to(k)];
    s.midi().set_routes(src, &to(1), None, resolve).unwrap();
    let _ = s.update();
    mail.lock().unwrap().push([0x2090_3c64, 0, 0, 0]);
    blocks(&s, 2);
    assert_eq!(ons(&a_seen), 1);

    s.midi().set_routes(src, &to(2), None, resolve).unwrap();
    let _ = s.update();
    blocks(&s, 2);
    assert_eq!(offs(&a_seen), 1, "A is told the note ended");

    mail.lock().unwrap().push([0x2080_3c00, 0, 0, 0]);
    blocks(&s, 2);
    assert_eq!(offs(&b_seen), 1, "B hears the orphan off");

    // And a key held across the unrouting.
    mail.lock().unwrap().push([0x2090_3d64, 0, 0, 0]);
    blocks(&s, 2);
    assert_eq!(ons(&b_seen), 1);
    s.midi()
        .set_routes(src, &[] as &[MidiRouteRule<u8>], None, resolve)
        .unwrap();
    let _ = s.update();
    blocks(&s, 2);
    assert_eq!(offs(&b_seen), 2, "B is told when the routes go");
}

/// Stands in for a hardware input that hears Groups 1 and 2: sends each packet
/// it is mailed out of the port its address names.
struct GroupSrc(Arc<Mutex<Vec<[u32; 4]>>>, MidiInputPorts);
impl Node for GroupSrc {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, self.1.count() as u16)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for w in self.0.lock().unwrap().drain(..) {
            let ump = Ump(w);
            let port = self.1.port_of(&ump).expect("a Group the node hears");
            let _ = io.event_out(port).push(Event::midi(Offset::ZERO, ump));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}
impl IntoNode for GroupSrc {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        IntoNode::into_parts(tutti_graph::Unforkable(self))
    }
}
impl SessionNode for GroupSrc {}

/// **A route by Group, cut with two notes held across it, hears one Note Off for
/// each at offset 0 of the next block, and then nothing.** Group 2's channel 0
/// reaches `A`; Group 1's reaches nobody (a note of Group 1 is not heard).
/// Two keys go down on Group 2, the routes are cleared and committed, and `A`
/// is told both ended on the first frame of the block the commit lands on
/// (frame 1,024 after two 512-frame blocks), and hears no more however long
/// the keys stay held and whatever Group 1 sends.
#[test]
fn a_group_route_cut_with_keys_held_ends_them_at_the_next_block_then_nothing() {
    let mut s = Session::builder()
        .midi_groups(MidiGroup::of(1))
        .build_offline(
            SampleRate::SR_48K,
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds");
    let ports = s
        .midi_nodes()
        .expect("the builder's MIDI nodes")
        .input_ports;
    let mail = Arc::new(Mutex::new(Vec::new()));
    let (src, ()) = s.graph().add(GroupSrc(Arc::clone(&mail), ports));
    let (a, a_seen) = tap(&mut s);
    let resolve = |k: u8| (k == 1).then_some(a);
    let by_group_2 = [MidiRouteRule::<u8>::any_channel()
        .in_group(MidiGroup::of(1))
        .to(1)];
    s.midi()
        .set_routes(src, &by_group_2, None, resolve)
        .unwrap();
    let _ = s.update();
    {
        let mut mail = mail.lock().unwrap();
        mail.push([0x2190_3c64, 0, 0, 0]); // Group 2, channel 0, note 60 on
        mail.push([0x2190_3e64, 0, 0, 0]); // note 62 on
        mail.push([0x2090_4064, 0, 0, 0]); // Group 1: not routed
    }
    blocks(&s, 2);
    assert_eq!(
        ons(&a_seen),
        2,
        "Group 2's two keys went down, Group 1's did not"
    );

    s.midi()
        .set_routes(src, &[] as &[MidiRouteRule<u8>], None, resolve)
        .unwrap();
    let _ = s.update();
    blocks(&s, 4);
    let offs_seen: Vec<u64> = a_seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, Ump(w))| is_off(w))
        .map(|(frame, _)| *frame)
        .collect();
    assert_eq!(
        offs_seen,
        [1_024, 1_024],
        "one off per held key, each on the first frame of the block after the commit"
    );
    mail.lock().unwrap().push([0x2180_3c00, 0, 0, 0]); // the key comes up later
    mail.lock().unwrap().push([0x2090_4264, 0, 0, 0]); // and Group 1 plays on
    blocks(&s, 4);
    assert_eq!(offs(&a_seen), 2, "and nothing after");
    assert_eq!(ons(&a_seen), 2);
}
