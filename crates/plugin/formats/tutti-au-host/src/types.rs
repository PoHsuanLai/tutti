//! AudioToolbox types, constants, and FFI — sourced from `coreaudio-sys`.
//!
//! The raw `extern "C"` declarations, `#[repr(C)]` structs, and four-char-code
//! constants that this module used to hand-declare now come straight from
//! `coreaudio-sys` (bindgen output over Apple's real `AudioToolbox.h` /
//! `AudioUnit.h`). This module re-exports the subset the rest of the crate
//! uses and adds a handful of thin, hand-rolled aliases/helpers where the
//! generated names would otherwise ripple through every call site:
//!
//! * `SCREAMING_CASE` constant aliases onto the bindgen `kAudioUnit*` /
//!   `kAudioFormat*` names, so scope/property/error/type/param constants read
//!   the same at every use site. Each alias is `= coreaudio_sys::k…`, so the
//!   value is provably byte-identical to Apple's.
//! * `NO_ERR` — the success sentinel (`coreaudio-sys` has no single alias).
//! * `AudioStreamBasicDescription::float32` / `::daw_default` and
//!   `AudioTimeStamp::with_sample_time` — the ASBD flag math and timestamp
//!   validity flag, kept as inherent-method extensions so the exact field
//!   values live in one audited place.
//! * `fourcc_to_string` / `cfstring_to_string` — string helpers.
//!
//! Field access uses the bindgen `m`-prefixed names (`mSampleRate`,
//! `mChannelsPerFrame`, …) directly.

use coreaudio_sys as sys;

// Opaque handles + scalar aliases (verbatim from coreaudio-sys).
pub use sys::{
    AUPreset, AURenderCallback, AURenderCallbackStruct, AudioBuffer, AudioBufferList,
    AudioComponent, AudioComponentDescription, AudioComponentInstance, AudioStreamBasicDescription,
    AudioTimeStamp, AudioUnit, AudioUnitCocoaViewInfo, AudioUnitParameterInfo,
    AudioUnitRenderActionFlags, CFArrayRef, OSStatus,
};

// AudioToolbox functions (verbatim from coreaudio-sys — these are the real
// framework symbols, replacing the crate's former hand-rolled `extern "C"`).
pub use sys::{
    AudioComponentCopyName, AudioComponentCount, AudioComponentFindNext,
    AudioComponentGetDescription, AudioComponentInstanceDispose, AudioComponentInstanceNew,
    AudioUnitGetParameter, AudioUnitGetProperty, AudioUnitGetPropertyInfo, AudioUnitInitialize,
    AudioUnitRender, AudioUnitSetParameter, AudioUnitSetProperty, AudioUnitUninitialize,
    MusicDeviceMIDIEvent,
};

/// Success status value for `OSStatus` returns. `coreaudio-sys` exposes this
/// only as `noErr`; alias it under the name the crate uses.
pub const NO_ERR: OSStatus = sys::noErr as OSStatus;

// AU error codes — aliases onto the bindgen `kAudioUnitErr_*` values so
// `error.rs`'s match arms keep reading in the crate's SCREAMING_CASE style.
pub const K_AUDIO_UNIT_ERR_INVALID_PROPERTY: OSStatus = sys::kAudioUnitErr_InvalidProperty;
pub const K_AUDIO_UNIT_ERR_INVALID_PARAMETER: OSStatus = sys::kAudioUnitErr_InvalidParameter;
pub const K_AUDIO_UNIT_ERR_INVALID_ELEMENT: OSStatus = sys::kAudioUnitErr_InvalidElement;
pub const K_AUDIO_UNIT_ERR_NO_CONNECTION: OSStatus = sys::kAudioUnitErr_NoConnection;
pub const K_AUDIO_UNIT_ERR_FAILED_INITIALIZATION: OSStatus =
    sys::kAudioUnitErr_FailedInitialization;
pub const K_AUDIO_UNIT_ERR_TOO_MANY_FRAMES_TO_PROCESS: OSStatus =
    sys::kAudioUnitErr_TooManyFramesToProcess;
pub const K_AUDIO_UNIT_ERR_INVALID_FILE: OSStatus = sys::kAudioUnitErr_InvalidFile;
pub const K_AUDIO_UNIT_ERR_UNKNOWN_FILE_TYPE: OSStatus = sys::kAudioUnitErr_UnknownFileType;
pub const K_AUDIO_UNIT_ERR_FILE_NOT_SPECIFIED: OSStatus = sys::kAudioUnitErr_FileNotSpecified;
pub const K_AUDIO_UNIT_ERR_FORMAT_NOT_SUPPORTED: OSStatus = sys::kAudioUnitErr_FormatNotSupported;
pub const K_AUDIO_UNIT_ERR_UNINITIALIZED: OSStatus = sys::kAudioUnitErr_Uninitialized;
pub const K_AUDIO_UNIT_ERR_INVALID_SCOPE: OSStatus = sys::kAudioUnitErr_InvalidScope;
pub const K_AUDIO_UNIT_ERR_PROPERTY_NOT_WRITABLE: OSStatus = sys::kAudioUnitErr_PropertyNotWritable;
pub const K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT: OSStatus =
    sys::kAudioUnitErr_CannotDoInCurrentContext;
pub const K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE: OSStatus =
    sys::kAudioUnitErr_InvalidPropertyValue;
pub const K_AUDIO_UNIT_ERR_PROPERTY_NOT_IN_USE: OSStatus = sys::kAudioUnitErr_PropertyNotInUse;
pub const K_AUDIO_UNIT_ERR_INITIALIZED: OSStatus = sys::kAudioUnitErr_Initialized;
pub const K_AUDIO_UNIT_ERR_INVALID_OFFLINE_RENDER: OSStatus =
    sys::kAudioUnitErr_InvalidOfflineRender;
pub const K_AUDIO_UNIT_ERR_UNAUTHORIZED: OSStatus = sys::kAudioUnitErr_Unauthorized;

// AU component types (four-char codes: "auou", "aufx", …).
pub const K_AUDIO_UNIT_TYPE_OUTPUT: u32 = sys::kAudioUnitType_Output;
pub const K_AUDIO_UNIT_TYPE_MUSIC_DEVICE: u32 = sys::kAudioUnitType_MusicDevice;
pub const K_AUDIO_UNIT_TYPE_MUSIC_EFFECT: u32 = sys::kAudioUnitType_MusicEffect;
pub const K_AUDIO_UNIT_TYPE_FORMAT_CONVERTER: u32 = sys::kAudioUnitType_FormatConverter;
pub const K_AUDIO_UNIT_TYPE_EFFECT: u32 = sys::kAudioUnitType_Effect;
pub const K_AUDIO_UNIT_TYPE_MIXER: u32 = sys::kAudioUnitType_Mixer;
pub const K_AUDIO_UNIT_TYPE_PANNER: u32 = sys::kAudioUnitType_Panner;
pub const K_AUDIO_UNIT_TYPE_GENERATOR: u32 = sys::kAudioUnitType_Generator;
pub const K_AUDIO_UNIT_TYPE_OFFLINE_EFFECT: u32 = sys::kAudioUnitType_OfflineEffect;
pub const K_AUDIO_UNIT_TYPE_MIDI_PROCESSOR: u32 = sys::kAudioUnitType_MIDIProcessor;

// Linear PCM format id + format flags.
pub const K_AUDIO_FORMAT_LINEAR_PCM: u32 = sys::kAudioFormatLinearPCM;
pub const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = sys::kAudioFormatFlagIsFloat;
pub const K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN: u32 = sys::kAudioFormatFlagIsBigEndian;
pub const K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = sys::kAudioFormatFlagIsSignedInteger;
pub const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = sys::kAudioFormatFlagIsPacked;
pub const K_AUDIO_FORMAT_FLAG_IS_ALIGNED_HIGH: u32 = sys::kAudioFormatFlagIsAlignedHigh;
pub const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = sys::kAudioFormatFlagIsNonInterleaved;

// Render action flags.
pub const K_AUDIO_UNIT_RENDER_ACTION_PRE_RENDER: AudioUnitRenderActionFlags =
    sys::kAudioUnitRenderAction_PreRender;
pub const K_AUDIO_UNIT_RENDER_ACTION_POST_RENDER: AudioUnitRenderActionFlags =
    sys::kAudioUnitRenderAction_PostRender;
pub const K_AUDIO_UNIT_RENDER_ACTION_OUTPUT_IS_SILENCE: AudioUnitRenderActionFlags =
    sys::kAudioUnitRenderAction_OutputIsSilence;

// AudioTimeStamp validity flags.
pub const K_AUDIO_TIME_STAMP_SAMPLE_TIME_VALID: u32 = sys::kAudioTimeStampSampleTimeValid;
pub const K_AUDIO_TIME_STAMP_HOST_TIME_VALID: u32 = sys::kAudioTimeStampHostTimeValid;

// AU property scopes.
pub const K_AUDIO_UNIT_SCOPE_GLOBAL: u32 = sys::kAudioUnitScope_Global;
pub const K_AUDIO_UNIT_SCOPE_INPUT: u32 = sys::kAudioUnitScope_Input;
pub const K_AUDIO_UNIT_SCOPE_OUTPUT: u32 = sys::kAudioUnitScope_Output;

// AU property IDs.
pub const K_AUDIO_UNIT_PROPERTY_CLASS_INFO: u32 = sys::kAudioUnitProperty_ClassInfo;
pub const K_AUDIO_UNIT_PROPERTY_MAKE_CONNECTION: u32 = sys::kAudioUnitProperty_MakeConnection;
pub const K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE: u32 = sys::kAudioUnitProperty_SampleRate;
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST: u32 = sys::kAudioUnitProperty_ParameterList;
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_INFO: u32 = sys::kAudioUnitProperty_ParameterInfo;
pub const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = sys::kAudioUnitProperty_StreamFormat;
pub const K_AUDIO_UNIT_PROPERTY_ELEMENT_COUNT: u32 = sys::kAudioUnitProperty_ElementCount;
pub const K_AUDIO_UNIT_PROPERTY_LATENCY: u32 = sys::kAudioUnitProperty_Latency;
pub const K_AUDIO_UNIT_PROPERTY_SUPPORTED_NUM_CHANNELS: u32 =
    sys::kAudioUnitProperty_SupportedNumChannels;
pub const K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE: u32 =
    sys::kAudioUnitProperty_MaximumFramesPerSlice;
pub const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 =
    sys::kAudioUnitProperty_SetRenderCallback;
pub const K_AUDIO_UNIT_PROPERTY_FACTORY_PRESETS: u32 = sys::kAudioUnitProperty_FactoryPresets;
pub const K_AUDIO_UNIT_PROPERTY_RENDER_QUALITY: u32 = sys::kAudioUnitProperty_RenderQuality;
pub const K_AUDIO_UNIT_PROPERTY_HOST_CALLBACKS: u32 = sys::kAudioUnitProperty_HostCallbacks;
pub const K_AUDIO_UNIT_PROPERTY_IN_PLACE_PROCESSING: u32 =
    sys::kAudioUnitProperty_InPlaceProcessing;
pub const K_AUDIO_UNIT_PROPERTY_ELEMENT_NAME: u32 = sys::kAudioUnitProperty_ElementName;
pub const K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT: u32 = sys::kAudioUnitProperty_BypassEffect;
pub const K_AUDIO_UNIT_PROPERTY_LAST_RENDER_ERROR: u32 = sys::kAudioUnitProperty_LastRenderError;
pub const K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET: u32 = sys::kAudioUnitProperty_PresentPreset;
pub const K_AUDIO_UNIT_PROPERTY_COCOA_UI: u32 = sys::kAudioUnitProperty_CocoaUI;

// Parameter unit kinds.
pub const K_AUDIO_UNIT_PARAMETER_UNIT_GENERIC: u32 = sys::kAudioUnitParameterUnit_Generic;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_BOOLEAN: u32 = sys::kAudioUnitParameterUnit_Boolean;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_PERCENT: u32 = sys::kAudioUnitParameterUnit_Percent;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_SECONDS: u32 = sys::kAudioUnitParameterUnit_Seconds;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_HERTZ: u32 = sys::kAudioUnitParameterUnit_Hertz;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_DECIBELS: u32 = sys::kAudioUnitParameterUnit_Decibels;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_LINEAR_GAIN: u32 = sys::kAudioUnitParameterUnit_LinearGain;

// Parameter flag bits.
pub const K_AUDIO_UNIT_PARAMETER_FLAG_IS_READABLE: u32 = sys::kAudioUnitParameterFlag_IsReadable;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_IS_WRITABLE: u32 = sys::kAudioUnitParameterFlag_IsWritable;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_HAS_NAME: u32 = sys::kAudioUnitParameterFlag_HasName;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_HAS_CF_NAME_STRING: u32 =
    sys::kAudioUnitParameterFlag_HasCFNameString;

/// ASBD flag set for canonical non-interleaved packed float32 linear PCM.
const FLOAT32_FORMAT_FLAGS: u32 = K_AUDIO_FORMAT_FLAG_IS_FLOAT
    | K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED
    | K_AUDIO_FORMAT_FLAG_IS_PACKED;

/// Extension helpers on the bindgen `AudioStreamBasicDescription`.
///
/// These own the exact field math for the crate's canonical audio format so it
/// lives in one audited place rather than being open-coded at each call site.
pub trait AsbdExt {
    /// Build an ASBD for 32-bit float, non-interleaved, packed linear PCM.
    ///
    /// This is the format the rest of the crate uses for AU I/O. For
    /// non-interleaved audio `mBytesPerFrame` / `mBytesPerPacket` are `4`
    /// (one channel's worth), *not* `channels * 4`.
    fn float32(sample_rate: f64, channels: u32) -> Self;

    /// The crate's historical `Default`: 44.1 kHz stereo float32.
    fn daw_default() -> Self;
}

impl AsbdExt for AudioStreamBasicDescription {
    fn float32(sample_rate: f64, channels: u32) -> Self {
        Self {
            mSampleRate: sample_rate,
            mFormatID: K_AUDIO_FORMAT_LINEAR_PCM,
            mFormatFlags: FLOAT32_FORMAT_FLAGS,
            mBytesPerPacket: 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        }
    }

    fn daw_default() -> Self {
        Self::float32(44100.0, 2)
    }
}

/// Extension helpers on the bindgen `AudioTimeStamp`.
pub trait AudioTimeStampExt {
    /// Build a timestamp with only `mSampleTime` marked valid.
    fn with_sample_time(sample_time: f64) -> Self;
}

impl AudioTimeStampExt for AudioTimeStamp {
    fn with_sample_time(sample_time: f64) -> Self {
        // Only `mSampleTime` + the sample-time validity flag matter to the
        // host; every other field (incl. the `mSMPTETime` sub-struct) stays
        // zeroed via `Default`.
        Self {
            mSampleTime: sample_time,
            mFlags: K_AUDIO_TIME_STAMP_SAMPLE_TIME_VALID,
            ..Default::default()
        }
    }
}

/// Convert a big-endian four-character code (e.g. `b"aufx"`) to its string form.
///
/// Invalid UTF-8 bytes are replaced with the Unicode replacement character.
pub fn fourcc_to_string(code: u32) -> String {
    let bytes = code.to_be_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

/// Copy a `CFStringRef` into an owned Rust `String`.
///
/// Returns an empty string when `cf_str` is null.
///
/// # Safety
/// `cf_str` must be either null or a valid `CFStringRef` with +1 retain count
/// suitable for `wrap_under_get_rule` (CoreFoundation's "Get" ownership model).
#[cfg(target_os = "macos")]
pub unsafe fn cfstring_to_string(cf_str: sys::CFStringRef) -> String {
    if cf_str.is_null() {
        return String::new();
    }
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    // `coreaudio-sys` and `core-foundation-sys` each declare their own opaque
    // `__CFString`; the pointers are ABI-identical, so cast across.
    let cf_str = cf_str as core_foundation_sys::string::CFStringRef;
    let s: CFString = TCFType::wrap_under_get_rule(cf_str);
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fourcc_to_string() {
        assert_eq!(fourcc_to_string(K_AUDIO_UNIT_TYPE_EFFECT), "aufx");
        assert_eq!(fourcc_to_string(K_AUDIO_UNIT_TYPE_MUSIC_DEVICE), "aumu");
        assert_eq!(fourcc_to_string(K_AUDIO_UNIT_TYPE_GENERATOR), "augn");
    }

    #[test]
    fn test_asbd_default() {
        let asbd = AudioStreamBasicDescription::daw_default();
        assert_eq!(asbd.mSampleRate, 44100.0);
        assert_eq!(asbd.mChannelsPerFrame, 2);
        assert_eq!(asbd.mBitsPerChannel, 32);
    }

    /// The canonical non-interleaved float32 ASBD field math. This is the
    /// safety-critical invariant: `coreaudio-sys`'s struct must produce the
    /// exact same bytes the hand-rolled struct did. Byte-for-byte assertions
    /// on every field, including the non-interleaved `mBytesPerFrame == 4`
    /// (NOT channels*4) subtlety.
    #[test]
    fn test_asbd_float32_exact_fields() {
        let asbd = AudioStreamBasicDescription::float32(48000.0, 1);
        assert_eq!(asbd.mSampleRate, 48000.0);
        assert_eq!(asbd.mFormatID, K_AUDIO_FORMAT_LINEAR_PCM);
        assert_eq!(
            asbd.mFormatFlags,
            K_AUDIO_FORMAT_FLAG_IS_FLOAT
                | K_AUDIO_FORMAT_FLAG_IS_PACKED
                | K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED
        );
        assert_eq!(asbd.mBytesPerPacket, 4);
        assert_eq!(asbd.mFramesPerPacket, 1);
        // Non-interleaved: one channel's worth per frame, NOT channels * 4.
        assert_eq!(asbd.mBytesPerFrame, 4);
        assert_eq!(asbd.mChannelsPerFrame, 1);
        assert_eq!(asbd.mBitsPerChannel, 32);

        // And the same for a stereo layout — mBytesPerFrame stays 4.
        let stereo = AudioStreamBasicDescription::float32(44100.0, 2);
        assert_eq!(stereo.mChannelsPerFrame, 2);
        assert_eq!(stereo.mBytesPerFrame, 4);
        assert_eq!(stereo.mBytesPerPacket, 4);
    }

    /// The `IsFloat` / `IsNonInterleaved` flag bits must be set (the checks the
    /// original `test_asbd_float32` made), now against the bindgen field.
    #[test]
    fn test_asbd_float32_flags_set() {
        let asbd = AudioStreamBasicDescription::float32(48000.0, 1);
        assert_ne!(asbd.mFormatFlags & K_AUDIO_FORMAT_FLAG_IS_FLOAT, 0);
        assert_ne!(
            asbd.mFormatFlags & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED,
            0
        );
    }

    #[test]
    fn test_timestamp_default() {
        let ts = AudioTimeStamp::with_sample_time(0.0);
        assert_eq!(ts.mSampleTime, 0.0);
        assert_ne!(ts.mFlags & K_AUDIO_TIME_STAMP_SAMPLE_TIME_VALID, 0);
    }

    #[test]
    fn test_timestamp_with_sample_time() {
        let ts = AudioTimeStamp::with_sample_time(1024.0);
        assert_eq!(ts.mSampleTime, 1024.0);
    }

    #[test]
    fn test_audio_component_description_default() {
        let desc = AudioComponentDescription::default();
        assert_eq!(desc.componentType, 0);
        assert_eq!(desc.componentSubType, 0);
        assert_eq!(desc.componentManufacturer, 0);
    }
}
