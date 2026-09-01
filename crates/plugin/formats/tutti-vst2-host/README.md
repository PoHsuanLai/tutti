# tutti-vst2-host

VST2 plugin hosting — audio, MIDI, parameters, state, and the native editor.

## What this is

The VST2 one of tutti's four format hosts. It loads VST2 plugins (`.vst`,
`.dll`, `.so`), drives audio + MIDI processing, exposes parameters and state
save/restore, and embeds the plugin's native editor into a host-supplied window.

Built on the vendored `vst-tutti` fork of the [`vst`](https://docs.rs/vst) crate,
which handles the AEffect-level FFI and adds back the host-side `audioMaster`
callbacks upstream swallowed. This crate adds what a host actually needs on top:
pre-allocated render scratch buffers, the MIDI codec, callback wiring,
transport-info bookkeeping, state save/restore, and editor lifecycle.

One type carries the whole lifecycle: [`Vst2Instance`]. That is the format's own
contract showing through, not a shortcut — see the constraint section below.

## What it does not own

**The shared vocabulary.** [`ParameterInfo`], [`TransportInfo`],
[`WindowHandle`], [`EditorSize`] and [`Samples`] are
[`tutti-plugin-types`](../../tutti-plugin-types)' — re-exported here so a caller
can stay format-agnostic, not defined here. MIDI is the workspace-wide UMP
[`tutti_midi_types::MidiEvent`], re-exported as [`MidiEvent`].

**Subprocess isolation.** VST2 fuses the editor and the audio processor into a
single `AEffect` instance, so you cannot host the editor in one process and audio
in another against the same plugin: callers must accept in-process hosting.
Isolation, where wanted, is the caller's job —
[`tutti-plugin-server`](../../tutti-plugin-server) does it by wrapping this crate
in a subprocess, which is what keeps a crashing VST2 from killing the host.

**The comparison against the other three formats.** VST3, CLAP, AU and VST2 model
the same two-state shape and reach three different answers. The comparative
account, and the rule for choosing among the strategies, is in `tutti-plugin`'s
crate documentation under *The plugin state machine*. Only VST2's own half is
below.

## Quick start

`no_run`: the load needs a real VST2 binary on disk.

```rust,no_run
use std::path::Path;
use tutti_vst2_host::{ProcessContext, RenderScratch, Vst2Instance};

let mut plugin = Vst2Instance::load(
    Path::new("/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst"),
    48_000.0,
    512,
)?;

let meta = plugin.metadata().clone();
let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, 512);

let inputs: Vec<Vec<f32>> =
    (0..meta.num_inputs.count()).map(|_| vec![0.0; 512]).collect();
let mut outputs: Vec<Vec<f32>> =
    (0..meta.num_outputs.count()).map(|_| vec![0.0; 512]).collect();

let in_refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
let mut out_refs: Vec<&mut [f32]> =
    outputs.iter_mut().map(|v| v.as_mut_slice()).collect();

let ctx = ProcessContext::new(48_000.0);
let _midi_out: &tutti_vst2_host::MidiEventVec =
    plugin.process_f32(&in_refs, &mut out_refs, 512, &ctx, &mut scratch);
# Ok::<(), tutti_vst2_host::Vst2Error>(())
```

### Parameters address positionally — VST2 alone among the four

`getParameter(effect, index)` and `numParams` are both `i32` in the ABI, and
consecutive parameters really are consecutive. That is the whole reason
[`tutti_plugin_types::ParamAddress`] has two arms: the other three formats hand
out an opaque, plugin-chosen handle on which arithmetic means nothing.

```rust,no_run
# use std::path::Path;
# use tutti_plugin_types::ParamAddress;
# use tutti_vst2_host::Vst2Instance;
# fn ex() -> tutti_vst2_host::Result<()> {
let plugin = Vst2Instance::load(Path::new("/usr/lib/vst/MyPlugin.so"), 48_000.0, 512)?;

for info in plugin.parameter_list() {
    // Always the `Index` arm here. A VST2 host may iterate positions; a
    // VST3/CLAP/AU host may not, which is what the enum keeps apart.
    let ParamAddress::Index(index) = info.id else {
        unreachable!("VST2 addresses parameters positionally")
    };
    println!("{index}: {} ({:?})", info.qualified_name(), info.bounds());
}

// The write takes the raw `i32` position and a normalized `0..=1` value —
// a C ABI, which is where the engine's unit newtypes stop.
plugin.set_parameter(0, 0.5);
# Ok(()) }
```

### State and editor

```rust,no_run
# use std::path::Path;
# use tutti_vst2_host::{Vst2Instance, WindowHandle};
# fn ex(native_view_ptr: *mut std::ffi::c_void) -> tutti_vst2_host::Result<()> {
# let mut plugin = Vst2Instance::load(Path::new("/usr/lib/vst/MyPlugin.so"), 48_000.0, 512)?;
// Chunk-based if the plugin declares `preset_chunks`, else the per-parameter
// fallback; `load_state` reads the header back and reports a refusal rather
// than swallowing it.
let saved = plugin.save_state()?;
plugin.load_state(&saved)?;

// The editor is the same `AEffect` the audio path drives — hence in-process.
let size = plugin.open_editor(unsafe { WindowHandle::from_raw(native_view_ptr) })?;
println!("editor is {}x{}", size.width, size.height);
plugin.close_editor();
# Ok(()) }
```

## Why one type carries the whole lifecycle

VST2's own forced constraint, and the reason this crate has no `Vst2Loaded`.

`effOpen`, `effSetSampleRate`, `effSetBlockSize` and the first
`effMainsChanged(1)` all run inside [`Vst2Instance::load`], so the instance is
ready to process the moment it exists. A `Vst2Loaded` type would sit over a
window no caller can observe and would carry no operations the fused type does
not — the split would buy a type boundary that guards nothing.

### Suspend and resume are a bracket, not a stage

[`suspend`][`Vst2Instance::suspend`] and [`resume`][`Vst2Instance::resume`] are
ordinary `&mut self` methods over a `resumed` flag, and the flag is not a demoted
lifecycle stage. In VST2 the suspended state is a *reconfiguration bracket* — the
thing a host does around a sample-rate or block-size change, because plugins
reallocate rate-dependent buffers in `effMainsChanged` and assume they are not
concurrently processing. It is not a state a host parks in: no VST2 operation is
legal only while suspended and interesting to a host, which is exactly what a
stage would need in order to be worth naming.

That is why the bracket is spelled as a private `suspend_for_reconfigure` /
`restore_after_reconfigure` pair rather than as two public transitions, and why it
is **edge-triggered**: the suspend half reports whether it actually dispatched,
and the restore half takes that answer, so a plugin already suspended on entry
stays suspended on exit. [`set_sample_rate`][`Vst2Instance::set_sample_rate`],
[`set_block_size`][`Vst2Instance::set_block_size`] and
[`reset_processing_state`][`Vst2Instance::reset_processing_state`] all share it.
An unconditional `suspend(); set(); resume()` would instead resume a plugin the
caller had deliberately stopped, and — since `effMainsChanged` is not documented
as idempotent — churn a full buffer teardown and reallocation on every setter
call.

## Parameter and MIDI metadata

[`ParameterProperties`] (`effGetParameterProperties`) plus the MIDI-metadata
family ([`MidiProgram`], [`MidiKeyName`], [`MidiProgramCategory`]) is the
**whole** of VST2's parameter/MIDI metadata surface. In particular VST2 has no
CC→parameter mapping query at all; the module docs carry the opcode evidence.

## Features

None. The crate is the VST2 loader.

## Testing

`tutti-vst2-test-plugin` is a dev-dependency, so `cargo test` builds a real
reference plugin's cdylib in the same invocation and the conformance harness
loads it. The harness re-opens that cdylib with `libloading` rather than reading
the linked rlib's statics: the two are separate images with separate globals, and
only the cdylib's are the ones the host actually touched.

RT-safety regressions run under a disabled global allocator, matching the wiring
the CLAP and VST3 hosts use.

## License

MIT OR Apache-2.0

[`Vst2Instance`]: crate::Vst2Instance
[`Vst2Instance::load`]: crate::Vst2Instance::load
[`Vst2Instance::suspend`]: crate::Vst2Instance::suspend
[`Vst2Instance::resume`]: crate::Vst2Instance::resume
[`Vst2Instance::set_sample_rate`]: crate::Vst2Instance::set_sample_rate
[`Vst2Instance::set_block_size`]: crate::Vst2Instance::set_block_size
[`Vst2Instance::reset_processing_state`]: crate::Vst2Instance::reset_processing_state
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
[`tutti_plugin_types::ParamAddress`]: tutti_plugin_types::ParamAddress
[`tutti_midi_types::MidiEvent`]: tutti_midi_types::MidiEvent
