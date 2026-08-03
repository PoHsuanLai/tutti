# Tutti Plugin

VST2, VST3, CLAP, and AU plugin hosting.

## What this is

Loads audio plugins in separate server processes. Each plugin runs in its own process, so crashes don't affect the main application. Audio buffers are passed via shared memory (mmap).

Uses [vst](https://crates.io/crates/vst) for VST2, raw C pointers for VST3, and [clap-sys](https://crates.io/crates/clap-sys) for CLAP.

## Quick Start

```rust
use tutti_plugin::{PluginClient, BridgeConfig};

// Start plugin server process
let mut client = PluginClient::new(BridgeConfig::default())?;
client.init().await?;

// Load plugin
client.load_plugin("/path/to/plugin.vst3", 44100.0).await?;

// Process audio
let buffer = AudioBuffer { /* ... */ };
client.process(&mut buffer);
```

## How it works

Client-server architecture with IPC. Audio buffers transferred via shared memory. Supports both f32 and f64 sample formats. MIDI events include frame offsets for sample-accurate timing. Per-block inputs a plugin can consume (transport, parameter automation, chord/scale context) are sent only when the plugin advertised wanting them.

## Design principles

The rules the code here obeys. New formats and new per-block inputs should follow them; a change that violates one is a smell worth a second look.

1. **Define the functionality we support, then score each format against it.** The interface is a fixed list of capabilities we handle; each format either supports a row or doesn't (see the capability table below). Don't instead collect everything VST3/CLAP/AU/VST2 can emit into a neutral superset. *Because* a format-shaped superset leaks format names into the shared vocabulary and grows one special case per format.

2. **Capabilities are data, not types.** A plugin's abilities ride as a `Features` bitset (on `LoadedPlugin`), and the engine gates each per-block send on a flag — never by matching on the format, never via a per-capability trait. *Because* a loaded plugin is a `Box<dyn PluginInstance>` across a process boundary; you cannot downcast across IPC, so capability facts must be data on the wire, not the type system. (Mirrors cpal / wgpu-hal: one fat trait + runtime capability queries, not a trait per capability.)

3. **Share the slot, not the value.** Anything host-side installed onto a running audio node — a MIDI clip source, harmony, transport, parameter automation — lives in a shared cell (`Arc<ArcSwapOption<…>>`), not a per-clone `Option<Arc<…>>`. *Because* fundsp's frontend/backend split runs a *different* clone than the one your setter mutates, and `Net::migrate` discards `node_mut`/clone edits on commit — so a per-clone field is a silent no-op that never reaches the audio thread. (This bit us across four producers at once; see `input_slot.rs`.)

4. **Unify by mechanism, separate by trigger.** Collapse code that is the same *mechanism* (the per-block producer slots all became one `InputSlot<B>`). Keep code separate when it reacts to a different *reactive trigger* (the harmony / param-automation / MIDI-clip install systems each fire on their own `Changed<T>` and stay separate). *Because* duplication inside one reactive scope is real and hides bugs, but merging two systems that watch different change-sets would couple unrelated edits and do needless work per frame. What looks like duplicated shape across two systems is usually their differing triggers showing through — not a smell.

## Capability model

### Functionality we support

Every host-side capability is one of three kinds:

- **Required** — the plugin must satisfy it or we refuse to load. Plain trait methods, no flag: **audio (f32)**, **parameter get/set + enumerate**, **state save/restore**. All four external formats provide these, so requiring them excludes nothing today while protecting the save/load guarantee.
- **Negotiated** — the plugin answers once at load; the host adapts. A `Features` flag: **f64 audio**, **MIDI in/out**, **editor**, **editor resize**. (Bus widths + latency are the numeric `Limits` half — plain fields on `LoadedPlugin`, not flags.)
- **Best-effort** — sent per block only to plugins that advertise the flag, gated on `Features::CONSUMES`, never on format: **transport**, **parameter automation**, **note expression**, **sequencer context**.
- **Reaction** — an edge-triggered host→plugin call the flag gates, *not* a per-block feed, so these stay out of `Features::CONSUMES`: **automation state**, **preset list**, **preset load**.

### Per-format capability table

What each format supports, as reported by its loader in `tutti-plugin-server/src/loaders/`. Where a format exposes a query the flag is live-probed at load (VST3 `IProcessContextRequirements`, CLAP note ports); otherwise the loader sets an honest blanket. Keep this table in sync with those loaders, not with a spec.

`●` full · `◐` conditional / probed / advisory · `○` not implemented by our loader · `✕` the format can't (by spec)

| Capability | Kind | VST3 | CLAP | AU | VST2 |
|---|---|:--:|:--:|:--:|:--:|
| Audio (f32) | Required | ● | ● | ● | ● |
| Params get/set + enumerate | Required | ● | ● | ● | ● |
| State save/restore | Required | ● | ● | ● | ● |
| `F64_AUDIO` | Negotiated | ◐ | ◐ | ○ | ◐ |
| `MIDI_IN` | Negotiated | ◐ | ◐ | ○ | ◐ |
| `MIDI_OUT` | Negotiated | ◐ | ◐ | ○ | ◐ |
| `EDITOR` | Negotiated | ● | ● | ● | ● |
| `EDITOR_RESIZE` | Negotiated | ◐ | ◐ | ○ | ○ |
| `TRANSPORT` | Best-effort | ◐ | ● | ○ | ● |
| `PARAM_AUTOMATION` | Best-effort | ● | ● | ○ | ○ |
| `NOTE_EXPRESSION` | Best-effort | ◐ | ◐ | ✕ | ✕ |
| `SEQUENCER_CONTEXT` | Best-effort | ◐ | ✕ | ✕ | ✕ |
| `PRESET_LIST` | Reaction | ✕ | ○ | ◐ | ○ |
| `PRESET_LOAD` | Reaction | ✕ | ◐ | ◐ | ○ |

Notes: the AU loader reports `EDITOR` plus the two preset bits — its MIDI / transport / f64 paths are unimplemented (`○`), not spec-impossible. VST2's `F64_AUDIO` is advisory (the `vst` crate is f32 internally). `SEQUENCER_CONTEXT` (chord/scale/per-note text) is a VST3-only concept by spec.

The preset bits are the one place a `✕` means "the format solves this elsewhere" rather than "the format cannot". A VST3 program is an ordinary parameter carrying `kIsProgramChange`, selected through the parameter path, so there is no separate preset mechanism for a flag to describe — reporting one would assert an API VST3 does not have. CLAP splits the two: `CLAP_EXT_PRESET_LOAD` loads from a path, while enumeration lives in the factory-level preset-discovery extension this host does not bind, which is why the bits are independent. AU backs both from `kAudioUnitProperty_FactoryPresets`. Both bits are edge-triggered host→plugin actions, so like `AUTOMATION_STATE` neither joins `Features::CONSUMES`.

**Reported, not yet wired.** These flags describe what a loaded plugin can do; the AU and CLAP preset calls do not cross the IPC boundary yet, so a consumer can read the capability but not act on it. Each needs its own format-specific wire message — they share no address space (an AU selector, a CLAP path), so there is no one preset call to add.

### `probed` — which capabilities a loader actually asked

A clear `Features` bit answers three questions the same way: the plugin declined, this loader never asked, or the format has no query. `LoadedPlugin::probed` is the mask that separates the first from the other two, and `LoadedPlugin::capability()` reads the pair — `Some(false)` for a refusal, `None` for silence. The per-block send-gate deliberately keeps reading `features` directly: it has no way to act on "unknown" and must stay one mask-and-compare.

The masks are constants in `tutti_plugin_types::features::probed`, one per format, so the claim is in one place rather than restated in each load path. VST2 has two loaders (in and out of process) that read the same constant.

`probed` does **not** distinguish `○` from `✕` — both are absent from it, because both mean no plugin spoke. Which one applies is the table above: it describes this codebase, not the plugin, and a runtime bit would go stale the moment a loader grows the missing path. So the AU row's nine `○`s and `NOTE_EXPRESSION`'s `✕` are all simply unset in `probed::AU`.

Keep this section, the table above, and the constants in step — a loader that grows a probe changes all three.

## License

MIT OR Apache-2.0
