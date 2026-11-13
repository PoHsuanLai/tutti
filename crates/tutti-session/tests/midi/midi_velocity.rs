//! A clip's velocity keeps its full 16-bit range to the event input.
//!
//! Two velocities one LSB apart at 16 bits
//! are the same number at 7, so this fails the moment anything on the
//! `Midi::play_clip` → clip node → event edge path narrows the field (it
//! caught exactly that once: a `note_on_7bit` that crushed the value and
//! widened it back).

use crate::common::{MF, Seen, Tap, note, offline};
use std::sync::Arc;
use tutti_midi_protocol::channel_voice;
use tutti_transport::MotionEvent;
use tutti_types::{Beat, BeatDuration, ChannelLayout, InterleavedMut};
use tutti_ump::Voice2;

#[test]
fn velocity_keeps_its_full_width() {
    let mut s = offline();
    let seen = Seen::default();
    let (tap, ()) = s.graph().add(Tap(Arc::clone(&seen)));
    let mut events = note(60, MF, Beat(0.0), BeatDuration(1.0)).to_vec();
    events.extend(note(64, MF + 1, Beat(0.0), BeatDuration(1.0)));
    s.midi()
        .play_clip(tap, Beat(0.0), events)
        .expect("a tap takes a clip");
    let _ = s.update();
    s.transport()
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
    let mut buf = vec![0.0f32; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));

    let mut velocities: Vec<u16> = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(_, e)| match channel_voice(*e) {
            Some((_, Voice2::NoteOn { velocity, .. })) => Some(velocity.get()),
            _ => None,
        })
        .collect();
    velocities.sort_unstable();
    assert_eq!(velocities, vec![MF, MF + 1]);
}
