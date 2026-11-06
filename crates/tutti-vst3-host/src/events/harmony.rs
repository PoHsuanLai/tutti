//! Chord, scale and note-expression text events, with their text as UTF-16.

use tutti_vst3_ffi::Staging;

use super::{Chord, Event, ExpressionText, Payload, Scale};

/// A chord context change for [`InputEvents`](super::InputEvents).
#[derive(Debug, Clone)]
pub struct Utf16Chord {
    /// Frame offset within the block the event is staged into.
    pub sample_offset: i32,
    /// Chord root as a MIDI note number, 0..=127.
    pub root: i16,
    /// Bass note as a MIDI note number, 0..=127.
    pub bass_note: i16,
    /// Bitmask of chord degrees present.
    pub mask: i16,
    /// Display name as UTF-16 code units, owned until staging interns it.
    pub text: Vec<u16>,
}

impl Utf16Chord {
    /// The event, its name interned into the block's arena.
    pub fn to_event(&self, staging: &mut Staging<'_>) -> Event {
        Event::at(
            self.sample_offset,
            Payload::Chord(Chord {
                root: self.root,
                bass_note: self.bass_note,
                mask: self.mask,
                text: staging.intern(&self.text),
            }),
        )
    }
}

/// A scale context change for [`InputEvents`](super::InputEvents).
#[derive(Debug, Clone)]
pub struct Utf16Scale {
    /// Frame offset within the block the event is staged into.
    pub sample_offset: i32,
    /// Scale root as a MIDI note number, 0..=127.
    pub root: i16,
    /// 12-bit mask of scale degrees, bit 0 = C.
    pub mask: i16,
    /// Display name as UTF-16 code units, owned until staging interns it.
    pub text: Vec<u16>,
}

impl Utf16Scale {
    /// The event, its name interned into the block's arena.
    pub fn to_event(&self, staging: &mut Staging<'_>) -> Event {
        Event::at(
            self.sample_offset,
            Payload::Scale(Scale {
                root: self.root,
                mask: self.mask,
                text: staging.intern(&self.text),
            }),
        )
    }
}

/// A per-note text annotation for [`InputEvents`](super::InputEvents).
#[derive(Debug, Clone)]
pub struct Utf16Text {
    /// Frame offset within the block the event is staged into.
    pub sample_offset: i32,
    /// Note id assigned by the originating note-on.
    pub note_id: i32,
    /// The note-expression type id the text annotates.
    pub type_id: u32,
    /// The annotation as UTF-16 code units, owned until staging interns it.
    pub text: Vec<u16>,
}

impl Utf16Text {
    /// The event, its text interned into the block's arena.
    pub fn to_event(&self, staging: &mut Staging<'_>) -> Event {
        Event::at(
            self.sample_offset,
            Payload::ExpressionText(ExpressionText {
                type_id: self.type_id,
                note_id: self.note_id,
                text: staging.intern(&self.text),
            }),
        )
    }
}

pub(super) fn decode_chord(event: &Event, arena: &[u16]) -> Option<Utf16Chord> {
    let Payload::Chord(e) = &event.payload else {
        return None;
    };
    Some(Utf16Chord {
        sample_offset: event.sample_offset(),
        root: e.root,
        bass_note: e.bass_note,
        mask: e.mask,
        text: e.text.slice(arena).unwrap_or_default().to_vec(),
    })
}

pub(super) fn decode_scale(event: &Event, arena: &[u16]) -> Option<Utf16Scale> {
    let Payload::Scale(e) = &event.payload else {
        return None;
    };
    Some(Utf16Scale {
        sample_offset: event.sample_offset(),
        root: e.root,
        mask: e.mask,
        text: e.text.slice(arena).unwrap_or_default().to_vec(),
    })
}

pub(super) fn decode_text(event: &Event, arena: &[u16]) -> Option<Utf16Text> {
    let Payload::ExpressionText(e) = &event.payload else {
        return None;
    };
    Some(Utf16Text {
        sample_offset: event.sample_offset(),
        note_id: e.note_id,
        type_id: e.type_id,
        text: e.text.slice(arena).unwrap_or_default().to_vec(),
    })
}
