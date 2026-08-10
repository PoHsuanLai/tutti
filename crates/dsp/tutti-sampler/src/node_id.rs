//! Node-id fingerprints for this crate's `AudioUnit` types.
//!
//! Each constant is an 8-byte ASCII mnemonic packed big-endian, matching the
//! convention and the reserved-mnemonic ledger in
//! [`tutti_core::node_id`]. Two instances of one type must return the same
//! value — fundsp mixes `get_id` into its per-unit pseudorandom hash, so a
//! per-instance id would make seeded noise and oscillator phase differ between
//! runs.
//!
//! [`assert_unique`](tutti_core::node_id::assert_unique) makes a duplicate
//! within this crate a `cargo build` failure rather than a silent aliasing of
//! two node types.
//!
//! Owned here rather than in tutti-core so the bottom layer names no node type
//! that lives above it.
pub(crate) const SAMPLER_NODE_ID: u64 = 0x_5341_4D50_4C52_4E44; // "SAMPLRND"
pub(crate) const STREAMING_SAMPLER_ID: u64 = 0x_5354_5253_4D50_4C52; // "STRSMPLR"
pub(crate) const TIME_STRETCH_ID: u64 = 0x_5453_5452_4348_4E54; // "TSTRCHNT"
pub(crate) const VOICE_NODE_ID: u64 = 0x_564F_4943_454E_4F44; // "VOICENOD"
const _: () = tutti_core::node_id::assert_unique(&[
    SAMPLER_NODE_ID,
    STREAMING_SAMPLER_ID,
    TIME_STRETCH_ID,
    VOICE_NODE_ID,
]);
