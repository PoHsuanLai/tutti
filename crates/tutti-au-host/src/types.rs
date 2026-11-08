//! AudioToolbox types, constants, and FFI — sourced from `coreaudio-sys`.
//!
//! The raw `extern "C"` declarations, `#[repr(C)]` structs and four-char-code
//! constants come from `coreaudio-sys` — bindgen output over Apple's real
//! `AudioToolbox.h` / `AudioUnit.h`, so nothing here is hand-transcribed unless
//! it says so. This module re-exports the subset the rest of the crate uses and
//! adds a handful of thin aliases/helpers where the generated names would
//! otherwise ripple through every call site:
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
//! * `fourcc_to_string` — the four-char-code string helper.
//!
//! Field access uses the bindgen `m`-prefixed names (`mSampleRate`,
//! `mChannelsPerFrame`, …) directly.

use coreaudio_sys as sys;

// Opaque handles + scalar aliases (verbatim from coreaudio-sys).
pub use sys::{
    AUDependentParameter, AUPreset, AURenderCallback, AURenderCallbackStruct, AudioBuffer,
    AudioBufferList, AudioChannelDescription, AudioChannelLayout, AudioComponent,
    AudioComponentDescription, AudioComponentInstance, AudioStreamBasicDescription, AudioTimeStamp,
    AudioUnit, AudioUnitCocoaViewInfo, AudioUnitParameter, AudioUnitParameterInfo,
    AudioUnitParameterStringFromValue, AudioUnitParameterValueFromString,
    AudioUnitRenderActionFlags, CFArrayRef, CFStringRef, OSStatus,
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
    AudioUnitUninitialize, MusicDeviceMIDIEvent, MusicDeviceSysEx,
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
/// `kAudioComponentFlag_RequiresAsyncInstantiation` — the component can only be
/// created through `AudioComponentInstantiate`.
///
/// `AudioComponent.h:200-201`: the system sets this automatically "when
/// registering components which require asynchronous instantiation ... (v3
/// audio units with views)", and `:498-502` says `AudioComponentInstantiate`
/// "must be used" for them.
pub const K_AUDIO_COMPONENT_FLAG_REQUIRES_ASYNC_INSTANTIATION: u32 =
    sys::kAudioComponentFlag_RequiresAsyncInstantiation;

/// `kAudioComponentFlag_IsV3AudioUnit` — set by the system when registering a
/// version 3 Audio Unit.
///
/// Distinct from the async flag: a v3 unit *without* a view is v3 and may still
/// be created synchronously, so this reports what the component is while the
/// async flag reports how it must be built.
pub const K_AUDIO_COMPONENT_FLAG_IS_V3_AUDIO_UNIT: u32 = sys::kAudioComponentFlag_IsV3AudioUnit;

/// `kAudioUnitErr_InvalidProperty`: the unit does not implement the property asked for.
pub const K_AUDIO_UNIT_ERR_INVALID_PROPERTY: OSStatus = sys::kAudioUnitErr_InvalidProperty;
/// `kAudioUnitErr_InvalidParameter`: the unit has no such parameter.
pub const K_AUDIO_UNIT_ERR_INVALID_PARAMETER: OSStatus = sys::kAudioUnitErr_InvalidParameter;
/// `kAudioUnitErr_InvalidElement`: the element (bus) index is out of range for the scope.
pub const K_AUDIO_UNIT_ERR_INVALID_ELEMENT: OSStatus = sys::kAudioUnitErr_InvalidElement;
/// `kAudioUnitErr_NoConnection`: the input bus has neither a connection nor a render callback.
pub const K_AUDIO_UNIT_ERR_NO_CONNECTION: OSStatus = sys::kAudioUnitErr_NoConnection;
/// `kAudioUnitErr_FailedInitialization`: the unit could not initialize with its current configuration.
pub const K_AUDIO_UNIT_ERR_FAILED_INITIALIZATION: OSStatus =
    sys::kAudioUnitErr_FailedInitialization;
/// `kAudioUnitErr_TooManyFramesToProcess`: a render asked for more frames than the maximum frames per slice.
pub const K_AUDIO_UNIT_ERR_TOO_MANY_FRAMES_TO_PROCESS: OSStatus =
    sys::kAudioUnitErr_TooManyFramesToProcess;
/// `kAudioUnitErr_InvalidFile`: the file is not a valid one for the unit.
pub const K_AUDIO_UNIT_ERR_INVALID_FILE: OSStatus = sys::kAudioUnitErr_InvalidFile;
/// `kAudioUnitErr_UnknownFileType`: the unit does not recognize the file type.
pub const K_AUDIO_UNIT_ERR_UNKNOWN_FILE_TYPE: OSStatus = sys::kAudioUnitErr_UnknownFileType;
/// `kAudioUnitErr_FileNotSpecified`: the unit needs a file and none was set.
pub const K_AUDIO_UNIT_ERR_FILE_NOT_SPECIFIED: OSStatus = sys::kAudioUnitErr_FileNotSpecified;
/// `kAudioUnitErr_FormatNotSupported`: the stream format is not one the unit supports.
pub const K_AUDIO_UNIT_ERR_FORMAT_NOT_SUPPORTED: OSStatus = sys::kAudioUnitErr_FormatNotSupported;
/// `kAudioUnitErr_Uninitialized`: the call needs an initialized unit and this one is not.
pub const K_AUDIO_UNIT_ERR_UNINITIALIZED: OSStatus = sys::kAudioUnitErr_Uninitialized;
/// `kAudioUnitErr_InvalidScope`: the scope is not valid for the property or parameter.
pub const K_AUDIO_UNIT_ERR_INVALID_SCOPE: OSStatus = sys::kAudioUnitErr_InvalidScope;
/// `kAudioUnitErr_PropertyNotWritable`: the property is read-only.
pub const K_AUDIO_UNIT_ERR_PROPERTY_NOT_WRITABLE: OSStatus = sys::kAudioUnitErr_PropertyNotWritable;
/// `kAudioUnitErr_CannotDoInCurrentContext`: the call cannot be made from this thread or in this state, and may succeed later.
pub const K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT: OSStatus =
    sys::kAudioUnitErr_CannotDoInCurrentContext;
/// `kAudioUnitErr_InvalidPropertyValue`: the value written is not valid for the property.
pub const K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE: OSStatus =
    sys::kAudioUnitErr_InvalidPropertyValue;
/// `kAudioUnitErr_PropertyNotInUse`: the property exists but is not in use in the unit's current configuration.
pub const K_AUDIO_UNIT_ERR_PROPERTY_NOT_IN_USE: OSStatus = sys::kAudioUnitErr_PropertyNotInUse;
/// `kAudioUnitErr_Initialized`: the call needs an uninitialized unit and this one is initialized.
pub const K_AUDIO_UNIT_ERR_INITIALIZED: OSStatus = sys::kAudioUnitErr_Initialized;
/// `kAudioUnitErr_InvalidOfflineRender`: an offline render was requested in a state the unit cannot do it.
pub const K_AUDIO_UNIT_ERR_INVALID_OFFLINE_RENDER: OSStatus =
    sys::kAudioUnitErr_InvalidOfflineRender;
/// `kAudioUnitErr_Unauthorized`: the process is not authorized to use the unit.
pub const K_AUDIO_UNIT_ERR_UNAUTHORIZED: OSStatus = sys::kAudioUnitErr_Unauthorized;

// AU component types (four-char codes: "auou", "aufx", …).
/// `kAudioUnitType_Output`: component type (`AudioComponentDescription::componentType`).
pub const K_AUDIO_UNIT_TYPE_OUTPUT: u32 = sys::kAudioUnitType_Output;
/// `kAudioUnitType_MusicDevice`: component type (`AudioComponentDescription::componentType`).
pub const K_AUDIO_UNIT_TYPE_MUSIC_DEVICE: u32 = sys::kAudioUnitType_MusicDevice;
/// `kAudioUnitType_MusicEffect`: component type (`AudioComponentDescription::componentType`).
pub const K_AUDIO_UNIT_TYPE_MUSIC_EFFECT: u32 = sys::kAudioUnitType_MusicEffect;
/// `kAudioUnitType_FormatConverter`: component type (`AudioComponentDescription::componentType`).
pub const K_AUDIO_UNIT_TYPE_FORMAT_CONVERTER: u32 = sys::kAudioUnitType_FormatConverter;
/// `kAudioUnitType_Effect`: component type (`AudioComponentDescription::componentType`).
pub const K_AUDIO_UNIT_TYPE_EFFECT: u32 = sys::kAudioUnitType_Effect;
/// `kAudioUnitType_Mixer`: component type (`AudioComponentDescription::componentType`).
pub const K_AUDIO_UNIT_TYPE_MIXER: u32 = sys::kAudioUnitType_Mixer;
/// `kAudioUnitType_Panner`: component type (`AudioComponentDescription::componentType`).
pub const K_AUDIO_UNIT_TYPE_PANNER: u32 = sys::kAudioUnitType_Panner;
/// `kAudioUnitType_Generator`: component type (`AudioComponentDescription::componentType`).
pub const K_AUDIO_UNIT_TYPE_GENERATOR: u32 = sys::kAudioUnitType_Generator;
/// `kAudioUnitType_OfflineEffect`: component type (`AudioComponentDescription::componentType`).
pub const K_AUDIO_UNIT_TYPE_OFFLINE_EFFECT: u32 = sys::kAudioUnitType_OfflineEffect;
/// `kAudioUnitType_MIDIProcessor`: component type (`AudioComponentDescription::componentType`).
pub const K_AUDIO_UNIT_TYPE_MIDI_PROCESSOR: u32 = sys::kAudioUnitType_MIDIProcessor;

// Linear PCM format id + format flags.
/// `kAudioFormatLinearPCM`: format id.
pub const K_AUDIO_FORMAT_LINEAR_PCM: u32 = sys::kAudioFormatLinearPCM;
/// `kAudioFormatFlagIsFloat`: `AudioStreamBasicDescription::mFormatFlags` bit.
pub const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = sys::kAudioFormatFlagIsFloat;
/// `kAudioFormatFlagIsBigEndian`: `AudioStreamBasicDescription::mFormatFlags` bit.
pub const K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN: u32 = sys::kAudioFormatFlagIsBigEndian;
/// `kAudioFormatFlagIsSignedInteger`: `AudioStreamBasicDescription::mFormatFlags` bit.
pub const K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = sys::kAudioFormatFlagIsSignedInteger;
/// `kAudioFormatFlagIsPacked`: `AudioStreamBasicDescription::mFormatFlags` bit.
pub const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = sys::kAudioFormatFlagIsPacked;
/// `kAudioFormatFlagIsAlignedHigh`: `AudioStreamBasicDescription::mFormatFlags` bit.
pub const K_AUDIO_FORMAT_FLAG_IS_ALIGNED_HIGH: u32 = sys::kAudioFormatFlagIsAlignedHigh;
/// `kAudioFormatFlagIsNonInterleaved`: `AudioStreamBasicDescription::mFormatFlags` bit.
pub const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = sys::kAudioFormatFlagIsNonInterleaved;

// Render action flags.
/// `kAudioUnitRenderAction_PreRender`: render action flag, set or read by a render notification.
pub const K_AUDIO_UNIT_RENDER_ACTION_PRE_RENDER: AudioUnitRenderActionFlags =
    sys::kAudioUnitRenderAction_PreRender;
/// `kAudioUnitRenderAction_PostRender`: render action flag, set or read by a render notification.
pub const K_AUDIO_UNIT_RENDER_ACTION_POST_RENDER: AudioUnitRenderActionFlags =
    sys::kAudioUnitRenderAction_PostRender;
/// `kAudioUnitRenderAction_OutputIsSilence`: render action flag, set or read by a render notification.
pub const K_AUDIO_UNIT_RENDER_ACTION_OUTPUT_IS_SILENCE: AudioUnitRenderActionFlags =
    sys::kAudioUnitRenderAction_OutputIsSilence;

// AudioTimeStamp validity flags.
/// `kAudioTimeStampSampleTimeValid`: `AudioTimeStamp::mFlags` validity bit.
pub const K_AUDIO_TIME_STAMP_SAMPLE_TIME_VALID: u32 = sys::kAudioTimeStampSampleTimeValid;
/// `kAudioTimeStampHostTimeValid`: `AudioTimeStamp::mFlags` validity bit.
pub const K_AUDIO_TIME_STAMP_HOST_TIME_VALID: u32 = sys::kAudioTimeStampHostTimeValid;

// AU property scopes.
/// `kAudioUnitScope_Global`: property and parameter scope.
pub const K_AUDIO_UNIT_SCOPE_GLOBAL: u32 = sys::kAudioUnitScope_Global;
/// `kAudioUnitScope_Input`: property and parameter scope.
pub const K_AUDIO_UNIT_SCOPE_INPUT: u32 = sys::kAudioUnitScope_Input;
/// `kAudioUnitScope_Output`: property and parameter scope.
pub const K_AUDIO_UNIT_SCOPE_OUTPUT: u32 = sys::kAudioUnitScope_Output;

// AU property IDs.
/// `kAudioUnitProperty_ClassInfo`: property id.
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
/// `kAudioUnitProperty_MakeConnection`: property id.
pub const K_AUDIO_UNIT_PROPERTY_MAKE_CONNECTION: u32 = sys::kAudioUnitProperty_MakeConnection;
/// `kAudioUnitProperty_SampleRate`: property id.
pub const K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE: u32 = sys::kAudioUnitProperty_SampleRate;
/// `kAudioUnitProperty_ParameterList`: property id.
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST: u32 = sys::kAudioUnitProperty_ParameterList;
/// `kAudioUnitProperty_ParameterInfo`: property id.
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_INFO: u32 = sys::kAudioUnitProperty_ParameterInfo;
/// `kAudioUnitProperty_StreamFormat`: property id.
pub const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = sys::kAudioUnitProperty_StreamFormat;
/// `kAudioUnitProperty_ElementCount`: property id.
pub const K_AUDIO_UNIT_PROPERTY_ELEMENT_COUNT: u32 = sys::kAudioUnitProperty_ElementCount;
/// `kAudioUnitProperty_Latency`: property id.
pub const K_AUDIO_UNIT_PROPERTY_LATENCY: u32 = sys::kAudioUnitProperty_Latency;
/// Seconds of audio an AU keeps producing after its input goes silent — a
/// reverb's decay, a delay's repeats. Distinct from `Latency`: latency shifts
/// audio in time, tail extends how long it lasts. An offline bounce that stops
/// at the last note truncates every tail on the master bus.
pub const K_AUDIO_UNIT_PROPERTY_TAIL_TIME: u32 = sys::kAudioUnitProperty_TailTime;
/// How long the audio on a given bus takes to *reach the listener* — the
/// latency the AU sits behind, not the latency it adds.
///
/// The exact inverse of [`K_AUDIO_UNIT_PROPERTY_LATENCY`] in direction: `Latency`
/// is the AU telling the host what it costs, this is the host telling the AU
/// what the rest of the chain costs. Apple's header: "This property is set by a
/// host to describe to the audio unit the presentation latency of both any of
/// its input and/or output audio data. It describes this latency in seconds. A
/// value of zero means either no latency or an unknown latency." Write-only, and
/// per `(scope, element)` — the header asks for it "on each active input and
/// output bus", because a multi-output AU's buses can be presented at different
/// times.
///
/// A `Float64` count of **seconds**, like `Latency` and `TailTime`. See
/// [`presentation_latency`](crate::transport::set_presentation_latency) for why
/// the host-side signature takes [`Seconds`] rather than converting to samples.
pub const K_AUDIO_UNIT_PROPERTY_PRESENTATION_LATENCY: u32 =
    sys::kAudioUnitProperty_PresentationLatency;
/// Which other parameters a *meta*-parameter moves when it is written.
///
/// Read-only, an array of `AUDependentParameter` (a `(scope, parameter id)`
/// pair, 8 bytes). Keyed by the meta-parameter's own address: the query is made
/// at the meta-parameter's scope with its **id in the element position**, the
/// same addressing `ParameterInfo` uses (see [`crate::parameters::info_at`]).
///
/// The property exists because a parameter carrying
/// `kAudioUnitParameterFlag_IsGlobalMeta` or `..._IsElementMeta` may change any
/// number of others when written, and the AU sends no per-parameter
/// notification for the ones it moved. A host caching parameter values or ranges
/// has no other way to learn what went stale.
pub const K_AUDIO_UNIT_PROPERTY_DEPENDENT_PARAMETERS: u32 =
    sys::kAudioUnitProperty_DependentParameters;
/// `kAudioUnitProperty_SupportedNumChannels`: property id.
pub const K_AUDIO_UNIT_PROPERTY_SUPPORTED_NUM_CHANNELS: u32 =
    sys::kAudioUnitProperty_SupportedNumChannels;
/// `kAudioUnitProperty_MaximumFramesPerSlice`: property id.
pub const K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE: u32 =
    sys::kAudioUnitProperty_MaximumFramesPerSlice;
/// `kAudioUnitProperty_SetRenderCallback`: property id.
pub const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 =
    sys::kAudioUnitProperty_SetRenderCallback;
/// `kAudioUnitProperty_FactoryPresets`: property id.
pub const K_AUDIO_UNIT_PROPERTY_FACTORY_PRESETS: u32 = sys::kAudioUnitProperty_FactoryPresets;
/// `kAudioUnitProperty_RenderQuality`: property id.
pub const K_AUDIO_UNIT_PROPERTY_RENDER_QUALITY: u32 = sys::kAudioUnitProperty_RenderQuality;
/// `kAudioUnitProperty_HostCallbacks`: property id.
pub const K_AUDIO_UNIT_PROPERTY_HOST_CALLBACKS: u32 = sys::kAudioUnitProperty_HostCallbacks;
/// `kAudioUnitProperty_InPlaceProcessing`: property id.
pub const K_AUDIO_UNIT_PROPERTY_IN_PLACE_PROCESSING: u32 =
    sys::kAudioUnitProperty_InPlaceProcessing;
/// `kAudioUnitProperty_ElementName`: property id.
pub const K_AUDIO_UNIT_PROPERTY_ELEMENT_NAME: u32 = sys::kAudioUnitProperty_ElementName;
/// Where in the *host's* project this instance sits — "track 3", "Drum Bus".
/// Read/write: all 57 instantiable units measured return the exact string
/// written. See [`crate::identity`].
pub const K_AUDIO_UNIT_PROPERTY_CONTEXT_NAME: u32 = sys::kAudioUnitProperty_ContextName;
/// A per-instance name distinguishing two loads of the *same* AU from each
/// other. Unlike `ContextName` this is the instance's own identity, so it is
/// what belongs in a saved session. See [`crate::identity`].
pub const K_AUDIO_UNIT_PROPERTY_NICK_NAME: u32 = sys::kAudioUnitProperty_NickName;
/// The AU's own answer to "if you had room for only N knobs, which?" — a
/// curated, *priority-ordered* subset of the parameter list. See
/// [`crate::identity`].
pub const K_AUDIO_UNIT_PROPERTY_PARAMETERS_FOR_OVERVIEW: u32 =
    sys::kAudioUnitProperty_ParametersForOverview;
/// A `CFURLRef` to an icon file for this AU, for a plugin browser. See
/// [`crate::identity`].
pub const K_AUDIO_UNIT_PROPERTY_ICON_LOCATION: u32 = sys::kAudioUnitProperty_IconLocation;
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

/// `kAudioUnitProperty_BypassEffect`: property id.
pub const K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT: u32 = sys::kAudioUnitProperty_BypassEffect;
/// `kAudioUnitProperty_LastRenderError`: property id.
pub const K_AUDIO_UNIT_PROPERTY_LAST_RENDER_ERROR: u32 = sys::kAudioUnitProperty_LastRenderError;
/// `kAudioUnitProperty_PresentPreset`: property id.
pub const K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET: u32 = sys::kAudioUnitProperty_PresentPreset;
/// `kAudioUnitProperty_CocoaUI`: property id.
pub const K_AUDIO_UNIT_PROPERTY_COCOA_UI: u32 = sys::kAudioUnitProperty_CocoaUI;
/// `kAudioUnitProperty_ParameterValueStrings`: property id.
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_STRINGS: u32 =
    sys::kAudioUnitProperty_ParameterValueStrings;
/// `kAudioUnitProperty_ParameterStringFromValue`: property id.
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_STRING_FROM_VALUE: u32 =
    sys::kAudioUnitProperty_ParameterStringFromValue;
/// `kAudioUnitProperty_ParameterValueFromString`: property id.
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_FROM_STRING: u32 =
    sys::kAudioUnitProperty_ParameterValueFromString;
/// `kAudioUnitProperty_ParameterClumpName`: property id.
pub const K_AUDIO_UNIT_PROPERTY_PARAMETER_CLUMP_NAME: u32 =
    sys::kAudioUnitProperty_ParameterClumpName;

// Parameter unit kinds.
/// `kAudioUnitParameterUnit_Generic`: parameter unit kind (`AudioUnitParameterInfo::unit`).
pub const K_AUDIO_UNIT_PARAMETER_UNIT_GENERIC: u32 = sys::kAudioUnitParameterUnit_Generic;
/// A discrete choice list: the value is an index into named options, and
/// `[min, max]` are the first and last index rather than a continuous span.
pub const K_AUDIO_UNIT_PARAMETER_UNIT_INDEXED: u32 = sys::kAudioUnitParameterUnit_Indexed;
/// `kAudioUnitParameterUnit_Boolean`: parameter unit kind (`AudioUnitParameterInfo::unit`).
pub const K_AUDIO_UNIT_PARAMETER_UNIT_BOOLEAN: u32 = sys::kAudioUnitParameterUnit_Boolean;
/// `kAudioUnitParameterUnit_Percent`: parameter unit kind (`AudioUnitParameterInfo::unit`).
pub const K_AUDIO_UNIT_PARAMETER_UNIT_PERCENT: u32 = sys::kAudioUnitParameterUnit_Percent;
/// `kAudioUnitParameterUnit_Seconds`: parameter unit kind (`AudioUnitParameterInfo::unit`).
pub const K_AUDIO_UNIT_PARAMETER_UNIT_SECONDS: u32 = sys::kAudioUnitParameterUnit_Seconds;
/// `kAudioUnitParameterUnit_Hertz`: parameter unit kind (`AudioUnitParameterInfo::unit`).
pub const K_AUDIO_UNIT_PARAMETER_UNIT_HERTZ: u32 = sys::kAudioUnitParameterUnit_Hertz;
/// `kAudioUnitParameterUnit_Decibels`: parameter unit kind (`AudioUnitParameterInfo::unit`).
pub const K_AUDIO_UNIT_PARAMETER_UNIT_DECIBELS: u32 = sys::kAudioUnitParameterUnit_Decibels;
/// `kAudioUnitParameterUnit_LinearGain`: parameter unit kind (`AudioUnitParameterInfo::unit`).
pub const K_AUDIO_UNIT_PARAMETER_UNIT_LINEAR_GAIN: u32 = sys::kAudioUnitParameterUnit_LinearGain;
/// A parameter whose value **is** a MIDI controller number, `0..=127` — not a
/// quantity in a physical unit. Aliased so a mapping-aware host can format it as
/// "CC 74" rather than as a bare `74`; see [`crate::midi_map`].
pub const K_AUDIO_UNIT_PARAMETER_UNIT_MIDI_CONTROLLER: u32 =
    sys::kAudioUnitParameterUnit_MIDIController;

// Parameter flag bits.
/// `kAudioUnitParameterFlag_IsReadable`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_IS_READABLE: u32 = sys::kAudioUnitParameterFlag_IsReadable;
/// `kAudioUnitParameterFlag_IsWritable`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_IS_WRITABLE: u32 = sys::kAudioUnitParameterFlag_IsWritable;
/// `kAudioUnitParameterFlag_HasName`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_HAS_NAME: u32 = sys::kAudioUnitParameterFlag_HasName;
/// `kAudioUnitParameterFlag_HasCFNameString`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_HAS_CF_NAME_STRING: u32 =
    sys::kAudioUnitParameterFlag_HasCFNameString;
/// `kAudioUnitParameterFlag_HasClump`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_HAS_CLUMP: u32 = sys::kAudioUnitParameterFlag_HasClump;
/// `kAudioUnitParameterFlag_ValuesHaveStrings`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_VALUES_HAVE_STRINGS: u32 =
    sys::kAudioUnitParameterFlag_ValuesHaveStrings;
/// `kAudioUnitParameterFlag_MeterReadOnly`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_METER_READ_ONLY: u32 =
    sys::kAudioUnitParameterFlag_MeterReadOnly;
/// `kAudioUnitParameterFlag_IsHighResolution`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_IS_HIGH_RESOLUTION: u32 =
    sys::kAudioUnitParameterFlag_IsHighResolution;
/// `kAudioUnitParameterFlag_NonRealTime`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_NON_REAL_TIME: u32 = sys::kAudioUnitParameterFlag_NonRealTime;
/// `kAudioUnitParameterFlag_CanRamp`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_CAN_RAMP: u32 = sys::kAudioUnitParameterFlag_CanRamp;
/// `kAudioUnitParameterFlag_OmitFromPresets`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_OMIT_FROM_PRESETS: u32 =
    sys::kAudioUnitParameterFlag_OmitFromPresets;
/// "Changing this parameter may change any number of others in the AudioUnit."
/// The dependents may live on any scope, and per Apple's header a non-global
/// dependent is dependent "in every element of their scope".
pub const K_AUDIO_UNIT_PARAMETER_FLAG_IS_GLOBAL_META: u32 =
    sys::kAudioUnitParameterFlag_IsGlobalMeta;
/// "Changing this parameter may change others in the same element as the current
/// parameter." Narrower than `IsGlobalMeta`: the dependents must share one scope
/// and apply only within that single element.
pub const K_AUDIO_UNIT_PARAMETER_FLAG_IS_ELEMENT_META: u32 =
    sys::kAudioUnitParameterFlag_IsElementMeta;

// Display-curve flags. `DISPLAY_MASK` covers a *non-contiguous* field: bits
// 16..=18 hold the curve index and bit 22 is the separate Logarithmic flag
// (`(7<<16) | (1<<22)`). Masking with only `7<<16` silently drops every
// logarithmic parameter — 24 of the 39 curve-carrying parameters measured on
// macOS 15.6 — so the mask must come from Apple's own constant, not a
// hand-written one.
/// `kAudioUnitParameterFlag_DisplayMask`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_MASK: u32 = sys::kAudioUnitParameterFlag_DisplayMask;
/// `kAudioUnitParameterFlag_DisplaySquareRoot`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_SQUARE_ROOT: u32 =
    sys::kAudioUnitParameterFlag_DisplaySquareRoot;
/// `kAudioUnitParameterFlag_DisplaySquared`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_SQUARED: u32 =
    sys::kAudioUnitParameterFlag_DisplaySquared;
/// `kAudioUnitParameterFlag_DisplayCubed`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_CUBED: u32 =
    sys::kAudioUnitParameterFlag_DisplayCubed;
/// `kAudioUnitParameterFlag_DisplayCubeRoot`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_CUBE_ROOT: u32 =
    sys::kAudioUnitParameterFlag_DisplayCubeRoot;
/// `kAudioUnitParameterFlag_DisplayExponential`: parameter flag bit (`AudioUnitParameterInfo::flags`).
pub const K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_EXPONENTIAL: u32 =
    sys::kAudioUnitParameterFlag_DisplayExponential;
/// `kAudioUnitParameterFlag_DisplayLogarithmic`: parameter flag bit (`AudioUnitParameterInfo::flags`).
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
/// `kAudioChannelLayoutTag_UseChannelDescriptions`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_USE_CHANNEL_DESCRIPTIONS: u32 =
    sys::kAudioChannelLayoutTag_UseChannelDescriptions;
/// `kAudioChannelLayoutTag_UseChannelBitmap`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_USE_CHANNEL_BITMAP: u32 =
    sys::kAudioChannelLayoutTag_UseChannelBitmap;
/// `kAudioChannelLayoutTag_Mono`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MONO: u32 = sys::kAudioChannelLayoutTag_Mono;
/// `kAudioChannelLayoutTag_Stereo`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_STEREO: u32 = sys::kAudioChannelLayoutTag_Stereo;
/// `kAudioChannelLayoutTag_StereoHeadphones`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_STEREO_HEADPHONES: u32 =
    sys::kAudioChannelLayoutTag_StereoHeadphones;
/// `kAudioChannelLayoutTag_MatrixStereo`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MATRIX_STEREO: u32 = sys::kAudioChannelLayoutTag_MatrixStereo;
/// `kAudioChannelLayoutTag_MidSide`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MID_SIDE: u32 = sys::kAudioChannelLayoutTag_MidSide;
/// `kAudioChannelLayoutTag_XY`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_XY: u32 = sys::kAudioChannelLayoutTag_XY;
/// `kAudioChannelLayoutTag_Binaural`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_BINAURAL: u32 = sys::kAudioChannelLayoutTag_Binaural;
/// `kAudioChannelLayoutTag_Ambisonic_B_Format`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AMBISONIC_B_FORMAT: u32 =
    sys::kAudioChannelLayoutTag_Ambisonic_B_Format;
/// `kAudioChannelLayoutTag_Quadraphonic`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_QUADRAPHONIC: u32 = sys::kAudioChannelLayoutTag_Quadraphonic;
/// `kAudioChannelLayoutTag_Pentagonal`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_PENTAGONAL: u32 = sys::kAudioChannelLayoutTag_Pentagonal;
/// `kAudioChannelLayoutTag_Hexagonal`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_HEXAGONAL: u32 = sys::kAudioChannelLayoutTag_Hexagonal;
/// `kAudioChannelLayoutTag_Octagonal`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_OCTAGONAL: u32 = sys::kAudioChannelLayoutTag_Octagonal;
/// `kAudioChannelLayoutTag_Cube`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_CUBE: u32 = sys::kAudioChannelLayoutTag_Cube;
/// `kAudioChannelLayoutTag_AudioUnit_5_0`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_0: u32 =
    sys::kAudioChannelLayoutTag_AudioUnit_5_0;
/// `kAudioChannelLayoutTag_AudioUnit_5_1`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_5_1: u32 =
    sys::kAudioChannelLayoutTag_AudioUnit_5_1;
/// `kAudioChannelLayoutTag_AudioUnit_6_0`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_6_0: u32 =
    sys::kAudioChannelLayoutTag_AudioUnit_6_0;
/// `kAudioChannelLayoutTag_AudioUnit_7_0`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_7_0: u32 =
    sys::kAudioChannelLayoutTag_AudioUnit_7_0;
/// `kAudioChannelLayoutTag_AudioUnit_7_1`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_7_1: u32 =
    sys::kAudioChannelLayoutTag_AudioUnit_7_1;
// The alias spellings, present only so the aliasing itself is testable.
/// `kAudioChannelLayoutTag_AudioUnit_4`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_4: u32 = sys::kAudioChannelLayoutTag_AudioUnit_4;
/// `kAudioChannelLayoutTag_AudioUnit_6`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_6: u32 = sys::kAudioChannelLayoutTag_AudioUnit_6;
/// `kAudioChannelLayoutTag_AudioUnit_8`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_AUDIO_UNIT_8: u32 = sys::kAudioChannelLayoutTag_AudioUnit_8;
/// `kAudioChannelLayoutTag_MPEG_5_0_B`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MPEG_5_0_B: u32 = sys::kAudioChannelLayoutTag_MPEG_5_0_B;
/// `kAudioChannelLayoutTag_MPEG_5_1_A`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
pub const K_AUDIO_CHANNEL_LAYOUT_TAG_MPEG_5_1_A: u32 = sys::kAudioChannelLayoutTag_MPEG_5_1_A;
/// `kAudioChannelLayoutTag_MPEG_7_1_C`: channel layout tag (`AudioChannelLayout::mChannelLayoutTag`).
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
    /// Builds an ASBD for 32-bit float, non-interleaved, packed linear PCM.
    ///
    /// This is the format the rest of the crate uses for AU I/O. For
    /// non-interleaved audio `mBytesPerFrame` / `mBytesPerPacket` are `4`
    /// (one channel's worth), *not* `channels * 4`.
    fn float32(sample_rate: f64, channels: u32) -> Self;

    /// The crate's fallback stream format: 44.1 kHz stereo float32.
    ///
    /// A starting point for a probe, not a claim about the AU — every real
    /// configuration re-reads the rate and channel count the unit accepted.
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
    /// Builds a timestamp with only `mSampleTime` marked valid.
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

/// Converts a big-endian four-character code (e.g. `b"aufx"`) to its string form.
///
/// Invalid UTF-8 bytes are replaced with the Unicode replacement character.
pub fn fourcc_to_string(code: u32) -> String {
    let bytes = code.to_be_bytes();
    String::from_utf8_lossy(&bytes).to_string()
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
