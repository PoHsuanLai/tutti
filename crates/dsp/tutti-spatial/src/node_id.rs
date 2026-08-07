//! Node-id fingerprints for this crate's AudioUnit types (returned by their
//! `get_id()`). Intra-crate uniqueness is a compile error via `assert_unique`
//! below; cross-crate uniqueness rests on the mnemonic convention, as
//! `tutti_core::node_id` documents.
//!
//! **These values are persisted fingerprints and must not be renumbered.** They
//! are unchanged from when they lived in `tutti_units::node_id`, and they moved
//! here with the panner types. Note that the guard `assert_unique` provides is
//! *per crate*: splitting these three ids out of `tutti-units` split the
//! compile-time check with them, so the mnemonics ("PAN\0", "BIN\0", "HRTF")
//! are what keep them distinct from that crate's — which is exactly the
//! convention `tutti_core::node_id` says to rely on.

pub(crate) const SPATIAL_PANNER_BASE_ID: u64 = 0x_0000_0000_5041_4E00; // "PAN\0"
#[cfg_attr(not(feature = "hrtf"), allow(dead_code))]
pub(crate) const BINAURAL_PANNER_ID: u64 = 0x_0000_0000_4249_4E00; // "BIN\0"
#[cfg_attr(not(feature = "hrtf"), allow(dead_code))]
pub(crate) const HRTF_BINAURAL_ID: u64 = 0x_0000_0000_4852_5446; // "HRTF"

// Compile-time intra-crate uniqueness guard (duplicate => cargo build error).
const _: () = tutti_core::node_id::assert_unique(&[
    SPATIAL_PANNER_BASE_ID,
    BINAURAL_PANNER_ID,
    HRTF_BINAURAL_ID,
]);
