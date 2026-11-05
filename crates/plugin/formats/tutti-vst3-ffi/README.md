# tutti-vst3-ffi

The unsafe half of the VST3 host. Every `unsafe` the host needs to talk to a
VST3 plugin lives here, behind safe handles over the [`vst3`] crate's COM
bindings; the host crate above it is
`#![forbid(unsafe_code)]` and holds the logic (the lifecycle, the shell, the
MIDI mapping, the edit ring).

This crate names nothing of tutti's except the value crates `tutti-types`,
`tutti-plugin-types` and `tutti-midi-types`.

## What is here

| | |
|---|---|
| `Module` | opens a library, runs its entry once per image, lists the factory's classes (`classes()`), creates a `Plugin` |
| `Plugin` | the **main-thread half** of an instance: component, processor, edit controller and its optional interfaces, the editor. Every `[UI-thread]` entry is a method; `Drop` runs the SDK's teardown order |
| `AudioSide` | the **audio-thread half**, taken once: `process` and `set_processing`, each under the instance's claim |
| `BusBuffers`, `BoundBuses`, `ProcessIo` | the per-bus `AudioBusBuffers` scratch, bound to one block's channel slices for exactly one `process` call; `SampleWidth` is the only width-specific type |
| `EventList`, `ParameterChangesImpl` | `IEventList` and `IParameterChanges`, preallocated: nothing allocates on the audio thread, and staging past capacity is refused and counted |
| `Event`, `Payload`, `EventKind` | the SDK's event union as an enum: plain-data payloads wrap the SDK struct, pointer-carrying ones index the list's storage. MIDI translation is `tutti-vst3-host`'s |
| `ComponentHandler` | `IComponentHandler` (and 2, 3, bus activation, `IProgress`, `IUnitHandler`) onto a request word and a bounded notification queue |
| `Vst3Error` | `tresult` by name (`False`, `InvalidArgument`, `NotImplemented`, ...) |

Not public: the `IBStream`, `IAttributeList`/`IMessage`, `IPlugFrame`,
`IHostApplication` and `IRunLoop` objects, which a `Plugin` builds for itself.
Every host callback runs under a panic guard (`scripts/check-guards.sh`) and
answers an error code instead of unwinding into the plugin.

## How the thread rules are in the types

A `[UI-thread]` entry is a method of `Plugin`, which is `Send` but not `Sync`;
`process` is a method of `AudioSide`, which names no main-thread entry.

`Plugin::audio_side` hands the audio half out once, and only for a plugin that is
set up (`setup_processing`) and active (`set_active`). While it exists, the
plugin's state changes (`set_active`, `set_processing`, `setup_processing`,
`set_bus_arrangements`, `activate_bus`) answer `Vst3Error::Busy`, so the
transitions the SDK reserves for the non-processing states cannot run beside a
block. `process` refuses a block whose width is not the setup's
`symbolicSampleSize`, that is longer than its `maxSamplesPerBlock`, a bound
channel or the aux block, or whose buses are not the plugin's arrangement (read
when it was activated).

The audio half holds no reference to the plugin, only a pointer it may use under
the instance's claim, which the audio thread only ever `try_lock`s. The plugin's
`Drop` retires the claim first, so a later block answers `NotInitialized`
without waiting, and the plugin and its module are destroyed on the thread that
drops the `Plugin`, never on the audio thread that drops an `AudioSide`.

A `BoundBuses` borrows both the scratch and the channel slices its pointers came
from, so a `process` call without a fresh binding cannot be written.

## What the embedder owes

Nothing unsafe. `WindowHandle` (from `tutti-plugin-types`) is where "this is a
valid parent window" is claimed.

## Testing

The unit tests sit beside the code (wrapper arithmetic, the host-implemented
objects through their raw vtables, the allocation-free gates). The host crate's
suites drive a real plugin, the in-repo `audio-probe`, through this crate.

[`vst3`]: https://docs.rs/vst3
