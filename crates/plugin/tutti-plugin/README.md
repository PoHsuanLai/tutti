# tutti-plugin

Audio-plugin hosting for Tutti: VST3, CLAP and AU plugins in isolated
subprocesses, VST2 in-process, each exposed as a node of Tutti's audio graph.

## What this is

Use this crate to load third-party plugins and run them inside a
[`tutti_graph`] graph. Every VST3, CLAP and AU plugin runs in its own
`plugin-server` subprocess, bridged over shared memory for audio and a local
socket for control, so a plugin that crashes takes down only its subprocess:
the host keeps running and reports the error. VST2 plugins run in the host
process behind the `vst2` feature.

The crate also covers discovery (scanning plugin directories, a persistent
catalog with incremental rescan and crash blacklisting) and the main-thread
control surface (parameters, state, presets, editor windows).

`bevy-tutti` builds its plugin-hosting feature (`plugin`) on this crate.

## Quick start

Load a plugin and put its node into a tutti graph. `no_run`: the load spawns a
subprocess against a real `.vst3` / `.clap` on disk.

```rust,no_run
use tutti_core::SampleRate;
use tutti_graph::{Editor, Prepare};
use tutti_plugin::catalog::Plugin;
use tutti_types::{NodeKey, Samples};

// `sample_rate` takes anything convertible to `SampleRate` — the engine's
// unit type, not a bare rate that could be a block size.
let plugin = Plugin::open("/usr/lib/vst3/MyPlugin.vst3", SampleRate::new(48_000.0))?;
println!("{} by {}", plugin.descriptor().name, plugin.descriptor().vendor);

// Two handles, one subprocess: keep the control surface before the node goes
// into the graph — the plugin dies when the last of either drops.
let handle = plugin.handle().clone();
println!("reported latency: {:?}", handle.loaded().latency());

// A `Plugin` is an `IntoNode`: inserting it hands back its controls (a
// subprocess plugin's `PluginControls`), and a fork source so an export can
// render it.
let (mut editor, _executor) = Editor::new(Prepare::new(SampleRate::new(48_000.0), Samples(512)));
let _controls = editor.insert(NodeKey(1), "plugin", plugin);
# Ok::<(), tutti_plugin::BridgeError>(())
```

A host that does not already know the path discovers one first — see
[`catalog`], whose two pure functions ([`discover`](catalog::discover) and
[`PluginRecord::probe`](catalog::PluginRecord::probe)) need no feature flag, and
whose [`Plugins`](catalog::Plugins) adds incremental rescan and crash recovery on
top.

## Architecture

Every out-of-process plugin runs in its own `plugin-server` subprocess (the
[`tutti-plugin-server`](https://docs.rs/tutti-plugin-server) crate). The host
talks to it over two channels:

- **Control** (Unix socket / named pipe) — editor open/close, parameter reads,
  state save/restore. Main-thread, blocking.
- **Audio** (shared-memory slab) — audio, MIDI and parameter changes per block.
  Audio-thread, lock-free and allocation-free.

The host looks for the `plugin-server` binary at the path in the
`TUTTI_PLUGIN_SERVER` environment variable, then next to the application
executable, then in its parent directory, then on `PATH`.

### Two handles per plugin

A loaded [`Plugin`](catalog::Plugin) yields two values that share the plugin's
lifetime — it dies only when the last of either drops:

- [`handles::PluginClient`] — the audio-graph node. Loaded `Unbound`;
  `bind()` makes it a `PluginClient<Bound>`, which owns the audio path and is
  what a graph inserts. `Plugin` does this for you when inserted directly.
- [`handles::PluginHandle`] — the main-thread control surface: editor,
  parameters, state, presets. Cheap to clone (`Arc`-shared).

Per-block inputs (MIDI, chord/scale context, parameter automation) are gated
on what the plugin reported at load: an accessor answers `None` or `false`
when the plugin declined the input. Capabilities are data — a [`Features`]
bitset on [`LoadedPlugin`](server::LoadedPlugin) — never a check of the
plugin's format.

## Main types

- [`catalog`] — discovering, persisting and loading plugins:
  [`Plugin`](catalog::Plugin) (one loaded plugin),
  [`Plugins`](catalog::Plugins) (catalog with incremental rescan and crash
  recovery), [`discover`](catalog::discover) /
  [`PluginRecord::probe`](catalog::PluginRecord::probe) (the stateless pair),
  and the [`PluginDescriptor`](catalog::PluginDescriptor) /
  [`PluginClass`](catalog::PluginClass) identity types.
- [`handles`] — [`PluginClient`](handles::PluginClient) (the graph node),
  [`PluginHandle`](handles::PluginHandle) (the control surface),
  [`PluginControls`](handles::PluginControls) and parameter automation.
- [`BridgeConfig`] — low-level bridge tuning (buffer size, timeouts).
- [`BridgeError`] — the error most calls return.
- [`backend`] — traits for plugging an out-of-crate loader into
  [`PluginHandle`](handles::PluginHandle).
- [`server`] — the wire contract shared with `tutti-plugin-server`; not for
  application code.

## Features

**Nothing is on by default.** Persistence and format support are the embedding
app's choices, so the default build pulls no `serde_json` and no format FFI.
Without any feature, VST3, CLAP, AU and VST2 plugins all load in the
subprocess, with no editor support.

- `json` — `catalog::JsonCatalog`, a JSON-file-backed
  [`catalog::PluginCatalog`]. One ready-made store, not the shape of the API:
  implement the trait over your own store instead, or use the pure
  [`catalog::discover`] / [`PluginRecord::probe`][catalog::PluginRecord::probe]
  pair.
- `vst3`, `clap`, `au` — editor support for that format. Loads the plugin
  library a second time in the *host* process for editor rendering only; audio
  still runs out of process. The editor instance is therefore not isolated: a
  crash in it takes the host down.
- `vst2` — in-process VST2 hosting (audio, MIDI, parameters, state, presets and
  native editor). VST2's `AEffect` fuses the editor and audio processor into one
  instance, so the two cannot live in separate processes; with this feature,
  [`Plugin::open`](catalog::Plugin::open) hosts `.vst` plugins in the host
  process.

## Capability model

### Functionality the host supports

Every host-side capability is one of four kinds:

- **Required** — the plugin must satisfy it or the load is refused. Plain trait
  methods, no flag: **audio (f32)**, **parameter get/set + enumerate**, **state
  save/restore**. All four formats provide these, so requiring them excludes
  nothing while protecting the save/load guarantee.
- **Negotiated** — the plugin answers once at load; the host adapts. A `Features`
  flag: **f64 audio**, **MIDI in/out**, **editor**, **editor resize**, **floating
  editor**. (Bus widths and latency are the numeric half — plain fields on
  `LoadedPlugin`, not flags.)
- **Best-effort** — sent per block only to plugins that advertise the flag, gated
  on `Features::CONSUMES` and never on format: **transport**, **parameter
  automation**, **note expression**, **sequencer context**.
- **Reaction** — an edge-triggered host→plugin call the flag gates, *not* a
  per-block feed, so these stay out of `Features::CONSUMES`: **automation
  state**, **preset list**, **preset load**, **render mode**.

### Per-format capability table

What each format's loader supports. Where a format exposes a query the flag is
live-probed at load (VST3 `IProcessContextRequirements`, CLAP note ports);
otherwise the loader sets a fixed answer.

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
| `PRESET_LIST` | Reaction | ◐ | ○ | ◐ | ◐ |
| `PRESET_LOAD` | Reaction | ◐ | ◐ | ◐ | ◐ |
| `RENDER_MODE` | Reaction | ● | ◐ | ◐ | ● |
| `EDITOR_FLOATING` | Negotiated | ✕ | ◐ | ✕ | ✕ |

Notes: the AU loader reports `MIDI_IN`, `EDITOR`, `RENDER_MODE` and the two
preset bits; its MIDI-output, transport and f64 paths are unimplemented (`○`),
not spec-impossible. AU's `MIDI_IN` is `◐` off the component type (`aumu` /
`aumf` / `aumi` receive MIDI, `aufx` does not), which is the same predicate the
process path gates its per-block MIDI on. `MIDI_OUT` is not its mirror: reading
events back needs a host callback this loader does not install, so it stays
`○`. VST2's `F64_AUDIO` is advisory (the `vst` crate is f32 internally).
`SEQUENCER_CONTEXT` (chord/scale/per-note text) is a VST3-only concept by spec.

The preset bits are two because the formats split there. A VST3 plugin answers
both from its program lists: loading one writes the parameter flagged
`kIsProgramChange` that owns the list, which the loader does for you. CLAP
splits the two: `CLAP_EXT_PRESET_LOAD` loads from a path, while enumeration
lives in the factory-level preset-discovery extension this host does not bind.
AU backs both from `kAudioUnitProperty_FactoryPresets`, and VST2 from one
number, `AEffect::numPrograms`. Like `AUTOMATION_STATE` and `RENDER_MODE`, the
preset bits are edge-triggered host→plugin actions, so none of them joins
`Features::CONSUMES`.

[`PluginHandle::presets`](handles::PluginHandle::presets) is the cross-format
surface, over an opaque [`PresetId`] that carries each format's own identifier
(an AU selector, a VST3 `(list, index)` pair, a CLAP path, a VST2 index) — a
single index would silently corrupt AU's sparse numbering.
[`PluginHandle::preset_support`](handles::PluginHandle::preset_support)
collapses the two bits into what a preset UI can offer.

### `probed` — which capabilities a loader actually asked

A clear `Features` bit answers three questions the same way: the plugin declined,
this loader never asked, or the format has no query. `LoadedPlugin::probed` is the
mask that separates the first from the other two, and `LoadedPlugin::capability()`
reads the pair — `Some(false)` for a refusal, `None` for silence. The per-block
send-gate deliberately keeps reading `features` directly: it has no way to act on
"unknown" and must stay one mask-and-compare.

The masks are constants in [`server::probed`], one per format. VST2 has two
loaders (in and out of process) that read the same constant.

`probed` does **not** distinguish `○` from `✕` — both are absent from it, because
both mean no plugin spoke. Which one applies is the table above: it describes this
codebase, not the plugin, and a runtime bit would go stale the moment a loader
grows the missing path. So the AU row's `○`s and `✕`s are all simply unset in
`probed::AU`.

## The plugin state machine

This section is for readers of the format host crates. The shared vocabulary
describes a plugin as a set of *capabilities* — [`server::PluginMeta`],
[`server::PluginAudio`], [`server::PluginParams`] and their siblings — and
deliberately says nothing about what state a plugin is in, so a lifecycle never
reaches this crate. A probe never activates at all.

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

## Related crates

- [`tutti-plugin-types`](https://docs.rs/tutti-plugin-types) — the shared value
  vocabulary ([`ParameterInfo`](server::ParameterInfo),
  [`TransportInfo`](server::TransportInfo), [`Features`], [`Preset`], …). The
  types a caller cannot avoid naming are re-exported at this crate's root.
- [`tutti-plugin-server`](https://docs.rs/tutti-plugin-server) — the
  subprocess side of the bridge.
- `tutti-vst3-host`, `tutti-clap-host`, `tutti-au-host`, `tutti-vst2-host` —
  the format loaders. This crate never sees one directly: a plugin arrives
  already loaded and activated inside the subprocess.
- [`tutti_graph`] — the audio graph a plugin node is inserted into.

## License

MIT OR Apache-2.0

[`Features`]: crate::Features
[`PresetId`]: crate::PresetId
[`Preset`]: crate::Preset
