# Audio Unit (AUv2 C API) — Host-Side Interface Reference

**Source of truth:** Apple SDK headers at
`/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/System/Library/Frameworks/AudioToolbox.framework/Headers/`
— `AUComponent.h`, `AudioUnitProperties.h`, `AudioUnitParameters.h`, `AudioComponent.h`, `AudioOutputUnit.h`, `MusicDevice.h`, `AUCocoaUIView.h`.

Extracted verbatim from those headers. Scope/Value Type/Access columns are copied from Apple's own doc comments; `—` means the header's doc block leaves the field blank.

**Direction legend**
- **H→P** — host calls into the plugin.
- **P→H** — plugin calls back into the host. These are the surfaces a host must *implement*.

---

## Table of Contents

- [A. Core C entry points (H→P)](#a-core-c-entry-points-hp)
- [B. Every `kAudioUnitProperty_*` and sibling property constant](#b-property-constants)
- [C. Host-callback surfaces (P→H)](#c-host-callback-surfaces-ph)
- [D. Key enums and structs](#d-key-enums-and-structs)
- [E. AUv3 note](#e-auv3-note)

---

# A. Core C entry points (H→P)

## A.1 Discovery & instantiation — `AudioComponent.h`

| Function | Signature | Purpose |
|---|---|---|
| `AudioComponentFindNext` | `AudioComponent __nullable AudioComponentFindNext(AudioComponent __nullable inComponent, const AudioComponentDescription *inDesc)` | Iterate registered components matching a description. Pass `NULL` to start; pass the previous result to continue. Zero fields in `inDesc` are wildcards. |
| `AudioComponentCount` | `UInt32 AudioComponentCount(const AudioComponentDescription *inDesc)` | Count components matching a description. |
| `AudioComponentCopyName` | `OSStatus AudioComponentCopyName(AudioComponent inComponent, CFStringRef __nullable * __nonnull outName)` | Retrieve the component's display name (caller releases). |
| `AudioComponentGetDescription` | `OSStatus AudioComponentGetDescription(AudioComponent inComponent, AudioComponentDescription *outDesc)` | Retrieve the full description of a found component. |
| `AudioComponentGetVersion` | `OSStatus AudioComponentGetVersion(AudioComponent inComponent, UInt32 *outVersion)` | Version as `0xMMMMmmDD` (Major, Minor, Dot). |
| `AudioComponentInstanceNew` | `OSStatus AudioComponentInstanceNew(AudioComponent inComponent, AudioComponentInstance __nullable * __nonnull outInstance)` | Synchronously create an instance. **Not valid** for components with `kAudioComponentFlag_RequiresAsyncInstantiation`. |
| `AudioComponentInstantiate` | `void AudioComponentInstantiate(AudioComponent inComponent, AudioComponentInstantiationOptions inOptions, void (^inCompletionHandler)(AudioComponentInstance __nullable, OSStatus))` | Asynchronous instantiation. Required for AUv3/out-of-process units. Carries the in-process vs out-of-process option. |
| `AudioComponentInstanceDispose` | `OSStatus AudioComponentInstanceDispose(AudioComponentInstance inInstance)` | Destroy an instance and free its resources. |
| `AudioComponentInstanceGetComponent` | `AudioComponent AudioComponentInstanceGetComponent(AudioComponentInstance inInstance)` | Recover the factory `AudioComponent` from a live instance. |
| `AudioComponentInstanceCanDo` | `Boolean AudioComponentInstanceCanDo(AudioComponentInstance inInstance, SInt16 inSelectorID)` | Ask whether the instance implements a given component selector (e.g. `kMusicDeviceStartNoteSelect`). |
| `AudioComponentCopyConfigurationInfo` | `OSStatus AudioComponentCopyConfigurationInfo(AudioComponent inComponent, CFDictionaryRef __nullable * __nonnull outConfigurationInfo)` | Static, **out-of-process** metadata dictionary — lets a host learn channel configs, custom-view presence, etc. *without opening the plugin*. |
| `AudioComponentRegister` | `AudioComponent AudioComponentRegister(const AudioComponentDescription *inDesc, CFStringRef inName, UInt32 inVersion, AudioComponentFactoryFunction inFactory)` | Register a component within the current process only. |
| `AudioComponentValidate` | `OSStatus AudioComponentValidate(AudioComponent inComponent, CFDictionaryRef __nullable inValidationParameters, AudioComponentValidationResult *outValidationResult)` | Run Apple's validation on a component. |
| `AudioComponentValidateWithResults` | `OSStatus AudioComponentValidateWithResults(AudioComponent inComponent, CFDictionaryRef __nullable inValidationParameters, void (^inCompletionHandler)(AudioComponentValidationResult, CFDictionaryRef __nullable))` | Async validation, returning a detail dictionary. |
| `AudioComponentCopyIcon` | `NSImage * AudioComponentCopyIcon(AudioComponent comp)` — macOS 11.0+ | Plugin icon. Replaces the deprecated `AudioComponentGetIcon`. |
| `AudioComponentGetIcon` | *(deprecated)* | **[DEPRECATED]** macOS 10.11–11.0; use `AudioComponentCopyIcon`. |

**Notifications (CFNotificationCenter local center, object `NULL`):**

| Constant | Purpose |
|---|---|
| `kAudioComponentRegistrationsChangedNotification` | The set of available AudioComponents changed — a host should rescan. |
| `kAudioComponentInstanceInvalidationNotification` | The connection to an audio-unit *extension process* was invalidated (the plugin process died). Host must tear down the instance. |

## A.2 Lifecycle & properties — `AUComponent.h`

| Function | Signature | Purpose |
|---|---|---|
| `AudioUnitInitialize` | `OSStatus AudioUnitInitialize(AudioUnit inUnit)` | Allocate resources and make the unit renderable. Formats/`MaximumFramesPerSlice` must be set *before* this. |
| `AudioUnitUninitialize` | `OSStatus AudioUnitUninitialize(AudioUnit inUnit)` | Release render resources; returns the unit to the configurable state. |
| `AudioUnitGetPropertyInfo` | `OSStatus AudioUnitGetPropertyInfo(AudioUnit inUnit, AudioUnitPropertyID inID, AudioUnitScope inScope, AudioUnitElement inElement, UInt32 * __nullable outDataSize, Boolean * __nullable outWritable)` | Query a property's data size and writability without fetching it. The standard way to size a variable-length property buffer. |
| `AudioUnitGetProperty` | `OSStatus AudioUnitGetProperty(AudioUnit inUnit, AudioUnitPropertyID inID, AudioUnitScope inScope, AudioUnitElement inElement, void *outData, UInt32 *ioDataSize)` | Read a property. `ioDataSize` is in/out. |
| `AudioUnitSetProperty` | `OSStatus AudioUnitSetProperty(AudioUnit inUnit, AudioUnitPropertyID inID, AudioUnitScope inScope, AudioUnitElement inElement, const void * __nullable inData, UInt32 inDataSize)` | Write a property. `inData == NULL` with `inDataSize == 0` *removes* a previously set value (only valid for some properties). |
| `AudioUnitAddPropertyListener` | `OSStatus AudioUnitAddPropertyListener(AudioUnit inUnit, AudioUnitPropertyID inID, AudioUnitPropertyListenerProc inProc, void * __nullable inProcUserData)` | Register for change notifications on one property. |
| `AudioUnitRemovePropertyListenerWithUserData` | `OSStatus AudioUnitRemovePropertyListenerWithUserData(AudioUnit inUnit, AudioUnitPropertyID inID, AudioUnitPropertyListenerProc inProc, void * __nullable inProcUserData)` | Unregister. `(inProc, inProcUserData)` is treated as a **tuple** — both must match. |
| `AudioUnitRemovePropertyListener` | `OSStatus AudioUnitRemovePropertyListener(AudioUnit, AudioUnitPropertyID, AudioUnitPropertyListenerProc)` | **[DEPRECATED]** macOS 10.0–10.5, 32-bit only. Could not disambiguate by user data. |

## A.3 Parameters (H→P)

| Function | Signature | Purpose |
|---|---|---|
| `AudioUnitGetParameter` | `OSStatus AudioUnitGetParameter(AudioUnit inUnit, AudioUnitParameterID inID, AudioUnitScope inScope, AudioUnitElement inElement, AudioUnitParameterValue *outValue)` — `CA_REALTIME_API` | Read one parameter value. |
| `AudioUnitSetParameter` | `OSStatus AudioUnitSetParameter(AudioUnit inUnit, AudioUnitParameterID inID, AudioUnitScope inScope, AudioUnitElement inElement, AudioUnitParameterValue inValue, UInt32 inBufferOffsetInFrames)` — `CA_REALTIME_API` | Set one parameter. `inBufferOffsetInFrames` should generally be 0 — see `AudioUnitScheduleParameters`. |
| `AudioUnitScheduleParameters` | `OSStatus AudioUnitScheduleParameters(AudioUnit inUnit, const AudioUnitParameterEvent *inParameterEvent, UInt32 inNumParamEvents)` — `CA_REALTIME_API` | Sample-accurate immediate **and ramped** parameter events. All events must apply to the current render call — schedule them from the **pre-render notification** callback. |

Parameter IDs are consistent across all elements of a scope: a mixer's "input volume" applies to any input, selected by element ID.

## A.4 Rendering (H→P)

| Function | Signature | Purpose |
|---|---|---|
| `AudioUnitRender` | `OSStatus AudioUnitRender(AudioUnit inUnit, AudioUnitRenderActionFlags * __nullable ioActionFlags, const AudioTimeStamp *inTimeStamp, UInt32 inOutputBusNumber, UInt32 inNumberFrames, AudioBufferList *ioData)` — `CA_REALTIME_API` | The main pull-model render call. Host supplies a timestamp whose **sample time must increment sequentially** by `inNumberFrames`; a discontinuity signals a timeline break to the unit. `ioData->mData` may be non-null (unit renders into host buffers, 16-byte aligned) or null (unit hands back its own buffers, valid for the calling thread's I/O cycle). |
| `AudioUnitProcess` | `OSStatus AudioUnitProcess(AudioUnit inUnit, AudioUnitRenderActionFlags * __nullable ioActionFlags, const AudioTimeStamp *inTimeStamp, UInt32 inNumberFrames, AudioBufferList *ioData)` — `CA_REALTIME_API`, macOS 10.7+ | Push-model in-place processing for effects: one buffer list in and out, no bus number, no input callback needed. |
| `AudioUnitProcessMultiple` | `OSStatus AudioUnitProcessMultiple(AudioUnit inUnit, AudioUnitRenderActionFlags * __nullable ioActionFlags, const AudioTimeStamp *inTimeStamp, UInt32 inNumberFrames, UInt32 inNumberInputBufferLists, const AudioBufferList * __nonnull * __nonnull inInputBufferLists, UInt32 inNumberOutputBufferLists, AudioBufferList * __nonnull * __nonnull ioOutputBufferLists)` — `CA_REALTIME_API`, macOS 10.7+ | Push-model with N input and M output buffer lists — the multi-bus form of `AudioUnitProcess`. |
| `AudioUnitReset` | `OSStatus AudioUnitReset(AudioUnit inUnit, AudioUnitScope inScope, AudioUnitElement inElement)` | Clear delay lines / internal DSP state. **Must only clear memory — never allocate or free** (that belongs in Initialize/Uninitialize). Call before re-inserting a unit into an active render chain. Typically Global scope, element 0. |
| `AudioUnitAddRenderNotify` | `OSStatus AudioUnitAddRenderNotify(AudioUnit inUnit, AURenderCallback inProc, void * __nullable inProcUserData)` | Register a pre/post render notification. Called **twice** per render: once with `kAudioUnitRenderAction_PreRender` set, once with `PostRender`. On post-render, `ioData` holds the rendered audio. |
| `AudioUnitRemoveRenderNotify` | `OSStatus AudioUnitRemoveRenderNotify(AudioUnit inUnit, AURenderCallback inProc, void * __nullable inProcUserData)` | Unregister. `(inProc, inProcUserData)` is again a tuple. |

## A.5 Output-unit control — `AudioOutputUnit.h`

The entire header is two functions:

| Function | Signature | Purpose |
|---|---|---|
| `AudioOutputUnitStart` | `OSStatus AudioOutputUnitStart(AudioUnit ci)` | Start the output unit's I/O thread pulling the graph. |
| `AudioOutputUnitStop` | `OSStatus AudioOutputUnitStop(AudioUnit ci)` | Stop it. |

Related output-unit calls declared in `AUComponent.h` (Inter-App Audio, iOS-flavoured, gated on `AU_SUPPORT_INTERAPP_AUDIO`):

| Function | Signature | Purpose |
|---|---|---|
| `AudioOutputUnitPublish` | `OSStatus AudioOutputUnitPublish(const AudioComponentDescription *inDesc, CFStringRef inName, UInt32 inVersion, AudioUnit inOutputUnit)` | Publish a node app's output unit so hosts can find it (Inter-App Audio). |
| `AudioOutputUnitGetHostIcon` | `UIImage * AudioOutputUnitGetHostIcon(AudioUnit au, float desiredPointSize)` | **[DEPRECATED]** iOS 7.0–14.0; use `AudioComponentCopyIcon`. |
| `AudioComponentGetLastActiveTime` | `CFAbsoluteTime AudioComponentGetLastActiveTime(AudioComponent comp)` | Last time an Inter-App Audio node app was active. |

## A.6 MusicDevice — instrument & music-effect calls — `MusicDevice.h`

`typedef AudioComponentInstance MusicDeviceComponent;`

| Function | Signature | Purpose |
|---|---|---|
| `MusicDeviceMIDIEvent` | `OSStatus MusicDeviceMIDIEvent(MusicDeviceComponent inUnit, UInt32 inStatus, UInt32 inData1, UInt32 inData2, UInt32 inOffsetSampleFrame)` — `CA_REALTIME_API` | Send one channel-voice MIDI 1.0 message. `inStatus` is the full status byte (command \| channel). `inOffsetSampleFrame` gives sample-accurate placement **only when called from the unit's render thread**; otherwise pass 0. |
| `MusicDeviceSysEx` | `OSStatus MusicDeviceSysEx(MusicDeviceComponent inUnit, const UInt8 *inData, UInt32 inLength)` — `CA_REALTIME_API` | Send any non-channel MIDI event (SysEx and system messages). No sample offset. |
| `MusicDeviceMIDIEventList` | `OSStatus MusicDeviceMIDIEventList(MusicDeviceComponent inUnit, UInt32 inOffsetSampleFrame, const struct MIDIEventList *evtList)` — `CA_REALTIME_API`, macOS 12+ | **MIDI 2.0 / UMP path.** Sends Universal MIDI Packets. Messages must be full non-SysEx events, partial SysEx, or complete SysEx; running status is disallowed. Events are delivered in the protocol reported by `kAudioUnitProperty_AudioUnitMIDIProtocol`. |
| `MusicDeviceStartNote` | `OSStatus MusicDeviceStartNote(MusicDeviceComponent inUnit, MusicDeviceInstrumentID inInstrument, MusicDeviceGroupID inGroupID, NoteInstanceID *outNoteInstanceID, UInt32 inOffsetSampleFrame, const MusicDeviceNoteParams *inParams)` — `CA_REALTIME_API` | Extended note API. Returns a `NoteInstanceID` token — required because fractional pitches make a MIDI note number insufficient to identify a note. Gate on `kMusicDeviceProperty_SupportsStartStopNote`. |
| `MusicDeviceStopNote` | `OSStatus MusicDeviceStopNote(MusicDeviceComponent inUnit, MusicDeviceGroupID inGroupID, NoteInstanceID inNoteInstanceID, UInt32 inOffsetSampleFrame)` — `CA_REALTIME_API` | Stop a note started by `MusicDeviceStartNote`. |
| `MusicDevicePrepareInstrument` | `OSStatus MusicDevicePrepareInstrument(MusicDeviceComponent inUnit, MusicDeviceInstrumentID inInstrument)` | **[DEPRECATED]** macOS 10.0–10.5. Multitimbral synths use Part scopes now. |
| `MusicDeviceReleaseInstrument` | `OSStatus MusicDeviceReleaseInstrument(MusicDeviceComponent inUnit, MusicDeviceInstrumentID inInstrument)` | **[DEPRECATED]** macOS 10.0–10.5. |

> **Pairing rule (from the header):** a note must be stopped by the *same API family* that started it. MIDI note-on → MIDI note-off; `MusicDeviceStartNote` → `MusicDeviceStopNote`. Mixing them is undefined.

**MusicDevice selectors** (usable with `AudioComponentInstanceCanDo`):

```c
kMusicDeviceRange                    = 0x0100,
kMusicDeviceMIDIEventSelect          = 0x0101,
kMusicDeviceSysExSelect              = 0x0102,
kMusicDevicePrepareInstrumentSelect  = 0x0103,
kMusicDeviceReleaseInstrumentSelect  = 0x0104,
kMusicDeviceStartNoteSelect          = 0x0105,
kMusicDeviceStopNoteSelect           = 0x0106,
kMusicDeviceMIDIEventListSelect      = 0x0107
```

## A.7 AUv3 component-list management

| Function | Signature | Purpose |
|---|---|---|
| `AudioUnitExtensionSetComponentList` | `OSStatus AudioUnitExtensionSetComponentList(CFStringRef extensionIdentifier, __nullable CFArrayRef audioComponentInfo)` | macOS 10.13+/iOS 11+ — an extension dynamically declares its component list. |
| `AudioUnitExtensionCopyComponentList` | `__nullable CFArrayRef AudioUnitExtensionCopyComponentList(CFStringRef extensionIdentifier)` | Read that list back. |

---

# B. Property constants

Every property constant in `AudioUnitProperties.h`, grouped by duty. Scope / Value Type / Access are Apple's own doc-comment fields.

## B.1 General / audio-unit-wide

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_ClassInfo` | 0 | Global (or Part) | `CFDictionaryRef` | Read / Write | The complete state of an audio unit. The core preset/save mechanism. |
| `kAudioUnitProperty_CPULoad` | 6 | Global | `Float64` | Read | Duty cycle (0–1) of render time spent in the unit's render call. |
| `kAudioUnitProperty_ElementCount` | 11 | Any (Global always 1) | `UInt32` | Read / Write | Number of input/output elements (buses). Writable on units that can add/remove buses. |
| `kAudioUnitProperty_Latency` | 12 | Global | `Float64` | Read | Processing latency in **seconds** (time to represent an input in the output). |
| `kAudioUnitProperty_TailTime` | 20 | Global | `Float64` | Read | Seconds remaining after the last valid input before output is silent. |
| `kAudioUnitProperty_MaximumFramesPerSlice` | 14 | Global | `UInt32` | Read / Write | Max frames the unit will ever be asked to produce in one `AudioUnitRender`. **Set before Initialize.** |
| `kAudioUnitProperty_LastRenderError` | 22 | Global | `OSStatus` | Read | The error from a failed render, retrievable by a listener. |
| `kAudioUnitProperty_LastRenderSampleTime` | 61 | Global | `Float64` | read-only | Absolute sample frame time of the most recent render timestamp. |
| `kAudioUnitProperty_BypassEffect` | 21 | Global | `UInt32` | Read / Write | Boolean bypass: input passes unchanged to output. |
| `kAudioUnitProperty_RenderQuality` | 26 | Global | `UInt32` | Read / Write | Quality/complexity of rendering, 0–127 (see `kRenderQuality_*`). |
| `kAudioUnitProperty_InPlaceProcessing` | 29 | Global | `UInt32` | Read / Write | Whether the unit can process input in place on the provided buffers. |
| `kAudioUnitProperty_OfflineRender` | 37 | Global | `UInt32` | Read / Write | Tells the unit it is rendering in a non-real-time (offline/bounce) context. |
| `kAudioUnitProperty_ElementName` | 30 | any | `CFStringRef` | read/write | Name of the specified element. Host owns a reference and must release. |
| `kAudioUnitProperty_ContextName` | 25 | Global | `CFString` | Read / Write | Host tells the unit where it lives, e.g. `"track 3"`. |
| `kAudioUnitProperty_NickName` | 54 | Global | `CFStringRef` | read/write | Host sets a custom user-facing name on the unit. |
| `kAudioUnitProperty_SupportsMPE` | 58 | Global | `UInt32` | read | Whether the unit supports Multi-dimensional Polyphonic Expression. |
| `kAudioUnitProperty_LoadedOutOfProcess` | 62 | Global | `UInt32` | read-only | Whether this AU is loaded out-of-process. |
| `kAudioUnitProperty_FastDispatch` | 5 (macOS only) | Global | `void*` (function pointer) | Read | Retrieve a direct function pointer for a selector, bypassing Component Manager dispatch overhead. |
| `kAudioUnitProperty_RenderContextObserver` | 60 | Global | `AURenderContextObserver` | read-only | A block the OS calls when the render context (workgroup) changes — for AUs with auxiliary realtime threads. Swift-unavailable. |

## B.2 I/O, format & channel layout

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_StreamFormat` | 8 | Input / Output | `AudioStreamBasicDescription` | Read / Write | The basic format of an audio data path. The central negotiation property. |
| `kAudioUnitProperty_SampleRate` | 2 | Input / Output | `Float64` | Read / Write | Sample rate of an I/O element. |
| `kAudioUnitProperty_SupportedNumChannels` | 13 | Global | `AUChannelInfo` array | Read | Array of supported in/out channel configurations. |
| `kAudioUnitProperty_AudioChannelLayout` | 19 | Input/Output | `AudioChannelLayout` | read/write | Order of channels within a stream. |
| `kAudioUnitProperty_SupportedChannelLayoutTags` | 32 | Input/Output | `AudioChannelLayoutTag[]` | read only | Which channel-layout tags the unit understands. |
| `kAudioUnitProperty_ShouldAllocateBuffer` | 51 | input/output elements | `UInt32` | read/write | Whether the element allocates its own render buffer (default true). Set false to render into host buffers. |
| `kAudioUnitProperty_SetExternalBuffer` | 15 (macOS only) | Global | `AudioUnitExternalBuffer` | Write | Hand the unit a buffer to use with its input render callback's buffer list. |
| `kAudioUnitProperty_PresentationLatency` | 40 | Input/Output | `Float64` | write | Host tells the unit the presentation latency (seconds) of its input/output audio. |
| `kAudioUnitProperty_InputAnchorTimeStamp` | 3016 | Input | `AudioTimeStamp` | Read / Write | Fetch/restore an input's anchor timestamp so its timeline stays continuous when moved between mixers. Cannot be accessed while rendering. |

## B.3 Connection & rendering

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_MakeConnection` | 1 | Input | `AudioUnitConnection` | Write | Connect a source unit's output to this unit's input (the AUGraph-style direct connection). |
| `kAudioUnitProperty_SetRenderCallback` | 23 | Input | `AURenderCallbackStruct` | Write | **P→H.** Supply input on an element via a host callback instead of a connection. |
| `kAudioUnitProperty_HostCallbacks` | 27 | Global | `HostCallbackInfo` | Write | **P→H.** Host-supplied beat/tempo/transport callbacks the AU may call during render. |
| `kAudioUnitProperty_InputSamplesInOutput` | 49 | Global | `AUInputSamplesInOutputCallbackStruct` | read/write | **P→H.** End-of-render callback mapping which input samples produced which output samples (varispeed/time-pitch). |
| `kAudioUnitProperty_FrequencyResponse` | 52 | input/output elements | `AudioUnitFrequencyResponseBin` | read | Points for a UI to draw the unit's frequency response. |

## B.4 Parameters

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_ParameterList` | 3 | Any | `AudioUnitParameterID[]` | Read | The list of parameter IDs on a scope. The enumeration entry point. |
| `kAudioUnitProperty_ParameterInfo` | 4 | Any (Element = the parameter ID) | `AudioUnitParameterInfo` | Read | Name, unit, min/max/default, flags for one parameter. |
| `kAudioUnitProperty_ParameterValueStrings` | 16 | Any (Element = parameter ID) | `CFArrayRef` | Read | CFStrings naming each value of an indexed parameter — build a menu from this. |
| `kAudioUnitProperty_ParameterStringFromValue` | 33 | any | `AudioUnitParameterStringFromValue` | read | Display string for a parameter value. Use when `kAudioUnitParameterFlag_ValuesHaveStrings` is set. |
| `kAudioUnitProperty_ParameterValueFromString` | 38 | any | `AudioUnitParameterValueFromString` | read | Inverse: parse a value from its string representation. |
| `kAudioUnitProperty_ParameterIDName` | 34 | any | `AudioUnitParameterIDName` | read | A truncated parameter name of a host-suggested length. |
| `kAudioUnitProperty_ParameterClumpName` | 35 | any | `AudioUnitParameterIDName` | read | Same, but `inID` is a clump ID from `AudioUnitParameterInfo.clumpID`. |
| `kAudioUnitProperty_DependentParameters` | 45 | any | `AUDependentParameter[]` | read | Which parameters depend on a meta-parameter (`IsGlobalMeta`/`IsElementMeta`). |
| `kAudioUnitProperty_ParameterHistoryInfo` | 53 | Global | `AudioUnitParameterHistoryInfo` | read | For `PlotHistory` parameters: recommended update rate and history duration. |
| `kAudioUnitProperty_ParametersForOverview` | 57 | Global | `AudioUnitParameter[]` (variable) | read | The N most important parameters, for a compact host overview strip. |

## B.5 Presets & state

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_ClassInfo` | 0 | Global / Part | `CFDictionaryRef` | Read / Write | Full serialized state (also listed above). |
| `kAudioUnitProperty_ClassInfoFromDocument` | 50 | Global | `CFDictionary` | read/write | Restore state from a **document** rather than a user preset. Set *before* `ClassInfo`. |
| `kAudioUnitProperty_FactoryPresets` | 24 | Global | `CFArray` of `AUPreset` | Read | Name + number for each factory preset. |
| `kAudioUnitProperty_PresentPreset` | 36 | Global/Part | `AUPreset` | read/write | The current preset. Replaces the deprecated `CurrentPreset`. Client owns the returned CFString. |
| `kAudioUnitProperty_CurrentPreset` | 28 | — | — | — | **[DEPRECATED]** Use `PresentPreset` — its CFString ownership was ill-defined. |

**ClassInfo dictionary keys** (`#define`, C string literals):

| Key macro | String | Notes |
|---|---|---|
| `kAUPresetVersionKey` | `"version"` | |
| `kAUPresetTypeKey` | `"type"` | componentType |
| `kAUPresetSubtypeKey` | `"subtype"` | componentSubType |
| `kAUPresetManufacturerKey` | `"manufacturer"` | componentManufacturer |
| `kAUPresetDataKey` | `"data"` | opaque plugin blob |
| `kAUPresetNameKey` | `"name"` | |
| `kAUPresetNumberKey` | `"preset-number"` | |
| `kAUPresetRenderQualityKey` | `"render-quality"` | |
| `kAUPresetCPULoadKey` | `"cpu-load"` | |
| `kAUPresetElementNameKey` | `"element-name"` | |
| `kAUPresetExternalFileRefs` | `"file-references"` | |
| `kAUPresetVSTDataKey` | `"vstdata"` | macOS only |
| `kAUPresetVSTPresetKey` | `"vstpreset"` | macOS only |
| `kAUPresetMASDataKey` | `"masdata"` | macOS only |
| `kAUPresetPartKey` | `"part"` | macOS only |

## B.6 UI / view

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_CocoaUI` | 31 (macOS only) | Global | `AudioUnitCocoaViewInfo` | read | **The AUv2 custom-editor path.** Publishes the bundle URL + principal class name of the plugin's Cocoa `NSView` factory. |
| `kAudioUnitProperty_GetUIComponentList` | 18 (macOS only) | Any | `AudioComponentDescription[]` | Read | Legacy Carbon (`'auvw'`) view components. |
| `kAudioUnitProperty_IconLocation` | 39 (macOS only) | Global | `CFURLRef` | Read | URL of an icon file for host UI. |
| `kAudioUnitProperty_RequestViewController` | 56 | Global | `void (^)(AUViewControllerBase *)` | write | AUv3 view-controller request. Copy rule applies to the block. |

## B.7 Host information (P→H direction properties)

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_HostCallbacks` | 27 | Global | `HostCallbackInfo` | Write | Beat/tempo/transport callbacks. See §C. |
| `kAudioUnitProperty_AUHostIdentifier` | 46 (macOS only) | Global | `AUHostVersionIdentifier` | write | Which host application + version is hosting the unit. |
| `kAudioUnitProperty_ContextName` | 25 | Global | `CFString` | Read / Write | The unit's context, e.g. track name. |

## B.8 MIDI & music-device

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_MIDIOutputCallbackInfo` | 47 | Global | `CFArrayRef` | read | How many MIDI output streams the AU generates, and each one's name. |
| `kAudioUnitProperty_MIDIOutputCallback` | 48 | Global | `AUMIDIOutputCallbackStruct` | write | **P→H.** Host callback for the AU to send MIDI 1.0 (`MIDIPacketList`) to the host. |
| `kAudioUnitProperty_MIDIOutputEventListCallback` | 63 | Global | `AUMIDIEventListBlock` | write | **P→H.** MIDI 2.0 / UMP equivalent — the AU sends `MIDIEventList` data during render. |
| `kAudioUnitProperty_AudioUnitMIDIProtocol` | 64 | Global | `SInt32` | read | The AU's MIDI protocol (a `MIDIProtocolID`). |
| `kAudioUnitProperty_HostMIDIProtocol` | 65 | Global | `SInt32` | write | The **host's** MIDI protocol, told to the AU. |
| `kAudioUnitProperty_MIDIOutputBufferSizeHint` | 66 | Global | `UInt32` | read/write | Plugin hint about its outgoing MIDI buffer size. |
| `kMusicDeviceProperty_MIDIXMLNames` | 1006 | — | — | — | MIDI XML name document for the instrument. |
| `kMusicDeviceProperty_PartGroup` | 1010 | — | — | — | Part→group association for multitimbral instruments. |
| `kMusicDeviceProperty_DualSchedulingMode` | 1013 | — | — | — | Lets the host reinterpret `inOffsetSampleFrame` as a bitfield distinguishing realtime vs scheduled events (see `kMusicDeviceSampleFrameMask_*`). |
| `kMusicDeviceProperty_SupportsStartStopNote` | 1014 | Global | `UInt32` | read | Whether the AU implements `MusicDeviceStartNote`/`StopNote` compliantly. **Gate the extended note API on this.** |

### MIDI parameter-mapping (macOS only)

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_AllParameterMIDIMappings` | 41 | any | `AUParameterMIDIMapping[]` | read/write | Get/set the whole parameter↔MIDI mapping state. |
| `kAudioUnitProperty_AddParameterMIDIMapping` | 42 | any | `AUParameterMIDIMapping[]` | write | Add mappings to the existing set. |
| `kAudioUnitProperty_RemoveParameterMIDIMapping` | 43 | any | `AUParameterMIDIMapping[]` | write | Remove mappings (matched by Scope/Element/ParameterID). |
| `kAudioUnitProperty_HotMapParameterMIDIMapping` | 44 | any | `AUParameterMIDIMapping` | read/write | MIDI-learn: map the next received MIDI message to this parameter. Set NULL to cancel a pending learn. |
| `kAudioUnitProperty_MIDIControlMapping` | 17 | — | — | — | **[DEPRECATED]** macOS 10.2. Superseded by the four above. |

## B.9 Offline units

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitOfflineProperty_InputSize` | 3020 | Global | `UInt64` | read/write | How many samples to process. **Setting it resets the unit's DSP state.** |
| `kAudioUnitOfflineProperty_OutputSize` | 3021 | Global | `UInt64` | read | Estimated output samples — a guide (e.g. progress bar) only. |
| `kAudioUnitOfflineProperty_StartOffset` | 3022 | Global | `UInt64` | read/write | The start offset of the data being processed changed. |
| `kAudioUnitOfflineProperty_PreflightRequirements` | 3023 | Global | `UInt32` | read | One of `kOfflinePreflight_NotRequired` / `_Optional` / `_Required`. |
| `kAudioUnitOfflineProperty_PreflightName` | 3024 | Global | `CFStringRef` | read | Human-readable name for the preflight operation. |
| `kAudioOfflineUnitProperty_InputSize` | = `kAudioUnitOfflineProperty_InputSize` | — | — | — | **[DEPRECATED]** old alias. |
| `kAudioOfflineUnitProperty_OutputSize` | = `kAudioUnitOfflineProperty_OutputSize` | — | — | — | **[DEPRECATED]** old alias. |

## B.10 Inter-App Audio (`AU_SUPPORT_INTERAPP_AUDIO`)

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_RemoteControlEventListener` | 100 | Global | `AudioUnitRemoteControlEventListener` | read/write | Host receives remote-control events from a node AU. |
| `kAudioUnitProperty_IsInterAppConnected` | 101 | Global | `UInt32` (0-1) | read-only | Whether the AU is connected to another app. |
| `kAudioUnitProperty_PeerURL` | 102 | Global | `CFURLRef` | read-only | URL that activates the peer app. |

## B.11 Output / device units (AUHAL)

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioOutputUnitProperty_CurrentDevice` | 2000 | Global | `AudioObjectID` | read/write | Which audio device the output unit uses. |
| `kAudioOutputUnitProperty_IsRunning` | 2001 | Global | `UInt32` | read-only | Whether the output unit is running. |
| `kAudioOutputUnitProperty_ChannelMap` | 2002 | Input/Output | `SInt32[]` | Read / Write | Map source channels to destination channels; `-1` silences a destination channel. Also works on AUConverter. |
| `kAudioOutputUnitProperty_EnableIO` | 2003 | {output, element 0} / {input, element 1} | `UInt32` | read/write | Enable/disable input or output operation. 0 = disabled, 1 = enabled. |
| `kAudioOutputUnitProperty_StartTime` | 2004 | Global | `AudioOutputUnitStartAtTimeParams` | write only | Make the *next* Start use `AudioDeviceStartAtTime` at a given timestamp. |
| `kAudioOutputUnitProperty_SetInputCallback` | 2005 | Global | `AURenderCallbackStruct` | read/write | **P→H.** Notifies the host that input is available; host then calls `AudioUnitRender` to fetch it. |
| `kAudioOutputUnitProperty_HasIO` | 2006 | {output, element 0} / {input, element 1} | `UInt32` | — | 1 if there are valid hardware streams on that element. |
| `kAudioOutputUnitProperty_StartTimestampsAtZero` | 2007 | Global | `UInt32` | read/write | If false, sample times reflect the HAL's rather than starting at 0. Also applies to AUConverter. |
| `kAudioOutputUnitProperty_OSWorkgroup` | 2015 | Global | `os_workgroup_t` | read-only | The realtime OS workgroup for this I/O unit. Returned **+1 retained** — caller must release. |
| `kAudioOutputUnitProperty_IntendedSpatialExperience` | 2016 | Global | `CASpatialAudioExperience*` | read/write | Spatial-experience override; default is `CAAutomaticSpatialAudio`. |
| `kAudioOutputUnitProperty_MIDICallbacks` | 2010 | Global | `AudioOutputUnitMIDICallbacks` | read/write | IAA: receive MIDI from the host at render time. |
| `kAudioOutputUnitProperty_HostReceivesRemoteControlEvents` | 2011 | Global | `UInt32` | read-only | IAA: whether the connected host receives remote-control events. |
| `kAudioOutputUnitProperty_RemoteControlToHost` | 2012 | Global | `AudioUnitRemoteControlEvent` | write-only | IAA: node app sends a remote-control event to the host. |
| `kAudioOutputUnitProperty_HostTransportState` | 2013 | Global | `UInt32` (dummy, always 0) | listener only | IAA: the host's transport state changed — refresh UI. |
| `kAudioOutputUnitProperty_NodeComponentDescription` | 2014 | Global | `AudioComponentDescription` | read-only | IAA: the description the host used to connect to this node. |

## B.12 Apple-specific: converters, mixers, spatial

### AUConverter
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_SampleRateConverterComplexity` | 3014 | Global | `UInt32` | read/write | SRC algorithm quality: `'line'` / `'norm'` / `'bats'`. |
| `kAudioUnitProperty_SRCAlgorithm` | 9 | — | — | — | **[DEPRECATED]** legacy (`'poly'`, `'csrc'`); use the above. |

### Mixers
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_MeteringMode` | 3007 | { scope / element } | `UInt32` | read/write | Enable/disable metering on a scope/element. |
| `kAudioUnitProperty_MatrixLevels` | 3006 | Global (AUMatrixMixer) / Input (AUMultiChannelMixer) | `Float32[]` | read/write | The whole crosspoint/volume state of a matrix mixer. |
| `kAudioUnitProperty_MatrixDimensions` | 3009 | Global | 2 × `UInt32` | Read only | Total input and output channel counts of a matrix mixer. |
| `kAudioUnitProperty_MeterClipping` | 3011 | Global | `AudioUnitMeterClipping` | Read | Peak since last call, plus infinity/NaN detection. |

### AUSpatialMixer
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_ReverbRoomType` | 10 | Global | `UInt32` | Read / Write | Internal reverb room type (`kReverbRoomType_*`). |
| `kAudioUnitProperty_UsesInternalReverb` | 1005 | Global | `UInt32` | Read / Write | Whether the unit uses its internal reverb. |
| `kAudioUnitProperty_SpatializationAlgorithm` | 3000 | Input | `UInt32` | Read / Write | Spatialisation algorithm per input (`kSpatializationAlgorithm_*`). |
| `kAudioUnitProperty_SpatialMixerRenderingFlags` | 3003 | Input | `UInt32` | Read / Write | Per-input rendering operations (`kSpatialMixerRenderingFlags_*`). |
| `kAudioUnitProperty_SpatialMixerSourceMode` | 3005 | Input | `UInt32` | Read / Write | How individual channels of an input bus render (`kSpatialMixerSourceMode_*`). |
| `kAudioUnitProperty_SpatialMixerDistanceParams` | 3010 | Input | `MixerDistanceParams` | Read / Write | Reference distance, max distance, max attenuation. |
| `kAudioUnitProperty_SpatialMixerAttenuationCurve` | 3013 | Input | `UInt32` | Read / Write | Distance attenuation curve. |
| `kAudioUnitProperty_SpatialMixerOutputType` | 3100 | Global | `UInt32` | Read / Write | Output hardware type for `kSpatializationAlgorithm_UseOutputType`. |
| `kAudioUnitProperty_SpatialMixerPointSourceInHeadMode` | 3103 | Input | `UInt32` | Read / Write | In-head rendering mode for point sources. |
| `kAudioUnitProperty_SpatialMixerEnableHeadTracking` | 3111 | Global | `UInt32` | Read / Write | AirPods motion-sensor head tracking. |
| `kAudioUnitProperty_SpatialMixerPersonalizedHRTFMode` | 3113 | Global | `UInt32` | Read / Write | Personalized HRTF mode (off/on/auto). |
| `kAudioUnitProperty_SpatialMixerAnyInputIsUsingPersonalizedHRTF` | 3116 | Global | `UInt32` | Read | Whether personalized HRTF is currently in use. |

### AUAudioMix
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAUAudioMixProperty_SpatialAudioMixMetadata` | 5000 | Global | `CFDataRef` | Read / Write | Remix metadata from the file asset. |
| `kAUAudioMixProperty_EnableSpatialization` | 5001 | Global | `UInt32` | Read / Write | 0 = FOA + mono foreground (default); 1 = render to mono/stereo/surround. |

### AUVoiceProcessing
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAUVoiceIOProperty_BypassVoiceProcessing` | 2100 | Global | `UInt32` | read/write | Bypass all mic-uplink processing (0 = default, processing active). |
| `kAUVoiceIOProperty_VoiceProcessingEnableAGC` | 2101 | Global | `UInt32` | read/write | Automatic gain control on the mic uplink; on by default. |
| `kAUVoiceIOProperty_MuteOutput` | 2104 | Global | `UInt32` | read/write | Mute the processed mic uplink. |
| `kAUVoiceIOProperty_MutedSpeechActivityEventListener` | 2106 | Global | `AUVoiceIOMutedSpeechActivityEventListener` | write only | Notifies when speech occurs while muted. |
| `kAUVoiceIOProperty_OtherAudioDuckingConfiguration` | 2108 | Global | `AUVoiceIOOtherAudioDuckingConfiguration` | read/write | Ducking of other audio: advanced enablement + level. |
| `kAUVoiceIOProperty_DuckNonVoiceAudio` | 2102 | Global | `UInt32` | read/write | **[DEPRECATED]** iOS 3.0–7.0. |
| `kAUVoiceIOProperty_VoiceProcessingQuality` | 2103 | Global | `UInt32` | read/write | **[DEPRECATED]** macOS 10.7–10.9 / iOS 3.0–7.0. Quality 0–127. |

Error: `kAUVoiceIOErr_UnexpectedNumberOfInputChannels = -66784` (macOS).

### AUNBandEQ
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAUNBandEQProperty_NumberOfBands` | 2200 | Global | `UInt32` | read/write | Number of EQ bands. **Settable only while uninitialized.** |
| `kAUNBandEQProperty_MaxNumberOfBands` | 2201 | Global | `UInt32` | read-only | Maximum bands. |
| `kAUNBandEQProperty_BiquadCoefficients` | 2203 | Global | `Float64[]` | read-only | 5 coefficients per band. |

### AUScheduledSoundPlayer / AUAudioFilePlayer
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_ScheduleAudioSlice` | 3300 | — | `ScheduledAudioSlice` | — | Schedule a slice of audio for sample-accurate future playback. |
| `kAudioUnitProperty_ScheduleStartTimeStamp` | 3301 | — | `AudioTimeStamp` | — | Sample time takes precedence; `-1` sample time (or host time 0) means "now". |
| `kAudioUnitProperty_CurrentPlayTime` | 3302 | — | `AudioTimeStamp` | — | Play position relative to start time; sample time `-1` if not started. |
| `kAudioUnitProperty_ScheduledFileIDs` | 3310 | — | `AudioFileID[]` | — | All files to be played — must be set on the file player. |
| `kAudioUnitProperty_ScheduledFileRegion` | 3311 | — | `ScheduledAudioFileRegion` | — | Schedule playback of a region of a file. |
| `kAudioUnitProperty_ScheduledFilePrime` | 3312 | — | `UInt32` | — | Frames to read from disk before returning; 0 = default. |
| `kAudioUnitProperty_ScheduledFileBufferSizeFrames` | 3313 | — | `UInt32` | — | Disk read buffer size. *(Header notes: currently unimplemented.)* |
| `kAudioUnitProperty_ScheduledFileNumberBuffers` | 3314 | — | `UInt32` | — | Number of disk read buffers. *(Header notes: currently unimplemented.)* |

### AUDeferredRenderer
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_DeferredRendererPullSize` | 3320 | — | `UInt32` | — | Constant buffer size at which the producer thread pulls upstream. |
| `kAudioUnitProperty_DeferredRendererExtraLatency` | 3321 | — | `UInt32` | — | Extra latency in frames beyond the one-pull-buffer minimum. |
| `kAudioUnitProperty_DeferredRendererWaitFrames` | 3322 | — | `UInt32` | — | If non-zero, Render sleeps this many frames of realtime for the producer. |

### DLSMusicDevice / AUMIDISynth / AUSampler
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kMusicDeviceProperty_InstrumentCount` | 1000 | Global | `UInt32` | Read | 0 for mono-timbral; otherwise the number of independent patches. |
| `kMusicDeviceProperty_InstrumentName` | 1001 | Global | `CFURLRef` | Read | Name of the instrument in use. |
| `kMusicDeviceProperty_InstrumentNumber` | 1004 | Global | `UInt32` | Read | Number of the instrument in use. |
| `kMusicDeviceProperty_BankName` | 1007 | Global | `CFStringRef` | Read | Name of the loaded bank. |
| `kMusicDeviceProperty_SoundBankData` | 1008 | — | — | — | Sound bank data (macOS). |
| `kMusicDeviceProperty_StreamFromDisk` | 1011 | — | — | — | Stream bank content from disk (macOS). |
| `kMusicDeviceProperty_SoundBankFSRef` | 1012 | — | — | — | FSRef of the sound bank file (macOS). |
| `kMusicDeviceProperty_SoundBankURL` | 1100 | Global | `CFURLRef` | Read (Read/Write on AUMIDISynth) | Currently-loaded bank file. |
| `kMusicDeviceProperty_UsesInternalReverb` | = `kAudioUnitProperty_UsesInternalReverb` | — | — | — | Alias for the DLSMusicDevice. |
| `kAUMIDISynthProperty_EnablePreload` | 4119 | Global | `UInt32` | Write | 1 = load instruments on program change. **Must be set back to 0 before playback.** |
| `kAUSamplerProperty_LoadInstrument` | 4102 | Global | `AUSamplerInstrumentData` | Write | Load an instrument from DLS/SF2/AUPreset/audio file/EXS24. |
| `kAUSamplerProperty_LoadAudioFiles` | 4101 | Global | `CFArrayRef` | Write | Build a new preset from an array of CFURLs. **Clears the previous preset entirely.** |
| `kAUSamplerProperty_LoadPresetFromBank` | 4100 | — | — | — | **[DEPRECATED]** Use `AUSamplerInstrumentData`. |
| `kAUSamplerProperty_BankAndPreset` | = above | — | — | — | **[DEPRECATED]** alias. |

### AUNetSend / AUNetReceive
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAUNetReceiveProperty_Hostname` | 3511 | Global | `CFStringRef` | — | Hostname to receive audio from (returned copy must be released). |
| `kAUNetReceiveProperty_Password` | 3512 | Global | `CFStringRef` | Read / Write | Password sent to the sender. |
| `kAUNetSendProperty_PortNum` | 3513 | Global | `UInt32` | Read / Write | Network port to send on. |
| `kAUNetSendProperty_TransmissionFormat` | 3514 | Global | `AudioStreamBasicDescription` | Read / Write | Arbitrary transmission format. |
| `kAUNetSendProperty_TransmissionFormatIndex` | 3515 | Global | `UInt32` | Read / Write | Index into the preset format list. |
| `kAUNetSendProperty_ServiceName` | 3516 | Global | `CFStringRef` | Read / Write | Published network service name. |
| `kAUNetSendProperty_Disconnect` | 3517 | Global | `UInt32` | Read / Write | Non-zero disconnects, zero connects. |
| `kAUNetSendProperty_Password` | 3518 | Global | `CFStringRef` | Read / Write | Password the receiver must use. |

### Panner (deprecated)
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_DistanceAttenuationData` | 3600 | Global | `AUDistanceAttenuationData` | Read | **[DEPRECATED]** macOS 10.5–10.11. |
| `kAudioUnitProperty_PannerMode` | 3008 | — | — | — | **[DEPRECATED]** |

### Translation / migration service
| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitMigrateProperty_FromPlugin` | 4000 | — | — | — | Migrate settings from MAS, VST, or an older AU into this unit. |
| `kAudioUnitMigrateProperty_OldAutomation` | 4001 | — | — | — | Translate another plugin's automation values into AU parameter values. |

### AUMixer3D (all deprecated — use AUSpatialMixer)
All carry `__OSX_AVAILABLE_BUT_DEPRECATED(__MAC_10_7, __MAC_10_11, __IPHONE_3_0, __IPHONE_9_0)`.

| Constant | Value | Scope | Value Type | Access | Purpose |
|---|---|---|---|---|---|
| `kAudioUnitProperty_3DMixerDistanceParams` | 3010 | — | — | — | **[DEPRECATED]** |
| `kAudioUnitProperty_3DMixerAttenuationCurve` | 3013 | — | — | — | **[DEPRECATED]** |
| `kAudioUnitProperty_DopplerShift` | 3002 | Input | `UInt32` | Write | **[DEPRECATED]** Boolean doppler enable. |
| `kAudioUnitProperty_3DMixerRenderingFlags` | 3003 | Input | `UInt32` | Read / Write | **[DEPRECATED]** |
| `kAudioUnitProperty_3DMixerDistanceAtten` | 3004 | — | — | — | **[DEPRECATED]** |
| `kAudioUnitProperty_ReverbPreset` | 3012 | — | — | — | **[DEPRECATED]** |

### Other deprecated
| Constant | Value | Purpose |
|---|---|---|
| `kAudioUnitProperty_ParameterValueName` | = `kAudioUnitProperty_ParameterStringFromValue` | **[DEPRECATED]** alias. |
| `kAudioUnitProperty_BusCount` | = `kAudioUnitProperty_ElementCount` | **[DEPRECATED]** alias. |
| `kAudioUnitProperty_SpeakerConfiguration` | 3001 | **[DEPRECATED]** Use `AudioChannelLayout`. |
| `kMusicDeviceProperty_GroupOutputBus` | 1002 | **[DEPRECATED]** |
| `kMusicDeviceProperty_SoundBankFSSpec` | 1003 | **[DEPRECATED]** |
| *(comment only)* `kAudioUnitProperty_SetInputCallback = 7` | 7 | Noted deprecated in the header comment; use `kAudioUnitProperty_SetRenderCallback` (23). |

### Configuration-info dictionary keys (`AudioComponentCopyConfigurationInfo`)

`kAudioUnitConfigurationInfo_HasCustomView` · `_ChannelConfigurations` · `_InitialInputs` · `_InitialOutputs` · `_IconURL` · `_BusCountWritable` · `_SupportedChannelLayoutTags` · `_MIDIProtocol` · `_MigrateFromPlugin` · `_AvailableArchitectures` (macOS only).

These let a host populate a plugin database **without instantiating** the unit.

---

# C. Host-callback surfaces (P→H)

These are what a host **must implement**. Each is installed by the host via `AudioUnitSetProperty` (or an `Add*` call), and then invoked by the plugin.

## C.1 Render input callback — `kAudioUnitProperty_SetRenderCallback` (23)

```c
typedef OSStatus
(*AURenderCallback)(void                        *inRefCon,
                    AudioUnitRenderActionFlags  *ioActionFlags,
                    const AudioTimeStamp        *inTimeStamp,
                    UInt32                       inBusNumber,
                    UInt32                       inNumberFrames,
                    AudioBufferList * __nullable ioData) CA_REALTIME_API;

struct AURenderCallbackStruct {
    AURenderCallback __nullable inputProc;
    void * __nullable           inputProcRefCon;
};
```

Set on **Input** scope, element = the input bus. The same prototype serves two roles:

1. **Input supply** (via property 23) — the plugin pulls input from the host.
2. **Render notification** (via `AudioUnitAddRenderNotify`) — the host observes pre/post render. Distinguish by testing `*ioActionFlags` for `kAudioUnitRenderAction_PreRender` / `PostRender`. `ioData` may be NULL in the notification case.

Buffers are 16-byte aligned. Set `kAudioUnitRenderAction_OutputIsSilence` when producing silence.

## C.2 Host transport/tempo callbacks — `kAudioUnitProperty_HostCallbacks` (27)

```c
struct HostCallbackInfo {
    void * __nullable                                hostUserData;
    HostCallback_GetBeatAndTempo        __nullable   beatAndTempoProc;
    HostCallback_GetMusicalTimeLocation __nullable   musicalTimeLocationProc;
    HostCallback_GetTransportState      __nullable   transportStateProc;
    HostCallback_GetTransportState2     __nullable   transportStateProc2;
};
```

Any member may be NULL; the plugin must tolerate that. All four are `CA_REALTIME_API` — they are called **from the render thread** and must be lock-free and allocation-free. Every out-parameter is `__nullable`: the plugin passes NULL for values it does not want, and the host must null-check each one before writing.

| Callback | Signature | Purpose |
|---|---|---|
| `HostCallback_GetBeatAndTempo` | `OSStatus (*)(void * __nullable inHostUserData, Float64 * __nullable outCurrentBeat, Float64 * __nullable outCurrentTempo)` | Current beat position and tempo (BPM). |
| `HostCallback_GetMusicalTimeLocation` | `OSStatus (*)(void * __nullable inHostUserData, UInt32 * __nullable outDeltaSampleOffsetToNextBeat, Float32 * __nullable outTimeSig_Numerator, UInt32 * __nullable outTimeSig_Denominator, Float64 * __nullable outCurrentMeasureDownBeat)` | Samples until the next beat, time signature, and the beat of the current measure's downbeat. |
| `HostCallback_GetTransportState` | `OSStatus (*)(void * __nullable inHostUserData, Boolean * __nullable outIsPlaying, Boolean * __nullable outTransportStateChanged, Float64 * __nullable outCurrentSampleInTimeLine, Boolean * __nullable outIsCycling, Float64 * __nullable outCycleStartBeat, Float64 * __nullable outCycleEndBeat)` | v1 transport state: playing, state-changed flag, timeline sample position, loop/cycle range. |
| `HostCallback_GetTransportState2` | `OSStatus (*)(void * __nullable inHostUserData, Boolean * __nullable outIsPlaying, Boolean * __nullable outIsRecording, Boolean * __nullable outTransportStateChanged, Float64 * __nullable outCurrentSampleInTimeLine, Boolean * __nullable outIsCycling, Float64 * __nullable outCycleStartBeat, Float64 * __nullable outCycleEndBeat)` | Same as v1 plus `outIsRecording`. |

> **Host implementation note:** v1 and v2 are *separate function pointers*, not a versioned union. A host that fills only `transportStateProc2` is invisible to any plugin that calls v1 only. Fill **both**, backing them with the same state.

## C.3 Property change notification

```c
typedef void
(*AudioUnitPropertyListenerProc)(void                *inRefCon,
                                 AudioUnit            inUnit,
                                 AudioUnitPropertyID  inID,
                                 AudioUnitScope       inScope,
                                 AudioUnitElement     inElement);
```

Registered via `AudioUnitAddPropertyListener`, removed via `AudioUnitRemovePropertyListenerWithUserData`. Note it carries no value — the listener must call `AudioUnitGetProperty` to read the new state. Not marked `CA_REALTIME_API`, but plugins do fire it from awkward contexts; treat it as untrusted-thread.

The properties a host most commonly listens to: `kAudioUnitProperty_ParameterList` (parameter set changed), `_PresentPreset` (preset switched by the plugin's own UI), `_Latency` / `_TailTime` (PDC must be recomputed), `_StreamFormat`, `_LastRenderError`, `_ParameterInfo`.

## C.4 Parameter change notification — `AudioUnitUtilities.h`

The AUv2 parameter-notification mechanism lives in `AudioUnitUtilities.h`, layered over the property listener:

```c
typedef void (*AUParameterListenerProc)(void                       *inUserData,
                                        void                       *inObject,
                                        const AudioUnitParameter   *inParameter,
                                        AudioUnitParameterValue     inValue);

typedef void (*AUEventListenerProc)(void                 *inUserData,
                                    void                 *inObject,
                                    const AudioUnitEvent *inEvent,
                                    UInt64                inEventHostTime,
                                    AudioUnitParameterValue inParameterValue);
```

`AudioUnitEvent` wraps an `AudioUnitEventType` — the important members for a host recording automation are `kAudioUnitEvent_ParameterValueChange`, `kAudioUnitEvent_BeginParameterChangeGesture`, and `kAudioUnitEvent_EndParameterChangeGesture`. The **gesture** events are what tell a host that a user has grabbed/released a knob in the plugin's own editor — the touch/latch boundary for automation write. Without listening to these a host cannot distinguish a drag from a programmatic change.

## C.5 MIDI output from plugin to host

| Property | Struct / block | Notes |
|---|---|---|
| `kAudioUnitProperty_MIDIOutputCallback` (48) | `struct AUMIDIOutputCallbackStruct { AUMIDIOutputCallback midiOutputCallback; void * __nullable userData; }` where `typedef OSStatus (*AUMIDIOutputCallback)(void * __nullable userData, const AudioTimeStamp *timeStamp, UInt32 midiOutNum, const struct MIDIPacketList *pktlist) CA_REALTIME_API;` | MIDI 1.0 path. `midiOutNum` indexes the streams named by `kAudioUnitProperty_MIDIOutputCallbackInfo`. |
| `kAudioUnitProperty_MIDIOutputEventListCallback` (63) | `typedef OSStatus (^AUMIDIEventListBlock)(AUEventSampleTime eventSampleTime, uint8_t cable, const struct MIDIEventList *eventList) CA_REALTIME_API;` | MIDI 2.0 / UMP path. `typedef int64_t AUEventSampleTime;` |

Both are called **on the render thread**. Errors: `kAudioUnitErr_MIDIOutputBufferFull` (-66753).

## C.6 Input-samples-in-output callback — `kAudioUnitProperty_InputSamplesInOutput` (49)

```c
typedef void
(*AUInputSamplesInOutputCallback)(void                   *inRefCon,
                                  const AudioTimeStamp   *inOutputTimeStamp,
                                  Float64                 inInputSample,
                                  Float64                 inNumberInputSamples) CA_REALTIME_API;

struct AUInputSamplesInOutputCallbackStruct {
    AUInputSamplesInOutputCallback inputToOutputCallback;
    void * __nullable              userData;
};
```

For varispeed / time-pitch units, where the input→output sample mapping is not 1:1. Lets a host keep a playhead honest through a rate-changing unit.

## C.7 Output-unit input availability — `kAudioOutputUnitProperty_SetInputCallback` (2005)

Uses `AURenderCallbackStruct`, but with inverted semantics: the callback is a **notification that input is available**, not a request for data. The host responds by calling `AudioUnitRender` on the input element to fetch it. This is the AUHAL capture path.

## C.8 Other plugin→host directions

| Surface | Direction | Notes |
|---|---|---|
| `AURenderContextObserver` (property 60) | P→H block, called by OS | `typedef void (^AURenderContextObserver)(const AudioUnitRenderContext *context)`. `AudioUnitRenderContext { os_workgroup_t workgroup; uint32_t reserved[6]; }`. Notifies when the realtime render context/workgroup changes. |
| `AudioUnitRemoteControlEventListener` (property 100) | P→H block | `typedef void (^)(AudioUnitRemoteControlEvent event)`. IAA transport control: `TogglePlayPause` (1), `ToggleRecord` (2), `Rewind` (3). |
| `AUVoiceIOMutedSpeechActivityEventListener` (property 2106) | P→H block | `typedef void (^)(AUVoiceIOSpeechActivityEvent event)`. |
| `ScheduledAudioSliceCompletionProc` | P→H | `typedef void (*)(void * __nullable userData, ScheduledAudioSlice *bufferList) CA_REALTIME_API` — slice finished. |
| `ScheduledAudioFileRegionCompletionProc` | P→H | `typedef void (*)(void * __nullable userData, ScheduledAudioFileRegion *fileRegion, OSStatus result)`. |
| `AudioOutputUnitMIDICallbacks` (property 2010) | P→H | IAA: `MIDIEventProc` / `MIDISysExProc` function pointers with raw status/data bytes. |
| `kAudioComponentInstanceInvalidationNotification` | System→H | The plugin's extension process died. |

---

# D. Key enums and structs

## D.1 `AudioUnitScope`

`typedef UInt32 AudioUnitScope;` — Apple reserves `0 < 1024`.

```c
kAudioUnitScope_Global    = 0,
kAudioUnitScope_Input     = 1,
kAudioUnitScope_Output    = 2,
kAudioUnitScope_Group     = 3,
kAudioUnitScope_Part      = 4,
kAudioUnitScope_Note      = 5,
kAudioUnitScope_Layer     = 6,
kAudioUnitScope_LayerItem = 7
```

Companion typedefs: `AudioUnitPropertyID` (`UInt32`), `AudioUnitElement` (`UInt32`), `AudioUnitParameterID` (`UInt32`), `AudioUnitParameterValue` (`Float32`), `AudioUnit` = `AudioComponentInstance`.

A scope has one or more members; a member is addressed by its **element**. Input bus 1 is (Input scope, element 1).

## D.2 `AudioUnitParameterUnit` — every unit

```c
kAudioUnitParameterUnit_Generic             = 0,   // untyped value, generally 0..1
kAudioUnitParameterUnit_Indexed             = 1,   // discrete menu; see ParameterValueStrings
kAudioUnitParameterUnit_Boolean             = 2,   // 0 = false, > 0 = true
kAudioUnitParameterUnit_Percent             = 3,
kAudioUnitParameterUnit_Seconds             = 4,
kAudioUnitParameterUnit_SampleFrames        = 5,
kAudioUnitParameterUnit_Phase               = 6,   // degrees
kAudioUnitParameterUnit_Rate                = 7,   // multiplier
kAudioUnitParameterUnit_Hertz               = 8,
kAudioUnitParameterUnit_Cents               = 9,
kAudioUnitParameterUnit_RelativeSemiTones   = 10,
kAudioUnitParameterUnit_MIDINoteNumber      = 11,
kAudioUnitParameterUnit_MIDIController      = 12,
kAudioUnitParameterUnit_Decibels            = 13,
kAudioUnitParameterUnit_LinearGain          = 14,
kAudioUnitParameterUnit_Degrees             = 15,
kAudioUnitParameterUnit_EqualPowerCrossfade = 16,
kAudioUnitParameterUnit_MixerFaderCurve1    = 17,
kAudioUnitParameterUnit_Pan                 = 18,
kAudioUnitParameterUnit_Meters              = 19,
kAudioUnitParameterUnit_AbsoluteCents       = 20,
kAudioUnitParameterUnit_Octaves             = 21,
kAudioUnitParameterUnit_BPM                 = 22,
kAudioUnitParameterUnit_Beats               = 23,
kAudioUnitParameterUnit_Milliseconds        = 24,
kAudioUnitParameterUnit_Ratio               = 25,
kAudioUnitParameterUnit_CustomUnit          = 26,  // read unitName from AudioUnitParameterInfo
kAudioUnitParameterUnit_MIDI2Controller     = 27
```

## D.3 `AudioUnitParameterOptions` flags

```c
kAudioUnitParameterFlag_CFNameRelease      = (1UL << 4),

kAudioUnitParameterFlag_OmitFromPresets    = (1UL << 13),
kAudioUnitParameterFlag_PlotHistory        = (1UL << 14),
kAudioUnitParameterFlag_MeterReadOnly      = (1UL << 15),

// bits 18,17,16 are the display scale; bit 19 reserved; bit 22 joins the mask
kAudioUnitParameterFlag_DisplayMask        = (7UL << 16) | (1UL << 22),
kAudioUnitParameterFlag_DisplaySquareRoot  = (1UL << 16),
kAudioUnitParameterFlag_DisplaySquared     = (2UL << 16),
kAudioUnitParameterFlag_DisplayCubed       = (3UL << 16),
kAudioUnitParameterFlag_DisplayCubeRoot    = (4UL << 16),
kAudioUnitParameterFlag_DisplayExponential = (5UL << 16),

kAudioUnitParameterFlag_HasClump           = (1UL << 20),
kAudioUnitParameterFlag_ValuesHaveStrings  = (1UL << 21),
kAudioUnitParameterFlag_DisplayLogarithmic = (1UL << 22),
kAudioUnitParameterFlag_IsHighResolution   = (1UL << 23),
kAudioUnitParameterFlag_NonRealTime        = (1UL << 24),
kAudioUnitParameterFlag_CanRamp            = (1UL << 25),
kAudioUnitParameterFlag_ExpertMode         = (1UL << 26),
kAudioUnitParameterFlag_HasCFNameString    = (1UL << 27),
kAudioUnitParameterFlag_IsGlobalMeta       = (1UL << 28),
kAudioUnitParameterFlag_IsElementMeta      = (1UL << 29),
kAudioUnitParameterFlag_IsReadable         = (1UL << 30),
kAudioUnitParameterFlag_IsWritable         = (1UL << 31)
```

Aliases and helpers:
- `kAudioUnitParameterFlag_HasName = kAudioUnitParameterFlag_ValuesHaveStrings` (deprecated spelling).
- `GetAudioUnitParameterDisplayType(flags)` / `SetAudioUnitParameterDisplayType(flags, t)` — mask helpers.
- Predicates: `AudioUnitDisplayTypeIsLogarithmic/SquareRoot/Squared/Cubed/CubeRoot/Exponential(flags)`.
- **[DEPRECATED]** scope flags: `kAudioUnitParameterFlag_Global` (1<<0), `_Input` (1<<1), `_Output` (1<<2), `_Group` (1<<3).

> **Host-critical reading:** `IsReadable`/`IsWritable` gate whether a host may get/set at all. `CanRamp` gates `AudioUnitScheduleParameters` ramped events. `NonRealTime` means the parameter must not be changed during render. `IsHighResolution` means the value range exceeds MIDI 7-bit resolution. `MeterReadOnly` marks an output meter, not a control. `OmitFromPresets` means don't persist it.

## D.4 `AudioUnitRenderActionFlags`

```c
kAudioUnitRenderAction_PreRender            = (1UL << 2),
kAudioUnitRenderAction_PostRender           = (1UL << 3),
kAudioUnitRenderAction_OutputIsSilence      = (1UL << 4),
kAudioOfflineUnitRenderAction_Preflight     = (1UL << 5),
kAudioOfflineUnitRenderAction_Render        = (1UL << 6),
kAudioOfflineUnitRenderAction_Complete      = (1UL << 7),
kAudioUnitRenderAction_PostRenderError      = (1UL << 8),
kAudioUnitRenderAction_DoNotCheckRenderArgs = (1UL << 9)
```

- `PostRenderError` on a post-render notify means the render failed; `ioData` is invalid and the real error is in `kAudioUnitProperty_LastRenderError`.
- `DoNotCheckRenderArgs` skips argument validation for speed — only when the host is certain its arguments are correct.
- `OutputIsSilence` is a hint a host should propagate, not silently drop.

## D.5 `AudioUnitParameterEvent` and `AUParameterEventType`

```c
typedef CF_ENUM(UInt32, AUParameterEventType) {
    kParameterEvent_Immediate = 1,
    kParameterEvent_Ramped    = 2
};

struct AudioUnitParameterEvent {
    AudioUnitScope        scope;
    AudioUnitElement      element;
    AudioUnitParameterID  parameter;
    AUParameterEventType  eventType;
    union {
        struct {
            SInt32                  startBufferOffset;
            UInt32                  durationInFrames;
            AudioUnitParameterValue startValue;
            AudioUnitParameterValue endValue;
        } ramp;
        struct {
            UInt32                  bufferOffset;
            AudioUnitParameterValue value;
        } immediate;
    } eventValues;
};
```

Note `ramp.startBufferOffset` is **signed** — a ramp in progress from a previous block has a negative start offset. A ramp is re-scheduled each render for its whole duration, with the offset tracking progress.

## D.6 `AudioUnitParameterInfo`

```c
struct AudioUnitParameterInfo {
    char                       name[52];       // UNUSED - set to zero
    CFStringRef __nullable     unitName;       // valid only if unit == CustomUnit
    UInt32                     clumpID;        // valid only if HasClump flag set
    CFStringRef __nullable     cfNameString;   // valid only if HasCFNameString flag set
    AudioUnitParameterUnit     unit;
    AudioUnitParameterValue    minValue;
    AudioUnitParameterValue    maxValue;
    AudioUnitParameterValue    defaultValue;
    AudioUnitParameterOptions  flags;
};
```

> `name[52]` is documented **UNUSED — set to zero**. The real name is `cfNameString`, present only when `kAudioUnitParameterFlag_HasCFNameString` is set. A host that reads `name` gets an empty string from any modern plugin. When `kAudioUnitParameterFlag_CFNameRelease` is set, the host owns and must release `cfNameString`.

`kAudioUnitClumpID_System = 0` — clump 0 is reserved.

Related parameter-naming structs:

```c
enum { kAudioUnitParameterName_Full = -1 };   // pass as inDesiredLength for the untruncated name

struct AudioUnitParameterNameInfo {           // aka AudioUnitParameterIDName
    AudioUnitParameterID   inID;
    SInt32                 inDesiredLength;
    CFStringRef __nullable outName;
};

struct AudioUnitParameterStringFromValue {
    AudioUnitParameterID            inParamID;
    const AudioUnitParameterValue  *inValue;   // NULL = use current value
    CFStringRef __nullable          outString;
};

struct AudioUnitParameterValueFromString {
    AudioUnitParameterID     inParamID;
    CFStringRef              inString;
    AudioUnitParameterValue  outValue;
};
```

## D.7 `AudioUnitConnection`, `AUChannelInfo`, buffers

```c
struct AudioUnitConnection {
    AudioUnit __nullable sourceAudioUnit;
    UInt32               sourceOutputNumber;
    UInt32               destInputNumber;
};

struct AUChannelInfo {
    SInt16 inChannels;
    SInt16 outChannels;
};

struct AudioUnitExternalBuffer {
    Byte  *buffer;
    UInt32 size;
};
```

**`AUChannelInfo` wildcard conventions** (essential for correct format negotiation): a value of `-1` means "any number of channels", and `-2` means "any number, independent of the other side". The pair `{-1, -1}` means any matched count; `{-1, -2}` / `{-2, -1}` express independent in/out counts. A unit publishing no `kAudioUnitProperty_SupportedNumChannels` at all is conventionally treated as supporting any matched configuration.

## D.8 Latency, tail time & related

| Property | Type | Meaning |
|---|---|---|
| `kAudioUnitProperty_Latency` (12) | `Float64` **seconds** | Time for an input to be represented in the output. Multiply by sample rate for PDC frames. |
| `kAudioUnitProperty_TailTime` (20) | `Float64` **seconds** | Time after the last valid input before output goes silent. Governs bounce/render truncation. |
| `kAudioUnitProperty_PresentationLatency` (40) | `Float64` seconds, **write** | Host tells the unit the downstream/upstream latency of its audio. |
| `kAudioUnitProperty_MaximumFramesPerSlice` (14) | `UInt32` frames | The render-block ceiling. Must be set before `AudioUnitInitialize`. Exceeding it yields `kAudioUnitErr_TooManyFramesToProcess`. |

There is no `AudioUnitFrameCount` typedef in these headers — frame counts are plain `UInt32` (or `Float64` seconds for the latency/tail pair). `typedef int64_t AUEventSampleTime;` is the MIDI-event time type.

Both `Latency` and `TailTime` are dynamic — a host must listen for property changes on them and recompute PDC.

## D.9 `AudioComponentDescription` and component types

```c
#pragma pack(push, 4)
typedef struct AudioComponentDescription {
    OSType componentType;
    OSType componentSubType;
    OSType componentManufacturer;
    UInt32 componentFlags;
    UInt32 componentFlagsMask;
} AudioComponentDescription;
#pragma pack(pop)
```

> The `#pragma pack(4)` is load-bearing for any FFI binding.

### `kAudioUnitType_*`

| Constant | FourCC | Meaning |
|---|---|---|
| `kAudioUnitType_Output` | `'auou'` | Output/device unit (AUHAL, default, system, generic, VoiceProcessingIO). |
| `kAudioUnitType_MusicDevice` | `'aumu'` | Instrument: MIDI in, audio out. |
| `kAudioUnitType_MusicEffect` | `'aumf'` | Effect that also accepts MIDI. |
| `kAudioUnitType_FormatConverter` | `'aufc'` | Format/rate/time conversion. |
| `kAudioUnitType_Effect` | `'aufx'` | Audio in, audio out. |
| `kAudioUnitType_Mixer` | `'aumx'` | N in, M out. |
| `kAudioUnitType_Panner` | `'aupn'` | Panning/spatialisation. |
| `kAudioUnitType_Generator` | `'augn'` | Audio out, no audio in, no MIDI. |
| `kAudioUnitType_OfflineEffect` | `'auol'` | Non-realtime effect. |
| `kAudioUnitType_MIDIProcessor` | `'aumi'` | MIDI in, MIDI out, no audio. |
| `kAudioUnitType_SpeechSynthesizer` | `'ausp'` | macOS 13+/iOS 16+. |
| `kAudioUnitType_RemoteEffect` | `'aurx'` | Inter-App Audio. |
| `kAudioUnitType_RemoteGenerator` | `'aurg'` | Inter-App Audio. |
| `kAudioUnitType_RemoteInstrument` | `'auri'` | Inter-App Audio. |
| `kAudioUnitType_RemoteMusicEffect` | `'aurm'` | Inter-App Audio. |

`kAudioUnitManufacturer_Apple = 'appl'`.

### Standard Apple subtypes

**Output:** `GenericOutput` `'genr'` · `VoiceProcessingIO` `'vpio'` · `HALOutput` `'ahal'` (macOS) · `DefaultOutput` `'def '` (macOS) · `SystemOutput` `'sys '` (macOS) · `RemoteIO` `'rioc'` (iOS).

**Music devices:** `DLSSynth` `'dls '` · `Sampler` `'samp'` · `MIDISynth` `'msyn'`.

**Format converters:** `AUConverter` `'conv'` · `Varispeed` `'vari'` · `DeferredRenderer` `'defr'` · `Splitter` `'splt'` · `MultiSplitter` `'mspl'` · `Merger` `'merg'` · `NewTimePitch` `'nutp'` · `AUiPodTimeOther` `'ipto'` · `RoundTripAAC` `'raac'` · `AUAudioMix` `'amix'` (macOS/iOS 26+) · `TimePitch` `'tmpt'` (macOS) · `AUiPodTime` `'iptm'` **[DEPRECATED]** iOS 2.0–13.0.

**Effects:** `PeakLimiter` `'lmtr'` · `DynamicsProcessor` `'dcmp'` · `LowPassFilter` `'lpas'` · `HighPassFilter` `'hpas'` · `BandPassFilter` `'bpas'` · `HighShelfFilter` `'hshf'` · `LowShelfFilter` `'lshf'` · `ParametricEQ` `'pmeq'` · `Distortion` `'dist'` · `Delay` `'dely'` · `SampleDelay` `'sdly'` · `NBandEQ` `'nbeq'` · `Reverb2` `'rvb2'` · `AUSoundIsolation` `'vois'` (macOS 13+/iOS 16+). **macOS-only:** `GraphicEQ` `'greq'` · `MultiBandCompressor` `'mcmp'` · `MatrixReverb` `'mrev'` · `Pitch` `'tmpt'` · `AUFilter` `'filt'` · `NetSend` `'nsnd'` · `RogerBeep` `'rogr'`. **[DEPRECATED]** `AUiPodEQ` `'ipeq'` (iOS 2.0–13.0).

**Mixers:** `MultiChannelMixer` `'mcmx'` · `MatrixMixer` `'mxmx'` · `SpatialMixer` `'3dem'` · `StereoMixer` `'smxr'` (macOS) · `3DMixer` **[DEPRECATED]** macOS 10.3–10.10 · `AU3DMixerEmbedded` **[DEPRECATED]** renamed to `SpatialMixer`.

**Panners (macOS):** `SphericalHeadPanner` `'sphr'` · `VectorPanner` `'vbas'` · `SoundFieldPanner` `'ambi'` · `HRTFPanner` `'hrtf'`.

**Generators:** `ScheduledSoundPlayer` `'sspl'` · `AudioFilePlayer` `'afpl'` · `NetReceive` `'nrcv'` (macOS).

### Component flags

```c
typedef CF_OPTIONS(UInt32, AudioComponentFlags) {
    kAudioComponentFlag_Unsearchable               = 1,
    kAudioComponentFlag_SandboxSafe                = 2,
    kAudioComponentFlag_IsV3AudioUnit              = 4,
    kAudioComponentFlag_RequiresAsyncInstantiation = 8,
    kAudioComponentFlag_CanLoadInProcess           = 0x10
};

typedef CF_OPTIONS(UInt32, AudioComponentInstantiationOptions) {
    kAudioComponentInstantiation_LoadOutOfProcess = 1,
    kAudioComponentInstantiation_LoadInProcess    = 2,   // macOS only
    kAudioComponentInstantiation_LoadedRemotely   = 1u << 31
};
```

> **Host-critical:** `kAudioComponentFlag_IsV3AudioUnit` distinguishes a v3 unit. `kAudioComponentFlag_RequiresAsyncInstantiation` means `AudioComponentInstanceNew` is **invalid** — the host must use `AudioComponentInstantiate`. A host that only ever calls the synchronous form silently cannot load a large class of modern plugins.

Component plugin interface (what a plugin implements; a host only sees it indirectly):

```c
typedef OSStatus (*AudioComponentMethod)(void *self, ...);

struct AudioComponentPlugInInterface {
    OSStatus                        (*Open)(void *self, AudioComponentInstance mInstance);
    OSStatus                        (*Close)(void *self);
    AudioComponentMethod __nullable (* __nonnull Lookup)(SInt16 selector);
    void * __nullable               reserved;   // must be NULL
};

typedef AudioComponentPlugInInterface * __nullable
    (*AudioComponentFactoryFunction)(const AudioComponentDescription *inDesc);
```

`AudioComponentValidationResult`: `kAudioComponentValidationResult_Unknown` (0), `_Passed`, `_Failed`, `_TimedOut`, `_UnauthorizedError_Open`, `_UnauthorizedError_Init`.

## D.10 Error codes

### `kAudioUnitErr_*`

| Constant | Value | Meaning |
|---|---|---|
| `kAudioUnitErr_InvalidProperty` | -10879 | The property is not supported. |
| `kAudioUnitErr_InvalidParameter` | -10878 | The parameter is not supported. |
| `kAudioUnitErr_InvalidElement` | -10877 | The specified element is not valid. |
| `kAudioUnitErr_NoConnection` | -10876 | No connection or render callback on an input. |
| `kAudioUnitErr_FailedInitialization` | -10875 | Initialize failed (usually format/config the unit cannot accept). |
| `kAudioUnitErr_TooManyFramesToProcess` | -10874 | Render asked for more frames than `MaximumFramesPerSlice`. |
| `kAudioUnitErr_InvalidFile` | -10871 | Invalid file. |
| `kAudioUnitErr_UnknownFileType` | -10870 | Unknown file type. |
| `kAudioUnitErr_FileNotSpecified` | -10869 | File not specified. |
| `kAudioUnitErr_FormatNotSupported` | -10868 | The requested stream format is unsupported. |
| `kAudioUnitErr_Uninitialized` | -10867 | Operation requires an initialized unit. |
| `kAudioUnitErr_InvalidScope` | -10866 | The scope is not valid. |
| `kAudioUnitErr_PropertyNotWritable` | -10865 | Property is read-only. |
| `kAudioUnitErr_CannotDoInCurrentContext` | -10863 | Not allowed right now (e.g. while rendering). Often retryable. |
| `kAudioUnitErr_InvalidPropertyValue` | -10851 | Property value rejected. |
| `kAudioUnitErr_PropertyNotInUse` | -10850 | Property exists but is inactive. |
| `kAudioUnitErr_Initialized` | -10849 | Operation requires an **un**initialized unit. |
| `kAudioUnitErr_InvalidOfflineRender` | -10848 | Invalid offline render. |
| `kAudioUnitErr_Unauthorized` | -10847 | Not authorized (licensing/copy protection). |
| `kAudioUnitErr_MIDIOutputBufferFull` | -66753 | The unit's outgoing MIDI buffer overflowed. |
| `kAudioUnitErr_RenderTimeout` | -66745 | Render did not complete in time. |
| `kAudioUnitErr_ExtensionNotFound` | -66744 | The AUv3 extension could not be found. |
| `kAudioUnitErr_InvalidParameterValue` | -66743 | Parameter value out of range. |
| `kAudioUnitErr_InvalidFilePath` | -66742 | Invalid file path. |
| `kAudioUnitErr_MissingKey` | -66741 | A required dictionary key is missing. |
| `kAudioUnitErr_ComponentManagerNotSupported` | -66740 | Component Manager path unsupported. |
| `kAudioUnitErr_MultipleVoiceProcessors` | -66635 | More than one voice-processing unit. |

### `kAudioComponentErr_*`

| Constant | Value | Meaning |
|---|---|---|
| `kAudioComponentErr_InstanceTimedOut` | -66754 | The instance timed out. |
| `kAudioComponentErr_DuplicateDescription` | -66752 | A component with that description already exists. |
| `kAudioComponentErr_UnsupportedType` | -66751 | Unsupported component type. |
| `kAudioComponentErr_TooManyInstances` | -66750 | Instance limit reached. |
| `kAudioComponentErr_InstanceInvalidated` | -66749 | **The extension process died.** The instance is permanently dead — dispose and reinstantiate. |
| `kAudioComponentErr_NotPermitted` | -66748 | Not permitted. |
| `kAudioComponentErr_InitializationTimedOut` | -66747 | Initialization timed out. |
| `kAudioComponentErr_InvalidFormat` | -66746 | Invalid format. |

Voice-processing: `kAUVoiceIOErr_UnexpectedNumberOfInputChannels = -66784`.

> **Out-of-process reality:** `-66749`, `-66754`, `-66747`, `-66745` all mean "the plugin is in another process and something went wrong there". A host must treat these as distinct from a parameter/format error — the recovery is reinstantiation, not retry.

## D.11 Other supporting structs

```c
struct AUPreset {
    SInt32                 presetNumber;   // < 0 = user preset; >= 0 selects a factory preset
    CFStringRef __nullable presetName;
};

enum {   // render quality
    kRenderQuality_Max = 127, kRenderQuality_High = 96, kRenderQuality_Medium = 64,
    kRenderQuality_Low = 32,  kRenderQuality_Min  = 0
};

enum { kNumberOfResponseFrequencies = 1024 };

struct AudioUnitFrequencyResponseBin { Float64 mFrequency; Float64 mMagnitude; };

struct AUDependentParameter { AudioUnitScope mScope; AudioUnitParameterID mParameterID; };

struct AudioUnitCocoaViewInfo {          // macOS
    CFURLRef    mCocoaAUViewBundleLocation;
    CFStringRef mCocoaAUViewClass[1];    // variable length
};

struct AUHostVersionIdentifier { CFStringRef hostName; UInt32 hostVersion; };

struct AudioUnitParameterHistoryInfo { Float32 updatesPerSecond; Float32 historyDurationInSeconds; };

struct AudioUnitParameter {  // the (unit, param, scope, element) tuple
    AudioUnit mAudioUnit; AudioUnitParameterID mParameterID;
    AudioUnitScope mScope; AudioUnitElement mElement;
};

struct AudioUnitProperty {   // the (unit, property, scope, element) tuple
    AudioUnit mAudioUnit; AudioUnitPropertyID mPropertyID;
    AudioUnitScope mScope; AudioUnitElement mElement;
};

struct AudioUnitMeterClipping { Float32 peakValueSinceLastCall; Boolean sawInfinity; Boolean sawNotANumber; };

struct AudioOutputUnitStartAtTimeParams { AudioTimeStamp mTimestamp; UInt32 mFlags; };

struct MusicDeviceStdNoteParams { UInt32 argCount /* = 2 */; Float32 mPitch; Float32 mVelocity; };
struct NoteParamsControlValue   { AudioUnitParameterID mID; AudioUnitParameterValue mValue; };
struct MusicDeviceNoteParams {
    UInt32 argCount;                  // 2 + number of controls
    Float32 mPitch;                   // MIDI note number, fractional allowed: 60.5 = middle C + 50 cents
    Float32 mVelocity;                // fractional MIDI velocity, 0..128
    NoteParamsControlValue mControls[1];  // variable length
};

typedef UInt32 MusicDeviceInstrumentID;
typedef UInt32 MusicDeviceGroupID;      // MIDI channel equivalent
typedef UInt32 NoteInstanceID;

enum { kMusicNoteEvent_UseGroupInstrument = 0xFFFFFFFF, kMusicNoteEvent_Unused = 0xFFFFFFFF };

enum {   // DualSchedulingMode offset bitfield
    kMusicDeviceSampleFrameMask_SampleOffset = 0xFFFFFF,
    kMusicDeviceSampleFrameMask_IsScheduled  = 0x01000000
};

enum { kOfflinePreflight_NotRequired = 0, kOfflinePreflight_Optional = 1, kOfflinePreflight_Required = 2 };

struct AUParameterMIDIMapping {   // macOS
    AudioUnitScope mScope; AudioUnitElement mElement; AudioUnitParameterID mParameterID;
    AUParameterMIDIMappingFlags mFlags;
    AudioUnitParameterValue mSubRangeMin, mSubRangeMax;
    UInt8 mStatus, mData1;
    UInt8 reserved1, reserved2; UInt32 reserved3;   // MUST be zero
};

typedef CF_OPTIONS(UInt32, AUParameterMIDIMappingFlags) {
    kAUParameterMIDIMapping_AnyChannelFlag = (1L << 0),
    kAUParameterMIDIMapping_AnyNoteFlag    = (1L << 1),
    kAUParameterMIDIMapping_SubRange       = (1L << 2),
    kAUParameterMIDIMapping_Toggle         = (1L << 3),
    kAUParameterMIDIMapping_Bipolar        = (1L << 4),
    kAUParameterMIDIMapping_Bipolar_On     = (1L << 5)
};

struct AUSamplerInstrumentData {
    CFURLRef fileURL; UInt8 instrumentType, bankMSB, bankLSB, presetID;
};
enum { kInstrumentType_DLSPreset = 1, kInstrumentType_SF2Preset = 1,
       kInstrumentType_AUPreset = 2, kInstrumentType_Audiofile = 3, kInstrumentType_EXS24 = 4 };
enum { kAUSampler_DefaultPercussionBankMSB = 0x78,
       kAUSampler_DefaultMelodicBankMSB = 0x79, kAUSampler_DefaultBankLSB = 0x00 };

struct ScheduledAudioSlice {
    AudioTimeStamp mTimeStamp;
    ScheduledAudioSliceCompletionProc __nullable mCompletionProc;
    void *mCompletionProcUserData;
    AUScheduledAudioSliceFlags mFlags;
    UInt32 mReserved;              // must be 0
    void * __nullable mReserved2;  // internal
    UInt32 mNumberFrames;
    AudioBufferList *mBufferList;  // deinterleaved Float32
};

struct ScheduledAudioFileRegion {
    AudioTimeStamp mTimeStamp;
    ScheduledAudioFileRegionCompletionProc __nullable mCompletionProc;
    void * __nullable mCompletionProcUserData;
    struct OpaqueAudioFileID *mAudioFile;
    UInt32 mLoopCount; SInt64 mStartFrame; UInt32 mFramesToPlay;
};

typedef CF_OPTIONS(UInt32, AUScheduledAudioSliceFlags) {
    kScheduledAudioSliceFlag_Complete          = 0x01,
    kScheduledAudioSliceFlag_BeganToRender     = 0x02,
    kScheduledAudioSliceFlag_BeganToRenderLate = 0x04,
    kScheduledAudioSliceFlag_Loop              = 0x08,
    kScheduledAudioSliceFlag_Interrupt         = 0x10,
    kScheduledAudioSliceFlag_InterruptAtLoop   = 0x20
};

struct AudioUnitOtherPluginDesc { UInt32 format; AudioClassDescription plugin; };
CF_ENUM(UInt32) { kOtherPluginFormat_Undefined = 0, kOtherPluginFormat_kMAS = 1,
                  kOtherPluginFormat_kVST = 2, kOtherPluginFormat_AU = 3 };
struct AudioUnitParameterValueTranslation {
    AudioUnitOtherPluginDesc otherDesc; UInt32 otherParamID; Float32 otherValue;
    AudioUnitParameterID auParamID; AudioUnitParameterValue auValue;
};

// [DEPRECATED] legacy
struct AUNumVersion { UInt8 nonRelRev, stage, minorAndBugRev, majorRev; };  // little-endian layout
struct AUHostIdentifier { CFStringRef hostName; AUNumVersion hostVersion; };
struct AudioUnitMIDIControlMapping { UInt16 midiNRPN; UInt8 midiControl, scope;
                                     AudioUnitElement element; AudioUnitParameterID parameter; };
struct AudioUnitParameterValueName { AudioUnitParameterID inParamID;
                                     const Float32 *inValue; CFStringRef outName; };
struct AUSamplerBankPresetData { CFURLRef bankURL; UInt8 bankMSB, bankLSB, presetID, reserved; };
struct AUDistanceAttenuationData { UInt32 inNumberOfPairs;
                                   struct { Float32 inDistance; Float32 outGain; } pairs[1]; };
```

## D.12 Cocoa custom view — `AUCocoaUIView.h`

The whole header is one Objective-C protocol (macOS, `__OBJC__` only):

```objc
@protocol AUCocoaUIBase
- (unsigned)interfaceVersion;                     // return 0 for macOS 10.3+
- (NSView * __nullable)uiViewForAudioUnit:(AudioUnit)inAudioUnit
                                 withSize:(NSSize)inPreferredSize;
@end
```

Host flow: read `kAudioUnitProperty_CocoaUI` → get `AudioUnitCocoaViewInfo` → load the bundle at `mCocoaAUViewBundleLocation` → look up the class named by `mCocoaAUViewClass[0]` → instantiate it → call `uiViewForAudioUnit:withSize:`.

Header contract notes a host must honour:
- The method is a **factory** — each call must return a *unique* view.
- Views come back with retain count 1, autoreleased. **The client retains and releases.**
- Plugins are encouraged to override `-description` to name the view; treat the returned string as a copy.

---

# E. AUv3 note

AUv3 (`AUAudioUnit`, an Objective-C class) does **not** replace the v2 C API for a host — Apple bridges them. A v3 unit is discovered by the same `AudioComponentFindNext` and driven through the same `AudioUnitRender`/`AudioUnitGetProperty` calls. What materially changes the v2 host contract:

1. **Instantiation must be asynchronous.** A v3 unit carries `kAudioComponentFlag_RequiresAsyncInstantiation`, so `AudioComponentInstanceNew` fails; use `AudioComponentInstantiate` with a completion block. `kAudioComponentFlag_IsV3AudioUnit` identifies them.
2. **The process can die.** `kAudioComponentErr_InstanceInvalidated` (-66749) and `kAudioComponentInstanceInvalidationNotification` exist only because of out-of-process hosting. A v2-era host with no handling for these will hang or crash on a plugin crash.
3. **The view is a view controller**, not an `NSView` factory — `kAudioUnitProperty_RequestViewController` (56) takes a block receiving an `AUViewControllerBase`, rather than the `kAudioUnitProperty_CocoaUI` bundle-and-class dance. A host wanting to display both must implement both paths.
4. **In-process loading is opt-in** on macOS via `kAudioComponentInstantiation_LoadInProcess`, and only when the unit sets `kAudioComponentFlag_CanLoadInProcess`.
5. `kAudioUnitProperty_LoadedOutOfProcess` (62) lets a host ask, after the fact, which world it got.

Everything else — the property IDs, scopes, parameter model, render callbacks, `HostCallbackInfo` — is shared. Per the task scope, the `AUAudioUnit` Obj-C surface itself is not enumerated here.
