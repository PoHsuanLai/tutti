# tutti-au-host

Audio Unit (AUv2) plugin hosting for macOS, via Apple's AudioToolbox framework.

## What this is

The Audio Unit host among tutti's four plugin-format hosts. It enumerates the
AUs installed on the system, instantiates and initializes them, renders audio
and MIDI through them, and exposes parameters, presets, state save/restore,
bus layouts, offline rendering and the Cocoa editor.

Most applications do not use this crate directly: `tutti-plugin` (with its `au`
feature) and `tutti-plugin-server` build on it and give every format one
interface. Use it directly when you want an AU host without the rest of the
engine.

The main types:

- `AuLoaded`: an AU that is instantiated and configured but not initialized:
  parameters, presets, state, bus layouts, identity, MIDI mappings and the
  editor all work. It implements `tutti_plugin_types::Loaded`, `Control`,
  `InProcess` and `OutOfProcess`.
- `AuEditorInstance`: its `OutOfProcess::Editor`, an `AuLoaded` loaded for the
  editor alone, in the host process while the audio instance runs in the server.
- `AuProcessor` / `AuController`: the two halves `Loaded::activate`
  (`AudioUnitInitialize`) splits it into. The Processor renders and nothing
  else, and is the only thing the realtime thread holds; the Controller is the
  main thread's, and derefs to `AuLoaded`. `Loaded::deactivate` rejoins them
  (see [below](#why-an-activation-splits-the-unit)).
- [`AuComponentInfo`] and [`AuType`]: an installed AU, as returned by
  `component::enumerate_components_of_type` (macOS).
- `StreamConfig` / `AuBusLayout`: the sample rate, block size and channel
  layout to instantiate with.
- `AuParameterListener`: parameter and property change notifications.
- `AuPreset`, `AuPresetIdentity` and `read_preset_metadata`: factory presets
  and `.aupreset` files (the interchange format Logic, Live, Reaper and
  GarageBand all read and write).
- `AuMidiOutput`: keeps a MIDI-output callback installed while held.
- `TransportState`: the tempo and position the AU's host callbacks read during
  render.
- `RenderNotify`: a pre/post-render callback on the AU's render thread.
- `AuEditor`: the AU's Cocoa view, embedded in a host window.
- [`AuError`]: the error type, with [`LoadStage`] for load failures.

The public modules `parameters`, `midi_out`, `offline` and `render_notify` hold
the lower-level functions that take a raw `AudioUnit`.

## What it does not own

**The shared vocabulary.** `EditorSize`, `TransportInfo` and
`WindowHandle` come from `tutti-plugin-types`, and `Samples` / `Seconds` from
`tutti-types`; all are re-exported here.

**Subprocess isolation.** This crate hosts in-process. `tutti-plugin-server`
wraps it in a subprocess behind its `au` feature, and `tutti-plugin` is the
host side.

**The comparison against the other three formats.** VST3, CLAP, AU and VST2
model the same two-state shape and reach three different answers. The
comparative account is in `tutti-plugin`'s crate documentation under *The
plugin state machine*. Only AU's own half is below.

## Platform

**macOS-only.** The crate compiles on other platforms but exposes only
[`AuError`] and the [`component`] types there; everything else is
`#[cfg(target_os = "macos")]`.

The examples below are `no_run`, because instantiating an AU needs a real
`.component` registered with the system.

## Quick start

```rust,no_run
# #[cfg(target_os = "macos")]
# {
use tutti_au_host::component::enumerate_components_of_type;
use tutti_au_host::{AuLoaded, AuType};
use tutti_plugin_types::{Loaded, MainThread, MaxBlock, Prepare, SampleRate, Samples};

let effects = enumerate_components_of_type(AuType::Effect);
if let Some(info) = effects.first() {
    let main = MainThread::claim().unwrap();
    let au = AuLoaded::new(info.component, 44100.0, 512).unwrap();
    let prepare = Prepare::new(SampleRate(44100.0), MaxBlock::of(Samples(512))).unwrap();
    let Ok((mut processor, _controller)) = au.activate(&main, prepare) else {
        return;
    };

    let input = vec![vec![0.0f32; 512]; 2];
    let mut output = vec![vec![0.0f32; 512]; 2];
    let in_refs: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
    processor.render_f32(&in_refs, &mut out_refs, 512).unwrap();
}
# }
```

### What the AU reports back

```rust,no_run
# #[cfg(target_os = "macos")]
# {
use tutti_au_host::component::enumerate_components_of_type;
use tutti_au_host::{AuLoaded, AuType};

# let effects = enumerate_components_of_type(AuType::Effect);
# if let Some(info) = effects.first() {
let au = AuLoaded::new(info.component, 44100.0, 512).unwrap();

// `Samples` and `Seconds`, not bare numbers: the engine's unit newtypes reach
// this far, and stop at the C ABI below them.
let latency = au.get_latency().unwrap();
let tail = au.get_tail_time().unwrap();
println!("{latency:?} of latency, {tail:?} of tail");

// Factory presets are AU's own vocabulary — there is no shared preset type to
// translate into, so `AuPreset` is re-exported at the crate root.
for preset in au.factory_presets() {
    println!("{}", preset.name);
}
# }
# }
```

## Why an activation splits the unit

An AU has two states: *loaded* (instantiated, its parameters, state and editor
all reachable) and *initialized* (`AudioUnitInitialize` has run and render
buffers exist, the only state in which a block renders). This crate gives them
the shape every format host has (`tutti_plugin_types::Loaded`): `activate`
consumes the `AuLoaded` and returns a Processor and a Controller, and
`deactivate` takes both back.

**The Processor owns what only rendering touches**: the renderer and the guard
that uninitializes the unit before the renderer (whose input callback the unit
may still call while initialized) is dropped, the SysEx reassembly buffer, the
transport the unit pulls from, and a lent copy of the parameter ranges.
`render_f32` and `render_f64` are its calls; AUv2 has no native f64 render, so
an f64 block costs a narrowing round trip through the one scratch it owns.

**The Controller owns the main thread's side**: it derefs to `AuLoaded`, so
parameters, state, presets, bypass, identity and the editor stay reachable while
audio runs, and adds the live latency and tail. A parameter write is applied
directly (`AudioUnitSetParameter` is callable from any thread), so there is no
edit queue between the halves.

**Both transitions are fallible.** `AudioUnitInitialize` can fail, and so can the
uninitialize that would undo it. A failed activation hands the unit back in
`Refused`, except for the one case that cannot: the unit refused the input render
callback *and* the compensating uninitialize, after which no valid `AuLoaded`
exists (`Refused::plugin` is `None`). A failed deactivation hands both halves
back in `Stuck`, still initialized.

A rate, block ceiling or render mode is the `Prepare` of an activation
(`AuLoaded::initialize` is the same transition with the typed `AuError`): the
stream is configured, the mode written, and the unit initialized, in that
order, so a change is a deactivate and an activate. The invariant the guard
keeps: `AudioUnitUninitialize` runs before the heap-pinned render scratch is
freed, since that call is what proves the AU's `ref_con` is dead.

## Channel order is keyed on the tag, never on the count

The discipline `topology.rs` exists to enforce, and worth knowing before touching
any layout code. Apple defines **four different orders of the identical six
speakers** (`MPEG_5_1_A` through `_D`), plus `Emagic_Default_7_1` and `WAVE_7_1`
as further orders of one eight-speaker set. A channel count cannot distinguish
any of them, so nothing may branch on width.

Not hypothetical: `AudioUnit_5_1` is `L R C LFE Ls Rs` while `AudioUnit_5_0` is
`L R Ls Rs C`, so the same index means centre in one and a surround in the other.
Separately, Apple's `Ls`/`Rs` are the engine's SMPTE `SL`/`SR`, and `Rls`/`Rrs`
are `BL`/`BR` — mapping by name rather than by channel swaps a 5.1 bus's
surrounds into its 7.1 rear slots.

## Features

None. Platform gating is `cfg`, not a feature.

## Threading and real-time safety

`AuProcessor::render` (and `render_f32`/`render_f64`) belong on the audio thread and does not allocate in the
steady state: render buffers are sized at initialization. Loading,
initialization, state, presets and the editor belong on the main thread; the
editor, state and preset-file methods take a `tutti_plugin_types::MainThread`,
and `AuEditor` also asks the OS (`pthread_main_np`).

## License

MIT OR Apache-2.0

[`AuComponentInfo`]: crate::AuComponentInfo
[`AuType`]: crate::AuType
[`component`]: crate::component
[`AuError`]: crate::AuError
[`LoadStage`]: crate::LoadStage
