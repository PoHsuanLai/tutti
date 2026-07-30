//! Bus (element) topology: how many buses an AU has, what each one's channel
//! layout is, and which channel configurations it declares it can run.
//!
//! Separate from [`crate::stream`], which owns the *configuration the host
//! applies* to bus 0. This module only ever reads: it is the discovery half,
//! and nothing here writes a property. Keeping the two apart is what stops a
//! topology query from silently reconfiguring the unit it was asked about.
//!
//! AUv2 calls a bus an "element". The two words are interchangeable in Apple's
//! own headers; this crate says **bus** in public API names because that is the
//! DAW-facing word, and **element** only where it names the AudioToolbox
//! property (`kAudioUnitProperty_ElementCount`).

#![cfg(target_os = "macos")]

use tutti_plugin_types::ChannelLayout;

use crate::error::Result;
use crate::ffi::{get_property, get_property_bytes};
use crate::types::*;

/// Which side of an AU a bus sits on.
///
/// AUv2 addresses buses by a `(scope, element)` pair, and the scope constants
/// are bare `u32`s that are trivially swapped at a call site — passing
/// `kAudioUnitScope_Output` where input was meant reads a *real* bus of the
/// wrong side and returns a plausible layout, so the mistake is silent. This
/// enum makes the direction a named argument the compiler checks.
///
/// `Global` is deliberately absent: it always has exactly one element (Apple's
/// header states this outright), so it can never be the subject of a
/// bus-topology question. Per-element *parameters* on the global scope are a
/// different axis and are addressed by [`crate::parameters::ParamAddress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BusDirection {
    /// Buses the AU reads from. Effects have one; instruments and generators
    /// have none.
    Input,
    /// Buses the AU writes to. Every AU that produces audio has at least one.
    Output,
}

impl BusDirection {
    /// The AudioToolbox scope constant this direction addresses.
    pub(crate) fn scope(self) -> u32 {
        match self {
            Self::Input => K_AUDIO_UNIT_SCOPE_INPUT,
            Self::Output => K_AUDIO_UNIT_SCOPE_OUTPUT,
        }
    }

    /// Both directions, for callers that walk the whole topology.
    pub const ALL: [BusDirection; 2] = [BusDirection::Input, BusDirection::Output];
}

impl std::fmt::Display for BusDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input => write!(f, "input"),
            Self::Output => write!(f, "output"),
        }
    }
}

/// One side of a `{inChannels, outChannels}` entry in
/// `kAudioUnitProperty_SupportedNumChannels`.
///
/// The raw field is an `SInt16` in which negative values are **sentinels, not
/// counts**, and that is the entire reason this is an enum rather than an
/// `i16` or a `u16`. Apple's `AudioUnitProperties.h` (the `SupportedNumChannels`
/// constant) defines:
///
/// * `0` — this side has no elements at all. Typically the input side, on a
///   generator or instrument, whose entry then only expresses what its *output*
///   can do.
/// * `-1` / `-2` — "any number of channels". Which of the two appears matters
///   only in combination with the other side; see [`AuChannelConfig`].
/// * a value **less than -2** — a *total* channel count across every bus on
///   that scope, `abs()` of the stored number, regardless of how the channels
///   are distributed over individual buses.
/// * any positive value — a literal channel count on any single bus.
///
/// Storing this as a bare number is the bug this type exists to prevent:
/// AUSampler and AUMIDISynth on macOS 15.6 both declare `{0, -16}`, which means
/// "no input elements, at most 16 channels of output in total". Read as a count
/// that is a nonsensical *negative sixteen channels*; coerced with `abs()` or
/// `max(0)` it silently becomes "16 channels on a single bus" or "0 channels",
/// both of which are wrong and neither of which the host could later detect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuChannelCount {
    /// `0`: the AU has no elements on this side.
    NoElements,
    /// A literal channel count on any one bus of this side.
    Exactly(u16),
    /// `-1` / `-2`: any number of channels. The `wildcard` byte is the raw
    /// sentinel (`-1` or `-2`), preserved because [`AuChannelConfig`] needs the
    /// *pair* of sentinels to decide whether the two sides must match.
    Any {
        /// The raw sentinel value, `-1` or `-2`.
        wildcard: i16,
    },
    /// A value below `-2`: a cap on the total channel count summed over every
    /// bus on this side. The stored figure is the magnitude — `{0, -16}` yields
    /// `TotalAcrossBuses { max_total: 16 }`.
    TotalAcrossBuses {
        /// Maximum channels summed across all buses on this side.
        max_total: u16,
    },
}

impl AuChannelCount {
    /// Decode one raw `SInt16` field of an `AUChannelInfo`.
    ///
    /// Every branch here is a distinct documented meaning, so there is no
    /// fallthrough that turns an unrecognized value into a count.
    pub fn from_raw(raw: i16) -> Self {
        match raw {
            0 => Self::NoElements,
            -1 | -2 => Self::Any { wildcard: raw },
            // Below -2: a total across the scope. `unsigned_abs` rather than
            // `-raw`, because `-i16::MIN` overflows and would panic in debug /
            // wrap to a negative in release.
            n if n < -2 => Self::TotalAcrossBuses {
                max_total: n.unsigned_abs(),
            },
            n => Self::Exactly(n as u16),
        }
    }

    /// Whether `channels` is permitted on a single bus by this side alone.
    ///
    /// Deliberately answers only the *per-bus* question, because that is the
    /// only one a single side can answer. A [`Self::TotalAcrossBuses`] cap
    /// constrains the sum over every bus, so knowing one bus's width is not
    /// enough to decide it — this treats the cap as an upper bound on any single
    /// bus (which it is, since one bus cannot exceed the total) and leaves the
    /// summed check to a caller that knows the whole topology.
    pub fn admits(self, channels: u16) -> bool {
        match self {
            Self::NoElements => channels == 0,
            Self::Exactly(n) => n == channels,
            Self::Any { .. } => true,
            Self::TotalAcrossBuses { max_total } => channels <= max_total,
        }
    }
}

impl std::fmt::Display for AuChannelCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoElements => write!(f, "none"),
            Self::Exactly(n) => write!(f, "{n}"),
            Self::Any { wildcard } => write!(f, "any({wildcard})"),
            Self::TotalAcrossBuses { max_total } => write!(f, "<={max_total} total"),
        }
    }
}

/// One `AUChannelInfo` entry: a channel configuration the AU declares it can run.
///
/// The pair is meaningful as a pair. Apple's header gives the two wildcard
/// spellings different meanings *only* in combination:
///
/// * `{-1, -1}` — any channel count, **as long as input and output match**.
/// * `{-1, -2}` — any count on input and any count on output, **independently**.
///
/// So a decoder that mapped both `-1` and `-2` onto a single "any" variant
/// would lose the match constraint, and a host relying on it would offer the
/// user a 2-in/6-out configuration on a unit that only ever does N-to-N. That
/// distinction is what [`Self::requires_matching_counts`] recovers, and it is
/// why [`AuChannelCount::Any`] keeps the raw sentinel rather than discarding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuChannelConfig {
    /// What the input side of this configuration permits.
    pub inputs: AuChannelCount,
    /// What the output side of this configuration permits.
    pub outputs: AuChannelCount,
}

impl AuChannelConfig {
    /// Decode one raw `AUChannelInfo`.
    fn from_raw(raw: AuChannelInfo) -> Self {
        Self {
            inputs: AuChannelCount::from_raw(raw.in_channels),
            outputs: AuChannelCount::from_raw(raw.out_channels),
        }
    }

    /// Whether this entry constrains input and output to the *same* channel
    /// count.
    ///
    /// True only for `{-1, -1}` — the one spelling Apple documents as "any
    /// number of channels on input and output as long as they are the same".
    /// `{-1, -2}` and `{-2, -2}` leave the two sides independent, and a pair of
    /// literal counts like `{2, 2}` already names both sides exactly, so it
    /// needs no separate matching rule.
    pub fn requires_matching_counts(&self) -> bool {
        matches!(
            (self.inputs, self.outputs),
            (
                AuChannelCount::Any { wildcard: -1 },
                AuChannelCount::Any { wildcard: -1 }
            )
        )
    }

    /// Whether an `in_channels`-in / `out_channels`-out topology satisfies this
    /// entry.
    ///
    /// Both sides must admit their count, *and* the matching constraint above
    /// must hold. Checking the sides independently and forgetting the match is
    /// exactly the error `requires_matching_counts` exists to prevent.
    pub fn admits(&self, in_channels: u16, out_channels: u16) -> bool {
        if self.requires_matching_counts() && in_channels != out_channels {
            return false;
        }
        self.inputs.admits(in_channels) && self.outputs.admits(out_channels)
    }
}

impl std::fmt::Display for AuChannelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{{}, {}}}", self.inputs, self.outputs)
    }
}

/// The raw `AUChannelInfo` struct, `{ SInt16 inChannels; SInt16 outChannels; }`.
///
/// Hand-declared rather than taken from `coreaudio-sys`: the generated bindings
/// this crate uses do not export it. The layout is pinned by
/// `bus::tests::channel_info_matches_the_c_abi` against the SDK header's own
/// field order and 4-byte size, so a wrong guess here fails a test rather than
/// silently mis-decoding every entry.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct AuChannelInfo {
    pub(crate) in_channels: i16,
    pub(crate) out_channels: i16,
}

/// How many buses `unit` has on `direction`.
///
/// A scope with no buses is a legitimate answer, not an error: instruments and
/// generators genuinely report `0` input elements, and that zero is what the
/// host keys its "install a render callback?" decision off. An AU that refuses
/// the property entirely also yields `0` rather than an error, because
/// `kAudioUnitProperty_ElementCount` is optional in practice and a refusal means
/// "nothing to enumerate" — the same observable state as zero buses.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub(crate) unsafe fn bus_count(unit: AudioUnit, direction: BusDirection) -> u32 {
    get_property::<u32>(
        unit,
        K_AUDIO_UNIT_PROPERTY_ELEMENT_COUNT,
        direction.scope(),
        // The element index is meaningless for ElementCount itself — the
        // property describes the whole scope — so it is always 0.
        0,
    )
    .unwrap_or(0)
}

/// The channel layout of bus `bus` on `direction`.
///
/// # Errors
/// An out-of-range `bus` returns the AU's own `kAudioUnitErr_InvalidElement`
/// (`-10877`) rather than a fabricated layout. That distinction is the point:
/// `ChannelLayout` has no "absent" value, so a host that got `Stereo` back for
/// bus 99 would size buffers for a bus that does not exist. Measured on macOS
/// 15.6: DLSMusicDevice has 2 output buses, and asking for output bus 2 or 99
/// returns -10877 from AudioToolbox — the error is the AU's, not invented here.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub(crate) unsafe fn bus_layout(
    unit: AudioUnit,
    direction: BusDirection,
    bus: u32,
) -> Result<ChannelLayout> {
    let asbd = get_property::<AudioStreamBasicDescription>(
        unit,
        K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
        direction.scope(),
        bus,
    )?;
    Ok(ChannelLayout::from(asbd.mChannelsPerFrame))
}

/// Every channel configuration the AU declares, decoded from
/// `kAudioUnitProperty_SupportedNumChannels`.
///
/// An empty vec means the AU declares nothing — which is *not* the same as
/// declaring it supports nothing. Most Apple effects (all 22 measured on macOS
/// 15.6) refuse this property outright, meaning "no constraint published, ask
/// the stream format instead". Callers must treat empty as "unconstrained", and
/// that reading is why this returns a plain `Vec` rather than a `Result`: a
/// refusal and an empty list are the same answer to the caller's question.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub(crate) unsafe fn supported_channel_configs(unit: AudioUnit) -> Vec<AuChannelConfig> {
    let bytes = match get_property_bytes(
        unit,
        K_AUDIO_UNIT_PROPERTY_SUPPORTED_NUM_CHANNELS,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    ) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };

    // Decode by chunk rather than by `slice::from_raw_parts` over the whole
    // buffer: an AU that reports a size which is not a whole multiple of the
    // struct would otherwise have its trailing partial entry read as a full
    // one, out of bounds of what was actually written.
    bytes
        .chunks_exact(std::mem::size_of::<AuChannelInfo>())
        .map(|c| {
            AuChannelConfig::from_raw(AuChannelInfo {
                in_channels: i16::from_ne_bytes([c[0], c[1]]),
                out_channels: i16::from_ne_bytes([c[2], c[3]]),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-declared `AUChannelInfo` must match Apple's. Two `SInt16`s in
    /// declared order, 4 bytes total, 2-byte aligned — if this drifts, every
    /// decoded entry is garbage but nothing else would notice.
    #[test]
    fn channel_info_matches_the_c_abi() {
        assert_eq!(std::mem::size_of::<AuChannelInfo>(), 4);
        assert_eq!(std::mem::align_of::<AuChannelInfo>(), 2);
        assert_eq!(std::mem::offset_of!(AuChannelInfo, in_channels), 0);
        assert_eq!(std::mem::offset_of!(AuChannelInfo, out_channels), 2);
    }

    /// Each documented sentinel decodes to its own variant. The failure this
    /// pins is the coercion one: `-16` must never become a channel *count* of
    /// 16 or of -16, only a total-across-buses cap.
    #[test]
    fn sentinels_decode_as_sentinels_not_as_counts() {
        assert_eq!(AuChannelCount::from_raw(0), AuChannelCount::NoElements);
        assert_eq!(AuChannelCount::from_raw(2), AuChannelCount::Exactly(2));
        assert_eq!(
            AuChannelCount::from_raw(-1),
            AuChannelCount::Any { wildcard: -1 }
        );
        assert_eq!(
            AuChannelCount::from_raw(-2),
            AuChannelCount::Any { wildcard: -2 }
        );
        // The AUSampler / AUMIDISynth case, measured on macOS 15.6: `{0, -16}`.
        assert_eq!(
            AuChannelCount::from_raw(-16),
            AuChannelCount::TotalAcrossBuses { max_total: 16 }
        );
        // -3 is the first value on the total-across-buses side of the boundary,
        // and -2 the last wildcard: the exact edge the `n < -2` guard draws.
        assert_eq!(
            AuChannelCount::from_raw(-3),
            AuChannelCount::TotalAcrossBuses { max_total: 3 }
        );
        // `i16::MIN` has no positive counterpart; `unsigned_abs` is what keeps
        // this from overflowing rather than producing a wrapped negative.
        assert_eq!(
            AuChannelCount::from_raw(i16::MIN),
            AuChannelCount::TotalAcrossBuses { max_total: 32_768 }
        );
    }

    /// `{-1,-1}` constrains the two sides to be equal; `{-1,-2}` does not. This
    /// is the whole reason the raw sentinel is retained inside `Any`.
    #[test]
    fn the_two_wildcard_spellings_mean_different_things() {
        let matched = AuChannelConfig {
            inputs: AuChannelCount::Any { wildcard: -1 },
            outputs: AuChannelCount::Any { wildcard: -1 },
        };
        let independent = AuChannelConfig {
            inputs: AuChannelCount::Any { wildcard: -1 },
            outputs: AuChannelCount::Any { wildcard: -2 },
        };

        assert!(matched.requires_matching_counts());
        assert!(!independent.requires_matching_counts());

        // N-to-N is fine under both.
        assert!(matched.admits(2, 2));
        assert!(independent.admits(2, 2));
        // N-to-M is fine only under the independent spelling. A decoder that
        // collapsed both sentinels into one "any" would admit this on `matched`
        // and offer the user a configuration the AU cannot run.
        assert!(!matched.admits(2, 6));
        assert!(independent.admits(2, 6));
    }

    /// The real entries measured on this machine (macOS 15.6), decoded.
    #[test]
    fn measured_apple_entries_decode_to_their_documented_meaning() {
        // DLSMusicDevice: `{0, 2}` — no input elements, exactly 2 out.
        let dls = AuChannelConfig::from_raw(AuChannelInfo {
            in_channels: 0,
            out_channels: 2,
        });
        assert_eq!(dls.inputs, AuChannelCount::NoElements);
        assert_eq!(dls.outputs, AuChannelCount::Exactly(2));
        assert!(dls.admits(0, 2));
        assert!(!dls.admits(2, 2), "DLS declares no input elements");

        // AUSampler / AUMIDISynth: `{0, -16}` — no input, <=16 channels total
        // across the output scope. NOT "-16 channels" and NOT "exactly 16".
        let sampler = AuChannelConfig::from_raw(AuChannelInfo {
            in_channels: 0,
            out_channels: -16,
        });
        assert_eq!(sampler.inputs, AuChannelCount::NoElements);
        assert_eq!(
            sampler.outputs,
            AuChannelCount::TotalAcrossBuses { max_total: 16 }
        );
        assert!(sampler.admits(0, 2));
        assert!(sampler.admits(0, 16));
        assert!(!sampler.admits(0, 17), "17 exceeds the declared 16 total");

        // AUMatrixMixer / AUMultiChannelMixer: `{-1, -2}` — any in, any out,
        // independently.
        let mixer = AuChannelConfig::from_raw(AuChannelInfo {
            in_channels: -1,
            out_channels: -2,
        });
        assert!(!mixer.requires_matching_counts());
        assert!(mixer.admits(8, 2));

        // AUMultiSplitter: `{-1, -1}` — any, but matched.
        let splitter = AuChannelConfig::from_raw(AuChannelInfo {
            in_channels: -1,
            out_channels: -1,
        });
        assert!(splitter.requires_matching_counts());
        assert!(splitter.admits(6, 6));
        assert!(!splitter.admits(6, 2));

        // AURoundTripAAC: `{1,1} … {8,8}` — literal pairs, no sentinels at all.
        let literal = AuChannelConfig::from_raw(AuChannelInfo {
            in_channels: 2,
            out_channels: 2,
        });
        assert!(literal.admits(2, 2));
        assert!(!literal.admits(2, 4));
        assert!(
            !literal.requires_matching_counts(),
            "a literal pair names both sides outright; the matching rule is for \
             the {{-1,-1}} spelling only"
        );
    }

    /// `NoElements` admits only zero. A side declaring no elements that
    /// nonetheless accepted a positive count would let a host wire input into
    /// an instrument.
    #[test]
    fn no_elements_admits_only_zero() {
        assert!(AuChannelCount::NoElements.admits(0));
        assert!(!AuChannelCount::NoElements.admits(1));
        assert!(!AuChannelCount::NoElements.admits(2));
    }

    /// Direction maps onto the AudioToolbox scope constants and nothing else.
    #[test]
    fn direction_maps_to_the_audio_toolbox_scopes() {
        assert_eq!(BusDirection::Input.scope(), K_AUDIO_UNIT_SCOPE_INPUT);
        assert_eq!(BusDirection::Output.scope(), K_AUDIO_UNIT_SCOPE_OUTPUT);
        assert_ne!(BusDirection::Input.scope(), BusDirection::Output.scope());
        assert_eq!(BusDirection::ALL.len(), 2);
    }
}
