//! Node-id fingerprints for this crate's AudioUnit types (returned by their
//! `get_id()`). Owned here, not in tutti-core, so core stays the bottom layer.
//! Intra-crate uniqueness is a compile error via `assert_unique` below.

pub(crate) const AUTOMATION_LANE_ID: u64 = 0x_4155_544F_4D41_5445; // "AUTOMATE"
pub(crate) const SVF_FILTER_ID: u64 = 0x_5356_4646_494C_5431; // "SVFFILT1"
pub(crate) const LADDER_FILTER_ID: u64 = 0x_4C41_4444_4552_4631; // "LADDERF1"
pub(crate) const EQ_BAND_ID: u64 = 0x_4551_4241_4E44_4E31; // "EQBANDN1"
pub(crate) const DELAY_LINE_ID: u64 = 0x_444C_594E_4F44_4531; // "DLYNODE1"
pub(crate) const STEREO_DELAY_LINE_ID: u64 = 0x_444C_594E_4F44_4532; // "DLYNODE2"
pub(crate) const LFO_ID: u64 = 0x_4C46_4F5F_4E4F_4445; // "LFO_NODE"
pub(crate) const PHASER_ID: u64 = 0x_5048_4153_4552_4E31; // "PHASERN1"
pub(crate) const CHORUS_ID: u64 = 0x_4348_4F52_5553_4E31; // "CHORUSN1"
pub(crate) const FLANGER_ID: u64 = 0x_464C_414E_4745_5231; // "FLANGER1"
pub(crate) const DISTORTION_ID: u64 = 0x_4449_5354_4F52_5431; // "DISTORT1"
pub(crate) const CONVOLVER_ID: u64 = 0x_434F_4E56_4E4F_4431; // "CONVNOD1"
pub(crate) const STEREO_CONVOLVER_ID: u64 = 0x_434F_4E56_4E4F_4432; // "CONVNOD2"
pub(crate) const GATE_ID: u64 = 0x_0000_5343_4741_5445; // "SCGATE"
pub(crate) const COMPRESSOR_ID: u64 = 0x_0000_5343_434F_4D50; // "SCCOMP"
pub(crate) const STEREO_GATE_ID: u64 = 0x_0000_5353_4347_4154; // "SSCGAT"
pub(crate) const STEREO_COMPRESSOR_ID: u64 = 0x_0000_5353_4343_4F4D; // "SSCCOM"
pub(crate) const LIMITER_ID: u64 = 0x_4C49_4D49_5445_5231; // "LIMITER1"
pub(crate) const BRICKWALL_LIMITER_ID: u64 = 0x_4252_4B57_4C4C_4D54; // "BRKWLLMT"
pub(crate) const PARAM_SHAPER_ID: u64 = 0x_5052_4D53_4841_5045; // "PRMSHAPE"
pub(crate) const PARAM_SUM_ID: u64 = 0x_5052_4D53_554D_5F31; // "PRMSUM_1"
pub(crate) const ATOMIC_SOURCE_ID: u64 = 0x_4154_4F4D_5352_4331; // "ATOMSRC1"
pub(crate) const BUS_STRIP_ID: u64 = 0x_4255_5353_5452_5031; // "BUSSTRP1"
/// The engine-level width-generic summing bus. Value unchanged from when it
/// was inline in `mix_bus.rs`; it is a persisted fingerprint, not a fresh id.
/// Distinct from the DAW-side StereoSumUnit id (0xDA02).
pub(crate) const CHANNEL_SUM_ID: u64 = 0x_0000_0000_0000_5501;
pub(crate) const DOWNMIX_ID: u64 = 0x_444F_574E_4D49_5831; // "DOWNMIX1"

// Compile-time intra-crate uniqueness guard (duplicate => cargo build error).
const _: () = tutti_core::assert_unique(&[
    AUTOMATION_LANE_ID,
    SVF_FILTER_ID,
    LADDER_FILTER_ID,
    EQ_BAND_ID,
    DELAY_LINE_ID,
    STEREO_DELAY_LINE_ID,
    LFO_ID,
    PHASER_ID,
    CHORUS_ID,
    FLANGER_ID,
    DISTORTION_ID,
    CONVOLVER_ID,
    STEREO_CONVOLVER_ID,
    GATE_ID,
    COMPRESSOR_ID,
    STEREO_GATE_ID,
    STEREO_COMPRESSOR_ID,
    LIMITER_ID,
    BRICKWALL_LIMITER_ID,
    PARAM_SHAPER_ID,
    PARAM_SUM_ID,
    ATOMIC_SOURCE_ID,
    BUS_STRIP_ID,
    CHANNEL_SUM_ID,
    DOWNMIX_ID,
]);
