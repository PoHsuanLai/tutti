//! [`TimedMidiEvent`]: a MIDI event at a beat.

use tutti_types::Beat;
use tutti_ump::Ump;

/// One [`Ump`] tagged with the absolute beat at which it fires.
///
/// What a [`MidiClipNode`](super::MidiClipNode) plays and clip-file import
/// yields, so a parsed file drops straight into a clip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedMidiEvent {
    /// The event itself. It carries no offset in storage —
    /// [`beat`](Self::beat) is the authority, and a reader stamps the offset
    /// from it at poll time.
    pub event: Ump,
    /// Beat position when this event should trigger.
    pub beat: Beat,
}

impl TimedMidiEvent {
    /// A timed event at `beat`. Field-order-independent, so callers never have
    /// to remember whether `beat` or `event` comes first.
    #[inline]
    #[must_use]
    pub const fn new(beat: Beat, event: Ump) -> Self {
        Self { event, beat }
    }
}

impl From<(f64, Ump)> for TimedMidiEvent {
    /// `(beat, event)`, for a caller that divides ticks itself and has no
    /// `Beat` yet. Converted here, once, on the way in.
    #[inline]
    fn from((beat, event): (f64, Ump)) -> Self {
        Self {
            event,
            beat: Beat(beat),
        }
    }
}

impl From<(Beat, Ump)> for TimedMidiEvent {
    #[inline]
    fn from((beat, event): (Beat, Ump)) -> Self {
        Self { event, beat }
    }
}
