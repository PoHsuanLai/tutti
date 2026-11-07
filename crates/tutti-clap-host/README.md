# tutti-clap-host

Safe hosting for [CLAP](https://cleveraudio.org/) audio plugins — the FFI is
wrapped so a caller writes no `unsafe` of its own.

## What this is

The CLAP host among tutti's four plugin-format hosts. It loads a `.clap` bundle,
drives audio and MIDI processing, exposes the parameter tree, audio- and
note-port topology, state save/restore with CLAP's save contexts, the plugin's
editor, and the host-callback polling surface.

Most applications reach it through `tutti-plugin` (feature `clap`) or
`tutti-plugin-server`, which give every format one interface. Use it directly
when you want a CLAP host without the rest of the engine.

Three types carry the lifecycle: [`ClapLoaded`] is the mapped, instantiated
plugin, which is not rendering. Activating it takes `self` by value and splits it
into a [`ClapProcessor`], the realtime half, where
[`process`][`ClapProcessor::process`] lives and whose sample format is the
activation's `Prepare::format`, and a [`ClapController`], the
main-thread half, which derefs to [`ClapLoaded`] so the whole control surface
stays reachable while audio runs. Both halves hold one plugin instance; the
reasoning for that is on `tutti_clap_ffi::Plugin`, which owns the instance and
hands the Processor its audio-thread half. They implement the shared
`Loaded`, `Processor` and `Controller` traits of `tutti-plugin-types`, which is
what `Lifecycle` drives.

The other types you will meet first:

- `AudioBufferMut` (`tutti-plugin-types`): the per-block audio, `F32` or `F64`,
  a [`process`][`ClapProcessor::process`] call reads and writes; its width is
  the processor's. [`AudioBuffer32`] / [`AudioBuffer64`] build the two.
- [`ClapProcessContext`]: per-block MIDI, parameter changes and transport.
- [`PluginInfo`]: the descriptor (id, name, vendor, features) read at load.
- [`ClapError`]: the error type, with [`LoadStage`] for load failures.

## What it does not own

**The shared vocabulary.** [`ParameterChanges`], [`TransportInfo`],
[`WindowHandle`], [`EditorSize`] and the `ParameterInfo` that
[`parameter_list`][`ClapLoaded::parameter_list`] hands back are
[`tutti-plugin-types`](../tutti-plugin-types)' — re-exported here so a caller
can stay format-agnostic, not defined here. MIDI is the workspace-wide UMP
[`tutti_midi_types::MidiEvent`], re-exported as [`MidiEvent`]; there is **no
CLAP-specific MIDI event trait to implement**. What *is* CLAP-native is
[`ClapNoteExpression`], the per-voice expression type, which keeps its own name
precisely so it does not shadow the shared one.

**Subprocess isolation.** This crate hosts in-process; `tutti-plugin-server` puts
it in a subprocess and [`tutti-plugin`](../tutti-plugin) is the host side.

**The comparison against the other three formats.** VST3, CLAP, AU and VST2 model
the same two-state shape and reach three different answers. The comparative
account, and the rule for choosing among the strategies, is in `tutti-plugin`'s
crate documentation under *The plugin state machine*. Only CLAP's own half is
below.

## Quick start

Load, activate, render one block. `no_run`: the load needs a real `.clap` bundle
on disk.

```rust,no_run
use tutti_ump::{MidiChannel, MidiGroup};
use tutti_clap_host::{
    AudioBuffer32, ClapLoaded, ClapProcessContext, LoadOptions, MidiEvent, TransportInfo,
};
use tutti_plugin_types::{AudioBufferMut, Loaded, Prepare, RenderMode, SampleFormat, SampleRate, Samples};

// `ClapLoaded` is the GUI / parameter / state stage. The rate and the block
// length are not load parameters: they are the activation's.
let loaded = ClapLoaded::load("/usr/lib/clap/MyPlugin.clap", LoadOptions::default())?;
println!("{} by {}", loaded.info().name, loaded.info().vendor);

// A refusal hands the plugin back in the error, so a plugin that declines f64
// can be retried at f32 without reloading.
let prepare = Prepare {
    rate: SampleRate(48_000.0),
    max_block: Samples(512),
    format: SampleFormat::Float32,
    mode: RenderMode::Realtime,
};
let (mut processor, controller) = loaded
    .activate(prepare)
    .map_err(|refused| tutti_clap_host::ClapError::ProcessError(refused.reason.to_string()))?;

// 512 is the block length in FRAMES; each channel slice holds that many.
let silence = vec![0.0f32; 512];
let inputs: [&[f32]; 2] = [&silence, &silence];
let (mut left, mut right) = (vec![0.0f32; 512], vec![0.0f32; 512]);
let mut outputs: [&mut [f32]; 2] = [&mut left, &mut right];
let mut buffer = AudioBuffer32::new(&inputs, &mut outputs, 48_000.0);

let transport = TransportInfo::default().with_tempo(120.0).with_playing(true);
let midi = [MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 16384)];
processor.process(&mut AudioBufferMut::F32(buffer), &ClapProcessContext {
    midi: &midi,
    transport: Some(&transport),
    ..Default::default()
})?;

// The halves rejoin into the plugin, deactivated.
let _loaded = ClapLoaded::deactivate(processor, controller)
    .map_err(|stuck| tutti_clap_host::ClapError::ProcessError(stuck.reason.to_string()))?;
# Ok::<(), tutti_clap_host::ClapError>(())
```

### Parameters and state

[`ClapController`] derefs to [`ClapLoaded`], so all of this is reachable while
audio is running as well as before activation. `no_run` for the same reason as
above.

```rust,no_run
# use tutti_clap_host::{ClapLoaded, LoadOptions, StateContext};
use tutti_plugin_types::{Control, Normalized, ParamId};
# fn ex() -> Result<(), Box<dyn std::error::Error>> {
let mut plugin = ClapLoaded::load("/usr/lib/clap/MyPlugin.clap", LoadOptions::default())?;

// CLAP addresses parameters by an opaque, plugin-chosen `clap_id`, never by
// position. `parameters` projects them into the shared `ParameterInfo`, so a
// consumer never learns which format it is reading.
for info in plugin.parameters() {
    println!("{} {:?}", info.qualified_name(), info.bounds());
}

// Values are normalized `0..=1` at the trait. A loaded plugin takes the write
// now; a controller queues it to the Processor, which applies it at the head
// of its next block.
plugin.set_parameter(ParamId::new(0), Normalized::new(0.75));
plugin.set_parameter(ParamId::new(1), Normalized::new(0.5));

// State save/restore is legal before any buffer exists.
let saved = plugin.save_state()?;
plugin.load_state(&saved)?;

// A plugin that implements the context-aware extension can be asked for a
// blob meant specifically for a preset file. This does NOT fall back to the
// plain save on refusal — the two legitimately differ, and answering a preset
// request with a project-context blob writes the wrong bytes into a `.preset`
// and surfaces only when someone loads it.
let preset = plugin.state_with_context(StateContext::ForPreset)?;
println!("{} bytes of preset state", preset.len());
# Ok(()) }
```

### Editor

```rust,no_run
# use tutti_clap_host::{ClapLoaded, ContentScale, LoadOptions, WindowHandle};
# fn ex(native_view_ptr: *mut std::ffi::c_void) -> tutti_clap_host::Result<()> {
# let mut plugin = ClapLoaded::load("/usr/lib/clap/MyPlugin.clap", LoadOptions::default())?;
if plugin.has_editor() {
    // The only unsafe boundary in the public API.
    let handle = unsafe { WindowHandle::from_raw(native_view_ptr) };
    let size = plugin.open_editor(handle, ContentScale::IDENTITY)?;
    println!("editor is {}x{}", size.width, size.height);
}
plugin.close_editor();
# Ok(()) }
```

## Why a refused `activate` hands the plugin back

CLAP's own forced constraint, and the one shape here no other format crate
shares: the plugin survives a refusal.

A refusal is not the end of the plugin, only of that configuration. CLAP plugins
routinely decline 64-bit audio or a rate, and a host that discovers this the
ordinary way — by asking — should not have to pay for a reload to fall back. So
[`Refused`](tutti_plugin_types::Refused) carries the **unconsumed**
[`ClapLoaded`] back beside the reason, and the caller retries at `f32` against
the same mapped library, the same instance, and the same already-read parameter
tree.

A plain `Result<_, PluginError>` would have destroyed the `ClapLoaded` on the one
path where it is still perfectly good, making "ask, and fall back" strictly more
expensive than guessing from [`supports_f64`][`ClapLoaded::supports_f64`] and
hoping. [`ClapLoaded::negotiate_format`] is the ask: it reads the ports again
and answers the format to activate at.
Both failure paths preserve it: the `f64`-unsupported check returns before any
FFI runs, and a plugin-side `activate` refusal leaves the instance untouched and
not active. Activation never leaves half a pair: the Processor and the
Controller exist only on success.

### Why `Deref` from `ClapController` to `ClapLoaded` is sound

CLAP does not *revoke* the loaded-state operations on activation; it re-tags some
of their **threading** contracts. The two states partition what is legal in one
direction only — `process` requires active — and never in the other, so
activation is additive and the pre-activation surface stays reachable.

Where a contract does change with activation, the condition is read at the call
rather than assumed from the type, which works precisely because the flag lives
on the *inner* `ClapLoaded`: [`flush_params`][`ClapLoaded::flush_params`] is
tagged `[active ? audio-thread : main-thread]` and branches on that live flag, so
the same code reached through `Deref` from a `ClapController` takes an
[`AudioThreadClaim`][`host::AudioThreadClaim`] where a bare `ClapLoaded` would
assert the main thread.

The distinction that gates it is *active* versus *processing*, and they are not
the same fact. They disagree for the whole window between activation and the
first [`process`][`ClapProcessor::process`] — exactly when a host pushes initial
parameter values. Reading `processing` there takes the main-thread branch against
a plugin that considers itself active, and a plugin with a validation layer
reports the host for calling on the wrong thread while the values are dropped in
silence. The flag lives apart from the Processor for the same reason: the
Processor sets it under the `[audio-thread]` claim and the Controller's teardown
reads it under that claim.

### Why the halves may share one instance

The Processor calls only the audio-side entries (`start_processing`, `process`,
`stop_processing`, `reset`), through its own audio-thread handle (`tutti_clap_ffi::AudioSide`), under the
`[audio-thread]` claim; the Controller calls only the main-thread set, through
the `ClapLoaded` it holds. CLAP annotates every entry with its thread, and the
two sets are the ones a host calls concurrently. The plugin is destroyed on the
main thread only, by the Controller's drop, which retires the activation first.
The argument, clause by clause, is on `tutti_clap_ffi::Plugin`; this crate
holds no `unsafe` of its own (`#![forbid(unsafe_code)]`).

A parameter written to a [`ClapController`] does not call across: it is queued
in an edit ring, and the Processor drains it at the head of its next block into
`PARAM_VALUE` events ahead of the block's own automation.

## Features

`default = []`.

- `clap-extras` — the less commonly needed part of the CLAP surface:
  param-indication, remote controls, context menus, triggers, tuning, audio-port
  reconfiguration, POSIX-fd polling, preset load, track info, transport
  requests, and the plugin undo/redo and resource-directory extensions. Without
  it those methods and types are not public. The core extensions (audio and
  note ports, params, state, gui, latency, voice-info, render mode, note names)
  are always available.

## Threading and real-time safety

CLAP tags every call `[main-thread]` or `[audio-thread]`, and this crate follows
the tags. Load, activate, deactivate, state, the editor and polling belong on
the thread that loaded the plugin; debug builds assert it.
[`ClapProcessor::process`] belongs on the audio thread and does not allocate: the
scratch it renders through is sized at activation.

A plugin that spreads its own work over `clap.thread-pool` has its requests
run on the pool given to [`ClapController::set_thread_pool`] (any
`tutti_graph::Pool`, such as `tutti_core::WorkerPool`), with the calling audio
thread taking part; without one, they are refused and the plugin runs the
tasks itself. Only a request from inside the plugin's `process` is served.

## License

MIT OR Apache-2.0

[`ClapLoaded`]: crate::ClapLoaded
[`ClapLoaded::parameter_list`]: crate::ClapLoaded::parameter_list
[`ClapLoaded::supports_f64`]: crate::ClapLoaded::supports_f64
[`ClapLoaded::negotiate_format`]: crate::ClapLoaded::negotiate_format
[`ClapLoaded::flush_params`]: crate::ClapLoaded::flush_params
[`ClapController`]: crate::ClapController
[`ClapController::set_thread_pool`]: crate::ClapController::set_thread_pool
[`ClapProcessor`]: crate::ClapProcessor
[`ClapProcessor::process`]: crate::ClapProcessor::process
[`ClapError`]: crate::ClapError
[`LoadStage`]: crate::LoadStage
[`AudioBuffer32`]: crate::AudioBuffer32
[`AudioBuffer64`]: crate::AudioBuffer64
[`ClapProcessContext`]: crate::ClapProcessContext
[`PluginInfo`]: crate::PluginInfo
[`ClapNoteExpression`]: crate::ClapNoteExpression
[`ParameterChanges`]: crate::ParameterChanges
[`TransportInfo`]: crate::TransportInfo
[`WindowHandle`]: crate::WindowHandle
[`EditorSize`]: crate::EditorSize
[`MidiEvent`]: crate::MidiEvent
[`host::AudioThreadClaim`]: crate::host::AudioThreadClaim
[`tutti_midi_types::MidiEvent`]: tutti_midi_types::MidiEvent
