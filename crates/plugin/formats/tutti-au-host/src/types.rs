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
    AudioChannelDescription, AudioChannelLayout, AudioComponent, AudioComponentDescription,
    AudioComponentInstance, AudioStreamBasicDescription, AudioTimeStamp, AudioUnit,
    AudioUnitCocoaViewInfo, AudioUnitParameterInfo, AudioUnitParameterStringFromValue,
    AudioUnitParameterValueFromString, AudioUnitRenderActionFlags, CFArrayRef, CFStringRef,
    OSStatus,
};

/// CoreMIDI packet types, re-exported for the MIDI-output-callback path.
///
/// These come from the same `coreaudio-sys` bindgen pass as everything else in
/// this module — its `core_midi` feature is on by default, so `CoreMIDI.h` is in
/// the header set and the packet layout is Apple's own rather than hand-declared.
/// `crate::midi_out` walks a `MIDIPacketList` the AU hands it on the render
/// thread; see that module for why the walk is by offset rather than by struct
/// read.
pub use sys::{MIDIPacket, MIDIPacketList};

// AudioToolbox functions (verbatim from coreaudio-sys — these are the real
// framework symbols, replacing the crate's former hand-rolled `extern "C"`).
pub use sys::{
    AudioComponentCopyName, AudioComponentCount, AudioComponentFindNext,
    AudioComponentGetDescription, AudioComponentInstanceDispose, AudioComponentInstanceNew,
    AudioUnitGetParameter, AudioUnitGetProperty, AudioUnitGetPropertyInfo, AudioUnitInitialize,
    AudioUnitRender, AudioUnitReset, AudioUnitSetParameter, AudioUnitSetProperty,
    AudioUnitUninitialize, MusicDeviceMIDIEvent,
};

// The *push* render entry points, used only by `crate::offline`.
//
// Separate from the block above because they are a different render contract,
// not merely two more calls: `AudioUnitRender` pulls its input through a
// callback the host installed, while these two are handed the input directly.
// Grouping them with `AudioUnitRender` would suggest they are interchangeable at
// a call site, and they are not — see `offline::process_push`.
pub use sys::{AudioUnitProcess, AudioUnitProcessMultiple};

/// `unimpErr` — "unimplemented core routine", the classic Mac OS status the
/// component manager returns when an AU does not implement a dispatch selector.
///
/// Not in `coreaudio-sys` (it is a CarbonCore value, not an AudioToolbox one) and
/// not in the SDK headers this crate can reach, so it is spelled out. It is
/// aliased at all because it is the *dominant* answer to
/// `AudioUnitProcess` / `AudioUnitProcessMultiple` on this system, and a test
/// asserting "this unit does not implement the push path" has to name the status
/// rather than assert a bare `is_err()` — an `is_err()` would pass just as well
/// if the render failed for an unrelated reason, which is exactly the vacuous
/// assertion that hides a regression. Measured on macOS 15.6: 6 of 9 corpus
/// effects answer `noErr` to `AudioUnitProcess`, the rest answer this.
pub const UNIMP_ERR: OSStatus = -4;

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
/// The document-restore twin of `ClassInfo` (property 50).
///
/// Apple's header says an AU implementing this "is going to do different actions
/// establishing its state from a document rather than from a user preset", and
/// that a host restoring a *document* must try this property **first**, falling
/// back to `ClassInfo` when the AU errors or does not implement it.
///
/// The distinction is real for units that key licensing, sample-library paths or
/// per-document resource references off which of the two was used: a `.aupreset`
/// is a user preset and must go through `ClassInfo`, while a project reload is a
/// document and should offer this first.
///
/// Measured on macOS 15.6: **no** unit on this machine implements it — AUDelay,
/// AUDistortion, AUMatrixReverb, AUSpatialMixer and AULowpass all answer
/// `kAudioUnitErr_InvalidProperty` (-10879). That is exactly the case the header
/// tells hosts to expect, which is why
/// [`AuInstance::load_document_state`](crate::instance::AuInstance::load_document_state)
/// treats the refusal as routine and falls back rather than surfacing it.
pub const K_AUDIO_UNIT_PROPERTY_CLASS_INFO_FROM_DOCUMENT: u32 = 50;
pub const K_AUDIO_UNIT_PROPERTY_MAKE_CONNECTION: u32 = sys::kAudioUnitProperty_MakeConnection;
pub const K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE: u32 = sys::kAudioUnitProperty_SampleRate;
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST: u32 = sys::kAudioUnitProperty_ParameterList;
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_INFO: u32 = sys::kAudioUnitProperty_ParameterInfo;
pub const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = sys::kAudioUnitProperty_StreamFormat;
pub const K_AUDIO_UNIT_PROPERTY_ELEMENT_COUNT: u32 = sys::kAudioUnitProperty_ElementCount;
pub const K_AUDIO_UNIT_PROPERTY_LATENCY: u32 = sys::kAudioUnitProperty_Latency;
/// Seconds of audio an AU keeps producing after its input goes silent — a
/// reverb's decay, a delay's repeats. Distinct from `Latency`: latency shifts
/// audio in time, tail extends how long it lasts. An offline bounce that stops
/// at the last note truncates every tail on the master bus.
pub const K_AUDIO_UNIT_PROPERTY_TAIL_TIME: u32 = sys::kAudioUnitProperty_TailTime;
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
/// The channel *order* a bus is running — which speaker each channel feeds.
/// Distinct from `StreamFormat`, which carries only a channel *count*: Apple's
/// header says outright that the stream format "cannot specify channel layout or
/// purpose". Without this, a 6-channel bus's centre and LFE are
/// indistinguishable. See [`crate::channel_layout`].
pub const K_AUDIO_UNIT_PROPERTY_AUDIO_CHANNEL_LAYOUT: u32 =
    sys::kAudioUnitProperty_AudioChannelLayout;
/// The channel orders a bus declares it understands. Read-only, and per-bus.
pub const K_AUDIO_UNIT_PROPERTY_SUPPORTED_CHANNEL_LAYOUT_TAGS: u32 =
    sys::kAudioUnitProperty_SupportedChannelLayoutTags;
/// How many MIDI output streams the AU offers, and their names — a `CFArrayRef`
/// of `CFStringRef` the host owns. Its presence is the gate on
/// `MIDIOutputCallback` being useful: see [`crate::midi_out`], and note that on
/// this machine **no** installed AU publishes it.
pub const K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK_INFO: u32 =
    sys::kAudioUnitProperty_MIDIOutputCallbackInfo;
/// The `AUMIDIOutputCallbackStruct` a host installs so the AU can hand it MIDI
/// during render. Write-only.
pub const K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK: u32 =
    sys::kAudioUnitProperty_MIDIOutputCallback;
// The parameter↔MIDI mapping family (41-44). Hand-declared for the same reason
// `K_AUDIO_UNIT_PROPERTY_CLASS_INFO_FROM_DOCUMENT` is: `coreaudio-sys` exports
// none of the four, nor the six `kAUParameterMIDIMapping_*` flag bits below, nor
// `AUParameterMIDIMapping` itself. Every value here was read out of a C program
// compiled against Apple's real `AudioUnitProperties.h` on macOS 15.6, and
// `midi_map::tests::the_flag_bits_match_the_header` asserts each one — a
// transposed pair changes what an incoming controller value means with no
// compile error. See [`crate::midi_map`] for which units implement them
// (measured: 2 of 59) and for the four ways both implementers deviate from the
// header.
/// Read/write the AU's whole parameter↔MIDI mapping table.
pub const K_AUDIO_UNIT_PROPERTY_ALL_PARAMETER_MIDI_MAPPINGS: u32 = 41;
/// Write-only: merge mappings into the AU's existing set.
pub const K_AUDIO_UNIT_PROPERTY_ADD_PARAMETER_MIDI_MAPPING: u32 = 42;
/// Write-only: drop mappings, matched on `(scope, element, parameterID)`.
pub const K_AUDIO_UNIT_PROPERTY_REMOVE_PARAMETER_MIDI_MAPPING: u32 = 43;
/// Read/write: "learn" mode — the AU maps the next MIDI message it sees.
pub const K_AUDIO_UNIT_PROPERTY_HOT_MAP_PARAMETER_MIDI_MAPPING: u32 = 44;

// `AUParameterMIDIMappingFlags`, `1 << 0` .. `1 << 5`.
/// Match the message on any MIDI channel, ignoring the status byte's low nibble.
pub const K_AU_PARAMETER_MIDI_MAPPING_ANY_CHANNEL: u32 = 1 << 0;
/// For a note command, match any note number.
pub const K_AU_PARAMETER_MIDI_MAPPING_ANY_NOTE: u32 = 1 << 1;
/// Confine the controller to `mSubRangeMin..=mSubRangeMax` of the parameter.
pub const K_AU_PARAMETER_MIDI_MAPPING_SUB_RANGE: u32 = 1 << 2;
/// The message flips the parameter rather than setting it from the value.
pub const K_AU_PARAMETER_MIDI_MAPPING_TOGGLE: u32 = 1 << 3;
/// The parameter takes only two states, thresholded at controller 64/65.
pub const K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR: u32 = 1 << 4;
/// Which end of the controller is the parameter's "on" state. Only meaningful
/// with [`K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR`] set.
pub const K_AU_PARAMETER_MIDI_MAPPING_BIPOLAR_ON: u32 = 1 << 5;

/// The **deprecated** read-only CC→parameter form (`= 17`), superseded in macOS
/// 10.2 by the 41-44 family above.
///
/// Aliased but deliberately **not** implemented, for the reason
/// [`crate::midi_map`]'s docs give at length: measured on macOS 15.6, **0 of 59**
/// installed components answer it, at any of the three scopes, in either
/// lifecycle phase. The constant exists so `tests/au_midi_map.rs` can sweep for
/// an implementer and report one loudly if it ever appears — a decoder written
/// now would be untestable against reality.
pub const K_AUDIO_UNIT_PROPERTY_MIDI_CONTROL_MAPPING: u32 = 17;

pub const K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT: u32 = sys::kAudioUnitProperty_BypassEffect;
pub const K_AUDIO_UNIT_PROPERTY_LAST_RENDER_ERROR: u32 = sys::kAudioUnitProperty_LastRenderError;
pub const K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET: u32 = sys::kAudioUnitProperty_PresentPreset;
pub const K_AUDIO_UNIT_PROPERTY_COCOA_UI: u32 = sys::kAudioUnitProperty_CocoaUI;
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_STRINGS: u32 =
    sys::kAudioUnitProperty_ParameterValueStrings;
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_STRING_FROM_VALUE: u32 =
    sys::kAudioUnitProperty_ParameterStringFromValue;
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_FROM_STRING: u32 =
    sys::kAudioUnitProperty_ParameterValueFromString;
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_CLUMP_NAME: u32 =
    sys::kAudioUnitProperty_ParameterClumpName;

// Parameter unit kinds.
pub const K_AUDIO_UNIT_PARAMETER_UNIT_GENERIC: u32 = sys::kAudioUnitParameterUnit_Generic;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_BOOLEAN: u32 = sys::kAudioUnitParameterUnit_Boolean;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_PERCENT: u32 = sys::kAudioUnitParameterUnit_Percent;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_SECONDS: u32 = sys::kAudioUnitParameterUnit_Seconds;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_HERTZ: u32 = sys::kAudioUnitParameterUnit_Hertz;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_DECIBELS: u32 = sys::kAudioUnitParameterUnit_Decibels;
pub const K_AUDIO_UNIT_PARAMETER_UNIT_LINEAR_GAIN: u32 = sys::kAudioUnitParameterUnit_LinearGain;
/// A parameter whose value **is** a MIDI controller number, `0..=127` — not a
/// quantity in a physical unit. Decoded as `Unknown(12)` before
/// [`crate::midi_map`] existed, which is why it is aliased here: a
/// mapping-aware host formats it as "CC 74", never as a bare `74`.
pub const K_AUDIO_UNIT_PARAMETER_UNIT_MIDI_CONTROLLER: u32 =
    sys::kAudioUnitParameterUnit_MIDIController;

// Parameter flag bits.
pub const K_AUDIO_UNIT_PARAMETER_FLAG_IS_READABLE: u32 = sys::kAudioUnitParameterFlag_IsReadable;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_IS_WRITABLE: u32 = sys::kAudioUnitParameterFlag_IsWritable;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_HAS_NAME: u32 = sys::kAudioUnitParameterFlag_HasName;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_HAS_CF_NAME_STRING: u32 =
    sys::kAudioUnitParameterFlag_HasCFNameString;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_HAS_CLUMP: u32 = sys::kAudioUnitParameterFlag_HasClump;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_VALUES_HAVE_STRINGS: u32 =
    sys::kAudioUnitParameterFlag_ValuesHaveStrings;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_METER_READ_ONLY: u32 =
    sys::kAudioUnitParameterFlag_MeterReadOnly;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_IS_HIGH_RESOLUTION: u32 =
    sys::kAudioUnitParameterFlag_IsHighResolution;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_NON_REAL_TIME: u32 = sys::kAudioUnitParameterFlag_NonRealTime;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_CAN_RAMP: u32 = sys::kAudioUnitParameterFlag_CanRamp;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_OMIT_FROM_PRESETS: u32 =
    sys::kAudioUnitParameterFlag_OmitFromPresets;

// Display-curve flags. `DISPLAY_MASK` covers a *non-contiguous* field: bits
// 16..=18 hold the curve index and bit 22 is the separate Logarithmic flag
// (`(7<<16) | (1<<22)`). Masking with only `7<<16` silently drops every
// logarithmic parameter — 24 of the 39 curve-carrying parameters measured on
// macOS 15.6 — so the mask must come from Apple's own constant, not a
// hand-written one.
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_MASK: u32 = sys::kAudioUnitParameterFlag_DisplayMask;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_SQUARE_ROOT: u32 =
    sys::kAudioUnitParameterFlag_DisplaySquareRoot;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_SQUARED: u32 =
    sys::kAudioUnitParameterFlag_DisplaySquared;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_CUBED: u32 =
    sys::kAudioUnitParameterFlag_DisplayCubed;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_CUBE_ROOT: u32 =
    sys::kAudioUnitParameterFlag_DisplayCubeRoot;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_EXPONENTIAL: u32 =
    sys::kAudioUnitParameterFlag_DisplayExponential;
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_LOGARITHMIC: u32 =
    sys::kAudioUnitParameterFlag_DisplayLogarithmic;

// `AudioChannelLayoutTag` values, aliased for `crate::channel_layout`.
//
// Several of these constants are the **same numeric value under two names** —
// `AudioUnit_4 == Quadraphonic`, `AudioUnit_5_1 == MPEG_5_1_A`,
// `AudioUnit_7_1 == MPEG_7_1_C`, `AudioUnit_5_0 == MPEG_5_0_B`,
// `AudioUnit_6 == Hexagonal`, `AudioUnit_8 == Octagonal`. Both spellings of each
// pair are aliased here on purpose: `channel_layout::tests` asserts the equality
// so a future SDK that splits a pair fails a test, and that assertion needs both
// names in scope. A `match` may still list only one of each pair — see
// `AuLayoutTag::from_raw`.
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_USE_CHANNEL_DESCRIPTIONS: u32 =
    sys::kAudioChannelLayoutTag_UseChannelDescriptions;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_USE_CHANNEL_BITMAP: u32 =
    sys::kAudioChannelLayoutTag_UseChannelBitmap;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MONO: u32 = sys::kAudioChannelLayoutTag_Mono;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_STEREO: u32 = sys::kAudioChannelLayoutTag_Stereo;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_STEREO_HEADPHONES: u32 =
    sys::kAudioChannelLayoutTag_StereoHeadphones;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MATRIX_STEREO: u32 = sys::kAudioChannelLayoutTag_MatrixStereo;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MID_SIDE: u32 = sys::kAudioChannelLayoutTag_MidSide;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_XY: u32 = sys::kAudioChannelLayoutTag_XY;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_BINAURAL: u32 = sys::kAudioChannelLayoutTag_Binaural;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AMBISONIC_B_FORMAT: u32 =
    sys::kAudioChannelLayoutTag_Ambisonic_B_Format;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_QUADRAPHONIC: u32 = sys::kAudioChannelLayoutTag_Quadraphonic;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_PENTAGONAL: u32 = sys::kAudioChannelLayoutTag_Pentagonal;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_HEXAGONAL: u32 = sys::kAudioChannelLayoutTag_Hexagonal;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_OCTAGONAL: u32 = sys::kAudioChannelLayoutTag_Octagonal;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_CUBE: u32 = sys::kAudioChannelLayoutTag_Cube;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_0: u32 =
    sys::kAudioChannelLayoutTag_AudioUnit_5_0;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_1: u32 =
    sys::kAudioChannelLayoutTag_AudioUnit_5_1;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_6_0: u32 =
    sys::kAudioChannelLayoutTag_AudioUnit_6_0;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_7_0: u32 =
    sys::kAudioChannelLayoutTag_AudioUnit_7_0;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_7_1: u32 =
    sys::kAudioChannelLayoutTag_AudioUnit_7_1;
// The alias spellings, present only so the aliasing itself is testable.
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_4: u32 = sys::kAudioChannelLayoutTag_AudioUnit_4;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_6: u32 = sys::kAudioChannelLayoutTag_AudioUnit_6;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_8: u32 = sys::kAudioChannelLayoutTag_AudioUnit_8;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MPEG_5_0_B: u32 = sys::kAudioChannelLayoutTag_MPEG_5_0_B;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MPEG_5_1_A: u32 = sys::kAudioChannelLayoutTag_MPEG_5_1_A;
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MPEG_7_1_C: u32 = sys::kAudioChannelLayoutTag_MPEG_7_1_C;

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

/// Copy a `CFStringRef` into an owned `String`, but only after confirming it
/// really is a `CFString`.
///
/// Returns `None` for null, for a misaligned pointer, and for a live CF object
/// of some other type.
///
/// ## Why this exists
///
/// [`cfstring_to_string`] guards only against null, which is sufficient for the
/// strings *this crate* creates but not for a pointer an **AU** supplies. A unit
/// whose preset table is corrupt or stale — or simply not made of `AUPreset`
/// structs — hands back a non-null value that is not a CFString, and passing that
/// to CoreFoundation aborts the process with SIGBUS. That was measured, not
/// hypothesised: a probe AU returning a `CFArray` of `CFData` made
/// `factory_presets` read CF header internals as `presetName` and killed the test
/// binary. See `tests/au_misbehaving.rs`.
///
/// `CFGetTypeID` is the documented way to ask "what is this really", but it is
/// **not safe to call on an arbitrary word** — it dereferences, and it aborts the
/// process on a value that is not a CF reference. So the plausibility gate below
/// runs first, and `CFGetTypeID` decides only among values that could be one.
///
/// ## The plausibility gate, and why plain alignment is the wrong test
///
/// Both halves were measured on macOS 15.6 / arm64:
///
/// | value | bit 63 | 8-aligned |
/// |---|---|---|
/// | `CFString::new("Clean")` (tagged) | 1 | no (`0x99339a0c57ed9bbe`) |
/// | long `CFString` (real object)     | 0 | yes (`0x10559e480`) |
/// | `CFData` (real object)            | 0 | yes (`0x10559e4d0`) |
/// | garbage from a `CFData` header    | 0 | no (`0x2bc139001484`) |
///
/// CoreFoundation returns short strings as **tagged pointers** carrying the
/// payload in the pointer word, so they are routinely misaligned — a bare
/// alignment reject would discard exactly the names real AUs ship ("Clean",
/// "Bright"), which is a bug this guard already had once. But `CFGetTypeID`
/// *faults* on the garbage row, so the gate cannot simply be dropped either.
///
/// Bit 63 separates the two: it is set on every tagged reference and clear on
/// both real objects and garbage. So a value is plausible when it is tagged
/// (bit 63 set) **or** properly aligned, and only garbage — misaligned and
/// untagged — is turned away before the dereference.
///
/// This is a heuristic, not a proof: a hostile value with bit 63 set would still
/// reach `CFGetTypeID`. Nothing can fully validate a pointer a plugin asserts is
/// valid; this narrows a guaranteed crash to an unlikely one while keeping every
/// legitimate name.
///
/// # Safety
/// `cf_str` must be null, or a pointer to a live CoreFoundation object (a real
/// address or a tagged-pointer reference). It need not be a `CFString`: that is
/// what this function checks.
#[cfg(target_os = "macos")]
pub unsafe fn cfstring_to_string_checked(cf_str: sys::CFStringRef) -> Option<String> {
    if cf_str.is_null() {
        return None;
    }
    let bits = cf_str as usize;
    // See the table above: tagged references carry the payload in the pointer
    // word and are legitimately misaligned, so they are admitted on the tag bit;
    // everything else must be a properly-aligned address to be dereferenceable.
    let tagged = (bits >> 63) & 1 == 1;
    let aligned = bits.is_multiple_of(std::mem::align_of::<*const std::os::raw::c_void>());
    if !tagged && !aligned {
        return None;
    }
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    let cf_str = cf_str as core_foundation_sys::string::CFStringRef;
    if core_foundation_sys::base::CFGetTypeID(cf_str as *const std::os::raw::c_void)
        != core_foundation_sys::string::CFStringGetTypeID()
    {
        return None;
    }
    let s: CFString = TCFType::wrap_under_get_rule(cf_str);
    Some(s.to_string())
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

    /// The checked converter must accept a real CFString and reject everything
    /// else, because it is the guard standing between a plugin-supplied pointer
    /// and a CoreFoundation dereference.
    ///
    /// A non-CFString *live CF object* is the case that matters: an AU with a
    /// corrupt preset table hands back a valid pointer to the wrong type, and the
    /// unchecked converter aborted the process on it (SIGBUS). Covered here as a
    /// unit test as well as in `tests/au_misbehaving.rs` so the guard is pinned
    /// even without an AU present.
    #[cfg(target_os = "macos")]
    #[test]
    fn cfstring_to_string_checked_rejects_non_strings() {
        use core_foundation::base::TCFType;
        use core_foundation::data::CFData;
        use core_foundation::string::CFString;

        // Genuine CFStrings round-trip. BOTH lengths are checked on purpose: on
        // arm64 a short string comes back as a *tagged pointer* (misaligned, the
        // payload inside the pointer word) while a long one is a real address.
        // An earlier version of the guard rejected anything not pointer-aligned
        // and so silently dropped every short preset name — exactly the names
        // real AUs use ("Clean", "Bright").
        for name in ["a", "Clean", "preset name", &"long name ".repeat(6)] {
            let s = CFString::new(name);
            let got =
                unsafe { cfstring_to_string_checked(s.as_concrete_TypeRef() as sys::CFStringRef) };
            assert_eq!(
                got.as_deref(),
                Some(name),
                "a real CFString of length {} must round-trip; if this fails for \
                 the short cases the guard is rejecting tagged pointers",
                name.len()
            );
        }

        // Null is rejected rather than turned into an empty string, so a caller
        // can tell "no name" from "the AU gave us nothing".
        assert!(unsafe { cfstring_to_string_checked(std::ptr::null()) }.is_none());

        // A live CF object of the wrong type: valid, retained memory that is not
        // a CFString. This is the shape that crashed the host with SIGBUS.
        let data = CFData::from_buffer(&[0u8; 64]);
        let mispointed = data.as_concrete_TypeRef() as *const std::os::raw::c_void;
        assert!(
            unsafe { cfstring_to_string_checked(mispointed as sys::CFStringRef) }.is_none(),
            "a CFData must not be accepted as a CFString"
        );

        // Misaligned AND untagged: the shape recovered from a CFData header when
        // it is misread as an `AUPreset`. `CFGetTypeID` faults on this (measured),
        // so the plausibility gate must reject it before the dereference. Bit 63
        // is clear here, which is what separates it from a tagged CFString.
        let garbage = 0x2bc1_3900_1484usize as sys::CFStringRef;
        assert!(
            unsafe { cfstring_to_string_checked(garbage) }.is_none(),
            "misaligned untagged garbage must be rejected without dereferencing"
        );
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
