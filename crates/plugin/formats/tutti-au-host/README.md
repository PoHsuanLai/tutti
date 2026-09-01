# tutti-au-host

Audio Unit (AUv2) plugin hosting for macOS, via Apple's AudioToolbox framework.

## What this is

The AU one of tutti's four format hosts: low-level bindings plus the host
machinery around them. `AuInstance` is the entry point — load → initialize →
render — and it tracks initialization state so `process` is only reachable once
the AU is ready. Around it:

- `component` — enumerating installed AUs by `AuType`.
- `bus` / `stream` / `channel_layout` / `topology` — bus counts, stream formats,
  Apple layout tags, and the tag ↔ `ChannelTopology` conversion.
- `parameters` / `listener` — parameter query and set, plus change listeners.
- `preset` / `aupreset` — factory presets, and `.aupreset` file I/O (the
  interchange format Logic, Live, Reaper and GarageBand all read and write).
- `midi_map` / `midi_out` — MIDI in and the output-callback registration.
- `transport` / `render_notify` — host callbacks the AU pulls during render.
- `offline` — bounce-time facilities a live-only host never needs and an
  exporting host cannot do without: offline render mode, in-place processing,
  and the push-model render path.
- `editor` — the Cocoa view factory and editor lifecycle.

## What it does not own

**The shared vocabulary.** `EditorSize`, `MidiEvent`, `TransportInfo` and
`WindowHandle` are [`tutti-plugin-types`](../../tutti-plugin-types)', and
`Samples` / `Seconds` are `tutti-types`' — all re-exported here (the latter two
because `get_latency` and `get_tail_time` hand them back, and a consumer that
cannot name a returned type cannot bind it), none defined here.

**Subprocess isolation.** This crate hosts in-process;
[`tutti-plugin-server`](../../tutti-plugin-server) wraps it in a subprocess
behind its `au` feature, and [`tutti-plugin`](../../tutti-plugin) is the host
side.

**The comparison against the other three formats.** VST3, CLAP, AU and VST2 model
the same two-state shape and reach three different answers. The comparative
account, and the rule for choosing among the strategies, is in `tutti-plugin`'s
crate documentation under *The plugin state machine*. Only AU's own half is
below.

## Platform

**macOS-only.** The crate compiles on other platforms but exposes no
functionality: nearly every module is `#[cfg(target_os = "macos")]`, and
`topology.rs` carries an inner `#![cfg(target_os = "macos")]`, which is why its
re-exports are gated to match — an ungated re-export would name items that do not
exist and fail to build on Linux.

That gate is load-bearing for the **doctests** as well as the build. Every
example below is wrapped in `# #[cfg(target_os = "macos")] { … }`, so off macOS
the body compiles to an empty block and a green `cargo test --doc` on Linux says
nothing about whether the example is correct. Only a macOS run type-checks them.
They are additionally `no_run`, because instantiating an AU needs a real
`.component` registered with the system.

## Quick start

```rust,no_run
# #[cfg(target_os = "macos")]
# {
use tutti_au_host::component::{enumerate_components_of_type, AuType};
use tutti_au_host::instance::AuInstance;

let effects = enumerate_components_of_type(AuType::Effect);
if let Some(info) = effects.first() {
    let mut au = unsafe { AuInstance::new(info.component, 44100.0, 512) }.unwrap();
    au.initialize().unwrap();

    let input = vec![vec![0.0f32; 512]; 2];
    let mut output = vec![vec![0.0f32; 512]; 2];
    let in_refs: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
    au.process(&in_refs, &mut out_refs, 512).unwrap();
}
# }
```

### What the AU reports back

```rust,no_run
# #[cfg(target_os = "macos")]
# {
use tutti_au_host::component::{enumerate_components_of_type, AuType};
use tutti_au_host::instance::AuInstance;

# let effects = enumerate_components_of_type(AuType::Effect);
# if let Some(info) = effects.first() {
let mut au = unsafe { AuInstance::new(info.component, 44100.0, 512) }.unwrap();
au.initialize().unwrap();

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

## Why the public type carries its state as data

AU's own forced constraint, and the reason this crate has no consuming
`AuInstance` type-state.

An AU has two states: *loaded* (instantiated, its parameters, state and editor
all reachable) and *ready* (`AudioUnitInitialize` has run and render buffers
exist, the only state in which `AuInstance::process` succeeds).

**The transitions are fallible in both directions.** `AuInstance::initialize` and
`AuInstance::uninitialize` take `&mut self` and return `Result<()>`, because
`AudioUnitInitialize` can fail and so can the uninitialize that would undo it. A
consuming transition has to produce one of the two types, and a failed transition
belongs to neither — the unit is neither in the state it started in nor the one
it was headed for. Worse, an AU can refuse *both* the callback install and the
compensating uninitialize, after which no valid state exists at all. A private
`State` enum names that honestly — its `Empty` variant is what a failed recovery
leaves behind, and every accessor then reports the instance as dead — while a
pair of consuming functions could only do so by handing back a third type nobody
wants.

**Most operations do not care which state they are in.** Parameters, state
save/restore, the editor, bus layouts and the raw `AudioUnit` pointer all work
loaded or ready. Splitting the public type would force a caller to re-thread
ownership through transitions for the sake of operations that were never
state-dependent — a real cost paid for a guarantee that covers only `process`.

So the split lives one layer down, where it is free: `AuLoaded` and `AuActive`
*are* distinct types with consuming transitions
(`AuActive::uninitialize(self) -> Result<AuLoaded, (Self, AuError)>`), and
`AuInstance` is the façade holding one or the other. The type system enforces the
ordering internally — notably the invariant that `AudioUnitUninitialize` runs
before the heap-pinned render scratch is freed, since that call is what proves
the AU's `ref_con` is dead — while the public API stays a single type. The price
is that calling `process` too early is a runtime error rather than a compile
error, and it is the AU's own refusal that reports it, arriving as
`AuError::OsStatus` carrying `kAudioUnitErr_Uninitialized`.

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

## Testing

The Cocoa editor-lifecycle tests run on the main thread — `harness = false` on the
`au_gui_lifecycle_main` target so it owns `main()`, which is the only way to reach
the main thread under cargo and is what AppKit requires. Its `main()` also calls
`mark_main_thread()`, which arms the affinity assert `AuEditor` makes.

## License

MIT OR Apache-2.0
