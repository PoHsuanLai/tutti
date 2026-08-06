//! Channel *order* — which speaker each channel of a bus feeds — plus the
//! per-element names an AU gives its buses.
//!
//! [`crate::bus`] answers "how many channels", [`crate::stream`] applies a
//! width. Neither says **where those channels go**, and that is a distinct
//! question with an audible wrong answer: an `AudioStreamBasicDescription`
//! carrying `mChannelsPerFrame == 6` describes a 5.1 bus without saying whether
//! channel 3 is the centre or the LFE. Apple's own header is explicit that
//! `kAudioUnitProperty_StreamFormat` "cannot specify channel layout or
//! purpose" — get the order wrong there and centre dialog swaps places with
//! the subwoofer rumble, full level, wrong driver.
//!
//! Three properties live here:
//!
//! * `kAudioUnitProperty_SupportedChannelLayoutTags` (32, read) — the orders the
//!   AU says it understands, per bus.
//! * `kAudioUnitProperty_AudioChannelLayout` (19, read/write) — the order a bus
//!   is running right now, and the one a host sets.
//! * `kAudioUnitProperty_ElementName` (30, read) — the AU's own name for a bus.
//!
//! Element names are here rather than in [`crate::bus`] for two reasons: the
//! property is addressed by the same `(scope, element)` pair the layout
//! properties are, and unlike everything in `bus.rs` it returns a
//! **CoreFoundation reference the host owns** (see [`element_name`]) — an
//! ownership discipline `bus.rs` has none of.
//!
//! # What was measured on macOS 15.6
//!
//! Every assertion here was probed against the ~35 Apple AUs plus TDR Nova /
//! TAL-NoiseMaker / TAL-Reverb-4. Two measurements shaped the API:
//! [`set_layout_tag`] (the width gate) and [`element_name`] (the retain-count
//! leak).

#![cfg(target_os = "macos")]

use std::mem::size_of;
use std::os::raw::c_void;

use crate::bus::BusDirection;
use crate::error::{AuError, Result};
use crate::ffi::{check, get_property_bytes};
use crate::types::*;

/// A channel *order* an AU can run a bus in — an `AudioChannelLayoutTag`.
///
/// # Why a typed enum with an `Unknown` arm
///
/// The tag namespace is an **open catalog**: CoreAudioTypes.h ships well over a
/// hundred tags and Apple adds more per release, and third-party AUs may
/// publish any of them. A closed enum would turn every OS update into a decode
/// failure, and a bare `u32` would let a caller compare a tag against a channel
/// *count* and compile. So the variants name the layouts a DAW routes by hand,
/// and [`Self::Unknown`] carries anything else verbatim for echoing back to the
/// AU or logging.
///
/// # The alias trap
///
/// Several of Apple's constants are **the same numeric value under two names**
/// — read off this SDK (macOS 15.6), not copied from documentation:
///
/// | value | names |
/// |---|---|
/// | `0x0064_0001` | `Mono`, `MPEG_1_0`, `ITU_1_0`, `DVD_0` |
/// | `0x0065_0002` | `Stereo`, `MPEG_2_0`, `ITU_2_0`, `DVD_1` |
/// | `0x006C_0004` | `Quadraphonic`, `AudioUnit_4` |
/// | `0x006E_0006` | `Hexagonal`, `AudioUnit_6` |
/// | `0x006F_0008` | `Octagonal`, `AudioUnit_8` |
/// | `0x0076_0005` | `AudioUnit_5_0`, `MPEG_5_0_B` |
/// | `0x0079_0006` | `AudioUnit_5_1`, `MPEG_5_1_A`, `ITU_3_2_1` |
/// | `0x0080_0008` | `AudioUnit_7_1`, `MPEG_7_1_C`, `ITU_3_4_1` |
///
/// So a `match` listing both members of any pair is an **unreachable-pattern
/// compile error**, and a `from_raw` written as a chain of `if x == k…` would
/// silently resolve to whichever alias came first. This enum therefore has
/// exactly **one variant per distinct value**, and each variant's doc names the
/// aliases it also answers for. The [`tests`] module pins the aliasing so a
/// future SDK that splits a pair fails a test rather than changing behaviour.
///
/// See [`Self::channel_count`] for how the width is recovered from the raw tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuLayoutTag {
    /// `kAudioChannelLayoutTag_UseChannelDescriptions`: the layout is not a tag
    /// at all — the AU wants an explicit per-channel description array. This is
    /// what AUMultiChannelMixer and AUNetSend publish (measured, macOS 15.6),
    /// and it is why `channel_count` returns `None`: the count lives in
    /// `mNumberChannelDescriptions`, not in the tag.
    UseChannelDescriptions,
    /// `kAudioChannelLayoutTag_UseChannelBitmap`: the channels are named by an
    /// `AudioChannelBitmap` instead. Apple's docs state the bitmap form "is NOT
    /// used within the context of the AudioUnit", so an AU reporting this is
    /// misbehaving — the variant exists so that is *named* rather than falling
    /// into `Unknown`, indistinguishable from a tag from a newer SDK.
    UseChannelBitmap,
    /// One channel. Also `MPEG_1_0`, `ITU_1_0`, `DVD_0`, `AudioUnit_1`.
    Mono,
    /// Two channels, L R. Also `MPEG_2_0`, `ITU_2_0`, `DVD_1`, `AudioUnit_2`.
    Stereo,
    /// Two channels for headphone playback. Distinct value from [`Self::Stereo`]
    /// despite the same width, because it tells the AU not to apply crosstalk
    /// or speaker compensation.
    StereoHeadphones,
    /// Two channels, matrix-encoded (Lt Rt). Distinct from [`Self::Stereo`]:
    /// naive stereo downmixing of a matrix pair destroys the encoded surround.
    MatrixStereo,
    /// Two channels, mid/side. Requires a decode to L/R before anything can pan
    /// it, which is exactly why the order must not be inferred from the width.
    MidSide,
    /// Two channels, coincident X/Y pair.
    XY,
    /// Two channels, binaural.
    Binaural,
    /// Four channels of first-order ambisonic B-format (W X Y Z). Same *width*
    /// as [`Self::Quadraphonic`] and a completely different meaning — B-format
    /// is a spherical-harmonic encoding, not four speaker feeds.
    AmbisonicBFormat,
    /// Four channels, L R Ls Rs. **Also `kAudioChannelLayoutTag_AudioUnit_4`** —
    /// the two constants are the same value, so this one variant answers for
    /// both. Measured: AUMatrixReverb publishes this on its output.
    Quadraphonic,
    /// Five channels, L R C Ls Rs (pentagonal placement).
    Pentagonal,
    /// Six channels. Also `kAudioChannelLayoutTag_AudioUnit_6`.
    Hexagonal,
    /// Eight channels. Also `kAudioChannelLayoutTag_AudioUnit_8`.
    Octagonal,
    /// Eight channels arranged as a cube.
    Cube,
    /// Five channels, L R C Ls Rs — the **AU** 5.0 order. Also
    /// `kAudioChannelLayoutTag_MPEG_5_0_B`; NOT the same value as
    /// [`Self::Pentagonal`] despite both being five channels. Measured:
    /// AUMatrixReverb publishes this on its output.
    AudioUnit5_0,
    /// Six channels, L R C Ls Rs LFE — the **AU** 5.1 order, and the layout
    /// whose order the module docs are about. Also
    /// `kAudioChannelLayoutTag_MPEG_5_1_A` and `ITU_3_2_1`.
    AudioUnit5_1,
    /// Six channels, L R Ls Rs C Cs.
    AudioUnit6_0,
    /// Seven channels, L R Ls Rs C Rls Rrs.
    AudioUnit7_0,
    /// Eight channels — the **AU** 7.1 order. Also
    /// `kAudioChannelLayoutTag_MPEG_7_1_C` and `ITU_3_4_1`.
    AudioUnit7_1,
    /// A tag this crate does not name. Carried verbatim so it can be echoed back
    /// to the AU, compared for equality, and logged — a newer SDK's spatial
    /// layout arrives here rather than becoming a decode failure.
    Unknown(u32),
}

impl AuLayoutTag {
    /// Decode a raw `AudioChannelLayoutTag`.
    ///
    /// A `match` on `const` patterns rather than an `if` chain, so the compiler
    /// enforces the no-duplicate-value rule from the type docs: an aliased
    /// constant added here is a hard error, not a silently-shadowed arm.
    pub fn from_raw(raw: u32) -> Self {
        // Local consts so these are `match` patterns (which reject duplicates)
        // rather than guard expressions (which would not).
        const USE_DESC: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_USE_CHANNEL_DESCRIPTIONS;
        const USE_BITMAP: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_USE_CHANNEL_BITMAP;
        const MONO: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_MONO;
        const STEREO: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_STEREO;
        const HEADPHONES: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_STEREO_HEADPHONES;
        const MATRIX: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_MATRIX_STEREO;
        const MID_SIDE: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_MID_SIDE;
        const XY: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_XY;
        const BINAURAL: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_BINAURAL;
        const AMBISONIC: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_AMBISONIC_B_FORMAT;
        const QUAD: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_QUADRAPHONIC;
        const PENTAGONAL: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_PENTAGONAL;
        const HEXAGONAL: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_HEXAGONAL;
        const OCTAGONAL: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_OCTAGONAL;
        const CUBE: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_CUBE;
        const AU_5_0: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_0;
        const AU_5_1: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_1;
        const AU_6_0: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_6_0;
        const AU_7_0: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_7_0;
        const AU_7_1: u32 = K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_7_1;

        match raw {
            USE_DESC => Self::UseChannelDescriptions,
            USE_BITMAP => Self::UseChannelBitmap,
            MONO => Self::Mono,
            STEREO => Self::Stereo,
            HEADPHONES => Self::StereoHeadphones,
            MATRIX => Self::MatrixStereo,
            MID_SIDE => Self::MidSide,
            XY => Self::XY,
            BINAURAL => Self::Binaural,
            AMBISONIC => Self::AmbisonicBFormat,
            QUAD => Self::Quadraphonic,
            PENTAGONAL => Self::Pentagonal,
            HEXAGONAL => Self::Hexagonal,
            OCTAGONAL => Self::Octagonal,
            CUBE => Self::Cube,
            AU_5_0 => Self::AudioUnit5_0,
            AU_5_1 => Self::AudioUnit5_1,
            AU_6_0 => Self::AudioUnit6_0,
            AU_7_0 => Self::AudioUnit7_0,
            AU_7_1 => Self::AudioUnit7_1,
            other => Self::Unknown(other),
        }
    }

    /// The raw `AudioChannelLayoutTag` value, for writing back to the AU.
    ///
    /// Round-trips with [`Self::from_raw`] for every named variant *and* for
    /// `Unknown` — a host can read a tag it does not understand off one AU and
    /// set it on another.
    pub fn to_raw(self) -> u32 {
        match self {
            Self::UseChannelDescriptions => K_AUDIO_CHANNEL_LAYOUT_TAG_USE_CHANNEL_DESCRIPTIONS,
            Self::UseChannelBitmap => K_AUDIO_CHANNEL_LAYOUT_TAG_USE_CHANNEL_BITMAP,
            Self::Mono => K_AUDIO_CHANNEL_LAYOUT_TAG_MONO,
            Self::Stereo => K_AUDIO_CHANNEL_LAYOUT_TAG_STEREO,
            Self::StereoHeadphones => K_AUDIO_CHANNEL_LAYOUT_TAG_STEREO_HEADPHONES,
            Self::MatrixStereo => K_AUDIO_CHANNEL_LAYOUT_TAG_MATRIX_STEREO,
            Self::MidSide => K_AUDIO_CHANNEL_LAYOUT_TAG_MID_SIDE,
            Self::XY => K_AUDIO_CHANNEL_LAYOUT_TAG_XY,
            Self::Binaural => K_AUDIO_CHANNEL_LAYOUT_TAG_BINAURAL,
            Self::AmbisonicBFormat => K_AUDIO_CHANNEL_LAYOUT_TAG_AMBISONIC_B_FORMAT,
            Self::Quadraphonic => K_AUDIO_CHANNEL_LAYOUT_TAG_QUADRAPHONIC,
            Self::Pentagonal => K_AUDIO_CHANNEL_LAYOUT_TAG_PENTAGONAL,
            Self::Hexagonal => K_AUDIO_CHANNEL_LAYOUT_TAG_HEXAGONAL,
            Self::Octagonal => K_AUDIO_CHANNEL_LAYOUT_TAG_OCTAGONAL,
            Self::Cube => K_AUDIO_CHANNEL_LAYOUT_TAG_CUBE,
            Self::AudioUnit5_0 => K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_0,
            Self::AudioUnit5_1 => K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_1,
            Self::AudioUnit6_0 => K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_6_0,
            Self::AudioUnit7_0 => K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_7_0,
            Self::AudioUnit7_1 => K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_7_1,
            Self::Unknown(raw) => raw,
        }
    }

    /// How many channels this layout describes, or `None` when the tag does not
    /// encode a count.
    ///
    /// Apple packs the count into the tag's low 16 bits, so this answers
    /// correctly even for [`Self::Unknown`] — which is what lets a host size
    /// buffers for a layout from a newer SDK. The two exceptions are
    /// [`Self::UseChannelDescriptions`] and [`Self::UseChannelBitmap`], whose low
    /// bits are `0`: reporting "0 channels" for them would be a lie a caller
    /// cannot detect, so they report `None` and the caller is forced to go read
    /// `mNumberChannelDescriptions` instead.
    pub fn channel_count(self) -> Option<u16> {
        match self {
            Self::UseChannelDescriptions | Self::UseChannelBitmap => None,
            other => Some((other.to_raw() & 0xFFFF) as u16),
        }
    }
}

impl std::fmt::Display for AuLayoutTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UseChannelDescriptions => write!(f, "UseChannelDescriptions"),
            Self::UseChannelBitmap => write!(f, "UseChannelBitmap"),
            Self::Mono => write!(f, "Mono"),
            Self::Stereo => write!(f, "Stereo"),
            Self::StereoHeadphones => write!(f, "StereoHeadphones"),
            Self::MatrixStereo => write!(f, "MatrixStereo"),
            Self::MidSide => write!(f, "MidSide"),
            Self::XY => write!(f, "XY"),
            Self::Binaural => write!(f, "Binaural"),
            Self::AmbisonicBFormat => write!(f, "AmbisonicBFormat"),
            Self::Quadraphonic => write!(f, "Quadraphonic"),
            Self::Pentagonal => write!(f, "Pentagonal"),
            Self::Hexagonal => write!(f, "Hexagonal"),
            Self::Octagonal => write!(f, "Octagonal"),
            Self::Cube => write!(f, "Cube"),
            Self::AudioUnit5_0 => write!(f, "AudioUnit_5_0"),
            Self::AudioUnit5_1 => write!(f, "AudioUnit_5_1"),
            Self::AudioUnit6_0 => write!(f, "AudioUnit_6_0"),
            Self::AudioUnit7_0 => write!(f, "AudioUnit_7_0"),
            Self::AudioUnit7_1 => write!(f, "AudioUnit_7_1"),
            Self::Unknown(raw) => write!(f, "Unknown({raw:#x})"),
        }
    }
}

/// The `AudioChannelLayout` byte length the AU properties actually want.
///
/// `AudioChannelLayout` is a C flexible-array struct: the Rust binding declares
/// `mChannelDescriptions: [AudioChannelDescription; 1]`, so `size_of` counts one
/// description a tag-only layout does not have. Subtracting it gives the
/// 12-byte header — tag, bitmap, description count — the form Apple documents
/// for a tag-only layout ("is only expected to have set ... the layout tag as
/// the valid field").
///
/// Measured on macOS 15.6: AUMatrixReverb accepts **both** 12 and 32 bytes for a
/// tag-only write, so the smaller size is used because it is *correct*, not
/// because it is required — 32 bytes claims a description array that was never
/// populated, which a stricter AU validating `mNumberChannelDescriptions`
/// against the byte count would rightly refuse.
const TAG_ONLY_LAYOUT_SIZE: usize =
    size_of::<AudioChannelLayout>() - size_of::<AudioChannelDescription>();

/// Every channel order the AU says it can run bus `bus` of `direction` in.
///
/// # Empty is "declines to say", never "supports nothing"
///
/// Same reading as [`crate::bus::supported_channel_configs`]:
/// `kAudioUnitProperty_SupportedChannelLayoutTags` is optional and most units
/// simply refuse it. Measured on macOS 15.6, of ~38 units only **11** answer at
/// all — AUMatrixReverb, AUSampler, AUMIDISynth, AUNewPitch, AURoundTripAAC,
/// AUNetSend, AUMixer3D, AUMultiChannelMixer and the three third-party units.
/// AUDelay, AUNBandEQ, AUMatrixMixer and the rest all report
/// `kAudioUnitErr_InvalidProperty` (-10879), absorbed into an empty vec rather
/// than an error — the common case is an AU with no opinion, not a failed read.
///
/// A published tag is not a settable tag: the list is what the AU understands
/// *in principle*, and whether it accepts a given entry right now depends on
/// the bus's configured width — see [`set_layout_tag`]'s width gate.
///
/// Duplicates are returned verbatim rather than deduplicated: AUNewPitch
/// publishes `Quadraphonic` twice (measured), the AU describing its own table,
/// and collapsing it would hide that from a host displaying the list.
pub(crate) unsafe fn supported_layout_tags(
    unit: AudioUnit,
    direction: BusDirection,
    bus: u32,
) -> Vec<AuLayoutTag> {
    let Ok(bytes) = get_property_bytes(
        unit,
        K_AUDIO_UNIT_PROPERTY_SUPPORTED_CHANNEL_LAYOUT_TAGS,
        direction.scope(),
        bus,
    ) else {
        return Vec::new();
    };
    // `chunks_exact` for the reason `bus.rs` uses it: an AU reporting a size
    // that is not a whole multiple of the element would otherwise have its
    // trailing partial tag read out of bounds of what was written.
    bytes
        .chunks_exact(size_of::<u32>())
        .map(|c| AuLayoutTag::from_raw(u32::from_ne_bytes([c[0], c[1], c[2], c[3]])))
        .collect()
}

/// The channel order bus `bus` of `direction` is running right now.
///
/// # Errors
///
/// Three distinct refusals reach the caller as [`AuError::OsStatus`], and the
/// status code is the only thing that separates them — which is why none is
/// flattened into a default:
///
/// * `kAudioUnitErr_InvalidProperty` (-10879) — the AU has no channel-layout
///   property at all. Measured: AUDelay, AUNBandEQ, AUMatrixMixer, and every
///   other Apple effect bar AUMatrixReverb.
/// * `kAudioUnitErr_PropertyNotInUse` (-10851) — the property exists but no
///   layout has been set. Apple's header names this case explicitly. Measured on
///   AUSampler's output, which *does* publish `[Mono, Stereo]` as supported.
/// * `kAudioUnitErr_InvalidElement` (-10877) — no such bus.
///
/// A default would mean picking a speaker order for a bus whose order is
/// genuinely unknown — how centre dialog ends up in the LFE — so the absence is
/// reported and the caller decides.
///
/// Only the tag is returned, not the whole `AudioChannelLayout`: every unit
/// measured answers with a tag-only layout (`mNumberChannelDescriptions == 0`),
/// and a description array would need an owned allocation whose only consumer —
/// a `UseChannelDescriptions` unit — none of them sets. If that changes, this
/// returns the tag `UseChannelDescriptions` and a caller can tell.
pub(crate) unsafe fn layout_tag(
    unit: AudioUnit,
    direction: BusDirection,
    bus: u32,
) -> Result<AuLayoutTag> {
    let bytes = get_property_bytes(
        unit,
        K_AUDIO_UNIT_PROPERTY_AUDIO_CHANNEL_LAYOUT,
        direction.scope(),
        bus,
    )?;
    // The AU controls this length, and measured sizes differ per unit for the
    // same logical value (AUMatrixReverb: 32 bytes; AUMultiChannelMixer and
    // AUSampler: 12), so a fixed-`size_of` `get_property::<AudioChannelLayout>`
    // would read a length neither agreed to. Anything shorter than the 4-byte
    // tag has no tag to report.
    if bytes.len() < size_of::<u32>() {
        return Err(AuError::OsStatus {
            function: "channel_layout::layout_tag",
            code: K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE,
        });
    }
    Ok(AuLayoutTag::from_raw(u32::from_ne_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
    ])))
}

/// Ask the AU to run bus `bus` of `direction` in the `tag` channel order.
///
/// # The width gate — the measurement that shapes this API
///
/// A tag from [`supported_layout_tags`] is **not** automatically settable. The
/// AU checks the tag's channel count against the width already configured on
/// that bus, and refuses any disagreement. Measured on macOS 15.6 against
/// AUMatrixReverb's output, which publishes `[Stereo, Quadraphonic,
/// AudioUnit_5_0]`:
///
/// | configured width | tag | result |
/// |---|---|---|
/// | 2 | `Stereo` | `noErr` |
/// | 4 | `Quadraphonic` | `noErr` |
/// | 5 | `AudioUnit_5_0` | `noErr` |
/// | 4 | `AudioUnit_5_0` | **-10851** |
/// | 5 | `Quadraphonic` | **-10851** |
/// | 2 | `Quadraphonic` | **-10851** |
///
/// Apple's header states the rule ("The number of channels it describes must
/// match the number of channels set for that scope/element"), but it is worth
/// spelling out because of how it presents: at the *default* stereo width every
/// surround tag the AU advertises is refused, so a host that read the supported
/// list and set an entry from it straight away would conclude the AU was lying.
/// It is not — the width has to be applied first, via
/// [`crate::stream::StreamConfig`] / `AuInstance::new_with_config`.
///
/// This function deliberately does **not** widen the stream format on the
/// caller's behalf: reconfiguring width is [`crate::stream`]'s duty, requires
/// an uninitialized AU, and doing it here would silently resize buffers the
/// caller had already allocated.
///
/// # Errors
///
/// [`AuError::OsStatus`] with the AU's own status. The two seen in practice:
///
/// * `kAudioUnitErr_InvalidPropertyValue` (-10851) — the tag is not one this bus
///   will take at its current width, per the table above. Also what an
///   unpublished tag returns (measured: `AudioUnit_5_1` on AUMatrixReverb at
///   6 channels, and `Quadraphonic` on AUSampler).
/// * `kAudioUnitErr_InvalidProperty` (-10879) — the AU has no channel-layout
///   property, so there is no order to set.
///
/// Propagated rather than absorbed for the same reason as
/// [`crate::instance::AuInstance::set_bypass`]: a host that believes it set 5.1
/// while the AU kept stereo will route six channels into a two-channel bus and
/// never learn why the surround came out wrong.
pub(crate) unsafe fn set_layout_tag(
    unit: AudioUnit,
    direction: BusDirection,
    bus: u32,
    tag: AuLayoutTag,
) -> Result<()> {
    let layout = AudioChannelLayout {
        mChannelLayoutTag: tag.to_raw(),
        // Zeroed deliberately: Apple's header says the bitmap form "is NOT used
        // within the context of the AudioUnit", and a tag-only layout declares
        // no descriptions. A non-zero count here would promise an array that
        // `TAG_ONLY_LAYOUT_SIZE` bytes do not carry.
        mChannelBitmap: 0,
        mNumberChannelDescriptions: 0,
        mChannelDescriptions: [AudioChannelDescription::default(); 1],
    };
    // Not `crate::ffi::set_property`: that helper sends `size_of::<T>()`, which
    // for this flexible-array struct over-reports by one description. See
    // `TAG_ONLY_LAYOUT_SIZE`.
    check(
        "AudioUnitSetProperty",
        AudioUnitSetProperty(
            unit,
            K_AUDIO_UNIT_PROPERTY_AUDIO_CHANNEL_LAYOUT,
            direction.scope(),
            bus,
            &layout as *const AudioChannelLayout as *const c_void,
            TAG_ONLY_LAYOUT_SIZE as u32,
        ),
    )
}

/// The AU's own name for bus `bus` of `direction` — "Sidechain", "stereo mix".
///
/// Without this a mixer's eight inputs render as "Bus 1..8" in the host UI while
/// the plugin has perfectly good names for them, and a sidechain input is
/// indistinguishable from a second audio input.
///
/// # This property leaks unless the host releases — measured
///
/// `kAudioUnitProperty_ElementName` is **Copy-rule**: the returned `CFStringRef`
/// carries a retain the host owns. Apple's header says so ("The Host owns a
/// reference to this property value ... and should release the string
/// retrieved"), and it was verified rather than trusted, because most of the
/// strings involved *look* like they need no release:
///
/// | unit / element | retain count |
/// |---|---|
/// | TDR Nova `in[0]` "Input" | `i64::MAX` (immortal constant) |
/// | DLSMusicDevice `out[0]` "stereo mix" | `0x0FFF_FFFF_FFFF_FFFF` (immortal) |
/// | TAL-NoiseMaker `out[0]` "Output Master" | **2** — a real, mortal object |
///
/// Reading the TAL-NoiseMaker name ten times **without** releasing walks the
/// retain count `2,3,4,…,11`; releasing each read holds it flat at `2`. The leak
/// is real and only that unit exposes it — a host tested against Apple's units
/// alone would see immortal strings, conclude no release was needed, and leak
/// one CFString per read on every third-party AU. A UI polling bus names on a
/// redraw leaks unboundedly.
///
/// [`crate::cf::CfString::from_copied`] takes that +1 and releases on drop, on
/// every path out including the `checked` rejection below.
///
/// # `_checked`, not the bare converter
///
/// The pointer comes from the plugin. `cfstring_to_string_checked` stands
/// between an AU that answers `noErr` with garbage and a `SIGBUS` inside
/// CoreFoundation — the crash class already fixed once here, on factory-preset
/// names. Element names are also exactly the short strings whose arm64 form is
/// a **tagged pointer**: "Input" is legitimately misaligned, so it is the
/// checked converter's tag-bit allowance, not a plain alignment test, that keeps
/// them from being silently dropped.
///
/// # Errors
///
/// [`AuError::OsStatus`]. The two statuses are distinct facts and neither becomes
/// an empty string:
///
/// * `kAudioUnitErr_PropertyNotInUse` (-10850) — the bus exists but the AU gave
///   it no name. Measured: **every Apple mixer**. AUMultiChannelMixer returns
///   -10850 for inputs 0..=7 and AUMatrixMixer for 0..=63 — i.e. for exactly
///   their real buses. So Apple's mixers publish no element names at all on
///   this machine; the units that do are DLSMusicDevice (`out[0]` "stereo
///   mix", `out[1]` "unused") plus the third-party effects (TDR Nova `in[1]`
///   "Sidechain").
/// * `kAudioUnitErr_InvalidElement` (-10877) — no such bus. Measured:
///   AUMultiChannelMixer input 8, AUMatrixMixer input 64, DLSMusicDevice output
///   2 — one past each unit's real count in every case.
///
/// The split is the useful part — a name-less bus versus one that does not
/// exist — and `Ok(String::new())` for either would erase it.
pub(crate) unsafe fn element_name(
    unit: AudioUnit,
    direction: BusDirection,
    bus: u32,
) -> Result<String> {
    let mut raw: CFStringRef = std::ptr::null();
    let mut size = size_of::<CFStringRef>() as u32;
    check(
        "AudioUnitGetProperty",
        AudioUnitGetProperty(
            unit,
            K_AUDIO_UNIT_PROPERTY_ELEMENT_NAME,
            direction.scope(),
            bus,
            &mut raw as *mut CFStringRef as *mut c_void,
            &mut size,
        ),
    )?;

    // Take ownership of the +1 FIRST, before anything can return early: the
    // `checked` conversion below can reject the pointer, and an early return
    // ahead of this line would leak the retain the AU just handed over — the
    // exact leak the doc comment above measured.
    //
    // `from_copied` is null-tolerant and returns `None`, the honest answer for a
    // unit that reports `noErr` with a null string.
    let owned = crate::cf::CfString::from_copied(raw);
    let Some(_owned) = owned else {
        return Err(AuError::OsStatus {
            function: "channel_layout::element_name",
            code: K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE,
        });
    };
    // Borrow through the raw pointer rather than `_owned.to_string()`: the
    // checked converter is what validates that this really is a CFString, and
    // `CfString::to_string` would dereference first.
    match cfstring_to_string_checked(raw) {
        Some(name) => Ok(name),
        // A non-null value that is not a live CFString. `_owned` still releases
        // on drop, so the retain is not leaked even on this path.
        None => Err(AuError::OsStatus {
            function: "channel_layout::element_name",
            code: K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The aliasing this type's docs claim must actually hold in the SDK we
    /// compile against. If a future CoreAudioTypes.h splits any of these pairs,
    /// `from_raw`'s `match` would gain an unreachable arm — or worse, start
    /// resolving to the other name — and this test is what notices.
    #[test]
    fn the_aliased_tags_really_are_the_same_value() {
        assert_eq!(
            K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_4, K_AUDIO_CHANNEL_LAYOUT_TAG_QUADRAPHONIC,
            "AudioUnit_4 and Quadraphonic are one value; listing both in a match \
             is an unreachable-pattern error"
        );
        assert_eq!(
            K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_1, K_AUDIO_CHANNEL_LAYOUT_TAG_MPEG_5_1_A,
            "AudioUnit_5_1 == MPEG_5_1_A"
        );
        assert_eq!(
            K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_7_1, K_AUDIO_CHANNEL_LAYOUT_TAG_MPEG_7_1_C,
            "AudioUnit_7_1 == MPEG_7_1_C"
        );
        assert_eq!(
            K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_0, K_AUDIO_CHANNEL_LAYOUT_TAG_MPEG_5_0_B,
            "AudioUnit_5_0 == MPEG_5_0_B"
        );
        assert_eq!(
            K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_6, K_AUDIO_CHANNEL_LAYOUT_TAG_HEXAGONAL,
            "AudioUnit_6 == Hexagonal"
        );
        assert_eq!(
            K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_8, K_AUDIO_CHANNEL_LAYOUT_TAG_OCTAGONAL,
            "AudioUnit_8 == Octagonal"
        );

        // And the counterweight: values that are NOT aliases must stay distinct,
        // or the enum would be collapsing two real layouts into one. Both pairs
        // below share a channel *width* while meaning different orders — which is
        // the entire reason a width cannot substitute for a tag.
        assert_ne!(
            K_AUDIO_CHANNEL_LAYOUT_TAG_QUADRAPHONIC, K_AUDIO_CHANNEL_LAYOUT_TAG_AMBISONIC_B_FORMAT,
            "quad speaker feeds and 4-channel B-format are different layouts"
        );
        assert_ne!(
            K_AUDIO_CHANNEL_LAYOUT_TAG_PENTAGONAL, K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_0,
            "two five-channel layouts with different speaker orders"
        );
        assert_ne!(
            K_AUDIO_CHANNEL_LAYOUT_TAG_STEREO, K_AUDIO_CHANNEL_LAYOUT_TAG_MATRIX_STEREO,
            "matrix-encoded stereo is not plain stereo"
        );
    }

    /// Every named variant round-trips, and so does `Unknown` — which is what
    /// lets a host echo a tag from a newer SDK back to an AU unchanged.
    #[test]
    fn every_tag_round_trips_through_raw() {
        let named = [
            AuLayoutTag::UseChannelDescriptions,
            AuLayoutTag::UseChannelBitmap,
            AuLayoutTag::Mono,
            AuLayoutTag::Stereo,
            AuLayoutTag::StereoHeadphones,
            AuLayoutTag::MatrixStereo,
            AuLayoutTag::MidSide,
            AuLayoutTag::XY,
            AuLayoutTag::Binaural,
            AuLayoutTag::AmbisonicBFormat,
            AuLayoutTag::Quadraphonic,
            AuLayoutTag::Pentagonal,
            AuLayoutTag::Hexagonal,
            AuLayoutTag::Octagonal,
            AuLayoutTag::Cube,
            AuLayoutTag::AudioUnit5_0,
            AuLayoutTag::AudioUnit5_1,
            AuLayoutTag::AudioUnit6_0,
            AuLayoutTag::AudioUnit7_0,
            AuLayoutTag::AudioUnit7_1,
        ];
        for tag in named {
            assert_eq!(
                AuLayoutTag::from_raw(tag.to_raw()),
                tag,
                "{tag} must survive a raw round-trip"
            );
        }

        // No two named variants may share a raw value, or `from_raw` is picking
        // a winner and the loser is unreachable.
        let mut raws: Vec<u32> = named.iter().map(|t| t.to_raw()).collect();
        let before = raws.len();
        raws.sort_unstable();
        raws.dedup();
        assert_eq!(
            raws.len(),
            before,
            "two named variants collide on one raw value"
        );

        // An unrecognised tag survives verbatim. `0x0099_0003` is in Apple's
        // reserved-but-unassigned space at the time of writing; the point is the
        // round-trip, not the specific number.
        let future = AuLayoutTag::from_raw(0x0099_0003);
        assert_eq!(future, AuLayoutTag::Unknown(0x0099_0003));
        assert_eq!(future.to_raw(), 0x0099_0003);
    }

    /// The count comes out of the tag's low 16 bits — including for `Unknown`,
    /// which is what makes an unrecognised layout still usable for sizing. The
    /// two "look elsewhere" tags report `None` rather than a misleading `0`.
    #[test]
    fn channel_count_reads_the_low_word_and_refuses_to_guess() {
        assert_eq!(AuLayoutTag::Mono.channel_count(), Some(1));
        assert_eq!(AuLayoutTag::Stereo.channel_count(), Some(2));
        assert_eq!(AuLayoutTag::MidSide.channel_count(), Some(2));
        assert_eq!(AuLayoutTag::Quadraphonic.channel_count(), Some(4));
        assert_eq!(AuLayoutTag::AmbisonicBFormat.channel_count(), Some(4));
        assert_eq!(AuLayoutTag::AudioUnit5_0.channel_count(), Some(5));
        assert_eq!(AuLayoutTag::Pentagonal.channel_count(), Some(5));
        assert_eq!(AuLayoutTag::AudioUnit5_1.channel_count(), Some(6));
        assert_eq!(AuLayoutTag::Hexagonal.channel_count(), Some(6));
        assert_eq!(AuLayoutTag::AudioUnit7_0.channel_count(), Some(7));
        assert_eq!(AuLayoutTag::AudioUnit7_1.channel_count(), Some(8));
        assert_eq!(AuLayoutTag::Octagonal.channel_count(), Some(8));

        // A tag from a newer SDK still yields a usable width.
        assert_eq!(AuLayoutTag::Unknown(0x0099_0003).channel_count(), Some(3));

        // These two encode 0 in the low word and mean "the count is somewhere
        // else". Reporting Some(0) would have a caller allocate an empty bus.
        assert_eq!(AuLayoutTag::UseChannelDescriptions.channel_count(), None);
        assert_eq!(AuLayoutTag::UseChannelBitmap.channel_count(), None);
    }

    /// The tag-only write size must be the 12-byte header, not the Rust struct's
    /// `size_of`. Getting this wrong sends a `mNumberChannelDescriptions == 0`
    /// layout inside 32 bytes, claiming a description array that is not there.
    #[test]
    fn the_tag_only_write_size_is_the_header_alone() {
        assert_eq!(
            TAG_ONLY_LAYOUT_SIZE, 12,
            "tag (4) + bitmap (4) + description count (4)"
        );
        // And it must genuinely be smaller than the full struct, or the
        // subtraction is not doing anything.
        assert!(TAG_ONLY_LAYOUT_SIZE < size_of::<AudioChannelLayout>());
        // The three header fields sit where the arithmetic assumes.
        assert_eq!(
            std::mem::offset_of!(AudioChannelLayout, mChannelLayoutTag),
            0
        );
        assert_eq!(std::mem::offset_of!(AudioChannelLayout, mChannelBitmap), 4);
        assert_eq!(
            std::mem::offset_of!(AudioChannelLayout, mNumberChannelDescriptions),
            8
        );
    }
}
