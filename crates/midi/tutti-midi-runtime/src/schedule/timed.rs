//! [`TimedMidiEvent`]: a MIDI event at a beat.

use tutti_core::Beat;
use tutti_midi_types::ump::MidiEvent;

/// One [`MidiEvent`] tagged with the absolute beat at which it fires.
///
/// What a [`MidiClipNode`](super::MidiClipNode) plays and clip-file import
/// yields, so a parsed file drops straight into a clip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedMidiEvent {
    /// The event itself. Its own `frame_offset` is meaningless in storage —
    /// [`beat`](Self::beat) is the authority, and a reader stamps the offset
    /// from it at poll time.
    pub event: MidiEvent,
    /// Beat position when this event should trigger.
    pub beat: Beat,
}

impl TimedMidiEvent {
    /// A timed event at `beat`. Field-order-independent, so callers never have
    /// to remember whether `beat` or `event` comes first.
    #[inline]
    pub const fn new(beat: Beat, event: MidiEvent) -> Self {
        Self { event, beat }
    }
}

impl From<(f64, MidiEvent)> for TimedMidiEvent {
    /// `(beat, event)` — matches the tuples [`tutti_midi_types::ParsedClipFile::timed`]
    /// yields, so a parsed clip file drops straight into the player/snapshot.
    ///
    /// The bare `f64` is the SMF edge: `ParsedClipFile` divides absolute ticks by
    /// ticks-per-quarter and has no beat vocabulary of its own. Converted here,
    /// once, on the way in.
    #[inline]
    fn from((beat, event): (f64, MidiEvent)) -> Self {
        Self {
            event,
            beat: Beat(beat),
        }
    }
}

impl From<(Beat, MidiEvent)> for TimedMidiEvent {
    #[inline]
    fn from((beat, event): (Beat, MidiEvent)) -> Self {
        Self { event, beat }
    }
}
