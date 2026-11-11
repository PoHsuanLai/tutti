# 029: plugin API, the user's surface

[021](021-plugin-hosting-axes.md) gave the plugin crates their structure: a
format is a `Loaded`, a hosting is `InProcess` or `OutOfProcess`, and the
server is one `Served<F>`. [023](023-plugin-capabilities.md) gives a plugin's
self-description tutti's types. Neither is about what a DAW author writes.
This design is: the surface a host application touches, from the scan to the
bounce, read as a user would read it.

It was written by walking that path (scan, load, wire, play, control, edit,
save, bounce) against the code as it stood after 021 step 5 (#625), and noting
every place the API asks the caller to know something it should not have to:
an internal layer's name, a tri-state `Option`, a callback slot, a job to
schedule by hand, a difference between hostings. Section 2 lists them by
surface. Section 3 is the one shape that replaces them. Section 4 is the steps.

It folds in 021 step 6 (the facade: `hosts!`, `AnyHost`, `Hosting`, `open`),
whose purpose was the same and whose shape this design fixes.

## 1. Principles

1. **One way in.** The session opens plugins. The catalog says what exists and
   refuses what is blacklisted; it does not open. A user who finds a second
   path finds one without crash recovery, meters or wiring.
2. **A plugin is a view, its surfaces are sub-views.** `session.plugin(node)`
   lends a `Plugin<'_>`; parameters, presets, state, the editor, buses, health
   and diagnostics are borrowed views of it. A handle cannot outlive the plugin
   it names, and a persisted reference is an id, re-resolved.
3. **What happens to a plugin is an event.** One stream, the session's
   `SessionEvent`, carries every change a plugin reports and every transition
   of its life. No callback slots, no polling calls.
4. **Hosting does not show.** A value means the same thing in process and out
   of process; no accessor is `None` because of which hosting the user got.
5. **The main thread is a view, not an argument.** The proof stays a type
   (`MainThread`, 016 §2.9a), but a caller holds it once, in a
   main-thread view of the session, and no call below that view takes it.
6. **Types over checks, at the boundary, once.** A parameter's unit, a
   state's owner, a plugin's identity in its bundle: checked when the handle is
   made, then carried by the type. A doc comment that says "read through X,
   not directly" is a field that should be private.
7. **The engine schedules its own work.** Blocking jobs (a load, a snapshot, a
   reload, a parameter read) run on a spawner the host supplies once, and land
   in `update()`. The host does not write spawn-and-land loops.

## 2. The surfaces as they are

Each row is what a caller writes today, what is wrong with it, and where it
goes.

### 2.1 Scan and load

| Today | Problem | Becomes |
|---|---|---|
| `Plugins::load(config).with_fresh_scan()` | `with_` is a setter by convention; this runs a blocking scan and discards its `ScanResult`, `ScanBusy` included | `Catalog::open_db(config)`, `catalog.scan() -> Result<ScanResult, ScanBusy>` |
| `PluginLoadRequest { id: PluginId::from_path(entry.path), state }` | `PluginId` is a path. The entry the user picked carries the plugin's id inside its bundle, and the request drops it: a multi-plugin bundle loads its first plugin (`Load::new()` in `tutti-plugin`'s open) | `PluginRef` (path, format, plugin id) from `&PluginEntry`; `PluginRef::file(path)` for a file found by hand; `PluginLoadRequest::new(plugin_ref)`, `.restoring(state)` |
| `Plugins::open(main, path, rate)` and `Session::plugin_load(..).run()` → `add_plugin(&main, ..)` | Two ways to open; the first has no recovery, meters or wiring | the session's only. `Plugins::open` is removed |
| `plugin_load` / `add_plugin` / `plugin` / `plugins` | A noun-phrase constructor among verbs | `load_plugin(r) -> PluginLoad` |
| `add_plugin(&main, ..)` always takes the token | Only an in-process (VST2) load needs it; a headless, server-hosted app must still claim one | taken by the main-thread view (§3.2); a load with no in-process plugin needs none |

### 2.2 State

| Today | Problem | Becomes |
|---|---|---|
| `HostedPlugin::snapshot()` | Reads as "save now". It is the last state captured while healthy, the crash-recovery copy; a project saved from it is stale | one view, `plugin.state()`: `last_good()` is that copy; saving is `state().save()` |
| state as `Vec<u8>` everywhere | A state can be restored into another plugin, or another version of the same one, and nothing says so | `PluginState { plugin: PluginRef, bytes }`; restoring into a plugin that is not its `plugin` is refused (`StateError::OtherPlugin`), by `PluginLoadRequest::restoring` and by `state().restore` |
| `withheld_snapshot()` | Correct (the state a reload left out because the plugin died loading it), named after the old word | `state().withheld()` |

### 2.3 Parameters

| Today | Problem | Becomes |
|---|---|---|
| `handle().any_thread().parameter_descriptors() -> Option<Vec<_>>` | `any_thread` is a hosting-layer name; `None` means "not yet", so every caller writes a retry | `plugin.params()`: a view over descriptors read at load and refreshed on the plugin's rescan notice |
| `parameter_value(id) -> Option<Normalized>` | May be an IPC round trip; `None` mixes "no such id" with "not yet" | `param.normalized()`, from the session's mirror, infallible for a resolved handle |
| `set_parameter_value(id, v)` | Nothing checks `id`; no gesture | `params.get(id)?` resolves once; `param.set(v)`; `param.edit()` is a guard: gesture begins, `set`s, gesture ends on drop |
| text: `parameter_text`, `parameter_value_from_text` | Fine, but on the wrong object | `param.text()`, `param.set_text(&str)` |
| `ParameterInfo`'s public fields, documented "read through `bounds()` / `flag()`" | The rule lives in a comment | private fields, the accessors only |
| plain values: `info.typed::<U>(plain)` | Typed, but per call | `params.typed::<Db>(name_or_id)? -> TypedParam<'_, Db>`: unit checked once against the declared `ParamUnit` (023 step 1), then `get() -> Db`, `set(Db)` through the range |
| `set_automation_mode(mode) -> Result<(), EditorError>` | Not an editor operation; wrong error | `params().set_automation_mode(mode)` |
| `on_parameter_changed(Fn)` + `clear_parameter_callback()` | One slot, silently replaced by a second subscriber; leaks if not cleared | `PluginEvent::ParamChanged { id, value, gesture }` (§3.3) |

### 2.4 Presets

| Today | Problem | Becomes |
|---|---|---|
| `presets(&main) -> Option<Vec<Preset>>`, `preset_support() -> PresetSupport` | Two calls to learn one thing | `plugin.presets() -> Result<Presets<'_>, PresetError>`; `Unsupported` is the error, `Presets::kind()` says list-and-load or load-from-file |
| `load_preset(&main, &id) -> Option<bool>` | Three answers in one value: no route, refused, loaded | `preset.load() -> Result<(), PresetError>` |
| `current_preset(&main) -> Option<PresetId>` | Fine | `presets.current()` |
| CLAP: load by path, no enumeration | Only reachable by knowing CLAP has no discovery bound | `presets.load_file(path)`; enumeration lands with 023 step 8 |

### 2.5 The editor

| Today | Problem | Becomes |
|---|---|---|
| `open_editor(&main, parent, scale) -> Result<EditorSize>`, `open_floating_editor(&main)` | The caller decides embed or float by matching an error against a feature bit | `editor.open(parent, scale) -> Result<Opened>`, `Opened::Embedded(size)` or `Opened::Floating` |
| `editor_idle(&main)` per handle, per frame | Every host loops over its open editors; a forgotten one freezes | the session's `update()` services every open editor |
| `set_editor_size`, `poll_editor_resize_request`, no rescale | Three mechanisms; a DPI change has none | `editor.resize(size) -> EditorSize` (the size applied); `PluginEvent::EditorWantsSize(size)`; `editor.rescale(scale)` |
| `editor_capabilities(&main)` | Fine | `editor.capabilities()` |
| `close_editor(&main)` | Fine; the state goes back to the audio instance (021 Q8) | `editor.close()` |

A window-system adapter (open, close, resize, rescale, embed-or-float for one
window) is what nearly every winit, eframe and iced host writes. It is not part
of this design's crates; §5 Q3 asks whether it is a crate of its own.

### 2.6 Health and recovery

| Today | Problem | Becomes |
|---|---|---|
| `is_dead()`, `cause()`, `is_crashed()`, `status()`, `liveness()`, `HostedPlugin::liveness()` | Six readings of one state machine | `plugin.health() -> Health` (`Running`, `Stalled`, `Died { cause }`, `Reloading { attempt }`, `GaveUp`) |
| `terminate(cause)` on the public handle | A kill switch among the getters | `diagnostics().terminate(cause)`, or the session's remove and replace |
| `PluginSnapshotDue(job)` → run → `land_snapshot`; `PluginParamsDue` → `land_params`; `PluginReloadDue` → `land_reload(&main, ..)` | Every host writes three spawn-and-land loops | the session runs them on its spawner (§3.4) and lands them in `update()`; the events remain as notifications (`PluginReloading`, `PluginRecovered`, `PluginGaveUp`) |

### 2.7 Diagnostics, modes, buses

| Today | Problem | Becomes |
|---|---|---|
| `server_pid()`, `block_counts()`, `dropped_parameter_events()` on the handle | Useful, crowding the main surface | `plugin.diagnostics()`, its counts named as the MIDI inlets name theirs (`lost`, `evicted`, `refused`, `late`) where the meaning is the same: a Group with no port is `refused` |
| `controls()` `None` in process; `meter()` `None` for a hand-inserted plugin | The hosting shows through | one accessor for latency, tail and meter, either hosting |
| bypass and mute: `Graph::set_mode(node, NodeMode)` | **Good**: one path, as for a native node. Kept | unchanged |
| `handle.set_render_mode(mode) -> bool` beside the session's `render_mode()` | A second path that can disagree with the session (a bounce drives the session's) | the session's only |
| `input_buses()`, `output_buses()`, `layout_support()` | A second vocabulary beside the graph's `BusRef` and `connect_bus` | `plugin.buses()` hands out the graph's bus handles (`main_input()`, `sidechain()`, `output(n)`); layout negotiation is 023 step 6 |

### 2.8 Missing

Note names, keyswitches, per-note expression declarations, voice information
and CLAP preset discovery are 023 steps 4, 7 and 8. They need no shape of their
own: each lands as a view of the one in §3.

## 3. The shape

### 3.1 The view

```rust
let p: Plugin<'_> = session.plugin(node)?;   // borrowed from the session
p.name();  p.descriptor();  p.node();
p.params()      // Params<'_>       -> Param<'_>, TypedParam<'_, U>, ParamEdit<'_>
p.presets()     // Result<Presets<'_>, PresetError>
p.state()       // StateView<'_>:  save(), restore(PluginState), last_good(), withheld()
p.buses()       // the graph's bus handles for this node
p.health()      // Health
p.diagnostics() // Diagnostics<'_>
```

A read-only `Plugin<'_>` borrows `&Session`; anything that writes to the
plugin's control side borrows the main-thread view (§3.2), where the format
needs it, or `&Session` where it does not (a parameter set goes to the edit
ring from any thread). `PluginHandle` and `AnyThread` stay as the low layer the
format hosts implement and the views are built on; they leave the umbrella's
prelude.

### 3.2 The main-thread view

`Session` is `Send` and has no main thread (a headless session has none to be
handed), so it cannot own the proof. The caller that has one makes a view:

```rust
let mut ui: MainSession<'_> = session.main(main);   // &mut Session + MainThread; !Send; no state of its own
ui.update();                                         // services editors, lands jobs
let node = ui.add_plugin(loaded)?;
ui.plugin(node)?.editor().open(&window, scale)?;
```

The freeing paths that today `debug_assert!` the main thread (a retired
in-process unit may tear down an editor in its `Drop`) run through this view
whenever the `plugin` feature is on, so the rule is the type's. `MainThread`
is passed by value (it is a `Copy` zero-sized proof; a reference to it adds
nothing). It is never derived from a window system's event loop, which winit
lets run off the OS main thread on Linux and Windows.

### 3.3 Events

`SessionEvent::Plugin { node, event: PluginEvent }`, with `PluginEvent`:

| Variant | Replaces |
|---|---|
| `ParamChanged { id, value, gesture }` | `on_parameter_changed` |
| `ParamsRescanned`, `ParamTitlesChanged` | `on_invalidate`'s parameter cases |
| `LatencyChanged`, `TailChanged`, `IoChanged` | `PluginLatencyChanged`, `on_refresh` |
| `PresetChanged(PresetId)` | polling `current_preset` |
| `EditorWantsSize(EditorSize)` | `poll_editor_resize_request` |
| `Died`, `Reloading`, `Recovered`, `GaveUp` | the existing `Plugin*` events, moved under one variant |

A host that needs a push (a parameter mirror on another thread) gets
`subscribe(f) -> Subscription`, which unsubscribes on drop; there is no
`clear_*`.

### 3.4 The spawner

```rust
Session::builder().spawn_with(|job: Job| { std::thread::spawn(move || job.run()); })
// or tokio::task::spawn_blocking, or a rayon pool
```

A `Job` is `Send` and blocks (a server launch, a state round trip, a reload).
The session hands its due jobs to the spawner and lands their results in the
next `update()`, as events. The default spawner is `std::thread`, so a headless
host writes nothing. It is the session's facility, not the plugins': the
bounce's job (`bounce_start` / `finish_bounce`) moves onto it later.
`load_plugin(..).run()` stays for a host that wants the load's latency in its
own hands.

### 3.5 Where it meets the graph and MIDI

Agreed with the graph's surface ([013](013-native-graph.md), "The session
graph's surface") and MIDI's ([017](017-midi.md)), so a host learns one way of
doing each thing:

- **The key.** `add_plugin` returns `Key<Plugin>`; `replace` and `crossfade`
  keep it. Ports come from it as from any node's (`synth.out(0)`,
  `fx.input(1)`, the event ports), and wiring is the graph's `connect`.
  `synth.param(ParamId)` is the same reference a native node's param method
  gives, so a modulation route or an automation lane targets a plugin
  parameter and a native one through one type. Values, text, gestures and
  units go through the view, which holds the plugin's descriptors.
- **Buses.** `plugin.buses()` hands out the graph's bus handles; a sidechain is
  `connect(kick.out(0), comp.sidechain())`.
- **MIDI.** A route targets the plugin node and the Group picks the port (Group
  N to CLAP note port or VST3 event bus N); there is no plugin port index on a
  route. Per-note control is MIDI 2.0's own messages (no note-expression type);
  a plugin's MIDI out is read by the same event tap as a native node's.
- **Events.** The session's rule, agreed with core and MIDI: one variant per
  area, `SessionEvent::Plugin { node, event: PluginEvent }` beside
  `SessionEvent::Midi(MidiEvent)`, `#[non_exhaustive]` at both levels.
- **Errors.** The session's rule: one variant per area. One
  `tutti::plugin::Error`, its specifics (`BridgeError`, `StateError`,
  `PresetError`, `EditorError`) as sources, is `tutti::Error::Plugin`.
- **The prelude** (feature `plugin`): `Catalog`, `PluginRef`, the load request,
  `PluginState`, `PluginEvent`, `ParamId`, `Normalized`, `MainThread`,
  `ContentScale`, `EditorSize` and the `Plugin` marker. Not `PluginHandle`,
  `AnyThread`, the bridge's tuning or a format's types.

## 4. Steps

Each is one reviewable PR that leaves `cargo xtask ci` green. Each deletes
what it replaces; no deprecated twin is kept.

| # | Step | Deletes | Test |
|---|---|---|---|
| 1 (built) | **Identity and state.** `PluginRef` from `&PluginEntry`; the load passes the plugin id; `PluginState` and `StateError::OtherPlugin`; one `state()` view with `last_good()`, `withheld()`, `save()` and `restore()`; the acceptance example `crates/tutti/examples/plugin_host.rs` (scan, load, wire, MIDI, parameters, editor, save, bounce), which each later step rewrites to its shape | `PluginId` as a path; `snapshot()`; `Load::new()` in the open | the second plugin of the CLAP test bundle loads by its entry; a state restored into another plugin is refused |
| 2 | **The view, the main-thread view and events.** `Plugin<'_>`, `MainSession<'_>`, `MainThread` by value, `PluginEvent`; `update()` services editors; the editor's `Opened`, `resize`, `rescale` | `on_*` and `clear_*`, `editor_idle` calls, `poll_editor_resize_request`, `open_floating_editor`, the freeing paths' `debug_assert!` | a plugin's own edit arrives as `ParamChanged`; an editor that requests a size raises `EditorWantsSize`; a freeing path is unreachable without the view (compile-fail doctest) |
| 3 | **Parameters.** `Params`, `Param`, `TypedParam<U>`, `ParamEdit`; `ParameterInfo`'s fields private; automation mode moved | `any_thread()` in user code, `parameter_*` free calls in the prelude | a gesture guard dropped mid-drag ends the gesture; a `typed::<Db>` on a Hz parameter is refused; a set through a stale handle after a rescan is refused |
| 4 | **Presets, health, diagnostics, modes.** `Presets`, `PresetError`, `Health`, `Diagnostics`; the per-plugin render mode goes; buses as graph handles | `presets() -> Option`, `load_preset -> Option<bool>`, the six liveness readers, `set_render_mode` on the handle | per format: a preset list loads by id, an unsupported format answers `Unsupported`; a killed server reads `Died` then `Reloading` |
| 5 | **The spawner.** `spawn_with`, `Job`; jobs landed in `update()` | `land_snapshot`, `land_params`, `land_reload` as public calls; the `*Due` events | a reload completes with no host code but `update()` |
| 6 | **The catalog and the facade.** `Catalog::open_db`, `scan()`; `Plugins::open` removed; the probe lists every plugin of a bundle (`FileState::Probed` already holds several); 021 step 6's `Hosting` as a `PluginRef` field (`Auto`, `InProcess`, `OutOfProcess`) | `with_fresh_scan`, `Plugins::open`, `Backend` | a blacklisted plugin is refused at load, not at open |

Steps 1 and 6 can run before or beside the others; 3, 4 and 5 need 2.

**As built (step 1).** `PluginRef` and `PluginState` are `tutti_plugin::catalog`'s
(serde behind `tutti-plugin`'s `serde` feature); `PluginRef::load()` is the
`Load` the loaders take, which for VST3 is now the descriptor's class id
(`vst3.<class id>`, `LoadOptions::class_id`), not a display name, so the id a scan
reports picks the plugin in every format. A `PluginClient` knows the `PluginRef`
it is (its origin's file and the id it reports), so `Graph::insert_plugin` and
`add_plugin` need no more; `Graph::host_plugin` takes the ref as an argument,
because a bare handle does not know its file. `PluginGaveUp` carries the
`PluginState` (`state`), and `state().restore` takes the `MainThread` the
handle's `load_state` takes. The editor-only instance of a served plugin loads the same plugin: the ipc
`EditorLoader` takes the `Load`, and the `PluginBridge` is made with the
`Origin` (file and `Load`) it serves. One thing stays for step 6: the probe
describes only the first plugin of a file, so a catalog lists one plugin per
bundle until the probe lists them all. `PluginClient::new` and `open` take a
`&PluginRef` (`PluginRef::file` is the explicit "the file's first"); no
path-only open is left.

## Lane order

The plugin lane is one ordered list; each item is a PR.

1. 021 step 5 (#625, merged).
2. 029 step 1 (identity and state).
3. AU MIDI input parity (Groups, `PerNoteState`, `Narrower` in `AuProcessor::send_midi`).
4. 029 step 6 (the catalog: `open_db`, `scan()`, one open path, `Hosting`; also the
   probe listing every plugin of a bundle).
5. 029 step 2 (the view; builds on Core's `Key<N>`, doc 030 §6 step 2).
6. 029 step 3.
7. 029 step 4.
8. 029 step 5.
9. 023 steps 2-3.
10. AU completeness.
11. Plugin MIDI out by Group, and the contract cases.
12. 021 step 8 (docs).

## 5. Open questions

| # | Question | Recommendation |
|---|---|---|
| Q1 | Does `Plugin<'_>` borrow `&Session`, or is it a cheap owned handle (`Arc` inside) that can be kept? | Borrowed. A kept handle is how a stale plugin gets touched after a crossfade; a host that keeps something keeps the `NodeKey` and the `ParamId` |
| Q2 | Does `PluginState` carry the plugin's version, and refuse across versions? | Carry it, do not refuse: plugins read their old states. Refuse only another plugin |
| Q3 | A window-system adapter (`tutti-plugin-winit`) as its own crate? | Yes, outside the engine's layers, depending on winit and `tutti` only; Bevy keeps its own. Owner's call: it is a new crate |
| Q4 | Without `spawn_with`, does the session spawn its own thread, or refuse to start the job? | **Decided with core:** `std::thread` is the default spawner, and the spawner is a session facility (the bounce moves onto it) |
