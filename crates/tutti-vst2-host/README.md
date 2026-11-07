# tutti-vst2-host

VST2 plugin hosting — audio, MIDI, parameters, state, and the native editor.

## What this is

The VST2 host among tutti's four plugin-format hosts. It loads VST2 plugins
(`.vst`, `.dll`, `.so`), drives audio + MIDI processing, exposes parameters and
state save/restore, and embeds the plugin's native editor into a host-supplied
window.

Most applications do not use this crate directly: `tutti-plugin` (with its
`vst2` feature) and `tutti-plugin-server` build on it and give every format one
interface. Use it directly when you want a VST2 host without the rest of the
engine.

Built on the `vst-tutti` fork of the [`vst`](https://docs.rs/vst) crate,
which handles the AEffect-level FFI and adds back the host-side `audioMaster`
callbacks upstream swallowed. This crate adds what a host actually needs on top:
pre-allocated render scratch buffers, the MIDI codec, callback wiring,
transport-info bookkeeping, state save/restore, and editor lifecycle.

A plugin is three types, as in the other three hosts (design 016): [`Vst2Loaded`]
is the plugin after `effOpen` and before the first `effMainsChanged(1)`;
activating it splits it into a [`Vst2Processor`], which renders on the audio
thread, and a [`Vst2Controller`], which runs the dispatcher on the main thread
and derefs to the `Vst2Loaded`. Rejoining them is `deactivate`.

The main types:

- [`Vst2Loaded`]: a loaded plugin; parameters, programs, state, editor.
- [`Vst2Processor`]: the realtime half; process, with its own render scratch.
- [`Vst2Controller`]: the main-thread half; the whole control surface beside a
  running `process`.
- [`Vst2ProcessContext`]: per-block MIDI input, transport and sample rate.
- [`PluginInfo`]: what the plugin declared at load (name, vendor, category).
- [`Vst2Error`]: the error type, with [`LoadStage`] for load failures.

## What it does not own

**The shared vocabulary.** [`ParameterInfo`], [`TransportInfo`],
[`WindowHandle`], [`EditorSize`] and [`Samples`] are
[`tutti-plugin-types`](../tutti-plugin-types)' — re-exported here so a caller
can stay format-agnostic, not defined here. MIDI is the workspace-wide UMP
[`tutti_midi_types::MidiEvent`], re-exported as [`MidiEvent`].

**Subprocess isolation.** VST2 fuses the editor and the audio processor into a
single `AEffect` instance, so you cannot host the editor in one process and audio
in another against the same plugin: callers must accept in-process hosting.
Isolation, where wanted, is the caller's job —
[`tutti-plugin-server`](../tutti-plugin-server) does it by wrapping this crate
in a subprocess, which is what keeps a crashing VST2 from killing the host.

**The comparison against the other three formats.** VST3, CLAP, AU and VST2 model
the same two-state shape and reach three different answers. The comparative
account, and the rule for choosing among the strategies, is in `tutti-plugin`'s
crate documentation under *The plugin state machine*. Only VST2's own half is
below.

## Quick start

`no_run`: the load needs a real VST2 binary on disk.

```rust,no_run
use tutti_plugin_types::{Loaded, Prepare, RenderMode, SampleFormat, SampleRate, Samples};
use tutti_vst2_host::{Vst2ProcessContext, Vst2Loaded};

let plugin = Vst2Loaded::load("/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst")?;
let (inputs_width, outputs_width) = plugin.pins();

// The rate, block size and precision are the activation's.
let Ok((mut processor, controller)) = plugin.activate(Prepare {
    rate: SampleRate(48_000.0),
    max_block: Samples(512),
    format: SampleFormat::Float32,
    mode: RenderMode::Realtime,
}) else {
    panic!("the plugin refused to run");
};

let inputs: Vec<Vec<f32>> =
    (0..inputs_width.count()).map(|_| vec![0.0; 512]).collect();
let mut outputs: Vec<Vec<f32>> =
    (0..outputs_width.count()).map(|_| vec![0.0; 512]).collect();

let in_refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
let mut out_refs: Vec<&mut [f32]> =
    outputs.iter_mut().map(|v| v.as_mut_slice()).collect();

let ctx = Vst2ProcessContext::new(48_000.0);
let _midi_out: &tutti_vst2_host::MidiEventVec =
    processor.process_f32(&in_refs, &mut out_refs, 512, &ctx);

// Ending it rejoins the halves.
let Ok(_plugin) = Vst2Loaded::deactivate(processor, controller) else {
    panic!("the plugin would not stop");
};
# Ok::<(), tutti_vst2_host::Vst2Error>(())
```

[`Lifecycle`](tutti_plugin_types::Lifecycle) drives the same transitions from
the plugin's own requests (`audioMasterIOChanged`, `audioMasterUpdateDisplay`),
which [`Vst2Loaded::take_requests`] collects; VST2 re-reads the figures instead of
restarting ([`RestartRules::NONE`](tutti_plugin_types::RestartRules::NONE)).

### A parameter id is its position — VST2 alone among the four

`getParameter(effect, index)` and `numParams` are both `i32` in the ABI, and
consecutive parameters really are consecutive. The shared [`ParamId`] carries
that index (this crate converts at its boundary); the other three formats hand
out an opaque, plugin-chosen handle on which arithmetic means nothing, and so
does the type.

```rust,no_run
# use tutti_plugin_types::{Control, Normalized};
# use tutti_vst2_host::Vst2Loaded;
# fn ex() -> tutti_vst2_host::Result<()> {
let mut plugin = Vst2Loaded::load("/usr/lib/vst/MyPlugin.so")?;

for info in plugin.parameters() {
    println!("{}: {} ({:?})", info.id, info.qualified_name(), info.bounds());
}

// A normalized `0..=1` value, written to the parameter at position 0.
let first = plugin.parameters()[0].id;
plugin.set_parameter(first, Normalized::of(0.5));
# Ok(()) }
```

### State and editor

```rust,no_run
# use tutti_plugin_types::Control;
# use tutti_vst2_host::{Vst2Loaded, WindowHandle};
# fn ex(native_view_ptr: *mut std::ffi::c_void) -> tutti_vst2_host::Result<()> {
# let mut plugin = Vst2Loaded::load("/usr/lib/vst/MyPlugin.so")?;
// Chunk-based if the plugin declares `preset_chunks`, else the per-parameter
// fallback; `load_state` reads the header back and reports a refusal rather
// than swallowing it.
let saved = plugin.save_state().expect("saves");
plugin.load_state(&saved).expect("restores");

// The editor is the same `AEffect` the audio path drives — hence in-process.
let size = plugin.open_editor(unsafe { WindowHandle::from_raw(native_view_ptr) })?;
println!("editor is {}x{}", size.width, size.height);
plugin.close_editor();
# Ok(()) }
```

## Why the Controller calls the plugin beside `process`

VST2 gives no threading guarantee for the dispatcher: the SDK says the dispatcher
is called from the UI thread and `process` from the audio thread, that
`setParameter` and `getParameter` may be called from either, and nothing about
any other opcode against `process`. Every major host runs the dispatcher on the
main thread while `process` runs on the audio thread, and plugins are written and
tested against that, so the [`Vst2Controller`] does the same: chunks, programs,
parameter display, the editor and the reads of latency and tail all run beside a
rendering Processor, with no gate and no dropped block. That is a convention, not
a rule the format states (see the invariant on the crate's private `Shell`), and
a plugin that breaks it misbehaves in every host.

Only what the plugin cannot take while processing is exclusive: `effSetSampleRate`,
`effSetBlockSize`, `effSetProcessPrecision` and `effMainsChanged` are the
activation and the rejoin, with the Processor recalled first. The bracket is
edge-triggered: `effStopProcess` is sent once, by whoever started processing,
and `effMainsChanged(0)` once, by the rejoin.

## Parameter and MIDI metadata

[`ParameterProperties`] (`effGetParameterProperties`) plus the MIDI-metadata
family ([`MidiProgram`], [`MidiKeyName`], [`MidiProgramCategory`]) is the
**whole** of VST2's parameter/MIDI metadata surface. In particular VST2 has no
CC→parameter mapping query at all; the module docs carry the opcode evidence.

## Features

None. The crate is the VST2 loader.

## Threading and real-time safety

[`Vst2Processor::process_f32`] and [`Vst2Processor::process_f64`] do not
allocate or lock, and are meant for the audio thread; a block the plugin cannot
take (a channel shorter than the block, a 32-bit block for a 64-bit activation)
is an error and renders nothing. Loading, the editor, and the activation and
rejoin belong on the main thread; debug builds assert it once
`tutti_plugin_types::mark_main_thread` has been called. The plugin is not
thread-safe: the Controller and the Processor are the two threads it is
reached from, and nothing else touches it. A Processor that outlives its
Controller does not destroy the plugin on the audio thread: it queues it for the
next control-side call (`activate`, a `flush_edits` tick) to destroy.

## License

MIT OR Apache-2.0

[`Vst2Loaded`]: crate::Vst2Loaded
[`Vst2Loaded::take_requests`]: crate::Vst2Loaded::take_requests
[`Vst2Processor`]: crate::Vst2Processor
[`Vst2Processor::process_f32`]: crate::Vst2Processor::process_f32
[`Vst2Processor::process_f64`]: crate::Vst2Processor::process_f64
[`Vst2Controller`]: crate::Vst2Controller
[`Vst2ProcessContext`]: crate::Vst2ProcessContext
[`PluginInfo`]: crate::PluginInfo
[`Vst2Error`]: crate::Vst2Error
[`LoadStage`]: crate::LoadStage
[`ParameterProperties`]: crate::ParameterProperties
[`MidiProgram`]: crate::MidiProgram
[`MidiKeyName`]: crate::MidiKeyName
[`MidiProgramCategory`]: crate::MidiProgramCategory
[`ParameterInfo`]: crate::ParameterInfo
[`TransportInfo`]: crate::TransportInfo
[`WindowHandle`]: crate::WindowHandle
[`EditorSize`]: crate::EditorSize
[`Samples`]: crate::Samples
[`MidiEvent`]: crate::MidiEvent
[`ParamId`]: tutti_plugin_types::ParamId
[`tutti_midi_types::MidiEvent`]: tutti_midi_types::MidiEvent
