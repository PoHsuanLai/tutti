//! Node-id fingerprints for this crate's AudioUnit types. Owned here so
//! tutti-core stays the bottom layer.
pub(crate) const POLY_SYNTH_ID: u64 = 0x_504F_4C59_5359_4E54; // "POLYSYNT"
pub(crate) const SOUNDFONT_ID: u64 = 0x_0052_5553_5459_5359; // "\0RUSTYSY"
const _: () = tutti_core::node_id::assert_unique(&[POLY_SYNTH_ID, SOUNDFONT_ID]);
