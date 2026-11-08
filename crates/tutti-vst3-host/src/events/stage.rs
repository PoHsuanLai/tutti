//! Staging a block's input events into a plugin's `IEventList`, and reading
//! back what the plugin wrote.

use tutti_plugin_types::{
    NoteExpressionIntValue, NoteExpressionValue, RtMidiEvents, SysExStep, push_sysex,
};
use tutti_vst3_ffi::{EventList, NOTE_EXPRESSION_ROOM, Staging};

use super::midi::{bank_program, midi1_data};
use super::{
    Event, EventExt, MidiEncoder, MidiEvent, Payload, ToEvent, Utf16Chord, Utf16Scale, Utf16Text,
    sample_offset,
};

/// Borrowed bundle of every input event stream staged into a VST3 plugin's event
/// list for one block: MIDI plus the five per-note-expressive families.
#[derive(Clone, Copy, Default, Debug)]
pub struct InputEvents<'a> {
    /// UMP MIDI for the block, in ascending frame-offset order.
    pub midi: &'a [MidiEvent],
    /// Continuous per-note expression values, addressed by note id.
    pub note_expressions: &'a [NoteExpressionValue],
    /// Chord context changes (root, bass, degree mask) for chord-aware plugins.
    pub chords: &'a [Utf16Chord],
    /// Scale/key context changes (root plus a 12-bit degree mask).
    pub scales: &'a [Utf16Scale],
    /// Per-note text annotations; the strings are copied into the event list's
    /// UTF-16 arena during staging.
    pub expr_texts: &'a [Utf16Text],
    /// Stepped per-note expressions carrying an `i64` rather than a normalized
    /// `f64`.
    pub expr_ints: &'a [NoteExpressionIntValue],
}

impl InputEvents<'_> {
    /// True when no stream carries an event this block, so staging can be
    /// skipped entirely.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.midi.is_empty()
            && self.note_expressions.is_empty()
            && self.chords.is_empty()
            && self.scales.is_empty()
            && self.expr_texts.is_empty()
            && self.expr_ints.is_empty()
    }
}

/// Fills a plugin's input [`EventList`] from [`InputEvents`], keeping the
/// MIDI stream's per-note bend ranges across blocks.
#[derive(Debug, Default)]
pub struct Stager {
    encoder: MidiEncoder,
}

impl Stager {
    /// Stages every source into `list`, replacing the previous block.
    ///
    /// MIDI is transcoded ([`MidiEncoder`]) and a multi-packet SysEx stages as
    /// one message at its `END` packet's frame. A dimension VST3 cannot encode
    /// is skipped, not coerced and not counted. Past [`NOTE_EXPRESSION_ROOM`]
    /// expression values, or the list's room in all, the rest are dropped and
    /// counted, except that a note-off takes the place of an event staged before
    /// it. Text for chords, scales and annotations is interned into the arena
    /// after the room check, so a dropped event spends none of it. Events end up
    /// in frame order, stably: on one frame they keep the order of the sources
    /// above and each source's own. Never allocates.
    ///
    /// A plugin thread inside a call on `list` leaves it unstaged, counted
    /// ([`EventList::busy`]), never waited on, and this returns `false`: the
    /// list still holds the last block, so the caller must not hand it to the
    /// plugin.
    #[must_use = "a list that was not staged still holds the last block"]
    pub fn stage(&mut self, list: &EventList, src: &InputEvents<'_>) -> bool {
        let Some(mut stage) = list.try_begin() else {
            return false;
        };
        // In frame order, so a per-note bend sensitivity applies to the bends after it.
        for event in src.midi {
            self.stage_midi(&mut stage, event);
        }
        // The budget keeps the harmony kinds staged after the expressions their room.
        let mut expressions = 0;
        for event in src.note_expressions.iter().filter_map(ToEvent::to_event) {
            if expressions < NOTE_EXPRESSION_ROOM {
                expressions += 1;
                stage.push(event);
            } else {
                stage.count_dropped(1);
            }
        }
        for expr in src.expr_ints {
            if let Some(event) = expr.to_event() {
                stage.push(event);
            }
        }
        for chord in src.chords {
            stage_with_text(&mut stage, |s| chord.to_event(s));
        }
        for scale in src.scales {
            stage_with_text(&mut stage, |s| scale.to_event(s));
        }
        for text in src.expr_texts {
            stage_with_text(&mut stage, |s| text.to_event(s));
        }
        stage.finish();
        true
    }

    /// Stages one MIDI event: a SysEx7 packet through the reassembler (a
    /// completed message stages as one event), anything else through the
    /// encoder.
    fn stage_midi(&mut self, stage: &mut Staging<'_>, event: &MidiEvent) {
        match stage.reassemble(event) {
            SysExStep::NotSysEx => {
                // A Program Change with a bank is three MIDI-1 messages (CC 0, CC 32,
                // PC), each its own event at the same offset.
                if let Some(messages) = bank_program(event) {
                    // All three or none: a bank is not sent without its program.
                    if stage.room() < messages.len() {
                        stage.count_dropped(messages.len() as u64);
                        return;
                    }
                    let offset = sample_offset(event.frame_offset);
                    for bytes in messages {
                        stage.push(midi1_data(offset, bytes));
                    }
                } else if let Some(event) = self.encoder.encode(event) {
                    stage.push(event);
                }
            }
            SysExStep::Held => {}
            SysExStep::Complete(bytes) => {
                stage.push(Event::at(
                    sample_offset(event.frame_offset),
                    Payload::SysEx(bytes),
                ));
            }
        }
    }
}

/// Stages a text-bearing event (`make` interns its text), or drops it, counted,
/// without touching the arena when the list has no room for it.
fn stage_with_text(stage: &mut Staging<'_>, make: impl FnOnce(&mut Staging<'_>) -> Event) {
    if stage.room() > 0 {
        let event = make(stage);
        stage.push(event);
    } else {
        stage.count_dropped(1);
    }
}

/// Drains the plugin's emitted MIDI from its output `list` into `out`. Clears
/// `out` first and never grows it: events past its cap are dropped
/// (`out.is_overflowed()` reports it), a SysEx that does not fit is dropped
/// whole and counted in the list's [`dropped`](EventList::dropped).
pub fn drain_midi(list: &EventList, out: &mut RtMidiEvents) {
    out.clear();
    let Some(refused) = list.try_with_events(|staged| {
        let mut refused = 0;
        for event in staged.events {
            if let Payload::SysEx(message) = event.payload {
                let frame = u32::try_from(event.sample_offset()).unwrap_or(0);
                if !push_sysex(out, frame, &staged.sysex[message.range()]) {
                    refused += 1;
                }
            } else if let Some(midi) = event.to_midi()
                && !out.push(midi)
            {
                break;
            }
        }
        refused
    }) else {
        return;
    };
    list.add_dropped(refused);
}

/// Tests build SDK structs and counts by hand, so they repeat the bounded
/// narrowings the production code justifies; one that overflows fails on its
/// own assertion, and the deny stays tight where the ABI is crossed.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
#[cfg(test)]
mod tests;
