//! The arena that holds event text on the audio thread, and `TextRef`, the handle into it.

use super::*;

/// A `(start, len)` slice into an `EventList`'s UTF-16 text arena.
///
/// VST3's chord / scale / note-expression-text events carry a borrowed
/// `const TChar*` (UTF-16) that must outlive the `process` call. To keep
/// [`Vst3Event`] `Copy` (no owned heap per event), the string lives in a shared
/// arena owned by the `EventList` — written only while staging, cleared once
/// per block — and the event holds only this index into it. `len` counts `u16`
/// code units, excluding any terminator.
///
/// `DataEvent` needs no arena: its payload is already an inline `[u8; 16]` in
/// the event, and the C-struct conversion points straight at it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextRef {
    /// Offset of the first `u16` code unit within the arena.
    pub start: u32,
    /// Length in `u16` code units, excluding any terminator.
    pub len: u32,
}

/// Largest UTF-16 text copied out of a plugin-supplied chord/scale/text event.
/// Plugin display names are short; this caps a malicious/garbage `textLen`.
pub(super) const MAX_EVENT_TEXT_LEN: usize = 256;

/// The UTF-16 owner behind chord / scale / note-expression-text events'
/// borrowed `text` pointers, with a fixed room reserved once at construction.
///
/// The plugin keeps every `text` pointer `getEvent` hands it for the rest of
/// `process`, and those pointers point into this buffer. A `Vec`'s elements
/// live on the heap, so moving the owner does not move them, but *growing* it
/// past its capacity reallocates and dangles every pointer already handed out
/// (and allocates on the audio thread). So the arena never grows: a text that
/// does not fit the room left is refused, interned as an empty [`TextRef`],
/// and counted in `TextArena::dropped`. The event itself is still staged.
#[derive(Debug)]
pub struct TextArena {
    buf: Vec<u16>,
    dropped: usize,
}

impl TextArena {
    /// UTF-16 units reserved per arena: room for this many maximum-length
    /// texts in one block. Text-bearing events (chords, scales, per-note text)
    /// are a handful per block in practice; past this, texts are refused.
    pub(crate) const CAPACITY: usize = 64 * MAX_EVENT_TEXT_LEN;

    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::with_capacity(Self::CAPACITY),
            dropped: 0,
        }
    }

    /// Copy `text` (clamped to `MAX_EVENT_TEXT_LEN`) in and address it, or
    /// refuse with an empty ref when the reserved room cannot hold it. Never
    /// reallocates.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the arena never exceeds CAPACITY (16 Ki units) and n is at most \
                  MAX_EVENT_TEXT_LEN, so both fit u32"
    )]
    pub(crate) fn intern(&mut self, text: &[u16]) -> TextRef {
        let n = text.len().min(MAX_EVENT_TEXT_LEN);
        if n == 0 {
            return TextRef::default();
        }
        if self.buf.len() + n > Self::CAPACITY {
            self.dropped += 1;
            return TextRef::default();
        }
        let start = self.buf.len() as u32;
        self.buf.extend_from_slice(&text[..n]);
        TextRef {
            start,
            len: n as u32,
        }
    }

    /// Empty the arena for the next block, keeping its reserved room.
    pub(crate) fn clear(&mut self) {
        self.buf.clear();
    }

    /// Texts refused for lack of room since this arena was made.
    pub(crate) fn dropped(&self) -> usize {
        self.dropped
    }
}

impl Default for TextArena {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for TextArena {
    type Target = [u16];
    fn deref(&self) -> &[u16] {
        &self.buf
    }
}

/// Intern `text` into `arena` and return a [`TextRef`] addressing it.
pub(super) fn intern_utf16(arena: &mut TextArena, text: &[u16]) -> TextRef {
    arena.intern(text)
}

pub(super) fn text_header(sample_offset: i32, event_type: u16) -> EventHeader {
    EventHeader {
        bus_index: 0,
        sample_offset,
        ppq_position: 0.0,
        flags: 0,
        event_type,
    }
}

pub(super) fn resolve_text(t: &TextRef, arena: &[u16]) -> Vec<u16> {
    let start = t.start as usize;
    let end = start + t.len as usize;
    arena
        .get(start..end)
        .map(|s| s.to_vec())
        .unwrap_or_default()
}
