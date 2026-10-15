# 015: a Bevy-free tutti API layer

Status: **proposed**. Doc only; nothing here has landed.

## Summary

The user's ask, verbatim:

> "i actually want a pure tutti api layer with an umbrella crate. do we have
> that too? if not queue this item after everything / this should be for all
> the tutti crates"

**We have the umbrella, but not the API layer.** `crates/tutti` re-exports
every engine crate. A headless host still hand-wires a dozen of them:

- the `Editor`, its `Executor`, the `Engine` and the transport;
- the device, `AudioCallbackState` and `TuttiDriver::from_parts`;
- `Modulation::apply` with a `ModHost` of its own;
- PDC publication, plugin loading, MIDI clips and export.

The examples show the cost. `crates/tutti/examples/headless_engine.rs` takes
six steps to make a sound, and
`crates/dsp/tutti-nodes/examples/headless_modulation.rs` takes 250 lines to
modulate one filter. The code that does all of this correctly already
exists, inside `bevy-tutti`. About 30% of `bevy-tutti`'s non-test source is
engine logic that only happens to live there (§2).

**Recommendation: one new logic crate, `tutti-session`, with the
single-owner pieces moved into their owning crates first (option a, fed by
option c).**

- `tutti-session` holds the cross-crate composition that no lower crate can
  own without inverting an edge: the graph handle, the bootstrap, PDC
  publication, the modulation protocol, the plugin lifecycle, MIDI wiring and
  the export job.
- It has no `bevy` feature and links no Bevy crate, and a gate checks both.
- `bevy-tutti` depends on it and shrinks to an adapter: ECS components,
  systems, assets and windows.
- `tutti` stays `pub use`-only and re-exports it as `tutti::session`, with
  `Session` in the prelude.
- So neither umbrella owns logic, and both sit on the same crate. The old
  failure was two umbrellas stacked so that the upper needed the lower's
  logic; this layout cannot form that stack (§3).

The plan is seven PRs (§9), and `bevy-tutti` stays green after each one.
§11 lists the four questions only the user can answer.

## 1. The constraint

The failure we must not repeat is recorded in `docs/engineering-notes.md`,
in "There are two umbrellas, and `tutti` holds no code":

- The root package at `9c75ec54` was a `tutti` holding `TuttiEngine`, the
  builder, the driver and an error aggregate.
- `0a4adf68` moved that logic into `bevy-tutti/src/engine/`, and `4b5bd2fd`
  deleted the package.
- **The problem was two stacked umbrellas where the lower one owned logic
  the upper one needed**. Re-exporting was never the problem.
- `crates/tutti/tests/no_logic.rs` keeps it from returning. The gate
  `scripts/check-canonical-paths.sh` makes `tutti::tutti_x` unspellable.

The user wants something that looks like the deleted package: one API, one
umbrella, every crate. Any design must answer two questions:

1. Which crate owns the logic?
2. Can either umbrella ever need logic the other owns?

## 2. Inventory: what a headless host hand-rolls today

The survey covers `crates/bevy-tutti/src/`: about 15,800 non-test lines,
doc comments included, counted up to each file's `#[cfg(test)]`.

Each piece below is one of two things:

- **glue**: ECS components, systems, resources, events, assets or windows;
- **logic**: engine behaviour that would be the same in any host.

Logic moves. A row marked **(c)** goes to a single owning crate. The rest
goes to `tutti-session`.

### 2.1 `engine/`

| Piece | Where | Kind | Destination |
|---|---|---|---|
| Ordered bootstrap (detail below) | `engine/build.rs:73-246` | logic | session builder |
| `assemble`: engine, callback state, driver | `engine/build.rs:269-299` | logic | session builder |
| `root_width` policy (plugin floor vs device width) | `engine/build.rs:326-330` | logic | session builder |
| Resource insertion, click/MIDI entities | inside `build_on` | glue | stays |
| `build_into` opening the device | `engine/build.rs:60-67` | logic | session builder |
| `rerate` (detail below) | `engine/restart.rs:169-241` | logic | `Session::restart` |
| `restart_device{,_on}`: the World/NonSend driver dance | `engine/restart.rs:98-160` | glue | stays, shrinks to a call |
| `AudioEngineState`, `AudioDeviceState` mirrors | `engine/state.rs`, `engine/device_state.rs` | glue (the fault polling is logic) | the polling moves |
| `TuttiPlugin` config | `plugin.rs` | glue over options | becomes `SessionBuilder` options |
| Aggregate `Error` | `error.rs` | logic | `tutti_session::Error` |

The ordered bootstrap in `build_on`, step by step:

1. MIDI hardware ports, then their rate.
2. The root width.
3. The transport, `MasterMeter`, `AudioTap`, `ClickSettings`.
4. One `ChannelCompensation` shared with the `DiskStreamer`'s PDC.
5. The graph with its workers, and an unwired click.
6. `Engine::new`.
7. The MIDI nodes, with the clock fed into hardware out.
8. Start the device and the streamer, then `TuttiDriver::from_parts`.
9. Enable the meter and close the tap.

`rerate`, step by step:

1. `graph.rerate`, then `transport.set_sample_rate`.
2. Settle, collect and check.
3. Widen the root, commit and settle again.
4. Publish compensation.
5. Set the MIDI port rate and the streamer rate.
6. Update `AudioConfig`.

### 2.2 `graph/`

| Piece | Where | Kind | Destination |
|---|---|---|---|
| `GraphRuntime` (whole type, see below) | `graph/runtime.rs:106-954` | logic | `tutti_session::Graph` |
| `ModHostOver`: target and feed resolution | `graph/runtime.rs:985-1023` | logic over an `Entity` key | session, keyed by a resolver trait (§4.6) |
| `AudioSide` renderer, `PendingExport` | `graph/runtime.rs:1028-1142` | logic | session |
| `AudioGraphRes(Mutex<GraphRuntime>)`, `AudioConfig`, `GraphSource` | `graph/resources.rs` | glue (the `Mutex` is only for `Sync`) | stays, wraps `Session` |
| `commit_graph`, `commit_and_publish` | `graph/commit.rs:29-80` | logic | `Session::commit` |
| `ChannelCompensation`, `GraphLatency`, `publish`, `publish_sent` | `graph/latency.rs:110-168` | logic | session |
| `LatencyCompensationPlugin`, the debug check | `graph/latency.rs:170-226` | glue | stays |
| Topology from `PortSources`/`MasterSources`, diff-apply, `Changed<AudioNode>` | `graph/topology.rs`, `graph/wire.rs` | glue (the Entity layer) | stays |
| The root-widen policy inside the rebuild | `graph/wire.rs` | logic | `Graph::widen_outputs` (already there) |
| `GraphReconcileSystems`, `GraphDirty` | `graph/schedule.rs` | glue | stays, built from `Session::PHASES` (§4.4) |
| `spawn_audio_node` and friends | `graph/spawn.rs:114-212` | glue | stays |
| Crossfade retry queue, 5 ms equal-amplitude fade | `graph/spawn.rs:327-429` (fade at `:367`) | logic | `Graph::crossfade` + session retry |
| `CapturedControls` (plugin controls, meter) | `graph/capture.rs:57-151` | logic, bound to an entity | the capture moves; `bind` stays |
| `EventFeeds` (named feeders merged per sink) | `graph/events.rs:80-110` | logic | session, keyed by `AudioNode` |
| `GraphNode` trait, `param_graph_node!` list | `graph/events.rs:112-280` | logic (which controls are a `ParamSet`) | `tutti_session::SessionNode` |
| Event `reconcile` | `graph/events.rs:292-347` | glue | stays |
| `AudioParam<U, P>` | `graph/param.rs` | glue | stays |
| `AudioPump` thread loop | `graph/pump.rs:135-240` | logic | **(c)** `tutti-io` |
| Pump drain/finalize systems | `graph/pump.rs:266-383` | glue | stays |
| Transport, tap and metering resource wrappers | `graph/transport.rs`, `tap.rs`, `metering.rs` | glue | stays |

`GraphRuntime` provides, all Bevy-free but for `bevy_log`:

- `insert`, `insert_plugin`, `replace_plugin`, `replace` (with
  `ReplaceRefused`) and `remove`;
- `set_event_sources`, which keeps other owners' edges;
- `set_source`, `set_output_source`, `set_outputs_from`, `widen_outputs`;
- `set_param_mod`, `clear_param_mod`, `param_mod`;
- `latency_plan`, `planned_compensation`, `sent_compensation`,
  `refresh_node_latency`;
- `commit` returning `Done`/`Retry`, `reprepare`, `check_reprepare`;
- `render_pool`, `fork`, `gather_for_export`, `render_frame_at`.

### 2.3 `modulation/`

| Piece | Where | Kind | Destination |
|---|---|---|---|
| `collect` (detail below) | `modulation/reconcile.rs:204-292` | logic | session |
| `apply` / `after_commit` protocol (detail below) | `modulation/reconcile.rs:325-444` | logic | `Session` update phases |
| Unresolved-target warning after 240 updates | `modulation/reconcile.rs:54` | logic | session |
| `ModulationState` | `modulation/reconcile.rs:39-105` | logic, keyed by `Entity` | session, keyed by `SourceId`/`AudioNode` |
| `ModulationRefused` → route/target entity | `modulation/reconcile.rs:109-185` | glue | stays |
| `Declared` query, `ModEntities` | `modulation/reconcile.rs:186-324` | glue | stays, implements the resolver |
| `Lfo`, `Macro`, `Follower`, `NoteEnvelope` → `tutti_mod::Source` | `modulation/declare.rs:142-330` | logic | **(c)** `tutti-mod` |
| `AutomationTake`, `close_takes` | `modulation/declare.rs:446-530` | logic | session (`tutti-mod`'s `Recorder` plus the close rule) |
| `ModSource`/`ModRoute`/`ModRange`/`Automation` components, `Mods` | `modulation/declare.rs` | glue | stays |

`collect` checks ranges and automation for `DuplicateTarget`, and gives a
range-less automation its default of ±inf.

The `apply` / `after_commit` protocol:

1. Apply.
2. Commit.
3. While targets are unresolved, re-apply and commit again.
4. Send the resyncs.

### 2.4 `plugin_host/`

| Piece | Where | Kind | Destination |
|---|---|---|---|
| Load (detail below) | `plugin_host/load.rs:225-375` | logic | `PluginLoad` job + `Session::add_plugin` |
| At most 2 concurrent loads | `plugin_host/load.rs:72` | logic | session |
| The ECS request/observer shape | `plugin_host/load.rs:115-224` | glue | stays |
| Meter cell bound from the metronome | `plugin_host/bind.rs` | logic | session |
| Latency poll: declared vs graph, `refresh_node_latency` | `plugin_host/latency.rs:73-96` | logic | session update phase |
| `PluginParamValues` mirror | `plugin_host/params.rs:48-122` | logic | **(c)** `tutti-plugin` |
| GUI edit queue → knob | `plugin_host/params.rs:135-169` | logic | session |
| Health debounce: 3 deaths, then remove; snapshot every 512 updates | `plugin_host/health.rs:67-260` | logic | **(c)** `tutti-plugin` (the counter), session (the removal) |
| Render mode: Offline during an export | `plugin_host/render_mode.rs:58-130` | logic | session export job |
| `scan.rs`, `catalog.rs` | | glue over `tutti-plugin` | stays |
| `editor.rs`, `native_window.rs`, `live_resize.rs` | | Bevy window glue | stays |

A load, step by step:

1. `Plugin::open_with`, off the control thread.
2. Restore the state blob.
3. `into_client`, else insert in-process.
4. Capture the controls and the meter.

### 2.5 `midi/`

| Piece | Where | Kind | Destination |
|---|---|---|---|
| `LiveMidi`: a queue node plus a feed | `midi/nodes.rs:93-157` | logic | session |
| Clip sequencing (detail below) | `midi/sequence.rs:101-188` | logic | `Session::play_clip` |
| Route rules → input ports, with a fallback | `midi/inbound/route.rs:143-206` | logic | **(c)** `tutti-midi-runtime` (pure compile) |
| `MidiOutRouter`, `drain_receiver_through`, `MidiOutDrops`, JR stamping | `midi/hardware/hardware_out.rs:35-300` | logic | session (it joins runtime and hardware) |
| Clock-out pump | `midi/hardware/clock_out.rs:55-80` | logic | session |
| `track_out`, `negotiation`, `metadata`, `device` | `midi/hardware/` | glue over the runtime's state machines | stays |
| `MidiFileContents` sniff and decode | `midi/file.rs:100-168` | logic | **(c)** `tutti-midi-file` |
| SMF → `(Beat, MidiEvent)` | not in the engine | missing | **(c)** `tutti-midi-file` |
| Asset loader, write tasks | `midi/file.rs:169-330` | glue | stays |

Clip sequencing keeps one `MidiClipNode` per target. It updates the events
in place with `set_events`, and clears or removes the node when the clip is
unplayable.

### 2.6 Samples, SoundFonts and export

| Piece | Where | Kind | Destination |
|---|---|---|---|
| `memory_voice`, `voice_width` | `sampler/voice.rs:149-186` | logic | **(c)** `tutti-sampler` |
| `GraphNode` impls for voices | `sampler/voice.rs:53-100` | logic | session's `SessionNode` list |
| `DiskStreamerRes`, `WaveAssetLoader` | `sampler/` | glue | stays |
| `.sf2` → `SoundFontUnit` at the graph rate, then `program_change` | `soundfont.rs:182-296` | logic | **(c)** `tutti-soundfont` |
| Asset and pending task | `soundfont.rs` | glue | stays |
| Export pipeline (detail below) | `export/run.rs:50-335` | logic | `ExportJob` |
| `ExportRequest`, observers, `poll_exports` | `export/request.rs`, `export/run.rs:336-372` | glue | stays |
| `io/mod.rs` | | re-exports | stays |

The export pipeline, step by step:

1. Keep one export in flight.
2. Gather on the control thread; build on a worker.
3. Run the prepare-hook edit, then commit.
4. Take the latency and the tail from the graph.
5. Render to a file, a normalized file or buffers.
6. Drop the leftovers on the control thread (`ToMain`, `ExportDrops`).
7. Name each error by its node.
8. `ExportClock` keeps a stopped timeline.

### 2.7 Headline

About **4,600 of 15,800 non-test lines (≈30%) move**. The figure includes
doc comments and is an estimate.

- Half of it is three pieces: `GraphRuntime` (1,142 lines, almost all of it),
  the export job (≈350) and the modulation protocol (≈650 across
  `reconcile.rs` and `declare.rs`).
- About 600 of those lines are **(c)** moves into single owning crates.
- The rest is glue, and it stays: topology/wire, spawn, the param
  components, the asset loaders, the plugin windows, the MIDI-CI device
  state, the request/observer shapes, and the schedule.

One gap is missing everywhere, not just in bevy-tutti: converting an SMF to
timed events. `ParsedMidiFile` holds midly messages; `ParsedClipFile::timed`
already yields `(Beat, MidiEvent)`.

## 3. The options

The test for each option is the §1 failure: does either umbrella end up
needing logic the other owns?

**(a) A new logic crate below both umbrellas.**

- The logic lives once, in a non-umbrella crate. `bevy-tutti` and `tutti`
  both depend on it; neither depends on the other.
- No umbrella is "lower", so the failure's shape cannot form. The crate is
  the *bottom* of the composition, never the middle of a stack.
- The risk is a second god-crate. §4.1 bounds it: it holds composition only,
  and anything with a single owner goes to that owner.

**(b) Logic in `tutti`, with `bevy-tutti` depending on `tutti`.**

- This *is* the failure. `tutti` becomes the lower umbrella owning logic
  `bevy-tutti` needs.
- It also re-creates the double-edge problem the notes record: `bevy-tutti`
  would keep direct edges for each crate's `bevy` feature, ending with two
  edges to each.
- To make it safe, `tutti` would have to stop being an umbrella. It would
  then need a second facade on top, which is option (a) with the names
  swapped.
- It loses.

**(c) Spread the API across the owning crates, with `tutti` a curated facade.**

- This is right for every piece with one owner, and §2 marks them.
- It cannot hold the composition. The bootstrap needs `tutti-cpal`,
  `tutti-midi-hardware`, `tutti-sampler`'s streamer and the click. The
  modulation protocol needs `tutti-nodes`, `tutti-mod` and the plugin
  mirror. Export needs `tutti-export` and `tutti-plugin`'s render mode.
- Putting the bootstrap in `tutti-cpal` (the notes' old suggestion,
  "device bootstrap in `tutti-cpal`") would make the device layer depend on
  the sampler, MIDI and plugins. `tutti-io` and the offline path would then
  pull a device in.
- Every candidate owner inverts an edge, or leaves the composition with the
  host, which is today's problem.
- Alone, it loses.

**(d) A logic crate per subsystem** (`tutti-live`, `tutti-plugin-session`,
and so on).

- This only moves the composition problem up one level: something must
  still own their order (§4.4) and their shared state (the
  `ChannelCompensation` table, the export's render-mode announcement).
- It loses to (a) on every count but crate size.

**Recommended: (a), fed by (c).**

- First, move every single-owner piece into its owner, each a PR of its own
  (§9).
- Then create `tutti-session` with only what is left: the pieces that touch
  two or more subsystems.

The rule that keeps it bounded is one sentence: **`tutti-session` holds
only code that names two or more engine crates, or the order they run in.**

## 4. The proposed API

### 4.1 Placement and edges

The crate is `crates/tutti-session`, beside the two umbrellas.

It belongs at the top tier, not in `core/`: it depends on the DSP, MIDI and
plugin crates, and `core/` is the floor.

| Edge | Always or feature | Why |
|---|---|---|
| `tutti-types`, `tutti-graph`, `tutti-core` | always | the graph, engine and transport |
| `tutti-nodes` | always | click, control lanes, `Modulation` |
| `tutti-cpal` | `device` | the sound card; off means offline only, with no ALSA headers |
| `tutti-io` | `audio-io` | pump, recorder, mic |
| `tutti-mod` | `modulation` | declarations |
| `tutti-midi-types`, `tutti-midi-runtime`, `tutti-midi-file` | `midi` | |
| `tutti-midi-hardware` | `midi-hardware` | |
| `tutti-sampler` | `sampler` | streamer, voices |
| `tutti-soundfont`, `tutti-polysynth` | `soundfont`, `synth` | |
| `tutti-spatial` | `spatial` | the `SessionNode` impls only |
| `tutti-export` | `export` | |
| `tutti-plugin` | `plugin` (+ `vst2`/`vst3`/`clap`/`au`) | |

Constraints on the crate:

- **There is no `bevy` feature, and nothing reaches Bevy.** Any type a Bevy
  app wants reflected lives in the crate that owns it, behind that crate's
  `bevy` feature, as today (`tutti-mod/bevy`).
- No tutti crate depends on `tutti-session` except `bevy-tutti` and
  `tutti`.
- `tutti-session` names neither umbrella.
- Like `tutti-graph`, its modules are private: everything is
  `tutti_session::Item`.

`tutti-plugin-server` stays out, as it does for `tutti`. A host that runs
plugins out of process builds the server binary, as `bevy-tutti`'s tests
do.

### 4.2 The types

```rust
pub struct SessionBuilder { /* options */ }
pub struct Session { /* graph, transport, engine side, subsystems */ }
pub struct Graph { /* today's GraphRuntime: Editor, nodes, feeds, pool */ }

pub enum Output {
    Device(DeviceSpec),                       // `device`
    Offline { rate: SampleRate, channels: ChannelLayout, max_block: Samples },
}

/// A node that may enter the graph: today's `GraphNode`, renamed.
/// `params` says which of its controls is a `ParamSet`.
pub trait SessionNode: IntoNode { fn params(c: &Self::Controls) -> Option<ParamSet> { None } }

pub enum SessionEvent {
    PluginDied { node: AudioNode },           // after the debounce
    PluginLatencyChanged { node: AudioNode, latency: Samples },
    ModulationRefused(ModRefusal),            // keyed by SourceId / ParamRef
    ExportDone(Result<ExportOutput, ExportError>),
    DeviceFault(StreamFault),
    MidiOutDropped(u64),
    // ...
}
```

- **The node handle is `AudioNode`**, as now. A crossfade keeps the key
  (`Editor::replace_or_swap` swaps the unit behind it,
  `graph/runtime.rs:435-466`), so a headless host can hold a handle across
  swaps.
- **The Entity layer is a Bevy concern.** It exists so a *component* can be
  re-bound to a new node.
- **`Session` is `Send`, not `Sync`**, and owned by one control thread.
  This is the Bevy main world's role today. `bevy-tutti` keeps its
  `Mutex`, which exists only for `Sync`.
- **Blocking work is a `Send` job the host places.** A plugin open, a
  SoundFont build and an export render are each a value with
  `fn run(self) -> Done`. The host runs it on any thread and hands `Done`
  back to the session on the control thread.
  - This is `tutti-export`'s existing rule, "spawns no threads", applied to
    the whole layer.
  - `bevy-tutti` places jobs on `AsyncComputeTaskPool`. A CLI calls
    `run()` inline.
  - Each job has a `_blocking` convenience on `Session` that does both
    halves.

### 4.3 Lifecycle

**Create, and start or stop the device.**

```rust
let mut s = Session::builder()
    .output(Output::Device(DeviceSpec::default()))   // or Output::Offline { .. }
    .outputs_floor(2)                                 // TuttiPlugin::outputs today
    .render_workers(0)                                // one per core
    .midi_hardware(true)                              // `midi-hardware`
    .build()?;                                        // build_on + assemble
s.restart(DeviceRestart::rate(SampleRate(96_000.0)))?; // rerate, §2.1
s.stop(); s.start()?;
```

- `Output::Offline` builds everything but the stream. The host pulls blocks
  with `s.render(&mut out)` (it calls `Engine::process`, in `tutti-core`), so
  the offline path never links `tutti-cpal`.
- Today's device-free path (`AudioEngine::from_spec` on a
  `ManualStreamDriver`) is kept for tests that want the callback state
  itself.

**Add and wire nodes.**

```rust
let osc = s.graph().insert(ForkByClone(Osc::saw(Hz(110.0))));      // (AudioNode, Controls)
let (filter, ctl) = s.graph().insert(SvfFilterNode::<f64>::new(..));
s.graph().set_source(filter, 0, GraphSource::node(osc.0, 0));
s.graph().set_outputs_from(filter);
s.graph().crossfade(filter, other_filter, Seconds(0.005))?;          // retried by update() while Busy
```

**Params.** The units rule wants a typed setter. `ParamKey<U>` already
exists (`tutti-types` `unit_param.rs:204`), so:

```rust
s.graph().set(filter, params::CUTOFF, Hz(800.0));   // ParamKey<Hz>, through the node's ParamSet
```

The untyped `set_param(node, UnitParam, f32)` stays for plugin and dynamic
addresses.

**Modulation.** These are declarations owned by the session, keyed by ids
it mints. The full example is §5.1.

```rust
let lfo = s.mods().lfo(Lfo::sine().rate(BeatDuration(0.5)));    // builders from tutti-mod
s.mods().range(ParamRef::unit(filter, UnitParam::Cutoff), 0.0, 1_000.0);
s.mods().route(lfo, ParamRef::unit(filter, UnitParam::Cutoff), Depth(0.3));
```

**Plugins.** The handle is the node; editor windows are the host's own. A
plugin's parameter edits arrive through `PluginParamValues` and become
knob writes at the next update.

```rust
let job = s.plugin_load(PluginRequest { id, state: None, ..Default::default() }); // Send
let done = job.run();                        // any thread
let synth = s.add_plugin(done)?;             // AudioNode; controls and meter captured
```

**MIDI.**

```rust
let live = s.midi().live_input(synth)?;           // MidiQueueNode fed into synth; returns a MidiSender
s.midi().play_clip(synth, events)?;               // one MidiClipNode per target, updated in place
s.midi().route(MidiRouteRule::any_channel().to(synth)); // `midi-hardware`
```

**Transport.** `s.transport()` returns today's `Transport` handle, and
`MotionFsm::schedule(At, ..)` is unchanged.

**Export.**

```rust
let job = s.export(ExportRequest::file("out.wav", ExportConfig::default()))?; // gather on this thread
let out = job.run();                  // build, announce Offline, render, all on a worker
s.finish_export(out)?;                // drop leftovers here, announce Realtime
```

### 4.4 `update()`, and the order is data

Bevy runs reconciliation as `GraphReconcileSystems`:
`Spawn → Params → Despawn → Compensate → Commit`, plus the plugin, MIDI
and modulation systems ordered around them. A headless host gets the same
order from one call:

```rust
pub const PHASES: &[Phase] = &[
    Phase::Collect,          // editor.collect, finished jobs promoted
    Phase::Plugins,          // health debounce, latency poll, GUI edits → knobs
    Phase::Modulation,       // collect + apply
    Phase::Commit,           // commit, publish compensation, retry crossfades
    Phase::AfterCommit,      // modulation re-apply while unresolved, resyncs
    Phase::MidiOut,          // hardware out drains, clock out
    Phase::Poll,             // device faults, export status
];
impl Session {
    pub fn update(&mut self) -> Vec<SessionEvent> { /* each phase in PHASES order */ }
    pub fn run_phase(&mut self, p: Phase) -> &mut Vec<SessionEvent>;
}
```

- Bevy's `Spawn`, `Params` and `Despawn` have no headless phase: a
  headless insert, param write or removal is a direct call between updates.
  `ModulationSystems::Apply` (between `Spawn` and `Params` today) maps to
  `Phase::Modulation`, and `ModulationSystems::AfterCommit` to
  `Phase::AfterCommit`.
- `bevy-tutti` builds its system-set chain *from* `PHASES`. Its schedule
  test asserts the chain and the constant agree, so the order cannot drift
  between the two hosts.
- Counts that are "frames" today become "updates":
  - the plugin-death debounce of 3;
  - the snapshot interval of 512;
  - the modulation warning after 240.
  They are builder options, with today's values as defaults. The host owns
  the cadence; at 60 Hz the behaviour is today's.

### 4.5 Wiring stays declared

"Wiring is declared, not called" is kept, and it needs no Entity layer.

- **The declaration is the spec.** `Editor`'s `spec_mut().topology` is a
  `Topology` value, and `set_source` writes one port of it.
- **The commit is the compile.** Nothing is spliced into a running graph,
  and PDC is the compiler's.
- Edits between two updates coalesce into one commit, as `GraphDirty` does
  per frame.
- **Partial declarations behave as they do today.** A port the host never
  writes keeps its value. In Bevy, the rebuild writes only the ports a
  `PortSources` declares.

Why the ECS layer exists: a *component* needs to be re-bound to a new node
and re-derive its wires (`Changed<AudioNode>`).

Why a headless host does not need it:

- Its handle is the `AudioNode` itself, stable across crossfades.
- Imperative calls *on a value that compiles at a boundary* are what
  "declared" means here.

Two things are deliberately not offered:

- a whole-`Topology` replace, since it would fight Bevy's partial
  declarations when both hosts share one session;
- direct `Editor` access. `Graph` stays opaque, as `AudioGraphRes` is, so
  the edit/commit boundary stays visible.

### 4.6 Modulation keys

`tutti_mod`'s declarations key targets by `TargetKey { node: u64, param }`
and sources by `SourceKey(u64)`. Today the u64 is an `Entity`'s bits, and
`ModHostOver` decodes it (`graph/runtime.rs:998`, `:1020`).

The session takes a resolver instead:

```rust
pub trait ModKeys {
    fn node(&self, target: u64) -> Option<AudioNode>;
    fn source(&self, source: SourceKey) -> Option<Feed>;
}
impl Session { pub fn apply_modulation(&mut self, d: &Declarations, keys: &dyn ModKeys) -> Applied; }
```

- **Headless:** `s.mods()` keeps its own declarations, keyed by the
  `AudioNode`'s `NodeKey` bits and by minted `SourceId`s, so the identity
  resolver applies.
- **Bevy:** `Declared` implements `ModKeys` over entities. The re-apply loop,
  the resync and the warning counter are the session's either way.

### 4.7 Errors

`tutti_session::Error` covers the build, the device, the graph (commit,
reprepare), plugins, MIDI and export.

Refusals keyed by a host id live in the host:

- `ModulationRefused` mapping a refusal to an `Entity`;
- `ExportError::NotForkable` naming an entity.

The session reports `AudioNode`s, `SourceId`s and `ParamRef`s. The adapter
translates them.

## 5. Examples

### 5.1 Doc 014 §8.1, headless

```rust
use tutti::prelude::*;

let mut s = Session::builder().output(Output::Device(DeviceSpec::default())).build()?;
let (filter, _) = s.graph().insert(SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(1_000.0), Q(0.7)));
let (pad, _) = s.graph().insert(pad_synth());
let (kick_bus, _) = s.graph().insert(kick());
s.graph().set_source(filter, 0, GraphSource::node(pad, 0));
s.graph().set_outputs_from(filter);

let cutoff = ParamRef::unit(filter, UnitParam::Cutoff);
let gain = ParamRef::unit(pad, UnitParam::Volume);
let mut m = s.mods();
m.range(cutoff, 0.0, 1_000.0);
m.range(gain, 0.0, 1.0);
let wobble = m.lfo(Lfo::sine().rate(BeatDuration(0.5)));
m.route(wobble, cutoff, Depth(0.3));
let kick = m.follower(kick_bus);
m.route(kick, gain, Depth(-0.6)); // sidechain

s.transport().play();
loop { for e in s.update() { log(e) } sleep(frame) }
```

- This is line for line the Bevy example, with `ParamRef` taking an
  `AudioNode` where Bevy takes an `Entity`.
- The 250 lines of `headless_modulation.rs` (the hand-built `ModHost`, the
  `Declarations` and `apply`) become the `update()` call.
- PR 4 ports `tests/mod_lanes.rs`'s
  `the_8_1_example_renders_its_analytic_values` onto an offline session,
  with the same analytic figures.

### 5.2 Load a plugin, play a MIDI file, export to WAV

```rust
use tutti::prelude::*;

let mut s = Session::builder()
    .output(Output::Offline { rate: SampleRate(48_000.0), channels: ChannelLayout::STEREO, max_block: Samples(512) })
    .build()?;

let synth = s.add_plugin(s.plugin_load(PluginRequest { id: PluginId::clap("org.example.synth"), ..Default::default() }).run())?;
s.graph().set_outputs_from(synth);

let file = MidiFile::read("song.mid")?;                 // tutti-midi-file: sniffs SMF vs Clip File
s.midi().play_clip(synth, file.timed().map(TimedMidiEvent::from))?;
s.transport().set_tempo(file.tempo());

let seconds = file.duration().to_seconds(file.tempo());  // f64: an hour-long render
let job = s.export(ExportRequest::file("song.wav", seconds)
    .latency_from_graph()                                // RenderConfig::latency, from the plan
    .tail_from_graph())?;                                // RenderConfig::tail, the plugin's declared tail
let out = job.run();                                     // the plugin hears Offline mode
s.finish_export(out)?;
```

`ExportRequest` is today's `bevy-tutti` request, with its `latency_from_graph`
and `tail_from_graph` fields (`export/request.rs:116-120`), moved. It fills
`tutti-export`'s `RenderConfig`, which stays a pure value: "the caller
knows, so the caller says" (`tutti-export` `config.rs:50-59`). The session is
that caller.

Three engine changes this example needs, each a **(c)** move in §9:

- `MidiFile::read`, with `timed()` yielding `(Beat, MidiEvent)` for both
  containers;
- `TimedMidiEvent: From<(Beat, MidiEvent)>`, which exists;
- the render-mode announcement, carried by the export job.

## 6. How `bevy-tutti` rebases

| Module | Today | After |
|---|---|---|
| `engine/` | builds everything into the World | `build_into` calls `SessionBuilder::build` and inserts the parts as resources; `restart` is a `Session::restart` call inside the NonSend dance |
| `graph/runtime.rs` | `GraphRuntime` | deleted; `AudioGraphRes(Mutex<Session>)`, its methods delegate |
| `graph/commit.rs`, `latency.rs` publish | logic | one system calling `run_phase(Commit)` |
| `graph/topology.rs`, `wire.rs`, `spawn.rs`, `despawn.rs`, `param.rs` | glue | unchanged; the spawn path's crossfade calls `Graph::crossfade` |
| `graph/events.rs` | `GraphNode`, `EventFeeds` | `GraphNode` becomes `SessionNode` (re-exported); feeds keyed by `AudioNode` |
| `graph/pump.rs` | thread loop + systems | systems only, over `tutti-io`'s pump |
| `modulation/` | declare + reconcile | components and `Mods` stay; `Declared: ModKeys`; reconcile calls `apply_modulation` |
| `plugin_host/` | load, health, latency, params, render mode, windows, scan | load is a `PluginLoad` job on the task pool; health, latency and params are session phases; windows, scan and catalog stay |
| `midi/` | nodes, sequence, routes, hardware out, files | components call `Session::midi()`; the asset loader wraps `MidiFile`; MIDI-CI device state stays |
| `sampler/`, `soundfont.rs` | logic + assets | assets and pending tasks only |
| `export/` | pipeline + request | `ExportRequest` observer creates an `ExportJob`, the task pool runs it, `poll_exports` calls `finish_export` |

**Stays Bevy-only:**

- the ECS vocabulary: `AudioParam<U, P>`, `PortSources`, `MasterSources`,
  `Mods` and the mod components, the requests;
- the asset loaders;
- the plugin editor windows and live resize;
- `GraphReconcileSystems` (built from `PHASES`);
- Reflect registration;
- the `On<Remove, AudioNode>` observer.

`bevy-tutti` keeps its direct edges to the engine crates for their `bevy`
features, and adds one to `tutti-session`. Its features forward to
`tutti-session`'s, as they forward today: `midi` → `tutti-session/midi`,
and so on. `device` is always on for `bevy-tutti`.

## 7. The umbrella, for all the tutti crates

`crates/tutti` is the answer to "an umbrella crate … for all the tutti
crates". It already re-exports every engine crate under an alias. It
gains:

```rust
pub use tutti_session as session;          // always: the API layer
pub mod prelude {
    pub use tutti_session::{Session, SessionBuilder, Output, SessionEvent, ParamRef, Phase};
    // ...today's prelude, unchanged
}
```

**Features.**

- `tutti`'s features keep their names. Each enables the member crate as
  today *and* `tutti-session`'s feature of the same name, so there is still
  one switch per subsystem.
- `full` is unchanged, and the plugin formats stay opt-in.
- A new feature, `session`, is on in `default`. It is the one deliberate
  exception to `default = []`: the API layer is what the umbrella is *for*.
  A host wanting only the raw crates sets `default-features = false`. This
  is §11 question 3.

**Organisation.** The root has the aliased crate modules (`tutti::core`,
`tutti::graph`, `tutti::session`, …) and one prelude.

The prelude is the session plus the vocabulary a session host writes
daily: units, `UnitParam`, `ParamKey`s, `AudioNode`, the transport's `At`,
the node constructors of the enabled DSP crates. Nothing in it is reached
through a second path.

**`no_logic.rs`** is unchanged. It already allows exactly `pub use`,
`pub mod` and `}`; `pub use tutti_session as session;` passes it.

**`check-canonical-paths.sh`:**

- The chained gate already covers a chain through `tutti_session`.
- Add `tutti_session` to the per-module gate invocation in CI, as
  `tutti_graph` is, since its modules are private.

## 8. Rules

The rule must stay mechanically enforced. Three checks enforce it, each
with its failure message naming the alternative.

1. **`tutti` holds no code.** `no_logic.rs`, unchanged.
2. **`tutti-session` is Bevy-free.** Add
   `crates/tutti-session/tests/bevy_free.rs`. It reads
   `cargo metadata --format-version 1` for `tutti-session --all-features`,
   resolves the normal-dependency closure, and fails on any package named
   `bevy` or `bevy_*`. A second assertion is that the crate's `Cargo.toml`
   declares no `bevy` feature.
   - Mutation: add `bevy_ecs` as an optional dep behind a feature. The test
     must fail on the `--all-features` closure.
3. **`bevy-tutti` holds no engine logic.** Add `scripts/check-adapter.sh`,
   run in CI. It fails if `crates/bevy-tutti/src` (tests excluded) names any
   of the engine's composition entry points:
   - `Editor::new`, `.commit()` on an `Editor`;
   - `Modulation::new`, `.apply(`;
   - `RenderGraph`, `TuttiDriver::from_parts`, `AudioCallbackState::new`;
   - `Plugin::open_with`;
   - `Engine::new`.
   After the rebase these appear only in `tutti-session`. The deny-list
   lives in the script, with each entry's owner beside it.
   - Mutation: call `Editor::commit` from a system. The script fails and
     names `Session::commit`.

`no_logic.rs` cannot tell "logic" from "glue" in general. The three checks
pin the one thing that failed before: both hosts call the same entry
points, so no umbrella owns the composition.

**CLAUDE.md edits** (in PR 7, when they become true):

- **Workspace tree:** add `tutti-session` with the line "Bevy-free API
  layer: the cross-crate composition both umbrellas use."
- **Bullets:**
  - Replace "A Bevy app depends on `bevy-tutti`; a headless consumer
    depends on `tutti`. `bevy-tutti` does not depend on `tutti`." with the
    same, plus: "Both use `tutti-session`. It holds only code that names
    two or more engine crates, or the order they run in, and has no `bevy`
    feature (`tests/bevy_free.rs`). `bevy-tutti` reaches the engine's
    composition only through it (`scripts/check-adapter.sh`)."
  - Add `tutti-session` to "keeps every module private".
- **"Rules that came from bugs":** replace "The `tutti` umbrella holds no
  code. Anything that wants to live there belongs in the crate that owns
  what it wires." with: "Neither umbrella holds engine logic. A piece with
  one owner goes to that crate; a piece composing several goes to
  `tutti-session`."
- **"Wiring is declared, not called":** add one line saying a headless
  `Session` declares through its own spec, keyed by `AudioNode`.
- **Commands:** add `scripts/check-adapter.sh` beside the other two
  scripts.

**engineering-notes edits:**

- **"There are two umbrellas…":** add a paragraph recording this design,
  that is, why (a) and not (b). Correct "device bootstrap in `tutti-cpal`,
  graph logic in `tutti-core`", which §3(c) shows would invert edges.
- **"Workspace Structure":** add the tree entry, and change the member
  count.
- **"Wiring is declared, not called":** add the headless paragraph of §4.5.

**doc 013:** a pointer from Phase 3's `bevy-tutti` items to this doc. The
graph contract does not change.

## 9. The PR plan

Each PR is green alone, `bevy-tutti`'s full suite included, and none is
merged by its author.

- Every new test carries its mutation record.
- Analytic figures only: no render compared against another reference
  render.
- Each PR logs its CHANGELOG entry under `[Unreleased]` in "Was | Now".
- The `audio-automation` pin and the bevy 0.19 line are unaffected.

PRs 1–3 are **(c)** moves and can land in any order. PR 4 needs them. PRs
5–7 are sequential.

### PR 1: MIDI and sample pieces to their owners

**Scope:**

- `tutti-midi-file`: `MidiFile` (sniffed SMF or Clip File, today's
  `MidiFileContents`) with `timed()` → `(Beat, MidiEvent)` for both. The
  SMF half is new (midly → `MidiEvent::from_midi1_bytes`).
- `tutti-midi-runtime`: the route-rule compile (rules → `input_ports`, with
  the fallback) as a pure function.
- `tutti-sampler`: `memory_voice` and `voice_width`.
- `bevy-tutti` calls them.

**Properties:**

- An SMF and a Clip File of the same notes yield the same
  `(Beat, MidiEvent)` list, velocity widened.
- A note-on at tick 480 of a 480-ppq file lands at `Beat(1.0)`.
- The route compile is total over the rule set, and its fallback holds only
  unrouted channels.
- The existing `voice_width` and memory-voice tests move with their code.

**Exit:** `bevy-tutti`'s MIDI file and route tests pass unchanged against
the moved code.

### PR 2: plugin and SoundFont pieces to their owners

**Scope:**

- `tutti-plugin`:
  - `PluginParamValues`;
  - the liveness debounce as a counter type (`Liveness::observe(dead) ->
    Verdict`, with the threshold a parameter);
  - the render-mode announcement rule (`needs_announcement`).
- `tutti-soundfont`: `SoundFontUnit::with_preset(font, rate, bank,
  program)`.
- `tutti-mod`: the `Lfo`/`Macro`/`Follower`/`NoteEnvelope` builders,
  producing a `Source`. Reflect stays behind `tutti-mod/bevy`.

**Properties:**

- The debounce declares dead on exactly the 3rd consecutive death and
  resets on a live reading.
- Each builder's `Source` is pinned field by field.
- A preset unit's first note-on renders that preset: a golden digest on the
  recording target.

**Exit:** `plugin_host/health.rs`'s and `modulation/tests.rs`'s tests pass,
calling the moved types.

### PR 3: the pump to `tutti-io`

**Scope:** `AudioPump` (`graph/pump.rs:135-240`) joins `tutti-io` beside
`Recorder`. If it duplicates `Recorder`'s loop, one of them goes; the
PR decides which by reading both.

**Properties:** the existing pump tests (ending on `ON_EMPTY`, `stop`
joining) run in `tutti-io`.

**Exit:** `bevy-tutti`'s pump systems compile over the moved type.

### PR 4: `tutti-session`: graph, bootstrap and commit

**Scope:**

- The crate, with features and `tests/bevy_free.rs`.
- `Graph` (today's `GraphRuntime`, moved with its tests).
- `SessionBuilder` with `Output::{Device, Offline}`, `restart`, `render`.
- Commit with compensation publication, the crossfade retry, `EventFeeds`,
  `SessionNode` with the engine-node list.
- `PHASES` and `update()`.

`bevy-tutti`'s `AudioGraphRes` wraps `Session`; `engine/build.rs` calls the
builder. The `tutti` umbrella re-exports `session`.

**Properties:**

- An offline session with a 1 kHz sine renders its analytic RMS.
- A latent node on one branch is compensated on the other, to the sample.
- A rerate from 48 kHz to 96 kHz keeps the published compensation in step
  with the plan.
- A crossfade refused `Busy` during a reprepare lands on a later update.
- `bevy_free.rs` and its mutation.

**Exit:** every `bevy-tutti` graph, engine and restart test passes over the
session. `headless_engine.rs` is rewritten on `Session` and is under 20
lines.

### PR 5: modulation and MIDI in the session

**Scope:**

- `ModKeys`, `apply_modulation` and the re-apply/resync loop.
- `s.mods()` with minted ids and the take recorder.
- `s.midi()`: live input, clip sequencing, routes, hardware out and clock
  out.
- `bevy-tutti`'s `Declared` implements `ModKeys`; its MIDI systems call the
  session.

**Properties:**

- §5.1 headless renders `the_8_1_example`'s analytic values on an offline
  session.
- A route to a target placed one update later resolves on that update
  (the re-apply loop).
- A clip edit updates in place: the node count stays the same.

**Exit:** `tests/mod_lanes.rs` and the MIDI suites pass.
`headless_modulation.rs` moves to `tutti-session/examples` as §5.1.

### PR 6: plugins and export in the session

**Scope:**

- `PluginLoad`, `add_plugin`, and the health, latency and GUI-edit phases.
- `ExportJob` with the render-mode announcement and drop-on-control-thread.
- `bevy-tutti` places jobs on the task pool.

**Properties:**

- An export over a CLAP test plugin announces Offline before its first
  block and Realtime after its last. The test plugin records the calls.
- A plugin whose server dies three updates running is removed and reported
  once.
- An export's leftovers are dropped on the calling thread (a `!Send` drop
  probe).

**Exit:** `clap_pdc_alignment`, `clap_crash_recovery` and the export
suites pass. §5.2 is an example under `tutti-session/examples`, run in CI
offline with the CLAP test plugin.

### PR 7: rules and gates

**Scope:**

- `scripts/check-adapter.sh` in `just ci` and CI.
- `bevy-tutti`'s schedule test against `PHASES`.
- The CLAUDE.md, engineering-notes and doc 013 edits of §8.
- The CHANGELOG migration table: `GraphRuntime` → `Graph`, `GraphNode` →
  `SessionNode`, `TuttiPlugin` options → builder.

**Properties:** both gates' mutations (§8).

**Exit:** `just ci` is green, and `scripts/check-adapter.sh` reports no
engine entry points in `bevy-tutti/src`.

## 10. Out of scope

- **A plugin editor window API for headless hosts.** It needs a window
  system; a host brings its own, and the `tutti-plugin` handle already
  opens an editor into a raw handle.
- **Project files and session save/load.** "Session" here means a running
  engine, not a document. If the name misleads, see §11 question 1.
- **Any change to the graph, the node contract or the RT rules.**

## 11. Open questions

1. **The crate's name.** The proposal is `tutti-session`, with `Session`.
   - `tutti-engine` collides with `tutti_core::Engine`.
   - `tutti-host` collides with the plugin host and the cpal host.
   - `tutti-app` collides with Bevy's `App`.
   - `tutti-runtime` is close to `tutti-midi-runtime`.
   - "Session" may suggest a saved project to a DAW user. Which name do you
     want?
2. **Should `bevy-tutti` re-export `tutti-session`?** Its users would then
   get `Session` too, for a headless test rig beside their app. This would
   be `bevy_tutti::session`, not a dependency on `tutti`.
3. **Should `tutti`'s default features include the API layer?** Today
   `tutti` is `default = []`, and §7 proposes `default = ["session"]`. The
   cost is that a bare `tutti` dependency builds `tutti-nodes` and the
   graph.
4. **Where should this sit in the queue?** You said "queue this item after
   everything". PRs 1–3 are small and independent. Should they wait too, or
   may they land while other work is in flight, with PRs 4–7 queued last?
