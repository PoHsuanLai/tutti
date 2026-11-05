//! The [`Vst3Event`] tagged union and its accessors.

use super::*;

/// Safe tagged-enum form of the VST3 `Event` union. See
/// [`Vst3Event::to_midi`] for MIDI conversion. Text-bearing variants reference the owning event
/// list's arena
/// (see [`TextRef`]) so the enum stays `Copy`.
#[derive(Debug, Clone, Copy)]
pub enum Vst3Event {
    /// A note started.
    NoteOn(NoteOnEvent),
    /// A note ended, carrying release velocity.
    NoteOff(NoteOffEvent),
    /// Raw bytes: CC, pitch bend, program change, channel pressure or SysEx.
    Data(DataEvent),
    /// A reassembled SysEx message held in the event list's buffer.
    SysEx(SysExEvent),
    /// Per-note aftertouch.
    PolyPressure(PolyPressureEvent),
    /// A continuous per-note expression value.
    NoteExpression(NoteExpressionValueEvent),
    /// A per-note text annotation; the string lives in the arena.
    NoteExpressionText(NoteExpressionTextEvent),
    /// A chord context change.
    Chord(ChordEvent),
    /// A scale/key context change.
    Scale(ScaleEvent),
    /// A stepped per-note expression carrying an `i64`.
    NoteExpressionInt(NoteExpressionIntValueEvent),
    /// A legacy MIDI CC the plugin emitted back to the host. Output-only.
    LegacyMidiCcOut(LegacyMidiCcOutEvent),
}

impl Vst3Event {
    /// Frame offset within the current processing block, from the underlying
    /// [`EventHeader`].
    pub fn sample_offset(&self) -> i32 {
        self.header().sample_offset
    }

    /// The common [`EventHeader`] of any variant.
    pub fn header(&self) -> &EventHeader {
        match self {
            Vst3Event::NoteOn(e) => &e.header,
            Vst3Event::NoteOff(e) => &e.header,
            Vst3Event::Data(e) => &e.header,
            Vst3Event::SysEx(e) => &e.header,
            Vst3Event::PolyPressure(e) => &e.header,
            Vst3Event::NoteExpression(e) => &e.header,
            Vst3Event::NoteExpressionText(e) => &e.header,
            Vst3Event::Chord(e) => &e.header,
            Vst3Event::Scale(e) => &e.header,
            Vst3Event::NoteExpressionInt(e) => &e.header,
            Vst3Event::LegacyMidiCcOut(e) => &e.header,
        }
    }

    /// Decodes this [`Vst3Event`] into a Tutti UMP [`MidiEvent`], promoting any
    /// MIDI-1 payload to Channel Voice 2. `None` for non-MIDI events. The
    /// encoding direction is the plugin's event list's: it keeps the MIDI
    /// stream's per-note bend ranges, which one event alone cannot carry.
    #[inline]
    #[must_use]
    pub fn to_midi(&self) -> Option<MidiEvent> {
        vst3_to_midi_event(self)
    }
}
