//! A held key and a rewire through the session: `Midi::retarget` and
//! `Midi::set_routes` (whose compile `unroute` shares) change a sink's feed in
//! a commit, and the sink that loses the feed hears the note end. The one that
//! gains it hears the later off of a note it never started, which ends nothing.

use std::sync::{Arc, Mutex};
use tutti_types::MaxBlock;

use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Event, EventKind, IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, Status,
};
use tutti_midi_runtime::{MIDI_INPUT_PORTS, TimedMidiEvent};
use tutti_session::{MidiRouteRule, Session, SessionNode};
use tutti_transport::MotionEvent;
use tutti_types::{Beat, BeatDuration, ChannelLayout, InterleavedMut, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup, Ump};

const MF: u16 = 0x8000;
type Seen = Arc<Mutex<Vec<(u64, [u32; 4])>>>;

struct Tap(Seen);

impl Node for Tap {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        for e in io.events(0) {
            if let EventKind::Midi(Ump(w)) = e.kind {
                self.0
                    .lock()
                    .unwrap()
                    .push((cx.env.frame.get() + u64::from(e.offset.get()), w));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}
impl IntoNode for Tap {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        IntoNode::into_parts(tutti_graph::Unforkable(self))
    }
}
impl SessionNode for Tap {}

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

fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

fn tap(s: &mut Session) -> (NodeKey, Seen) {
    let seen = Seen::default();
    let (node, ()) = s.graph().add(Tap(Arc::clone(&seen)));
    (node, seen)
}

fn blocks(s: &Session, n: usize) {
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..n {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
    }
}

fn is_off(w: &[u32; 4]) -> bool {
    Ump(*w).is_note_off()
}
fn ons(seen: &Seen) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, w)| !is_off(w) && matches!(w[0] >> 28, 2 | 4) && (w[0] >> 20) & 0xf == 9)
        .count()
}
fn offs(seen: &Seen) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, w)| is_off(w))
        .count()
}

/// A clip holds note 60 for a long time; mid-note the host re-aims it.
///
/// Mutation (run): `Held::observe` in `tutti-graph` doing nothing -> the sink
/// that loses the feed is never told the note ended -> fails.
#[test]
fn retarget_with_a_held_clip_note() {
    let mut s = offline();
    let (a, a_seen) = tap(&mut s);
    let (b, b_seen) = tap(&mut s);
    let clip = s
        .midi()
        .play_clip(
            a,
            [
                TimedMidiEvent::new(
                    Beat(1.0 / 3.0),
                    midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, MF),
                ),
                TimedMidiEvent::new(
                    Beat(1.0 / 3.0) + BeatDuration(8.0),
                    midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
                ),
            ],
        )
        .unwrap();
    s.update();
    s.transport().motion.try_send(MotionEvent::Play).unwrap();
    blocks(&s, 20); // the note starts at about frame 2,400
    assert_eq!(ons(&a_seen), 1, "A heard the note start");
    s.midi().retarget(clip, b).unwrap();
    s.update();
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
///
/// Mutation (run): `Held::observe` doing nothing -> fails.
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
    s.update();
    mail.lock().unwrap().push([0x2090_3c64, 0, 0, 0]);
    blocks(&s, 2);
    assert_eq!(ons(&a_seen), 1);

    s.midi().set_routes(src, &to(2), None, resolve).unwrap();
    s.update();
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
    s.update();
    blocks(&s, 2);
    assert_eq!(offs(&b_seen), 2, "B is told when the routes go");
}
