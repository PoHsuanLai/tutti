//! Node-id fingerprints for this crate's AudioUnit types. Owned here so
//! tutti-core stays the bottom layer.
pub(crate) const AUDIO_INPUT_BACKEND_ID: u64 = 0x_4155_4449_4E42_4B44; // "AUDINBKD"
pub(crate) const SAMPLER_NODE_ID: u64 = 0x_5341_4D50_4C52_4E44; // "SAMPLRND"
pub(crate) const STREAMING_SAMPLER_ID: u64 = 0x_5354_5253_4D50_4C52; // "STRSMPLR"
pub(crate) const TIME_STRETCH_ID: u64 = 0x_5453_5452_4348_4E54; // "TSTRCHNT"
pub(crate) const VOICE_NODE_ID: u64 = 0x_564F_4943_454E_4F44; // "VOICENOD"
const _: () = tutti_core::node_id::assert_unique(&[
    AUDIO_INPUT_BACKEND_ID, SAMPLER_NODE_ID, STREAMING_SAMPLER_ID, TIME_STRETCH_ID, VOICE_NODE_ID,
]);
