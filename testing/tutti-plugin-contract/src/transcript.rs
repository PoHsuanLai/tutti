//! The plugin's state blob, read and patched. The layout is the one
//! documented in `tutti-shared-plugin`'s `core` module.
//!
//! The plugin is configured and observed through its state, the one channel
//! every format and every process boundary has: nothing here depends on an
//! environment variable or a symbol of the plugin image.

use tutti_plugin_types::Control;

use crate::Format;
use crate::spec::Mode;

const MAGIC: [u8; 4] = *b"TSP1";

/// What kind of event the host sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A parameter value.
    Param,
    /// A note on.
    NoteOn,
    /// A note off.
    NoteOff,
}

/// An event as the plugin received it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Logged {
    /// What it was.
    pub kind: Kind,
    /// Its sample offset in the block.
    pub offset: u32,
    /// The parameter id or the key.
    pub id: u32,
    /// The parameter value (0 for a note).
    pub value: f64,
}

/// What the plugin recorded of what the host sent, read from its state.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    /// The mode index.
    pub mode: u32,
    /// The `level` and `aux` parameters, as the plugin holds them.
    pub level: f64,
    /// See `level`.
    pub aux: f64,
    /// Blocks processed since the last activation.
    pub calls: u64,
    /// Frames processed since the last activation.
    pub frames: u64,
    /// Frame counts of the last blocks, oldest first.
    pub blocks: Vec<u32>,
    /// Activations.
    pub prepares: u32,
    /// The rate it was last activated at.
    pub rate: f64,
    /// The block size it was last activated with.
    pub max_block: u32,
    /// Events in arrival order.
    pub events: Vec<Logged>,
    /// The host sent more than the plugin keeps, so the lists above are
    /// incomplete.
    pub overflow: bool,
}

/// A reader over the blob.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let (head, rest) = self
            .0
            .split_first_chunk::<N>()
            .expect("the blob is complete");
        self.0 = rest;
        *head
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }
    fn f64(&mut self) -> f64 {
        f64::from_bits(self.u64())
    }
}

/// Where the plugin's bytes start inside a state the host saved verbatim.
fn blob_at(state: &[u8]) -> usize {
    state
        .windows(MAGIC.len())
        .position(|w| w == MAGIC)
        .expect("the state holds the plugin's blob")
}

/// The plugin's blob, found in a state the host stores verbatim: what
/// [`Format::extract_blob`] does by default.
#[must_use]
pub fn find_blob(state: &[u8]) -> Vec<u8> {
    state[blob_at(state)..].to_vec()
}

/// `state` with the plugin's blob replaced by `blob` (the same length, found
/// as [`find_blob`] does): the default of
/// [`Format::embed_blob`].
#[must_use]
pub fn replace_blob(state: &[u8], blob: &[u8]) -> Vec<u8> {
    let at = blob_at(state);
    let mut out = state.to_vec();
    out[at..at + blob.len()].copy_from_slice(blob);
    out
}

impl Transcript {
    /// Parses the plugin's blob (from its magic).
    ///
    /// # Panics
    ///
    /// When `blob` is not a complete blob of the plugin's.
    #[must_use]
    pub fn parse(blob: &[u8]) -> Self {
        let mut c = Cursor(blob);
        assert_eq!(
            c.take::<4>(),
            MAGIC,
            "the blob starts with the plugin's magic"
        );
        let mode = c.u32();
        let (level, aux) = (c.f64(), c.f64());
        let (calls, frames) = (c.u64(), c.u64());
        let prepares = c.u32();
        let rate = c.f64();
        let max_block = c.u32();
        let overflow = c.u32() != 0;
        let blocks = (0..c.u32()).map(|_| c.u32()).collect();
        let events = (0..c.u32())
            .map(|_| {
                let head = c.u64();
                let kind = match head >> 32 {
                    1 => Kind::Param,
                    2 => Kind::NoteOn,
                    3 => Kind::NoteOff,
                    other => panic!("the transcript holds event kind {other}"),
                };
                let id = c.u64() as u32;
                Logged {
                    kind,
                    offset: head as u32,
                    id,
                    value: c.f64(),
                }
            })
            .collect();
        Self {
            mode,
            level,
            aux,
            calls,
            frames,
            blocks,
            prepares,
            rate,
            max_block,
            events,
            overflow,
        }
    }

    /// The transcript, refused when it is incomplete: a case that iterates a
    /// list the plugin truncated would otherwise pass on what is left.
    ///
    /// # Panics
    ///
    /// When the plugin's log overflowed.
    #[must_use]
    pub fn complete(self) -> Self {
        assert!(
            !self.overflow,
            "the plugin's log overflowed: the host sent more than it keeps"
        );
        self
    }

    /// The events of one kind, in arrival order.
    pub fn of(&self, kind: Kind) -> impl Iterator<Item = &Logged> {
        self.events.iter().filter(move |e| e.kind == kind)
    }
}

/// The transcript of what `c`'s plugin has been sent.
///
/// # Panics
///
/// When the plugin's log overflowed.
pub fn transcript<F: Format, C: Control + ?Sized>(c: &mut C) -> Transcript {
    let state = c.save_state().expect("the plugin saves");
    Transcript::parse(&F::extract_blob(&state)).complete()
}

/// Selects the plugin's mode by saving its state, patching the mode word of the
/// plugin's blob and loading it back. A mode parameter would not do: a write to
/// a VST3 separate controller reaches the component only inside a block, after
/// setup has read the mode.
pub fn set_mode<F: Format, C: Control + ?Sized>(c: &mut C, mode: Mode) {
    let state = c.save_state().expect("the plugin saves");
    let patched = F::embed_blob(&state, &patch_mode(F::extract_blob(&state), mode));
    c.load_state(&patched)
        .expect("the plugin takes its configuration");
}

/// Selects the plugin's `level` the same way: through the state, so it reaches
/// the component itself and not a controller that a host delivers separately.
pub fn set_level<F: Format, C: Control + ?Sized>(c: &mut C, level: f64) {
    let state = c.save_state().expect("the plugin saves");
    let patched = F::embed_blob(&state, &patch_level(F::extract_blob(&state), level));
    c.load_state(&patched)
        .expect("the plugin takes its configuration");
}

/// `blob` with its `level` set.
#[must_use]
pub fn patch_level(mut blob: Vec<u8>, level: f64) -> Vec<u8> {
    let at = MAGIC.len() + 4;
    blob[at..at + 8].copy_from_slice(&level.to_bits().to_le_bytes());
    blob
}

/// `blob` with its mode word set.
#[must_use]
pub fn patch_mode(mut blob: Vec<u8>, mode: Mode) -> Vec<u8> {
    let at = MAGIC.len();
    blob[at..at + 4].copy_from_slice(&mode.index().to_le_bytes());
    blob
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(overflow: u32, blocks: &[u32]) -> Vec<u8> {
        let mut b = MAGIC.to_vec();
        b.extend_from_slice(&1u32.to_le_bytes()); // mode
        b.extend_from_slice(&0.25f64.to_bits().to_le_bytes()); // level
        b.extend_from_slice(&0.75f64.to_bits().to_le_bytes()); // aux
        b.extend_from_slice(&5u64.to_le_bytes()); // calls
        b.extend_from_slice(&320u64.to_le_bytes()); // frames
        b.extend_from_slice(&2u32.to_le_bytes()); // prepares
        b.extend_from_slice(&48_000f64.to_bits().to_le_bytes());
        b.extend_from_slice(&64u32.to_le_bytes());
        b.extend_from_slice(&overflow.to_le_bytes());
        b.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
        for n in blocks {
            b.extend_from_slice(&n.to_le_bytes());
        }
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&(2u64 << 32 | 7).to_le_bytes());
        b.extend_from_slice(&60u64.to_le_bytes());
        b.extend_from_slice(&0f64.to_bits().to_le_bytes());
        b
    }

    // Mutation (run): reading the fields one word early fails every assertion.
    #[test]
    fn a_blob_parses_to_what_it_holds() {
        let t = Transcript::parse(&blob(0, &[64, 1])).complete();
        assert_eq!(
            (t.mode, t.prepares, t.rate, t.max_block),
            (1, 2, 48_000.0, 64)
        );
        assert_eq!((t.level, t.aux, t.calls, t.frames), (0.25, 0.75, 5, 320));
        assert_eq!(t.blocks, [64, 1]);
        assert_eq!(
            t.events,
            [Logged {
                kind: Kind::NoteOn,
                offset: 7,
                id: 60,
                value: 0.0
            }]
        );
    }

    // Mutation (run): `complete` not checking the flag fails it.
    #[test]
    #[should_panic(expected = "log overflowed")]
    fn an_overflowed_transcript_is_refused() {
        let _ = Transcript::parse(&blob(1, &[64])).complete();
    }

    // Mutation (run): `replace_blob` writing at the start of the state fails it.
    #[test]
    fn a_blob_is_found_and_replaced_inside_a_wrapper() {
        let mut state = b"wrap".to_vec();
        state.extend(blob(0, &[64]));
        state.extend_from_slice(b"tail");
        let found = find_blob(&state);
        let patched = replace_blob(&state, &patch_mode(found, Mode::Notes));
        assert!(patched.starts_with(b"wrap") && patched.ends_with(b"tail"));
        assert_eq!(
            Transcript::parse(&find_blob(&patched)).mode,
            Mode::Notes.index()
        );
    }
}
