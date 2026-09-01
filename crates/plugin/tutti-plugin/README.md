# tutti-plugin

Out-of-process VST2, VST3, CLAP and AU plugin hosting.

## What this is

Loads audio plugins in isolated subprocesses, bridges audio + MIDI over shared
memory, and exposes each plugin as a fundsp [`AudioUnit`] node. A crash inside a
plugin stays contained to its subprocess — the host keeps running and reports the
error.

## What it does not own

**The format loaders.** VST2, VST3, CLAP and AU each have their own crate under
`formats/`, and each models its own lifecycle in its own vocabulary. This crate
never sees one: a plugin arrives already loaded and activated inside the
subprocess, and `BridgeMessage::PluginLoaded` reports only the outcome. The
comparative account of *why* the four disagree is the one thing this crate does
own — see [The plugin state machine](#the-plugin-state-machine).

**The shared value vocabulary.** `ParameterInfo`, `TransportInfo`, `Features`,
`Preset` and their siblings are
[`tutti-plugin-types`](../tutti-plugin-types)'; the ones a caller cannot avoid
naming are re-exported at this crate's root.

**The subprocess itself.** That is
[`tutti-plugin-server`](../tutti-plugin-server), the guest side of the bridge.

## Quick start

Load a plugin and put its node into a tutti graph. `no_run`: the load spawns a
subprocess against a real `.vst3` / `.clap` on disk.

```rust,no_run
use tutti_core::{dsp::Net, SampleRate};
use tutti_plugin::catalog::Plugin;

// `sample_rate` takes anything convertible to `SampleRate` — the engine's
// unit type, not a bare rate that could be a block size.
let plugin = Plugin::open("/usr/lib/vst3/MyPlugin.vst3", SampleRate::new(48_000.0))?;
println!("{} by {}", plugin.descriptor().name, plugin.descriptor().vendor);

// Two handles, one subprocess. `into_parts` hands back both, because
// `into_unit` alone consumes the `Plugin` and the control surface is still
// wanted afterwards — the plugin dies when the last of either drops.
let (unit, handle) = plugin.into_parts();
println!("reported latency: {:?}", handle.loaded().latency());

// The node is a fundsp `AudioUnit`, so it enters `Net` like any other.
let mut net = Net::new(0, 2);
let id = net.push(unit);
net.pipe_output(id);
net.commit();
# Ok::<(), tutti_plugin::BridgeError>(())
```

A host that does not already know the path discovers one first — see
[`catalog`], whose two pure functions ([`discover`](catalog::discover) and
[`PluginRecord::probe`](catalog::PluginRecord::probe)) need no feature flag, and
whose [`Plugins`](catalog::Plugins) adds incremental rescan and crash recovery on
top.

## Architecture

Every plugin runs in its own `tutti-plugin-server` subprocess. The host talks to
it over two channels:

- **Control** (Unix socket / named pipe) — editor open/close, parameter reads,
  state save/restore. Main-thread, blocking.
- **Audio** (shared memory slab) — audio + MIDI + parameter changes per block.
  Audio-thread, lock-free.

This split lets the audio path stay RT-safe while the main thread does IPC freely
for anything editor-related.

### Two handles per plugin

Loading a plugin returns two values:

- [`handles::PluginClient`] — the audio-graph node. Owns the audio path; fundsp
  clones and routes it.
- [`handles::PluginHandle`] — the main-thread control surface. Editor,
  parameters, state. Cheap to clone (`Arc`-shared).

Both share the subprocess lifetime — the plugin dies only when the last of either
drops.

## Design principles

The rules this crate obeys; new formats and per-block inputs should follow them.

1. **Define the functionality the host supports, then score each format against
   it.** [`Features`] is a fixed list of those capabilities; each format either
   supports a row or doesn't (see [the capability table](#per-format-capability-table)).
   Don't instead collect everything the formats emit into a neutral superset —
   that leaks format names into shared types and grows a special case per format.
2. **Capabilities are data, not types.** Abilities ride as a [`Features`] bitset
   and gate sends by flag — never by matching the format, never a per-capability
   trait — because a loaded plugin is `Box<dyn PluginInstance>` across IPC and
   cannot be downcast. (Mirrors cpal / wgpu-hal: one fat trait plus runtime
   capability queries, not a trait per capability.)
3. **Share the slot, not the value.** Host-installed per-block sources (MIDI,
   harmony, transport, automation) live in a shared `Arc<ArcSwapOption<…>>`, not
   a per-clone `Option`, because fundsp runs a different clone than the setter
   mutates — a per-clone field is a silent no-op that never reaches the audio
   thread. See [`handles::PluginClient`] and the `input_slot` module.
4. **Unify by mechanism, separate by trigger.** Collapse same-mechanism code (the
   per-block producers became one `InputSlot`); keep systems that react to
   different `Changed<T>` triggers separate — merging them would couple unrelated
   edits and do needless work per frame.
5. **The plugin lifecycle stays inside the format crate.** Each format's state
   machine is modelled in that format's own vocabulary and never crosses the IPC
   boundary — see [below](#the-plugin-state-machine).

## The plugin state machine

The shared vocabulary describes a plugin as a set of *capabilities* —
`PluginMeta`, `PluginAudio`, `PluginParams` and their siblings in
`tutti_plugin_types::format_host` — and deliberately says nothing about what state
a plugin is in. That crate's docs argue the case; the consequence for this one is
that a lifecycle never reaches it. A probe never activates at all.

What that buys is room. No format crate has to meet another in the middle, so each
one models its own lifecycle as tightly as its own contract allows — and left to
do that, the four land on three different answers. They are worth reading
together, because the differences are not stylistic: each is the format's own rule
showing through, and the same reasoning decides the shape of any format added
later.

All four start from the same two states. A plugin is *loaded* — library mapped,
instance created, parameters and editor reachable — and later *activated*, which
allocates buffers at a fixed sample rate and block size and makes `process`
legal. Everything below is disagreement about what to do with that shape.

### Which model each format gets, and why

Three modelling strategies are in use, and the choice is forced by the format's
own contract rather than picked for consistency.

**Consuming type-state — VST3 and CLAP.** `Vst3Loaded → Vst3Instance<T>` and
`ClapLoaded → ClapActive<T>`. Both formats define a large, fully legal
pre-activation surface: the parameter tree, units and program lists, note
expression, state save/restore and the editor are all reachable before any audio
buffer exists, and a host is *expected* to read them there. "Loaded but not
processing" is a state a user spends real time in, so it earns a type of its own.
The transitions take `self` by value and hand back the other type
(`activate(self) -> Result<Active>`, `deactivate(self) -> Loaded`), which is what
makes a stale handle to a deactivated plugin unrepresentable rather than merely
discouraged — the compiler rejects it, and no runtime `is_active` check is needed
on the process path. The `T` parameter fixes the sample width at the same moment,
because both formats commit to a sample format in the same call that allocates
the buffers.

Two details differ, and each is the format's rule showing through. VST3 chooses
its `ProcessMode` on the transition rather than on the instance, because
`setupProcessing` delivers it exactly once per activation. CLAP's `activate`
returns `Err((Self, ClapError))` — the *unconsumed* `ClapLoaded` comes back on
refusal, so a plugin that declines 64-bit audio can be retried at `f32` without
being reloaded.

**One fused type — VST2.** `Vst2Instance` has no split, because VST2 has no
meaningful state to split off: `effOpen`, `effSetSampleRate`, `effSetBlockSize`
and the first `effMainsChanged(1)` all run during construction, and the instance
is ready to process the moment it exists. A `Vst2Loaded` type would carry no
operations the fused type does not, so the split would buy a type boundary that
guards nothing. Suspend and resume remain as ordinary `&mut self` methods with a
`resumed: bool`, because in VST2 they are a *reconfiguration bracket* — the thing
you do around a sample rate change — and not a lifecycle stage a host parks in.

**Internal state enum — AU.** `AuInstance` carries a private `Loaded` / `Ready`
state as data, because both AU transitions are fallible in *both* directions and a
failed transition belongs to neither type. `tutti-au-host`'s own documentation
carries the argument, since that is where the type lives.

### Choosing a shape for a fifth format

Read together, the three answers reduce to one question asked twice. Do the two
states have genuinely different operations, and can the transition between them
fail in a way that belongs to neither? Two distinct surfaces and a transition that
always lands somewhere is the case a consuming type-state was made for. A
pre-activation state with nothing of its own to do should be fused, because the
extra type guards nothing. A transition that can fail in both directions has to
carry its state as data and pay for the check at runtime, because there is no
third type to return.

The failure worth naming is the first one: reaching for a compile-time guarantee
the underlying contract cannot honour. That is how a type ends up confidently
describing a state the plugin is not actually in, which is worse than the runtime
check it replaced.

## Capability model

### Functionality the host supports

Every host-side capability is one of four kinds:

- **Required** — the plugin must satisfy it or the load is refused. Plain trait
  methods, no flag: **audio (f32)**, **parameter get/set + enumerate**, **state
  save/restore**. All four external formats provide these, so requiring them
  excludes nothing today while protecting the save/load guarantee.
- **Negotiated** — the plugin answers once at load; the host adapts. A `Features`
  flag: **f64 audio**, **MIDI in/out**, **editor**, **editor resize**. (Bus widths
  and latency are the numeric half — plain fields on `LoadedPlugin`, not flags.)
- **Best-effort** — sent per block only to plugins that advertise the flag, gated
  on `Features::CONSUMES` and never on format: **transport**, **parameter
  automation**, **note expression**, **sequencer context**.
- **Reaction** — an edge-triggered host→plugin call the flag gates, *not* a
  per-block feed, so these stay out of `Features::CONSUMES`: **automation
  state**, **preset list**, **preset load**.

### Per-format capability table

What each format supports, as reported by its loader in
`tutti-plugin-server/src/loaders/`. Where a format exposes a query the flag is
live-probed at load (VST3 `IProcessContextRequirements`, CLAP note ports);
otherwise the loader sets an honest blanket. Keep this table in sync with those
loaders, not with a spec.

`●` full · `◐` conditional / probed / advisory · `○` not implemented by our loader · `✕` the format can't (by spec)

| Capability | Kind | VST3 | CLAP | AU | VST2 |
|---|---|:--:|:--:|:--:|:--:|
| Audio (f32) | Required | ● | ● | ● | ● |
| Params get/set + enumerate | Required | ● | ● | ● | ● |
| State save/restore | Required | ● | ● | ● | ● |
| `F64_AUDIO` | Negotiated | ◐ | ◐ | ○ | ◐ |
| `MIDI_IN` | Negotiated | ◐ | ◐ | ◐ | ◐ |
| `MIDI_OUT` | Negotiated | ◐ | ◐ | ○ | ◐ |
| `EDITOR` | Negotiated | ● | ● | ● | ● |
| `EDITOR_RESIZE` | Negotiated | ◐ | ◐ | ○ | ○ |
| `TRANSPORT` | Best-effort | ◐ | ● | ○ | ● |
| `PARAM_AUTOMATION` | Best-effort | ● | ● | ○ | ○ |
| `NOTE_EXPRESSION` | Best-effort | ◐ | ◐ | ✕ | ✕ |
| `SEQUENCER_CONTEXT` | Best-effort | ◐ | ✕ | ✕ | ✕ |
| `PRESET_LIST` | Reaction | ✕ | ○ | ◐ | ◐ |
| `PRESET_LOAD` | Reaction | ✕ | ◐ | ◐ | ◐ |

Notes: the AU loader reports `MIDI_IN`, `EDITOR` and the two preset bits — its
MIDI-output / transport / f64 paths are unimplemented (`○`), not spec-impossible.
AU's `MIDI_IN` is `◐` off the component type (`aumu` / `aumf` / `aumi` receive
MIDI, `aufx` does not), which is the same predicate the process path gates its
per-block `send_midi` on. `MIDI_OUT` is not its mirror: reading events back needs
a host callback this loader does not install, so it stays `○`. VST2's `F64_AUDIO`
is advisory (the `vst` crate is f32 internally). `SEQUENCER_CONTEXT` (chord/scale
/per-note text) is a VST3-only concept by spec.

The preset bits are the one place a `✕` means "the format solves this elsewhere"
rather than "the format cannot". A VST3 program is an ordinary parameter carrying
`kIsProgramChange`, selected through the parameter path, so there is no separate
preset mechanism for a flag to describe — reporting one would assert an API VST3
does not have. CLAP splits the two: `CLAP_EXT_PRESET_LOAD` loads from a path,
while enumeration lives in the factory-level preset-discovery extension this host
does not bind, which is why the bits are independent. AU backs both from
`kAudioUnitProperty_FactoryPresets`, and VST2 backs both from one number —
`AEffect::numPrograms`, since it has no separate "can you load" opcode and a
plugin declaring programs can always be sent `effProgramChange`. That the two move
together *for VST2* is why the bits are still independent: CLAP splits them, and
one bit could not describe both formats. Both bits are edge-triggered host→plugin
actions, so like `AUTOMATION_STATE` neither joins `Features::CONSUMES`.

**Wired for VST2 only.** `PluginHandle::presets` / `load_preset` /
`current_preset` are the cross-format surface, over an opaque `PresetId` that
carries each format's own identifier (an AU selector, a VST3 `(list, index)` pair,
a CLAP path) — a single index would work for VST2 and silently corrupt AU's sparse
numbering. The in-process VST2 path implements it. AU and CLAP report the
capability but their calls do not cross the IPC boundary yet, so a consumer can
read the flag and not act on it.

### `probed` — which capabilities a loader actually asked

A clear `Features` bit answers three questions the same way: the plugin declined,
this loader never asked, or the format has no query. `LoadedPlugin::probed` is the
mask that separates the first from the other two, and `LoadedPlugin::capability()`
reads the pair — `Some(false)` for a refusal, `None` for silence. The per-block
send-gate deliberately keeps reading `features` directly: it has no way to act on
"unknown" and must stay one mask-and-compare.

The masks are constants in `tutti_plugin_types::features::probed`, one per format,
so the claim is in one place rather than restated in each load path. VST2 has two
loaders (in and out of process) that read the same constant.

`probed` does **not** distinguish `○` from `✕` — both are absent from it, because
both mean no plugin spoke. Which one applies is the table above: it describes this
codebase, not the plugin, and a runtime bit would go stale the moment a loader
grows the missing path. So the AU row's eight `○`s and `NOTE_EXPRESSION`'s `✕` are
all simply unset in `probed::AU`.

Keep this section, the table above, and the constants in step — a loader that
grows a probe changes all three.

## Module map

- [`catalog`] — discovering, persisting, and loading plugins (incl. the
  [`PluginDescriptor`][catalog::PluginDescriptor] /
  [`PluginClass`][catalog::PluginClass] identity types)
- [`handles`] — [`PluginClient`][handles::PluginClient] and
  [`PluginHandle`][handles::PluginHandle]
- [`server`] — wire contract for `tutti-plugin-server` only (the full set of
  frame types, incl. [`ParameterInfo`][server::ParameterInfo])
- [`BridgeConfig`] at the crate root for low-level bridge tuning

## Features

**Nothing is on by default.** This is a library: persistence and format support
are the embedding app's choices, so you wire up exactly what you use and the
default build pulls no `serde_json` and no format FFI.

- `json` — JSON-file-backed `catalog::JsonCatalog`. One ready-made
  [`catalog::PluginCatalog`] impl, not the shape of the API: implement the trait
  over your own store instead, or skip it entirely and use the pure
  [`catalog::discover`] /
  [`PluginRecord::probe`][catalog::PluginRecord::probe] pair.
- `vst3`, `clap`, `au` — in-process GUI support. Loads the plugin library in the
  *host* process for editor rendering only; audio still runs out-of-process.
- `vst2` — in-process VST2 hosting (audio + MIDI + parameters + state + native
  editor). Unlike VST3/CLAP/AU, VST2 is always in-process: its `AEffect` fuses
  the editor and audio processor into one instance, so the two cannot live in
  separate processes.

This crate hosts the four industry plugin formats. A host that defines its *own*
format can still reuse the control surface and audio-node wiring here by
implementing the [`backend`] traits over its own loader.

## License

MIT OR Apache-2.0

[`AudioUnit`]: tutti_core::AudioUnit
[`Features`]: crate::Features
