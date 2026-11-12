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

### Lane order

The plugin lane is one ordered list. Each item is a PR or a short series. Items 3 and 4 come from the spec audit (§6).

1. 021 step 5 (#625, merged).
2. 029 step 1 (identity and state; #645, merged).
3. The spec audit's correctness fixes (§6.2): the VST2 struct (#647, merged), then one PR per format (CLAP, VST3, AU, VST2).
4. The shared-layer fixes (§6.1): each format converts its own values, one `ParamEdit` with its gesture, `f64` on the wire.
5. AU MIDI input parity: UMP through `MusicDeviceMIDIEventList` where the unit takes it, the `Narrower` on the legacy path (§6.3).
6. 029 step 6: the catalog (`open_db`, `scan()`, one open path, `Hosting`), the probe listing every plugin of a file, VST2 shells included.
7. 029 step 2: the view. It builds on Core's `Key<N>` (doc 030 §6 step 2), its event ports and its `ParamRef`, and the editor servicing takes in VST3's run loop and resize (§6.3).
8. 029 step 3, built on item 4.
9. 029 step 4, with the buses and the transport items of §6.3.
10. 029 step 5.
11. 023 steps 2–3.
12. AU completeness, including the in-process registered AU test plugin.
13. AUv3 through the v2 bridge (§6.4) and served VST2 (§6.5). Whichever comes first builds the server's event loop.
14. Plugin MIDI out by Group, and the contract cases.
15. 021 step 8 (docs).

## 5. Open questions

| # | Question | Recommendation |
|---|---|---|
| Q1 | Does `Plugin<'_>` borrow `&Session`, or is it a cheap owned handle (`Arc` inside) that can be kept? | Borrowed. A kept handle is how a stale plugin gets touched after a crossfade; a host that keeps something keeps the `NodeKey` and the `ParamId` |
| Q2 | Does `PluginState` carry the plugin's version, and refuse across versions? | Carry it, do not refuse: plugins read their old states. Refuse only another plugin |
| Q3 | A window-system adapter (`tutti-plugin-winit`) as its own crate? | Yes, outside the engine's layers, depending on winit and `tutti` only; Bevy keeps its own. Owner's call: it is a new crate |
| Q4 | Without `spawn_with`, does the session spawn its own thread, or refuse to start the job? | **Decided with core:** `std::thread` is the default spawner, and the spawner is a session facility (the bounce moves onto it) |

## 6. What the spec audit changed

On 2026-11-12, after step 1, each format host was read against its own spec:
- CLAP: the 1.2.10 headers.
- VST3: the vendored SDK and the developer portal.
- AU: the macOS 27.2 AudioToolbox headers.
- VST2: the 2.4 ABI headers, plus our fork.

A sixth pass read the format-generic layer against all four. The reports, with every finding's spec and code citation, are kept outside the repo; this section is what they changed in the plan.

### 6.1 The abstraction

The audit asked for no format-specific case in the shared types. What it found are three places where the shared layer **flattened** something and got it wrong. Fixing them removes special cases:

1. **Value conversion belongs to the format.**
   - `ParamRange::Plain` treats every plain range as linear. As a result, `Normalized` means the plugin's own value for VST3 and VST2, but a position tutti invented for CLAP and AU.
   - The consequences:
     - A CLAP stepped parameter lands one step low, because the host computes it in `f32` and never truncates as the spec says.
     - A VST3 parameter with a non-linear `normalizedParamToPlain` gets a wrong plain value and default, and a decreasing map runs backwards.
     - AU's log and exponential display curves are ignored.
   - The fix: each format converts between plain and normalized itself, and the linear assumption goes. Step 3's `TypedParam<U>` is built on that conversion, not on a range.
2. **A parameter edit is one shared event:** `ParamEdit { id, value, gesture }`, with the gesture `Begin`, `Change` or `End`.
   - CLAP's gesture events and `DONT_RECORD`, VST3's `beginEdit`/`performEdit`/`endEdit`, VST2's `audioMasterBeginEdit`/`EndEdit`, and AU's parameter listener all become it.
   - The served wire carries it. Today it forwards no edit made in a plugin's own editor for VST3 in process or for AU, and no gesture at all.
   - Step 2's `ParamChanged { gesture }` and step 3's `param.edit()` carry this type.
3. **The wire carries `f64` parameter values.** Set, get and changed are narrowed to `f32` today, while automation points and text queries are already `f64`.

Two candidates for a change to the abstraction were rejected.

- **AUv3 needs nothing shared.** Asynchronous instantiation stays inside `Loaded::load`: the AU implementation keeps the main run loop turning until the unit arrives, which the header allows (it forbids blocking the main thread, not pumping it). The v3 view controller is one more `EditorWindow` implementation. Loading in process or in the system's extension process is an AU load option. Parameter ids through the v2 bridge are 32-bit.
- **Serving VST2 does not get a VST2 case.**
  - Out of process, CLAP and VST3 keep a second, editor-only instance in the host. VST2 cannot have one: it has no state-dirty signal, and its meters read the audio object.
  - An experiment showed its editor *can* live in the server, on the audio instance, parented to a host window on X11.
  - So where an editor lives becomes a generic choice of the served client (§6.5), not a variant of `OutOfProcess::Editor` and not a VST2 marker.
  - 021 Q2 is reversed to that design.

### 6.2 Fixed or found per format

Each row is one PR, mostly inside one host crate.

| PR | Findings | Spec |
|---|---|---|
| VST2 struct (#647, merged) | `VstParameterProperties::categoryLabel` is 24 bytes; it was 8, so a plugin wrote 16 bytes past a host stack buffer | aeffectx.h:71, :480 |
| CLAP correctness | stepped values truncated as the spec says (with 6.1 item 1); `bar_number` is 0 at song position 0 (`BarNumber`'s doc claimed CLAP was 1-based); loop seconds filled; `gui.closed` serviced; the `*_COMPAT` extension ids answered; rescans raised while loaded kept; `deinit` called before unload | events.h:298, params.h, gui.h, entry.h |
| VST3 correctness | the `IMidiMapping` table built at activation, not only on `kMidiCCAssignmentChanged`; MIDI 1.0 channel messages never sent as `DataEvent` type 0 (`kMidiSysEx`); the processor's `outputParameterChanges` delivered to the controller; in-process editor edits reported; `setupProcessing` returning `kResultFalse` refused; `inputParameterChanges` never null; a `numSamples == 0` flush while not processing | ivstmidicontrollers.h, ivstevents.h, ivstaudioprocessor.h, the portal's call sequence and FAQ |
| AU correctness | automation scheduled at its frame (`AudioUnitScheduleParameters` with buffer offsets), not the block's last point at its head; the plugin's own edits heard through a parameter listener; `NonRealTime` parameters not written on the audio thread; a null `mData` in the input callback | AUComponent.h, AudioUnitUtilities.h |
| VST2 correctness | an `effSetChunk` returning 0 is not a refusal (the header gives it no meaning); UI string reads do not take the audio thread's lock; `audioMasterIOChanged` re-reads the pins (S7); the state carries the program index | aeffect.h, aeffectx.h |

The latent violations (no known plugin breaks on them) go into the same PRs as their format's bugs. Where a contract case can pin one, the PR adds the case:
- CLAP's MIDI dialects, overlapping notes and READONLY writes.
- VST3's legacy CCs 130/132–140 and the output bus index.
- The VST2 header order of `effOpen`.

### 6.3 Where the rest went

| Finding | Step |
|---|---|
| VST2 shell plugins (`effShellGetNextPlugin`, a sub-plugin id) make "one plugin per VST2 file" false; `audioMasterCurrentId` answers 0 outside a shell | 6 (the catalog lists every plugin of a file, shells included) |
| AU's 20 contract gaps are one missing AU shared plugin, not AU limits; the two `f32` round trips want a per-format tolerance, not a gap | AU completeness (the fixture is an in-process registered `AUAudioUnit` subclass) |
| AU input as UMP (`MusicDeviceMIDIEventList`, `HostMIDIProtocol` / `AudioUnitMIDIProtocol`, the Group in each packet); the legacy path keeps the `Narrower`; v2 has no cable routing | AU MIDI input parity |
| Project time computed from beats at one tempo (`transport_source.rs`), not from the `TempoMap`; VST2 extrapolates a split block's transport at the block's first tempo | a transport PR beside step 4 |
| A VST3 aux bus without `kDefaultActive` stays inactive though the graph wires it | step 4 (buses as graph handles activate what is wired) |
| VST3 editor: the served editor's run loop not pumped on Linux, in-process resize stubbed, no content-scale update after open | step 2 (`update()` services every editor) |
| AUv3 | after AU completeness (§6.4) |
| `NoteId` has no Group; note expression lacks port, channel and key | plugin MIDI out by Group |
| Format statuses (VST3 `tresult`, AU `OSStatus`) stringified at the shared error | step 4 (`Diagnostics`) |
| `clap-sys` 0.5.0 binds CLAP 1.2.2; only the draft `transport-control` id moved since | 021 step 8 |

### 6.4 AUv3

AUv3 units are hosted through Apple's v2 bridge, which the header documents as bridging "all major pieces of functionality" (AUAudioUnit.h:337). No native `AUAudioUnit` host is planned. Today a v3 unit without a view already loads, and one with a view is refused (`RequiresAsyncInstantiation`).

The step:
- **Asynchronous loading.** `AudioComponentInstantiate`, with the main thread pumping its run loop until the unit arrives. This also gives the served AU a pumped main queue, which it lacks today.
- **The v3 editor.** `kAudioUnitProperty_RequestViewController`, embedding the view and following its size.
- **Loading in process** when `kAudioComponentFlag_CanLoadInProcess` is set. Otherwise the system's extension process renders it over XPC, whose death the health watch must recognise.
- **The v3 flags in the descriptor.**
- **A test fixture:** an in-process registered `AUAudioUnit` subclass. It is the AU completeness fixture's second face, since no v3 unit can be assumed installed (none is on the test Mac).

About 1.5–2.5k lines in two or three PRs.

Risk: a v3 parameter address is 64-bit, while the bridge's and `ParamId` are 32-bit. Step 3 decides whether `ParamId` widens.

### 6.5 Serving VST2: the editor in the server

A served plugin's editor lives in one of two places:

- **In the host:** an editor-only second instance. This is CLAP, VST3 and AU today, through `OutOfProcess::Editor`.
- **In the server:** the editor opens on the served instance itself, parented to a host window. Any format may use this; VST2 can use nothing else.

The choice is the served client's, made at launch. The editor loader `Ipc::launch` takes becomes optional, and a format without one gets the remote editor. It is a runtime value, not a trait marker. In the server, the editor opens through the format's own in-process editor call (`Control::open_editor`), so no format gains code for it.

What changes:

1. **The server's main thread runs an event loop:** socket reads, an idle timer (VST2's `effEditIdle`, about 30 Hz) and the window system's events, in one loop. Today it blocks in a socket read. This is shared work: the served AU needs a pumped main queue (§6.2), and AUv3's asynchronous load needs it too (§6.4). Whichever of the three comes first builds it.
2. **Editor messages on the wire.**
   - Host to server: `OpenEditor { parent, scale }`, `CloseEditor` and `SetEditorSize`. The protocol carried an `OpenEditor` until version 25.
   - Server to host: `EditorWantsSize`, `EditorClosed` and the shared `ParamEdit { id, value, gesture }` (§6.1). For VST2, `ParamEdit` carries `audioMasterAutomate` and `BeginEdit`/`EndEdit`.
3. **A remote editor in `Ipc`.** It is an `EditorWindow` that forwards open, close and resize over the wire, and it is what step 2's editor view holds for such a plugin.
4. **The parent window, per OS:**
   - **X11:** the window id is valid in another process, so the editor embeds. The server opens its own display connection and passes it in `effEditOpen`'s `value`, as LXVST hosts do.
   - **Windows:** a parent across processes (`SetParent`) works, but it joins the two input queues, so a stalled server could freeze the host's UI. The first version is a floating owned window; embedding follows.
   - **macOS:** no public API embeds another process's view. The editor floats, and the server runs as an app. This is step 2's `Opened::Floating`, so the API does not change.
   - **Wayland:** limited exactly as in process, since an Xlib editor needs an X11 window.
5. **VST2's row.**
   - `impl OutOfProcess for Vst2Loaded`: its served load is its in-process load.
   - A `served!` row, which takes several extensions (`.so`, `.dll`, `.vst`).
   - The client no longer refuses VST2 (`BridgeError::NotOutOfProcess`), and the contract's `hostings` and the `check-hosting` gate gain served VST2.
   - The `compile_fail` doctest that pins today's refusal is deleted.
6. **Which hosting a VST2 gets** is step 6's `Hosting`. In process stays the default and served is opted into. On Linux served may become the default later, since it isolates a crash.

What VST2 needs no work for:
- **Editor sync.** With the editor on the audio instance there is no second instance to keep in step, which was the real obstacle.
- **Audio, parameters and state.** These already work served: the server hosted VST2 audio before #625, and the experiment's served tests pass.

About 1.5–2k lines, in three PRs:
1. The event loop, the editor messages and the remote editor, with no VST2 in them.
2. VST2's row, the client and the contract.
3. Windows: floating, then embedded.

The step comes after AU completeness.
