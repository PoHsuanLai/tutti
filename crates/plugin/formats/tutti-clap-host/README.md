# tutti-clap-host

Safe hosting for [CLAP](https://cleveraudio.org/) audio plugins — the FFI is
wrapped so a caller writes no `unsafe` of its own.

## What this is

The CLAP one of tutti's four format hosts. It loads a `.clap` bundle, drives
audio and MIDI processing, exposes the parameter tree, audio- and note-port
topology, state save/restore with CLAP's save contexts, the plugin's editor, and
the host-callback polling surface.

Two types carry the lifecycle: [`ClapLoaded`] is the mapped, instantiated plugin,
and [`ClapActive<T>`][`ClapActive`] is the same plugin with buffers allocated and
[`process`][`ClapActive::process`] legal. Both transitions take `self` by value.

## What it does not own

**The shared vocabulary.** [`ParameterChanges`], [`TransportInfo`],
[`WindowHandle`], [`EditorSize`] and the `ParameterInfo` that
[`parameter_list`][`ClapLoaded::parameter_list`] hands back are
[`tutti-plugin-types`](../../tutti-plugin-types)' — re-exported here so a caller
can stay format-agnostic, not defined here. MIDI is the workspace-wide UMP
[`tutti_midi_types::MidiEvent`], re-exported as [`MidiEvent`]; there is **no
CLAP-specific MIDI event trait to implement**. What *is* CLAP-native is
[`ClapNoteExpression`], the per-voice expression type, which keeps its own name
precisely so it does not shadow the shared one.

**`ClapInstance`.** That name belongs to
[`tutti-plugin-server`](../../tutti-plugin-server), which wraps this crate's
types as its per-format loader adapter. Nothing in this crate is called that.

**Subprocess isolation.** This crate hosts in-process; `tutti-plugin-server` puts
it in a subprocess and [`tutti-plugin`](../../tutti-plugin) is the host side.

**The comparison against the other three formats.** VST3, CLAP, AU and VST2 model
the same two-state shape and reach three different answers. The comparative
account, and the rule for choosing among the strategies, is in `tutti-plugin`'s
crate documentation under *The plugin state machine*. Only CLAP's own half is
below.

## Quick start

Load, activate, render one block. `no_run`: the load needs a real `.clap` bundle
on disk.

```rust,no_run
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_clap_host::{AudioBuffer32, ClapLoaded, MidiEvent, ProcessContext, TransportInfo};

// `ClapLoaded` is the GUI / parameter / state stage; sample rate and the
// max block length are fixed here, so `activate` takes no arguments.
let loaded = ClapLoaded::load("/usr/lib/clap/MyPlugin.clap", 48_000.0, 512)?;
println!("{} by {}", loaded.info().name, loaded.info().vendor);

// `activate` hands `self` back on refusal — a plugin that declines f64 can
// be retried at f32 without reloading.
let mut active = loaded.activate::<f32>().map_err(|(_, e)| e)?;

// 512 is the block length in FRAMES; each channel slice holds that many.
let silence = vec![0.0f32; 512];
let inputs: [&[f32]; 2] = [&silence, &silence];
let (mut left, mut right) = (vec![0.0f32; 512], vec![0.0f32; 512]);
let mut outputs: [&mut [f32]; 2] = [&mut left, &mut right];
let mut buffer = AudioBuffer32::new(&inputs, &mut outputs, 48_000.0);

let transport = TransportInfo::default().with_tempo(120.0).with_playing(true);
let midi = [MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 16384)];
active.process(&mut buffer, &ProcessContext {
    midi: &midi,
    transport: Some(&transport),
    ..Default::default()
})?;
# Ok::<(), tutti_clap_host::ClapError>(())
```

### Parameters and state

[`ClapActive`] derefs to [`ClapLoaded`], so all of this is reachable while audio
is running as well as before activation. `no_run` for the same reason as above.

```rust,no_run
# use tutti_clap_host::{ClapLoaded, StateContext};
# fn ex() -> tutti_clap_host::Result<()> {
let mut plugin = ClapLoaded::load("/usr/lib/clap/MyPlugin.clap", 48_000.0, 512)?;

// CLAP addresses parameters by an opaque, plugin-chosen `clap_id`, never by
// position. `parameter_list` projects them into the shared `ParameterInfo`, so
// a consumer never learns which format it is reading.
for info in plugin.parameter_list() {
    println!("{} {:?}", info.qualified_name(), info.bounds());
}

// The write takes the id and a normalized `0..=1` value, and chains.
plugin.set_parameter(0, 0.75).set_parameter(1, 0.5);

// State save/restore is legal before any buffer exists.
let saved = plugin.state()?;
plugin.set_state(&saved)?;

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
# use tutti_clap_host::{ClapLoaded, WindowHandle};
# fn ex(native_view_ptr: *mut std::ffi::c_void) -> tutti_clap_host::Result<()> {
# let mut plugin = ClapLoaded::load("/usr/lib/clap/MyPlugin.clap", 48_000.0, 512)?;
if plugin.has_editor() {
    // The only unsafe boundary in the public API.
    let handle = unsafe { WindowHandle::from_raw(native_view_ptr) };
    let size = plugin.open_editor(handle)?;
    println!("editor is {}x{}", size.width, size.height);
}
plugin.close_editor();
# Ok(()) }
```

## Why `activate` returns `Err((Self, ClapError))`

CLAP's own forced constraint, and the one shape here no other format crate
shares.

A refusal is not the end of the plugin, only of that configuration. CLAP plugins
routinely decline 64-bit audio, and a host that discovers this the ordinary way —
by asking — should not have to pay for a reload to fall back. So the error variant
carries the **unconsumed** [`ClapLoaded`] back beside the [`ClapError`], and the
caller retries at `f32` against the same mapped library, the same instance, and
the same already-read parameter tree.

A plain `Result<_, ClapError>` would have destroyed the `ClapLoaded` on the one
path where it is still perfectly good, making "ask, and fall back" strictly more
expensive than "guess from [`supports_f64`][`ClapLoaded::supports_f64`] and hope".
Both failure paths preserve it: the `f64`-unsupported check returns before any
FFI runs, and a plugin-side `activate` refusal leaves the instance untouched and
not active.

The `Err` is large, which is why `clippy::result_large_err` is suppressed at that
function rather than obeyed — boxing would add a heap allocation on the failure
path in order to hide the exact ownership return that is the point.

### Why `Deref` from `ClapActive` to `ClapLoaded` is sound

CLAP does not *revoke* the loaded-state operations on activation; it re-tags some
of their **threading** contracts. The two states partition what is legal in one
direction only — `process` requires active — and never in the other, so
activation is additive and the pre-activation surface stays reachable.

Where a contract does change with activation, the condition is read at the call
rather than assumed from the type, which works precisely because the flag lives
on the *inner* `ClapLoaded`: [`flush_params`][`ClapLoaded::flush_params`] is
tagged `[active ? audio-thread : main-thread]` and branches on that live flag, so
the same code reached through `Deref` from a `ClapActive` takes an
[`AudioThreadClaim`][`host::AudioThreadClaim`] where a bare `ClapLoaded` would
assert the main thread.

The distinction that gates it is *active* versus *processing*, and they are not
the same fact. They disagree for the whole window between
[`activate`][`ClapLoaded::activate`] and the first
[`process`][`ClapActive::process`] — exactly when a host pushes initial parameter
values. Reading `processing` there takes the main-thread branch against a plugin
that considers itself active, and a plugin with a validation layer reports the
host for calling on the wrong thread while the values are dropped in silence.

## Features

`default = []`.

- `clap-extras` — the speculative part of the CLAP surface that no consumer (the
  `tutti-plugin-server` loader, the in-process GUI host) currently calls:
  param-indication, remote controls, context menus, triggers, tuning, audio-port
  reconfiguration, POSIX-fd polling, preset load, and the plugin undo/redo and
  resource-directory extensions. The code and its tests stay compiled either way
  — the feature gates the public re-exports, so the default API surface stays
  lean. The consumed extensions (audio and note ports, params, state, gui,
  latency, voice-info, render mode, note names) are always on.

## Testing

`tutti-clap-test-plugin` is a dev-dependency, so `cargo test` builds a real
reference plugin's cdylib in the same invocation and the conformance suites load
it — no third-party plugin need be installed, and the suite is not macOS-only.
RT-safety regressions run under a disabled global allocator, the same wiring the
VST2 and VST3 hosts use.

## License

MIT OR Apache-2.0

[`ClapLoaded`]: crate::ClapLoaded
[`ClapLoaded::activate`]: crate::ClapLoaded::activate
[`ClapLoaded::parameter_list`]: crate::ClapLoaded::parameter_list
[`ClapLoaded::supports_f64`]: crate::ClapLoaded::supports_f64
[`ClapLoaded::flush_params`]: crate::ClapLoaded::flush_params
[`ClapActive`]: crate::ClapActive
[`ClapActive::process`]: crate::ClapActive::process
[`ClapError`]: crate::ClapError
[`ClapNoteExpression`]: crate::ClapNoteExpression
[`ParameterChanges`]: crate::ParameterChanges
[`TransportInfo`]: crate::TransportInfo
[`WindowHandle`]: crate::WindowHandle
[`EditorSize`]: crate::EditorSize
[`MidiEvent`]: crate::MidiEvent
[`host::AudioThreadClaim`]: crate::host::AudioThreadClaim
[`tutti_midi_types::MidiEvent`]: tutti_midi_types::MidiEvent
