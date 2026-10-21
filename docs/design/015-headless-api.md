# 015: a Bevy-free tutti API layer

Status: **complete**. Every step of §9 has landed; each says so in a
"Landed" note, and the one item still deferred (Bevy's `Phase::Poll`) is in
§9.7's. The user has made every
decision it asked for (§11). §2's file:line citations are the survey of
`bevy-tutti` before the migration.

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

**The user's decisions**, applied below:

- `tutti` turns the layer on by default: `default = ["session"]` (§7).
- `bevy-tutti` does **not** re-export the session (§6).
- All seven PRs of §9 will run.
- The crate is named `tutti-session`, re-exported as `tutti::session`.

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

The survey covers `crates/bevy-tutti/src/`: about 15,280 non-test lines,
doc comments included. The count stops at each file's `#[cfg(test)] mod`
and leaves out the separate `tests.rs` files.

Each piece below is one of two things:

- **glue**: ECS components, systems, resources, events, assets, windows, or
  a scheduling choice that belongs to the host;
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
| `build_into` opening the device | `engine/build.rs:60-67` | logic | `SessionBuilder::build_device` |
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
| `GraphRuntime::modulate` over `ModEntities` | `graph/runtime.rs:566` | logic over an `Entity` seam | session, over `ModKeys` (§4.6) |
| `ModHostOver`: target and feed resolution | `graph/runtime.rs:985-1023` | logic, decodes `Entity::try_from_bits` (`:998`, `:1020`) | session; the decode stays in bevy-tutti's `ModKeys` impl |
| `AudioSide` renderer, `PendingExport` | `graph/runtime.rs:1028-1142` | logic | session |
| `AudioGraphRes(Mutex<GraphRuntime>)`, `AudioConfig`, `GraphSource` | `graph/resources.rs` | glue (the `Mutex` is only for `Sync`) | stays, wraps `Session` |
| `commit_graph`, `commit_and_publish` | `graph/commit.rs:29-80` | logic | `Phase::Commit` |
| `ChannelCompensation`, `GraphLatency`, `publish`, `publish_sent` | `graph/latency.rs:110-168` | logic | session |
| `LatencyCompensationPlugin`, the debug check | `graph/latency.rs:170-226` | glue | stays |
| Topology from `PortSources`/`MasterSources`, diff-apply, `Changed<AudioNode>` | `graph/topology.rs`, `graph/wire.rs` | glue (the Entity layer) | stays |
| The root-widen policy inside the rebuild | `graph/wire.rs` | logic | `Graph::widen_outputs` (already there) |
| `GraphDirty` | `graph/schedule.rs:99` | logic: it decides whether a commit goes out, and `after_commit` reads it (`reconcile.rs:428-436`) | replaced by the session's own edit flag (§4.4) |
| `GraphReconcileSystems` | `graph/schedule.rs` | glue | stays, built from `PHASES` (§4.4) |
| `spawn_audio_node` and friends | `graph/spawn.rs:114-212` | glue | stays |
| Crossfade retry queue, 5 ms equal-amplitude fade | `graph/spawn.rs:327-429` (fade at `:367`) | logic | `Graph::crossfade` + session retry |
| `CapturedControls` (plugin controls, meter) | `graph/capture.rs:57-151` | logic, bound to an entity | the capture moves; `bind` stays |
| `EventFeeds` (named feeders merged per sink) | `graph/events.rs:80-110` | logic | session, keyed by `AudioNode` (§4.5) |
| `GraphNode` trait, `param_graph_node!` list | `graph/events.rs:112-280` | logic (which controls are a `ParamSet`) | `tutti_session::SessionNode` |
| Event `reconcile` | `graph/events.rs:292-347` | glue | stays |
| `AudioParam<U, P>` | `graph/param.rs` | glue | stays |
| `AudioPump` thread loop | `graph/pump.rs:135-240` | logic | **(c)** `tutti-io`: **done**, the loop was `Recorder`'s; `AudioPump` now holds a `Recorder` |
| Pump drain/finalize systems | `graph/pump.rs:266-383` | glue | stays |
| Transport, tap and metering resource wrappers | `graph/transport.rs`, `tap.rs`, `metering.rs` | glue | stays |

`GraphRuntime` provides, all Bevy-free but for `bevy_log` (`runtime.rs:243`,
`:590`, `:905`, `:929`), which becomes `tracing` (§4.1):

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
| `apply` / `after_commit` protocol (detail below) | `modulation/reconcile.rs:325-444` | logic | `Phase::Modulation`, `Phase::AfterCommit` |
| Unresolved-target warning after 240 frames | `modulation/reconcile.rs:54` | logic | session, as a time (§4.4) |
| `ModulationState` | `modulation/reconcile.rs:39-105` | logic, keyed by `Entity` | session, keyed by `tutti_mod`'s `TargetKey`/`SourceKey` |
| `ModulationRefused` → route/target entity | `modulation/reconcile.rs:109-185` | glue | stays |
| `ModEntities` (`node`, `plugin_value`, `feed`) | `modulation/reconcile.rs:174-182` | the seam | becomes `ModKeys` (§4.6) |
| `Declared` query | `modulation/reconcile.rs:186-324` | glue | stays, implements `ModKeys` |
| `Lfo`, `Macro` → `tutti_mod::Source` | `modulation/declare.rs:142-243` | logic | **(c)** `tutti-mod` |
| `Follower`, `NoteEnvelope` | `modulation/declare.rs:245-330` | logic (the `Source`) plus glue (the `Entity` they follow, `ModFeed::Audio(Entity)` at `:79`) | split: the `Source` half to **(c)** `tutti-mod`, the feed stays |
| `AutomationTake`, `close_takes` | `modulation/declare.rs:446-530` | logic | session (`tutti-nodes`' automation `Recorder` plus the close rule) |
| `ModSource`/`ModRoute`/`ModRange`/`Automation` components, `Mods` | `modulation/declare.rs` | glue | stays |

`collect` checks ranges and automation for `DuplicateTarget`, and gives a
range-less automation its default of ±inf.

The `apply` / `after_commit` protocol:

1. Apply.
2. Commit.
3. While targets are unresolved, re-apply and commit again.
4. Send the resyncs.

Both halves read the declarations: `after_commit` collects them again from
the ECS queries (`reconcile.rs:416-444`). §4.4 says how the session gets
them.

### 2.4 `plugin_host/`

| Piece | Where | Kind | Destination |
|---|---|---|---|
| Load (detail below) | `plugin_host/load.rs:225-375` | logic | `PluginLoad` job + `Session::add_plugin` |
| At most 2 concurrent loads (`MAX_CONCURRENT_LOADS`) | `plugin_host/load.rs:72` | glue: a rule for Bevy's shared task pool | stays |
| The ECS request/observer shape | `plugin_host/load.rs:115-224` | glue | stays |
| Meter cell bound from the metronome | `plugin_host/bind.rs` | logic | session |
| Latency poll: declared vs graph, `refresh_node_latency` | `plugin_host/latency.rs:73-96` | logic | `Phase::Plugins` |
| `PluginParamValues` mirror | `plugin_host/params.rs:48-122` | logic, except `read`'s spawn on `AsyncComputeTaskPool` (`:72`) | **(c)** `tutti-plugin`, with the read split out as a job |
| GUI edit queue → knob | `plugin_host/params.rs:135-169` | logic | `Phase::Plugins` |
| Health debounce: 3 deaths, then remove | `plugin_host/health.rs:67`, `:187-240` | logic | **(c)** `tutti-plugin` (the rule), session (the removal) |
| State snapshot every 512 frames | `plugin_host/health.rs:76`, `:251-300` | logic, except the spawn on `AsyncComputeTaskPool` (`:295`) | session, with the snapshot as a job |
| Render mode: Offline while any export is in flight | `plugin_host/render_mode.rs:58-130` | logic, level-triggered | `Phase::RenderMode` (§4.3), `needs_announcement` to **(c)** `tutti-plugin` |
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
| Clip sequencing (detail below) | `midi/sequence.rs:101-188` | logic | `Session::midi().play_clip` |
| Route rules → input ports, with a fallback | `midi/inbound/route.rs:143-206` | logic | **(c)** `tutti-midi-runtime` (pure compile) |
| `MidiOutRouter`, `drain_receiver_through`, `MidiOutDrops`, JR stamping | `midi/hardware/hardware_out.rs:35-300` | logic | session (it joins runtime and hardware) |
| Clock-out pump | `midi/hardware/clock_out.rs:55-80` | logic | session |
| `track_out`, `metadata`, `device` | `midi/hardware/` | glue over the runtime's state machines | stays |
| MIDI-CI and UMP endpoint discovery | `midi/hardware/negotiation.rs` | glue over `tutti-midi-runtime`'s `CiResponder`/`CiInitiator` | stays; out of scope for the session (§10) |
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
| `soundfont_playback_system`, `promote_pending_soundfonts` | `soundfont.rs:182-296` | mostly glue (queries, `AsyncComputeTaskPool`, commands); the logic is the rate cast and `SoundFontUnit::new` (`:205-212`) and the `program_change` (`:266`) | the logic to **(c)** `tutti-soundfont`; the systems stay |
| Asset and pending task | `soundfont.rs` | glue | stays |
| Export pipeline (detail below) | `export/run.rs:50-335` | logic | `ExportJob` |
| `ExportRequest` component: `ExportSource::Node(Entity)` (`request.rs:61`), `PrepareGraph = Fn(&World)` (`:609`) | `export/request.rs` | glue over a request | stays; translates into the session's request (§4.3) |
| `NodeNames`: entity ↔ node, error names through `Name` | `export/run.rs:272-296` | glue | stays; maps the session's `AudioNode`s back |
| Observers, `poll_exports` | `export/run.rs:336-372` | glue | stays |
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

About **4,600 of 15,280 non-test lines (≈30%) move**. The figure includes
doc comments and is an estimate.

- Half of it is three pieces: `GraphRuntime` (1,142 lines, almost all of it),
  the export job (≈350) and the modulation protocol (≈650 across
  `reconcile.rs` and `declare.rs`).
- About 550 of those lines are **(c)** moves into single owning crates.
- The rest is glue, and it stays: topology/wire, spawn, the param
  components, the asset loaders and their systems, the plugin windows, the
  MIDI-CI device state, the request/observer shapes, the load cap and the
  schedule.

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
- The risks are a second god-crate and a third umbrella. §4.1 bounds the
  first: it holds composition only, and anything with a single owner goes to
  that owner. §8 check 4 bounds the second: it may not re-export a crate
  whole.

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
  `ChannelCompensation` table, the export's render-mode phase).
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
| `tracing` | always | logging; what `GraphRuntime`'s `bevy_log` calls become |
| `tutti-cpal` | `device` | the sound card; off means offline only, with no ALSA headers |
| `tutti-io` | `audio-io` | the pump (`Recorder`), mic |
| `tutti-mod` | `modulation` | declarations |
| `tutti-midi-types`, `tutti-midi-runtime`, `tutti-midi-file` | `midi` | |
| `tutti-midi-hardware` | `midi-hardware` | |
| `tutti-sampler` | `sampler` | streamer, voices |
| `tutti-soundfont`, `tutti-polysynth` | `soundfont`, `synth` | |
| `tutti-spatial` | `spatial` | the `SessionNode` impls only |
| `tutti-export` | `export` | |
| `tutti-plugin` | `plugin` (+ `vst2`/`vst3`/`clap`/`au`) | |

**Logging is `tracing`.** The workspace has no `log` dependency; `tracing
0.1` is what `tutti-sampler`, `tutti-midi-file`, `tutti-midi-hardware` and
`tutti-plugin` already use. `bevy_log` is a `tracing` subscriber, so a Bevy
app sees the session's events where it sees `GraphRuntime`'s today, and a
headless host installs any subscriber it likes.

Constraints on the crate:

- **`default = []`.** Every subsystem is a feature, as on every engine
  crate.
- **There is no `bevy` feature, and nothing reaches Bevy.** Any type a Bevy
  app wants reflected lives in the crate that owns it, behind that crate's
  `bevy` feature, as today (`tutti-mod/bevy`).
- **It is not an umbrella.** It never re-exports a crate whole (§8 check
  4). It re-exports single items its own signatures take, as any crate does.
- No tutti crate depends on `tutti-session` except `bevy-tutti` and
  `tutti`.
- `tutti-session` names neither umbrella.
- Like `tutti-graph`, its modules are private: everything is
  `tutti_session::Item`.

`tutti-plugin-server` stays out, as it does for `tutti`. A host that runs
plugins out of process builds the server binary, as `bevy-tutti`'s tests
do. The session's own plugin tests need it too, and fail without it (§8).

### 4.2 The types

```rust
pub struct SessionBuilder { /* options */ }
pub struct Session { /* graph, transport, engine side, subsystems */ }
pub struct Graph { /* today's GraphRuntime: Editor, nodes, feeds, pool */ }

/// The sound card: owns the `TuttiDriver`. `!Send`, so it stays on the
/// thread that opened it.
#[cfg(feature = "device")]
pub struct Device { driver: TuttiDriver, _not_send: PhantomData<*const ()> }

/// A node that may enter the graph: today's `GraphNode`, renamed.
/// `params` says which of its controls is a `ParamSet`.
pub trait SessionNode: IntoNode { fn params(c: &Self::Controls) -> Option<ParamSet> { None } }

pub enum SessionEvent {
    PluginDied { node: AudioNode },           // after the debounce (§4.4)
    PluginLatencyChanged { node: AudioNode, latency: Samples },
    ModulationRefused(ModRefusal),            // keyed by the host's TargetKey / SourceKey / RouteKey (§4.7)
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

**`Session` is `Send`, not `Sync`. `Device` is neither.**

- The session is owned by one control thread, the Bevy main world's role
  today. `bevy-tutti` keeps `AudioGraphRes(Mutex<Session>)`; the `Mutex`
  exists only for `Sync`.
- The device is split out because its stream may not move.
  `cpal::Stream` is `!Send`. `tutti-cpal` wraps it as `Send` with an
  `unsafe impl` whose safety argument is that the stream is created and
  dropped on the control thread (`driver_seam.rs:179-183`). `TuttiDriver`
  is therefore `Send` but not `Sync` (`driver.rs:89-91`), which is why
  `bevy-tutti` holds it with `insert_non_send` (`engine/build.rs:185`,
  `engine/restart.rs:154`).
  - A `Send` session that held the driver would let a host move the stream
    to another thread and drop it there. The `unsafe impl`'s argument would
    then be false, with nothing to say so.
  - `Device` is `!Send` on purpose, stricter than `TuttiDriver`, so that
    argument becomes a type error instead of a comment.
- The builder hands back the two halves (§4.3). A restart takes both,
  because it stops the stream (`Device`) and re-rates the graph
  (`Session`).

**The main-thread rule.** Some session calls free values whose `Drop` is
only legal on the host's main (UI) thread:

- The commit frees the previous graph version, including in-process plugin
  nodes whose `Drop` tears down a native editor window (AppKit, Win32,
  X11). `bevy-tutti` pins `commit_graph` and `after_commit` to the main
  thread with `NonSendMarker` (`graph/commit.rs:22-30`,
  `modulation/reconcile.rs:416-417`).
- An export's leftovers include forks of in-process plugins, whose
  teardown is main-thread-only (`tutti-export` `lib.rs:172-174`).

So these calls are main-thread calls: `update()`, `run_phase(Collect)`,
`run_phase(Commit)`, `run_phase(AfterCommit)`, `add_plugin`,
`finish_export`, and dropping the `Session`. The free itself is guarded
where it happens, at the editor's collect (the `Collect` phase, and on a
detached session every commit, `render_frame` and `set_sample_rate`, which
apply and collect at once), so no path around the list frees off the main
thread unchecked. `remove` is not on the list: it frees
nothing itself, and Bevy calls it from parallel systems (a clip whose target
lost its event input); the node it removes is freed at the next collect.

- With `plugin` on, each asserts it with `tutti-plugin`'s existing
  debug-only guard (`assert_main_thread`, `tutti-plugin-types`
  `main_thread.rs:40`). The host marks its main thread with
  `mark_main_thread()` at startup, as `bevy-tutti` does
  (`plugin_host/mod.rs:188`). The guard is a no-op until marked, so a
  test rig without a UI thread is unaffected.
- `Session: Send` stays, because a Bevy resource must be `Send`. A host may
  build the session on one thread and hand it to its main thread before
  the first plugin loads.

**Blocking work is a `Send` job the host places.** A plugin open, a
plugin's parameter read, a state snapshot, a SoundFont build and an export
render are each a value with `fn run(self) -> Done`. The host runs it on any
thread and hands `Done` back to the session on the control thread.

- The rule is **no blocking call on the control thread**. The session never
  spawns a thread for a job; the host decides where it runs.
- `bevy-tutti` places jobs on `AsyncComputeTaskPool`, and keeps its own
  scheduling rules there, such as `MAX_CONCURRENT_LOADS`
  (`plugin_host/load.rs:72`). A CLI calls `run()` inline.
- Each job has a `_blocking` convenience on `Session` that does both
  halves.

### 4.3 Lifecycle

**Create, and start or stop the device.**

```rust
// Offline: no stream, no tutti-cpal.
let mut s = Session::builder()
    .outputs_floor(2)                                  // TuttiPlugin::outputs today
    .render_workers(0)                                 // one per core
    .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(512))?;

// With the sound card (`device`): two halves.
let (mut s, mut device) = Session::builder()
    .midi_hardware(true)                               // `midi-hardware`
    .build_device(DeviceSelector::Default)?;           // build_on + assemble
s.restart(&mut device, DeviceSelector::Name("USB Audio".into()))?;  // rerate, §2.1
device.stop(); device.start()?;
```

- `build_offline` builds everything but the stream. The host pulls blocks
  with `s.render(&mut out)` (it calls `Engine::process`, in `tutti-core`), so
  the offline path never links `tutti-cpal`.
- `build_device` opens the device first, reads its `OutputSpec` and builds
  the graph at that rate, as `build_into` does.
- Today's device-free path (`AudioEngine::from_spec` on a
  `ManualStreamDriver`) stays, as `build_device_on(OutputSpec, driver)`, for
  tests that want the callback state itself.

**Add and wire nodes.**

```rust
let (pad, _) = s.graph().insert(PolySynth::new(SynthConfig::default())?);
let (filter, _) = s.graph().insert(SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(1_000.0), Q(0.7)));
s.graph().set_source(filter, 0, GraphSource::node(pad, 0));
s.graph().set_outputs_from(filter);
s.graph().crossfade(filter, other_filter, Seconds(0.005))?;  // retried by update() while Busy
```

**Params.** The units rule wants a typed setter. `ParamKey<U>` already
exists (`tutti-types` `unit_param.rs:204`), with one constant per
`UnitParam`, so:

```rust
s.graph().set(filter, ParamKey::<Hz>::CUTOFF, Hz(800.0));  // through the node's ParamSet
```

The untyped `set_param(node, UnitParam, f32)` stays for plugin and dynamic
addresses.

**Modulation.** These are declarations owned by the session, keyed by ids
it mints. The full example is §5.1.

```rust
let lfo = s.mods().lfo(Lfo::sine().rate(BeatDuration(0.5)));    // a SourceKey; builders from tutti-mod
s.mods().range(ParamRef::unit(filter, UnitParam::Cutoff), 0.0, 1_000.0);
s.mods().route(lfo, ParamRef::unit(filter, UnitParam::Cutoff), Depth(0.3));
```

**Plugins.** The handle is the node; editor windows are the host's own. A
plugin's parameter edits arrive through `PluginParamValues` and become
knob writes at the next update.

```rust
let job = s.plugin_load(PluginLoadRequest { id: PluginId::from_path(path), state: None }); // &self; Send; rate from the session
let done = job.run();                        // any thread
let synth = s.add_plugin(done)?;             // main thread: AudioNode; controls and meter captured
```

**MIDI.**

```rust
let live = s.midi().live_input(synth)?;           // MidiQueueNode fed into synth; returns a MidiSender
s.midi().play_clip(synth, events)?;               // one MidiClipNode per target, updated in place
s.midi().route(MidiRouteRule::any_channel().to(synth)); // `midi-hardware`
```

**Transport.** `s.transport()` returns today's `Transport` handle, and
`MotionFsm::schedule(At, ..)` is unchanged.

**Export.** The session has its own request, free of `Entity` and `World`.
It is today's `bevy-tutti` request with the two Bevy-bound fields
replaced:

```rust
pub enum ExportSource { Mix, Node(AudioNode) }       // was Node(Entity)
pub struct ExportRequest {
    pub source: ExportSource,
    pub target: ExportTarget,                        // File { path, normalize } | Buffers, moved
    pub config: ExportConfig,                        // tutti-export's, unchanged
    pub clock: ExportClock,                          // moved
    pub edit: Option<PrepareEdit>,                   // the edit half only: Box<dyn FnOnce(PreparedGraph) + Send>
    pub latency_from_graph: bool,
    pub tail_from_graph: Option<Samples>,
}
impl ExportRequest {
    pub fn file(path: impl Into<PathBuf>, config: ExportConfig) -> Self; // Mix, frozen clock
    pub fn on_timeline<T>(self, timeline: Arc<T>) -> Self;
    pub fn with_edit(self, edit: PrepareEdit) -> Self;
    pub fn trim_reported_latency(self) -> Self;
    pub fn with_reported_tail(self, cap: Samples) -> Self;
}
```

```rust
let job = s.export(ExportRequest::file("out.wav", config))?;  // gather here; announces Offline (below)
let done = job.run();                 // build, edit, render: any thread
s.finish_export(done)?;               // main thread: drop the leftovers, release the token
```

- The Bevy `ExportRequest` component stays in `bevy-tutti`. Its observer
  runs the `PrepareGraph` hook's `Fn(&World)` half on the main thread,
  resolves `ExportSource::Node(Entity)` to its `AudioNode`, and builds the
  session's request with the returned edit. `NodeNames` stays too: it maps
  the `AudioNode` an `ExportError` names back to an entity and its `Name`.
- **One export at a time.** `s.export` returns `Error::ExportInFlight`
  while another is in flight. `bevy-tutti` keeps its queue of request
  entities, as today.

**The render mode is a level, not an edge.** Today's rule
(`plugin_host/render_mode.rs:19-33`, `:64-99`) is "Offline while any export is in
flight", evaluated on the control thread every frame. It covers
completion, failure, cancellation and a despawned request with one
observable. The session keeps that rule:

- `s.export` mints an `ExportToken`. It is a clone of an `Arc<()>` the
  session holds, and it rides inside the `ExportJob` and then its `Done`.
  An export is in flight while any token lives.
- `Phase::RenderMode` (§4.4) computes the wanted mode from that level:
  Offline while a token lives, Realtime otherwise. It tells each plugin
  per `needs_announcement` (in `tutti-plugin`): on a change
  of mode, or if the plugin was never told. A plugin loaded mid-bounce is
  told Offline on its first update.
- `s.export` runs that phase itself before it returns. Offline is
  announced on the control thread before the job can render a block.
- `finish_export` drops the token and runs the phase again. A job or `Done`
  dropped without `finish_export` (a cancelled task, a panic on the
  worker) drops its token, and the next `update()` restores Realtime.
- `finish_export_unannounced` drops the leftovers and the token and leaves
  the announcement to the next `Phase::RenderMode`. A host that hands one
  export's slot to the next in the same frame uses it, so the plugins see
  no Realtime between the two. Bevy's `poll_exports` does: its
  `plugin_render_mode` runs after `start_exports`, and the level it reads
  is Offline again. `finish_export` and `export_blocking` keep announcing.
- A token leaked past `EXPORT_STALL_WARNING` (an hour) holds Offline for
  good; `Phase::Collect` warns about it once per export (`tracing::warn!`),
  on the `now` the host passes. The clock restarts when an export starts,
  so a queue handed from one export to the next within a frame is not one
  long stall.
- Nothing is announced on a worker, and nothing waits for `Done` to come
  back.

### 4.4 `update()`, and the order is data

Bevy runs reconciliation as `GraphReconcileSystems`:
`Spawn → Params → Despawn → Compensate → Commit`, plus the plugin, MIDI
and modulation systems ordered around them. A headless host gets the same
order from one call:

```rust
pub const PHASES: &[Phase] = &[
    Phase::Collect,          // editor.collect (frees retired units), finished jobs promoted
    Phase::Plugins,          // health, latency poll, GUI edits → knobs, snapshots due
    Phase::RenderMode,       // Offline while an export token lives (§4.3)
    Phase::Modulation,       // collect + apply
    Phase::Commit,           // commit iff edited, publish compensation, retry crossfades
    Phase::AfterCommit,      // modulation re-apply while unresolved, resyncs
    Phase::MidiOut,          // hardware out drains, clock out
    Phase::Poll,             // device faults, through the session's clone of the Device's Arc<StreamFaults>
];

/// What one phase reads besides the session.
pub struct PhaseCx<'a> {
    pub now: Instant,
    /// `None`: the session's own declarations (`s.mods()`).
    pub mods: Option<ModInput<'a>>,
}
pub struct ModInput<'a> {
    pub declarations: Declarations<'a>,   // tutti_mod's: borrowed slices
    pub keys: &'a dyn ModKeys,            // §4.6
}

impl Session {
    /// Every phase in `PHASES` order, with `Instant::now()` and the session's own declarations.
    pub fn update(&mut self) -> Vec<SessionEvent>;
    pub fn run_phase(&mut self, p: Phase, cx: PhaseCx<'_>, out: &mut Vec<SessionEvent>);
}
```

**Bevy runs the phases one system at a time.**

- Bevy's `Spawn`, `Params` and `Despawn` have no headless phase: a
  headless insert, param write or removal is a direct call between updates.
  `ModulationSystems::Apply` (between `Spawn` and `Params` today) maps to
  `Phase::Modulation`, and `ModulationSystems::AfterCommit` to
  `Phase::AfterCommit`.
- `bevy-tutti` builds its system-set chain *from* `PHASES`. Its schedule
  test asserts the chain and the constant agree, so the order cannot drift
  between the two hosts.

**The modulation phases take the declarations as an argument.** The session
does not cache a copy.

- Bevy's declarations live in ECS queries, and today they are collected
  twice per frame, once in `apply` and once in `after_commit`
  (`reconcile.rs:416-444`). Both collects borrow the World.
- So `bevy-tutti`'s two modulation systems each collect into an owned
  `Collected`, as today, and pass it with `Declared` as the `ModInput`.
- A cached copy inside the session would be a mirror the adapter must
  reconcile every frame. CLAUDE.md says to derive at the point of use
  instead, and the point of use is the phase.
- A headless session passes `None`, and the phase reads `s.mods()`. A
  session has one declarer: passing `Some` while `s.mods()` holds
  declarations is refused with a `SessionEvent`.

**`update()` commits iff the graph was edited.** The session has one edit
flag. It is today's `GraphRuntime::edited` (`runtime.rs:866`), set by every
edit made through the session.

- `GraphDirty` goes. Today it separates "the frame's commit is waiting"
  from "a host's own staged edit through `AudioGraphRes`", and
  `after_commit` commits the second early (`reconcile.rs:428-436`).
- With the flag as the only signal, there is no second state. An edit
  through `AudioGraphRes` is a session edit, so the frame's commit sends it.
- Edits still coalesce: one commit per update at most, plus the
  re-apply commit in `AfterCommit` while a target is unresolved.
- A refused commit (`Retry`) leaves the flag set, and the next update
  retries, as `GraphDirty` does today.
- `bevy-tutti` gives up one behaviour: a host can no longer stage an edit
  through `AudioGraphRes` that the frame's commit leaves alone. The
  CHANGELOG records it.
- Two tests pin the choice (§9.4).

**Thresholds are times, not counts.** A Bevy frame is about 16 ms. A
headless loop may call `update()` back to back, which would shrink a
frame-counted window to microseconds. Each threshold becomes a `Duration`,
measured on `PhaseCx::now`:

- **The plugin-death debounce.** It was 3 frames (`health.rs:61-67`
  before the move), sized to outlast the crash flag's publish from the bridge's
  thread. The rule is now (`tutti_plugin::Liveness`): dead after at
  least 3 consecutive unhealthy observations spanning at least 50 ms (three
  60 Hz frame intervals). In a Bevy app that is the first unhealthy frame
  at least 50 ms after the first: the 4th at a steady 60 Hz, later under
  jitter. A healthy reading resets it. The count still guards against one racy observation; the time keeps
  the window when updates are fast.
- **The state snapshot** of a healthy plugin: every 8 s, today's 512 frames
  at 60 Hz (`health.rs:76`).
- **The unresolved-target warning:** after 4 s, today's 240 frames
  (`reconcile.rs:54`).

They are builder options, with those defaults. A test passes its own `now`
through `run_phase`, so none of them needs a sleep.

### 4.5 Wiring stays declared

"Wiring is declared, not called" is kept, and it needs no Entity layer.

- **The declaration is the spec.** `Editor`'s `spec_mut().topology` is a
  `Topology` value, and `set_source` writes one port of it.
- **The commit is the compile.** Nothing is spliced into a running graph,
  and PDC is the compiler's.
- Edits between two updates coalesce into one commit (§4.4).
- **Partial declarations behave as they do today.** A port the host never
  writes keeps its value. In Bevy, the rebuild writes only the ports a
  `PortSources` declares.

Why the ECS layer exists: a *component* needs to be re-bound to a new node
and re-derive its wires (`Changed<AudioNode>`).

Why a headless host does not need it:

- Its handle is the `AudioNode` itself, stable across crossfades.
- Imperative calls *on a value that compiles at a boundary* are what
  "declared" means here.

**Event feeds are keyed by the sink's `AudioNode`.** Today `EventFeeds` is
keyed by the sink's `Entity` (`graph/events.rs:80`), and an entity survives
a re-bind. A node key does not. So:

- `Graph::move_feeds(from: AudioNode, to: AudioNode)` moves every feeder's
  sources from one sink to another.
- `bevy-tutti`'s `Changed<AudioNode>` path calls it with the entity's old
  and new nodes when it re-derives the entity's wires. The old node is the
  one `LiveGraph` last recorded.
- The source side needs nothing new. Each feeder (a clip, a route rule, a
  plugin's automation) re-sets its own sources when its node changes, as it
  does today.

Two things are deliberately not offered:

- a whole-`Topology` replace, since it would fight Bevy's partial
  declarations when both hosts share one session;
- direct `Editor` access. `Graph` stays opaque, as `AudioGraphRes` is, so
  the edit/commit boundary stays visible.

### 4.6 Modulation keys

`tutti_mod`'s declarations key targets by `TargetKey { node: u64, param }`,
sources by `SourceKey(u64)` and routes by `RouteKey(u64)`. Today the u64 is
an `Entity`'s bits, and `ModHostOver` decodes it (`graph/runtime.rs:998`,
`:1020`).

The session takes a resolver instead. Its boundary in one sentence: it
resolves a host's modulation keys to graph nodes.

```rust
pub trait ModKeys {
    fn node(&self, target: u64) -> Option<AudioNode>;
    /// Plugin parameter `id`'s value on `target`'s node: `None` while not read yet,
    /// `Some(None)` for a parameter it does not list. Goes later (below).
    fn plugin_value(&self, target: u64, id: u32) -> Option<Option<f32>>;
    fn feed(&self, source: SourceKey) -> Option<Feed>;
}
```

- **It replaces `ModEntities`** (`modulation/reconcile.rs:174-182`) one for
  one, `plugin_value` included (`:179`).
- **It lands with the session (§9.4), not with modulation (§9.5).**
  `GraphRuntime::modulate` takes `&dyn ModEntities` (`runtime.rs:566-570`),
  and `GraphRuntime` moves into the session. So the seam moves with it, and
  `bevy-tutti`'s `Declared` implements `ModKeys`, keeping the
  `Entity::try_from_bits` decode on its side.
- **`plugin_value` is temporary.** Until plugins move into the session
  (§9.6) the plugin mirror (`PluginParamValues`) is a component on the
  plugin's entity, so only the host can read it. Then the session owns the
  mirror, keyed by `AudioNode`, reads the value itself once `node`
  resolves, and the method goes.
- **Headless:** `s.mods()` keeps its own declarations. It mints
  `SourceKey`s and `RouteKey`s from a counter, and keys targets by the
  `AudioNode`'s `NodeKey` bits, so the session's own resolver applies.
  There is no new id type: `tutti_mod::SourceId(pub u32)` (`program.rs:46`)
  is the compiler's own index and stays out of the API.
- **Bevy:** `Declared` implements `ModKeys` over entities. The re-apply
  loop, the resync and the warning are the session's either way.

### 4.7 Errors

`tutti_session::Error` covers the build, the device, the graph (commit,
reprepare), plugins, MIDI and export.

**Refusals are reported in the host's keys.** A modulation refusal names
the `TargetKey`, `SourceKey` or `RouteKey` the host declared, never a
session `ParamRef`. Those keys are the host's own minting, so each host can
map them back:

- `bevy-tutti`'s `ModulationRefused` decodes them to the route or target
  entity, as today (`modulation/reconcile.rs:109-185`);
- a headless host compares them with the keys `s.mods()` returned.

The other host ids stay in the host: `ExportError::NotForkable` names an
`AudioNode`, and `bevy-tutti`'s `NodeNames` maps it to an entity and its
`Name`.

## 5. Examples

### 5.1 Doc 014 §8.1, headless

```rust
use tutti::prelude::*;

let (mut s, _device) = Session::builder().build_device(DeviceSelector::Default)?;
let (filter, _) = s.graph().insert(SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(1_000.0), Q(0.7)));
let (pad, _) = s.graph().insert(PolySynth::new(SynthConfig::default())?);
let (kick_bus, _) = s.graph().insert(ChannelSumNode::new(1, ChannelLayout::STEREO));
s.graph().set_source(filter, 0, GraphSource::node(pad, 0));
s.graph().set_outputs_from(filter);

let cutoff = ParamRef::unit(filter, UnitParam::Cutoff);
let gain = ParamRef::unit(pad, UnitParam::Volume);
let mut m = s.mods();
m.range(cutoff, 0.0, 1_000.0);
m.range(gain, 0.0, 1.0);
let wobble = m.lfo(Lfo::sine().rate(BeatDuration(0.5)));
m.route(wobble, cutoff, Depth(0.3));
let kick = m.follower(Follower::default(), kick_bus);
m.route(kick, gain, Depth(-0.6)); // sidechain

s.transport().play();
loop { for e in s.update() { log(e) } sleep(frame) }
```

- This is line for line the Bevy example, with `ParamRef` taking an
  `AudioNode` where Bevy takes an `Entity`. The session's `ParamRef` is
  `{ node: AudioNode, param: ParamAddr }`.
- `m.follower` takes `tutti-mod`'s `Follower` (attack and release, the
  `Source` half) and the node it follows (the feed). Bevy's `Follower`
  component keeps its `input: Entity` and wraps the same builder.
- The 250 lines of `headless_modulation.rs` (the hand-built `ModHost`, the
  `Declarations` and `apply`) become the `update()` call.
- §9.5 ports `tests/mod_lanes.rs`'s
  `the_8_1_example_renders_its_analytic_values` onto an offline session,
  with its own probe nodes and the same analytic figures.

### 5.2 Load a plugin, play a MIDI file, export to WAV

```rust
use tutti::prelude::*;

let mut s = Session::builder()
    .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(512))?;

let job = s.plugin_load(PluginLoadRequest { id: PluginId::from_path("Synth.clap"), state: None });
let synth = s.add_plugin(job.run())?;
s.graph().set_outputs_from(synth);

let file = MidiFile::read("song.mid")?;                 // tutti-midi-file: sniffs SMF vs Clip File
let tempo = file.tempo().unwrap_or(Bpm(120.0));         // the tempo at beat 0; an SMF always has one
s.midi().play_clip(synth, file.timed().map(TimedMidiEvent::from))?;
s.transport().set_tempo(tempo);

let mut config = ExportConfig::default();
config.render.sample_rate = SampleRate(48_000.0);
// `end()` is a `Beat`. One tempo is right only for a file that never changes
// it: in general the length integrates over `file.tempo_map()`. An f64
// converter that does (`BeatDuration::to_seconds` is f32 `Seconds` at one
// tempo) does not exist yet: §9.6's to add.
config.render.duration_seconds = file_seconds(file.end(), &file.tempo_map());
let job = s.export(
    ExportRequest::file("song.wav", config)
        .on_timeline(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            tempo,
            sample_rate: SampleRate(48_000.0),
            ..Default::default()
        })))
        .trim_reported_latency()                         // RenderConfig::latency, from the plan
        .with_reported_tail(Samples(48_000 * 10)),       // RenderConfig::tail, the plugin's declared tail
)?;                                                      // the plugin hears Offline from here
s.finish_export(job.run())?;                             // and Realtime from here
```

The request fills `tutti-export`'s `RenderConfig`, which stays a pure value:
"the caller knows, so the caller says" (`tutti-export` `config.rs:50-59`).
The session is that caller: `trim_reported_latency` and
`with_reported_tail` ask the rendered graph, as `bevy-tutti`'s request does
today (`export/request.rs:116-120`).

Three engine changes this example needs, each a **(c)** move in §9:

- `MidiFile::read`, with `timed()` yielding `(Beat, MidiEvent)` for both
  containers, plus `tempo()` and `end()` (landed; `tempo()` is the
  tempo at beat 0 as an `Option<Bpm>`, since a Clip File may declare none,
  and `tempo_map()` holds every change);
- `TimedMidiEvent: From<(Beat, MidiEvent)>`, which exists;
- `needs_announcement`, for the render-mode phase.

## 6. How `bevy-tutti` rebases

| Module | Today | After |
|---|---|---|
| `engine/` | builds everything into the World | `build_into` calls `SessionBuilder::build_device`; the `Session` goes into `AudioGraphRes`, the `Device` in with `insert_non_send`; `restart` is a `Session::restart(&mut device, ..)` call inside the NonSend dance |
| `graph/runtime.rs` | `GraphRuntime` | deleted; `AudioGraphRes(Mutex<Session>)`, its methods delegate |
| `graph/commit.rs`, `latency.rs` publish | logic | one main-thread system calling `run_phase(Commit)` |
| `graph/schedule.rs` | `GraphReconcileSystems`, `GraphDirty` | the sets, built from `PHASES`; `GraphDirty` deleted (§4.4) |
| `graph/topology.rs`, `wire.rs`, `spawn.rs`, `despawn.rs`, `param.rs` | glue | unchanged but for two calls: the crossfade calls `Graph::crossfade`, the re-bind calls `Graph::move_feeds` |
| `graph/events.rs` | `GraphNode`, `EventFeeds` | `GraphNode` becomes `tutti_session::SessionNode`; feeds keyed by `AudioNode`, in the session |
| `graph/pump.rs` | thread loop + systems | systems only, over `tutti-io`'s `Recorder` (done) |
| `modulation/` | declare + reconcile | components and `Mods` stay; `Declared: ModKeys`; the two systems collect and call `run_phase` with a `ModInput` |
| `plugin_host/` | load, health, latency, params, render mode, windows, scan | load, param read and snapshot are jobs on the task pool (two loads at a time); health, latency, params and render mode are session phases; windows, scan and catalog stay |
| `midi/` | nodes, sequence, routes, hardware out, files | components call `Session::midi()`; the asset loader wraps `MidiFile`; MIDI-CI negotiation and device state stay |
| `sampler/`, `soundfont.rs` | logic + assets | assets, systems and pending tasks only |
| `export/` | pipeline + request | the `ExportRequest` component stays; its observer translates it into the session's request (§4.3) and places the job on the task pool; `poll_exports` calls `finish_export` |

**`bevy-tutti` does not re-export the session.** A Bevy app sees
`bevy-tutti`'s own API. Two paths still need the session's names:

- **`param_graph_node!`** expands to `impl tutti_session::SessionNode for
  $ty`. The macro names the trait through `bevy-tutti`'s existing hidden
  `$crate::__private` module (`lib.rs:212-213`), which gains
  `pub use tutti_session::SessionNode;`. `#[doc(hidden)]` keeps it out of
  the API, so the decision holds, and a host using the macro adds no
  dependency.
- **A hand-written `SessionNode` impl** (a node with controls the macro
  does not cover) needs the trait by name. That host adds `tutti-session`
  as a direct dependency, as a host that writes `ParamNode` depends on
  `tutti-graph` today.

**Stays Bevy-only:**

- the ECS vocabulary: `AudioParam<U, P>`, `PortSources`, `MasterSources`,
  `Mods` and the mod components, the requests;
- the asset loaders and their systems;
- the plugin editor windows and live resize;
- the plugin load cap;
- MIDI-CI negotiation;
- `GraphReconcileSystems` (built from `PHASES`);
- Reflect registration;
- the `On<Remove, AudioNode>` observer.

`bevy-tutti` keeps its direct edges to the engine crates for their `bevy`
features, and adds one to `tutti-session`. Its features forward to
`tutti-session`'s: `midi` → `tutti-session/midi`, and so on. `device` is
always on for `bevy-tutti`.

## 7. The umbrella, for all the tutti crates

`crates/tutti` is the answer to "an umbrella crate … for all the tutti
crates". It already re-exports every engine crate under an alias. It
gains:

```rust
#[cfg(feature = "session")]
pub use tutti_session as session;          // the API layer, on by default
pub mod prelude {
    #[cfg(feature = "session")]
    pub use tutti_session::{Session, SessionBuilder, SessionEvent, ParamRef, Phase};
    // ...today's prelude, unchanged
}
```

**Features.** The user decided the layer is on by default.

```toml
[dependencies]
tutti-session = { path = "../tutti-session", default-features = false, optional = true }

[features]
# `default = ["session"]`: the API layer is what this umbrella is for. It is
# the one member whose default is not empty, and it costs nothing a feature
# did not already ask for: `tutti-session` itself has `default = []`, so the
# default build adds the session's own code and nothing else. Set
# `default-features = false` for the raw crates alone.
default = ["session"]
session = ["dep:tutti-session"]
device = ["dep:tutti-cpal", "tutti-session?/device"]
midi = ["dep:tutti-midi-types", "dep:tutti-midi-file", "dep:tutti-midi-runtime",
        "tutti-core/midi", "tutti-session?/midi"]
# ...each feature forwards the same way
```

- Each `tutti` feature enables the member crate as today, *and*
  `tutti-session`'s feature of the same name with `?`. A host without the
  session gets no session edge from a feature.
- **No ALSA by default.** `tutti` depends on `tutti-session` with
  `default-features = false`, and `tutti-session` has `default = []`. So
  `default = ["session"]` never turns on `device`, and a headless default
  build links neither `tutti-cpal` nor ALSA.
- **The cost of the default** is the session's own code. Its always-on
  edges (`tutti-types`, `tutti-graph`, `tutti-core`, `tutti-nodes`) are
  already always-on edges of `tutti`; `tracing` is the one new crate.
- The comment in `crates/tutti/Cargo.toml` that reads "`default = []`, as
  on every other member" is replaced by the one above.
- `full` is unchanged, and the plugin formats stay opt-in.

**Organisation.** The root has the aliased crate modules (`tutti::core`,
`tutti::graph`, `tutti::session`, …) and one prelude.

The prelude is the session plus the vocabulary a session host writes
daily: units, `UnitParam`, `ParamKey`s, `AudioNode`, the transport's `At`,
the node constructors of the enabled DSP crates. Nothing in it is reached
through a second path.

**`no_logic.rs`** is unchanged. It skips attribute lines and allows
`pub use`, `pub mod` and `}`, so the `#[cfg]`-gated `pub use tutti_session
as session;` passes it.

**`check-canonical-paths.sh`:**

- The chained gate already covers a chain through `tutti_session`.
- Add `tutti_session` to the per-module gate invocation in CI, as
  `tutti_graph` is, since its modules are private.

## 8. Rules

The rule must stay mechanically enforced. Four checks enforce it, each
with its failure message naming the alternative.

1. **`tutti` holds no code.** `no_logic.rs`, unchanged.
2. **`tutti-session` is Bevy-free.** `scripts/check-bevy-free.sh`, run in
   `just ci` and CI, runs

   ```bash
   cargo tree -p tutti-session -e normal --all-features --prefix none
   ```

   and fails on any line starting `bevy` (`bevy`, `bevy_*`). It also fails
   if `crates/tutti-session/Cargo.toml` declares a `bevy` feature.
   - **Why `cargo tree`, not `cargo metadata`.** `cargo metadata` resolves
     features once for the whole workspace, so `bevy-tutti`'s `bevy`
     features leak into every member's closure. Probed on `origin/main`
     (489649fe): `tutti`'s normal closure in `cargo metadata` contains
     `bevy_ecs`, `bevy_reflect` and `bevy_tasks` even without
     `--all-features`. `cargo tree -p tutti --all-features -e normal`
     resolves only `tutti`'s features and lists no Bevy crate.
   - **Mutation (run on `tutti` as the probe):** add `bevy_ecs` as an
     optional dependency behind a feature. `cargo tree --all-features`
     then lists `bevy_ecs v0.19.1` and the check fails. Without
     `--all-features` it lists none, which is why the flag is required.
   - A script rather than a test, because a test that shells out to
     `cargo` inside `cargo nextest` contends for the build lock.
3. **`bevy-tutti` holds no engine composition.** `scripts/check-adapter.sh`,
   run in `just ci` and CI. It fails if `crates/bevy-tutti/src`, tests and
   comment lines excluded, names any of these types as a whole word:
   - `Editor`, `Executor` (the graph's halves: `Session` and `Graph` own
     them);
   - `Modulation` (the reconcile: `run_phase(Modulation)`);
   - `TuttiDriver`, `AudioCallbackState` (the device: `Device`);
   - `Engine` (the renderer: `SessionBuilder`);
   - `RenderGraph` (export: `Session::export`).

   Method names such as `.commit()` and `.apply(` are not checked: they
   cannot tell an `Editor` from a `Session`. A type name can, and no
   composition happens without naming one. The deny-list lives in the
   script, with each entry's owner beside it.
   - Mutation: add `fn f(e: &mut tutti_graph::Editor) { e.commit(); }` to a
     system module. The script fails and names `Session`.
4. **`tutti-session` is not an umbrella.** The same script fails if
   `crates/tutti-session/src` holds a whole-crate re-export: `pub use
   tutti_x as …;`, `pub use tutti_x;` or `pub use tutti_x::*;`. Single-item
   re-exports are allowed.
   - Mutation: add `pub use tutti_mod as modulation;` to the crate root.
     The script fails and names `tutti` as the umbrella.

`no_logic.rs` cannot tell "logic" from "glue" in general. The checks pin
the one thing that failed before: both hosts call the same entry points,
so no umbrella owns the composition, and the logic crate cannot become a
third umbrella.

**CLAUDE.md edits** (each with the step of §9 that makes it true):

- **Workspace tree** (§9.4): add `tutti-session` with the line "Bevy-free
  API layer: the cross-crate composition both umbrellas use."
- **Bullets** (§9.7):
  - Replace "A Bevy app depends on `bevy-tutti`; a headless consumer
    depends on `tutti`. `bevy-tutti` does not depend on `tutti`." with the
    same, plus: "Both use `tutti-session`. It holds only code that names
    two or more engine crates, or the order they run in, has no `bevy`
    feature (`scripts/check-bevy-free.sh`) and re-exports no crate whole.
    `bevy-tutti` reaches the engine's composition only through it
    (`scripts/check-adapter.sh`), and does not re-export it."
  - Add `tutti-session` to "keeps every module private" (§9.4).
- **"Build `plugin-server` first"** (§9.6): add `tutti-session`'s plugin
  and export tests to the list. They fail without the binary, with the
  same rule: fail, don't skip.
- **"Rules that came from bugs"** (§9.7): replace "The `tutti` umbrella
  holds no code. Anything that wants to live there belongs in the crate
  that owns what it wires." with: "Neither umbrella holds engine logic. A
  piece with one owner goes to that crate; a piece composing several goes
  to `tutti-session`."
- **"Wiring is declared, not called"** (§9.7): add one line saying a
  headless `Session` declares through its own spec, keyed by `AudioNode`,
  and that `update()` commits iff the graph was edited.
- **Commands** (§9.7): add `scripts/check-bevy-free.sh` and
  `scripts/check-adapter.sh` beside the other two scripts.

**engineering-notes edits** (§9.7):

- **"There are two umbrellas…":** add a paragraph recording this design,
  that is, why (a) and not (b). Correct "device bootstrap in `tutti-cpal`,
  graph logic in `tutti-core`", which §3(c) shows would invert edges.
- **"Workspace Structure":** add the tree entry, and change the member
  count.
- **"Wiring is declared, not called":** add the headless paragraph of §4.5.
- A note on why `Device` is `!Send` while `TuttiDriver` is `Send` (§4.2).

**doc 013:** a pointer from its "Flip the adapter" `bevy-tutti` items to this doc. The
graph contract does not change.

## 9. The migration

Each step is green alone, `bevy-tutti`'s full suite included.

- Every new test carries its mutation record.
- Analytic figures only: no render compared against another reference
  render.
- The `audio-automation` pin and the bevy 0.19 line are unaffected.

§9.1–§9.3 are **(c)** moves and can land in any order. §9.4 needs them.
§9.5–§9.7 are sequential.

### 9.1 MIDI and sample pieces to their owners

**Scope:**

- `tutti-midi-file`: `MidiFile` (sniffed SMF or Clip File, today's
  `MidiFileContents`) with `timed()` → `(Beat, MidiEvent)` for both, plus
  `tempo()` and `end()`. The SMF half is new (midly →
  `MidiEvent::from_midi1_bytes`).
- `tutti-midi-runtime`: the route-rule compile (rules → `input_ports`, with
  the fallback) as a pure function.
- `tutti-sampler`: `memory_voice` and `voice_width`.
- `bevy-tutti` calls them.

**Properties:**

- An SMF and a Clip File of the same notes yield the same
  `(Beat, MidiEvent)` list, velocity widened. (Only channel-voice messages
  compare: a clip's `timed()` keeps its Flex Data, a Set Tempo or time
  signature, while an SMF's meta events are not MIDI messages and are not
  in it.)
- A note-on at tick 480 of a 480-ppq file lands at `Beat(1.0)`.
- The route compile is total over the rule set, and its fallback holds only
  unrouted channels.
- The existing `voice_width` and memory-voice tests move with their code.

**Exit:** `bevy-tutti`'s MIDI file and route tests pass unchanged against
the moved code.

The exit is met with the four decode tests moved rather than left
unchanged: their assertions are equivalent, with the types renamed
(`MidiFileContents` → `MidiFile`, `MidiFileLoaderError` → `Error`). The
route, loader-extension, write and prelude tests are unchanged.

**Landed.** What moved, old → new:

- `bevy_tutti::midi::MidiFileContents` → `tutti_midi_file::MidiFile`, its
  sniff-and-decode (`MidiFileAsset::from_bytes`) → `MidiFile::from_bytes`,
  the extension list → `MidiFile::EXTENSIONS`, and `MidiFileLoaderError` →
  `tutti_midi_file::Error` (which gained `Clip` and `UnknownFormat`). The
  SMF variant is `Smf(SmfFile)`: its tracks and its merged stream come from
  one `midly` parse, behind accessors, so the two cannot disagree; `timed()`
  reads the stream, because the paired notes alone would drop every
  controller. `tempo()` is the tempo at beat 0 as `Option<Bpm>` (an SMF
  always answers: a Set Tempo at tick 0, else its spec's 120; a Clip File
  answers only with a Set Tempo at beat 0). `tempo_map()` lists every Set
  Tempo in time order. A file whose tempo changes needs the map: one tempo
  does not give its length in seconds, and §9.6's f64 duration converter
  must integrate over the map. `end()` is the last event's `Beat`.
- The route compile → `tutti_midi_runtime::compile_routes` over
  `RouteRule { channel, targets }`; `bevy-tutti`'s `rebuild` filters the
  armed rules, compiles, and writes the feeds.
- `memory_voice`, `voice_width` → `tutti_sampler`, in a private
  `voice::build` module; `bevy_tutti::prelude` re-exports them from there.

Test dispositions: `voice_width`'s and the memory-voice test moved with
their code. The four decode tests of `bevy-tutti`'s `midi/file/tests.rs`
(an SMF to its tracks, a clip by its magic, neither format, a truncated SMF)
moved to `tutti-midi-file/tests/midi_file.rs`, since they test the decode,
not the loader; the loader-extension and write tests stay unchanged, as do
`tests/midi_routes.rs` and `tests/prelude_surface.rs`.

### 9.2 Plugin, SoundFont and mod-source pieces to their owners

**Scope:**

- `tutti-plugin`:
  - `PluginParamValues`, with the read split in two: `start(&handle)`
    installs the callback and returns the mirror plus a `Send`
    `ParamRead` job, and `land(values)` fills the mirror. Nothing in
    `tutti-plugin` spawns.
  - The liveness debounce, `Liveness::observe(healthy, now: Instant) ->
    Verdict`: dead after at least 3 consecutive unhealthy observations
    spanning at least a window (default 50 ms), reset by a healthy one.
  - The render-mode announcement rule (`needs_announcement`).
- `tutti-soundfont`: `SoundFontUnit::with_preset(font, rate, channel,
  preset)`: the rate cast, `SoundFontUnit::new` and `program_change` of
  `soundfont.rs:205-212` and `:266`.
- `tutti-mod`: the `Lfo` and `Macro` builders, and the `Source` half of
  `Follower` (attack, release) and `NoteEnvelope` (ADSR), each producing a
  `Source`. Reflect stays behind `tutti-mod/bevy`.
- `bevy-tutti` keeps the rest: `Follower`'s and `NoteEnvelope`'s followed
  `Entity` and `ModFeed`, the `AsyncComputeTaskPool` spawns of the read and
  the snapshot, and `MAX_CONCURRENT_LOADS`.

**Properties:**

- The debounce declares dead on the 3rd consecutive unhealthy observation
  once 50 ms have passed. Three observations 1 µs apart are not dead; a
  healthy reading resets both the count and the window.
- `ParamRead::run` over a fake handle returns its descriptors' values, and
  `land` makes the mirror ready.
- Each builder's `Source` is pinned field by field.
- A preset unit's first note-on renders that preset: a golden digest on the
  recording target.

**Exit:** `plugin_host/health.rs`'s, `params.rs`'s and
`modulation/tests.rs`'s tests pass, calling the moved types.

**Landed.** As scoped, with these specifics:

- `tutti_plugin::{PluginParamValues, ParamRead, Liveness, Verdict,
  needs_announcement}` at the crate root. `PluginParamValues` is still a
  component on the plugin's entity until §9.6: `tutti-plugin` gained a
  `bevy` feature (off by default, `bevy_ecs` derives only) for it, which
  `bevy-tutti`'s `plugin` enables. Its edits apply through
  `drain_edits(apply)`, which holds them until the read lands. Bevy's read
  task is a separate `PluginParamRead` component, started by
  `read_plugin_params`.
- `Liveness::DEATHS` (3) and `DEFAULT_WINDOW` (50 ms), `with_window`; a
  death is latched. `bevy-tutti`'s `PluginHealth` holds one and feeds it
  `Instant::now()` each frame, so a crashed plugin is written off on the
  first unhealthy frame at least 50 ms after the first (the 4th at a steady
  60 Hz, later under jitter), not on its 3rd. Its adapter tests space their
  frames half a window apart; the rule's own tests, with explicit `now`s,
  are `tutti-plugin`'s. `PluginLiveness` stays as the component's mirror.
- `tutti_mod::{Lfo, LfoRate, Macro, Follower, NoteEnvelope}`, each a
  `Source` through `From`. `bevy_tutti::modulation` re-exports `Lfo`,
  `LfoRate` and `Macro`; its `Follower` and `NoteEnvelope` wrap the
  `tutti-mod` builder in an `envelope` field beside their entity. No
  `Reflect` was added: none of these was reflected before.
- `SoundFontUnit::with_preset` takes a `SampleRate`; `bevy-tutti`'s
  off-thread build now applies the preset too.
- The render-mode truth table moved to `tutti-plugin` with the rule.

### 9.3 The pump to `tutti-io`

**Scope:** `AudioPump` (`graph/pump.rs:135-240`) joins `tutti-io` beside
`Recorder`. If it duplicates `Recorder`'s loop, one of them goes; the
PR decides which by reading both.

**Properties:** the existing pump tests (ending on `ON_EMPTY`, `stop`
joining) run in `tutti-io`.

**Exit:** `bevy-tutti`'s pump systems compile over the moved type.

**Landed.** The two loops were one loop: `AudioPump`'s thread ran
`Recorder`'s `ThreadDriver` body (poll, park on `Starved`, break on
`EndOfStream`, finalize once, join) line for line. `Recorder` stays and
`AudioPump`'s loop goes, because `Recorder` already had what the component
lacked: the width check (`AudioPump::start` could only `debug_assert` it),
the `Drop` finalization, `wait`, and the driver seam its tests count passes
through. What `AudioPump` had that `Recorder` lacked is now `Recorder`'s:

- any `AudioOut<S>` and any element `S`, not only `WavOut` and `f32`;
- `start_with_capacity` (frames per pass, zero taken as one) and
  `ThreadDriver::with_park` (the back-off; `IDLE_PARK` is now public);
- the non-blocking end a frame loop needs: `request_stop`, `is_running`,
  `is_finished` (`RunningPump::is_finished`), and `shutdown(&mut self)`,
  which joins in place and returns `None` once collected.

`bevy_tutti::graph::AudioPump<S>` is a component holding a `Recorder`; its
`start` signature and its systems are unchanged. A width mismatch now starts
no thread and is reported through `PumpFinished` as an `InvalidInput` error
carrying `tutti_io::Error::ChannelWidthMismatch` (new
`From<tutti_io::Error> for io::Error`). `tutti-io` became a required
dependency of `bevy-tutti`, since `graph::pump` is always compiled.

Tests: `bevy-tutti`'s `audio_io_pump.rs` passes unchanged, plus
`a_width_mismatch_is_reported_not_recorded`. Their Bevy-free halves are in
`tutti-io`: `is_finished_reports_a_finite_take_ending_itself_without_blocking`
(ending on `ON_EMPTY`) and `a_requested_stop_ends_a_live_take_without_blocking`
(`stop` joining) in `tests/recorder_thread_driver.rs`, and the capacity and
in-place shutdown tests in `recorder.rs`.

### 9.4 `tutti-session`: graph, bootstrap, commit and the modulation seam

**Scope:**

- The crate, `default = []`, with its features, `tracing` for logging,
  `scripts/check-bevy-free.sh` and the "no whole-crate re-export" check.
- `Graph` (today's `GraphRuntime`), with `modulate` over `ModKeys`
  (§4.6). `bevy-tutti`'s `Declared` implements `ModKeys`, `plugin_value`
  included, and `ModEntities` is deleted.
- `SessionBuilder` with `build_offline`, `build_device` and
  `build_device_on`; `Session` and the `!Send` `Device`;
  `Session::restart(&mut Device, ..)`; `render`.
- The main-thread asserts of §4.2.
- Commit with compensation publication, the crossfade retry, `EventFeeds`
  keyed by `AudioNode` with `move_feeds`, `SessionNode` with the
  engine-node list.
- `PHASES`, `PhaseCx`, `run_phase` and `update()`, with the edit flag
  replacing `GraphDirty`.
- `bevy-tutti`'s `AudioGraphRes` wraps `Session`, and `engine/build.rs`
  calls the builder. `param_graph_node!` names the trait through
  `__private`.
- The `tutti` umbrella re-exports `session`, with `default = ["session"]`,
  the `?` forwarding of §7 and the new `Cargo.toml` comment.

**Porting `runtime.rs`'s tests.** Its three tests
(`every_param_write_path_reaches_a_fork`,
`a_crossfade_during_a_re_prepare_lands_after_it`,
`a_crossfade_on_a_poisoned_graph_is_refused_and_keeps_the_controls`) build
a `bevy_app` `App` (`runtime.rs:1146`, `:1264`), so they cannot move as
they are. Each splits in two:

- The graph property becomes a `tutti-session` test that calls `Session`
  directly, with no App: the same node, the same edits and the same
  asserted figures.
- The ECS path (a spawn, an `AudioParam` write, a mod route reaching the
  fork) stays in `bevy-tutti` as an adapter test over `AudioGraphRes`.
- Each half carries a mutation record. Breaking the property must fail
  the half that owns it.

**Properties:**

- An offline session with a 1 kHz sine renders its analytic RMS.
- A latent node on one branch is compensated on the other, to the sample.
- A rerate from 48 kHz to 96 kHz keeps the published compensation in step
  with the plan.
- A crossfade refused `Busy` during a reprepare lands on a later update.
- **The edit flag:** an edit between two updates is committed by the next
  update; an update with no edit sends no plan (the plan generation is
  unchanged). Mutations: commit unconditionally (the second fails); commit
  only on an external flag nobody sets (the first fails).
- `move_feeds` moves every feeder's sources, and a re-bound `bevy-tutti`
  sink keeps its clip feed.
- `Device` is `!Send`: a `compile_fail` doctest that moves one into
  `std::thread::spawn`.
- `check-bevy-free.sh` and the re-export check, with their mutations
  (§8).

**Exit:** every `bevy-tutti` graph, engine and restart test passes over the
session. `headless_engine.rs` is rewritten on `Session` and is under 20
lines.

**Landed.** `crates/tutti-session`: every module private (`builder`,
`device`, `error`, `graph`, `node`, `session`), `#![forbid(unsafe_code)]`,
`default = []`. What moved, old → new:

| Was (`bevy-tutti`) | Now (`tutti-session`) |
|---|---|
| `graph::runtime::GraphRuntime` | `Graph` (`Session::graph`) |
| `GraphNode` (and its impls for the engine's nodes, the voices, the panners) | `SessionNode`; `param_graph_node!` names it through `__private` |
| `GraphNode::captured` (default: nothing captured) | removed: a Bevy-free trait cannot name `CapturedControls`. The spawn paths bind `CapturedControls::default()` (a plugin's, `for_plugin`), and a host with other controls calls `CapturedControls::bind` |
| `GraphDirty` | the edit flag, `Graph::is_edited`; `update()` and `Phase::Commit` commit iff it is set |
| `EventFeeds` (keyed by `Entity`) | `Graph::{set_feed, feed, move_feeds}`, keyed by `AudioNode`; `remove` purges a node from every feed |
| `ModEntities` | `ModKeys` (`node`, `plugin_value`, `feed`), implemented by `Declared` |
| `engine/build.rs`'s assembly (graph, transport, click, MIDI nodes, streamer, callback state) | `SessionBuilder::{build_offline, build_detached, build_device, build_device_on}` |
| `TuttiDriver` as a resource | `Device` (`!Send`, a non-send resource) |
| `engine/restart.rs`'s re-rate | `Session::{restart, restart_following, restart_on}` |
| `bevy_tutti::Error` | `tutti_session::Error`, re-exported |
| the crossfade retry and compensation publish in `commit_graph` | `Phase::{Collect, Commit}` |

Tests: the three `runtime.rs` tests split as planned. The session halves
are in `tests/graph.rs` (`every_param_write_path_reaches_a_fork`,
`a_crossfade_refused_busy_during_a_re_prepare_lands_on_a_later_update`,
`a_crossfade_on_a_poisoned_graph_is_refused`); the adapter halves are in
`bevy-tutti`'s `graph/tests.rs`. The properties above are
`an_offline_session_renders_a_sines_analytic_rms`,
`a_latent_branch_is_compensated_on_the_other_to_the_sample`,
`a_rerate_keeps_the_published_compensation_in_step_with_the_plan`
(`tests/device.rs`), `an_edit_between_updates_is_committed_by_the_next`,
`an_update_with_no_edit_sends_no_plan`,
`move_feeds_moves_every_feeders_sources` with `bevy-tutti`'s
`a_re_bound_target_keeps_its_clip`, and `Device`'s `compile_fail`
doctest. The engine tests of `engine/build.rs` (the click, the rate, the
voices, `root_width`) moved to `tests/engine.rs` and `tests/graph.rs`;
`the_transport_converts_at_the_sessions_rate` is new, because the click
cannot see the transport's rate. `scripts/check-bevy-free.sh` runs in
`just ci` and CI, with its mutations recorded in the script. After review:
`a_target_removed_then_re_bound_keeps_its_clip`,
`a_keyboard_follows_its_entity_onto_a_node_bound_after_a_removal` and
`a_removal_is_any_threads_and_a_free_is_the_main_threads`, and CI and
`just test-doc` run `cargo test --doc -p tutti-session --features device`
(the `Device` `compile_fail` proof) by name.

Where the code differs from the text above:

- **`bevy-tutti` re-exports single session items** (`SessionNode`,
  `GraphSource`, `Crossfade`, `Device`, `DeviceRestart`, `Error`, …),
  where §6 says it re-exports none and a hand-written impl adds the
  dependency. They are the types its own signatures take, so a host
  naming them adds no dependency; the crate is still not re-exported
  whole.
- **The crossfade retry runs in `Collect`**, not `Commit`: a crossfade
  refused `Busy` is parked and retried once the re-prepare is collected,
  and lands as `SessionEvent::CrossfadeLanded` (Bevy binds its controls
  then, `PendingCrossfades`).
- **Extra builder and restart entry points:** `build_detached(rate)` (no
  engine; the executor stays on this side), `Session::restart_following`
  (takes a closure run with the new `OutputSpec`, which Bevy uses to
  re-rate its disk streamer and `AudioConfig`) and `restart_on` (any
  `StreamDriver`, for the device-free tests).
- **`DeviceRestart` is `Clone`, not `Copy`**, and its `device` is a
  `DeviceSelector`; `TuttiDriver::{restart, restart_with}` take
  `impl Into<DeviceSelector>` (an `Option<usize>` still converts).
- **`Error` is `#[non_exhaustive]`.**
- **A staged edit waits for the next frame.** An edit a host makes through
  `AudioGraphRes` after `commit_graph` is committed by the next frame's
  commit; before, `GraphDirty` could be set and cleared by whichever
  system ran next. Modulation's `after_commit` no longer commits a host's
  staged edit early: it resolves its waiting targets, and commits what
  that built, only while the graph is otherwise clean.
- **`EngineNodes` is inserted only when the click exists** (a detached or
  offline-without-click session has none).
- **A live MIDI input is fed once its entity has an `AudioNode`.** The
  inbound routes, the clips and the live inputs each re-assert their feed
  on the entity's current node when its `AudioNode` changes, so a re-bound
  target keeps all three, also when the old `AudioNode` was removed first
  (`Graph::remove` drops the old node's feeds, so `move_feeds` alone would
  find nothing; a review caught this).
- **`Session::remove` does not assert the main thread** (§4.2): the free
  is guarded at the editor's collect, every path to which asserts.

Deferred: `GraphReconcileSystems` is not yet built from `PHASES` (the sets
run the same steps in the same order by hand), and Bevy does not run
`Phase::Poll` (its device poll stays a system). The modulation reconcile
loop, MIDI wiring, plugins and export are §9.5's and §9.6's.

### 9.5 Modulation and MIDI in the session

**Scope:**

- `Phase::Modulation` and `Phase::AfterCommit` with `ModInput`: the
  re-apply/resync loop and the time-based warning.
- `s.mods()`, minting `SourceKey`s and `RouteKey`s, and the take recorder.
- Refusals keyed by the host's `TargetKey`/`SourceKey`/`RouteKey`.
- `s.midi()`: live input, clip sequencing, routes, hardware out and clock
  out.
- `bevy-tutti`'s two modulation systems collect and call `run_phase`; its
  MIDI systems call the session.

**Properties:**

- §5.1 headless renders `the_8_1_example`'s analytic values on an offline
  session.
- A route to a target placed one update later resolves on that update
  (the re-apply loop).
- A refusal in `bevy-tutti` still names its route entity.
- The warning fires after 4 s of `now`, not after N updates.
- A clip edit updates in place: the node count stays the same.

**Exit:** `tests/mod_lanes.rs` and the MIDI suites pass.
`headless_modulation.rs` moves to `tutti-session/examples` as §5.1.

**Landed.** Two private modules, `modulation` (feature `modulation`) and
`midi` (feature `midi`). What moved, old → new:

| Was (`bevy-tutti`) | Now (`tutti-session`) |
|---|---|
| `reconcile`'s apply and refusal bookkeeping, `after_commit`'s re-apply, commit and resync | `Phase::Modulation`, `Phase::AfterCommit` (`ModInput` in `PhaseCx`); the systems collect `Declared` and call `run_phase` |
| `commit_and_publish` (`graph/commit.rs`) | deleted: the re-apply commits with `Session::commit`, which publishes on `Sent` |
| the unresolved warning after N frames | `SessionEvent::ModulationUnresolved`, after 4 s of `PhaseCx::now` (`UNRESOLVED_WARNING`, `SessionBuilder::unresolved_warning`) |
| `ModulationState` (a resource) | `ModState` (`Session::mod_state`); Bevy's `ModulationState<'_>` is a view, `AudioGraphRes::modulation()` |
| `collect`'s duplicate-range and -automation check | `declare_targets` |
| `AutomationTake`'s recorder | `Take` (Bevy's component wraps it) |
| `sequence::rebuild`'s clip nodes | `Midi::{play_clip, stop_clip, clip, retarget}`; `SequencedClips` maps an entity to the session's clip |
| `attach_live_midi`'s queue node | `Midi::live_input` (a `LiveInput`) and `Midi::retarget` |
| `inbound::route::rebuild`'s compile and `RoutedTargets` | `Midi::set_routes` (host-keyed, with a resolver) and the session's own `route`/`unroute`/`set_route_fallback` |
| `UmpOutRes`, `MidiOutDrops`, `MidiOutRouter`, `drain_receiver_through`, `out_active` | `UmpOut`, `MidiOutDrops`, `MidiOutRouter`, `drain_receiver_through` (re-exported by `bevy_tutti::midi`); `Midi::{set_hardware_out, set_jr_stamping, hardware_out, drain_out, out_drops}` |
| `pump_clock_out_system`'s drain of `ClockMasterRes::out` | `Phase::MidiOut` (the session's MIDI nodes' out ring); `ClockMasterRes` holds the master alone |
| `crates/dsp/tutti-nodes/examples/headless_modulation.rs` | `crates/tutti-session/examples/headless_modulation.rs`, §5.1 on an offline session |

The session API: `Session::mods()` (`Mods`: `source`, `lfo`, `macro_`,
`follower`, `note_envelope`, `set_source`, `remove_source`, `route`,
`route_mut`, `remove_route`, `range`, `clear_range`, `automate`,
`clear_automation`, `record`, `take_mut`) mints `SourceKey`s and
`RouteKey`s and names targets by `ParamRef` (`unit`, `plugin`, `key`,
`from_key`); `Session::mod_state()`; `Session::midi()` (`Midi`). A refusal
is `ModRefusal` (`Apply(ReconcileError)` or `TwoDeclarers`, with `route`,
`target` and `source` in the declarer's keys), reported once per change as
`SessionEvent::ModulationRefused`. `Session::remove` forgets the session's
own declarations on the node, and a clip or live input that was it.

Tests. The session's: `tests/modulation.rs`
(`the_8_1_example_renders_its_analytic_values`,
`a_target_placed_one_update_later_resolves_on_that_update`,
`the_unresolved_warning_fires_after_four_seconds_not_n_updates`,
`a_refusal_names_the_route_key_the_session_minted`,
`a_host_and_the_session_cannot_both_declare`,
`removing_a_node_forgets_its_declarations`,
`a_param_ref_round_trips_through_its_key`) and `tests/midi.rs`
(`a_clip_plays_its_note_on_its_frame`, `a_clip_edit_updates_in_place`,
`a_target_without_an_event_input_takes_no_clip`,
`a_clip_and_a_live_input_survive_a_re_bind`,
`route_rules_wire_the_hardware_input`,
`the_hardware_out_drains_with_stamping_off`), each with its mutations
recorded. Moved: `hardware_out.rs`'s five wire tests, into `midi.rs`'s unit
tests; `out_active_yields_the_output_with_stamping_disabled` split, its
logic half the session's stamping tests and its adapter half
`the_stamper_switch_reaches_the_sessions_output`. Kept and passing over the
moved code: `tests/mod_lanes.rs`, `midi_sequence.rs`, `midi_routes.rs`,
`midi_soundfont.rs`, `midi_hardware_io.rs`, with the mutation records that
named moved code re-run against it.
`a_target_removed_then_re_bound_keeps_its_clip` also asserts the clip is
the same node, since a skipped re-aim now makes a second clip that plays
rather than none. `a_host_edit_committed_for_a_target_publishes_its_latency`
says what it covers: since §9.4 the frame's own commit takes a host's edit,
so in that scene the re-apply's commit adds a macro's lane and moves no
figure (the old mutation had already survived §9.4). A
re-apply can move the latency (a follower of a latent node reaches its
target's param port only through the lane the re-apply builds): the
session's `a_re_apply_that_moves_the_latency_publishes_it` and Bevy's
`a_re_apply_that_moves_the_latency_is_mirrored_on_that_frame` cover the
publish and the mirror. After review: `the_routes_have_one_declarer`,
`dropped_ticks_are_reported_at_most_once_a_second`,
`with_no_graph_the_drained_events_are_counted`,
`a_keyboard_declared_before_the_graph_attaches_when_it_arrives` and
`a_keyboard_on_a_node_without_midi_attaches_after_a_rebind_to_a_synth`.

Where the code differs from the text above:

- **`ModRefusal::TwoDeclarers`.** A session has one declarer: a host's
  `ModInput` while `s.mods()` holds anything is refused, not merged.
- **The session logs keys, not entity names.** Bevy's
  `ModulationRefused` maps a refusal's keys back to entities.
- **`Graph::modulate` and `Graph::send_mod_resyncs` are `pub(crate)`**:
  the phases are the one way in.
- **`AudioNode` is `Ord`**, for the session's `BTreeMap`s.
- **`live_input` returns a `LiveInput`** (its node and its `MidiSender`),
  and **`Midi::retarget`** re-aims a clip or live input: both outlive their
  target, so a host whose entity is re-bound keeps them, and removes one
  with `Session::remove`.
- **Routes are under `midi`, not `midi-hardware`**: the input node exists
  without the OS layer. `set_routes` takes the host's own keys and a
  resolver, which keeps Bevy's rule that a target with no node yet still
  covers its rule's ports.
- **`Midi::hardware_out()` returns the router** (a host's MIDI-CI reply or
  track mailbox is stamped and counted through it), and a rising drop
  count is `SessionEvent::MidiOutDropped`, at most once per
  `MIDI_OUT_DROP_REPORT` (1 s) of `now`.
- **The routes have one declarer**, as modulation does: the session's own
  `route`/`set_route_fallback` or a host's `set_routes`, the other refused
  with `Error::TwoRouteDeclarers`. `set_routes` returns a `Result`.
- **The §5.1 example** uses `build_offline` and `Graph::add`, not a
  device and `insert`, and an LFO and a follower on an SVF.

§9.4's deferred items stay deferred: building `GraphReconcileSystems` from
`PHASES` waits for `Phase::Plugins` (§9.6) and is §9.7's; Bevy's device
poll is not modulation or MIDI, and stays a system until then. Bevy runs
`Phase::{Modulation, AfterCommit}` from its modulation systems and
`Phase::MidiOut` from the clock pump.

### 9.6 Plugins and export in the session

**Scope:**

- `PluginLoad`, `add_plugin`, the session-owned parameter mirror (and
  `ModKeys::plugin_value` deleted), and the health, snapshot, latency and
  GUI-edit work of `Phase::Plugins`.
- `Phase::RenderMode` and the `ExportToken` (§4.3).
- The session's `ExportRequest`, `ExportJob`, `finish_export` and the
  drop of leftovers on the main thread.
- `bevy-tutti`'s request observer translates its component; jobs go on the
  task pool.
- CLAUDE.md's "build `plugin-server` first" list gains these tests.

**Properties and tests:**

- `an_export_announces_offline_before_its_first_block_and_realtime_after`:
  over the CLAP test plugin, which records the calls.
- `a_dropped_export_job_restores_realtime_on_the_next_update`: drop the job
  (and, separately, its `Done`) without `finish_export`; the next
  `update()` announces Realtime. Mutation: announce Realtime only in
  `finish_export`, and the test fails.
- `a_plugin_added_mid_export_is_told_offline`: `needs_announcement`'s
  `!told` half, end to end.
- `a_plugin_dead_for_the_window_is_removed_and_reported_once`, driven with
  explicit `now`s.
- `export_leftovers_drop_on_the_main_thread`: the live graph holds a
  `ForkByClone` probe node whose `Drop` records `thread::current().id()`
  into a shared cell, so the export's fork carries a clone of it. The test
  runs `job.run()` on a spawned thread and `finish_export` on its own, and
  asserts the fork's recorded id is the test's thread. The probe is
  `Send`, so it can ride in the job and its `Done`. (Not a probe captured
  by the prepare edit: an edit's captures drop on the worker by design,
  `export/request.rs:612-620`.) Mutation: drop the leftovers inside
  `run()`, and the recorded id is the worker's.

**Exit:** those session tests, `bevy-tutti`'s plugin-host and export suites,
and the §5.2 example under `tutti-session/examples`, run in CI offline with
the CLAP test plugin. All of them fail, not skip, without
`tutti-plugin-server`.

**Landed.** Two private modules, `plugin` (feature `plugin`) and `export`
(feature `export`; the render mode needs both). What moved, old → new:

| Was (`bevy-tutti`) | Now (`tutti-session`) |
|---|---|
| `plugin_load_start`'s task: `PluginClient` load, param read, state restore | `Session::plugin_load` → `PluginLoad` (`Send`; `with_config`, `at_rate`), `run()` → `PluginLoaded`; `Session::add_plugin` (and `add_plugin_blocking`) |
| `plugin_load_promote`'s insert, controls and meter capture | `Session::add_plugin` keeps a `HostedPlugin` record per node (`Session::plugin`, `plugins`); Bevy binds its shadow from it (`CapturedControls::for_hosted`) |
| `PluginParamValues` as a component, `read_plugin_params`, `PluginParamRead`, `plugin_param_edits` | the record's mirror (`HostedPlugin::param_values`), started in the load job; GUI edits drained into the lanes' knobs by `Phase::Plugins` |
| `ModKeys::plugin_value` | deleted: `ModHostOver` reads the session's mirror by `AudioNode` |
| `plugin_health_poll`'s debounce and teardown | `Phase::Plugins`: `PluginLiveness` on the record, the dead node removed by the session, `SessionEvent::PluginDied { node, cause }` once |
| `plugin_latency_poll`, `AudioGraphRes::refresh_node_latency` | `Phase::Plugins` → `Graph::refresh_node_latency`, `SessionEvent::PluginLatencyChanged` |
| `plugin_state_snapshot`'s 512-frame counter and inline task | `SessionEvent::PluginSnapshotDue(PluginSnapshot)` every `SNAPSHOT_INTERVAL` (8 s) of `now`, a job; `Session::land_snapshot(PluginSnapshotTaken)` |
| `PluginRenderMode`, `RenderModeAnnounced`, `plugin_render_mode_drive` | `Phase::RenderMode` and the export token; `Session::render_mode()`; Bevy's `plugin_render_mode` runs the phase |
| `AudioGraphRes::export`, `ExportRefused`, `start_exports`' fork build and render | `Session::export(ExportRequest)` → `ExportJob`, `run()` → `ExportRendered`, `Session::finish_export`; `export_blocking`, `export_in_flight` |
| `ExportTarget`, `ExportOutput`, `ExportClock`, `PreparedGraph`, `PrepareEdit`, `Stopped` (`export/request.rs`) | the session's; `bevy_tutti::export` re-exports each by name |
| `ExportDrops` | the session's leftovers channel, drained by `Phase::Collect`, `finish_export` and `drop_export_leftovers` |
| the §5.2 example's f64 length | `MidiFile::seconds_at(Beat) -> f64`, integrated over `tempo_map()` |

The session API: `Session::{plugin_load, add_plugin, add_plugin_blocking,
plugin, plugins, land_snapshot, render_mode, export, finish_export,
finish_export_unannounced, export_blocking, export_in_flight,
drop_export_leftovers, land_params}`, `PluginLoad::opens_in_process`,
`PluginParamRead` → `PluginParamsRead` (`SessionEvent::PluginParamsDue`), `Graph::host_plugin` (records a handle at an
existing node), `SessionBuilder::snapshot_interval`,
`EXPORT_STALL_WARNING`, `Error::ExportInFlight`, and the events above.
`tutti-plugin` gained `Plugin::opens_in_process` (the load job defers an
in-process open to `add_plugin`) and `PluginControls::{missed_chunks,
chunk_in_flight_answered}` (what the node played, for a test that paces a
live render on the server's answers). `HostedPlugin` exposes `handle`, `controls`, `meter`,
`liveness`, `snapshot` and `param_values`.

Tests. The session's `tests/plugin.rs`, over the CLAP test plugin:
`an_export_announces_offline_before_its_first_block_and_realtime_after`,
`a_dropped_export_job_restores_realtime_on_the_next_update`,
`a_plugin_added_mid_export_is_told_offline`,
`a_plugin_dead_for_the_window_is_removed_and_reported_once`,
`export_leftovers_drop_on_the_main_thread`; its `plugin` unit tests
(`snapshots_are_taken_on_the_interval_one_at_a_time`,
`a_declined_save_keeps_the_previous_snapshot`,
`a_failing_plugin_is_not_snapshotted`,
`a_plugin_replaced_by_a_native_node_is_no_longer_hosted`,
`a_crossfade_to_another_plugin_drops_the_old_snapshot`,
`a_crossfade_to_another_plugin_rebuilds_the_mirror`); its `export` unit
tests (`an_unannounced_finish_leaves_the_mode_to_the_phase`,
`a_leaked_export_is_warned_about_once`,
`a_frozen_exports_timeline_is_stopped_at_beat_zero`); `tutti-plugin`'s
`the_chunk_counters_report_misses_and_answers`; Bevy's
`back_to_back_renders_stay_offline` (two queued requests over real session
jobs, a probe between `poll_exports` and `start_exports`),
`a_non_plugin_node_never_marks_the_graph_edited`,
`the_poll_is_inert_without_a_graph` and
`two_in_process_loads_landing_together_open_in_two_frames`
(`tests/plugin_vst2_open.rs`); and the §5.2 example,
`examples/plugin_midi_export.rs`, run as a test (`test = true`,
`the_example_plays`). Each carries its mutations, run. Split: Bevy's
snapshot cadence test moved to the session (`snapshots_are_taken_on_...`);
the Bevy half keeps the placing (`a_snapshot_is_captured_and_stored`,
`the_triggering_frame_does_not_wait_for_the_plugin`,
`a_declined_save_does_not_erase_the_previous_snapshot`), over a session
with a zero interval. Bevy's health tests run the phase through a
`Graph::host_plugin` fixture; its render-mode tests run real session
export jobs. Kept and passing over the moved code, with every mutation
record that named moved code re-run against it: `plugin_capture.rs`,
`mod_plugin.rs`, `export_fork.rs` (its `off_thread` tests threaded),
`export_surface.rs`. Two records in `export_fork.rs`'s
`a_node_export_follows_a_90_bpm_timeline` were stale before this step
(`ExportClock::offline` and `Stopped::is_rolling`: a memory voice reads
the render's `Env`, not the fork's timeline); they now name the render
clock, which the test does pin. `ExportClock::offline` is pinned by
`export_surface.rs`'s rebinding tests. `Stopped::is_rolling` is read by
no node while the render is stopped, but a `PrepareEdit` reads it:
`a_frozen_exports_timeline_is_stopped_at_beat_zero` pins it.

`mod_plugin.rs`'s `an_export_is_the_live_render_on_a_clap_gain` paced its
live blocks on a fixed 25 ms sleep and failed intermittently in CI. It now
waits for the server's answer before each block
(`chunk_in_flight_answered`) and fails by name if a block was played
unanswered (`missed_chunks`).

Where the code differs from the text above:

- **The plugin records live in `Graph`**, not beside it, so
  `Graph::crossfade_plugin` and `replace_plugin` move a record to the
  incoming plugin, and `Graph::remove`, `replace` and `crossfade` (to a
  native node) drop it. The incoming plugin is told the render mode on
  the next phase and gets a fresh liveness, no snapshot (a fresh snapshot
  token, so a save in flight from the old plugin is refused) and a mirror
  read from it. That read is a job, not a call on the control thread (one
  round trip per parameter): `Phase::Plugins` hands it out as
  `SessionEvent::PluginParamsDue(PluginParamRead)`, and
  `Session::land_params` lands it. Until then the mirror is not ready.
- **The load job reads the params and restores the state**, off the
  control thread: `add_plugin` only inserts and records. A refused state
  is logged and the plugin loads at its defaults.
- **No `ExportToken` type.** The token is a clone of the session's
  `Arc<()>` inside `ExportJob` and `ExportRendered` (the doc's `Done`);
  `export_in_flight()` asks it.
- **Snapshots are a job with a token**, one in flight per plugin; a
  dropped job frees the plugin for the next interval, and a declined save
  keeps the previous snapshot.
- **`Phase::Plugins` asserts the main thread** (a dead plugin's removal
  may drop an in-process plugin with an editor), so Bevy's
  `plugin_health_poll` is `NonSendMarker`-pinned. It runs in
  `GraphReconcileSystems::Spawn`, after `plugin_load_promote`.
- **Any plugin the graph hosts is watched**, including one a host inserts
  with `AudioGraphRes::insert_plugin`: it has a record.
- **`Phase::RenderMode` needs `export`**; with `plugin` alone it is a
  no-op (nothing can be in flight).
- **`ExportPlugin` schedules `plugin_render_mode`**, after `poll_exports`
  and `start_exports`, so the frame's announcement follows both.
  `TuttiPlugin` adds the hosting plugin before `ExportPlugin`, so a check
  in the hosting plugin for `ExportPlugin` never scheduled it.
- **An in-process plugin (VST2) opens in `add_plugin`**, on the main
  thread, not in the load job: `PluginLoaded` carries the request back.
  The open (the library's init, the state restore and the parameter read)
  stalls the frame it lands in, so Bevy's `plugin_load_promote` opens at
  most one per frame (`PluginLoad::opens_in_process`); the rest wait,
  unpolled, for later frames.
  Bevy's `plugin_load_promote`, `plugin_state_snapshot` and
  `plugin_render_mode` are `NonSendMarker`-pinned like
  `plugin_health_poll`.
- **An export failure is keyed by `NodeKey`** in the session; Bevy maps it
  to an entity and `Name` (`NodeNames`, as before).
- **The §5.2 example** sets the tempo through
  `s.transport().settings.set_tempo`, and its length is
  `file.seconds_at(file.end())`.
- **`bevy-tutti`'s `plugin` feature no longer enables `tutti-plugin/bevy`**:
  nothing puts `PluginParamValues` on an entity. The derive itself stays in
  `tutti-plugin` (a cleanup can drop the feature).

### 9.7 Rules and gates

**Scope:**

- `scripts/check-adapter.sh` in `just ci` and CI.
- `bevy-tutti`'s schedule test against `PHASES`.
- The CLAUDE.md, engineering-notes and doc 013 edits of §8 not already
  made.
- The CHANGELOG migration table: `GraphRuntime` → `Graph`, `GraphNode` →
  `SessionNode`, `TuttiPlugin` options → builder, `GraphDirty` → none (an
  `AudioGraphRes` edit is committed by the frame), the Bevy
  `ExportRequest` → unchanged for Bevy users, translated internally.
- The umbrella prelude. §5.2 uses `tutti::prelude::*`, but the prelude
  lacks the session's `ExportRequest`, `PluginLoadRequest` and similar
  request types; the example names them by path until then.

**Properties:** the gates' mutations (§8).

**Exit:** `just ci` is green, and `scripts/check-adapter.sh` reports no
denied type in `bevy-tutti/src`.

**Landed.** As scoped, plus a reproducible toolchain (the user's
addition). Specifics:

- **`scripts/check-adapter.sh`** holds §8 checks 3 and 4 (check 4 moved
  there from `check-bevy-free.sh`), in `just ci` and CI's paths job. Check
  3 is a small Perl lexer over `bevy-tutti/src`, not a line grep: it
  blanks comments (nested too), string, raw-string and char literals, and
  whatever a `#[cfg(test)]` or `#[cfg(all(test, …))]` governs (an item to
  the `}` closing its first brace or its `;`; a field, argument or element
  to its `,` or the closer of its group), keeping line numbers; an
  out-of-line `#[cfg(test)] mod x;` is excluded by path (`#[path]`
  honoured). Check 4 parses each `pub use` as a use tree, groups nested to
  any depth. `Phase::Modulation` is a phase,
  not the reconcile, and is not a hit. Its limits are in the script: an
  item under `cfg(any(test, …))` is checked, a name built by a macro or
  reached through an alias declared outside the crate is not seen, an
  inner `#![cfg(test)]` is not read, and a test-only item with a comma in
  its generics is blanked only to that comma (a false hit, never a hidden
  one). A review caught the scan running past a `cfg(test)` field's group
  and blanking the rest of the file; it stops at the group's closer now.
- **One real hit on main:** `bevy_tutti::export` re-exported
  `tutti_export::RenderGraph`. Nothing in the adapter used it; it is
  removed, and the one test that named it imports it from `tutti-export`.
- **The schedule is built from `PHASES`.** `SessionPhaseSystems(Phase)`
  sets are chained from the constant and placed against
  `GraphReconcileSystems` (`configure_phases`). Two orderings on main
  disagreed with `PHASES`:
  - `Phase::Collect` ran inside `commit_graph`, after `Plugins` and
    `Modulation`. It is now its own system, `collect_graph`, first in the
    frame; `commit_graph` runs only `Commit`.
  - `Phase::MidiOut` (the clock pump) had no order against the commit and
    `AfterCommit`. It now follows them.
- **The test** (`graph::schedule::tests`) records the phases the session
  ran in each frame (a `cfg(test)` log on `AudioGraphRes`) and asserts
  they are strictly increasing in `PHASES` rank, with every phase the
  features schedule present. It reads the order from `PHASES`, so
  reordering the constant reorders the expectation.
- **The prelude** gains the session's requests behind their features
  (`ExportRequest`, `ExportOutput`, `ExportConfig`, `PluginLoadRequest`,
  `PluginId`, `ParamRef`, `MidiFile`, `TimedMidiEvent`, `GraphSource`, the
  offline timeline). **Deviation:** §5.1 and §5.2 moved from
  `tutti-session/examples` to `tutti/examples` so they can use
  `tutti::prelude::*`: `tutti-session` cannot dev-depend on `tutti`, which
  depends on it. The §5.2 example is still a test (`test = true`), run by
  `tutti --features clap,export`.
- **Reproducible builds:** `Cargo.lock` is committed and every CI and
  `just` cargo command passes `--locked`; `rust-toolchain.toml` pins 1.99.1,
  which CI installs in place of `dtolnay/rust-toolchain@stable`; miri names
  `+nightly`. The pin surfaced no new lint. `bevy-tutti --features
  plugin,modulation` had dead test code (`tests/mod_plugin.rs` loads a
  plugin only with `export` or `vst2`); the file is gated on them, and
  dark-features lints that combination and `tutti --features
  clap,export,modulation`.
- **Deferred:** Bevy still does not run `Phase::Poll`. Its set exists,
  last in the chain, but `device_state_sync_system` reads the device's
  faults into `AudioDeviceState` itself. `tutti-plugin`'s `bevy` feature
  (unused since §9.6) is left for a cleanup.

## 10. Out of scope

- **A plugin editor window API for headless hosts.** It needs a window
  system; a host brings its own, and the `tutti-plugin` handle already
  opens an editor into a raw handle.
- **MIDI-CI and UMP endpoint negotiation.** The state machines are
  `tutti-midi-runtime`'s. A headless host drives `CiResponder` and
  `CiInitiator` itself and writes their output through
  `s.midi().hardware_out()`. `bevy-tutti`'s `negotiation.rs` stays its
  adapter.
- **Project files and session save/load.** "Session" here means a running
  engine, not a document (§11).
- **Any change to the graph, the node contract or the RT rules.**

## 11. Decisions

No question is open. The user decided:

1. **The name:** `tutti-session`, with `Session`, re-exported as
   `tutti::session`. The alternatives each collided with something:
   `tutti-engine` with `tutti_core::Engine`, `tutti-host` with the plugin
   and cpal hosts, `tutti-app` with Bevy's `App`, and `tutti-runtime` with
   `tutti-midi-runtime`. "Session" means a running engine here, not a saved
   project (§10).
2. **The default:** `tutti` has `default = ["session"]` (§7).
3. **No re-export from `bevy-tutti`** (§6).
4. **All seven PRs run** (§9).
