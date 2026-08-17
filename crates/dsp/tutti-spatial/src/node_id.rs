//! Node-id fingerprints for this crate's AudioUnit types (returned by `get_id()`).
//!
//! **Persisted values — do not renumber.** `assert_unique` guards them within
//! this crate; cross-crate uniqueness rests on the mnemonic convention (see
//! `tutti_core::node_id`).

pub(crate) const VBAP_PANNER_BASE_ID: u64 = 0x_0000_0000_5041_4E00; // "PAN\0"
#[cfg_attr(not(feature = "hrtf"), allow(dead_code))]
pub(crate) const HRTF_BINAURAL_ID: u64 = 0x_0000_0000_4852_5446; // "HRTF"

// Compile-time intra-crate uniqueness guard (duplicate => cargo build error).
const _: () = tutti_core::assert_unique(&[VBAP_PANNER_BASE_ID, HRTF_BINAURAL_ID]);
