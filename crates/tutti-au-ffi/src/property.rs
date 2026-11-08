//! Typed addresses: the property, scope and element of an AudioUnit call.

use coreaudio_sys as sys;

/// An AudioUnit property: its id and the value it carries. `Value` is `()` for a
/// property read through a dedicated method (a string, an array, a property
/// list, raw bytes).
pub trait Property {
    /// The `AudioUnitPropertyID`.
    const ID: u32;
    /// The plain value a [`Unit::get`](crate::Unit::get) returns.
    type Value;
}

/// Where a property, parameter or event lives: global, or one side of the unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scope(u32);

impl Scope {
    /// `kAudioUnitScope_Global`.
    pub const GLOBAL: Self = Self(sys::kAudioUnitScope_Global);
    /// `kAudioUnitScope_Input`.
    pub const INPUT: Self = Self(sys::kAudioUnitScope_Input);
    /// `kAudioUnitScope_Output`.
    pub const OUTPUT: Self = Self(sys::kAudioUnitScope_Output);

    /// A scope from its raw value (a unit may use scopes of its own).
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw `AudioUnitScope`.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// A bus within a scope, or a parameter id where a property is keyed by one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Element(pub u32);

impl Element {
    /// Element 0, the only one of the global scope.
    pub const ZERO: Self = Self(0);
}

/// `ClassInfo`: The unit's whole state as a property list.
#[derive(Debug, Clone, Copy)]
pub struct ClassInfo;

impl Property for ClassInfo {
    const ID: u32 = sys::kAudioUnitProperty_ClassInfo;
    type Value = ();
}

/// `ClassInfoFromDocument`: The document-restore twin of `ClassInfo` (id 50, absent from the generated bindings); a host restoring a document tries it first.
#[derive(Debug, Clone, Copy)]
pub struct ClassInfoFromDocument;

impl Property for ClassInfoFromDocument {
    const ID: u32 = 50;
    type Value = ();
}

/// `SampleRate`: The sample rate of a scope.
#[derive(Debug, Clone, Copy)]
pub struct SampleRate;

impl Property for SampleRate {
    const ID: u32 = sys::kAudioUnitProperty_SampleRate;
    type Value = f64;
}

/// `ParameterList`: The parameter ids of a scope.
#[derive(Debug, Clone, Copy)]
pub struct ParameterList;

impl Property for ParameterList {
    const ID: u32 = sys::kAudioUnitProperty_ParameterList;
    type Value = u32;
}

/// `ParameterInfo`: One parameter's description, addressed with the parameter id as the element.
#[derive(Debug, Clone, Copy)]
pub struct ParameterInfo;

impl Property for ParameterInfo {
    const ID: u32 = sys::kAudioUnitProperty_ParameterInfo;
    type Value = sys::AudioUnitParameterInfo;
}

/// `StreamFormat`: The stream format of a bus.
#[derive(Debug, Clone, Copy)]
pub struct StreamFormat;

impl Property for StreamFormat {
    const ID: u32 = sys::kAudioUnitProperty_StreamFormat;
    type Value = sys::AudioStreamBasicDescription;
}

/// `ElementCount`: The number of buses in a scope.
#[derive(Debug, Clone, Copy)]
pub struct ElementCount;

impl Property for ElementCount {
    const ID: u32 = sys::kAudioUnitProperty_ElementCount;
    type Value = u32;
}

/// `Latency`: The latency the unit adds, in seconds.
#[derive(Debug, Clone, Copy)]
pub struct Latency;

impl Property for Latency {
    const ID: u32 = sys::kAudioUnitProperty_Latency;
    type Value = f64;
}

/// `TailTime`: Seconds of output after the input goes silent.
#[derive(Debug, Clone, Copy)]
pub struct TailTime;

impl Property for TailTime {
    const ID: u32 = sys::kAudioUnitProperty_TailTime;
    type Value = f64;
}

/// `PresentationLatency`: The latency downstream of a bus, in seconds; write-only.
#[derive(Debug, Clone, Copy)]
pub struct PresentationLatency;

impl Property for PresentationLatency {
    const ID: u32 = sys::kAudioUnitProperty_PresentationLatency;
    type Value = f64;
}

/// `DependentParameters`: The parameters a meta-parameter moves.
#[derive(Debug, Clone, Copy)]
pub struct DependentParameters;

impl Property for DependentParameters {
    const ID: u32 = sys::kAudioUnitProperty_DependentParameters;
    type Value = sys::AUDependentParameter;
}

/// `SupportedNumChannels`: The channel-count pairs the unit accepts.
#[derive(Debug, Clone, Copy)]
pub struct SupportedNumChannels;

impl Property for SupportedNumChannels {
    const ID: u32 = sys::kAudioUnitProperty_SupportedNumChannels;
    type Value = ();
}

/// `MaximumFramesPerSlice`: The largest render the unit sizes its buffers for.
#[derive(Debug, Clone, Copy)]
pub struct MaximumFramesPerSlice;

impl Property for MaximumFramesPerSlice {
    const ID: u32 = sys::kAudioUnitProperty_MaximumFramesPerSlice;
    type Value = u32;
}

/// `SetRenderCallback`: The input-pull callback of an input bus.
#[derive(Debug, Clone, Copy)]
pub struct SetRenderCallback;

impl Property for SetRenderCallback {
    const ID: u32 = sys::kAudioUnitProperty_SetRenderCallback;
    type Value = sys::AURenderCallbackStruct;
}

/// `FactoryPresets`: The factory preset array.
#[derive(Debug, Clone, Copy)]
pub struct FactoryPresets;

impl Property for FactoryPresets {
    const ID: u32 = sys::kAudioUnitProperty_FactoryPresets;
    type Value = ();
}

/// `RenderQuality`: Render quality, 0 to 127.
#[derive(Debug, Clone, Copy)]
pub struct RenderQuality;

impl Property for RenderQuality {
    const ID: u32 = sys::kAudioUnitProperty_RenderQuality;
    type Value = u32;
}

/// `InPlaceProcessing`: Whether the unit can render in place.
#[derive(Debug, Clone, Copy)]
pub struct InPlaceProcessing;

impl Property for InPlaceProcessing {
    const ID: u32 = sys::kAudioUnitProperty_InPlaceProcessing;
    type Value = u32;
}

/// `ElementName`: A bus name.
#[derive(Debug, Clone, Copy)]
pub struct ElementName;

impl Property for ElementName {
    const ID: u32 = sys::kAudioUnitProperty_ElementName;
    type Value = ();
}

/// `ContextName`: Where the host has put the instance ("track 3").
#[derive(Debug, Clone, Copy)]
pub struct ContextName;

impl Property for ContextName {
    const ID: u32 = sys::kAudioUnitProperty_ContextName;
    type Value = ();
}

/// `NickName`: The instance's own name.
#[derive(Debug, Clone, Copy)]
pub struct NickName;

impl Property for NickName {
    const ID: u32 = sys::kAudioUnitProperty_NickName;
    type Value = ();
}

/// `ParametersForOverview`: The unit's curated shortlist of parameters.
#[derive(Debug, Clone, Copy)]
pub struct ParametersForOverview;

impl Property for ParametersForOverview {
    const ID: u32 = sys::kAudioUnitProperty_ParametersForOverview;
    type Value = sys::AudioUnitParameter;
}

/// `IconLocation`: A URL of the unit's icon.
#[derive(Debug, Clone, Copy)]
pub struct IconLocation;

impl Property for IconLocation {
    const ID: u32 = sys::kAudioUnitProperty_IconLocation;
    type Value = ();
}

/// `AudioChannelLayoutProperty`: The channel order of a bus.
#[derive(Debug, Clone, Copy)]
pub struct AudioChannelLayoutProperty;

impl Property for AudioChannelLayoutProperty {
    const ID: u32 = sys::kAudioUnitProperty_AudioChannelLayout;
    type Value = sys::AudioChannelLayout;
}

/// `SupportedChannelLayoutTags`: The channel orders a bus accepts.
#[derive(Debug, Clone, Copy)]
pub struct SupportedChannelLayoutTags;

impl Property for SupportedChannelLayoutTags {
    const ID: u32 = sys::kAudioUnitProperty_SupportedChannelLayoutTags;
    type Value = ();
}

/// `MidiOutputCallbackInfo`: The names of the MIDI output streams.
#[derive(Debug, Clone, Copy)]
pub struct MidiOutputCallbackInfo;

impl Property for MidiOutputCallbackInfo {
    const ID: u32 = sys::kAudioUnitProperty_MIDIOutputCallbackInfo;
    type Value = ();
}

/// `AllParameterMidiMappings`: Read or write the whole parameter-to-MIDI mapping table (id 41).
#[derive(Debug, Clone, Copy)]
pub struct AllParameterMidiMappings;

impl Property for AllParameterMidiMappings {
    const ID: u32 = 41;
    type Value = ();
}

/// `AddParameterMidiMapping`: Merge mappings into the table (id 42, write-only).
#[derive(Debug, Clone, Copy)]
pub struct AddParameterMidiMapping;

impl Property for AddParameterMidiMapping {
    const ID: u32 = 42;
    type Value = ();
}

/// `RemoveParameterMidiMapping`: Drop mappings from the table (id 43, write-only).
#[derive(Debug, Clone, Copy)]
pub struct RemoveParameterMidiMapping;

impl Property for RemoveParameterMidiMapping {
    const ID: u32 = 43;
    type Value = ();
}

/// `HotMapParameterMidiMapping`: "Learn" mode: the unit maps the next MIDI message it sees (id 44).
#[derive(Debug, Clone, Copy)]
pub struct HotMapParameterMidiMapping;

impl Property for HotMapParameterMidiMapping {
    const ID: u32 = 44;
    type Value = ();
}

/// `MidiControlMapping`: The deprecated read-only CC mapping (id 17), implemented by no installed unit.
#[derive(Debug, Clone, Copy)]
pub struct MidiControlMapping;

impl Property for MidiControlMapping {
    const ID: u32 = 17;
    type Value = ();
}

/// `BypassEffect`: Whether the effect is bypassed.
#[derive(Debug, Clone, Copy)]
pub struct BypassEffect;

impl Property for BypassEffect {
    const ID: u32 = sys::kAudioUnitProperty_BypassEffect;
    type Value = u32;
}

/// `LastRenderError`: The status of the unit's last failed render.
#[derive(Debug, Clone, Copy)]
pub struct LastRenderError;

impl Property for LastRenderError {
    const ID: u32 = sys::kAudioUnitProperty_LastRenderError;
    type Value = i32;
}

/// `PresentPreset`: The selected preset.
#[derive(Debug, Clone, Copy)]
pub struct PresentPreset;

impl Property for PresentPreset {
    const ID: u32 = sys::kAudioUnitProperty_PresentPreset;
    type Value = sys::AUPreset;
}

/// `CocoaUi`: Where the unit's Cocoa view lives.
#[derive(Debug, Clone, Copy)]
pub struct CocoaUi;

impl Property for CocoaUi {
    const ID: u32 = sys::kAudioUnitProperty_CocoaUI;
    type Value = sys::AudioUnitCocoaViewInfo;
}

/// `ParameterValueStrings`: The value names of an indexed parameter.
#[derive(Debug, Clone, Copy)]
pub struct ParameterValueStrings;

impl Property for ParameterValueStrings {
    const ID: u32 = sys::kAudioUnitProperty_ParameterValueStrings;
    type Value = ();
}

/// `ParameterStringFromValue`: The unit's text for a value.
#[derive(Debug, Clone, Copy)]
pub struct ParameterStringFromValue;

impl Property for ParameterStringFromValue {
    const ID: u32 = sys::kAudioUnitProperty_ParameterStringFromValue;
    type Value = sys::AudioUnitParameterStringFromValue;
}

/// `ParameterValueFromString`: The value the unit parses from text.
#[derive(Debug, Clone, Copy)]
pub struct ParameterValueFromString;

impl Property for ParameterValueFromString {
    const ID: u32 = sys::kAudioUnitProperty_ParameterValueFromString;
    type Value = sys::AudioUnitParameterValueFromString;
}

/// `OfflineRender`: Whether the unit renders offline.
#[derive(Debug, Clone, Copy)]
pub struct OfflineRender;

impl Property for OfflineRender {
    const ID: u32 = sys::kAudioUnitProperty_OfflineRender;
    type Value = u32;
}

/// The crate-private property `HostCallbacks`.
pub(crate) struct HostCallbacksProperty;

impl Property for HostCallbacksProperty {
    const ID: u32 = sys::kAudioUnitProperty_HostCallbacks;
    type Value = crate::transport::HostCallbackInfo;
}

/// The property `MidiOutputCallback`.
pub(crate) struct MidiOutputCallbackProperty;

impl Property for MidiOutputCallbackProperty {
    const ID: u32 = sys::kAudioUnitProperty_MIDIOutputCallback;
    type Value = crate::midi_out::AuMidiOutputCallbackStruct;
}

/// The property `ParameterClumpName`.
pub struct ParameterClumpName;

impl Property for ParameterClumpName {
    const ID: u32 = sys::kAudioUnitProperty_ParameterClumpName;
    type Value = crate::parameters::ClumpNameRequest;
}
