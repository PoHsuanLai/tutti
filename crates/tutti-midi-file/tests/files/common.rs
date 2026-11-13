//! What the file tests share: a track of events and a note.

use tutti_midi_file::{FileEvent, Tick, Track};
use tutti_ump::{Message, MidiChannel, NoteNumber, U7};

/// A track of `events` exactly as given: not sorted, so a test can hand the
/// constructor events out of order.
pub fn unsorted(events: Vec<(Tick, FileEvent)>) -> Track {
    Track {
        end: events.last().map_or(Tick(0), |&(tick, _)| tick),
        events,
        ..Track::new()
    }
}

pub fn track(events: Vec<(u64, FileEvent)>) -> Track {
    unsorted(events.into_iter().map(|(t, e)| (Tick(t), e)).collect())
}

/// A Note On of `key` at velocity 100 on `channel`, in the first Group.
pub fn note(channel: u8, key: u8) -> FileEvent {
    Message::note_on(MidiChannel::of(channel), NoteNumber::of(key), U7::of(100)).into()
}
