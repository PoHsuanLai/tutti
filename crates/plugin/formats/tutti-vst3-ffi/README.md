# tutti-vst3-ffi

The unsafe half of the VST3 host. Every `unsafe` the host needs to talk to a
VST3 plugin lives here, behind safe handles over the [`vst3`] crate's COM
bindings; the host crate above it, `tutti-vst3-host`, is
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
| `ComponentHandler` | `IComponentHandler` (and 2, 3, bus activation, `IProgress`, `IUnitHandler`) onto a request word and a bounded notification queue |
| `Vst3Error` | `tresult` by name (`False`, `InvalidArgument`, `NotImplemented`, ...) |

Not public: the `IBStream`, `IAttributeList`/`IMessage`, `IPlugFrame`,
`IHostApplication` and `IRunLoop` objects, which a `Plugin` builds for itself.
Every host callback runs under a panic guard (`scripts/check-guards.sh`) and
answers an error code instead of unwinding into the plugin.

## How the thread rules are in the types

A `[UI-thread]` entry is a method of `Plugin`, which is `Send` but not `Sync`;
`process` is a method of `AudioSide`, which names no main-thread entry. The
`AudioSide` and the plugin's `Drop` take one claim, so teardown waits out a
block in flight and a later block answers `NotInitialized` without reaching the
plugin. A `BoundBuses` borrows both the scratch and the channel slices its
pointers came from, so a `process` call without a fresh binding cannot be
written, and a block longer than a bound channel (or the aux block) is refused.

## What the embedder owes

Nothing unsafe. `WindowHandle` (from `tutti-plugin-types`) is where "this is a
valid parent window" is claimed.

## Testing

The unit tests sit beside the code (wrapper arithmetic, the host-implemented
objects through their raw vtables, the allocation-free gates). The host crate's
suites drive a real plugin, the in-repo `audio-probe`, through this crate.

[`vst3`]: https://docs.rs/vst3
