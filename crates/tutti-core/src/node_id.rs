//! Type-fingerprint IDs returned by [`AudioUnit::get_id`].
//!
//! [`AudioUnit::get_id`] is fundsp's type-fingerprint hook. It serves two
//! purposes:
//!
//! 1. **Deterministic hashing** — fundsp's `ping()` mixes `get_id()` into a
//!    per-unit pseudorandom hash. Two instances of the same type MUST return
//!    the same value, or the hash (and any seeded noise, oscillator phase,
//!    etc.) becomes non-deterministic between runs.
//! 2. **Graph scanning** — tutti's PDC code uses marker ids to find
//!    auto-inserted delay nodes without carrying state between commits.
//!
//! Per-instance routing identity (for MIDI dispatch) lives on
//! [`MidiUnitId`] via [`MidiTarget`], *not* on `get_id()`.
//!
//! ## Collisions
//!
//! fundsp reserves small integers (0..~100) for its own AudioNode types.
//! This catalog uses 64-bit values that encode ASCII mnemonics, well
//! outside fundsp's range. Each constant below is unique by construction.
//!
//! [`AudioUnit::get_id`]: fundsp::audiounit::AudioUnit::get_id
//! [`MidiUnitId`]: crate::midi::MidiUnitId
//! [`MidiTarget`]: crate::midi::MidiTarget

// ──────────────────────────────────────────────────────────────────────
// PDC markers (scanned by graph.rs to find auto-inserted delays)
// ──────────────────────────────────────────────────────────────────────
pub const PDC_DELAY_ID: u64 = 0x_0000_0050_4443_4445; // "PDCDE"
pub const MONO_PDC_DELAY_ID: u64 = 0x_0000_0000_4D50_4443; // "MPDC"

// ──────────────────────────────────────────────────────────────────────
// Transport / control nodes
// ──────────────────────────────────────────────────────────────────────
pub const TRANSPORT_CLOCK_ID: u64 = 0x_5452_4E53_434C_4B00; // "TRNSCLK\0"
pub const AUTOMATION_INPUT_ID: u64 = 0x_4155_544F_494E_5054; // "AUTOINPT"
pub const AUTOMATION_LANE_ID: u64 = 0x_4155_544F_4D41_5445; // "AUTOMATE"

// ──────────────────────────────────────────────────────────────────────
// MIDI-receiving synths (instance identity lives on MidiTarget)
// ──────────────────────────────────────────────────────────────────────
pub const POLY_SYNTH_ID: u64 = 0x_504F_4C59_5359_4E54; // "POLYSYNT"
pub const SOUNDFONT_ID: u64 = 0x_0052_5553_5459_5359; // "\0RUSTYSY"
pub const PLUGIN_CLIENT_ID: u64 = 0x_504C_5547_494E_434C; // "PLUGINCL"

// ──────────────────────────────────────────────────────────────────────
// tutti-units filters
// ──────────────────────────────────────────────────────────────────────
pub const SVF_FILTER_ID: u64 = 0x_5356_4646_494C_5431; // "SVFFILT1"
pub const LADDER_FILTER_ID: u64 = 0x_4C41_4444_4552_4631; // "LADDERF1"
pub const EQ_BAND_ID: u64 = 0x_4551_4241_4E44_4E31; // "EQBANDN1"

// ──────────────────────────────────────────────────────────────────────
// tutti-units delay / modulation
// ──────────────────────────────────────────────────────────────────────
pub const DELAY_LINE_ID: u64 = 0x_444C_594E_4F44_4531; // "DLYNODE1"
pub const STEREO_DELAY_LINE_ID: u64 = 0x_444C_594E_4F44_4532; // "DLYNODE2"
pub const LFO_ID: u64 = 0x_4C46_4F5F_4E4F_4445; // "LFO_NODE"
pub const PHASER_ID: u64 = 0x_5048_4153_4552_4E31; // "PHASERN1"
pub const CHORUS_ID: u64 = 0x_4348_4F52_5553_4E31; // "CHORUSN1"
pub const FLANGER_ID: u64 = 0x_464C_414E_4745_5231; // "FLANGER1"
pub const DISTORTION_ID: u64 = 0x_4449_5354_4F52_5431; // "DISTORT1"
pub const CONVOLVER_ID: u64 = 0x_434F_4E56_4E4F_4431; // "CONVNOD1"
pub const STEREO_CONVOLVER_ID: u64 = 0x_434F_4E56_4E4F_4432; // "CONVNOD2"

// ──────────────────────────────────────────────────────────────────────
// tutti-units dynamics
// ──────────────────────────────────────────────────────────────────────
pub const GATE_ID: u64 = 0x_0000_5343_4741_5445; // "SCGATE"
pub const COMPRESSOR_ID: u64 = 0x_0000_5343_434F_4D50; // "SCCOMP"
pub const STEREO_GATE_ID: u64 = 0x_0000_5353_4347_4154; // "SSCGAT"
pub const STEREO_COMPRESSOR_ID: u64 = 0x_0000_5353_4343_4F4D; // "SSCCOM"
pub const LIMITER_ID: u64 = 0x_4C49_4D49_5445_5231; // "LIMITER1"
pub const BRICKWALL_LIMITER_ID: u64 = 0x_4252_4B57_4C4C_4D54; // "BRKWLLMT"

// ──────────────────────────────────────────────────────────────────────
// tutti-units spatial (SpatialPanner ORs num_outputs into the low byte)
// ──────────────────────────────────────────────────────────────────────
pub const SPATIAL_PANNER_BASE_ID: u64 = 0x_0000_0000_5041_4E00; // "PAN\0"
pub const BINAURAL_PANNER_ID: u64 = 0x_0000_0000_4249_4E00; // "BIN\0"

// ──────────────────────────────────────────────────────────────────────
// tutti-sampler
// ──────────────────────────────────────────────────────────────────────
pub const AUDIO_INPUT_BACKEND_ID: u64 = 0x_4155_4449_4E42_4B44; // "AUDINBKD"
pub const SAMPLER_NODE_ID: u64 = 0x_5341_4D50_4C52_4E44; // "SAMPLRND"
pub const STREAMING_SAMPLER_ID: u64 = 0x_5354_5253_4D50_4C52; // "STRSMPLR"
pub const TIME_STRETCH_ID: u64 = 0x_5453_5452_4348_4E54; // "TSTRCHNT"
