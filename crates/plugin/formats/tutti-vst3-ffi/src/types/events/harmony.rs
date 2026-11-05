//! harmony: part of the VST3 event vocabulary.

use super::*;

/// Current chord context: root + bass note (0..127), a degree `mask`, and a
/// display name in the arena. Drives chord-aware instruments / harmonizers.
#[derive(Debug, Clone, Copy)]
pub struct ChordEvent {
    /// Bus, frame offset and event-type discriminant.
    pub header: EventHeader,
    /// Chord root as a MIDI note number, 0..=127.
    pub root: i16,
    /// Bass note as a MIDI note number, 0..=127; differs from `root` for
    /// inversions and slash chords.
    pub bass_note: i16,
    /// Bitmask of chord degrees present.
    pub mask: i16,
    /// Display name, as a slice into the event list's UTF-16 arena.
    pub text: TextRef,
}

/// Current scale/key context: root (0..127) + a 12-bit `mask` of scale degrees
/// (bit 0 = C), and a display name in the arena.
#[derive(Debug, Clone, Copy)]
pub struct ScaleEvent {
    /// Bus, frame offset and event-type discriminant.
    pub header: EventHeader,
    /// Scale root as a MIDI note number, 0..=127.
    pub root: i16,
    /// 12-bit mask of scale degrees, bit 0 = C.
    pub mask: i16,
    /// Display name, as a slice into the event list's UTF-16 arena.
    pub text: TextRef,
}

/// Host-facing chord context (see [`ChordEvent`]).
#[derive(Debug, Clone)]
pub struct ChordValue {
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

impl ChordValue {
    /// Stage into a [`Vst3Event`], interning the display name into `arena`.
    pub(crate) fn to_vst3_event(&self, arena: &mut TextArena) -> Vst3Event {
        Vst3Event::Chord(ChordEvent {
            header: text_header(self.sample_offset, K_CHORD_EVENT),
            root: self.root,
            bass_note: self.bass_note,
            mask: self.mask,
            text: intern_utf16(arena, &self.text),
        })
    }
}

/// Host-facing scale/key context (see [`ScaleEvent`]).
#[derive(Debug, Clone)]
pub struct ScaleValue {
    /// Frame offset within the block the event is staged into.
    pub sample_offset: i32,
    /// Scale root as a MIDI note number, 0..=127.
    pub root: i16,
    /// 12-bit mask of scale degrees, bit 0 = C.
    pub mask: i16,
    /// Display name as UTF-16 code units, owned until staging interns it.
    pub text: Vec<u16>,
}

impl ScaleValue {
    /// Stage into a [`Vst3Event`], interning the display name into `arena`.
    pub(crate) fn to_vst3_event(&self, arena: &mut TextArena) -> Vst3Event {
        Vst3Event::Scale(ScaleEvent {
            header: text_header(self.sample_offset, K_SCALE_EVENT),
            root: self.root,
            mask: self.mask,
            text: intern_utf16(arena, &self.text),
        })
    }
}

/// Reads a chord/scale/text/int event off an output list, resolving its
/// [`TextRef`] against `arena` into an owned UTF-16 `Vec`. Returns `None` for
/// any other variant.
pub fn vst3_to_chord(event: &Vst3Event, arena: &[u16]) -> Option<ChordValue> {
    match event {
        Vst3Event::Chord(e) => Some(ChordValue {
            sample_offset: e.header.sample_offset,
            root: e.root,
            bass_note: e.bass_note,
            mask: e.mask,
            text: resolve_text(&e.text, arena),
        }),
        _ => None,
    }
}

/// See [`vst3_to_chord`].
pub fn vst3_to_scale(event: &Vst3Event, arena: &[u16]) -> Option<ScaleValue> {
    match event {
        Vst3Event::Scale(e) => Some(ScaleValue {
            sample_offset: e.header.sample_offset,
            root: e.root,
            mask: e.mask,
            text: resolve_text(&e.text, arena),
        }),
        _ => None,
    }
}
