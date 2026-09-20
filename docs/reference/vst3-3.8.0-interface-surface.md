# VST3 3.8.0 - host-facing interface surface

**Source:** Steinberg VST3 SDK (MIT), `vst3sdk/pluginterfaces/`, `kVstVersionString = "VST 3.8.0"` (commit 58f8da7)
**Extracted:** 2026-08-03. Read from the SDK headers only - no tutti code was consulted, so this describes the format, not our coverage of it.
**Scope:** host's perspective. Every interface is marked plugin-implements (host calls it) or host-implements (plugin calls it).

Every signature below is copied **verbatim** from the header, including the SDK's own inline `/*in*/`, `/*out*/`, `/*inout*/` direction comments — those carry real information about who owns and fills each argument, and the SDK is not uniform about them (many older interfaces annotate nothing at all; that absence is reproduced here rather than guessed at). Where the SDK omits an annotation, none is shown.

Two direction annotations worth flagging up front because they are easy to assume wrong:

- `IAudioProcessor::getBusArrangement` takes `SpeakerArrangement& arr /*inout*/` — **not** `/*out*/`. The host passes a value in.
- `IUnitInfo`'s getters are almost all `/*inout*/` on their result parameter, not `/*out*/`.

## Root: FUnknown

Every interface derives from `FUnknown` (`base/funknown.h`), IID `0x00000000, 0x00000000, 0xC0000000, 0x00000046` — the COM `IUnknown` UUID.

```cpp
virtual tresult PLUGIN_API queryInterface (const TUID _iid, void** obj) = 0;
virtual uint32 PLUGIN_API addRef () = 0;
virtual uint32 PLUGIN_API release () = 0;
```

`tresult` codes (COM-compatible on Windows, a different numeric set on other platforms — do **not** hardcode `0x80004002` cross-platform): `kResultOk`/`kResultTrue` = 0, `kResultFalse` = 1, plus `kNoInterface`, `kInvalidArgument`, `kNotImplemented`, `kInternalError`, `kNotInitialized`, `kOutOfMemory`.

---

# A. Plugin-implements → Host calls

## A.1 Module entry & factory (`base/ipluginbase.h`)

Entry point: exported C symbol `IPluginFactory* PLUGIN_API GetPluginFactory()`, typedef'd as `typedef Steinberg::IPluginFactory* (PLUGIN_API *GetFactoryProc) ();`.

### IPluginFactory — `7A4D811C 52114A1F AED9D2EE 0B43BF9F`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getFactoryInfo (PFactoryInfo* info /*inout*/) = 0;` | Vendor / URL / email / flags |
| `virtual int32 PLUGIN_API countClasses () = 0;` | Number of exported classes |
| `virtual tresult PLUGIN_API getClassInfo (int32 index /*in*/, PClassInfo* info /*inout*/) = 0;` | Class cid/cardinality/category/name |
| `virtual tresult PLUGIN_API createInstance (FIDString cid /*in*/, FIDString _iid /*in*/, void** obj /*out*/) = 0;` | Instantiate a class |

### IPluginFactory2 — `0007B650 F24B4C0B A464EDB9 F00B2ABB` (extends IPluginFactory)

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getClassInfo2 (int32 index /*in*/, PClassInfo2* info /*out*/) = 0;` | Adds classFlags, subCategories, vendor, version, sdkVersion |

### IPluginFactory3 — `4555A2AB C1234E57 9B122910 36878931` (extends IPluginFactory2)

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getClassInfoUnicode (int32 index /*in*/, PClassInfoW* info /*out*/) = 0;` | UTF-16 name/vendor/version |
| `virtual tresult PLUGIN_API setHostContext (FUnknown* context /*in*/) = 0;` | Host hands its context to the factory (this is where a Linux `IRunLoop` reaches a plugin with no editor) |

### IPluginBase — `22888DDB 156E45AE 8358B348 08190625`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API initialize (FUnknown* context /*in*/) = 0;` | Base of IComponent & IEditController. `context` is mandatory and should implement `IHostApplication`. **If this returns non-`kResultOk`, the object is released immediately and `terminate` is NOT called.** Do heavy allocation here, not in the ctor |
| `virtual tresult PLUGIN_API terminate () = 0;` | Release all host interface references |

### IPluginCompatibility — `4AFD4B6A 35D7C240 A5C31414 FB7D15E6` (`base/iplugincompatibility.h`)

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getCompatibilityJSON (IBStream* stream) = 0;` | Old-cid→new-cid mapping as UTF-8 JSON5, for plugins without `moduleinfo.json`. Class category `"Plugin Compatibility Class"`. **The host ignores this class if the bundle has a moduleinfo.json** |

### Factory structs

```cpp
struct PFactoryInfo
{
    enum FactoryFlags
    {
        kNoFlags                 = 0,
        kClassesDiscardable      = 1 << 0,
        kLicenseCheck            = 1 << 1,   // DEPRECATED - see below
        kComponentNonDiscardable = 1 << 3,
        kUnicode                 = 1 << 4
    };
    enum { kURLSize = 256, kEmailSize = 128, kNameSize = 64 };

    char8 vendor[kNameSize];
    char8 url[kURLSize];
    char8 email[kEmailSize];
    int32 flags;
};

struct PClassInfo
{
    enum ClassCardinality { kManyInstances = 0x7FFFFFFF };
    enum { kCategorySize = 32, kNameSize = 64 };

    TUID  cid;                        // 16-byte class GUID
    int32 cardinality;
    char8 category[kCategorySize];
    char8 name[kNameSize];
};

struct PClassInfo2
{
    TUID   cid;
    int32  cardinality;
    char8  category[PClassInfo::kCategorySize];
    char8  name[PClassInfo::kNameSize];
    enum { kVendorSize = 64, kVersionSize = 64, kSubCategoriesSize = 128 };
    uint32 classFlags;
    char8  subCategories[kSubCategoriesSize];
    char8  vendor[kVendorSize];
    char8  version[kVersionSize];
    char8  sdkVersion[kVersionSize];
};

struct PClassInfoW      // identical to PClassInfo2 but name/vendor/version/sdkVersion are char16
{
    TUID   cid;
    int32  cardinality;
    char8  category[PClassInfo::kCategorySize];
    char16 name[PClassInfo::kNameSize];
    enum { kVendorSize = 64, kVersionSize = 64, kSubCategoriesSize = 128 };
    uint32 classFlags;
    char8  subCategories[kSubCategoriesSize];
    char16 vendor[kVendorSize];
    char16 version[kVersionSize];
    char16 sdkVersion[kVersionSize];
};
```

`kLicenseCheck` is **DEPRECATED** — header says "do not use anymore, resp. it will get ignored from Cubase/Nuendo 12 and later".

Class categories: `kVstAudioEffectClass` = `"Audio Module Class"` (processor), `kVstComponentControllerClass` = `"Component Controller Class"` (controller), `kPluginCompatibilityClass` = `"Plugin Compatibility Class"`.

`ComponentFlags` (classFlags in PClassInfo2): `kDistributable = 1<<0`, `kSimpleModeSupported = 1<<1`.

`Vst::kDefaultFactoryFlags = PFactoryInfo::kUnicode`.

## A.2 The mandatory core

| Interface | IID | Header | Notes |
|---|---|---|---|
| **IComponent** (extends IPluginBase) | `E831FF31 F2D54301 928EBBEE 25697802` | `ivstcomponent.h` | mandatory, 3.0.0 |
| **IAudioProcessor** | `42043F99 B7DA453C A569E79D 9AAEC33D` | `ivstaudioprocessor.h` | mandatory, 3.0.0 |
| **IEditController** (extends IPluginBase) | `DCD7BBE3 7742448D A874AACC 979C759E` | `ivsteditcontroller.h` | mandatory, 3.0.0 |
| **IConnectionPoint** | `70A4156F 6E6E4026 989148BF AA60D8D1` | `ivstmessage.h` | mandatory, 3.0.0 — **both plugin AND host implement it** |

### IComponent

| Signature | Purpose / thread+state note from header |
|---|---|
| `virtual tresult PLUGIN_API getControllerClassId (TUID classId /*out*/) = 0;` | cid of the paired IEditController. `[UI-thread & Created]` |
| `virtual tresult PLUGIN_API setIoMode (IoMode mode /*in*/) = 0;` | Optional, called **before** `initialize`. `[UI-thread & Created]` |
| `virtual int32 PLUGIN_API getBusCount (MediaType type /*in*/, BusDirection dir /*in*/) = 0;` | `[UI-thread & Initialized]` |
| `virtual tresult PLUGIN_API getBusInfo (MediaType type /*in*/, BusDirection dir /*in*/, int32 index /*in*/, BusInfo& bus /*out*/) = 0;` | `[UI-thread & Initialized]` |
| `virtual tresult PLUGIN_API getRoutingInfo (RoutingInfo& inInfo /*in*/, RoutingInfo& outInfo /*out*/) = 0;` | inInfo always refers to an input bus, outInfo must refer to an output bus. `[UI-thread & Initialized]` |
| `virtual tresult PLUGIN_API activateBus (MediaType type /*in*/, BusDirection dir /*in*/, int32 index /*in*/, TBool state /*in*/) = 0;` | `[UI-thread & Setup Done]`. An already-active bus needs no reactivation after `setBusArrangements` |
| `virtual tresult PLUGIN_API setActive (TBool state /*in*/) = 0;` | `[UI-thread & Setup Done]` |
| `virtual tresult PLUGIN_API setState (IBStream* state /*in*/) = 0;` | Processor state (the one that gets saved) |
| `virtual tresult PLUGIN_API getState (IBStream* state /*inout*/) = 0;` | Note `/*inout*/` — the host supplies a stream to write into |

### IAudioProcessor

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API setBusArrangements (SpeakerArrangement* inputs /*in*/, int32 numIns /*in*/, SpeakerArrangement* outputs /*in*/, int32 numOuts /*in*/) = 0;` | Three legal plugin responses; `kResultTrue` = accepted as-is, `kResultFalse` = plugin adapted/kept something else — **re-read `getBusArrangement` after a false**. `[UI-thread & (Initialized \| Connected \| Setup Done)]` |
| `virtual tresult PLUGIN_API getBusArrangement (BusDirection dir /*in*/, int32 index /*in*/, SpeakerArrangement& arr /*inout*/) = 0;` | Must agree with `IComponent::getBusInfo`. **`/*inout*/`, not `/*out*/`** |
| `virtual tresult PLUGIN_API canProcessSampleSize (int32 symbolicSampleSize /*in*/) = 0;` | `kSample32` / `kSample64`. `[UI-thread & (Initialized \| Connected)]` |
| `virtual uint32 PLUGIN_API getLatencySamples () = 0;` | Group delay. `[UI-thread & Setup Done]`. Changes must be announced via `restartComponent(kLatencyChanged)` |
| `virtual tresult PLUGIN_API setupProcessing (ProcessSetup& setup /*in*/) = 0;` | Called in **disabled** state, before `setProcessing`. `[UI-thread & (Initialized \| Connected)]` |
| `virtual tresult PLUGIN_API setProcessing (TBool state /*in*/) = 0;` | May be UI **or** processing thread; keep it light (no allocation). `setProcessing(false)` may follow `(true)` with zero process calls. `[(UI-thread or processing-thread) & Activated]` |
| `virtual tresult PLUGIN_API process (ProcessData& data /*in*/) = 0;` | `[processing-thread & Processing]` |
| `virtual uint32 PLUGIN_API getTailSamples () = 0;` | `kNoTail = 0`, `kInfiniteTail = kMaxInt32u`, else `x * sampleRate`. `[UI-thread & Setup Done]` |

### IEditController

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API setComponentState (IBStream* state /*in*/) = 0;` | Controller receives the **processor's** state to sync its mirror |
| `virtual tresult PLUGIN_API setState (IBStream* state /*in*/) = 0;` | Controller-only (UI) state — a separate stream from the component's |
| `virtual tresult PLUGIN_API getState (IBStream* state /*inout*/) = 0;` | |
| `virtual int32 PLUGIN_API getParameterCount () = 0;` | |
| `virtual tresult PLUGIN_API getParameterInfo (int32 paramIndex /*in*/, ParameterInfo& info /*out*/) = 0;` | Indexed, not by id |
| `virtual tresult PLUGIN_API getParamStringByValue (ParamID id /*in*/, ParamValue valueNormalized /*in*/, String128 string /*out*/) = 0;` | |
| `virtual tresult PLUGIN_API getParamValueByString (ParamID id /*in*/, TChar* string /*in*/, ParamValue& valueNormalized /*out*/) = 0;` | |
| `virtual ParamValue PLUGIN_API normalizedParamToPlain (ParamID id /*in*/, ParamValue valueNormalized /*in*/) = 0;` | |
| `virtual ParamValue PLUGIN_API plainParamToNormalized (ParamID id /*in*/, ParamValue plainValue /*in*/) = 0;` | |
| `virtual ParamValue PLUGIN_API getParamNormalized (ParamID id /*in*/) = 0;` | |
| `virtual tresult PLUGIN_API setParamNormalized (ParamID id /*in*/, ParamValue value /*in*/) = 0;` | **The controller must never echo this back to the host via IComponentHandler** — GUI update only |
| `virtual tresult PLUGIN_API setComponentHandler (IComponentHandler* handler /*in*/) = 0;` | **Mandatory if the host uses IEditController at all**. `[UI-thread & Initialized]` |
| `virtual IPlugView* PLUGIN_API createView (FIDString name /*in*/) = 0;` | Only `ViewType::kEditor` = `"editor"` is defined. View lifetime never exceeds the controller's |

All IEditController methods above are `[UI-thread & Connected]` except `setComponentHandler`, which is `[UI-thread & Initialized]`.

### IConnectionPoint (both directions — `[plug imp]` and `[host imp]`)

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API connect (IConnectionPoint* other /*in*/) = 0;` | `[UI-thread & Initialized]` |
| `virtual tresult PLUGIN_API disconnect (IConnectionPoint* other /*in*/) = 0;` | `[UI-thread & Connected]` |
| `virtual tresult PLUGIN_API notify (IMessage* message /*in*/) = 0;` | `[UI-thread & Connected]` |

Some hosts insert a **proxy** between the two components rather than connecting them directly.

## A.3 Plugin-side optional / extension interfaces (the long tail)

### IAudioPresentationLatency — `309ECE78 EB7D4FAE 8B2225D9 09FD08B6` · extends IAudioProcessor · 3.1.0 · `ivstaudioprocessor.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API setAudioPresentationLatencySamples (BusDirection dir /*in*/, int32 busIndex /*in*/, uint32 latencyInSamples /*in*/) = 0;` | Tells the plugin how far it is from input acquisition / output presentation. Zero = none *or unknown*. `[UI-thread & Activated]` |

### IProcessContextRequirements — `2A654303 EF764E3D 95B5FE83 730EF6D0` · extends IAudioProcessor · 3.7.0 · `ivstaudioprocessor.h`

| Signature | Purpose |
|---|---|
| `virtual uint32 PLUGIN_API getProcessContextRequirements () = 0;` | **Marked `[mandatory]` in the header despite being an extension** — a plugin that doesn't implement it "may not get any information at all" in ProcessContext. Host asks once between `initialize` and `setActive`; cannot change afterwards. `[UI-thread & Setup Done]` |

### IEditController2 — `7F4EFE59 F3204967 AC27A3AE AFB63038` · extends IEditController · 3.1.0 · `ivsteditcontroller.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API setKnobMode (KnobMode mode /*in*/) = 0;` | `kResultFalse` = unsupported mode |
| `virtual tresult PLUGIN_API openHelp (TBool onlyCheck /*in*/) = 0;` | `onlyCheck` probes support without acting |
| `virtual tresult PLUGIN_API openAboutBox (TBool onlyCheck /*in*/) = 0;` | |

### IMidiMapping — `DF0FF9F7 49B74669 B63AB732 7ADBF5E5` · extends IEditController · 3.0.1 · `ivsteditcontroller.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getMidiControllerAssignment (int32 busIndex /*in*/, int16 channel /*in*/, CtrlNumber midiControllerNumber /*in*/, ParamID& id /*out*/) = 0;` | `midiControllerNumber` "could be bigger than 127" — see ControllerNumbers in C.10 |

### IMidiMapping2 — `6DE14B88 03F94F09 A2552F0F 9326593E` · extends IEditController · **3.8.0** · `ivstmidimapping2.h`

**Explicitly `[replaces Vst::IMidiMapping]`.** A MIDI 2.0 capable host queries this first and uses IMidiMapping as fallback. Lists are host-preallocated; the plugin fills them.

| Signature | Purpose |
|---|---|
| `virtual uint32 PLUGIN_API getNumMidi2ControllerAssignments (BusDirections direction) = 0;` | |
| `virtual tresult PLUGIN_API getMidi2ControllerAssignments (BusDirections direction, const Midi2ControllerParamIDAssignmentList& list) = 0;` | Note: `const` reference to a list whose `map` array the plugin writes through |
| `virtual uint32 PLUGIN_API getNumMidi1ControllerAssignments (BusDirections direction) = 0;` | |
| `virtual tresult PLUGIN_API getMidi1ControllerAssignments (BusDirections direction, const Midi1ControllerParamIDAssignmentList& list) = 0;` | |

```cpp
using MidiGroup   = uint8;
using MidiChannel = uint8;
using BusIndex    = int32;

struct Midi2Controller
{
    uint8 bank : 7;        // msb
    TBool registered : 1;  // true: registered, false: assignable
    uint8 index : 7;       // lsb
    TBool reserved : 1;
};

struct Midi2ControllerParamIDAssignment
{
    ParamID         pId;
    BusIndex        busIndex;
    MidiChannel     channel;
    Midi2Controller controller;
};

struct Midi2ControllerParamIDAssignmentList
{
    uint32 count;
    Midi2ControllerParamIDAssignment* map;
};

struct Midi1ControllerParamIDAssignment
{
    ParamID     pId;
    BusIndex    busIndex;
    MidiChannel channel;
    CtrlNumber  controller;
};

struct Midi1ControllerParamIDAssignmentList
{
    uint32 count;
    Midi1ControllerParamIDAssignment* map;
};
```

### IEditControllerHostEditing — `C1271208 70594098 B9DD34B3 6BB0195E` · extends IEditController · 3.5.0 · `ivsteditcontroller.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API beginEditFromHost (ParamID paramID /*in*/) = 0;` | Host brackets its own `setParamNormalized` runs on non-automatable/non-readonly/non-hidden helper params |
| `virtual tresult PLUGIN_API endEditFromHost (ParamID paramID /*in*/) = 0;` | |

### INoteExpressionController — `B7F8F859 41234872 91169581 4F3721A3` · extends IEditController · 3.5.0 · `ivstnoteexpression.h`

| Signature | Purpose |
|---|---|
| `virtual int32 PLUGIN_API getNoteExpressionCount (int32 busIndex /*in*/, int16 channel /*in*/) = 0;` | |
| `virtual tresult PLUGIN_API getNoteExpressionInfo (int32 busIndex /*in*/, int16 channel /*in*/, int32 noteExpressionIndex /*in*/, NoteExpressionTypeInfo& info /*out*/) = 0;` | |
| `virtual tresult PLUGIN_API getNoteExpressionStringByValue (int32 busIndex /*in*/, int16 channel /*in*/, NoteExpressionTypeID id /*in*/, NoteExpressionValue valueNormalized /*in*/, String128 string /*out*/) = 0;` | |
| `virtual tresult PLUGIN_API getNoteExpressionValueByString (int32 busIndex /*in*/, int16 channel /*in*/, NoteExpressionTypeID id /*in*/, const TChar* string /*in*/, NoteExpressionValue& valueNormalized /*out*/) = 0;` | |

### IKeyswitchController — `1F2F76D3 BFFB4B96 B99527A5 5EBCCEF4` · extends IEditController · 3.5.0 · `ivstnoteexpression.h`

| Signature | Purpose |
|---|---|
| `virtual int32 PLUGIN_API getKeyswitchCount (int32 busIndex /*in*/, int16 channel /*in*/) = 0;` | |
| `virtual tresult PLUGIN_API getKeyswitchInfo (int32 busIndex /*in*/, int16 channel /*in*/, int32 keySwitchIndex /*in*/, KeyswitchInfo& info /*out*/) = 0;` | Feeds VST Expression Maps |

### INoteExpressionPhysicalUIMapping — `B03078FF 94D24AC8 90CCD303 D4133324` · extends IEditController · 3.6.11 · `ivstphysicalui.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getPhysicalUIMapping (int32 busIndex /*in*/, int16 channel /*in*/, PhysicalUIMapList& list /*inout*/) = 0;` | Maps X/Y/pressure to note-expression type ids. The MPE translation path. Host sets `physicalUITypeID` and `count`; plugin fills `noteExpressionTypeID` |

```cpp
typedef uint32 PhysicalUITypeID;

struct PhysicalUIMap
{
    PhysicalUITypeID     physicalUITypeID;      // set by the CALLER (host)
    NoteExpressionTypeID noteExpressionTypeID;  // filled by the PLUGIN; kInvalidTypeID if none
};

struct PhysicalUIMapList
{
    uint32         count;   // set by the caller of getPhysicalUIMapping
    PhysicalUIMap* map;
};
```

### IUnitInfo — `3D4BD6B5 913A4FD2 A886E768 A5EB92C1` · extends IEditController · 3.0.0 · `ivstunits.h`

All methods are `[UI-thread & (Initialized | Connected)]`.

| Signature | Purpose |
|---|---|
| `virtual int32 PLUGIN_API getUnitCount () = 0;` | Flat count; always >= 1 (root is the component) |
| `virtual tresult PLUGIN_API getUnitInfo (int32 unitIndex, UnitInfo& info /*inout*/) = 0;` | |
| `virtual int32 PLUGIN_API getProgramListCount () = 0;` | |
| `virtual tresult PLUGIN_API getProgramListInfo (int32 listIndex, ProgramListInfo& info /*inout*/) = 0;` | |
| `virtual tresult PLUGIN_API getProgramName (ProgramListID listId, int32 programIndex, String128 name /*inout*/) = 0;` | |
| `virtual tresult PLUGIN_API getProgramInfo (ProgramListID listId, int32 programIndex, CString attributeId /*in*/, String128 attributeValue /*inout*/) = 0;` | attributeId keys from `vstpresetkeys.h` |
| `virtual tresult PLUGIN_API hasProgramPitchNames (ProgramListID listId, int32 programIndex) = 0;` | |
| `virtual tresult PLUGIN_API getProgramPitchName (ProgramListID listId, int32 programIndex, int16 midiPitch, String128 name /*inout*/) = 0;` | Changes announced via `IUnitHandler::notifyProgramListChange` |
| `virtual UnitID PLUGIN_API getSelectedUnit () = 0;` | |
| `virtual tresult PLUGIN_API selectUnit (UnitID unitId) = 0;` | |
| `virtual tresult PLUGIN_API getUnitByBus (MediaType type, BusDirection dir, int32 busIndex, int32 channel, UnitID& unitId /*inout*/) = 0;` | Mainly: which unit is a given MIDI input channel |
| `virtual tresult PLUGIN_API setUnitProgramData (int32 listOrUnitId, int32 programIndex, IBStream* data /*in*/) = 0;` | With IProgramListData the target is (listId, programIndex); with IUnitData the target is the unit and `programIndex < 0` |

```cpp
static const UnitID        kRootUnitId       = 0;   // root unit
static const UnitID        kNoParentUnitId   = -1;  // root has no parent
static const ProgramListID kNoProgramListId  = -1;  // unit uses no programs
static const int32         kAllProgramInvalid = -1; // for notifyProgramListChange

struct UnitInfo
{
    UnitID        id;
    UnitID        parentUnitId;
    String128     name;            // optional for root, required otherwise
    ProgramListID programListId;
};

struct ProgramListInfo
{
    ProgramListID id;
    String128     name;
    int32         programCount;
};
```

Invariants the host can rely on: root unit is the component itself so `getUnitCount() >= 1`; root id is `kRootUnitId = 0`; a unit's program-list reference must not change; each unit using a program list references one program of that list.

### IProgramListData — `8683B01F 7B354F70 A2651DEC 353AF4FF` · extends IComponent · 3.0.0 · `ivstunits.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API programDataSupported (ProgramListID listId) = 0;` | |
| `virtual tresult PLUGIN_API getProgramData (ProgramListID listId, int32 programIndex, IBStream* data /*inout*/) = 0;` | |
| `virtual tresult PLUGIN_API setProgramData (ProgramListID listId, int32 programIndex, IBStream* data /*in*/) = 0;` | |

### IUnitData — `6C389611 D391455D B870B833 94A0EFDD` · extends IComponent · 3.0.0 · `ivstunits.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API unitDataSupported (UnitID unitID) = 0;` | |
| `virtual tresult PLUGIN_API getUnitData (UnitID unitId, IBStream* data /*inout*/) = 0;` | |
| `virtual tresult PLUGIN_API setUnitData (UnitID unitId, IBStream* data /*in*/) = 0;` | |

A component supports program-list data via IProgramListData **or/and** unit preset data via IUnitData.

### IAutomationState — `B4E8287F 1BB346AA 83A46667 68937BAB` · extends IEditController · 3.6.5 · `ivstautomationstate.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API setAutomationState (int32 state /*in*/) = 0;` | `kNoAutomation=0`, `kReadState=1<<0`, `kWriteState=1<<1`, `kReadWriteState = kReadState \| kWriteState` |

### IPrefetchableSupport — `8AE54FDA E93046B9 A28555BC DC98E21E` · extends IComponent · 3.6.5 · `ivstprefetchablesupport.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getPrefetchableSupport (PrefetchableSupport& prefetchable /*out*/) = 0;` | `kIsNeverPrefetchable=0`, `kIsYetPrefetchable`, `kIsNotYetPrefetchable`, `kNumPrefetchableSupport`. Changes announced via `restartComponent(kPrefetchableSupportChanged)`. `typedef uint32 PrefetchableSupport` |

### IParameterFunctionName — `6D21E1DC 91199D4B A2A02FEF 6C1AE55C` · extends IEditController · 3.7.0 · `ivstparameterfunctionname.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getParameterIDFromFunctionName (UnitID unitID /*in*/, FIDString functionName /*in*/, ParamID& paramID /*inout*/) = 0;` | Returns `kResultFalse` and sets `kNoParamId` when absent. Lets a host draw e.g. a gain-reduction meter from a named param |

### IXmlRepresentationController — `A81A0471 48C34DC4 AC30C9E1 3C8393D5` · extends IEditController · 3.5.0 · `ivstrepresentation.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getXmlRepresentationStream (RepresentationInfo& info /*in*/, IBStream* stream /*inout*/) = 0;` | Hardware-remote page/cell/layer XML per DTD `http://dtd.steinberg.net/VST-Remote-1.1.dtd` |

```cpp
struct RepresentationInfo
{
    enum { kNameSize = 64 };
    char8 vendor[kNameSize];   // e.g. "Yamaha"
    char8 name[kNameSize];     // e.g. "O2"
    char8 version[kNameSize];  // e.g. "1.0"
    char8 host[kNameSize];     // optional: representation for one host only
};
```

### IParameterFinder — `0F618302 215D4587 A512073C 77B9D383` · extends IPlugView · 3.0.2 · `ivstplugview.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API findParameter (int32 xPos, int32 yPos, ParamID& resultTag /*out*/) = 0;` | Header: "highly recommended"; **all Steinberg hosts require it for the AI Knob**. `[UI-thread & (Initialized \| Connected) & plugView]` |

### IRemapParamID — `2B88021E 6286B646 B49DF76A 5663061C` · extends IEditController · 3.7.11 · `ivstremapparamid.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getCompatibleParamID (const TUID pluginToReplaceUID /*in*/, ParamID oldParamID /*in*/, ParamID& newParamID /*out*/) = 0;` | Plugin-replacement / VST2→VST3 automation remap. `oldParamID` is an *index* for VST2 plugins. `kResultFalse` leaves `newParamID` undefined. `[UI-thread & Initialized]` |

### IMidiLearn — `6B2449CC 419740B5 AB3C79DA C5FE5C86` · extends IEditController · 3.6.12 · `ivstmidilearn.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API onLiveMIDIControllerInput (int32 busIndex /*in*/, int16 channel /*in*/, CtrlNumber midiCC /*in*/) = 0;` | Host pushes live CC so the plugin can self-map; plugin then calls `restartComponent(kMidiCCAssignmentChanged)` |

### IMidiLearn2 — `F07E498A 78864327 8B431CED A3C553FC` · extends IEditController · **3.8.0** · `ivstmidimapping2.h`

`[replaces Vst::IMidiLearn]`.

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API onLiveMidi2ControllerInput (BusIndex index, MidiChannel channel, Midi2Controller midiCC) = 0;` | |
| `virtual tresult PLUGIN_API onLiveMidi1ControllerInput (BusIndex index, MidiChannel channel, CtrlNumber midiCC) = 0;` | |

### ChannelContext::IInfoListener — `0F194781 8D984ADA BBA0C1EF C011D8D0` · extends IEditController · 3.6.5 · `ivstchannelcontextinfo.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API setChannelContextInfos (IAttributeList* list /*in*/) = 0;` | Host pushes channel name/color/index/image/location on **every** change. Plugin must `addRef` if it keeps the list. `[UI-thread & (Initialized \| Connected \| Setup Done \| Activated \| Processing)]` |

### IDataExchangeReceiver — `45A759DC 84FA4907 ABCB6175 2FC786B6` · 3.7.9 · `ivstdataexchange.h`

All three return `void`, not `tresult`.

| Signature | Purpose |
|---|---|
| `virtual void PLUGIN_API queueOpened (DataExchangeUserContextID userContextID, uint32 blockSize, TBool& dispatchOnBackgroundThread) = 0;` | Plugin writes `dispatchOnBackgroundThread` to choose its delivery thread (defaults false = main thread) |
| `virtual void PLUGIN_API queueClosed (DataExchangeUserContextID userContextID) = 0;` | |
| `virtual void PLUGIN_API onDataExchangeBlocksReceived (DataExchangeUserContextID userContextID, uint32 numBlocks, DataExchangeBlock* blocks, TBool onBackgroundThread) = 0;` | **Block data is valid only inside this call** |

### IInterAppAudioConnectionNotification — `6020C72D 5FC24AA1 B0950DB5 D7D6D5CF` · extends IEditController · 3.6.0 · `ivstinterappaudio.h` · iOS only

| Signature | Purpose |
|---|---|
| `virtual void PLUGIN_API onInterAppAudioConnectionStateChange (TBool newState) = 0;` | `void` return |

### IInterAppAudioPresetManager — `ADE6FCC4 46C94E1D B3B49A80 C93FEFDD` · extends IEditController · 3.6.0 · `ivstinterappaudio.h` · iOS only

**Direction subtlety: the header lists it under plugin-implements, but it is created by the *host* via `IInterAppAudioHost::createPresetManager(cid)`.**

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API runLoadPresetBrowser () = 0;` | |
| `virtual tresult PLUGIN_API runSavePresetBrowser () = 0;` | |
| `virtual tresult PLUGIN_API loadNextPreset () = 0;` | |
| `virtual tresult PLUGIN_API loadPreviousPreset () = 0;` | |

### IContextMenuTarget — `3CDF2E75 85D34144 BF86D36B D7C4894D` · 3.5.0 · `ivstcontextmenu.h`

**`[host imp]` AND `[plug imp]`**: whoever contributed the menu item implements the target.

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API executeMenuItem (int32 tag /*in*/) = 0;` | `[UI-thread & (Initialized \| Connected) & plugView]` |

## A.4 GUI, plugin side (`gui/`)

### IPlugView — `5BC32507 D06049EA A6151B52 2B755B29` · 3.0.0 · `gui/iplugview.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API isPlatformTypeSupported (FIDString type) = 0;` | `type` is one of the platformUIType IDStrings below |
| `virtual tresult PLUGIN_API attached (void* parent, FIDString type) = 0;` | Parent is owned by the caller; the plugin may only add its own views. The plugin **may call `IPlugFrame::resizeView` from inside this call** |
| `virtual tresult PLUGIN_API removed () = 0;` | Remove all own views from the parent |
| `virtual tresult PLUGIN_API onWheel (float distance) = 0;` | |
| `virtual tresult PLUGIN_API onKeyDown (char16 key, int16 keyCode, int16 modifiers) = 0;` | `key` = unicode, `keyCode` = `VirtualKeyCodes` (keycodes.h), `modifiers` = `KeyModifier`. **Return `kResultTrue` ONLY if really handled, else host key commands are blocked** |
| `virtual tresult PLUGIN_API onKeyUp (char16 key, int16 keyCode, int16 modifiers) = 0;` | |
| `virtual tresult PLUGIN_API getSize (ViewRect* size) = 0;` | |
| `virtual tresult PLUGIN_API onSize (ViewRect* newSize) = 0;` | |
| `virtual tresult PLUGIN_API onFocus (TBool state) = 0;` | |
| `virtual tresult PLUGIN_API setFrame (IPlugFrame* frame) = 0;` | |
| `virtual tresult PLUGIN_API canResize () = 0;` | |
| `virtual tresult PLUGIN_API checkSizeConstraint (ViewRect* rect) = 0;` | Called during live resize; plugin may adjust `rect` to a supported size |

```cpp
struct ViewRect
{
    ViewRect (int32 l = 0, int32 t = 0, int32 r = 0, int32 b = 0);
    int32 left;
    int32 top;
    int32 right;
    int32 bottom;
    int32 getWidth () const;   // right - left
    int32 getHeight () const;  // bottom - top
};
// SMTG_TYPE_SIZE_CHECK (ViewRect, 16, 16, 16, 16)
```

Platform type strings: `kPlatformTypeHWND` `"HWND"`, `kPlatformTypeHIView` `"HIView"`, `kPlatformTypeNSView` `"NSView"`, `kPlatformTypeUIView` `"UIView"`, `kPlatformTypeX11EmbedWindowID` `"X11EmbedWindowID"`, `kPlatformTypeWaylandSurfaceID` `"WaylandSurfaceID"`.

Two host-critical rules from the header: **coordinates are logical units on NSView but physical pixels on HWND/X11**; and `onKeyDown` must only return `kResultTrue` if the key was *really* handled.

Resize handshake: plugin calls `IPlugFrame::resizeView(newSize)` → host **must** call `IPlugView::onSize(newSize)` in the same callstack if the size changed. A `getSize()` before that `onSize()` returns the *old* size.

### IPlugViewContentScaleSupport — `65ED9690 8AC44525 8AADEF7A 72EA703F` · extends IPlugView · 3.6.6

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API setContentScaleFactor (ScaleFactor factor /*in*/) = 0;` | `typedef float ScaleFactor`. Host may call it **any time the view is valid**, including before `setFrame`. On `kResultTrue` the plugin scales and calls `IPlugFrame::resizeView`. `[UI-thread]` |

### Linux::IEventHandler — `561E65C9 13A0496F 813A2C35 654D7983` · 3.6.8

| Signature | Purpose |
|---|---|
| `virtual void PLUGIN_API onFDIsSet (FileDescriptor fd) = 0;` | `using FileDescriptor = int`. `void` return |

### Linux::ITimerHandler — `10BDD94F 41424774 821FAD8F ECA72CA9` · 3.6.8

| Signature | Purpose |
|---|---|
| `virtual void PLUGIN_API onTimer () = 0;` | `void` return |

---

# B. Host-implements → Plugin calls

These are what a host must provide. Grouped by how the plugin reaches them.

## B.1 Reached via `queryInterface` on the `IComponentHandler` the host passed to `setComponentHandler`

### IComponentHandler — `93A0BEA3 0BD045DB 8E890B0C C1E46AC6` · 3.0.0 · **mandatory**

All four are **UI-thread only**, `[UI-thread & Connected]`.

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API beginEdit (ParamID id /*in*/) = 0;` | Before a performEdit (e.g. mouse-down) |
| `virtual tresult PLUGIN_API performEdit (ParamID id /*in*/, ParamValue valueNormalized /*in*/) = 0;` | Between beginEdit and endEdit |
| `virtual tresult PLUGIN_API endEdit (ParamID id /*in*/) = 0;` | After a performEdit (e.g. mouse-up) |
| `virtual tresult PLUGIN_API restartComponent (int32 flags /*in*/) = 0;` | `flags` is a combination of `RestartFlags` — see C.1 |

### IComponentHandler2 — `F040B4B3 A36045EC ABCDC045 B4D5A2CC` · 3.1.0 · optional

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API setDirty (TBool state /*in*/) = 0;` | Something besides parameters changed; host should save before quitting |
| `virtual tresult PLUGIN_API requestOpenEditor (FIDString name = ViewType::kEditor /*in*/) = 0;` | **Has a default argument** — `"editor"`. Use instead of blocking alerts |
| `virtual tresult PLUGIN_API startGroupEdit () = 0;` | Host keeps one timestamp for all begin/perform/endEdit until finishGroupEdit |
| `virtual tresult PLUGIN_API finishGroupEdit () = 0;` | |

### IComponentHandler3 — `69F11617 D26B400D A4B6B964 7B6EBBAB` · 3.5.0 · optional · `ivstcontextmenu.h`

| Signature | Purpose |
|---|---|
| `virtual IContextMenu* PLUGIN_API createContextMenu (IPlugView* plugView /*in*/, const ParamID* paramID /*in*/) = 0;` | **The plugin must release the returned menu.** `paramID` zero/null ⇒ host may create a generic menu. The IPlugView must be valid. `[UI-thread & (Initialized \| Connected) & plugView]` |

### IComponentHandlerBusActivation — `067D02C1 5B4E274D A92D90FD 6EAF7240` · 3.6.8 · optional

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API requestBusActivation (MediaType type /*in*/, BusDirection dir /*in*/, int32 index /*in*/, TBool state /*in*/) = 0;` | A *request*; if accepted the host later calls `IComponent::activateBus` |

### IProgress — `00C9DC5B 9D904254 91A388C8 B4E91B69` · 3.7.0 · optional

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API start (ProgressType type /*in*/, const tchar* optionalDescription /*in*/, ID& outID /*out*/) = 0;` | Host mints the ID |
| `virtual tresult PLUGIN_API update (ID id /*in*/, ParamValue normValue /*in*/) = 0;` | `normValue` in [0, 1] |
| `virtual tresult PLUGIN_API finish (ID id /*in*/) = 0;` | |

```cpp
enum ProgressType : uint32
{
    AsyncStateRestoration = 0,  // plug-in state restored async on a background thread
    UIBackgroundTask            // a plug-in task triggered by a UI action
};
using ID = uint64;
```

**The host may unload the plugin at any point during a progress** — the plugin must survive that.

### IComponentHandlerSystemTime — `F9E53056 D1554CD5 B7695E1B 7B0F7745` · 3.7.9 · optional

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getSystemTime (int64& systemTime /*out*/) = 0;` | Same clock as `ProcessContext::systemTime` |

### IUnitHandler — `4B5147F8 4654486B 8DAB30BA 163A3C56` · 3.0.0 · optional · `ivstunits.h`

Header: "retrieve via queryInterface from IComponentHandler". Both are `[UI-thread & (Initialized | Connected)]`. Note neither carries direction annotations.

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API notifyUnitSelection (UnitID unitId) = 0;` | A module was selected in the plugin GUI |
| `virtual tresult PLUGIN_API notifyProgramListChange (ProgramListID listId, int32 programIndex) = 0;` | `kAllProgramInvalid = -1` ⇒ all program info stale, else just that index |

### IUnitHandler2 — `F89F8CDF 699E4BA5 96AAC9A4 81452B01` · 3.6.5 · optional · `ivstunits.h`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API notifyUnitByBusChange () = 0;` | Host must re-call `IUnitInfo::getUnitByBus` |

## B.2 Reached via the `context` passed to `IPluginBase::initialize` (and `IPluginFactory3::setHostContext`)

### IHostApplication — `58E595CC DB2D4969 8B6AAF8C 36A664E5` · 3.0.0 · **mandatory**

Both `[UI-thread & Initialized]`.

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getName (String128 name) = 0;` | Host application name |
| `virtual tresult PLUGIN_API createInstance (TUID cid, TUID _iid, void** obj) = 0;` | How a plugin allocates an `IMessage`. SDK helper: `inline IMessage* allocateMessage (IHostApplication* host)` |

### IPlugInterfaceSupport — `4FB58B9E 9EAA4E0F AB361C1C CCB56FEA` · 3.6.12 · optional

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API isPlugInterfaceSupported (const TUID _iid) = 0;` | "Does *this host* actually use interface X". How a plugin decides between IMidiMapping2 and IMidiMapping |

### IDataExchangeHandler — `36D551BD 6FF54F08 B48E830D 8BD5A03B` · 3.7.9 · optional

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API openQueue (IAudioProcessor* processor, uint32 blockSize, uint32 numBlocks, uint32 alignment, DataExchangeUserContextID userContextID, DataExchangeQueueID* outID) = 0;` | Main thread only, component **not active** but initialized+connected. `alignment` 0 ⇒ platform default |
| `virtual tresult PLUGIN_API closeQueue (DataExchangeQueueID queueID) = 0;` | Frees all memory; locked blocks are freed and invalidated |
| `virtual tresult PLUGIN_API lockBlock (DataExchangeQueueID queueId, DataExchangeBlock* block) = 0;` | **Only from inside `IAudioProcessor::process`.** Returns `kOutOfMemory` when all blocks are locked |
| `virtual tresult PLUGIN_API freeBlock (DataExchangeQueueID queueId, DataExchangeBlockID blockID, TBool sendToController) = 0;` | **Only from inside `process`.** `sendToController` false ⇒ discard |

```cpp
typedef uint32 DataExchangeQueueID;
typedef uint32 DataExchangeBlockID;
typedef uint32 DataExchangeUserContextID;

static SMTG_CONSTEXPR DataExchangeQueueID InvalidDataExchangeQueueID = kMaxInt32;
static SMTG_CONSTEXPR DataExchangeBlockID InvalidDataExchangeBlockID = kMaxInt32;

struct DataExchangeBlock
{
    void*               data;
    uint32              size;
    DataExchangeBlockID blockID;
};
```

**Queue lifecycle rules a host must honour:** `openQueue` main-thread only while inactive (the plugin's best site is `setupProcessing`); `lockBlock`/`freeBlock` only inside `process`; `closeQueue` after deactivation and before `IConnectionPoint` disconnect. The host guarantees all blocks are delivered before the plugin is deactivated.

### Wrapper markers — no methods, pure `queryInterface` probes

| Interface | IID | Since | Meaning |
|---|---|---|---|
| **IVst3ToVst2Wrapper** | `29633AEC 1D1C47E2 BB85B97B D36EAC61` | 3.1.0 | A VST2 wrapper sits between plugin and real host |
| **IVst3ToAUWrapper** | `A3B8C6C5 C0954688 B0916F0B B697AA44` | 3.1.0 | An AU wrapper sits between |
| **IVst3ToAAXWrapper** | `6D319DC6 60C56242 B32C951B 93BEF4C6` | 3.6.8 | An AAX wrapper sits between |

### IVst3WrapperMPESupport — `44149067 42CF4BF9 8800B750 F7359FE3` · 3.6.12 · optional

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API enableMPEInputProcessing (TBool state) = 0;` | |
| `virtual tresult PLUGIN_API setMPEInputDeviceSettings (int32 masterChannel, int32 memberBeginChannel, int32 memberEndChannel) = 0;` | All zero-based. Defaults: enabled, master 0, begin 1, end 14 |

### IInterAppAudioHost — `0CE5743D 68DF415E AE285BD4 E2CDC8FD` · 3.6.0 · optional · iOS only

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getScreenSize (ViewRect* size, float* scale) = 0;` | |
| `virtual tresult PLUGIN_API connectedToHost () = 0;` | `kResultTrue` if an IAA connection exists |
| `virtual tresult PLUGIN_API switchToHost () = 0;` | |
| `virtual tresult PLUGIN_API sendRemoteControlEvent (uint32 event) = 0;` | `AudioUnitRemoteControlEvent` values |
| `virtual tresult PLUGIN_API getHostIcon (void** icon) = 0;` | Pointer to a `CGImageRef` |
| `virtual tresult PLUGIN_API scheduleEventFromUI (Event& event) = 0;` | |
| `virtual IInterAppAudioPresetManager* PLUGIN_API createPresetManager (const TUID& cid) = 0;` | **Caller must release** |
| `virtual tresult PLUGIN_API showSettingsView () = 0;` | MIDI + tempo settings |

## B.3 Passed inside `ProcessData` each block

### IParameterChanges — `A4779663 0BB64A56 B44384A8 466FEB9D` · 3.0.0 · mandatory

| Signature | Purpose |
|---|---|
| `virtual int32 PLUGIN_API getParameterCount () = 0;` | Count of *changed* parameters only |
| `virtual IParamValueQueue* PLUGIN_API getParameterData (int32 index /*in*/) = 0;` | |
| `virtual IParamValueQueue* PLUGIN_API addParameterData (const ParamID& id /*in*/, int32& index /*out*/) = 0;` | Used by the plugin for `outputParameterChanges` |

### IParamValueQueue — `01263A18 ED074F6F 98C9D356 4686F9BA` · 3.0.0 · mandatory

| Signature | Purpose |
|---|---|
| `virtual ParamID PLUGIN_API getParameterId () = 0;` | |
| `virtual int32 PLUGIN_API getPointCount () = 0;` | |
| `virtual tresult PLUGIN_API getPoint (int32 index /*in*/, int32& sampleOffset /*out*/, ParamValue& value /*out*/) = 0;` | |
| `virtual tresult PLUGIN_API addPoint (int32 sampleOffset /*in*/, ParamValue value /*in*/, int32& index /*out*/) = 0;` | |

**The host is responsible for the automation-curve encoding**, and it is not obvious: a queue is a *linear-approximation segment* of the curve for exactly this block. The point at block position 0 may be **omitted** — a constant-slope run transmits only the last sample's value, and the implied previous point sits at block position **-1** with the last transmitted value. A jump must be sent as **two** points (old value, then new value at the next sample offset). A non-linear curve must be linearized by the host. Empty queue for a parameter = slope 0 continuing. The header's own interpolation snippet:

```cpp
double x1 = -1;                    // position of last point related to current buffer
double y1 = currentParameterValue; // last transmitted value

int32 pointTime = 0;
ParamValue pointValue = 0;
IParamValueQueue::getPoint (0, pointTime, pointValue);

double x2 = pointTime;
double y2 = pointValue;

double slope  = (y2 - y1) / (x2 - x1);
double offset = y1 - (slope * x1);

double curveValue = (slope * bufferTime) + offset; // bufferTime is any position in buffer
```

### IEventList — `3A2C4214 346349FE B2C4F397 B9695A44` · 3.0.0 · mandatory

| Signature | Purpose |
|---|---|
| `virtual int32 PLUGIN_API getEventCount () = 0;` | |
| `virtual tresult PLUGIN_API getEvent (int32 index /*in*/, Event& e /*out*/) = 0;` | |
| `virtual tresult PLUGIN_API addEvent (Event& e /*in*/) = 0;` | Used by the plugin for `outputEvents` |

## B.4 Attributes, messaging, streams

### IAttributeList — `1E5F0AEB CC7F4533 A2544011 38AD5EE4` · 3.0.0 · mandatory

`typedef const char* AttrID;`

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API setInt (AttrID id /*in*/, int64 value /*in*/) = 0;` | |
| `virtual tresult PLUGIN_API getInt (AttrID id /*in*/, int64& value /*out*/) = 0;` | |
| `virtual tresult PLUGIN_API setFloat (AttrID id /*in*/, double value /*in*/) = 0;` | |
| `virtual tresult PLUGIN_API getFloat (AttrID id /*in*/, double& value /*out*/) = 0;` | |
| `virtual tresult PLUGIN_API setString (AttrID id /*in*/, const TChar* string /*in*/) = 0;` | UTF-16, must be null-terminated |
| `virtual tresult PLUGIN_API getString (AttrID id /*in*/, TChar* string /*out*/, uint32 sizeInBytes /*in*/) = 0;` | **`sizeInBytes` is BYTES, not characters — multiply the length by `sizeof (TChar)`** |
| `virtual tresult PLUGIN_API setBinary (AttrID id /*in*/, const void* data /*in*/, uint32 sizeInBytes /*in*/) = 0;` | |
| `virtual tresult PLUGIN_API getBinary (AttrID id /*in*/, const void*& data /*out*/, uint32& sizeInBytes) = 0;` | Note the SDK annotates `data` but **not** `sizeInBytes` |

### IMessage — `936F033B C6C047DB BB0882F8 13C1E613` · 3.0.0 · mandatory

Created only via `IHostApplication::createInstance`.

| Signature | Purpose |
|---|---|
| `virtual FIDString PLUGIN_API getMessageID () = 0;` | e.g. `"TextMessage"` |
| `virtual void PLUGIN_API setMessageID (FIDString id /*in*/) = 0;` | **`void` return** |
| `virtual IAttributeList* PLUGIN_API getAttributes () = 0;` | |

### IStreamAttributes — `D6CE2FFC EFAF4B8C 9E74F1BB 12DA44B4` · 3.6.0 · optional · extends IBStream

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getFileName (String128 name /*inout*/) = 0;` | Filename without extension |
| `virtual IAttributeList* PLUGIN_API getAttributes () = 0;` | Carries `PresetAttributes::kStateType` (project vs preset) and `kFilePathStringType` |

### IBStream — `C3BF6EA2 30994752 9B6BF990 1EE33E9B` · mandatory · `base/ibstream.h`

Note the **default arguments** on the first three.

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API read (void* buffer, int32 numBytes, int32* numBytesRead = nullptr) = 0;` | Pass null for the count if uninterested |
| `virtual tresult PLUGIN_API write (void* buffer, int32 numBytes, int32* numBytesWritten = nullptr) = 0;` | |
| `virtual tresult PLUGIN_API seek (int64 pos, int32 mode, int64* result = nullptr) = 0;` | `mode` ∈ `kIBSeekSet=0`, `kIBSeekCur`, `kIBSeekEnd` |
| `virtual tresult PLUGIN_API tell (int64* pos) = 0;` | **Read and write share one position** |

### ISizeableStream — `04F9549E E02F4E6E 87E86A87 47F4E17F` · optional

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API getStreamSize (int64& size) = 0;` | |
| `virtual tresult PLUGIN_API setStreamSize (int64 size) = 0;` | File streams resize only if write-enabled |

### IContextMenu — `2E93C863 0C9C4588 97DBECF5 AD17817D` · 3.5.0 · optional

`typedef IContextMenuItem Item;` All methods `[UI-thread]`.

| Signature | Purpose |
|---|---|
| `virtual int32 PLUGIN_API getItemCount () = 0;` | |
| `virtual tresult PLUGIN_API getItem (int32 index /*in*/, Item& item /*out*/, IContextMenuTarget** target /*out*/) = 0;` | Target may be unassigned |
| `virtual tresult PLUGIN_API addItem (const Item& item /*in*/, IContextMenuTarget* target /*in*/) = 0;` | |
| `virtual tresult PLUGIN_API removeItem (const Item& item /*in*/, IContextMenuTarget* target /*in*/) = 0;` | |
| `virtual tresult PLUGIN_API popup (UCoord x /*in*/, UCoord y /*in*/) = 0;` | Coords relative to the plug view's top-left |

```cpp
struct IContextMenuItem
{
    String128 name;
    int32     tag;
    int32     flags;

    enum Flags {
        kIsSeparator  = 1 << 0,
        kIsDisabled   = 1 << 1,
        kIsChecked    = 1 << 2,
        kIsGroupStart = 1 << 3 | kIsDisabled,   // COMPOSITE, not a single bit
        kIsGroupEnd   = 1 << 4 | kIsSeparator,  // COMPOSITE, not a single bit
    };
};
```

## B.5 GUI, host side

### IPlugFrame — `367FAF01 AFA94693 8D4DA2A0 ED0882A3` · 3.0.0 · mandatory

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API resizeView (IPlugView* view, ViewRect* newSize) = 0;` | Host must then call `IPlugView::onSize()` |

### Linux::IRunLoop — `18C35366 97764F1A 9C5B8385 7A871389` · 3.6.8 · Linux-mandatory

Reached via `IPlugFrame` **or** the `setHostContext` context (so a plugin without an editor can still get a run loop).

| Signature | Purpose |
|---|---|
| `virtual tresult PLUGIN_API registerEventHandler (IEventHandler* handler, FileDescriptor fd) = 0;` | Host calls the handler when the fd is readable |
| `virtual tresult PLUGIN_API unregisterEventHandler (IEventHandler* handler) = 0;` | |
| `virtual tresult PLUGIN_API registerTimer (ITimerHandler* handler, TimerInterval milliseconds) = 0;` | `using TimerInterval = uint64`. Repeats until unregistered |
| `virtual tresult PLUGIN_API unregisterTimer (ITimerHandler* handler) = 0;` | |

### IWaylandHost — `5E9582EE 86594652 B213678E 7F1A705E` · `gui/iwaylandframe.h`

| Signature | Purpose |
|---|---|
| `virtual wl_display* PLUGIN_API openWaylandConnection () = 0;` | **The plugin must not connect to the system compositor itself** |
| `virtual tresult PLUGIN_API closeWaylandConnection (wl_display* display) = 0;` | |

### IWaylandFrame — `809FAEC6 231C4FFA 98ED046C 6E9E2003` · `gui/iwaylandframe.h`

| Signature | Purpose |
|---|---|
| `virtual wl_surface* PLUGIN_API getWaylandSurface (wl_display* display) = 0;` | |
| `virtual xdg_surface* PLUGIN_API getParentSurface (ViewRect& parentSize, wl_display* display) = 0;` | |
| `virtual xdg_toplevel* PLUGIN_API getParentToplevel (wl_display* display) = 0;` | |

## B.6 Base-layer host services (`base/`, not VST-specific)

### IErrorContext — `12BCD07B 7C694336 B7DA77C3 444A0CD0` · `base/ierrorcontext.h`

| Signature |
|---|
| `virtual void PLUGIN_API disableErrorUI (bool state) = 0;` |
| `virtual tresult PLUGIN_API errorMessageShown () = 0;` |
| `virtual tresult PLUGIN_API getErrorMessage (IString* message) = 0;` |

### IUpdateHandler — `F5246D56 86544D60 B026AFB5 7B697B37` · `base/iupdatehandler.h`

| Signature |
|---|
| `virtual tresult PLUGIN_API addDependent (FUnknown* object, IDependent* dependent) = 0;` |
| `virtual tresult PLUGIN_API removeDependent (FUnknown* object, IDependent* dependent) = 0;` |
| `virtual tresult PLUGIN_API triggerUpdates (FUnknown* object, int32 message) = 0;` |
| `virtual tresult PLUGIN_API deferUpdates (FUnknown* object, int32 message) = 0;` |

### IDependent — `F52B7AAE DE72416D 8AF18ACE 9DD7BD5E` · `base/iupdatehandler.h`

| Signature |
|---|
| `virtual void PLUGIN_API update (FUnknown* changedUnknown, int32 message) = 0;` |

### IStringResult — `550798BC 872049DB 84920A15 3B50B7A8` · `base/istringresult.h`

| Signature |
|---|
| `virtual void PLUGIN_API setText (const char8* text) = 0;` |

### IString — `F99DB7A3 0FC14821 800B0CF9 8E348EDF` · `base/istringresult.h`

| Signature | Note |
|---|---|
| `virtual void PLUGIN_API setText8 (const char8* text) = 0;` | **"!Do not use this method!"** — early implementations took ownership of the pointer |
| `virtual void PLUGIN_API setText16 (const char16* text) = 0;` | **Same warning** |
| `virtual const char8* PLUGIN_API getText8 () = 0;` | |
| `virtual const char16* PLUGIN_API getText16 () = 0;` | |
| `virtual void PLUGIN_API take (void* s, bool isWide) = 0;` | |
| `virtual bool PLUGIN_API isWideString () const = 0;` | Note the trailing `const` |

### IPersistent — `BA1A4637 3C9F46D0 A65DBA0E B85DA829` · `base/ipersistent.h`

| Signature |
|---|
| `virtual tresult PLUGIN_API getClassID (char8* uid) = 0;` |
| `virtual tresult PLUGIN_API saveAttributes (IAttributes* ) = 0;` |
| `virtual tresult PLUGIN_API loadAttributes (IAttributes* ) = 0;` |

### IAttributes — `FA1E32F9 CA6D46F5 A982F956 B1191B58` · `base/ipersistent.h` (distinct from `Vst::IAttributeList`)

| Signature |
|---|
| `virtual tresult PLUGIN_API set (IAttrID attrID /*in*/, const FVariant& data /*in*/) = 0;` |
| `virtual tresult PLUGIN_API queue (IAttrID listID /*in*/, const FVariant& data /*in*/) = 0;` |
| `virtual tresult PLUGIN_API setBinaryData (IAttrID attrID /*in*/, void* data /*in*/, uint32 bytes /*in*/, bool copyBytes /*in*/) = 0;` |
| `virtual tresult PLUGIN_API get (IAttrID attrID /*in*/, FVariant& data /*out*/) = 0;` |
| `virtual tresult PLUGIN_API unqueue (IAttrID listID /*in*/, FVariant& data /*out*/) = 0;` |
| `virtual int32 PLUGIN_API getQueueItemCount (IAttrID attrId /*in*/) = 0;` |
| `virtual tresult PLUGIN_API resetQueue (IAttrID attrID /*in*/) = 0;` |
| `virtual tresult PLUGIN_API resetAllQueues () = 0;` |
| `virtual tresult PLUGIN_API getBinaryData (IAttrID attrID /*in*/, void* data /*inout*/, uint32 bytes /*in*/) = 0;` |
| `virtual uint32 PLUGIN_API getBinaryDataSize (IAttrID attrID /*in*/) = 0;` |

### IAttributes2 — `1382126A FECA4871 97D52A45 B042AE99` · `base/ipersistent.h` (extends IAttributes)

| Signature |
|---|
| `virtual int32 PLUGIN_API countAttributes () const = 0;` |
| `virtual IAttrID PLUGIN_API getAttributeID (int32 index /*in*/) const = 0;` |

### ICloneable — `D45406B9 3A2D4443 9DAD9BA9 85A1454B` · `base/icloneable.h`

| Signature |
|---|
| `virtual FUnknown* PLUGIN_API clone () = 0;` |

## B.7 Validator-only (not part of a real host)

### ITestPlugProvider — `86BE70EE 4E99430F 978F1E6E D68FB5BA` · `vst/ivsttestplugprovider.h`

| Signature |
|---|
| `virtual IComponent* PLUGIN_API getComponent () = 0;` |
| `virtual IEditController* PLUGIN_API getController () = 0;` |
| `virtual tresult PLUGIN_API releasePlugIn (IComponent* component /*in*/, IEditController* controller /*in*/) = 0;` |
| `virtual tresult PLUGIN_API getSubCategories (IStringResult& result /*out*/) const = 0;` |
| `virtual tresult PLUGIN_API getComponentUID (FUID& uid /*out*/) const = 0;` |

### ITestPlugProvider2 — `C7C75364 7B8343AC A4495B0A 3E5A46C7` (extends ITestPlugProvider)

| Signature |
|---|
| `virtual IPluginFactory* PLUGIN_API getPluginFactory () = 0;` |

`test/itest.h` also declares `ITest`, `ITestResult`, `ITestSuite`, `ITestFactory` — each twice under `#if`-guarded platform variants, hence duplicate IIDs when grepping.

---

# C. Enums & Constants

## C.1 RestartFlags (`ivsteditcontroller.h`) — complete, all 12

```cpp
enum RestartFlags : int32
```

| Flag | Value | Since | What the host must do |
|---|---|---|---|
| `kReloadComponent` | `1<<0` | 3.0.0 | Unload processor+controller completely and reload |
| `kIoChanged` | `1<<1` | 3.0.0 | Deactivate, re-ask bus configs, adapt the graph, reactivate |
| `kParamValuesChanged` | `1<<2` | 3.0.0 | Invalidate all cached param values, re-read from controller |
| `kLatencyChanged` | `1<<3` | 3.0.0 | Deactivate + reactivate, **then** call `getLatencySamples` |
| `kParamTitlesChanged` | `1<<4` | 3.0.0 | Invalidate all cached `ParameterInfo` (title, shortTitle, units, default, stepCount, flags) |
| `kMidiCCAssignmentChanged` | `1<<5` | 3.0.1 | Rebuild MIDI-CC→param map; re-read program-change params (stepCount + unitId) |
| `kNoteExpressionChanged` | `1<<6` | 3.5.0 | Invalidate note-expression info, count **and** PhysicalUIMapping |
| `kIoTitlesChanged` | `1<<7` | 3.5.0 | Re-read bus titles |
| `kPrefetchableSupportChanged` | `1<<8` | 3.6.1 | Deactivate, call `getPrefetchableSupport`, reactivate |
| `kRoutingInfoChanged` | `1<<9` | 3.6.6 | Re-call `IComponent::getRoutingInfo` |
| `kKeyswitchChanged` | `1<<10` | 3.7.3 | Invalidate keyswitch info/count |
| `kParamIDMappingChanged` | `1<<11` | 3.7.11 | Param IDs changed — remap automation via `IRemapParamID`. Emitted from `setComponentState`/`setState` during project load |

## C.2 Bus / IO enums (`ivstcomponent.h`)

```cpp
enum MediaTypes    { kAudio = 0, kEvent, kNumMediaTypes };
enum BusDirections { kInput = 0, kOutput };
enum BusTypes      { kMain  = 0, kAux };            // kAux = sidechain
enum IoModes       { kSimple = 0, kAdvanced, kOfflineProcessing };

struct BusInfo
{
    MediaType    mediaType;     // a value of MediaTypes
    BusDirection direction;     // a value of BusDirections
    int32        channelCount;  // for kEvent busses: number of supported MIDI channels
    String128    name;
    BusType      busType;       // a value of BusTypes
    uint32       flags;         // a combination of BusFlags

    enum BusFlags
    {
        kDefaultActive    = 1 << 0,
        kIsControlVoltage = 1 << 1   // [released: 3.7.0]
    };
};

struct RoutingInfo
{
    MediaType mediaType;
    int32     busIndex;
    int32     channel;    // -1 for all channels
};
```

- **`kMain` busses must be placed before any `kAux`.**
- `kSimple` / `kAdvanced` IoModes are instruments-only.
- `kIsControlVoltage` (3.7.0): audio-rate control data in the same [-1..1] format as audio; **a host must prevent it reaching speakers**. Audio media type only.
- `channelCount` must be re-read after `setBusArrangements`.
- **All busses start inactive.** `kDefaultActive` is only a wish — the header says the host is allowed to ignore it and activate only the first bus.

## C.3 Processing enums (`ivstaudioprocessor.h`)

```cpp
enum SymbolicSampleSizes { kSample32, kSample64 };
enum ProcessModes        { kRealtime, kPrefetch, kOffline };

static const uint32 kNoTail       = 0;
static const uint32 kInfiniteTail = kMaxInt32u;

struct ProcessSetup
{
    int32      processMode;         // ProcessModes
    int32      symbolicSampleSize;  // SymbolicSampleSizes
    int32      maxSamplesPerBlock;
    SampleRate sampleRate;
};

struct AudioBusBuffers
{
    int32  numChannels;    // must match the current bus arrangement; may be 0 to flush params
    uint64 silenceFlags;   // bitset of silence state per channel
    union
    {
        Sample32** channelBuffers32;
        Sample64** channelBuffers64;
    };
};

struct ProcessData
{
    int32 processMode;          // ProcessModes
    int32 symbolicSampleSize;   // SymbolicSampleSizes
    int32 numSamples;
    int32 numInputs;            // number of audio input busses
    int32 numOutputs;           // number of audio output busses
    AudioBusBuffers* inputs;
    AudioBusBuffers* outputs;

    IParameterChanges* inputParameterChanges;
    IParameterChanges* outputParameterChanges;  // optional
    IEventList*        inputEvents;             // optional
    IEventList*        outputEvents;            // optional
    ProcessContext*    processContext;          // optional, but most welcome
};

class IProcessContextRequirements
{
    enum Flags
    {
        kNeedSystemTime           = 1 <<  0,  // kSystemTimeValid
        kNeedContinousTimeSamples = 1 <<  1,  // kContTimeValid
        kNeedProjectTimeMusic     = 1 <<  2,  // kProjectTimeMusicValid
        kNeedBarPositionMusic     = 1 <<  3,  // kBarPositionValid
        kNeedCycleMusic           = 1 <<  4,  // kCycleValid
        kNeedSamplesToNextClock   = 1 <<  5,  // kClockValid
        kNeedTempo                = 1 <<  6,  // kTempoValid
        kNeedTimeSignature        = 1 <<  7,  // kTimeSigValid
        kNeedChord                = 1 <<  8,  // kChordValid
        kNeedFrameRate            = 1 <<  9,  // kSmpteValid
        kNeedTransportState       = 1 << 10,  // kPlaying, kCycleActive, kRecording
    };
};
```

**Realtime↔Prefetch switches happen on the RT thread with no `setupProcessing` call** — the plugin reads `ProcessData::processMode`. Switching to/from `kOffline` requires the host to call `setupProcessing`.

Host buffer contract: the channel-buffer array must always be supplied and sized to match the arrangement, **even for inactive busses** (the pointers themselves may then be null). `numChannels` may be 0 when flushing params with no processing. Even when a silence flag is set, **the channel buffers must still point to valid memory**. Bus buffer indices always match `getBusInfo` indices for `kAudio`.

## C.4 ProcessContext (`ivstprocesscontext.h`)

```cpp
struct FrameRate
{
    enum FrameRateFlags
    {
        kPullDownRate = 1 << 0,
        kDropRate     = 1 << 1
    };
    uint32 framesPerSecond;
    uint32 flags;
};

struct Chord
{
    uint8 keyNote;    // key note in chord
    uint8 rootNote;   // lowest note in chord
    int16 chordMask;  // 1st bit = minor second, 2nd = major second, ... NO bit for the keynote

    enum Masks {
        kChordMask    = 0x0FFF,
        kReservedMask = 0xF000
    };
};

struct ProcessContext
{
    enum StatesAndFlags
    {
        kPlaying               = 1 << 1,
        kCycleActive           = 1 << 2,
        kRecording             = 1 << 3,

        kSystemTimeValid       = 1 << 8,
        kContTimeValid         = 1 << 17,

        kProjectTimeMusicValid = 1 << 9,
        kBarPositionValid      = 1 << 11,
        kCycleValid            = 1 << 12,

        kTempoValid            = 1 << 10,
        kTimeSigValid          = 1 << 13,
        kChordValid            = 1 << 18,

        kSmpteValid            = 1 << 14,
        kClockValid            = 1 << 15
    };

    uint32        state;                  // combination of StatesAndFlags

    double        sampleRate;             // always valid
    TSamples      projectTimeSamples;     // always valid

    int64         systemTime;             // nanoseconds            (optional)
    TSamples      continousTimeSamples;   // project time, no loop  (optional)

    TQuarterNotes projectTimeMusic;       // quarter notes          (optional)
    TQuarterNotes barPositionMusic;       // last bar start         (optional)
    TQuarterNotes cycleStartMusic;        //                        (optional)
    TQuarterNotes cycleEndMusic;          //                        (optional)

    double        tempo;                  // BPM                    (optional)
    int32         timeSigNumerator;       // e.g. 3 for 3/4         (optional)
    int32         timeSigDenominator;     // e.g. 4 for 3/4         (optional)

    Chord         chord;                  //                        (optional)

    int32         smpteOffsetSubframes;   // 1/80 of frame          (optional)
    FrameRate     frameRate;              //                        (optional)

    int32         samplesToNextClock;     // 24 PPQ, CAN BE NEGATIVE (optional)
};
```

Bit 16 is unused; bit ordering is not monotonic with field order. 29.97-drop fps is `framesPerSecond = 30` with `flags = kDropRate|kPullDownRate`.

## C.5 Event types (`ivstevents.h`)

```cpp
enum NoteIDUserRange
{
    kNoteIDUserRangeLowerBound = -10000,   // reserved for plug-ins, never used by the host
    kNoteIDUserRangeUpperBound = -1000,
};

struct NoteOnEvent
{
    int16 channel;   // channel index in event bus
    int16 pitch;     // [0, 127] = [C-2, G8], A3 = 440Hz (12-TET)
    float tuning;    // 1.f = +1 cent, -1.f = -1 cent
    float velocity;  // [0.0, 1.0]
    int32 length;    // in sample frames (optional; a Note Off must follow regardless)
    int32 noteId;    // -1 if not available
};

struct NoteOffEvent
{
    int16 channel;
    int16 pitch;
    float velocity;  // NOTE: velocity precedes noteId here, unlike NoteOnEvent
    int32 noteId;    // associated noteOn identifier, -1 if not available
    float tuning;    // NOTE: tuning is LAST here, and there is no length field
};

struct DataEvent
{
    uint32       size;   // size in bytes of the data block
    uint32       type;   // see DataTypes
    const uint8* bytes;

    enum DataTypes { kMidiSysEx = 0 };
};

struct PolyPressureEvent
{
    int16 channel;
    int16 pitch;
    float pressure;  // [0.0, 1.0]
    int32 noteId;    // applied to the noteId if not -1
};

struct ChordEvent
{
    int16        root;      // [0, 127]
    int16        bassNote;  // [0, 127]
    int16        mask;      // root is bit 0
    uint16       textLen;   // chars before the terminating null, EXCLUDING the null
    const TChar* text;      // UTF-16, null terminated, host's chord name
};

struct ScaleEvent
{
    int16        root;     // [0, 127] = root note / transpose factor
    int16        mask;     // bit 0 = C, bit 1 = C#, ... (0x5AB5 = major scale)
    uint16       textLen;
    const TChar* text;     // UTF-16, null terminated, host's scale name
};

struct LegacyMIDICCOutEvent      // [released: 3.6.12]
{
    uint8 controlNumber;  // see ControllerNumbers [0, 255]
    int8  channel;        // [0, 15]
    int8  value;          // [0, 127]
    int8  value2;         // [0, 127] for pitch bend (kPitchBend) and polyPressure
};

struct Event
{
    int32         busIndex;
    int32         sampleOffset;   // sample frames from current block start
    TQuarterNotes ppqPosition;    // position in project
    uint16        flags;          // combination of EventFlags

    enum EventFlags
    {
        kIsLive        = 1 << 0,   // played live, directly from keyboard
        kUserReserved1 = 1 << 14,
        kUserReserved2 = 1 << 15
    };

    enum EventTypes
    {
        kNoteOnEvent                 = 0,
        kNoteOffEvent                = 1,
        kDataEvent                   = 2,
        kPolyPressureEvent           = 3,
        kNoteExpressionValueEvent    = 4,
        kNoteExpressionTextEvent     = 5,
        kChordEvent                  = 6,
        kScaleEvent                  = 7,
        kNoteExpressionIntValueEvent = 8,      // [released: 3.8.0]
        kLegacyMIDICCOutEvent        = 65535
    };

    uint16 type;   // a value from EventTypes
    union
    {
        NoteOnEvent                 noteOn;
        NoteOffEvent                noteOff;
        DataEvent                   data;
        PolyPressureEvent           polyPressure;
        NoteExpressionValueEvent    noteExpressionValue;
        NoteExpressionTextEvent     noteExpressionText;
        NoteExpressionIntValueEvent noteExpressionIntValue;
        ChordEvent                  chord;
        ScaleEvent                  scale;
        LegacyMIDICCOutEvent        midiCCOut;
    };
};
```

The field-order trap is worth restating: `NoteOnEvent` is `{channel, pitch, tuning, velocity, length, noteId}` but `NoteOffEvent` is `{channel, pitch, velocity, noteId, tuning}` — **`tuning` and `velocity` sit in different positions**, and note-off has no `length`. Every `textLen` excludes the terminating null.

## C.6 NoteExpressionTypeIDs (`ivstnoteexpression.h`)

```cpp
typedef uint32 NoteExpressionTypeID;
typedef double NoteExpressionValue;

enum NoteExpressionTypeIDs : uint32
{
    kVolumeTypeID = 0,
    kPanTypeID,
    kTuningTypeID,
    kVibratoTypeID,
    kExpressionTypeID,
    kBrightnessTypeID,
    kTextTypeID,
    kPhonemeTypeID,

    kCustomStart = 100000,
    kCustomEnd   = 200000,

    kInvalidTypeID = 0xFFFFFFFF
};
```

| ID | Value | Plain-value mapping |
|---|---|---|
| `kVolumeTypeID` | 0 | `[0 = -inf, 0.25 = 0dB, 0.5 = +6dB, 1 = +12dB]`; `plain = 20 * log (4 * norm)` |
| `kPanTypeID` | 1 | `[0 = left, 0.5 = center, 1 = right]` |
| `kTuningTypeID` | 2 | `[0 = -120.0 (ten octaves down), 0.5 = none, 1 = +120.0]`; `plain = 240 * (norm - 0.5)`, `norm = plain / 240 + 0.5`. One octave = `12.0/240.0`, one half-tune = `1.0/240.0` |
| `kVibratoTypeID` | 3 | |
| `kExpressionTypeID` | 4 | |
| `kBrightnessTypeID` | 5 | |
| `kTextTypeID` | 6 | Uses `NoteExpressionTextEvent` |
| `kPhonemeTypeID` | 7 | Header says "TODO:" |
| `kCustomStart` / `kCustomEnd` | 100000 / 200000 | Custom range bounds |
| `kInvalidTypeID` | 0xFFFFFFFF | |

```cpp
struct NoteExpressionValueDescription
{
    NoteExpressionValue defaultValue;  // normalized [0,1]
    NoteExpressionValue minimum;
    NoteExpressionValue maximum;
    int32               stepCount;     // 0: continuous, 1: toggle, else discrete
};

struct NoteExpressionValueEvent
{
    NoteExpressionTypeID typeId;
    int32                noteId;
    NoteExpressionValue  value;   // normalized [0.0, 1.0]
};

struct NoteExpressionIntValueEvent      // [released: 3.8.0]
{
    NoteExpressionTypeID typeId;
    int32                noteId;
    uint64               value;
};

struct NoteExpressionTextEvent
{
    NoteExpressionTypeID typeId;   // kTextTypeID or kPhoneticTypeID
    int32                noteId;
    uint32               textLen;  // excludes the terminating null
    const TChar*         text;     // UTF-16, null terminated
};

struct NoteExpressionTypeInfo
{
    NoteExpressionTypeID          typeId;
    String128                     title;                  // e.g. "Volume"
    String128                     shortTitle;             // e.g. "Vol"
    String128                     units;                  // e.g. "dB"
    int32                         unitId;                 // -1 means no unit used
    NoteExpressionValueDescription valueDesc;
    ParamID                       associatedParameterId;  // only if kAssociatedParameterIDValid
    int32                         flags;

    enum NoteExpressionTypeFlags
    {
        kIsBipolar                  = 1 << 0,  // centered, otherwise unipolar
        kIsOneShot                  = 1 << 1,  // occurs once, at the noteOn
        kIsAbsolute                 = 1 << 2,  // absolute change, not relative offset
        kAssociatedParameterIDValid = 1 << 3,
    };
};
```

Expression event values are **always absolute normalized [0.0, 1.0]**, and expression events for a noteId may only occur **after** its note-on. **The host must keep the `IEventList` properly sorted.** There is exactly **one NoteExpressionTypeID per channel of an event bus**.

```cpp
typedef uint32 KeyswitchTypeID;

enum KeyswitchTypeIDs : uint32
{
    kNoteOnKeyswitchTypeID = 0,   // press before noteOn is played
    kOnTheFlyKeyswitchTypeID,     // press while noteOn is played
    kOnReleaseKeyswitchTypeID,    // press before entering release
    kKeyRangeTypeID               // key maintained pressed for playing
};

struct KeyswitchInfo
{
    KeyswitchTypeID typeId;
    String128       title;         // e.g. "Accentuation"
    String128       shortTitle;    // e.g. "Acc"
    int32           keyswitchMin;  // [0, 127]
    int32           keyswitchMax;  // [0, 127]
    int32           keyRemapped;   // optional remapped key, default -1
    int32           unitId;        // -1 means no unit used
    int32           flags;         // not yet used (set to 0)
};
```

`PhysicalUITypeIDs` (`ivstphysicalui.h`): `kPUIXMovement=0` (`[0=left, 0.5=middle, 1=right]`), `kPUIYMovement` (`[0=bottom/near, 0.5=center, 1=top/far]`), `kPUIPressure` (`[0=none, 1=full]`), `kPUITypeCount`, `kInvalidPUITypeID=0xFFFFFFFF`.

## C.7 ParameterInfo & flags (`ivsteditcontroller.h`)

```cpp
struct ParameterInfo
{
    ParamID   id;                       // unique identifier (named "tag" too)
    String128 title;                    // e.g. "Volume"
    String128 shortTitle;               // e.g. "Vol"
    String128 units;                    // e.g. "dB"
    int32     stepCount;                // 0: continuous, 1: toggle, else discrete (= max - min)
    ParamValue defaultNormalizedValue;  // [0,1]; discrete: defDiscreteValue / stepCount
    UnitID    unitId;
    int32     flags;                    // ParameterFlags

    enum ParameterFlags : int32
    {
        kNoFlags         = 0,        // [SDK 3.0.0]
        kCanAutomate     = 1 << 0,   // [SDK 3.0.0]
        kIsReadOnly      = 1 << 1,   // implies kCanAutomate is NOT set [SDK 3.0.0]
        kIsWrapAround    = 1 << 2,   // out-of-range sets wrap around [SDK 3.0.2]
        kIsList          = 1 << 3,   // display as list [SDK 3.1.0]
        kIsHidden        = 1 << 4,   // implies readOnly set, canAutomate clear [SDK 3.7.0]
        kIsProgramChange = 1 << 15,  // [SDK 3.0.0]
        kIsBypass        = 1 << 16   // only ONE allowed per plug-in [SDK 3.0.0]
    };
};

namespace ViewType { const CString kEditor = "editor"; }

using KnobMode = int32;
enum KnobModes : KnobMode
{
    kCircularMode = 0,      // circular with jump to clicked position
    kRelativCircularMode,   // circular without jump  (SDK's spelling: "Relativ")
    kLinearMode             // depends on vertical movement
};
```

All undefined `ParameterFlags` bits are reserved for future use.

## C.8 Core typedefs (`vsttypes.h`)

```cpp
typedef char16       TChar;          // UTF-16 character
typedef TChar        String128[128]; // 128 character UTF-16 string
typedef const char8* CString;

typedef int32 MediaType;
typedef int32 BusDirection;
typedef int32 BusType;
typedef int32 IoMode;
typedef int32 UnitID;

typedef double ParamValue;   // normalized => [0.0, 1.0]
typedef uint32 ParamID;      // valid range [0, 0x7FFFFFFF];
                             // [0x80000000, 0xFFFFFFFF] is RESERVED FOR THE HOST

typedef int32 ProgramListID;
typedef int16 CtrlNumber;    // see ControllerNumbers

typedef double TQuarterNotes;
typedef int64  TSamples;
typedef uint32 ColorSpec;    // ARGB

static const ParamID kNoParamId  = 0xFFFFFFFF;
static const ParamID kMinParamId = 0;
static const ParamID kMaxParamId = 0x7FFFFFFF;

typedef float  Sample32;
typedef double Sample64;
typedef double SampleRate;

typedef uint64 SpeakerArrangement;  // bitset of speakers
typedef uint64 Speaker;             // bit for one speaker
```

Version macros: `kVstVersionMajor 3`, `kVstVersionMinor 8`, `kVstVersionSub 0`, `VST_VERSION = (major<<16)|(minor<<8)|sub`, plus a `VST_3_x_y_VERSION` constant per release back to `VST_3_0_0_VERSION 0x030000` and matching `SDKVersion_3_x_y` constexprs.

## C.9 Speaker arrangements (`vstspeaker.h`)

**Scheme:** a `SpeakerArrangement` is a `uint64` **bitset** of `Speaker` bits — the channel count is the popcount, and the channel *order* in the buffer is bit-index order (low bit first). There is no separate count field.

Bits 0–19 (classic):

```cpp
const Speaker kSpeakerL    = 1 << 0;   // Left
const Speaker kSpeakerR    = 1 << 1;   // Right
const Speaker kSpeakerC    = 1 << 2;   // Center
const Speaker kSpeakerLfe  = 1 << 3;   // Subbass
const Speaker kSpeakerLs   = 1 << 4;   // Left Surround
const Speaker kSpeakerRs   = 1 << 5;   // Right Surround
const Speaker kSpeakerLc   = 1 << 6;   // Left of Center
const Speaker kSpeakerRc   = 1 << 7;   // Right of Center
const Speaker kSpeakerS    = 1 << 8;   // Surround
const Speaker kSpeakerCs   = kSpeakerS;// Center of Surround - ALIAS, SAME BIT
const Speaker kSpeakerSl   = 1 << 9;   // Side Left
const Speaker kSpeakerSr   = 1 << 10;  // Side Right
const Speaker kSpeakerTc   = 1 << 11;  // Top Center
const Speaker kSpeakerTfl  = 1 << 12;  // Top Front Left
const Speaker kSpeakerTfc  = 1 << 13;  // Top Front Center
const Speaker kSpeakerTfr  = 1 << 14;  // Top Front Right
const Speaker kSpeakerTrl  = 1 << 15;  // Top Rear Left
const Speaker kSpeakerTrc  = 1 << 16;  // Top Rear Center
const Speaker kSpeakerTrr  = 1 << 17;  // Top Rear Right
const Speaker kSpeakerLfe2 = 1 << 18;  // Subbass 2
const Speaker kSpeakerM    = 1 << 19;  // Mono - ITS OWN BIT, not L|R
```

Bits 20–23 + 38–58: Ambisonic ACN 0–24 (**non-contiguous — ACN0–3 at bits 20–23, ACN4–24 at bits 38–58**). Bits 24–37: `kSpeakerTsl` (24), `kSpeakerTsr` (25), `kSpeakerLcs` (26), `kSpeakerRcs` (27), `kSpeakerBfl` (28), `kSpeakerBfc` (29), `kSpeakerBfr` (30), `kSpeakerPl` (31), `kSpeakerPr` (32), `kSpeakerBsl` (33), `kSpeakerBsr` (34), `kSpeakerBrl` (35), `kSpeakerBrc` (36), `kSpeakerBrr` (37). Bits 59–60: `kSpeakerLw`, `kSpeakerRw` (wide).

Common arrangements in `namespace SpeakerArr`: `kEmpty=0`, `kMono` (=`kSpeakerM`), `kStereo` (=`L|R`, 0x03), `kStereoSurround`, `kStereoCenter`, `kStereoSide`, `kStereoCLfe`, `kStereoWide`, `kStereoTF/TS/TR/BF`, `kCineFront`, `k30Cine` (LRC), `k30Music` (LRS), `k31Cine`, `k31Music`, `k40Cine` (LRCS), `k40Music` (Quadro), `k41Cine`, `k41Music`, `k50`, `k51`, `k60Cine`, `k60Music`, `k61Cine`, `k61Music`, `k70Cine` (SDDS), `k70Music`, `k71Cine`, `k71Music`, `k71CineFullFront`, `k71CineFullRear`, `k71CineSideFill`, `k71Proximity`, `k80Cine`, `k80Music`, `k81Cine`, `k81Music`, `k90Cine`, `k91Cine`, `k100Cine`, `k101Cine`.

Immersive layouts carry **dual names** — a legacy `kNM` name and a modern `kX_Y[_Z]` name — that are the *same constant*: `k80Cube == k40_4`, `k71CineFrontHigh == k71MPEG3D == k51_2`, `k70CineFrontHigh == k70MPEG3D == k50_2`, `k70CineSideHigh == k50_2_TS`, `k71CineSideHigh == k51_2_TS`, `k90 == k50_4`, `k91 == k51_4`, `k71_2 == k91Atmos`, `k71_4 == k111MPEG3D`, `k81MPEG3D == k41_4_1`, `k100 == k50_5`, `k101 == k51_5`. Plus `k70_2/k71_2`, `k70_2_TF/k71_2_TF`, `k70_3`, `k72_3`, `k70_4/k71_4`, `k70_6/k71_6`, `k90_4/k91_4`, `k90_6/k91_6`, Dolby wide variants `k90_4_W/k91_4_W/k90_6_W/k91_6_W`, `k50_4_1`/`k51_4_1`, `k71CineTopCenter`, `k71CineCenterHigh`.

Ambisonic: `kAmbi1stOrderACN` (4ch) … `kAmbi7thOrderACN = 0xFFFFFFFFFFFFFFFF` (all 64 bits). Note `kAmbi5thOrderACN = 0x000FFFFFFFFF` and `kAmbi6thOrderACN = 0x0001FFFFFFFFFFFF` are given as raw literals, not OR-chains.

The header also carries a parallel set of display-name `CString`s (`kStringEmpty`, `kStringMono`, `kStringStereo`, `kStringStereoWide` = `"Stereo (Lw Rw)"`, `kString51`, `kString71Music` = `"7.1"`, `kString51_2`, plus `…Old` variants like `kString70MusicOld = "7.0 Music (Dolby)"`) for host UI, with helper functions to convert between arrangement, channel count, speaker index and string.

## C.10 MIDI controller numbers (`ivstmidicontrollers.h`)

`enum ControllerNumbers` covers standard CC 0–127 by name — `kCtrlBankSelectMSB=0`, `kCtrlModWheel=1`, `kCtrlBreath=2`, `kCtrlFoot=4`, `kCtrlPortaTime=5`, `kCtrlDataEntryMSB=6`, `kCtrlVolume=7`, `kCtrlBalance=8`, `kCtrlPan=10`, `kCtrlExpression=11`, `kCtrlEffect1=12`, `kCtrlEffect2=13`, `kCtrlGPC1..4=16..19`, `kCtrlBankSelectLSB=32`, `kCtrlDataEntryLSB=38`, `kCtrlSustainOnOff=64`, `kCtrlPortaOnOff=65`, `kCtrlSustenutoOnOff=66`, `kCtrlSoftPedalOnOff=67`, `kCtrlLegatoFootSwOnOff=68`, `kCtrlHold2OnOff=69`, `kCtrlSoundVariation=70`, `kCtrlFilterCutoff=71`, `kCtrlReleaseTime=72`, `kCtrlAttackTime=73`, `kCtrlFilterResonance=74`, `kCtrlDecayTime=75`, `kCtrlVibratoRate=76`, `kCtrlVibratoDepth=77`, `kCtrlVibratoDelay=78`, `kCtrlSoundCtrler10=79`, `kCtrlGPC5..8=80..83`, `kCtrlPortaControl=84`, `kCtrlEff1Depth..kCtrlEff5Depth=91..95`, `kCtrlDataIncrement=96`, `kCtrlDataDecrement=97`, `kCtrlNRPNSelectLSB=98`, `kCtrlNRPNSelectMSB=99`, `kCtrlRPNSelectLSB=100`, `kCtrlRPNSelectMSB=101`, `kCtrlAllSoundsOff=120`, `kCtrlResetAllCtrlers=121`, `kCtrlLocalCtrlOnOff=122`, `kCtrlAllNotesOff=123`, `kCtrlOmniModeOff=124`, `kCtrlOmniModeOn=125`, `kCtrlPolyModeOnOff=126`, `kCtrlPolyModeOn=127` — then **extends past 127**:

```cpp
    //---Extra--------------------------
    kAfterTouch = 128,          // After Touch (associated to Channel Pressure)
    kPitchBend  = 129,          // Pitch Bend Change

    kCountCtrlNumber,           // = 130

    //---Extra for kLegacyMIDICCOutEvent-
    kCtrlProgramChange       = 130,  // use LegacyMIDICCOutEvent.value only
    kCtrlPolyPressure        = 131,  // value = pitch, value2 = pressure
    kCtrlQuarterFrame        = 132,  // value only
    kSystemSongSelect        = 133,  // value only
    kSystemSongPointer       = 134,  // value = LSB, value2 = MSB
    kSystemCableSelect       = 135,  // value only
    kSystemTuneRequest       = 136,  // value only
    kSystemMidiClockStart    = 137,  // value only
    kSystemMidiClockContinue = 138,  // value only
    kSystemMidiClockStop     = 139,  // value only
    kSystemActiveSensing     = 140,  // value only
```

This is why `IMidiMapping::getMidiControllerAssignment` documents that `midiControllerNumber` "could be bigger than 127".

## C.11 Other constant groups

- **PlugType subcategories** (`ivstaudioprocessor.h`, `namespace PlugType`): 25 `Fx|*` strings — `kFx` `"Fx"`, `kFxAnalyzer`, `kFxBass`, `kFxChannelStrip`, `kFxDelay`, `kFxDistortion`, `kFxDrums`, `kFxDynamics`, `kFxEQ`, `kFxFilter`, `kFxGenerator`, `kFxGuitar`, `kFxInstrument`, `kFxInstrumentExternal`, `kFxMastering`, `kFxMicrophone`, `kFxModulation`, `kFxNetwork`, `kFxPitchShift`, `kFxRestoration`, `kFxReverb`, `kFxSpatial`, `kFxSurround`, `kFxTools`, `kFxVocals` — 7 instrument strings — `kInstrument`, `kInstrumentDrum`, `kInstrumentExternal`, `kInstrumentPiano`, `kInstrumentSampler`, `kInstrumentSynth`, `kInstrumentSynthSampler` — plus behavioural markers `kAmbisonics`, `kAnalyzer` (not selectable as insert), `kNoOfflineProcess`, `kOnlyARA`, `kOnlyOfflineProcess`, `kOnlyRealTime` (`"OnlyRT"`), `kSpatial`, `kSpatialFx`, `kUpDownMix`, and channel hints `kMono`/`kStereo`/`kSurround`. Combined with `|` in `PClassInfo2::subCategories`.
- **ChannelContext AttrIDs** (`ivstchannelcontextinfo.h`): `kChannelUIDKey` (string), `kChannelUIDLengthKey` (int64), `kChannelRuntimeIDKey` (int64), `kChannelNameKey` (string), `kChannelNameLengthKey` (int64), `kChannelColorKey` (ColorSpec), `kChannelIndexKey` (int64), `kChannelIndexNamespaceOrderKey` (int64), `kChannelIndexNamespaceKey` (string), `kChannelIndexNamespaceLengthKey` (int64), `kChannelImageKey` (PNG binary), `kChannelPluginLocationKey` (int64). `enum ChannelPluginLocation { kPreVolumeFader = 0, kPostVolumeFader, kUsedAsPanner }`. `ColorSpec` is ARGB with `GetAlpha/GetRed/GetGreen/GetBlue` helpers. **Channel indices start at 1, not 0**, and so does the index-namespace order.
- **FunctionNameType** (`ivstparameterfunctionname.h`): `kCompGainReduction`, `kCompGainReductionMax`, `kCompGainReductionPeakHold`, `kCompResetGainReductionMax`, `kLowLatencyMode` (0 = disable, 1 = enable), `kDryWetMix` (0.0 = dry only, 0.5 = 50/50, 1.0 = wet only), `kRandomize`, `kPanPosCenterX` / `kPanPosCenterY` / `kPanPosCenterZ` (each `[0, 1]`).
- **Preset attributes** (`vstpresetkeys.h`): `PresetAttributes::kStateType`, `PresetAttributes::kFilePathStringType`, plus `StateType::kProject` — how a plugin distinguishes a project load from a preset load inside `setState`.
- **XML representation** (`ivstrepresentation.h`): `namespace LayerType { kKnob=0, kPressedKnob, kSwitchKnob, kSwitch, kLED, kLink, kDisplay, kFader, kEndOfLayerType }` with a parallel `static const FIDString layerTypeFIDString[]`; `namespace CurveType { kSegment, kValueList }`; `namespace Attributes { kStyle, kLEDStyle, kSwitchStyle, kKnobTurnsPerFullRange, kFunction, kFlags }`; `namespace AttributesFunction` (`kPanPosCenterXFunc`, `kPanPosCenterYFunc`, `kPanPosFrontLeftXFunc`, `kPanPosFrontLeftYFunc`, `kPanPosFrontRightXFunc`, `kPanPosFrontRightYFunc`, `kPanRotationFunc`, `kPanLawFunc`, `kPanMirrorModeFunc`, `kPanLfeGainFunc`, `kGainReductionFunc`, `kSoloFunc`, `kMuteFunc`, `kVolumeFunc`); `namespace AttributesStyle` (`kInverseStyle`; LED `wrapLeft`/`wrapRight`/`spread`/`boostCut`/`singleDot`; switch `push`/`pushIncLooped`/`pushDecLooped`/`pushInc`/`pushDec`/`latch`); `namespace AttributesFlags { kHideableFlag }`; generic remote names `GENERIC`, `GENERIC_4_CELLS`, `GENERIC_8_CELLS`, `GENERIC_12_CELLS`, `GENERIC_24_CELLS`, `GENERIC_N_CELLS`, `QUICK_CONTROL_8_CELLS`.

---

# Deprecations & version notes

Explicit in-header deprecations are sparse — only **two** exist in the whole tree:

1. `PFactoryInfo::kLicenseCheck` — "This flag is deprecated, do not use anymore, resp. it will get ignored from Cubase/Nuendo 12 and later."
2. `IString::setText8`/`setText16` (`base/istringresult.h`) — "!Do not use this method!" (early implementations took ownership of the given pointer).

Soft supersessions marked with `[replaces …]` rather than "deprecated": **IMidiMapping2 replaces IMidiMapping** and **IMidiLearn2 replaces IMidiLearn** (both 3.8.0). A host should query the `2` variant first and fall back — and should advertise which it supports through `IPlugInterfaceSupport`.

`IProcessContextRequirements` is a special case worth flagging for host implementers: it is an *extension* interface but tagged `[mandatory]`, and pre-3.7 plugins that lack it get the legacy "all information, possibly less accurate" path.

The only interfaces a host is likely to skip on desktop: the InterApp Audio trio (iOS-only) and the Wayland pair (Linux-only).
