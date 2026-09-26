# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **tutti-graph: events as ports, the graph side** (design doc 013,
  "Rewrite order" item 5; no MIDI node is ported yet, and `Legacy` MIDI
  through `MidiInPort` mailboxes is unchanged). What changes for a caller:

  | Was | Now |
  |---|---|
  | an event input's sources tied at one offset in the order the spec listed them (`GraphSpec::events`' `Vec`, `connect_events` appending) | ties go by **source order, the source's `(NodeKey, port)`**, whatever the listing (doc 013 decision 6, recorded). `connect_events` inserts in that order, so two specs with one wiring compare equal. For a graph built with `GraphBuilder` nothing changes: its keys increase in the order nodes are added |
  | every event slot held the executor's one capacity (`Editor::with_event_capacity`, default 512) per port | a node may declare its own, per event output per block: `Shape::with_event_capacity(n)` (`Shape::event_capacity`, `None` for the default). Slots, merges and PDC/feedback FIFOs are sized from the declarations |
  | `Plan::event_slot_weight() -> &[u32]` (multiples of the one capacity) | `Plan::event_slot_capacity() -> &[EventSlotCapacity]` (`declared` events plus `defaults` ports at the executor's default; `events(default)` prices it). `Plan::event_port_capacity(EventOut)` reads a port's declaration |
  | a writer past its capacity refused the push and the executor counted it | the same, at the node's declared capacity: **drop the newest, and count it** (`EventRejected::Full`, `Executor::dropped_events`). The verifier now checks that no slot downstream of a writer can refuse: merges hold the sum of their inputs, delay outputs what their FIFO can hold |
  | `Editor::replace` / `verify_fades` compared ports, latency, in-place acceptance and event resolution | also `event_capacity`: a replace queued behind a running fade runs the old unit under the new plan, so the two must fit the same buffers (`CommitError::FadeShape` otherwise) |
  | the reference interpreter's fading-out unit got accepting event writers | detached ones, as the executor's: every push refused and not counted |
  | an event delay FIFO's note-off reserve was bounded by its allocation, which a retune to a shorter delay never shrinks | bounded by `limit + reserve`, the figure its output slot is priced at: a PDC delay's output and an event feedback slot hold everything their FIFO can (`EventSlotCapacity` for them is FIFO-sized, not the source's one block), so events the source wrote across two of its blocks that fall due in one are delivered on their frames |

  `Reference::dropped_events` counts what the oracle's writers refuse, so
  the differential suite compares the two interpreters' drop counts rather
  than assuming none; its generator gains an emitter declaring a capacity
  below its burst. New tests: `tests/event_ports.rs` (same-block delivery,
  fan-in ties, PDC across blocks, feedback latency, overflow, declarations
  sizing merges and delays), a no-alloc gate for writers refusing past a
  declaration, and the contract suite's `EventFanIn` path behind PDC.

- **tutti-core's `Engine` renders only the native graph** (design doc 013,
  Phase 3 PR 15; Phase 3 is done). The engine's fundsp `Net` backend is
  removed: the `Engine::new(MotionFsm, NetBackend)` constructor, the
  `Net` render path and the root re-export of `NetBackend`. `Net` itself
  stays, as a graph container (`tutti_core::dsp::Net`, `topology::compile`),
  until Phase 5 deletes fundsp. What changes for a host:

  | Was | Now |
  |---|---|
  | `Engine::new(transport.motion.clone(), net.backend())` | build the graph natively and hand the engine its executor: `let (mut editor, executor) = GraphBuilder::new(..)` (`add_unit` where you had `push(Box::new(..))`; `connect`, `pipe`, `pipe_output` as on `Net`) `.build(Prepare::new(rate, max_block))?`, then `Engine::new(&transport, &mut editor, executor)?`. Keep the editor on the control thread: it is how you edit the graph from then on (`insert`, `spec_mut`, `commit`) |
  | `Engine::with_graph(&transport, &mut editor, executor)` | `Engine::new(&transport, &mut editor, executor)`: renamed, since it is the one constructor and there is no other runtime to contrast it with |
  | `Engine::with_graph_capacity(..)` | `Engine::with_capacity(..)` |
  | `Engine::graph_block_capacity() -> Option<Samples>` (`None` for a `Net` engine) | `Engine::block_capacity() -> Samples` |
  | `tutti_core::DEFAULT_GRAPH_BLOCK_CAPACITY` | `tutti_core::DEFAULT_BLOCK_CAPACITY` (the same 8 192 frames) |
  | a `TransportClock` pushed into the `Net` to move the playhead | nothing: the engine drives the transport's clock itself and hands each block its transport in `Env`. A node that wants the beat as a signal is fed by an `EnvClock`. The graph must not hold a `TransportClock` of its own (it would consume the transport's seeks); nothing refuses one, so keep the rule yourself |
  | a root wider than `MAX_ROOT_CHANNELS` clamped to it | refused, with `GraphEngineError::Limits(CommitError::TooManyOutputs { .. })` at construction or the commit that widens it, as it already was for a native graph |
  | `tutti_core::NetBackend` | removed from tutti-core. `Net::backend()` still exists for a `Net` you drive yourself |
  | `tutti_core::net_fade(curve)` | removed: nothing takes fundsp's `Fade` any more. `CrossfadeCurve` is unchanged |
  | `unsafe Engine::settle_graph` answered `true` for a `Net` engine | it always settles the graph |

  The `tutti` umbrella's `headless_engine` example, its README and
  tutti-core's and tutti-cpal's READMEs build the native graph. The tests
  that compared the native graph with a `Net` (tutti-core's `engine_graph`,
  `env_clock`; tutti-export's `graph_source`; bevy-tutti's `net_parity`,
  now `scene_render`, `engine::build`'s engine tests and `export_fork`) are
  pinned to analytic figures, to invariants of the render, and, for samples
  that call libm, to golden digests asserted on Linux/glibc.

- **tutti-export renders only the native graph** (design doc 013, Phase 3 PR
  14). fundsp's `Net` is no longer an export source: the `Net` arm of
  `RenderGraph`, its conversion from a `Net`, and the `Net`-taking latency
  and tail queries are removed. What changes for a caller:

  | Was | Now |
  |---|---|
  | `RenderGraph::Graph { editor, executor }` | `RenderGraph::new(editor, executor)?`: a struct with private fields, since there is no other kind of graph to render (a one-variant enum would select nothing). `new` refuses (`Error::InvalidConfig`) an editor that does not feed the executor, which used to be found only at render; `RenderGraph::fork` is unchanged |
  | editing the pair's `editor` / `executor` before a render | `graph.editor()` / `graph.editor_mut()`, then `graph.commit()` (commits and installs the edit on the executor, which is no longer reachable) |
  | `RenderGraph::Net(net)`, `RenderGraph::from(net)`, or a `Net` passed straight to `render_to_file` / `render_to_buffers` / `render_normalized_to_file` | removed. Build the graph with `tutti_graph::GraphBuilder` (`add_unit` where you had `push(Box::new(..))`; `connect`, `pipe`, `pipe_output` as on `Net`), `build` it at `RenderGraph::prepare(rate)`, and pass `RenderGraph::new(editor, executor)?`. A host exporting a live graph forks it with `RenderGraph::fork` |
  | the entry points took `impl Into<RenderGraph>` | they take `RenderGraph` |
  | `tutti_export::reported_latency(&mut net)` | `graph.reported_latency()` on the `RenderGraph`: the compiled plan's worst-case output latency, at the rate the graph was prepared at (the `Net` version answered at the net's last rate, 44.1 kHz for one never rendered, unless re-rated first) |
  | `tutti_export::reported_tail(&net)` | `graph.reported_tail()`. For a `Net` you still hold elsewhere, `tutti_types::graph_tail(&net)` is the same fold |
  | `RenderGraph::reported_latency(&mut self)` | `&self` |
  | bevy-tutti: `if let RenderGraph::Graph { editor, .. } = prepared.graph` in an export `prepare` hook | `prepared.graph.editor_mut()`: `PreparedGraph::graph` is always a native graph, and a hook can no longer swap in a `Net`. The adapter still commits what the hook leaves |
  | `tutti` umbrella: an export graph needed a direct `tutti-graph` dependency | `tutti::graph` re-exports `tutti-graph` (`tutti::graph::GraphBuilder`) |

- **bevy-tutti runs on the native graph only** (design doc 013, Phase 3 PR
  13). fundsp's `Net` is gone from the adapter: `AudioGraphRes` holds the
  native `tutti-graph` editor, PDC is the compiler's (no delay nodes are
  spliced into the graph), and every export forks the live graph. What
  changes for a host:

  | Was | Now |
  |---|---|
  | `TuttiPlugin { graph_backend: GraphBackend::Native, .. }` | delete the field: there is one runtime. `graph_backend` and `GraphBackend` are removed (a one-variant enum would select nothing) |
  | `TuttiPlugin::default()` ran on `Net` | it runs on the native graph. Behaviour that differed, now everywhere: a `set_param` lands on the node's next rendered block; `inspect` reads the node's shadow (an isolated copy with every setting applied); `render_frame` panics once the audio side is taken; `replace` fades only between units of one latency (else it swaps); a device-rate restart keeps every unit instance |
  | `AudioGraphRes::headless_with(backend, i, o)` / `unattached_with(backend, i, o)` | `AudioGraphRes::headless(i, o)` |
  | `AudioGraphRes::unattached(i, o)` | `AudioGraphRes::headless(i, o)`: the two built the same graph on the native runtime (a setting is never applied at once), so one name stays |
  | `AudioGraphRes::backend()` | removed |
  | `PreparedGraph::ctx: Option<&OfflineTransport>` (`None` for a `Net` master export) | `ctx: &OfflineTransport`: every export is a fork rebound onto the request's timeline |
  | `PreparedGraph::graph` could be `RenderGraph::Net` | always `RenderGraph::Graph` (the enum keeps its `Net` arm until tutti-export's graph-only API, PR 14) |
  | `ExportSource::Master` on `Net`: a plain clone keeping live bindings and running state | a fork of what the outputs hear, isolated, rebound and reset (PR 12's behaviour, now the only one) |
  | `bevy_tutti::Net`, and `Net` in `bevy_tutti::prelude` | removed: nothing in the adapter hands one out. Name `tutti_core::dsp::Net` if you still build one yourself |
  | `offline_export` example's `-- --native` | removed; the example always forks |
  | An export of a graph holding an **in-process VST2 plugin** (inserted boxed) cloned it | `ExportError::NotForkable`, naming the node: its clones share the one plugin. **Follow-up**, not by design: an in-process VST2 fork by state transfer (doc 013, PR 13's follow-ups). Meanwhile export a node it does not feed, freeze it, or host it out of process |
  | An export of a graph holding a **unit with a captured MIDI port pushed with `insert` / `insert_boxed`** rendered it | `ExportError::NotForkable`, naming the node: its fork would drop the clip on its port. **By design**: insert it with `AudioGraphRes::insert_with` and its `CapturedControls` (what `spawn_audio_node` does), and its fork carries the clip |
  | An export of a graph whose outputs reach a **mic monitor** (`MicMonitorNode`) cloned it, and the render read the live input ring | `ExportError::NotForkable`, naming the node. **By design**: a live input has nothing to render offline. Route the monitor away from what you export, or export a node it does not feed. A monitor no output reaches does not refuse a master export |
  | `TuttiDriver::restart_with` / `restart_on` hooks took `(&OutputSpec)` (tutti-cpal) | they take `(&OutputSpec, &Stopped)`; `Stopped::settle_graph` reaches the engine while no callback can (`unsafe fn Engine::settle_graph`, new in tutti-core: the caller guarantees no callback runs; `Stopped` is the safe way in). Ignore the second argument if the hook does not need it |
  | Without `LatencyCompensationPlugin`, `ChannelCompensation` / `GraphLatency` stayed empty while the graph compensated | `commit_graph` publishes the sent plan's figures with every commit, on every graph; `GraphReconcilePlugin` inits both resources. `LatencyCompensationPlugin` is now an optional debug check |

  - **A device restart finishes its re-prepare before the first block.**
    `restart_device` runs both halves of the graph's re-prepare inside the
    driver's hook, so the first block on the new device plays the
    re-prepared graph (no silent block) and the new rate's PDC figures are
    published before it (a disk source seeking in that block pre-rolls by
    them). A graph a re-prepare poisons now refuses the restart with the
    stream stopped.
  - **The rebuild's debug consistency check no longer panics over a
    partially declared graph.** `topology::disagreements` compared the
    latency plan of the whole graph against the value's, which holds only
    declared ports, so a host that wired the master itself (an empty
    `MasterSources`), or a port a short `PortSources` leaves undeclared,
    through a latent node panicked a debug build. It compares declared
    edges and outputs only; restricted to those, the plan comparison is
    implied by them.
  - **`LatencyCompensationPlugin` neither applies nor publishes.** The graph
    compensates every commit, and `commit_graph` publishes the figures; the
    plugin adds a debug-build check that the topology's latency fold agrees
    with the compiled plan. Its API is unchanged.

- **bevy-tutti exports fork the native graph** (design doc 013, Phase 3 PR
  12). On `GraphBackend::Native` an `ExportRequest` renders `Editor::fork`
  of the live graph — `ExportSource::Master` what the global outputs hear,
  `Node` the sub-graph feeding it — in `ForkMode::Offline` on the request's
  timeline, through tutti-export's `RenderGraph::Graph`. The live graph is
  not touched and keeps playing while the render runs. `GraphBackend::Net`
  exports as before until PR 13 removes it. What changes for a host:

  | Was | Now |
  |---|---|
  | `ExportRequest::new(source, target, config, Arc::new(FrozenClock))` | `ExportRequest::new(source, target, config, ExportClock::frozen())` |
  | `ExportRequest::new(.., timeline.clone()).on_timeline(timeline)` | `ExportRequest::new(.., ExportClock::timeline(timeline))` (`on_timeline` still sets it on a built request) |
  | `ExportRequest::clock: Arc<dyn RenderClock>` and `offline: Option<OfflineTransport>` | one field, `clock: ExportClock`: the renderer's clock and the nodes' timeline are one object, so they cannot disagree. A frozen clock rebinds the nodes onto a timeline stopped at beat 0 (a clip reader plays nothing), not a default rolling one nothing advances |
  | `ExportRequest` and its fields constructed with a struct literal | `#[non_exhaustive]` (new fields `clock`, `latency_from_graph`, `tail_from_graph`): build with `ExportRequest::new` |
  | `ExportDone::result: tutti_export::Result<ExportOutput>` | `Result<ExportOutput, ExportError>`. `ExportError::Render` wraps the engine's error; `NotForkable`, `ForkSource` and `ForkFailed` name the node as an `ExportNode` (its entity, `Name` and graph key; `#[non_exhaustive]`) |
  | `ExportRequest::with_prepare(\|PreparedNet { net, ctx }, world\| ..)` | `with_prepare(\|PreparedGraph { graph, ctx }, world\| ..)`, `graph: &mut RenderGraph`: `RenderGraph::Net` on `Net`, the fork's own editor and executor on `Native` (the adapter commits what the hook edits; `PreparedGraph::fresh_key` names a node the hook inserts) |
  | `PrepareNet` | `PrepareGraph` |
  | On `Native`, an export reported `InvalidConfig` ("not yet available") | it renders |
  | "export target node has no outputs" for a graph with no outputs | "the graph has no outputs to export", for either source; an entity with no node is "export target entity is not a graph node" |

  - **A native master export renders what the graph is driven to play,
    from silence**: every node isolated, rebound and reset, where `Net`'s
    master export was a plain clone keeping the live bindings and running
    state. The two render the same samples from a graph whose live side has
    not advanced.
  - **An engine-built graph forks**: the native beat clock (`EnvClock`) is
    inserted with a fork source (`tutti_graph::ForkByClone`, new), so a
    graph holding it — every graph `build_into` makes — exports.
  - **Hosted plugins are forkable**: `plugin_load_promote` inserts the
    concrete `PluginClient` (`Plugin::into_client`, new) with its fork
    source, so a fork loads a fresh instance with the live one's state. An
    in-process VST2 plugin is still inserted boxed and refuses an export by
    name.
  - **A disk-streamed sampler voice exports**: its fork reads the voice's
    file itself, on the render's thread, and plays it on the export's
    timeline (see the tutti-sampler entry below). It no longer refuses an
    export as `ExportError::NotForkable`. On `Net` a node export plays it
    the same way; a `Net` master export is still a plain clone that reads
    the live voice's ring from the render thread, until PR 13 removes it.
  - New on `ExportRequest`, either backend: `trim_reported_latency()` and
    `with_reported_tail(cap)` take the render's latency trim and tail from
    the graph that is rendered, after the `prepare` hook. The tail resolves
    by `GraphTail::resolve`: a node that never said renders no tail, not the
    cap.
  - `bevy_tutti::export` re-exports `RenderGraph`, `ForkCause`,
    `ForkFaultKind` and `NodeKey`, the engine types its API hands out.

- **tutti-sampler: a forked `DiskVoice` plays its file offline.** A copy
  taken for an offline render (a graph fork, or `clone_isolated`) no longer
  touches the live stream: `isolate` gives it a private snapshot of the
  stream's control cell (so its gate can no longer ask the live butler to
  seek) and switches it off the ring, and `rebind_offline` hands it the
  stream's file, loop and rate as the stream's record says at that moment
  (each butler stream keeps a small `StreamRecord`; `Status::take_disk_voice`
  hands the voice a read-only handle on it, never the plan map). The copy
  re-opens the file by its path and decodes it on demand on the render's
  thread in two resident pages laid out ahead of the read in either
  direction — or reads the butler's cached `Arc<Wave>` in place when the
  cache holds the file (always, for a file that cannot seek) — and reads
  each position as `MemorySource` reads it (the same four taps and kernel,
  split out of `interp::read_frame`), so a clip rendered from disk is
  bit-identical to the same clip in memory at matched rates. It plays the
  voice's window on the render's timeline, resampled from the file's rate
  to the rate it was last told (`set_sample_rate`; never a default),
  looped as the stream is, reversed as the memory tier reverses; the
  butler's PDC preroll is not applied offline. It registers no butler
  stream, and closes its file once past its window. A copy that cannot play
  what it describes (its stream ended before the fork, its file unreadable,
  no sample rate) renders silence **and latches the failure**, reported
  through `AudioUnit::render_fault` so the render fails by name.
  `DiskVoice::forkable` is `true` again (the trait default), and so is a
  `VoiceNode` holding one.
- **tutti-sampler / tutti-types: whole frames land exactly.**
  `interp::window_position` (both tiers) lands a position within
  `FRAME_TOLERANCE` of a whole frame on it (`tutti_types::snap_to_whole_frame`,
  new, beside the tolerance), and `tap_indices` reads a position whose
  fraction rounds to 1.0 in `f32` as the next frame at `t` = 0. Converting
  the clock's beat back through seconds landed an ulp off (frame 128 of a
  clip at 120 BPM, 48 kHz came out 127.99999999999), so a clip placed on a
  beat played its own samples only to within an ulp.
- **tutti-node / tutti-graph: a `Legacy` unit can fail its fork while it
  renders.** `AudioUnit::render_fault()` (default `None`) hands a copy's
  failure probe (`RenderFault`; `FaultLatch` is the plain one) to
  `Legacy`'s fork source, which gives it to the forked editor as a
  `ForkHealth`; `ForkFaultKind` gained `Failed` (a unit could not produce
  what it describes, its cause says why). tutti-export's `fork_health`
  check then fails the render, and bevy-tutti reports
  `ExportError::ForkFailed` naming the node — for a disk voice, with the
  file.
- **tutti-graph: `ForkTarget::Master` forks what the global outputs
  reach**, walking back along audio, feedback and event edges, and nothing
  else. A node no output reaches is not copied and need not be forkable
  (an unrouted mic monitor no longer refuses a master fork; an unrouted
  plugin launches no server). `ForkByClone<N>` inserts a native `Node` whose
  `Clone` shares nothing with a fork source that clones and resets it.

- **tutti-plugin: a plugin fork carries its MIDI clip.** An offline
  `PluginClient` fork copies the clip source installed on the live node's
  MIDI port onto its own port, with a fresh cursor on the render's timeline,
  so an exported instrument plays its notes (its live inbox and MIDI-out are
  still not carried). A source that cannot be rebound fails the fork with
  `PluginForkError::MidiSource` (new) instead of rendering its notes as
  silence. `PluginClient::fork_source` hands out the fork source `IntoNode`
  uses, for a host inserting through its own node builder.

- **tutti-midi-types / tutti-midi-runtime: a MIDI source is handed its
  unit's rate, and says whether it survives an offline render.** Breaking:

  | Was | Now |
  |---|---|
  | `MidiUnitIn::poll_unit(unit, block, buf)` | `poll_unit(unit, block, sample_rate, buf)`: the polling unit's rate for the block, so no source keeps a copy of it to fall out of step |
  | — | `MidiUnitIn::rebind_offline(unit, ctx) -> Option<Arc<dyn MidiUnitIn>>`, **required**: an offline copy on the render's timeline, or `None` for a source that cannot be carried |
  | `MidiInPort::poll(block, buf)` | `poll(block, sample_rate, buf)` |
  | — | `MidiInPort::rebind_offline_into(fork, ctx) -> OfflineRebind` (`NoSource`, `Rebound`, `NotRebindable`) |
  | `MidiClipSource::new(unit, events, transport, sample_rate)` | `MidiClipSource::new(unit, events, transport)`: it places events at the rate it is polled at |
  | `Midi::drain_for_process(block)` / `drain_for_tick()` (tutti-plugin) | `drain_for_process(block, sample_rate)` / `drain_for_tick(sample_rate)` |

  `tutti_core::transport::BeatCursor` gained `advance_at(block, rate)` and
  `unrated(transport)` for such a source. bevy-tutti's
  `midi::sequence::rebuild` no longer rebuilds every installed clip when the
  device rate changes (#39 did, to re-rate the clip): a re-rated unit
  already hands its clip the new rate, and the rebuild's all-notes-off could
  cut a sounding note. `MidiClipSource`'s offline copy
  has no hardware-out tap (an export must not play the clip on external
  MIDI); `MidiSnapshotReader` answers `None` (it is already offline, bound
  to its own timeline).

- **bevy-tutti's `AudioGraphRes` is opaque: its methods are the only way to
  the graph.** The field was `pub Net`; it is private, so `graph.0` no longer
  compiles outside the crate. The graph is still fundsp's `Net` inside. The
  methods are named in graph terms and take an `AudioNode` and a `GraphSource`,
  so the native backend (design doc 013, Phase 3) can sit behind the same
  signatures. Every removed or changed item, with its replacement:

  | Was | Now |
  |---|---|
  | `AudioGraphRes(Net::with_backend(n))` | `AudioGraphRes::headless(0, n)`: a graph with no device, whose commits go to an audio side that is taken and dropped |
  | `AudioGraphRes(Net::new(i, o))` (no backend) | `AudioGraphRes::unattached(i, o)`: no audio side, so a param write lands on the node at once. It cannot commit |
  | `net.backend()` before inserting the net | `graph.take_audio_side()`, which returns `impl AudioUnit` to render what a device would hear |
  | `graph.0.add(unit)` / `graph.0.push(boxed)` → `NodeId` | `graph.insert(unit)` / `graph.insert_boxed(boxed)` → `AudioNode` |
  | `graph.0.contains(id)` then `graph.0.remove(id)` | `graph.contains(node)`, `graph.remove(node) -> bool` (`false` if it was not there) |
  | `graph.0.source` / `set_source` / `output_source` / `set_output_source` with `tutti_core::dsp::Source` | the same names with `bevy_tutti::graph::GraphSource`: `Node(AudioNode, port)` for `Local`, `Input(port)` for `Global`, `Silence` for `Zero` |
  | `graph.0.pipe_output(id)` | `graph.set_outputs_from(node)` (it still wraps a narrow node across a wide root, and `MasterSources` is still the declared way) |
  | `graph.0.crossfade(id, fade, secs, unit)` | `graph.replace(node, unit, Seconds, CrossfadeCurve)` |
  | `graph.0.set(unit_param::node_setting(id, param, v))` | `graph.set_param(node, param, v)` |
  | `graph.0.inputs()` / `outputs()` / `inputs_in(id)` / `outputs_in(id)` | `graph.inputs()` / `outputs()` / `node_inputs(node)` / `node_outputs(node)` |
  | `LatencyGraph::latency(&graph.0, id)` / `TailGraph::tail(&graph.0, id)` | `graph.node_latency(node)` / `graph.node_tail(node)` |
  | `tutti_core::latency::plan(&graph.0)` | `graph.latency_plan()` |
  | `graph.0.set_sample_rate(rate)` | `graph.set_sample_rate(rate)` |
  | `graph.0.tick(&[], out)` | `graph.render_frame(out)` |
  | `graph.0.node(id)` (read-only, for inspection) | `graph.inspect(node, \|unit\| ..)` |
  | `graph.0.commit()` in a host system | set `GraphDirty`; `commit_graph` commits once per frame. The commit is no longer public |
  | `graph.0.clone()` / `clone_isolated` for an offline render | an `ExportRequest` (`bevy_tutti::export`) |
  | `CapturedControls::bind(entity, NodeId)` | `CapturedControls::bind(entity, AudioNode)`, which takes what `insert` returns |

  `GraphSource` is new, in `bevy_tutti::graph` and the prelude. `bevy_tutti::Net`
  stays: on `GraphBackend::Net` an export's `prepare` hook is still handed
  the `Net` it renders (as `RenderGraph::Net`, since export moved to `Fork`;
  see the entry above). *Superseded by PR 13: `bevy_tutti::Net` is removed
  (the first entry).*

- **bevy-tutti captures a node's controls when the node is inserted, and never
  reaches back into the graph for them.** `MidiTargetRegistry` and
  `ModTargetRegistry` are now read once per unit, as it goes in (by
  `spawn_audio_node`, `insert_audio_node`, `crossfade_audio_node`, soundfont
  promotion and plugin load), and the answers live on the entity as
  `MidiTarget`, `ModParamsHandle` and `PluginShadow`. Two consequences for a
  host:
  - **Register a node type before spawning nodes of it.** A node inserted while
    its type was unregistered carries no captured controls and stays
    unaddressable; registering later does not reach it.
  - **`ModTargetRegistry::register::<T>` now requires `T: Clone`**, because the
    capture keeps a clone of the unit as its `ModParams`. Every engine node is
    `Clone`; a host's own node type needs the derive.
  - A host that pushes a unit into `AudioGraphRes` itself and binds
    `AudioNode` by hand runs the same capture with
    `CapturedControls::capture(world, &unit)` and `.bind(&mut entity, node)`.

  `tutti_plugin::handles::PluginControls` (from `PluginClient::controls()`) is
  the plugin half: the node's input slots, latency, tail and sample rate, all
  shared with it. `tutti_nodes::param_mod_parts` builds an audio-rate param
  chain as owned parts; `build_param_mod` is unchanged.

- **`AudioUnit` lost seven derived methods and `footprint` is defaulted.**
  `get_mono`, `get_stereo`, `filter_mono`, `filter_stereo`, `response`,
  `response_db` and `display` had no caller outside the fundsp fork; they are
  `fundsp_tutti::audiounit::AudioUnitExt` now (blanket-implemented, fork
  preludes only). A caller that ticked a unit through `get_stereo` calls
  `tick(&[], &mut [0.0; 2])` instead. `footprint` defaults to
  `size_of_val(self)`, so a new node need not write it; existing overrides are
  untouched. `ping` and `set_hash` stay on the trait: the fork's `Net` seeds
  its own generators through them by dynamic dispatch (see the trait docs).

- **File I/O moved from `tutti-core` (the fundsp fork) to `tutti-io`.** Old →
  new: `tutti_core::Wave` → `tutti_io::Wave`, `tutti_core::FileIn` →
  `tutti_io::FileIn`, `tutti_core::{WaveAsset, WaveMetadata, WaveError}` →
  `tutti_io::{WaveAsset, WaveMetadata, WaveError}`,
  `tutti_core::{can_decode, decodable_extensions}` →
  `tutti_io::{can_decode, decodable_extensions}`. Through the `tutti` umbrella
  they are `tutti::io::…`, gated on a new `io` feature that every codec
  feature and `audio-io` imply. Decoded samples are bit-identical
  (`tutti-io/tests/decode_golden.rs` pins them against fixtures in
  `assets/audio/`: bit-exact digests for WAV and FLAC on every platform and for
  MP3 and Ogg on Linux x86_64, where they were recorded, since those decoders'
  trig is libm-dependent; frame counts and per-channel levels everywhere).

  Features moved with them. `tutti-core` has no `wav`/`flac`/`mp3`/`ogg` or
  `bevy_asset` features any more; name `tutti-io/wav` etc., and
  `tutti-io/bevy` for `WaveAsset`'s `Asset` derive. `tutti-sampler`,
  `bevy-tutti` and `tutti` keep their codec features and forward them to
  `tutti-io`. `tutti-sampler` now depends on `tutti-io` and re-exports
  `tutti_sampler::Wave`; `bevy_tutti::sampler` re-exports `Wave` and
  `WaveAsset`, so a voice or asset consumer needs no direct `tutti-io` edge.

  The API was trimmed to what the engine calls. `Wave` keeps `new`,
  `with_capacity`, `zero`, `from_samples`, `sample_rate`, `channels`,
  `channel`, `channel_mut`, `at`, `set`, `len`, `is_empty`, `duration`, `load`
  and `probe_metadata`. `Wave::push(frame)` (a typenum tuple or a broadcast
  scalar) is `Wave::push_frame(&[f32])`, one sample per channel. Gone, with no
  engine caller: `render*`, `filter*`, `multifilter*`, `resample_fir`,
  `fade*`, `normalize`, `amplify`, `amplitude`, `retain`, `append`, `mix*`,
  `set_sample_rate`, channel insert/remove/push, `load_slice*`,
  `load_track*`, `load_with_progress`, `load_with_peaks`, and the WAV writers
  (`WavOut` is the engine's sink). `FileIn::open(path, track)` is
  `FileIn::open(path)`; every caller passed `None`. The fork's own `Wave` stays
  for its internal nodes and no longer decodes; its `files`/codec/`bevy_asset`
  features, symphonia dependency and `pub use symphonia` are gone.

- **Every count on the `AudioIn`/`AudioOut` edge is a `Samples`, not a
  `usize`.** `AudioIn::poll_into` and `pump` return `Samples` — the engine's
  existing frame count, the same type latency and `Seconds::to_samples` use —
  and so do `tutti_io::PumpPass::Wrote`, `ManualPump::pump_until_dry` and
  `tutti-sampler`'s `RegionOut::{push_interleaved, write_interleaved_reversed,
  write_space, capacity}`. `AudioPump::start`'s `capacity` is a `Samples`.
  The frame ↔ slice-length crossing is named: `Samples::interleaved_len(layout)`
  and `Samples::from_interleaved_len(len, layout)`.

  Migrating an implementor: return `Samples(n)` instead of `n`, or
  `Samples::from_interleaved_len(out.len(), self.layout())` for "the whole
  buffer". A caller comparing against zero uses `.is_zero()`.

- **`ChannelLayout` no longer implements `Default`.** It defaulted to `STEREO`,
  and that guess leaked through every `#[derive(Default)]` on a struct holding
  one — which is how `Topology::default()` once declared two global inputs.
  Name the width: `ChannelLayout::STEREO`, `::EMPTY`, `::from(n)`. Knock-on:
  `tutti_vst2_host::PluginInfo` no longer derives `Default`; `EncodeConfig`
  (stereo), `PeakBlocks` and `PeakAccum` (empty) have hand-written defaults
  that name their width.

- **`tutti-midi-io` is renamed `tutti-midi-hardware`.** The crate is the OS port
  edge and nothing else; `-io` invited the reading that it also covered file I/O,
  which is `tutti-midi-file`'s job. `bevy-tutti`'s `midi-hardware` feature now
  shares the crate's name, which is the intent — the feature does nothing but
  pull the crate in.

- **`tutti-midi-hardware` no longer re-exports the SMF / Clip File codecs.** The
  `tutti_midi_io::smf` and `::clip` spellings are gone, along with the
  `tutti-midi-file` dependency behind them. Under the old name passing the file
  API through was merely odd; under `-hardware` it was a category error, since a
  file is not a device. Depend on `tutti-midi-file` directly — `bevy-tutti` and
  the known hosts already did, and nothing used the re-export.

  `Error::File(tutti_midi_file::Error)` went with it. It existed only to make
  the re-exported codecs share this crate's `Result`, and nothing ever
  constructed or matched it; a consumer that reads files owns that error itself
  (`bevy_tutti::midi::file::MidiFileError::Smf`).

- **The MIDI delivery traits split on arity.** There are now four, in two pairs,
  one pair per direction — keeping the `In`/`Out` axis `AudioIn`/`AudioOut` set:

  |          | one stream | one of many, by id |
  |----------|------------|--------------------|
  | **push** | `MidiOut`  | `MidiRouter`       |
  | **pull** | `MidiIn`   | `MidiUnitIn` *(new)* |

  `MidiIn` previously spanned both columns, carrying a `unit_id` for the fan-out
  half. Three of its four implementors treated that parameter as dead weight —
  two checked it against an id they already owned, one ignored it — and the
  hardware edge had to be polled with a `MidiUnitId::new(0)` **sentinel**. Since
  `0` is a real id, a per-unit source installed at that seam would silently have
  received the entire hardware stream. That install is now a compile error.

  Migrating an implementor: if it ignored `unit_id`, implement `MidiIn` and drop
  the parameter (`poll_into` → `poll_block`). If it dispatched on it, implement
  `MidiUnitIn` (`poll_into` → `poll_unit`). `MidiInPort::install` and
  `MidiPreBlock::set_input` now take the respective trait.

  `MidiReceiver` no longer implements a read trait at all. Its inherent
  `poll_into(&self, out)` is unchanged and is what every caller already used —
  the trait impl's only added behaviour was a check its sole caller deliberately
  routed around.

- **`MidiOut::queue` returns the accepted count** (`()` → `usize`). A sink that
  cannot fail returns `events.len()`; a short count means the rest were dropped.

  This fixes a live bug: `MidiSession::send` returned `events.len()` whenever an
  output was open, under a doc promising an *accepted* count, so a device
  refusing every event reported full success. Both OS backends now count what
  they wrote and stop at the first failure, so the number names an unbroken
  prefix rather than a subset with holes.

  **The return value's meaning changed without its type changing** — a consumer
  treating `send(&e) == e.len()` as "connected" now correctly gets `false` when
  the device is refusing.

- **`InputConnection` carries no methods.** Its doc said exactly that and then
  declared `fn endpoint()`, which had no callers — the id is already the key of
  the map connections are stored under. The trait remains as the RAII marker its
  doc describes: dropping it closes the port.

- **`tutti-midi-io` is native-UMP; `midir` is gone.** MIDI reaches the wire as
  UMP words on every supported platform, so MIDI-2-only messages — per-note
  controllers, per-note pitch bend, JR Timestamps — survive. They were
  previously dropped in silence by `to_midi1_bytes` returning `None`.
  - macOS: CoreMIDI, no new dependencies.
  - Linux: ALSA UMP sequencer, needs alsa-lib ≥ 1.2.10. `build.rs` probes the
    version and degrades to a stub with a `cargo:warning` below that floor, so
    older distros still build.
  - Windows: a stub. No `windows` crate version binds Windows MIDI Services.
  - JR-stamped output used to be macOS-only, because `UmpOutRes` wrapped a
    CoreMIDI type. It now takes an erased sink, so every backend gets it.

### Fixed

- **Four tutti-sampler reads were wrong on the memory tier, three of them on
  a forked disk voice too** (design doc 013's follow-ups to "Disk voices
  export": the placed `MemorySource` rate, S1, S3 and N2). Each reached live
  playback. `MemorySource` (bare, or in a `VoicePool` / `VoiceNode` slot) and
  the offline read a forked `DiskVoice` plays now read a position through the
  same code, so the tiers cannot part again:
  - **A placed file at another rate than the session's played at the wrong
    speed within a block.** The in-block step was varispeed alone; it is now
    varispeed × `file_rate / session_rate` (× the stretch on the stretched
    path), while the gate's origin keeps varispeed alone. A 24 kHz clip on a
    48 kHz clock read frames 60…63 and then 32 at every 64-frame block; it
    now reads half a frame per frame, monotone. A placed read seats on the
    clock and steps from there, so `tick` against a clock that moves once
    per block steps through the block as `process` does (it repeated one
    frame).
  - **Reverse past a file's first frame held frame 0 as DC.** It is now
    silent, as forward is past the end (memory tier and disk fork; the
    butler's reverse refill already pushed silence, now pinned).
  - **A loop crossfade replayed the loop's head.** It blended the tail
    toward `[start, start + fade)`, then the wrap played `start` again: a
    jump of `fade` frames at every loop. It now blends toward the frames
    that lead *into* the start, `[start - fade, start)`, with frame `k`
    weighing the lead-in `(k + 1) / (fade + 1)`, so the wrap continues
    seamlessly. With fewer than `fade` frames before the start (a loop from
    frame 0), the tail blends toward the loop's own head `[start, start +
    fade)` and the wrap resumes at `start + fade`, still continuous; the
    fade is at most half the loop there, and at most the loop otherwise.
    `LoopSetting::On` documents the fade actually used. The butler captures
    its crossfade buffers by the same rule.
  - **Interpolation taps next to a loop's end read the file past it.** They
    now wrap through the loop's start (and, behind the start after a wrap,
    read the loop's last frame): the frame sequence the butler's ring holds.
  - **A placed `MemorySource` honours its loop** going forward, as a disk
    voice's stream does; it ignored it. Loop points are whole frames,
    truncated, as the butler takes them, and a loop's end is clamped to the
    file.
  - A placed read's seat keeps the rate it steps by: a varispeed or stretch
    change before the clock moves continues from where the read stands,
    instead of rescaling the frames already stepped. A stopped clock
    silences a placed read at once, through `tick` and `process`.
  - `MemorySource` reads a loop's fade from the wave in place: its internal
    crossfade buffer, and that buffer's 4096-frame cap on `crossfade_frames`,
    are gone, and `loop_setting()` returns the crossfade length asked for.
  - **Not fixed: a live disk voice on a looped stream.** The butler's loop
    bookkeeping compares a consumed-frame counter against the loop's file
    frames, so past the loop's end it flushes the ring on every cycle; doc
    013 records it ("The live disk loop") and the fix.

- **A synth instrument exported silent on the native graph** (design doc
  013, PR 12's follow-up). A forked `PolySynth` or `SoundFontUnit` was
  cloned from its `Legacy::controlled` shadow, whose MIDI port was severed
  when the node was inserted — before any `MidiSourceInstall` clip reached
  the live port — so a native export played none of its notes. Each synth
  now has a fork source of its own, as a hosted plugin has:
  - `PolySynth::fork_source` / `fork_instance` and
    `SoundFontUnit::fork_source` / `fork_instance` (new) keep a template
    sharing the live synth's port and control cells; a fork isolates it,
    rebinds the live port's clip onto the render's timeline
    (`MidiInPort::rebind_offline_into`) and resets. A MIDI source that
    cannot be rebound fails the fork by name — `tutti_polysynth::Error::MidiSource`
    and `tutti_soundfont::Error::MidiSource` (new variants), reaching a host
    as `ExportError::ForkSource` naming the synth's entity — never silence.
  - **Any registered MIDI-receiving unit exports its clip, or refuses by
    name — never silent.** `MidiTargetRegistry` now captures how a unit
    forks as well as its port, and bevy-tutti's native backend hands that to
    the editor: a type's own source (`MidiNode::fork_source`, new and
    defaulted `None`; the two synths override it), or the generic fork — the
    node's shadow, with the live port's clip re-installed, rebound, on the
    fork's port by a hook (`tutti_graph::Legacy::with_fork_hook`, new). A
    source that cannot be rebound is
    `bevy_tutti::midi::MidiForkError::NotRebindable`. New
    `AudioGraphRes::insert_with` / `replace_with` take the unit's
    `CapturedControls`; every insertion path in the crate uses them. A unit
    with a captured port pushed with the plain `insert` refuses a native
    export that holds it (`ExportError::NotForkable`, naming its entity).
  - A `PolySynth` fork reads its `Param` cells (volume, unison detune and
    spread) at the fork, not at insert as the shadow did; `isolate`
    detaches them, so a live move afterwards does not reach the render.
  - A `SoundFontUnit` fork shares the decoded SoundFont and keeps the live
    unit's preset, and renders at the export's rate: its node re-rates on
    prepare through `SoundFontUnit::with_sample_rate` (new; a copy at another
    rate, keeping the channel state, via the vendored
    `Synthesizer::with_sample_rate`). A live unit's rate stays fixed.

- **`FileIn` streamed every Ogg Vorbis file as empty.** Vorbis's first packet
  decodes to zero frames, and the streamer took any zero-frame packet for
  end-of-stream, so a streamed `.ogg` clip played silence from its first block
  (and a seek's preroll stopped early). `tutti_sampler::probe` still reported
  such files streamable, since the container carries a frame count. Zero-frame
  packets are now skipped; a streamed read now yields the same samples as
  `Wave::load`, bit for bit, for every fixture in `assets/audio/`.

- **A `FileIn` seek into an Ogg Vorbis file landed up to ~1024 frames late.**
  After a seek resets the decoder, its first Vorbis packet only primes the
  overlap; the preroll discard was counted from the seek's `actual_ts` rather
  than from the packet that produced audio, and when the primer was the packet
  holding the target frame no discard could recover it. `seek` now counts from
  the producing packet's timestamp and steps back past a primer that swallowed
  the target. Every loop wrap, seek and reverse refill on a streamed Ogg clip
  was off by ~23 ms. Seeks now land bit-exact on `Wave::load`'s frames for
  every fixture, lossy included.

- **Four defects in `Routing::route`, the arithmetic plugin delay
  compensation is computed from.** Each was inert in the tree as it stood,
  because every in-tree caller happened to pass the argument that makes the
  wrong answer and the right one coincide — which is exactly why they
  survived, and why a fix now costs nothing and later costs a version.

  - **`Routing::Arbitrary` added the node's own latency once per input**, so
    a node's reported latency depended on the *order its input channels were
    wired*: sources at 0 and 10 with a node latency of 5 gave 10 one way
    round and 5 the other. `route` now folds the inputs with no extra
    latency and adds the node's own once at the end. Live case:
    `fundsp-tutti`'s `resynth.rs`, the only caller passing a non-zero value.
  - **`Routing::Generator` was unreachable for zero-input generators**, i.e.
    all of them — the empty-input guard ran before the match, so `noise`,
    `envelope`, `wave`, `sequencer`, `shared` and `ring` all reported
    `Unknown`. Handled ahead of the guard now. Every caller passes `0.0`
    today and `tutti-export` does `unwrap_or(0.0)`, so nothing observable
    changes; the first generator to declare a non-zero latency would have
    had it silently dropped.
  - **`Routing::Join` panicked on two shapes**, both reachable from
    `AudioUnit::latency`/`response` rather than from audio processing: more
    outputs than inputs indexed past the end, and zero outputs divided by
    zero. It now answers an empty frame for zero outputs (as every other
    variant does) and leaves un-fed outputs `Unknown`.
  - The same guard also swallows `Routing::Reverse`'s `assert_eq!`. **Left
    as is, deliberately**: an empty frame means "no signal information", not
    a width mismatch, and making the assert fire there would put a new panic
    on the graph-commit path. Documented at the guard and at the test.

- **`PolySynth` was capped at 16 voices for a removable reason.**
  `PolySynth::new` rejected any `max_voices` above 16 because
  `finished_indices` was a `SmallVec<[usize; 16]>` and a spill would have put
  a `malloc` in the audio callback. The reasoning was sound and the tool was
  wrong: a `Vec` sized to `max_voices` at construction and only `clear()`ed
  never reallocates, which is the same guarantee with no ceiling. 16 voices
  is low for a sustain-pedal part, and the cap is gone; `smallvec` is no
  longer a dependency of this crate.

  The steady-state no-alloc test now runs at **64 voices**, which makes it
  strictly stronger — at 16-of-16 the collection was always inline, so it
  passed whether or not the drain touched the heap.

  A second test covers what that one structurally cannot: it warms the
  *thread* and gates a *fresh instance*, including a `Clone` (the shape
  `Net::commit` hands a running callback). Both construction sites are
  mutation-covered; an earlier draft covered only `new` and the `Clone`
  mutation passed against it.

- **Found while doing the above, not fixed: the first block on a cold thread
  allocates.** A `PolySynth` that has never been processed, with no MIDI and
  no active voices, allocates 128 bytes on its first `process` — and it is
  **per thread, not per instance** (a fresh synth on an already-used thread
  allocates nothing). The path is `poll_midi_events_sorted` ->
  `MidiInPort::poll` -> `self.source.load()` on an `ArcSwapOption`;
  `arc-swap` initialises its per-thread fast slots lazily. It lands on the
  **first callback of any new audio thread**, and `CpalDriver::restart`
  makes a new one on every device switch. The whole `rt_no_alloc` suite was
  blind to it because every test warmed the instance, and therefore the
  thread, before opening the gate. Recorded as an `#[ignore]`d test at
  `tutti-polysynth/tests/rt_no_alloc.rs`; the fix belongs in `MidiInPort`.

- **`Recorder` could not await a finite take's natural end.** `stop()` clears
  the run flag *before* joining, which is right for a live source and wrong
  for a finite one: a source that has not reached its end is cut off wherever
  the pump happened to be, and the take is silently truncated — a short file,
  no error anywhere. `FinalizeStatus::is_done` is set only by the
  `Drop`/`stop` shutdown, not by the thread breaking on
  `OnEmpty::EndOfStream`, so it could not be polled for this either. New
  `Recorder::wait()` joins without touching the flag, so the loop breaks
  where it was always going to. It shares `shutdown`'s join half rather than
  copying it; the one line that differs is the one that truncates. On a
  `Starved` source it blocks forever — deliberately, since a timeout would
  report a complete take with no idea whether it was one.

- **Clip-file round trips duplicated their metadata.** Tempo and time
  signature are Flex Data *events* in M2-116, so `read_clip_file` hands them
  back inside `ParsedClipFile::events` — and `write_clip_file_with_header`
  then emitted them a second time from the `ClipHeader`. Parse, write, parse
  grew a duplicate pair every cycle. Nothing errored and a reader takes the
  *first* declaration, so the file kept playing correctly while accumulating
  junk; an open-and-save loop was quietly corrupting user data.
  `write_clip_file_with_header` now **replaces** a header the events already
  carry rather than prepending to it, stripping only the leading zero-delta
  run so a mid-clip tempo change is untouched. New `ParsedClipFile::header()`
  returns the pair to hand back, and is `Some` only when the file declares
  both halves — substituting `ClipHeader::default`'s 120/4-4 for a missing
  one would write a tempo the file never claimed.

- **`reset_owners` has been a no-op, and three doc comments said otherwise.**
  Found by mutation-testing: deleting the `reset_owners()` call from a stream
  restart changes nothing. Every call in the chain bottoms out in
  `AudioThreadCell::reset_owner`, whose own doc reads "the cell pins no owner
  thread, so a device switch needs no reset" — the cell's debug check detects
  a *concurrent borrow*, not a foreign thread. `RtEventBuf::reset_owner`
  already admitted this; `AudioCallbackState::reset_owners` and
  `MotionFsm::reset_owner` still claimed "the owner checks would otherwise
  flag the new thread as an intruder". The comments are corrected. The calls
  stay — they are public API, and the property is one a future cell might
  reinstate — but nothing should be written that depends on them acting.

- **Plugin MIDI-out reached nothing.** `tutti-cpal` held an
  `Option<MidiPostBlock>` and called `run()` in the audio callback, but nothing
  anywhere *constructed* one — so the whole outbound path was assembled, tested,
  RT-safe, and unreachable. `bevy-tutti`'s engine build now creates it from the
  same routing snapshot and bus the inbound phase uses, so a node's MIDI-out is
  routed by exactly the rules a hardware input is.

  The new `MidiOutSinkRes` publishes the collection point; a host hands it to
  whatever emits (`plugin.set_midi_out(sink.handle())`). It is deliberately
  **not** installed automatically: an inbox is an *address* and costs one map
  slot, but a sink is a *routing decision* — and a plugin's MIDI-out capability
  is a per-instance negotiated fact that the node's Rust type cannot answer.

  `tutti-midi-runtime`'s new `outbound_block_path` test assembles the entire
  round trip with no Bevy in scope, pinning the path as engine-side: if it ever
  needs an adapter type to compile, the adapter has stopped being a wrapper.

### Added

- **`just check-features` / `just test-features`, and a `dark features` CI job
  — the feature-gated code nothing was compiling.**
  `cargo tree --workspace -e features -i tutti-cpal` reported only `default`:
  nothing in this workspace turned `tutti-cpal/capture`, `tutti-cpal/midi` or
  `bevy-tutti/audio-io` on, so `just test`, `just lint` and every CI job
  typechecked none of them. All of `tutti-cpal/src/mic.rs` was uncompiled, so
  were the `pre_block`/`post_block` arms of `process_audio` — the ordering that
  module's header calls "the design" — and `bevy-tutti/tests/audio_io_pump.rs`,
  twelve tests, **had never once run in this repo.** (They pass. That is luck
  rather than evidence, which is the point.) This is the same hole
  `just check-windows` exists to close and has the same failure mode: a cfg
  block nothing compiles is a cfg block nothing lints, and it rots in silence.
  Both recipes are in `just ci`. Adding a feature means adding a line there.

- **`tutti` — the Bevy-free umbrella, and it contains no code.**
  `bevy-tutti` was the only one-dependency entry point, so a headless
  consumer hand-wired a dozen path deps. `tutti-export`'s own showcase
  example names five crates; through the façade it names one, and
  `crates/tutti/examples/headless_export.rs` is that rewrite (the original
  stays put — it proves `tutti-export` is usable standalone).

  The façade also reaches strictly more than `bevy-tutti` does: that umbrella
  depends on neither `tutti-analysis` nor `tutti-node`, which is why
  `export.rs` could not have been written through it either.

  **The history is easy to misread and the docs now say so.** A `tutti`
  package was deleted once — but it was the *workspace root package*
  (`9c75ec54`: root `Cargo.toml` with both `[workspace]` and `[package]`),
  and it held `TuttiEngine`, a builder, `TuttiDriver` and the CPAL callback.
  `0a4adf68`/`4b5bd2fd` dissolved it because `bevy-tutti` needed that logic
  and two stacked umbrellas, where the lower owns what the upper needs, is
  one too many. **Re-exporting was never the problem; owning logic was.**

  So the rule is enforced, not merely written: `crates/tutti/tests/no_logic.rs`
  reads `lib.rs` and rejects any statement that is not a `pub use` or
  `pub mod` (by shape, not by keyword blacklist — a blacklist misses a type
  alias or a const), and `scripts/check-canonical-paths.sh` gains a check that
  every whole-crate re-export there is aliased, so `tutti::tutti_core::…` is
  unspellable. Both were verified by making the violation and watching them
  fail.

  `just check-bevy-free` and the CI job gain a **negative dependency
  assertion** — `cargo tree -p tutti --features full -e normal` must contain
  no bevy crate at any depth. A compile proves the crate builds; it says
  nothing about what came in with it, and the repo had no guarantee of that
  shape before. Also verified by making it fail.

  `bevy-tutti` deliberately does not depend on it: it would keep its direct
  edges anyway for their `bevy` features, ending with two edges to each crate.
  Its own `full` is otherwise transcribed unchanged, minus every `/bevy`
  forward — that difference *is* the crate.

- **Benchmarks, and the first numbers this engine has ever had.**
  Five criterion suites (`engine_render`, `audio_callback`, `polysynth`,
  `voice_pool`, `offline_render`), a `docs/benchmarks.md` baseline naming the
  machine it was taken on, and `just bench` / `bench-save` / `bench-cmp` /
  `bench-smoke`. `criterion` is unified on 0.8 in `[workspace.dependencies]`;
  `tutti-core` carried a dead 0.5 that never had a `[[bench]]` while the
  vendored fork was already on 0.8, so the tree resolved two of each.

  What the numbers say, on a Ryzen 9 9950X:
  - **~5,500 simple filter nodes** fill a 64-frame block's 1.333 ms budget,
    single-threaded, and scaling is linear.
  - **The real callback costs ~34% more than the graph render** — metering
    and the stereo fold, not the format conversion (i16 adds only 4% over
    f32). A graph-only benchmark understates the audio thread by a quarter.
  - **The phase vocoder costs 16×** the bypass path (10.2 µs → 165 µs at 8
    voices). It is by far the most expensive thing in the sampler.
  - **A FLAC export is ~98% encoder, ~2% engine.**
  - `PolySynth` was **hard-capped at 16 voices** (`FINISHED_NOTES_CAPACITY`).
    The cap is now removed — see Fixed.

  Three drafts produced *wrong* numbers before these, and the reasons are
  recorded in the bench headers because each is a trap the next person will
  hit: `max_voices` defaults to 8 so every polysynth case above 8 measured
  identically; `BufferArray<U2>` is 64 frames wide so a "512-frame" axis was
  reporting the 64-frame cost; and detuning each sampler voice by a cent put
  all but the first through the pitch shifter, making plain playback look
  105× superlinear.

  **CI gates none of it.** Runners swing 30–50% and `profile_stretch_clone`
  documents an 81× spread on a quiet machine; a flapping perf gate earns
  `continue-on-error: true` within a month and then tests nothing. The
  `bench-smoke` job proves the harnesses still *run*, and the real gate is
  the new `tutti-core/tests/alloc_budget.rs` — allocation counts are
  machine-independent, so a budget on them survives a shared vCPU.

- **The device layer has a seam, a host selector and a fault sink.**
  `tutti-cpal` had four `cpal::default_host()` calls, no device abstraction of
  any kind, and 6 tests. JACK was unreachable even with cpal's `jack`
  dependency compiled in, because `default_host()` returns ALSA regardless —
  the host has to be *named*. It now has 29 tests, none of which opens a sound
  card.

  - **`StreamDriver` / `RunningStream`, with `CpalDriver` and
    `ManualStreamDriver`** — the direct analogue of `tutti_io`'s
    `PumpDriver`/`ThreadDriver`/`ManualDriver`, and it earns the same claim:
    `CpalDriver`'s closure body is `move |data, _| block.render(data)`, so it
    is not a second implementation of the callback. `AudioEngine::from_spec`
    plus a manual driver gives a complete lifecycle — start, render, fault,
    stop, restart — with no device. `AudioEngine` holds a
    `Box<dyn RunningStream>` rather than a type parameter, for the reason
    `Recorder`'s field doc gives: a generic would push the driver choice into
    `TuttiDriver`, into `bevy-tutti`'s `NonSend`, and into every host field.

  - **`AudioHost` / `DeviceHost` / `DeviceSelector`**, and the `jack` feature.
    Every `AudioHost` variant exists on every platform on purpose — cpal's own
    `HostId` is cfg-generated, so mirroring it would make a host's config
    struct a different type per OS; an unreachable host is
    `Error::HostUnavailable` at runtime instead. `DeviceSelector::Name`
    survives the re-enumeration that invalidates an index, which is the best
    available answer while cpal 0.15 exposes no hot-plug notification.
    `just check-jack` ships with the feature rather than after it: cpal
    declares no `jack` feature of its own (it is the implicit feature of an
    optional dep in cpal's Linux/BSD target table), so the code is invisible
    to every other recipe — the same shape as the `#[cfg(windows)]` gap that
    once took Windows from 37 failures to 47.

  - **`StreamFaults`** — both error callbacks were literally `|_err| {}`, so a
    device unplugged mid-session surfaced *nowhere*: `is_running()` stayed
    true and the host went on reporting a healthy stream to a user hearing
    silence. Faults are now accumulated behind a handle taken before anything
    goes wrong (the `FinalizeStatus` shape, publication order and all), and
    `bevy-tutti`'s `AudioDeviceState` mirrors them per frame.

    **`AudioEngine::is_running()` is a behaviour change without a signature
    change**: it can now return `false` while a stream object exists, because
    it consults the disconnect flag. That was the defect, not the contract.

  - **`MicIn::open` takes the graph's sample rate.** A breaking change, and
    deliberately not offered as an opt-in overload, because an opt-in safe
    path reproduces the bug it fixes. `MicMonitorNode` does not resample — its
    `set_sample_rate` is a documented no-op resting on "the device layer opens
    the mic at the graph's rate" — and *nothing enforced that*: `MicIn` took
    whatever the input device reported while `AudioEngine` took whatever the
    output device reported, and the two were never compared. A 44.1 kHz mic on
    a 48 kHz graph drifted silently for the length of the take. Now
    `Error::SampleRateMismatch`, decided by a free `choose_input_config` that
    needs no device to test, with a paired `debug_assert` in
    `MicMonitorNode::set_sample_rate` — the two-check shape `pump`'s layout
    assert and `Recorder::start`'s error already use.

- **Coverage where a silent wrong answer reaches a user's recording.**
  The plugin subsystem carried ~1,731 tests; `tutti-io` had 27 and
  `tutti-midi-file` 13, with no integration tests and no input files of any
  kind. Those are the crates every consumer hits on day one. Now 36 and 27.

  `tutti-io` gains `tests/recorder_thread_driver.rs` — the **production**
  driver's first tests ever; every existing `Recorder` test drives a
  `ManualDriver`, so `thread::spawn`, the Acquire/Release stop handshake, the
  `PumpPass::Ended` break, `IDLE_PARK`, and `impl RunningPump for JoinHandle`'s
  "recording thread panicked" arm had never run. Plus
  `tests/tap_to_wav_roundtrip.rs`, the `AudioTap → TapIn → Recorder → WavOut`
  end-to-end that existed only as a README doctest nextest does not run, and
  the first coverage of `FinalizeStatus::error()` returning `Some` — the whole
  reason that handle carries an error rather than just a done flag.

  `tutti-midi-file` and `tutti-midi-types` gain an independent SMF
  encoder/decoder in `tests/support/`, written from the spec. `midly` is the
  wrapped dependency so it cannot be its own second opinion, and the
  alternatives do not qualify (`nodi` wraps midly, `rimd` is unmaintained);
  for MIDI 2.0 Clip Files there is no second implementation anywhere. The
  builder doubles as the fixture generator, including the malformed cases a
  committed corpus could not carry. Highest-value additions: a delta on a
  sysex must still advance the beat grid, LIFO pairing of overlapping notes,
  per-channel pairing, and — for the 1,154 hand-rolled lines of clip codec —
  "no prefix of a valid file is valid, and none panics", which sweeps every
  length bound at once.

  Every test was mutation-checked by actually running the mutation. Two drafts
  **passed** under the mutation they were written to catch and were rewritten:
  waiting for a source to drain proves nothing about the `Ended` break (a
  spinning driver still finalizes correctly), and two notes opened
  simultaneously pair identically whether or not the channel is in the key.
  Both cases are recorded in the test headers, because the next person will
  reach for the same first draft.

- **`MicMonitorNode::tick` no longer discards a frame on a short buffer.**
  It called `next_frame()` — which pops the ring — and *then* checked
  `output.len() >= 2`, so a narrow buffer consumed a captured frame and wrote
  nothing. Unreachable through fundsp, which always hands a 2-out node a 2-wide
  buffer, so this was latent rather than live; fixed because a discard is never
  the branch you want on a path whose job is not losing frames, and ordering
  the check first costs nothing.

- **`tutti_cpal::OutputBlock`** — the output callback, liftable out of CPAL.
  `process_audio` was only ever the *inner* seam. Everything around it lived
  inside the closure handed to `build_output_stream`: the `MAX_FRAMES` clamp,
  the zero-fill, the stereo metering fold, `meter_output`, and the eight-way
  sample-format conversion. Nothing but CPAL with a real sound card open could
  run any of it, which is why none of it has a test. `OutputBlock::render` is
  that body, and CPAL's closure is now `move |data, _| block.render(data)` —
  not a second implementation of the callback, the same claim
  `tutti_io::ManualDriver` makes about `PumpLoop::pump_once`.

  The `debug_assert!` on the callback size deliberately stays at the CPAL
  boundary rather than moving into `render`: it is a claim about *CPAL's*
  contract, and keeping it out is what will let a debug-build test observe the
  clamp instead of panicking before it.

- **`tutti-midi-file`** — the SMF and MIDI 2.0 Clip File codecs, split out of
  `tutti-midi-io`. Reading a `.mid` needs no OS MIDI port, and pairing the two
  behind one feature flag meant a consumer wanting only the codecs linked
  CoreMIDI unless it knew to pass `default-features = false`. `tutti-midi-io`
  re-exports the file API, so `tutti_midi_io::smf` still resolves.
- `MidiSession` replaces `MidiIo`: same job, no driver code, no background
  threads, and an output sink that is **absent** rather than silently
  discarding when nothing is connected. `send` returns the accepted count.
- `UmpCapability` records what an endpoint can carry — protocol and function
  blocks — read from the OS rather than assumed.

### Removed

- **`tutti-midi-io`'s `midi-hardware` feature.** With the file codecs split out,
  the crate contains nothing that is not OS MIDI, so the flag no longer named an
  axis: turning it off left the whole session/port surface compiled with no
  backend to drive it. Platform gating is `cfg(target_os)`, which is what
  actually decided this all along. `bevy-tutti` keeps its own `midi-hardware` —
  that axis is still real.
- The MIDI-1.0 `VirtualMidiSource` / `VirtualMidiDestination` pair, which had no
  consumers.

### Fixed

- SysEx reassembly, extracted to `tutti-midi-io`'s `Sysex7Assembler` and now
  unit-testable off the driver thread. Three bugs it had been hiding: bytes
  after the terminating `0xF7` were discarded (a device packing two dumps into
  one buffer lost the second); the buffer had no ceiling, so a lost `0xF7` grew
  it for the lifetime of the connection; and each completed message allocated
  twice on the driver callback thread. A mid-run `0xF0` now restarts the run
  rather than being kept as payload, where the 7-bit mask silently turned it
  into `0x70`.

## [0.0.1] - 2025-01-29

### Added
- Initial release of Tutti audio engine
- Core audio graph runtime with FunDSP integration
- MIDI subsystem with I/O, MPE, and MIDI 2.0 support
- Sample playback with Butler thread and time-stretch
- DSP building blocks: LFO, dynamics, envelope followers, spatial audio
- Plugin hosting for VST2, VST3, and CLAP (multi-process with crash isolation)
- Neural audio synthesis and effects (GPU-accelerated)
- Audio analysis tools: waveform, transient detection, pitch detection
- Offline audio export (WAV, FLAC)
- Real-time transport with tempo mapping
- EBU R128 LUFS metering
- Plugin Delay Compensation (PDC)
- Modular feature flags for flexible builds
- Ergonomic graph API with `pipe()`, `node_mut()`, `add_split()`, `add_join()`
- 9 comprehensive examples showcasing core features

### Architecture
- Workspace with 8 independent crates
- Lock-free audio thread design
- Framework-agnostic (works without Bevy/egui)
- MIT OR Apache-2.0 dual license

[Unreleased]: https://github.com/PoHsuanLai/Tutti/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/PoHsuanLai/Tutti/releases/tag/v0.0.1
