//! [`Event`] to and from the SDK `Event` struct.

use super::{
    Chord, Data, Event, EventHeader, EventKind, ExpressionInt, ExpressionText, ExpressionValue,
    K_DATA_TYPE_MIDI_SYSEX, LegacyCcOut, MAX_EVENT_TEXT_LEN, NoteOff, NoteOn, Payload,
    PolyPressure, Scale, TextArena, TextRef, sdk,
};

/// A member of `event`'s union, which `kind` (its own tag) names as live.
macro_rules! member {
    ($event:expr, $field:ident) => {
        // SAFETY: only evaluated in the arm for the tag that names this member live.
        unsafe { $event.__field0.$field }
    };
}

impl Event {
    /// The SDK `Event` for this event.
    ///
    /// The plugin may read every pointer in it until its `process` call ends,
    /// so they all borrow storage the list does not touch during the block:
    /// `Data.bytes` points into `self`, which sits in the list's event `Vec`
    /// (staged before `getEvent` and not mutated by it), and text and SysEx
    /// bytes point into `text_arena` and `sysex`. A reference outside its
    /// storage becomes a null pointer of length 0.
    #[must_use]
    pub fn to_c<'a>(&'a self, text_arena: &'a [u16], sysex: &'a [u8]) -> sdk::Event {
        let mut out: sdk::Event = crate::sdk::zeroed();
        out.busIndex = self.header.bus_index;
        out.sampleOffset = self.header.sample_offset;
        out.ppqPosition = self.header.ppq_position;
        out.flags = self.header.flags;
        out.r#type = self.payload.kind() as u16;

        let text = |t: &TextRef| match t.slice(text_arena) {
            Some(units) => (units.as_ptr(), u16::try_from(t.len).unwrap_or(u16::MAX)),
            None => (std::ptr::null(), 0),
        };

        match &self.payload {
            Payload::NoteOn(e) => out.__field0.noteOn = e.0,
            Payload::NoteOff(e) => out.__field0.noteOff = e.0,
            Payload::PolyPressure(e) => out.__field0.polyPressure = e.0,
            Payload::ExpressionValue(e) => out.__field0.noteExpressionValue = e.0,
            Payload::ExpressionInt(e) => out.__field0.noteExpressionIntValue = e.0,
            Payload::LegacyCcOut(e) => out.__field0.midiCCOut = e.0,
            Payload::Data(e) => {
                out.__field0.data = sdk::DataEvent {
                    // `size()` never exceeds the inline array (`Data`'s fields are private).
                    size: e.size(),
                    r#type: e.data_type(),
                    bytes: e.payload().as_ptr(),
                };
            }
            Payload::SysEx(r) => {
                let (bytes, size) = match sysex.get(r.range()) {
                    Some(m) => (m.as_ptr(), u32::try_from(m.len()).unwrap_or(u32::MAX)),
                    None => (std::ptr::null(), 0),
                };
                out.__field0.data = sdk::DataEvent {
                    size,
                    r#type: K_DATA_TYPE_MIDI_SYSEX,
                    bytes,
                };
            }
            Payload::ExpressionText(e) => {
                let (text, len) = text(&e.text);
                out.__field0.noteExpressionText = sdk::NoteExpressionTextEvent {
                    typeId: e.type_id,
                    noteId: e.note_id,
                    textLen: u32::from(len),
                    text,
                };
            }
            Payload::Chord(e) => {
                let (text, len) = text(&e.text);
                out.__field0.chord = sdk::ChordEvent {
                    root: e.root,
                    bassNote: e.bass_note,
                    mask: e.mask,
                    textLen: len,
                    text,
                };
            }
            Payload::Scale(e) => {
                let (text, len) = text(&e.text);
                out.__field0.scale = sdk::ScaleEvent {
                    root: e.root,
                    mask: e.mask,
                    textLen: len,
                    text,
                };
            }
        }
        out
    }

    /// The event a plugin handed over as an SDK `Event`, or `None` for a tag the
    /// SDK does not define. Text is copied into `text_arena`.
    ///
    /// # Safety
    ///
    /// `event.type` must name the union member that is live. A chord, scale or
    /// text event's text pointer must be null or valid for reads of
    /// `min(textLen, MAX_EVENT_TEXT_LEN)` `u16`s.
    pub(crate) unsafe fn from_c(event: &sdk::Event, text_arena: &mut TextArena) -> Option<Self> {
        let kind = EventKind::try_from(event.r#type).ok()?;
        let header = EventHeader {
            bus_index: event.busIndex,
            sample_offset: event.sampleOffset,
            ppq_position: event.ppqPosition,
            flags: event.flags,
        };
        let mut intern = |ptr: *const u16, len: usize| {
            let n = len.min(MAX_EVENT_TEXT_LEN);
            if ptr.is_null() || n == 0 {
                return TextRef::default();
            }
            // SAFETY: non-null and valid for `n` units, by this fn's contract.
            text_arena.intern(unsafe { std::slice::from_raw_parts(ptr, n) })
        };

        let payload = match kind {
            EventKind::NoteOn => Payload::NoteOn(NoteOn(member!(event, noteOn))),
            EventKind::NoteOff => Payload::NoteOff(NoteOff(member!(event, noteOff))),
            EventKind::PolyPressure => {
                Payload::PolyPressure(PolyPressure(member!(event, polyPressure)))
            }
            EventKind::NoteExpressionValue => {
                Payload::ExpressionValue(ExpressionValue(member!(event, noteExpressionValue)))
            }
            EventKind::NoteExpressionIntValue => {
                Payload::ExpressionInt(ExpressionInt(member!(event, noteExpressionIntValue)))
            }
            EventKind::LegacyMidiCcOut => {
                Payload::LegacyCcOut(LegacyCcOut(member!(event, midiCCOut)))
            }
            EventKind::Data => {
                let e = member!(event, data);
                let mut bytes = [0u8; Data::CAPACITY];
                if !e.bytes.is_null() {
                    let n = (e.size as usize).min(Data::CAPACITY);
                    // SAFETY: non-null, and the ABI makes it valid for `size` bytes; `n`
                    // is at most that and at most the destination's length.
                    unsafe { std::ptr::copy_nonoverlapping(e.bytes, bytes.as_mut_ptr(), n) };
                }
                Payload::Data(Data::from_plugin(e.r#type, e.size, bytes))
            }
            EventKind::NoteExpressionText => {
                let e = member!(event, noteExpressionText);
                Payload::ExpressionText(ExpressionText {
                    type_id: e.typeId,
                    note_id: e.noteId,
                    text: intern(e.text, e.textLen as usize),
                })
            }
            EventKind::Chord => {
                let e = member!(event, chord);
                Payload::Chord(Chord {
                    root: e.root,
                    bass_note: e.bassNote,
                    mask: e.mask,
                    text: intern(e.text, usize::from(e.textLen)),
                })
            }
            EventKind::Scale => {
                let e = member!(event, scale);
                Payload::Scale(Scale {
                    root: e.root,
                    mask: e.mask,
                    text: intern(e.text, usize::from(e.textLen)),
                })
            }
        };
        Some(Self { header, payload })
    }
}
