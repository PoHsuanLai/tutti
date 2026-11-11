//! A clip's velocity keeps its full 16-bit range to the event input.
//!
//! The session half of `bevy-tutti`'s `midi_sequence`
//! `velocity_keeps_its_full_width`. Two velocities one LSB apart at 16 bits
//! are the same number at 7, so this fails the moment anything on the
//! `Midi::play_clip` → clip node → event edge path narrows the field (it
//! caught exactly that once: a `note_on_7bit` that crushed the value and
//! widened it back).
//!
//! Mutation (run): `MidiClipNode` sending a 7-bit note on (`note_on_7bit`, since deleted) of the
//! event's note and velocity >> 9 → the two velocities come out equal and
//! the assertion fails.

use std::sync::{Arc, Mutex};

use tutti_fixtures::midi::{note_off, note_on};
use tutti_graph::{Cx, EventKind, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
use tutti_midi_protocol::channel_voice;
use tutti_midi_runtime::TimedMidiEvent;
use tutti_session::{Session, SessionNode};
use tutti_transport::MotionEvent;
use tutti_types::{Beat, BeatDuration, ChannelLayout, InterleavedMut, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup, Ump, Voice2};

/// The mezzo-forte default: MIDI 2.0's center velocity.
const MF: u16 = 0x8000;

type Seen = Arc<Mutex<Vec<(u32, Ump)>>>;

/// A stand-in synth: one MIDI event input, logged.
struct Tap(Seen);

impl Node for Tap {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            if let EventKind::Midi(Ump(data)) = e.kind {
                seen.push((e.offset.get(), Ump(data)));
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

fn note(number: u8, velocity: u16) -> [TimedMidiEvent; 2] {
    [
        TimedMidiEvent::new(
            Beat(0.0),
            note_on(MidiGroup::FIRST, MidiChannel::FIRST, number, velocity),
        ),
        TimedMidiEvent::new(
            Beat(0.0) + BeatDuration(1.0),
            note_off(MidiGroup::FIRST, MidiChannel::FIRST, number, 0),
        ),
    ]
}

#[test]
fn velocity_keeps_its_full_width() {
    let mut s = Session::builder()
        .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds");
    let seen = Seen::default();
    let (tap, ()) = s.graph().add(Tap(Arc::clone(&seen)));
    let mut events = note(60, MF).to_vec();
    events.extend(note(64, MF + 1));
    s.midi().play_clip(tap, events).expect("a tap takes a clip");
    s.update();
    s.transport()
        .motion
        .try_send(MotionEvent::Play)
        .expect("room");
    let mut buf = vec![0.0f32; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));

    let mut velocities: Vec<u16> = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(_, e)| match channel_voice(*e) {
            Some((_, Voice2::NoteOn { velocity, .. })) => Some(velocity),
            _ => None,
        })
        .collect();
    velocities.sort_unstable();
    assert_eq!(velocities, vec![MF, MF + 1]);
}
