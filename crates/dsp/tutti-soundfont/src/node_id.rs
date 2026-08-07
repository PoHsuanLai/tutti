//! Node-id fingerprints for this crate's AudioUnit types. Owned here so
//! tutti-core stays the bottom layer.
pub(crate) const SOUNDFONT_ID: u64 = 0x_0052_5553_5459_5359; // "\0RUSTYSY"
const _: () = tutti_core::node_id::assert_unique(&[SOUNDFONT_ID]);
