# tutti-vst3-host

VST3 plugin hosting through the native COM interfaces. No C++ SDK needed at the
call site.

## What this is

The VST3 one of tutti's four format hosts. It loads a `.vst3` bundle, walks the
class factory, drives audio + MIDI processing, exposes the parameter tree, unit
and program lists, note expression, state save/restore and the plugin's own
editor, and implements the host-side COM interfaces a plugin discovers through
`IComponentHandler::queryInterface`.

The public surface is a three-stage lifecycle encoded in the type system:
[`Vst3Library`] holds the loaded DSO and its factory, [`Vst3Loaded`] is an
`initialize()`'d plugin suitable for GUI and parameter work, and
[`Vst3Active<T>`][`Vst3Active`] adds the activation state
[`Vst3Active::process`] requires. Transitions move ownership, so the compiler
rejects a call that is illegal in the current state.

## What it does not own

**The shared vocabulary.** [`ParameterChanges`], [`TransportInfo`],
[`ProcessOutput`], [`WindowHandle`], [`EditorSize`] and the `Features` bitset are
[`tutti-plugin-types`](../../tutti-plugin-types)' — this crate re-exports them so
a caller can stay format-agnostic, but does not define them. MIDI is the
workspace-wide UMP [`tutti_midi_types::MidiEvent`], re-exported here as
[`MidiEvent`]; there is **no VST3-specific MIDI event trait to implement**. To
send something that is not already a `MidiEvent`, convert it — [`events`] exposes
the typed VST3 event structs and [`Vst3Event::from_midi`] the conversion.

**Subprocess isolation.** This crate hosts in-process.
[`tutti-plugin-server`](../../tutti-plugin-server) wraps it in a subprocess, and
[`tutti-plugin`](../../tutti-plugin) is the host side of that bridge.

**The comparison against the other three formats.** VST3, CLAP, AU and VST2 model
the same two-state shape and reach three different answers. The comparative
account, and the rule for choosing between the strategies, is in
`tutti-plugin`'s crate documentation under *The plugin state machine*. Only
VST3's own half is below.

## Quick start

Walk the stages, then render one block. `no_run`: the load needs a real `.vst3`
bundle on disk.

```rust,no_run
use std::path::Path;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_vst3_host::{
    AudioBuffer, MidiEvent, TransportInfo, Vst3InputEvents, Vst3Loaded,
};

// Stage 2: initialized. Parameters and the editor are reachable here, with
// no activation cost paid.
let loaded = Vst3Loaded::load(Path::new("/usr/lib/vst3/MyPlugin.vst3"))?;
println!("{} by {}", loaded.info().name, loaded.info().vendor);

// Stage 3: activated. `T` fixes the sample width — `f64` errors out unless
// the plugin advertises 64-bit support.
let mut plugin = loaded.activate::<f32>(48_000.0, 512)?;

// 512 is the block length in FRAMES; each channel slice holds that many.
let silence = vec![0.0f32; 512];
let inputs: [&[f32]; 2] = [&silence, &silence];
let (mut left, mut right) = (vec![0.0f32; 512], vec![0.0f32; 512]);
let mut outputs: [&mut [f32]; 2] = [&mut left, &mut right];
let mut buffer = AudioBuffer::new(&inputs, &mut outputs, 48_000.0);

// The sample rate stays a raw `f64` here: this is a C ABI, which is where
// the engine's unit newtypes deliberately stop.
let midi = [MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xC000)];
let events = Vst3InputEvents {
    midi: &midi,
    ..Default::default()
};
let transport = TransportInfo::default().with_tempo(120.0).with_playing(true);

let out = plugin.process(&mut buffer, &events, None, &transport);
println!("plugin emitted {} MIDI events", out.midi_events.len());
# Ok::<(), tutti_vst3_host::Vst3Error>(())
```

### Parameters, state, and what the plugin's own GUI did

[`Vst3Active`] derefs to [`Vst3Loaded`], so all of this is reachable while audio
is running as well as before activation. `no_run` for the same reason as above.

```rust,no_run
# use std::path::Path;
# use tutti_vst3_host::Vst3Loaded;
# fn ex() -> tutti_vst3_host::Result<()> {
let mut plugin = Vst3Loaded::load(Path::new("/usr/lib/vst3/MyPlugin.vst3"))?;

// Parameters are addressed by an opaque, plugin-chosen `ParamID`, never by
// position: `parameter_info` reads one by enumeration index, and the `id` it
// hands back is what `set_parameter` takes. The titles are UTF-16 in the ABI,
// so they are read through the `_string` accessors rather than as fields.
for index in 0..plugin.parameter_count() {
    let Some(info) = plugin.parameter_info(index) else {
        continue;
    };
    println!(
        "{} ({}) default {}",
        info.title_string(),
        info.units_string(),
        info.default_normalized_value,
    );
}

// The write takes the id and a normalized `0..=1` value.
if let Some(info) = plugin.parameter_info(0) {
    plugin.set_parameter(info.id, 0.75);
}

// State save/restore is legal before any buffer exists — a project load
// restores state before the transport rolls.
let saved = plugin.get_state()?;
plugin.set_state(&saved)?;

// One drain, four kinds of news — not a poller per kind. `RestartComponent`
// requests are folded into `restart` rather than appearing among the param
// edits, because acting on one means a deactivate/reactivate cycle the loaded
// type cannot perform itself.
let news = plugin.poll_plugin_notifications();
println!(
    "{} param edits, {} progress reports, {} unit events",
    news.param_edits.len(),
    news.progress.len(),
    news.units.len(),
);
# Ok(()) }
```

### Editor

```rust,no_run
# use std::path::Path;
# use tutti_vst3_host::{Vst3Loaded, WindowHandle};
# fn ex(native_view_ptr: *mut std::ffi::c_void) -> tutti_vst3_host::Result<()> {
# let mut plugin = Vst3Loaded::load(Path::new("/usr/lib/vst3/MyPlugin.vst3"))?;
if plugin.has_editor() {
    // The only unsafe boundary in the public API.
    let handle = unsafe { WindowHandle::from_raw(native_view_ptr) };
    let size = plugin.open_editor(handle)?;
    println!("editor is {}x{}", size.width, size.height);
}
plugin.close_editor();
# Ok(()) }
```

## Why the lifecycle is three types rather than one flag

**The pre-activation state is a real place to work, not a construction step.**
VST3 makes almost its entire control surface legal after `initialize()` and
before any buffer exists: the parameter tree and its plain/normalized
conversions, units and program lists, note expression and keyswitch tables,
`IMidiLearn`, physical UI mapping, XML representations, the compatibility JSON,
state save/restore, and the whole editor. That is the whole pre-activation
surface on [`Vst3Loaded`], and a host is *expected* to call it there — a plugin
browser reading parameter metadata, or a GUI-only session, never activates at
all. A state a host genuinely spends time in, with its own operations, earns a
type.

The split costs nothing in reachable surface, which is what makes it affordable:
[`Vst3Active`] `Deref`s to [`Vst3Loaded`], so that whole surface stays callable
while active. That is sound because VST3 keeps it legal in the active state — the
type-state boundary removes `process` from the loaded state and adds nothing to
the active one.

[`Vst3Library`] is a stage for a different reason: one bundle commonly exposes
several plugin classes, so the loaded DSO plus its factory outlives any one
instance and is shared by `Arc`.

### What rides on the transition, and what does not

Two things are fixed at activation because VST3 fixes them there. `T` commits the
sample width, since `setupProcessing` names the sample size in the same call that
sizes the buffers. [`ProcessMode`] is chosen on
[`activate_with_mode`][`Vst3Loaded::activate_with_mode`] rather than on the
instance, because `setupProcessing` delivers it exactly once per activation —
that method's own documentation carries the reasoning, including the one
exception (the realtime↔prefetch pair, switchable via
[`Vst3Active::set_prefetch`]).

Sample rate and block size are *not* frozen the same way.
[`Vst3Active::set_sample_rate`] exists, and it works by running a full
deactivate/reactivate cycle internally, because `setupProcessing` is spec'd for
the disabled state. A reconfiguration is a bracket around the active state, not a
fourth stage a host parks in.

## Host interfaces implemented

All discoverable by the plugin through `IComponentHandler::queryInterface`:

| Interface | What it carries |
|---|---|
| `IComponentHandler` | Parameter edit notifications from the plugin GUI |
| `IComponentHandler2` | Grouped edits, dirty state, editor requests |
| `IComponentHandler3` | Context menu support |
| `IComponentHandlerBusActivation` | Bus activation requests |
| `IProgress` | Progress for long operations (preset load, sample scan) |
| `IUnitHandler` / `IUnitHandler2` | Unit selection and program-list change news |
| `IHostApplication` | Host name identification |
| `IConnectionPoint` | Processor ↔ controller messaging |
| `IBStream` | State serialization |

Everything a plugin sends through these arrives at the one
[`poll_plugin_notifications`][`Vst3Loaded::poll_plugin_notifications`] drain.

## Features

`default = []`.

- `conformance` — exposes `host::conformance`, a test-only observation seam that
  hands the fully-built `ProcessData` to an installed observer just before
  `IAudioProcessor::process`. The host-conformance tests use it to check what
  this host assembles against the VST3 spec.

## Testing

`build.rs` compiles `audio-probe`, a reference VST3 plugin, against the SDK
submodules vendored under `crates/plugin/vendor/vst3-sdk/`. That is
what lets the suite run on a bare checkout instead of needing `VST3_SDK_DIR` to
name an external SDK — but **a `git clone` without `--recursive` leaves those
submodules empty**, and the build script says so. Fix with
`git submodule update --init --recursive`.

RT-safety regressions run under a disabled global allocator, the same wiring the
CLAP and VST2 hosts use.

## License

MIT OR Apache-2.0

[`Vst3Library`]: crate::Vst3Library
[`Vst3Loaded`]: crate::Vst3Loaded
[`Vst3Loaded::activate_with_mode`]: crate::Vst3Loaded::activate_with_mode
[`Vst3Loaded::poll_plugin_notifications`]: crate::Vst3Loaded::poll_plugin_notifications
[`Vst3Active`]: crate::Vst3Active
[`Vst3Active::process`]: crate::Vst3Active::process
[`Vst3Active::set_prefetch`]: crate::Vst3Active::set_prefetch
[`Vst3Active::set_sample_rate`]: crate::Vst3Active::set_sample_rate
[`ProcessMode`]: crate::ProcessMode
[`ParameterChanges`]: crate::ParameterChanges
[`TransportInfo`]: crate::TransportInfo
[`ProcessOutput`]: crate::ProcessOutput
[`WindowHandle`]: crate::WindowHandle
[`EditorSize`]: crate::EditorSize
[`MidiEvent`]: crate::MidiEvent
[`events`]: crate::events
[`Vst3Event::from_midi`]: crate::events::Vst3Event::from_midi
