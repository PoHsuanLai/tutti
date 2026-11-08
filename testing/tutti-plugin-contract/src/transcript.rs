//! The plugin's state blob, read and patched. The layout is the one
//! documented in `tutti-shared-plugin`'s `core` module.
//!
//! The plugin is configured and observed through its state, the one channel
//! every format and every process boundary has: nothing here depends on an
//! environment variable or a symbol of the plugin image.

use crate::active::main_thread;
use tutti_plugin_types::Control;

use crate::Format;
use crate::spec::Mode;
pub use tutti_shared_plugin::transcript::{
    Kind, Logged, MAGIC, Transcript, find_blob, replace_blob,
};

/// The transcript of what `c`'s plugin has been sent.
///
/// # Panics
///
/// When the plugin's log overflowed.
pub fn transcript<F: Format, C: Control + ?Sized>(c: &mut C) -> Transcript {
    let state = c.save_state(&main_thread()).expect("the plugin saves");
    Transcript::parse(&F::extract_blob(&state)).complete()
}

/// Selects the plugin's mode by saving its state, patching the mode word of the
/// plugin's blob and loading it back. A mode parameter would not do: a write to
/// a VST3 separate controller reaches the component only inside a block, after
/// setup has read the mode.
pub fn set_mode<F: Format, C: Control + ?Sized>(c: &mut C, mode: Mode) {
    let state = c.save_state(&main_thread()).expect("the plugin saves");
    let patched = F::embed_blob(&state, &patch_mode(F::extract_blob(&state), mode));
    c.load_state(&main_thread(), &patched)
        .expect("the plugin takes its configuration");
}

/// Selects the plugin's `level` the same way: through the state, so it reaches
/// the component itself and not a controller that a host delivers separately.
pub fn set_level<F: Format, C: Control + ?Sized>(c: &mut C, level: f64) {
    let state = c.save_state(&main_thread()).expect("the plugin saves");
    let patched = F::embed_blob(&state, &patch_level(F::extract_blob(&state), level));
    c.load_state(&main_thread(), &patched)
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
