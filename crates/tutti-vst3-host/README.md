# tutti-vst3-host

VST3 plugin hosting through the native COM interfaces. No C++ SDK needed at the
call site.

## What this is

The VST3 host among tutti's four plugin-format hosts. It loads a `.vst3`
bundle, walks the class factory, drives audio + MIDI processing, exposes the
parameter tree, unit and program lists, note expression, state save/restore and
the plugin's own editor, and implements the host-side COM interfaces a plugin
discovers through `IComponentHandler::queryInterface`.

The lifecycle is encoded in the type system: [`Library`] holds the loaded
DSO and its factory, and [`Vst3Loaded`] is an `initialize()`'d plugin suitable
for GUI and parameter work. Activating it takes `self` by value and splits it
into a [`Vst3Processor<T>`][`Vst3Processor`], the realtime half, where
[`process`][`Vst3Processor::process`] lives, and a [`Vst3Controller`], the
main-thread half, which derefs to [`Vst3Loaded`] so the whole control surface
stays reachable while audio runs. Both halves hold one plugin instance; the
reasoning for that is on the crate's private shell. They implement the shared
`Loaded`, `Processor` and `Controller` traits of `tutti-plugin-types`, which is
what `Lifecycle` drives. Transitions move ownership, so the compiler rejects a
call that is illegal in the current state.

Most applications reach it through `tutti-plugin` (feature `vst3`) or
`tutti-plugin-server`, which give every format one interface. Use it directly
when you want a VST3 host without the rest of the engine.

The other types you will meet first: [`AudioBuffer`] (the per-block audio),
[`InputEvents`] (per-block MIDI and other events), [`PluginInfo`] (the class
metadata read at load) and [`Vst3Error`] (with [`LoadStage`] for load failures).

## What it does not own

**The shared vocabulary.** [`ParameterChanges`], [`TransportInfo`],
[`ProcessOutput`], [`WindowHandle`], [`EditorSize`] and the `Features` bitset are
[`tutti-plugin-types`](../tutti-plugin-types)' — this crate re-exports them so
a caller can stay format-agnostic, but does not define them. MIDI is the
workspace-wide UMP [`tutti_midi_types::MidiEvent`], re-exported here as
[`MidiEvent`]; there is **no VST3-specific MIDI event trait to implement**. To
send something that is not already a `MidiEvent`, convert it. The crate root
exposes the typed VST3 events and the translation: [`MidiEncoder`] encodes a `MidiEvent`
stream into them, following each channel's per-note bend range (RPN #00/07)
across blocks, and [`EventExt::to_midi`] decodes one back.

**Subprocess isolation.** This crate hosts in-process.
[`tutti-plugin-server`](../tutti-plugin-server) wraps it in a subprocess, and
[`tutti-plugin`](../tutti-plugin) is the host side of that bridge.

**The comparison against the other three formats.** VST3, CLAP, AU and VST2 model
the same two-state shape and reach three different answers. The comparative
account, and the rule for choosing between the strategies, is in
`tutti-plugin`'s crate documentation under *The plugin state machine*. Only
VST3's own half is below.

## Quick start

Walk the stages, then render one block. `no_run`: the load needs a real `.vst3`
bundle on disk.

```rust,no_run
use tutti_ump::{MidiChannel, MidiGroup};
use tutti_plugin_types::{Prepare, RenderMode, SampleFormat, SampleRate, Samples};
use tutti_vst3_host::{
    AudioBuffer, InputEvents, LoadOptions, MidiEvent, TransportInfo, Vst3Loaded,
};

// Initialized. Parameters and the editor are reachable here, with no
// activation cost paid. The rate and the block length are not load
// parameters: they are the activation's.
let loaded = Vst3Loaded::load("/usr/lib/vst3/MyPlugin.vst3", LoadOptions::new())?;
println!("{} by {}", loaded.plugin_info().name, loaded.plugin_info().vendor);

// Activated. `T` fixes the sample width: `f64` is refused unless the plugin
// advertises 64-bit support, and a refusal hands the plugin back in the error,
// so it can be retried at `f32` without reloading.
let prepare = Prepare {
    rate: SampleRate(48_000.0),
    max_block: Samples(512),
    format: SampleFormat::Float32,
    mode: RenderMode::Realtime,
};
let (mut processor, controller) = loaded
    .activate_typed::<f32>(prepare)
    .map_err(|refused| tutti_vst3_host::Vst3Error::NotSupported(refused.reason.to_string()))?;

// 512 is the block length in FRAMES; each channel slice holds that many.
let silence = vec![0.0f32; 512];
let inputs: [&[f32]; 2] = [&silence, &silence];
let (mut left, mut right) = (vec![0.0f32; 512], vec![0.0f32; 512]);
let mut outputs: [&mut [f32]; 2] = [&mut left, &mut right];
let mut buffer = AudioBuffer::new(&inputs, &mut outputs, 48_000.0);

// The sample rate stays a raw `f64` here: this is a C ABI, which is where
// the engine's unit newtypes deliberately stop.
let midi = [MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xC000)];
let events = InputEvents {
    midi: &midi,
    ..Default::default()
};
let transport = TransportInfo::default().with_tempo(120.0).with_playing(true);

let out = processor
    .process(&mut buffer, &events, None, &transport)
    .map_err(|failed| tutti_vst3_host::Vst3Error::NotSupported(failed.to_string()))?;
println!("plugin emitted {} MIDI events", out.midi_events.len());

// The halves rejoin into the plugin, deactivated.
let _loaded = Vst3Loaded::deactivate_typed(processor, controller)
    .map_err(|(_, _, reason)| tutti_vst3_host::Vst3Error::NotSupported(reason.to_string()))?;
# Ok::<(), tutti_vst3_host::Vst3Error>(())
```

### Parameters, state, and what the plugin's own GUI did

The [`Vst3Controller`] derefs to [`Vst3Loaded`], so all of this is reachable
while audio is running as well as before activation. `no_run` for the same
reason as above.

```rust,no_run
# use std::path::Path;
# use tutti_plugin_types::{Control, Normalized, ParamId};
# use tutti_vst3_host::{LoadOptions, Vst3Loaded};
# fn ex() -> Result<(), Box<dyn std::error::Error>> {
let mut plugin = Vst3Loaded::load(Path::new("/usr/lib/vst3/MyPlugin.vst3"), LoadOptions::new())?;

// Parameters are addressed by an opaque, plugin-chosen `ParamID`, never by
// position: `parameter_ids` lists them, `parameter_info` reads one by id, and
// the id is what `Control::set_parameter` takes. The titles are UTF-16 in the
// ABI, so they are read through the `_string` accessors rather than as fields.
for id in plugin.parameter_ids() {
    let Some(info) = plugin.parameter_info(id) else {
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
if let Some(&id) = plugin.parameter_ids().first() {
    plugin.set_parameter(id, Normalized::of(0.75));
}

// State save/restore is legal before any buffer exists — a project load
// restores state before the transport rolls.
let saved = plugin.save_state()?;
plugin.load_state(&saved)?;

// One drain, four kinds of news — not a poller per kind. Edits, progress and
// unit news come to the closure in arrival order; `restartComponent` requests
// are coalesced into the returned outcome, because acting on one means a
// deactivate/reactivate cycle the loaded type cannot perform itself. Nothing is
// collected for you, so the poll allocates nothing.
use tutti_vst3_host::PluginNotification;
let mut edits = 0;
let restart = plugin.poll_plugin_notifications(|news| {
    if let PluginNotification::Edit(_) = news {
        edits += 1;
    }
});
println!("{edits} param edits; latency changed: {}", restart.latency_changed);
# Ok(()) }
```

### Editor

```rust,no_run
# use std::path::Path;
# use tutti_vst3_host::{ContentScale, LoadOptions, Vst3Loaded, WindowHandle};
# fn ex(native_view_ptr: *mut std::ffi::c_void) -> tutti_vst3_host::Result<()> {
# let mut plugin = Vst3Loaded::load(Path::new("/usr/lib/vst3/MyPlugin.vst3"), LoadOptions::new())?;
if plugin.has_editor() {
    // The only unsafe boundary in the public API.
    let handle = unsafe { WindowHandle::from_raw(native_view_ptr) };
    let size = plugin.open_editor(handle, ContentScale::IDENTITY)?;
    println!("editor is {}x{}", size.width, size.height);
}
plugin.close_editor();
# Ok(()) }
```

## Why the lifecycle is types rather than one flag

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
the [`Vst3Controller`] `Deref`s to [`Vst3Loaded`], so that whole surface stays
callable while active. That is sound because VST3 keeps it legal in the active
state: the type-state boundary removes `process` from the loaded state and adds
nothing to the active one. What the Controller changes is the write of a
parameter, which it queues to the Processor as well as applying to the edit
controller, so a plugin whose component is separate from its controller hears of
it in the next block's `inputParameterChanges`.

[`Library`] is a stage for a different reason: one bundle commonly exposes
several plugin classes, so the loaded DSO plus its factory outlives any one
instance and is shared by `Arc`.

### What rides on the activation, and what does not

Everything in the shared `Prepare` is fixed at activation, because VST3 fixes it
there: `setupProcessing` names the sample size, the rate, the block length and
the process mode in one call that may only run while the plugin is deactivated.
A change of any of them is a deactivate and an activate, which `Lifecycle` plans
as a reconfiguration and the plugin survives, state and all. The one exception
the spec carves out is the realtime/prefetch pair, switchable on a live
Processor ([`Vst3Processor::set_prefetch`]). A reconfiguration is a bracket
around the active state, not a stage a host parks in.

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
[`poll_plugin_notifications`][`Vst3Loaded::poll_plugin_notifications`] drain,
except `restartComponent`, which is also raised as the core's requests
([`Vst3Loaded::take_requests`]) for a host that drives a `Lifecycle`. None of the
callbacks allocates, since plugins call some of them from their process thread:
restart requests are two atomic words (the flags, and the core's requests), and
the rest go on a bounded queue of inline values whose overflow is counted in
`Vst3Loaded::dropped_notifications` and reported once by the next poll. A caller
on an audio thread that needs only the restarts reads them with
`Vst3Loaded::take_requests` (or `take_restart`), and drops the rest with
`Vst3Loaded::discard_notifications`.

## Features

`default = []`.

- `conformance` — exposes `host::conformance`, a test-only observation seam that
  hands the fully-built `ProcessData` to an installed observer just before
  `IAudioProcessor::process`. The host-conformance tests use it to check what
  this host assembles against the VST3 spec. Enabling it makes the build script
  compile Steinberg's HostChecker and a reference plugin against the VST3 SDK
  (the in-repo submodules, or `VST3_SDK_DIR`).

## Threading and real-time safety

[`Vst3Processor::process`] belongs on the audio thread and does not allocate:
buffers and event lists are sized at activation, and the Controller's writes
reach it through a ring it drains at the head of the block. Loading, activation,
state and the editor belong on the main thread.

## License

MIT OR Apache-2.0

[`Library`]: crate::Library
[`Vst3Loaded`]: crate::Vst3Loaded
[`Vst3Loaded::poll_plugin_notifications`]: crate::Vst3Loaded::poll_plugin_notifications
[`Vst3Loaded::take_requests`]: crate::Vst3Loaded::take_requests
[`Vst3Processor`]: crate::Vst3Processor
[`Vst3Processor::process`]: crate::Vst3Processor::process
[`Vst3Processor::set_prefetch`]: crate::Vst3Processor::set_prefetch
[`Vst3Controller`]: crate::Vst3Controller
[`AudioBuffer`]: crate::AudioBuffer
[`InputEvents`]: crate::InputEvents
[`PluginInfo`]: crate::PluginInfo
[`Vst3Error`]: crate::Vst3Error
[`LoadStage`]: crate::LoadStage
[`ParameterChanges`]: crate::ParameterChanges
[`TransportInfo`]: crate::TransportInfo
[`ProcessOutput`]: crate::ProcessOutput
[`WindowHandle`]: crate::WindowHandle
[`EditorSize`]: crate::EditorSize
[`MidiEvent`]: crate::MidiEvent
[`MidiEncoder`]: crate::MidiEncoder
[`EventExt::to_midi`]: crate::EventExt::to_midi
