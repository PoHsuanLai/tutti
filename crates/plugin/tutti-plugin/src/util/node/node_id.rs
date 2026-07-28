//! Node-id fingerprint for this crate's AudioUnit type. Owned here so
//! tutti-core stays the bottom layer.
//!
//! Public via [`crate::backend`] so out-of-crate in-process plugin nodes report
//! the same stable `AudioUnit::get_id()` fingerprint as the native plugin
//! clients — fundsp uses this id to treat the node as identity-stable across
//! graph commits.
pub const PLUGIN_CLIENT_ID: u64 = 0x_504C_5547_494E_434C; // "PLUGINCL"
