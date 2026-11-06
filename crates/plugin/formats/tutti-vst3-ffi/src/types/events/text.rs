//! The arena that holds event text on the audio thread, and `TextRef`, the handle into it.

/// A `(start, len)` slice into an `EventList`'s UTF-16 text arena.
///
/// Chord, scale and note-expression-text events carry a borrowed `TChar*`
/// that must outlive the `process` call. The string lives in the list's arena
/// and the event holds this index, which keeps [`Event`](super::Event) `Copy`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextRef {
    /// Offset of the first `u16` code unit within the arena.
    pub start: u32,
    /// Length in `u16` code units, excluding any terminator.
    pub len: u32,
}

impl TextRef {
    /// The text this refers to in `arena`, or `None` if it lies outside.
    #[must_use]
    pub fn slice(self, arena: &[u16]) -> Option<&[u16]> {
        let start = self.start as usize;
        arena.get(start..start + self.len as usize)
    }
}

/// Largest UTF-16 text taken into an arena: a cap on a garbage `textLen`.
pub(crate) const MAX_EVENT_TEXT_LEN: usize = 256;

/// The UTF-16 owner behind events' borrowed `text` pointers, with its room
/// reserved once.
///
/// The plugin keeps every pointer `getEvent` hands it for the rest of
/// `process`, so the buffer never grows (growth would reallocate under them,
/// on the audio thread): a text past the room is refused as an empty
/// [`TextRef`] and counted. The event is still staged.
#[derive(Debug)]
pub struct TextArena {
    buf: Vec<u16>,
    dropped: usize,
}

impl TextArena {
    /// UTF-16 units reserved: room for 64 maximum-length texts per block.
    pub const CAPACITY: usize = 64 * MAX_EVENT_TEXT_LEN;

    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::with_capacity(Self::CAPACITY),
            dropped: 0,
        }
    }

    /// Copies `text` (cut to `MAX_EVENT_TEXT_LEN`) in, or refuses with an
    /// empty ref when the reserved room cannot hold it. Never reallocates.
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

    /// Empties the arena for the next block, keeping its room.
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
