//! What the note-logic tests share: a one-voice patch and a step at a channel.

use tutti_types::Amplitude;
use tutti_ump::{Attribute, MidiChannel, NoteNumber, NoteVelocity, Voice2};
use tutti_voicing::{Command, Patch, VoiceSpec, Voicing};

pub struct One;
impl Patch for One {
    fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
        spec(VoiceSpec {
            group: None,
            scope: 0,
            tag: 0,
        });
    }
}

/// `message` on `channel`, every voice sounding at unit level.
pub fn step(core: &mut Voicing, channel: u8, message: Voice2) -> Vec<Command> {
    core.step(MidiChannel::of(channel), message, &One, |_| {
        Some(Amplitude(1.0))
    })
    .to_vec()
}

/// A Note On of `key` at velocity `0x8000`.
pub fn note_on(key: u8) -> Voice2 {
    Voice2::NoteOn {
        note: NoteNumber::of(key),
        velocity: NoteVelocity::CENTER,
        attribute: Attribute::None,
    }
}
