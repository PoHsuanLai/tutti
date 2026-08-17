//! Node-id fingerprints for this crate's AudioUnit types. Owned here so
//! tutti-core stays the bottom layer.
pub(crate) const POLY_SYNTH_ID: u64 = 0x_504F_4C59_5359_4E54; // "POLYSYNT"
const _: () = tutti_core::assert_unique(&[POLY_SYNTH_ID]);
