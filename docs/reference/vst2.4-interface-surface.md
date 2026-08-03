# VST 2.4 Plugin-Format Interface Surface

Extracted from the vendored `vst-rs` bindings at
`crates/tutti/crates/plugin/vendor/vst-tutti/src/` — treated as the machine-readable
form of the VST 2.4 SDK. Host's perspective.

Two call directions, both funnelled through one C signature shape
(`effect, opcode: i32, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize`):

- **host -> plugin**: `AEffect::dispatcher`, opcodes from `plugin::OpCode` (`eff*`)
- **plugin -> host**: the `HostCallbackProc` we hand to `VSTPluginMain`, opcodes from
  `host::OpCode` (`audioMaster*`)

Numeric values below were computed from the `#[repr(i32)]` enum declaration order, not
transcribed. Both enums are positional: inserting a variant renumbers everything below
it. `plugin.rs`'s own test pins `StartProcess = 71` / `StopProcess = 72` against the SDK
for exactly this reason.

---

## A. Host -> Plugin opcodes (`plugin::OpCode`, `eff*`) — 80 variants

Dispatched by the host into `AEffect::dispatcher`. Variants prefixed `_` in the Rust enum
are deprecated/VST1 leftovers that exist only to hold their numeric slot.

### Lifecycle & configuration

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose | Status |
|---|---|---|---|---|---|---|---|---|---|
| 0 | `Initialize` | `effOpen` | — | — | — | — | 0 | Plugin fully constructed; begin init | 2.4 core |
| 1 | `Shutdown` | `effClose` | — | — | — | — | 0 | Teardown. **Plugin frees its own `AEffect`** — dispatch exactly once, never touch the pointer after | 2.4 core |
| 10 | `SetSampleRate` | `effSetSampleRate` | — | — | — | **new rate (Hz)** | 0 | Sample rate change. Rate arrives in `opt`, not `value` | 2.4 core |
| 11 | `SetBlockSize` | `effSetBlockSize` | — | **max block size** | — | — | 0 | Max frames per process call | 2.4 core |
| 12 | `StateChanged` | `effMainsChanged` | — | **1 = resume, 0 = suspend** | — | — | 0 | Suspend/resume transition. One opcode for both directions | 2.4 core |
| 71 | `StartProcess` | `effStartProcess` | — | — | — | — | 0 | Process calls are about to begin | 2.4 core |
| 72 | `StopProcess` | `effStopProcess` | — | — | — | — | 0 | Process calls have stopped | 2.4 core |
| 73 | `SetTotalSampleToProcess` | `effSetTotalSampleToProcess` | — | **sample count** | — | — | 0 | Offline: total length ahead of process | 2.4 core |
| 77 | `SetPrecision` | `effSetProcessPrecision` | — | **0 = f32, else f64** | — | — | 0 | Select 32/64-bit process path | 2.4 core |
| 43 | `_SetBlocksizeAndSampleRate` | `effSetBlockSizeAndSampleRate` | — | — | — | — | — | Superseded by 10 + 11 | **deprecated** |
| 53 | `_Idle` | `effIdle` | — | — | — | — | — | VST1 idle | **deprecated** |

### Presets / programs

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose | Status |
|---|---|---|---|---|---|---|---|---|---|
| 2 | `ChangePreset` | `effSetProgram` | — | **preset number** | — | — | 0 | Switch program. Number is in `value`, not `index` | 2.4 core |
| 3 | `GetCurrentPresetNum` | `effGetProgram` | — | — | — | — | **current preset index** | Read active program | 2.4 core |
| 4 | `SetCurrentPresetName` | `effSetProgramName` | — | — | `char*` in, `MAX_PRESET_NAME_LEN` (24) | — | 0 | Rename active program | 2.4 core |
| 5 | `GetCurrentPresetName` | `effGetProgramName` | — | — | `char*` out, 24 | — | 1 on success | Name of active program | 2.4 core |
| 29 | `GetPresetName` | `effGetProgramNameIndexed` | **program index** | — | `char*` out, 24 | — | 1 on success | Name of program at index | 2.4 core |
| 67 | `BeginSetPreset` | `effBeginSetProgram` | — | — | — | — | 0 | Bracket opened before a preset load | 2.4 core |
| 68 | `EndSetPreset` | `effEndSetProgram` | — | — | — | — | 0 | Bracket closed after a preset load | 2.4 core |
| 75 | `BeginLoadBank` | `effBeginLoadBank` | — | — | `*mut VstPatchChunkInfo` | — | **-1 = can't load, 1 = can, 0 = unsupported** | Pre-flight a bank load | 2.4 core, **binding TODO** (struct not defined) |
| 76 | `BeginLoadPreset` | `effBeginLoadProgram` | — | — | `*mut VstPatchChunkInfo` | — | **-1 / 1 / 0** as above | Pre-flight a preset load | 2.4 core, **binding TODO** |
| 28 | `_GetNumCategories` | `effGetNumProgramCategories` | — | — | — | — | — | VST1 program categories | **deprecated** |
| 30 | `_CopyPreset` | `effCopyProgram` | — | — | — | — | — | VST1 | **deprecated** |

### State chunks

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose | Status |
|---|---|---|---|---|---|---|---|---|---|
| 23 | `GetData` | `effGetChunk` | **0 = bank, 1 = program** | — | `void**` — plugin writes *its own* buffer address here | — | **byte length** (negative = error) | Save opaque state | 2.4 core |
| 24 | `SetData` | `effSetChunk` | **0 = bank, 1 = program** | **byte length** | `void*` data in | — | **1 = accepted** | Restore opaque state | 2.4 core |

Only meaningful when `effFlagsProgramChunks` (bit 5) is set; otherwise the host saves
parameters individually.

### Parameters

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose | Status |
|---|---|---|---|---|---|---|---|---|---|
| 6 | `GetParameterLabel` | `effGetParamLabel` | **param** | — | `char*` out, `MAX_PARAM_STR_LEN` (32) | — | 1 | Unit string ("dB", "ms") | 2.4 core |
| 7 | `GetParameterDisplay` | `effGetParamDisplay` | **param** | — | `char*` out, 32 | — | 1 | Formatted value ("0.5", "ROOM") | 2.4 core |
| 8 | `GetParameterName` | `effGetParamName` | **param** | — | `char*` out, 32 | — | 1 | Parameter name | 2.4 core |
| 26 | `CanBeAutomated` | `effCanBeAutomated` | **param** | — | — | — | **1 = yes, 0 = no** | Automatable? | 2.4 core |
| 27 | `StringToParameter` | `effString2Parameter` | **param** | — | `char*` in | — | **true on success** | Parse typed text into a value | 2.4 core |
| 56 | `GetParamInfo` | `effGetParameterProperties` | **param** | — | `*mut ParameterProperties` out | — | **1 if supported** | Structured param metadata | 2.4 core; **plugin-side dispatch is commented out** in `interfaces.rs` (host side implemented via `PluginInstance::parameter_properties`) |
| 9 | `_GetVu` | `effGetVu` | — | — | — | — | — | VST1 VU meter | **deprecated** |

Parameter *values* do not go through the dispatcher at all — they use the dedicated
`AEffect::getParameter` / `setParameter` function pointers.

### Audio I/O & routing

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose | Status |
|---|---|---|---|---|---|---|---|---|---|
| 33 | `GetInputInfo` | `effGetInputProperties` | **input idx** | — | `*mut ChannelProperties` out | — | **1 if supported** | Per-input channel metadata | 2.4 core |
| 34 | `GetOutputInfo` | `effGetOutputProperties` | **output idx** | — | `*mut ChannelProperties` out | — | **1 if supported** | Per-output channel metadata | 2.4 core |
| 42 | `SetSpeakerArrangement` | `effSetSpeakerArrangement` | — | **input** `*mut VstSpeakerArrangement` (a pointer passed in `value`) | **output** `*mut VstSpeakerArrangement` | — | 1 if accepted | Negotiate channel layout | 2.4 core, **binding TODO** |
| 69 | `GetSpeakerArrangement` | `effGetSpeakerArrangement` | — | **input** `*mut VstSpeakerArrangement` | **output** `*mut VstSpeakerArrangement` | — | 1 if supported | Read current layout | 2.4 core, **binding TODO** |
| 44 | `SoftBypass` | `effSetBypass` | — | **1 = bypass, 0 = not** | — | — | 1 if handled | Automatable bypass (plugin still processes) | 2.4 core |
| 74 | `SetPanLaw` | `effSetPanLaw` | — | **pan law enum** | — | **gain** | 1 if handled | Host pan law | 2.4 core, **binding TODO** (`PanLaw` not defined) |
| 31 | `_ConnectIn` | `effConnectInput` | — | — | — | — | — | VST1 | **deprecated** |
| 32 | `_ConnectOut` | `effConnectOutput` | — | — | — | — | — | VST1 | **deprecated** |
| 37 | `_GetDestinationBuffer` | `effGetDestinationBuffer` | — | — | — | — | — | VST1 | **deprecated** |

`SetSpeakerArrangement` is the one opcode where **`value` carries a pointer**, not an
integer — the input arrangement goes in `value`, the output in `ptr`. Easy to get backwards.

### Events / MIDI

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose | Status |
|---|---|---|---|---|---|---|---|---|---|
| 25 | `ProcessEvents` | `effProcessEvents` | — | — | `*const api::Events` | — | 1 if handled | Deliver MIDI/SysEx for the coming block. Must precede `process*` | 2.4 core |
| 62 | `GetMidiProgramName` | `effGetMidiProgramName` | **MIDI channel** | — | `*mut MidiProgramName`; host pre-writes `this_program_index` | — | **number of programs serviced; 0 = unsupported** (not a bool) | Named MIDI programs | 2.4 core, **binding TODO** plugin-side; host-side implemented |
| 63 | `GetCurrentMidiProgram` | `effGetCurrentMidiProgram` | **MIDI channel** | — | `*mut MidiProgramName` | — | **index of current program** | Which program is active | 2.4 core, **binding TODO** plugin-side |
| 64 | `GetMidiProgramCategory` | `effGetMidiProgramCategory` | **MIDI channel** | — | `*mut MidiProgramCategory`; host pre-writes `this_category_index` | — | **number of categories used** | Program grouping | 2.4 core, **binding TODO** plugin-side |
| 65 | `HasMidiProgramsChanged` | `effHasMidiProgramsChanged` | **MIDI channel** | — | — | — | **1 = changed** | Cache invalidation for the three above | 2.4 core, **binding TODO** plugin-side |
| 66 | `GetMidiKeyName` | `effGetMidiKeyName` | **MIDI channel** | — | `*mut MidiKeyName`; host pre-writes `this_program_index` + `this_key_number` | — | **1 = supported, 0 = not** | Per-key labels (drum pads) | 2.4 core, **binding TODO** plugin-side |
| 78 | `GetNumMidiInputs` | `effGetNumMidiInputChannels` | — | — | — | — | **count (1-15)** | MIDI in channel count | 2.4 core |
| 79 | `GetNumMidiOutputs` | `effGetNumMidiOutputChannels` | — | — | — | — | **count (1-15)** | MIDI out channel count | 2.4 core |

For the five MIDI-metadata opcodes the host **writes the query into the struct before
dispatch** — `this_program_index`, `this_category_index`, `this_key_number` are inputs,
not outputs. Leaving them zero asks about item 0 every time and reads as "the plugin
returns the same name for everything".

### Identity / capability

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose | Status |
|---|---|---|---|---|---|---|---|---|---|
| 35 | `GetCategory` | `effGetPlugCategory` | — | — | — | — | **`PluginCategory` code** | Effect/synth/analysis/… | 2.4 core |
| 45 | `GetEffectName` | `effGetEffectName` | — | — | `char*` out | — | 1 on success | Effect name | 2.4 core |
| 47 | `GetVendorName` | `effGetVendorString` | — | — | `char*` out, `MAX_VENDOR_STR_LEN` (64) | — | 1 on success | Vendor | 2.4 core |
| 48 | `GetProductName` | `effGetProductString` | — | — | `char*` out, `MAX_PRODUCT_STR_LEN` (64) | — | 1 on success | Product | 2.4 core |
| 49 | `GetVendorVersion` | `effGetVendorVersion` | — | — | — | — | **vendor version int** | Version | 2.4 core |
| 50 | `VendorSpecific` | `effVendorSpecific` | free | free | free | free | free | Vendor escape hatch; all six args vendor-defined | 2.4 core |
| 51 | `CanDo` | `effCanDo` | — | — | `char*` capability string in | — | **1 = yes, 0 = maybe/don't know, -1 = no** | Feature query | 2.4 core |
| 52 | `GetTailSize` | `effGetTailSize` | — | — | — | — | **see quirk below** | Ring-out length in samples | 2.4 core |
| 58 | `GetApiVersion` | `effGetVstVersion` | — | — | — | — | **2400 for VST 2.4** | Version handshake; the host's load gate | 2.4 core |
| 70 | `ShellGetNextPlugin` | `effShellGetNextPlugin` | — | — | `char*` out, 64 | — | **next plugin's uniqueID; 0 ends the list** | Enumerate a shell plugin's children | 2.4 core |
| 46 | `_GetErrorText` | `effGetErrorText` | — | — | — | — | — | VST1 | **deprecated** |
| 57 | `_KeysRequired` | `effKeysRequired` | — | — | — | — | — | VST1 (inverted-meaning legacy) | **deprecated** |
| 36 | `_GetCurrentPosition` | `effGetCurrentPosition` | — | — | — | — | — | VST1 | **deprecated** |

### Editor / GUI

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose | Status |
|---|---|---|---|---|---|---|---|---|---|
| 13 | `EditorGetRect` | `effEditGetRect` | — | — | **`Rect**`** — plugin writes a pointer to *its* `Rect` | — | 1 on success | Editor size/position | 2.4 core |
| 14 | `EditorOpen` | `effEditOpen` | — | — | **platform window handle** (`HWND` / `NSView*` / X11 window id) | — | **> 0 = opened** | Embed the editor | 2.4 core |
| 15 | `EditorClose` | `effEditClose` | — | — | — | — | 0 | Tear down the editor | 2.4 core |
| 19 | `EditorIdle` | `effEditIdle` | — | — | — | — | 0 | Periodic GUI tick from the host | 2.4 core |
| 59 | `EditorKeyDown` | `effEditKeyDown` | **ASCII char** | **`Key` keycode** | — | **modifier bitmask** | **1 if consumed** | Key press | 2.4 core |
| 60 | `EditorKeyUp` | `effEditKeyUp` | **ASCII char** | **`Key` keycode** | — | **modifier bitmask** | **1 if consumed** | Key release | 2.4 core |
| 61 | `EditorSetKnobMode` | `effSetEditKnobMode` | — | **0 = circular, 1 = circular relative, 2 = linear** | — | — | 1 if handled | Knob drag behaviour | 2.4 core |
| 16 | `_EditorDraw` | `effEditDraw` | — | — | — | — | — | VST1 | **deprecated** |
| 17 | `_EditorMouse` | `effEditMouse` | — | — | — | — | — | VST1 | **deprecated** |
| 18 | `_EditorKey` | `effEditKey` | — | — | — | — | — | VST1, superseded by 59/60 | **deprecated** |
| 20 | `_EditorTop` | `effEditTop` | — | — | — | — | — | VST1 | **deprecated** |
| 21 | `_EditorSleep` | `effEditSleep` | — | — | — | — | — | VST1 | **deprecated** |
| 22 | `_EditorIdentify` | `effIdentify` | — | — | — | — | — | VST1 | **deprecated** |
| 54 | `_GetIcon` | `effGetIcon` | — | — | — | — | — | VST1 | **deprecated** |
| 55 | `_SetVewPosition` | `effSetViewPosition` | — | — | — | — | — | VST1 (typo is in the binding) | **deprecated** |

`EditorKeyDown`/`EditorKeyUp` pass the modifier mask through **`opt`, an `f32`**, and the
plugin-side dispatcher reads it back with `opt.to_bits() as u8` — i.e. the bits are
reinterpreted, not converted. A host must write the mask the same way.

### Offline processing

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose | Status |
|---|---|---|---|---|---|---|---|---|---|
| 38 | `OfflineNotify` | `effOfflineNotify` | **start flag** | **count** | `VstAudioFile*` array | — | 1 if handled | Announce offline files | 2.4 core, **binding TODO** |
| 39 | `OfflinePrepare` | `effOfflinePrepare` | — | **count** | `VstOfflineTask*` array | — | 1 if handled | Prepare offline tasks | 2.4 core, **binding TODO** |
| 40 | `OfflineRun` | `effOfflineRun` | — | **count** | `VstOfflineTask*` array | — | 1 if handled | Run offline tasks | 2.4 core, **binding TODO** |
| 41 | `ProcessVarIo` | `effProcessVarIo` | — | — | `*mut VstVariableIo` | — | 1 if handled | Variable-rate I/O (time-stretch) | 2.4 core, **binding TODO** — `VstVariableIo` is **not defined anywhere** in the bindings |

---

## B. Plugin -> Host opcodes (`host::OpCode`, `audioMaster*`) — 49 variants

These are what a host must implement. Numbering has a **gap at 5** (the enum jumps
`_PinConnected = 4` straight to `_WantMidi = 6`; the binding comments "Not a typo" —
`audioMasterPinConnected` is 4 and `audioMasterWantMidi` is 6 in the SDK).

### Implemented by the bindings' `host_dispatch`

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose |
|---|---|---|---|---|---|---|---|---|
| 0 | `Automate` | `audioMasterAutomate` | **param index** | — | — | **new value (0..1)** | 0 | Parameter changed from the GUI; host records automation. Value rides in `opt` |
| 1 | `Version` | `audioMasterVersion` | — | — | — | — | **2400** | Host VST version. A `0` return means "no host" and `vst::main` aborts the load |
| 2 | `CurrentId` | `audioMasterCurrentId` | — | — | — | — | **plugin ID** | Shell-plugin selection: which sub-plugin `VSTPluginMain` should return |
| 3 | `Idle` | `audioMasterIdle` | — | — | — | — | 0 | Plugin is in a modal loop; give the host time |
| 7 | `GetTime` | `audioMasterGetTime` | — | **request mask (`TimeInfoFlags`)** | — | — | **`TimeInfo*` or 0** | Transport/tempo query. Returns a *pointer as an integer* |
| 8 | `ProcessEvents` | `audioMasterProcessEvents` | — | — | `VstEvents*` | — | **1 if handled** | Plugin emits MIDI. Only legal from inside `process*` |
| 13 | `IOChanged` | `audioMasterIOChanged` | — | — | — | — | **1 if supported** | I/O count or latency changed; host should re-read `AEffect` |
| 15 | `SizeWindow` | `audioMasterSizeWindow` | **new width** | **new height** | — | — | **1 if resized** | Plugin asks the host to resize its editor window |
| 16 | `GetSampleRate` | `audioMasterGetSampleRate` | — | — | — | — | **sample rate** | Current rate. See quirk below |
| 17 | `GetBlockSize` | `audioMasterGetBlockSize` | — | — | — | — | **block size** | Current max block |
| 18 | `GetInputLatency` | `audioMasterGetInputLatency` | — | — | — | — | **samples** | Host input latency |
| 19 | `GetOutputLatency` | `audioMasterGetOutputLatency` | — | — | — | — | **samples** | Host output latency |
| 23 | `GetCurrentProcessLevel` | `audioMasterGetCurrentProcessLevel` | — | — | — | — | **`ProcessLevel`: 0 unknown, 1 user, 2 realtime, 3 prefetch, 4 offline** | Which thread am I on |
| 24 | `GetAutomationState` | `audioMasterGetAutomationState` | — | — | — | — | **`VstAutomationStates`; 0 = unsupported** | Read/write/off automation mode |
| 32 | `GetVendorString` | `audioMasterGetVendorString` | — | — | `char*` out, 64 | — | 1 on success | Host vendor |
| 33 | `GetProductString` | `audioMasterGetProductString` | — | — | `char*` out, 64 | — | 1 on success | Host product |
| 34 | `GetVendorVersion` | `audioMasterGetVendorVersion` | — | — | — | — | **version int** | Host version |
| 37 | `CanDo` | `audioMasterCanDo` | — | — | `char*` capability in | — | **1 if supported** | Host capability query. **Binding only logs it and returns 0** — a real gap |
| 43 | `BeginEdit` | `audioMasterBeginEdit` | **param index** | — | — | — | **true on success** | Gesture start (knob touched); begins an automation record |
| 44 | `EndEdit` | `audioMasterEndEdit` | **param index** | — | — | — | **true on success** | Gesture end (knob released) |

`Automate` must be bracketed by `BeginEdit`/`EndEdit` for the host to distinguish a user
gesture from a programmatic change.

### Recognised by the enum but NOT handled by `host_dispatch` — fall through to `0`

These are real VST 2.4 opcodes a plugin may call; the bindings decode the number but have
no `Host` trait method, so they silently answer 0 ("unhandled"). Each is a gap a host
built on these bindings must fill itself.

| # | Rust name | Classic | index | value | ptr | opt | Return | Purpose |
|---|---|---|---|---|---|---|---|---|
| 25 | `OfflineStart` | `audioMasterOfflineStart` | **num new audio files** | **num audio files** | `AudioFile*` | — | 1 | Plugin begins offline processing |
| 26 | `OfflineRead` | `audioMasterOfflineRead` | **bool: read original (true) vs plugin-written (false) samples** | **`OfflineOption`** | `OfflineTask*` | — | **1 on success** | Plugin reads offline data |
| 27 | `OfflineWrite` | `audioMasterOfflineWrite` | — | **`OfflineOption`** | `OfflineTask*` | — | 1 | Plugin writes offline data |
| 28 | `OfflineGetCurrentPass` | `audioMasterOfflineGetCurrentPass` | — | — | — | — | pass no. | Offline pass counter (undocumented in bindings) |
| 29 | `OfflineGetCurrentMetaPass` | `audioMasterOfflineGetCurrentMetaPass` | — | — | — | — | meta pass no. | Offline meta-pass counter (undocumented) |
| 35 | `VendorSpecific` | `audioMasterVendorSpecific` | free | free | free | free | free | Vendor escape hatch into the host |
| 38 | `GetLanguage` | `audioMasterGetLanguage` | — | — | — | — | **`HostLanguage`: 1 English, 2 German, 3 French, 4 Italian, 5 Spanish, 6 Japanese** | Host UI language |
| 41 | `GetDirectory` | `audioMasterGetDirectory` | — | — | — | — | **`FSSpec` on OS X, `char*` elsewhere** | Host/plugin directory |
| 42 | `UpdateDisplay` | `audioMasterUpdateDisplay` | — | — | — | — | 0 | Params changed programmatically; refresh the GUI. **Called on `HostCallback` but not decoded in `host_dispatch`** |
| 45 | `OpenFileSelector` | `audioMasterOpenFileSelector` | — | — | `VstFileSelect*` | — | **true on success** | Host-native file dialog |
| 46 | `CloseFileSelector` | `audioMasterCloseFileSelector` | — | — | `VstFileSelect*` | — | **true on success** | Free the selector's results |

`UpdateDisplay` is asymmetric in the bindings: `HostCallback::update_display` (plugin side)
*sends* opcode 42, but `interfaces::host_dispatch` has no arm for it, so a plugin loaded by
this host gets 0 back and the `Host::update_display` trait method is never invoked. That is
a bug in the vendored bindings, not in the format.

### Deprecated / VST1 host opcodes

| # | Rust name | Classic | Status |
|---|---|---|---|
| 4 | `_PinConnected` | `audioMasterPinConnected` | **deprecated** |
| 6 | `_WantMidi` | `audioMasterWantMidi` | **deprecated** (5 is skipped) |
| 9 | `_SetTime` | `audioMasterSetTime` | **deprecated** |
| 10 | `_TempoAt` | `audioMasterTempoAt` | **deprecated** — use `GetTime` |
| 11 | `_GetNumAutomatableParameters` | `audioMasterGetNumAutomatableParameters` | **deprecated** |
| 12 | `_GetParameterQuantization` | `audioMasterGetParameterQuantization` | **deprecated** |
| 14 | `_NeedIdle` | `audioMasterNeedIdle` | **deprecated** |
| 20 | `_GetPreviousPlug` | `audioMasterGetPreviousPlug` | **deprecated** |
| 21 | `_GetNextPlug` | `audioMasterGetNextPlug` | **deprecated** |
| 22 | `_WillReplaceOrAccumulate` | `audioMasterWillReplaceOrAccumulate` | **deprecated** |
| 30 | `_SetOutputSampleRate` | `audioMasterSetOutputSampleRate` | **deprecated** |
| 31 | `_GetOutputSpeakerArrangement` | `audioMasterGetOutputSpeakerArrangement` | **deprecated** |
| 36 | `_SetIcon` | `audioMasterSetIcon` | **deprecated** |
| 39 | `_OpenWindow` | `audioMasterOpenWindow` | **deprecated** |
| 40 | `_CloseWindow` | `audioMasterCloseWindow` | **deprecated** |
| 47 | `_EditFile` | `audioMasterEditFile` | **deprecated** |
| 48 | `_GetChunkFile` | `audioMasterGetChunkFile` | **deprecated** — `ptr`: `char[2048]` or `sizeof(FSSpec)`, returns 1 if supported |
| 49 | `_GetInputSpeakerArrangement` | `audioMasterGetInputSpeakerArrangement` | **deprecated** |

---

## C. The `AEffect` struct

`#[repr(C)]`. Allocated and populated by the **plugin**; the host receives a pointer from
`VSTPluginMain` and hands it back on every call.

| Field | Type | Meaning |
|---|---|---|
| `magic` | `i32` | Must be `VST_MAGIC` = `'VstP'` = `0x56737450`. The validity check |
| `dispatcher` | `Option<DispatcherProc>` | **Entry point for every host->plugin opcode.** Nullable |
| `_process` | `Option<ProcessProc>` | Deprecated **accumulating** process — *adds into* the outputs. Outputs must be zeroed first |
| `setParameter` | `Option<SetParameterProc>` | `(effect, index, value: f32)`. Normalized 0..1. Nullable |
| `getParameter` | `Option<GetParameterProc>` | `(effect, index) -> f32`. Normalized 0..1. Nullable |
| `numPrograms` | `i32` | Preset count |
| `numParams` | `i32` | Parameter count (same for every program) |
| `numInputs` | `i32` | Audio input channels |
| `numOutputs` | `i32` | Audio output channels |
| `flags` | `i32` | `PluginFlags` bitmask (see D) |
| `reserved1` | `isize` | "Reserved for host, must be 0" — **the bindings hijack it** to stash the `Arc<Host>` pointer |
| `reserved2` | `isize` | Reserved for host, must be 0 |
| `initialDelay` | `i32` | Latency / group delay in samples (PDC). Valid in resume state |
| `_realQualities` | `i32` | Deprecated, unused |
| `_offQualities` | `i32` | Deprecated, unused |
| `_ioRatio` | `f32` | Deprecated, unused |
| `object` | `*mut c_void` | Plugin's own instance data |
| `user` | `*mut c_void` | User pointer |
| `uniqueId` | `i32` | Registered 4-char ID; identifies the plugin across save/load |
| `version` | `i32` | Plugin version (1100 = v1.1.0.0) |
| `processReplacing` | `Option<ProcessProc>` | **The VST 2.4 audio entry point.** Overwrites outputs |
| `processReplacingF64` | `Option<ProcessProcF64>` | 64-bit replacing process. No accumulating counterpart exists |
| `future` | `[u8; 56]` | Reserved, zero |

### Function pointer signatures

```rust
DispatcherProc  = extern "C" fn(*mut AEffect, opcode: i32, index: i32,
                                value: isize, ptr: *mut c_void, opt: f32) -> isize
ProcessProc     = extern "C" fn(*mut AEffect, inputs: *const *const f32,
                                outputs: *mut *mut f32, sample_frames: i32)
ProcessProcF64  = extern "C" fn(*mut AEffect, inputs: *const *const f64,
                                outputs: *mut *mut f64, sample_frames: i32)
SetParameterProc = extern "C" fn(*mut AEffect, index: i32, parameter: f32)
GetParameterProc = extern "C" fn(*mut AEffect, index: i32) -> f32
PluginMain      = fn(callback: HostCallbackProc) -> *mut AEffect
HostCallbackProc = extern "C" fn(*mut AEffect, opcode: i32, index: i32,
                                 value: isize, ptr: *mut c_void, opt: f32) -> isize
```

Audio buffers are **non-interleaved**: an array of per-channel pointers, each addressing
`sample_frames` samples.

### Every function slot is `Option<fn>`, and that is load-bearing

The bindings changed all six from bare `extern "C" fn` to `Option<...>` after a real crash.
A bare `fn` type is non-nullable, so rustc folded `(p as *const u8).is_null()` to `false`
under `-O`, the guard shipped only in debug, and release jumped to address 0 — taking the
whole DAW down. `Option<fn>` is ABI-identical (null pointer optimization) and makes the
check real at every optimization level.

A host must null-check **before every call**, including `processReplacing`, even though
VST 2.4 makes it mandatory: a plugin can report api version >= 2400 and still leave the
slot null. Likewise `setParameter`/`getParameter` may legitimately be null when
`numParams == 0`.

---

## D. Key enums and structs

### `PluginFlags` (`effFlags*`) — `AEffect::flags`

| Constant | Bit | Value | Meaning |
|---|---|---|---|
| `HAS_EDITOR` | 0 | 1 | Plugin publishes a GUI. The correct gate for `effEditOpen` |
| `CAN_REPLACING` | 4 | 16 | `processReplacing` is installed. **Mandatory in VST 2.4**, but check it |
| `PROGRAM_CHUNKS` | 5 | 32 | State is opaque chunks (`effGetChunk`/`effSetChunk`), not per-parameter |
| `IS_SYNTH` | 8 | 256 | Instrument, not effect |
| `NO_SOUND_IN_STOP` | 9 | 512 | Produces no sound when input is silent (host may skip processing) |
| `CAN_DOUBLE_REPLACING` | 12 | 4096 | `processDoubleReplacingF64` is available |

Bits 1, 2, 3, 6, 7, 10, 11 are the deprecated VST1 flags (`effFlagsHasClip`,
`effFlagsHasVu`, `effFlagsCanMono`, `effFlagsExtIsAsync`, `effFlagsExtHasBuffer`) and are
**not modelled** in the bindings.

### `Events` / `Event` / `MidiEvent` / `SysExEvent`

`Events` — the container passed by `effProcessEvents` / `audioMasterProcessEvents`:

| Field | Type | Meaning |
|---|---|---|
| `num_events` | `i32` | Event count |
| `_reserved` | `isize` | Zero |
| `events` | `[*mut Event; 2]` | **Variable-length array declared with initial size 2.** For > 2 events, a larger allocation is stored here and read past the declared bound |

`Event` — the common header every event type starts with:

| Field | Type | Meaning |
|---|---|---|
| `event_type` | `EventType` (`i32`) | Discriminator; tells you which type to transmute to |
| `byte_size` | `i32` | `sizeof` of the *concrete* type |
| `delta_frames` | `i32` | Sample offset into the current block |
| `_flags` | `i32` | Generic; none defined |
| `_reserved` | `[u8; 16]` | Padding — the concrete types are **not all the same size** |

`EventType`: `_Placeholder = 0`, `Midi = 1`, `_Audio = 2`, `_Video = 3`, `_Parameter = 4`,
`_Trigger = 5`, `SysEx = 6`. Types 2-5 are deprecated; the bindings surface them as
`Event::Deprecated`.

`MidiEvent`:

| Field | Type | Meaning |
|---|---|---|
| `event_type` | `EventType` | `Midi` |
| `byte_size` | `i32` | `sizeof::<MidiEvent>()` |
| `delta_frames` | `i32` | Sample offset in block |
| `flags` | `i32` | `MidiEventFlags` |
| `note_length` | `i32` | Full note length in frames, else 0 |
| `note_offset` | `i32` | Offset into the note from its start, else 0 |
| `midi_data` | `[u8; 3]` | 1-3 raw MIDI bytes |
| `_midi_reserved` | `u8` | 0 |
| `detune` | `i8` | -63..+64 cents (microtuning) |
| `note_off_velocity` | `u8` | 0-127 |
| `_reserved1`, `_reserved2` | `u8` | 0 |

`MidiEventFlags`: `REALTIME_EVENT = 1` — event is live rather than sequencer playback, so
a high-latency plugin can prioritize it.

`SysExEvent`:

| Field | Type | Meaning |
|---|---|---|
| `event_type` | `EventType` | `SysEx` |
| `byte_size` | `i32` | `sizeof::<SysExEvent>()` |
| `delta_frames` | `i32` | Sample offset |
| `_flags` | `i32` | None defined |
| `data_size` | `i32` | Payload length in bytes |
| `_reserved1` | `isize` | 0 |
| `system_data` | `*mut u8` | Pointer to payload (**not inline**) |
| `_reserved2` | `isize` | 0 |

`SysExEvent` is larger than `MidiEvent`, which is why a send buffer that must hold either
allocates at `SysExEvent` size.

### `TimeInfo` (returned by `audioMasterGetTime`)

| Field | Type | Meaning | Gated by |
|---|---|---|---|
| `sample_pos` | `f64` | Position in samples | always valid |
| `sample_rate` | `f64` | Hz | always valid |
| `nanoseconds` | `f64` | System time (10^-9 s) | `NANOSECONDS_VALID` |
| `ppq_pos` | `f64` | Musical position in quarter notes | `PPQ_POS_VALID` |
| `tempo` | `f64` | BPM | `TEMPO_VALID` |
| `bar_start_pos` | `f64` | Last bar start, in quarter notes | `BARS_VALID` |
| `cycle_start_pos` | `f64` | Loop left locator, quarter notes | `CYCLE_POS_VALID` |
| `cycle_end_pos` | `f64` | Loop right locator, quarter notes | `CYCLE_POS_VALID` |
| `time_sig_numerator` | `i32` | e.g. 3 in 3/4 | `TIME_SIG_VALID` |
| `time_sig_denominator` | `i32` | e.g. 4 in 3/4 | `TIME_SIG_VALID` |
| `smpte_offset` | `i32` | SMPTE subframes (bits; 1/80 frame) | `SMPTE_VALID` |
| `smpte_frame_rate` | `SmpteFrameRate` | Frame rate enum | `SMPTE_VALID` |
| `samples_to_next_clock` | `i32` | MIDI clock (24 PPQ); may be negative = nearest clock | `VST_CLOCK_VALID` |
| `flags` | `i32` | `TimeInfoFlags` | — |

`TimeInfoFlags` — used **both** as the plugin's request mask in `value` and as the host's
validity report in `TimeInfo::flags`:

| Constant | Bit | Value | Meaning |
|---|---|---|---|
| `TRANSPORT_CHANGED` | 0 | 1 | Play/cycle/record state changed |
| `TRANSPORT_PLAYING` | 1 | 2 | Sequencer is playing |
| `TRANSPORT_CYCLE_ACTIVE` | 2 | 4 | Cycle/loop mode on |
| `TRANSPORT_RECORDING` | 3 | 8 | Recording |
| `AUTOMATION_WRITING` | 6 | 64 | Automation write mode |
| `AUTOMATION_READING` | 7 | 128 | Automation read mode |
| `NANOSECONDS_VALID` | 8 | 256 | `nanoseconds` valid |
| `PPQ_POS_VALID` | 9 | 512 | `ppq_pos` valid |
| `TEMPO_VALID` | 10 | 1024 | `tempo` valid |
| `BARS_VALID` | 11 | 2048 | `bar_start_pos` valid |
| `CYCLE_POS_VALID` | 12 | 4096 | both cycle positions valid |
| `TIME_SIG_VALID` | 13 | 8192 | both time-sig fields valid |
| `SMPTE_VALID` | 14 | 16384 | `smpte_offset` + `smpte_frame_rate` valid |
| `VST_CLOCK_VALID` | 15 | 32768 | `samples_to_next_clock` valid |

Bits 4 and 5 are unassigned. **A request does not guarantee delivery** — the plugin sets
request bits in `value`, and must re-check the returned `flags` to see which were honored.
Computing the mask is expensive for a host, which is why the request exists at all.

`SmpteFrameRate`: `Smpte24fps = 0`, `Smpte25fps = 1`, `Smpte2997fps = 2`, `Smpte30fps = 3`,
`Smpte2997dfps = 4`, `Smpte30dfps = 5`, `SmpteFilm16mm = 6`, `SmpteFilm35mm = 7`,
`Smpte239fps = 10`, `Smpte249fps = 11`, `Smpte599fps = 12`, `Smpte60fps = 13`.
**Values 8 and 9 do not exist** — the enum is non-contiguous, so a naive range check
accepts two invalid discriminants.

### `CanDo` strings

Sent as a C string in `ptr` for both `effCanDo` (host asks plugin) and `audioMasterCanDo`
(plugin asks host). The bindings' `CanDo` enum only models the plugin-side set; anything
unrecognized becomes `CanDo::Other(String)`.

| Variant | Wire string | Meaning |
|---|---|---|
| `SendEvents` | `sendVstEvents` | Plugin emits events |
| `SendMidiEvent` | `sendVstMidiEvent` | Plugin emits MIDI |
| `ReceiveEvents` | `receiveVstEvents` | Plugin accepts events |
| `ReceiveMidiEvent` | `receiveVstMidiEvent` | Plugin accepts MIDI |
| `ReceiveTimeInfo` | `receiveVstTimeInfo` | Plugin wants `TimeInfo` |
| `Offline` | `offline` | Offline processing supported |
| `MidiProgramNames` | `midiProgramNames` | The `effGetMidiProgram*` family works |
| `Bypass` | `bypass` | Soft bypass (`effSetBypass`) supported |
| `ReceiveSysExEvent` | `receiveVstSysexEvent` | SysEx accepted |
| `MidiSingleNoteTuningChange` | `midiSingleNoteTuningChange` | Per-note tuning (marked "Bitwig specific?") |
| `MidiKeyBasedInstrumentControl` | `midiKeyBasedInstrumentControl` | Per-key control (marked "Bitwig specific?") |
| `Other(String)` | anything else | Catch-all |

**Host-can-do strings are not modelled at all.** `host_dispatch`'s `CanDo` arm only logs
the string and returns 0, so a plugin asking the host `sendVstEvents`, `sizeWindow`,
`openFileSelector`, `acceptIOChanges`, `shellCategory`, `supplyIdle` etc. always hears
"no". This is a genuine gap in the bindings, not in the format.

### `Supported` — how `effCanDo` returns are decoded

| Value | Variant | Meaning |
|---|---|---|
| 1 | `Yes` | Supported |
| 0 | `Maybe` | Don't know / unhandled |
| -1 | `No` | Not supported |
| anything else | `Custom(isize)` | Undocumented answer, surfaced verbatim |

`Custom` exists because real plugins return whatever their dispatcher left in the return
slot — commonly `strlen` of the queried string, or an uninitialized stack value. Folding
those into `Yes` misclassifies the plugin, and panicking aborts during load (the host
probes canDo on every plugin it scans). A pinned test asserts that 2, 14, -2, 9999,
`isize::MIN` and `isize::MAX` all decode to something that is **not** `Yes`.

### `ParameterProperties` (`effGetParameterProperties`) — 136 bytes

The only structured parameter metadata VST 2.4 has. Field offsets are pinned by test.

| Offset | Field | Type | Meaning |
|---|---|---|---|
| 0 | `step_float` | `f32` | Normal step, normalized |
| 4 | `small_step_float` | `f32` | Fine step (shift-drag) |
| 8 | `large_step_float` | `f32` | Coarse step (page up/down) |
| 12 | `label` | `[u8; 64]` | Full parameter label |
| 76 | `flags` | `i32` | `ParameterFlags` |
| 80 | `min_integer` | `i32` | Lowest integer value |
| 84 | `max_integer` | `i32` | Highest integer value |
| 88 | `step_integer` | `i32` | Integer step |
| 92 | `large_step_integer` | `i32` | Coarse integer step |
| 96 | `short_label` | `[u8; 8]` | Short label (6 chars + delimiter) |
| 104 | `display_index` | `i16` | Preferred UI position |
| 106 | `category` | `i16` | Category, **1-based**; 0 = uncategorised |
| 108 | `num_parameters_in_category` | `i16` | Siblings in this category |
| 110 | `reserved` | `i16` | Zero |
| 112 | `category_label` | `[u8; 8]` | Category name |
| 120 | `future` | `[u8; 16]` | Reserved |

`ParameterFlags`:

| Constant | Bit | Value | Gates |
|---|---|---|---|
| `USES_INT_STEP` | 0 | 1 | `step_integer`, `min_integer`, `max_integer` |
| `USES_FLOAT_STEP` | 1 | 2 | `step_float`, `small_step_float`, `large_step_float` |
| `USES_INDEX` | 2 | 4 | `display_index` |
| `USES_CATEGORY` | 3 | 8 | `category`, `num_parameters_in_category`, `category_label` |
| `CAN_RAMP` | 4 | 16 | (doc comment in the bindings says "parameter cannot be automated" — that contradicts the constant's name, which in the SDK is `kVstParameterCanRamp`. Treat the doc comment as suspect) |

Reading `min_integer`/`max_integer` without checking `USES_INT_STEP` yields whatever the
plugin left there — usually zero, so the parameter silently collapses to the range 0..0.

### `MidiProgramName` (80 bytes)

| Offset | Field | Type | Meaning |
|---|---|---|---|
| 0 | `this_program_index` | `i32` | **Host writes this** — the question |
| 4 | `name` | `[u8; 64]` | Program name |
| 68 | `midi_program` | `u8` | Program-change number 0-127 |
| 69 | `midi_bank_msb` | `u8` | Bank select MSB (CC 0), or **255 = unused** |
| 70 | `midi_bank_lsb` | `u8` | Bank select LSB (CC 32), or **255 = unused** |
| 71 | `reserved` | `u8` | Zero |
| 72 | `parent_category_index` | `i32` | Category index, or **-1 = uncategorised** |
| 76 | `flags` | `i32` | `MidiProgramFlags` |

`MidiProgramFlags`: `IS_OMNI = 1` — the program is a GM drum kit, so its keys are separate
instruments and `effGetMidiKeyName` is how to label them.

### `MidiProgramCategory` (76 bytes)

| Offset | Field | Type | Meaning |
|---|---|---|---|
| 0 | `this_category_index` | `i32` | **Host writes this** |
| 4 | `name` | `[u8; 64]` | Category name |
| 68 | `parent_category_index` | `i32` | Parent, or -1 at top level |
| 72 | `flags` | `i32` | Zero |

### `MidiKeyName` (80 bytes)

| Offset | Field | Type | Meaning |
|---|---|---|---|
| 0 | `this_program_index` | `i32` | **Host writes this** |
| 4 | `this_key_number` | `i32` | **Host writes this** — MIDI note 0-127 |
| 8 | `keyname` | `[u8; 64]` | Plugin's label for the key |
| 72 | `reserved` | `i32` | Zero |
| 76 | `flags` | `i32` | Zero |

### `ChannelProperties` (`effGetInputProperties` / `effGetOutputProperties`)

| Field | Type | Meaning |
|---|---|---|
| `name` | `[u8; 64]` | Channel name |
| `flags` | `i32` | `ChannelFlags` |
| `arrangement_type` | `SpeakerArrangementType` (`i32`) | Speaker arrangement this channel belongs to |
| `short_name` | `[u8; 8]` | Short name |
| `future` | `[u8; 48]` | Reserved |

`ChannelFlags`: `ACTIVE = 1` (ignored by host), `STEREO = 2` (first of a stereo pair),
`SPEAKER = 4` (use `arrangement_type` rather than the stereo flag).

### `SpeakerArrangementType` (`#[repr(i32)]`, -2..=28)

`Custom = -2`, `Empty = -1`, `Mono = 0`, then contiguously:
`Stereo`(1), `StereoSurround`(2), `StereoCenter`(3), `StereoSide`(4), `StereoCLfe`(5),
`Cinema30`(6), `Music30`(7), `Cinema31`(8), `Music31`(9), `Cinema40`(10), `Music40`(11),
`Cinema41`(12), `Music41`(13), `Surround50`(14), `Surround51`(15), `Cinema60`(16),
`Music60`(17), `Cinema61`(18), `Music61`(19), `Cinema70`(20), `Music70`(21),
`Cinema71`(22), `Music71`(23), `Cinema80`(24), `Music80`(25), `Cinema81`(26),
`Music81`(27), `Surround102`(28).

Cinema and Music variants at the same channel count are technically identical layouts;
the distinction is informational.

### `VstVariableIo` (`effProcessVarIo`, opcode 41)

**Not defined anywhere in the bindings.** The opcode names the struct in a doc comment and
nothing else. Implementing variable-rate/time-stretch offline I/O requires going back to
the SDK header.

### Other supporting types

- `ProcessLevel` (`audioMasterGetCurrentProcessLevel`): `Unknown = 0`, `User = 1`,
  `Realtime = 2`, `Prefetch = 3`, `Offline = 4`.
- `HostLanguage` (`audioMasterGetLanguage`): `English = 1` .. `Japanese = 6`.
- `Category` (`effGetPlugCategory`, `#[repr(isize)]`): `Unknown = 0`, `Effect`, `Synth`,
  `Analysis`, `Mastering`, `Spacializer`, `RoomFx`, `SurroundFx`, `Restoration`,
  `OfflineProcess`, `Shell`, `Generator` (= 11).
- `KnobMode` (`effSetEditKnobMode`): `Circular = 0`, `CircularRelative = 1`, `Linear = 2`.
- `Key`: 55 platform-independent key codes, `None = 0` .. `Equals = 54`.
- `ModifierKey` bitflags: `SHIFT = 1`, `ALT = 2`, `COMMAND = 4`, `CONTROL = 8`.
  **The doc comments are swapped**: `COMMAND` is documented "Control on mac" and `CONTROL`
  "Command on mac, ctrl on other". One of the two is wrong.
- `FileSelectCommand`: `Load = 0`, `Save`, `LoadMultipleFiles`, `SelectDirectory`.
- `FileSelect` / `FileType`: descriptor structs for `audioMasterOpenFileSelector`.
- `Rect`: `{ top, left, bottom, right }`, all `i16`. Note the **field order is
  top/left/bottom/right**, not the left/top/right/bottom you might assume.
- String length constants: `MAX_PRESET_NAME_LEN = 24`, `MAX_PARAM_STR_LEN = 32`,
  `MAX_LABEL = 64`, `MAX_SHORT_LABEL = 8`, `MAX_PRODUCT_STR_LEN = 64`,
  `MAX_VENDOR_STR_LEN = 64`.

---

## E. Contract quirks and traps

Places where VST 2.4 is unusual, inverted, or differs from other plugin formats.

### 1. `effGetTailSize` inverts zero and one

```rust
if get_plugin().get_tail_size() == 0 { return 1; } else { return get_plugin().get_tail_size(); }
```

- **0** = "unknown / use the host default"
- **1** = "**no tail**"
- **n > 1** = tail length in samples

So the natural C default of 0 means "I don't know", and a plugin that genuinely has no tail
must say 1. A host reading 1 as "1 sample of tail" is technically harmless; a host reading 0
as "no tail" truncates every reverb. Inverted relative to every other format, where 0 means
no tail.

The bindings' plugin-side `Plugin::get_tail_size` default returns 0, which the dispatcher
then rewrites to 1 — i.e. the default plugin claims "no tail", not "unknown".

### 2. Boolean returns are `> 0`, not `== 1`

Shipping plugins return other positive values for success — some return the window handle
from `effEditOpen`. The host-side code was fixed from `== 1` to `> 0` for `effEditOpen`,
`effSetChunk`, `effCanBeAutomated` and `effString2Parameter`, because `== 1` reported
successful opens and accepted presets as refusals.

**The exceptions**, where the bindings deliberately keep `== 1`:
- `effGetParameterProperties` — plugins return `-1` for "no", and `!= 0` would read that as yes
- `effGetMidiKeyName` — spec says "1 = supported, 0 = not"
- `effHasMidiProgramsChanged` — a cache-invalidation signal; a garbage non-zero would mean
  "changed" forever

So there is no single rule. Per-opcode.

### 3. `effGetChunk` has four distinct failure shapes

The return is a **length**, and the pointer comes back through `ptr` as a `void**` the
plugin overwrites with its *own* buffer:

| len | ptr | Meaning |
|---|---|---|
| `0` | null | Nothing saved. Normal for a chunk-capable plugin before any preset loads. **`slice::from_raw_parts(null, 0)` is UB even at length 0** |
| `< 0` | any | Explicit error. `-1 as usize` = `usize::MAX` → a 16-exbibyte allocation |
| `> 0` | null | Plugin sized the chunk but failed to hand back the buffer |
| `> 0` | valid | The good case |

"Nothing saved" and "the save failed" must not collapse into the same empty `Vec` — a host
that conflates them downgrades a failed chunk save to a parameter snapshot and silently
drops the plugin's non-parameter state. The bindings model this as
`ChunkError::{Failed, NullBuffer}`.

The plugin owns the returned buffer and the contract keeps it valid only **until the next
dispatch**. Copy it immediately.

### 4. `audioMasterGetTime` returns a pointer in an integer return slot

`isize` return, cast to `*const TimeInfo`; `0` means "not supported". The bindings' host
side stores the `TimeInfo` in a **thread-local `Cell`** and returns its address, because
the host has to hand back memory that outlives the call but is not per-call allocated.
A host must ensure the pointed-to storage survives until the plugin has read it, and
handle being called from the audio thread.

### 5. `audioMasterGetSampleRate` returns the rate through the integer slot

`return host.get_sample_rate() as isize` — an f32 truncated to an integer, over the same
channel as `GetBlockSize`. Non-integer rates cannot be expressed. (Contrast
`effSetSampleRate`, which uses the `f32` `opt`.)

### 6. `audioMasterAutomate` puts the value in `opt`, `effSetProgram` puts the number in `value`

Neither is where you would guess. Also `effSetSampleRate` uses `opt` while
`effSetBlockSize` uses `value` — the two configuration opcodes disagree with each other.

### 7. `effSetSpeakerArrangement` passes a pointer through `value`

The only opcode where `value` is a pointer rather than an integer. Input arrangement in
`value`, output in `ptr`.

### 8. Editor key modifiers travel as reinterpreted f32 bits

`effEditKeyDown`/`Up` carry the modifier bitmask in `opt` (an `f32`), and the receiver
reads it with `opt.to_bits() as u8`. That is a bit reinterpretation, not a numeric
conversion — writing `mask as f32` produces the wrong bits.

### 9. `effEditOpen` is not idempotent, and neither is `effEditClose`

VST 2.4 requires an `effEditClose` between two `effEditOpen`s; opening twice leaks the
first window. A second `effEditClose` has **no defined meaning** and several real plugins
double-free their window resources on it. Hosts routinely have two closing paths (explicit
close + `Drop`), so the guard is mandatory.

`effEditGetRect` may legitimately fail until the editor is open — do not require it as a
precondition for "has an editor". Use `effFlagsHasEditor`.

### 10. `effClose` frees the plugin's own `AEffect`

Dispatch it exactly once, and never dereference the pointer afterwards. It also **must**
happen: a licensed plugin releases its session seat on `effClose`, and skipping it leaks
one live instance and one licence per A/B of a slot.

Separately: the module itself is **not** `dlclose`d. JUCE-based plugins (and others) crash
in their static destructor sequence on unload, so hosts leak the library handle
deliberately. These are two distinct events — conflating them is what caused the leak.

### 11. Unwinding across the FFI boundary is UB, in both directions

Both `interfaces::dispatch` (host→plugin) and `callback_wrapper` (plugin→host) wrap their
whole body in `catch_unwind` and degrade a panic to `0` — the VST2 "unhandled" answer for
every opcode. A panic escaping into the other side's C++ frame is undefined behaviour.

Concretely: `Supported::from` used to panic on undocumented `effCanDo` returns, which
aborted **during plugin scanning**, since a host probes canDo on every plugin it finds.

### 12. The host callback is called from the audio thread — do not put a lock on it

`audioMasterGetTime` is called from inside `processReplacing` in nearly every synth, along
with `audioMasterAutomate` and `audioMasterProcessEvents`. `audioMasterSizeWindow` and
`audioMasterUpdateDisplay` come from the UI thread. A `std::sync::Mutex` shared by both is
a priority inversion — the audio thread blocks on the GUI thread and the stream drops out.
The bindings changed `Arc<Mutex<T>>` to a plain `Arc<T>` with `&self` methods throughout;
implementors own their interior mutability (atomics / `ArcSwap` / lock-free channels).

### 13. Optional opcodes do not touch your buffer — detect absence by return value only

An unimplemented opcode falls through the plugin's dispatcher and returns 0 **without
writing anything**. So:

- Passing `MaybeUninit::uninit()` and calling `assume_init()` unconditionally reads
  uninitialized memory — UB before any garbage value is ever interpreted. Zero the buffer
  instead.
- Inspecting the buffer cannot distinguish "unimplemented" from "the plugin really means
  zero". Only the return value can.

Measured against three shipping plugins (TAL-NoiseMaker, TAL-Reverb-4, TDR Nova):
**all three return 0 from `effGetParameterProperties` for every parameter**. Absence is the
common case, not an error — fall back to the name/label pair.

### 14. `effGetCurrentMidiProgram`'s zero return is irreducibly ambiguous

The value is a program *index*, so `0` is both a valid answer and what an unimplemented
opcode returns. No amount of care at that call site resolves it. Disambiguate with an
opcode whose zero is unambiguous — `effGetMidiProgramName` returns a *serviced count*,
where 0 genuinely means unsupported.

### 15. Plugin-written enums and flags must be screened before they are materialised

`ChannelProperties::arrangement_type` is a `#[repr(i32)]` enum the *plugin* filled in.
Valid discriminants are **-2..=28**; simply `match`ing on an out-of-range value is UB. Read
the raw `i32` and clamp before letting it become the enum type.

Note the bindings' own comment in `host.rs::channel_properties` says "valid discriminants
are -2..=27", which is off by one — `Surround102` is 28. The *code* is correct (it computes
`MAX` from `Surround102 as i32` rather than hardcoding), only the comment is wrong.

Similarly `ChannelProperties::flags`: VST 2.4 defines 3 bits and says nothing about the
other 29, so a plugin setting a private bit is legal. `from_bits_truncate`, never
`from_bits().expect()` — the latter turns a benign plugin quirk into a host crash from
inside an `extern "C"` call chain.

(`event.rs` still has this bug: `api::MidiEventFlags::from_bits(event.flags).unwrap()` will
panic on any undefined bit in an incoming MIDI event's flags.)

### 16. `AEffect::reserved1` is documented "reserved for host, must be 0" — and the bindings use it

`PluginLoader::instance` stashes a `Box<Arc<Host>>` pointer there so `callback_wrapper` can
find the host on later calls. It is the host's field to use, but note that a
`LOAD_POINTER` static covers the window *before* it is set, during
`VSTPluginMain` — and that static is documented as failing if two plugins with two
different host instances load simultaneously.

### 17. The accumulating `process` adds; the replacing one overwrites

Falling back to the deprecated `_process` (which JUCE also does) requires **zeroing the
output buffers first**, because it accumulates into them. `processReplacing` overwrites,
so it does not. There is no accumulating counterpart for `f64` — `processDoubleReplacing`
is the only 64-bit entry point, so a plugin without `CAN_DOUBLE_REPLACING` must be driven
through the `f32` path.

### 18. `Events::events` is a `[*mut Event; 2]` read past its declared bound

The VST standard declares a variable-length array with initial size 2. For more than two
events, a larger allocation is stored there and indexed beyond 2. `num_events` is the real
count; the `2` is a C-ism, not a limit.

### 19. Both opcode enums are positional and unnumbered

Neither `plugin::OpCode` nor `host::OpCode` writes out most of its discriminants — they
rely on declaration order. Inserting a variant silently renumbers everything below it and
re-points every dispatch. `host::OpCode` has one explicit jump (`_PinConnected = 4` to
`_WantMidi = 6`), which is the only thing preventing a 1-off shift across 43 opcodes.

---

## Gaps in the bindings (not in the format)

Things a host built on `vst-tutti` must supply itself:

| Gap | Detail |
|---|---|
| `VstVariableIo` | Struct never defined. `effProcessVarIo` (41) is unusable |
| `VstSpeakerArrangement` | Never defined. `effSetSpeakerArrangement` (42) / `effGetSpeakerArrangement` (69) unusable |
| `VstPatchChunkInfo` | Never defined. `effBeginLoadBank` (75) / `effBeginLoadPreset` (76) unusable |
| `PanLaw` | Never defined. `effSetPanLaw` (74) unusable |
| `VstAudioFile` / `VstOfflineTask` | Never defined. The whole offline family (38-40 plugin-side, 25-29 host-side) unusable |
| `audioMasterUpdateDisplay` (42) | Sent by the plugin side, **not decoded** by `host_dispatch`; `Host::update_display` is never called |
| `audioMasterCanDo` (37) | Only logs the string, always returns 0 — the host advertises no capabilities |
| Host-can-do string vocabulary | Not modelled at all (no enum, no constants) |
| `audioMasterVendorSpecific` (35) | Decoded but unhandled |
| `audioMasterGetLanguage` (38), `GetDirectory` (41), `OpenFileSelector` (45), `CloseFileSelector` (46) | Decoded but unhandled |
| `effGetParameterProperties` (56) plugin-side | Dispatch arm is commented out (`//OpCode::GetParamInfo => { /*TODO*/ }`) |
| MIDI program/key opcodes (62-66) plugin-side | All marked `//TODO: Implement`; host side is implemented |
| `effGetTailSize` doc/impl mismatch | `ParameterFlags::CAN_RAMP` doc comment says "cannot be automated", contradicting the name |
| `ModifierKey` doc comments | `COMMAND` and `CONTROL` descriptions appear swapped |
| `event.rs` `from_bits().unwrap()` | Will panic on any undefined MIDI event flag bit — the same class of bug already fixed in `channels.rs` |
| `Rect` ownership | `effEditGetRect` leaks: the plugin-side arm `Box::into_raw`s a `Rect` with a `// TODO: free memory`, and the host side has `// TODO: Who owns rect?` |
