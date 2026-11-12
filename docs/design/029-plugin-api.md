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

**The freeing paths stay on `Session`, in every build.** A retired in-process
unit may tear down an editor in its `Drop`, but moving `update`, `commit`,
`remove` and `replace` behind the view whenever `plugin` is on would make the
feature non-additive (a crate that calls `session.update()` would stop compiling
when something else in the build turned `plugin` on), and `Session` would no
longer be usable headless. Instead nothing frees a plugin on those paths: what a
removal, a replace, a crossfade or a reload retires (the plugin's record and its
control handle) goes to a **graveyard** the session holds, and the view's
`update()` drops it with the proof, beside `run_deferred` (the work a format's
`Drop` handed to the main thread) and the editor servicing. The `debug_assert!`s
are gone. A host that never claims the main thread keeps the retired plugins
until it does; the leak is bounded by what the host removes, is counted
(`Session::retired_plugins`) and is warned about once past sixteen
(`Phase::Plugins`). The main-thread-only calls (an editor, a load, a state
restore) take the proof, or are the view's, and the compile-fail doctests pin
that `Plugin<'_>` has no `editor()` and that `MainSession` is `!Send`. A format's
`Drop` keeps `MainThread::current()` (a `Drop` cannot take a token): CLAP, VST2 and VST3 (with an editor open) move what they own to
`defer_to_main` when released off the main thread, and `run_deferred` (the
view's `update`) drops it there. `MainThread`
is passed by value (it is a `Copy` zero-sized proof; a reference to it adds
nothing). It is never derived from a window system's event loop, which winit
lets run off the OS main thread on Linux and Windows.

### 3.3 Events

`SessionEvent::Plugin(PluginEvent)`, nested like `Midi(MidiEvent)`,
`Device(DeviceEvent)` and `Take(TakeEvent)`. The node is the session's own
`NodeKey`, inside the event: `PluginEvent { node, change: PluginChange }`, both
`#[non_exhaustive]`.

| `PluginChange` | Replaces |
|---|---|
| `GestureBegan { id }`, `ParamChanged { id, value, recorded }`, `GestureEnded { id }` | `on_parameter_changed` (a plugin's own edit, its gesture's ends kept) |
| `ParamsRescanned`, `ParamTitlesChanged` | `on_refresh`, and `on_invalidate`'s parameter cases |
| `LatencyChanged { latency }`, `TailChanged { tail }`, `IoChanged` | `PluginLatencyChanged`, `on_invalidate` |
| `PresetChanged(PresetId)` | polling `current_preset` (step 4) |
| `EditorWantsSize(EditorSize)` | `poll_editor_resize_request` (step 2, editor half) |
| `Died`, `Reloading`, `Recovered`, `GaveUp` | the existing `Plugin*` events, moved under one variant |

A host that needs a push from the thread that hears the plugin (a parameter
mirror on another thread) subscribes at the low layer,
`PluginHandle::subscribe(f) -> Subscription`, which unsubscribes on drop; there
is no `clear_*`, and the session has no second channel for the same events.

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

As built (`tutti-session`'s `jobs` module): the public surface is
`SessionBuilder::spawn_with` and `Job::run`; a job the spawner drops unrun ends
as failed (`JobFailed::Dropped`), and a panic in it is caught
(`JobFailed::Panicked`). Inside the crate, `submit(work, land)` lands one result
and `submit_stream(work, land)` lands each message the work sends through its
`Landings` (unbounded, never blocking) and then its end, exactly once and after
every message: `work` borrows the `Landings`, so nothing it sends outlives it.
Landing is the first thing `Phase::Collect` does, so what a job landed is in the
graph that update commits. There is no cancel: a job that stops carries its own
flag, and `Landings::is_closed` (a send returns the message) tells it the session
is gone. The recording take's pump (doc 030 step 8) and the MIDI file and
SoundFont loads (doc 017 L1f-b) are its first users; the bounce moves onto it
later.

### 3.5 Where it meets the graph and MIDI

Agreed with the graph's surface ([013](013-native-graph.md), "The session
graph's surface") and MIDI's ([017](017-midi.md)), so a host learns one way of
doing each thing:

- **The key.** `add_plugin` returns `Key<PluginNode>` (the marker is
  `PluginNode`: `Plugin` is the view); `replace` and `crossfade`
  keep it. Ports come from it as from any node's (`synth.out(0)`,
  `fx.input(1)`, the event ports), and wiring is the graph's `connect`.
  `synth.param(ParamId)` (feature `modulation`) is the same reference a native node's param method
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
  area, a tuple, `SessionEvent::Plugin(PluginEvent)` beside
  `SessionEvent::Midi(MidiEvent)` and `Device(DeviceEvent)`, with the node
  inside the event; `#[non_exhaustive]` at both levels.
- **Errors.** The session's rule: one variant per area. One
  `tutti::plugin::Error`, its specifics (`BridgeError`, `StateError`,
  `PresetError`, `EditorError`) as sources, is `tutti::Error::Plugin`.
- **The prelude** (feature `plugin`): `Catalog`, `PluginRef`, the load request,
  `PluginState`, `PluginEvent`, `ParamId`, `Normalized`, `MainThread`,
  `ContentScale`, `EditorSize`, the `Plugin` view and the `PluginNode` marker. Not `PluginHandle`,
  `AnyThread`, the bridge's tuning or a format's types.

## 4. Steps

Each is one reviewable PR that leaves `cargo xtask ci` green. Each deletes
what it replaces; no deprecated twin is kept.

| # | Step | Deletes | Test |
|---|---|---|---|
| 1 (built) | **Identity and state.** `PluginRef` from `&PluginEntry`; the load passes the plugin id; `PluginState` and `StateError::OtherPlugin`; one `state()` view with `last_good()`, `withheld()`, `save()` and `restore()`; the acceptance example `crates/tutti/examples/plugin_host.rs` (scan, load, wire, MIDI, parameters, editor, save, bounce), which each later step rewrites to its shape | `PluginId` as a path; `snapshot()`; `Load::new()` in the open | the second plugin of the CLAP test bundle loads by its entry; a state restored into another plugin is refused |
| 2 (view and events built; main-thread view and editor servicing open) | **The view, the main-thread view and events.** `Plugin<'_>`, `MainSession<'_>`, `MainThread` by value, `PluginEvent`; `update()` services editors; the editor's `Opened`, `resize`, `rescale` | `on_*` and `clear_*`, `editor_idle` calls, `poll_editor_resize_request`, `open_floating_editor`, the freeing paths' `debug_assert!` | a plugin's own edit arrives as `ParamChanged`; an editor that requests a size raises `EditorWantsSize`; a freeing path is unreachable without the view (compile-fail doctest) |
| 3 (built) | **Parameters.** `Params`, `Param`, `TypedParam<U>`, `ParamEdit`; `ParameterInfo`'s fields private; automation mode moved | `any_thread()` in user code, `parameter_*` free calls in the prelude | a gesture guard dropped mid-drag ends the gesture; a `typed::<Db>` on a Hz parameter is refused; a set through a stale handle after a rescan is refused |
| 4 (4a and 4b built, but for the items 4b lists as not done) | **Presets, health, diagnostics, modes.** `Presets`, `PresetError`, `Health`, `Diagnostics`; the per-plugin render mode goes; buses as graph handles | `presets() -> Option`, `load_preset -> Option<bool>`, the six liveness readers, `set_render_mode` on the handle | per format: a preset list loads by id, an unsupported format answers `Unsupported`; a killed server reads `Died` then `Reloading` |
| 5 | **The spawner.** `spawn_with`, `Job`; jobs landed in `update()` | `land_snapshot`, `land_params`, `land_reload` as public calls; the `*Due` events | a reload completes with no host code but `update()` |
| 6 | **The catalog and the facade.** `Catalog::open_db`, `scan()`; `Plugins::open` removed; the probe lists every plugin of a bundle (`FileState::Probed` already holds several); 021 step 6's `Hosting` as a `PluginRef` field (`Auto`, `InProcess`, `OutOfProcess`) | `with_fresh_scan`, `Plugins::open`, `Backend` | a blacklisted plugin is refused at load, not at open |

**As built (step 2, the view and events).** `session.plugin(node)` lends a
`Plugin<'_>` (`Result<_, NoSuchNode>`; `plugins()` yields them): `node`, `name`,
`descriptor`, `plugin`, `state`, `liveness`, and, until steps 3 and 4 replace
them, `handle`, `controls`, `meter` and `param_values`. The record behind it,
`HostedPlugin`, is crate-private. The handle's three one-slot callbacks
(`on_parameter_changed`, `on_refresh`, `on_invalidate`, and their `clear_*`) are
gone: a backend feeds three `Listener`s, now fan-outs, and
`PluginHandle::subscribe(f) -> Subscription` hears all three as a `Notice`
(every subscriber hears every notice, so the parameter mirror and the session no
longer fight over one slot). The session subscribes once per plugin
(re-subscribing when a crossfade or reload replaces the handle), queues what is
heard, and `Phase::Plugins` turns it into `PluginEvent`s in the order the plugin
made them. A latency the plugin reports is not an event of its own: the update
that hands it to the graph reports `LatencyChanged` once; a rate refusal, a
crash and an in-place reload stay the plugin's health (step 4's `Health`).
A gesture is three variants, not a field:
`GestureBegan`, `ParamChanged { recorded }`, `GestureEnded`, since the two ends
carry no value (`ParamEdit`). `PluginSnapshotDue`, `PluginParamsDue` and
`PluginReloadDue` stay `SessionEvent` variants until step 5. Still open in
step 2: `MainSession` and `Session::main`, `update()` servicing editors, the
editor's `Opened`, `resize` and `rescale`, `EditorWantsSize`, and removing the
freeing paths' `debug_assert!`: built next, below.

**As built (step 2, the main-thread view).** `Session::main(MainThread) ->
MainSession<'_>` is `&mut Session` and the proof (`MainThread` was already
`Copy`: it is `!Send` and `!Sync` and zero-sized, so a copy cannot leave its
thread). It does not deref to the session: its own verbs are `update` and
`plugin`, and the rest goes through `session()` / `session_mut()`, so a call that
reaches `Session::update` and skips the main thread's share is visible where it
is written. `MainSession::update()` is `Session::update`,
then the graveyard, `run_deferred`, and every editor opened through the view:
`editor_idle`, then a poll of the plugin's resize request, raised as
`PluginChange::EditorWantsSize(EditorSize)`. `ui.plugin(node)?` lends a
`MainPlugin` (`view()` for the read-only `Plugin<'_>`, `editor()`), whose
`EditorView` has `open(&window, scale) -> Result<Opened, EditorError>` (`Opened::Embedded(size)`,
or `Floating` for a plugin whose `Features::EDITOR_FLOATING` is set), `resize`,
`rescale` and `close`. `rescale` is `EditorWindow::set_editor_scale`, a live call (CLAP's
`gui.set_scale`, withheld on a logical-pixel api; VST3's
`IPlugViewContentScaleSupport::setContentScaleFactor`); VST2 and AU, which take
the scale at open or have the OS scale the window, answer the typed
`EditorError::ScaleUnsupported`. A served plugin's editor is the editor-only
instance in the host process, so the call reaches it through `PluginBridge`
with no wire change. A floating editor is the plugin's window. `PluginHandle::open_editor` takes a
`WindowHandle` now (`WindowHandle::from_window` still takes any
`raw-window-handle` 0.6 window). The VST3 pieces of §6.3: the served editor's
`editor_idle` pumps the Linux `IRunLoop` (it did nothing), and an in-process
VST3's `Control` reports its resize capabilities, takes `set_editor_size` and
answers `poll_editor_resize_request` (they were stubs). What the audit of the
drop paths found: CLAP and VST2 hand their release to `defer_to_main` when
dropped off the main thread, and a VST3 plugin with an editor open, which used
to be leaked whole (every COM object and the module), now does too: `Plugin`
wraps its `Parts`, and its `Drop` hands them to the main thread. So whatever
`Editor::collect` retires closes on the main thread, for every format.

**As built (step 3, parameters).** `session.plugin(node)?.params()` is a
`Params<'_>` (`tutti-session`'s `plugin_params` module), and the session holds
what it shows: the descriptors and the value of each live in the record's
`PluginParamValues` (`tutti-plugin-host`, now holding the `ParameterInfo`s as
well, every parameter with a value: one the plugin did not answer reads as its
declared default). The mirror is every hosted plugin's, no longer a
`modulation` feature: `Session::add_plugin` reads it in the load job; a plugin
the host inserted itself (`host_plugin`, `insert_plugin`) or a crossfade
rebuilds it by the job the session hands out as `SessionEvent::PluginParamsDue`
(run it, `Session::land_params`; step 5 replaces the event with the spawner);
and a plugin's rescan, `PluginChange::ParamsRescanned` or `ParamTitlesChanged`,
asks for the same read again, landing descriptors and values together. A read
the plugin cannot answer (an in-process plugin whose main thread did not
answer) leaves `Params::is_ready` false and `get` answers
`PluginParamError::NotRead`.

- **`Params`**: `get(id)? -> Param` (`PluginParamError::{NotRead, Unknown}`),
  `typed::<U>(id)? -> TypedParam<U>`, `iter()`, `len()`, `is_ready()`,
  `set_automation_mode(mode)`. `Param`: `info()`, `id()`, `normalized()`
  (infallible: the mirror, which answers a value the host just set or the plugin
  just reported at once, before the next update drains it), `set`, `text`,
  `set_text`, `plain`, `set_plain` (the format's own map, §6.1), `edit()`, and,
  with `modulation`, `target() -> ParamRef`.
- **Stale handles are a borrow error, not a check.** `Plugin<'_>`, `Params`,
  `Param`, `TypedParam` and `ParamGesture` all borrow the session, and the
  only code that replaces what they show (a rescan landing, a reload, a
  crossfade) takes `&mut Session`. So no handle can be held across a rescan,
  and there is no generation counter to compare: what a host keeps is a
  `ParamId`, which `get` refuses once the plugin stops declaring it (a
  compile-fail doctest pins the borrow; a runtime test pins the replacement).
  An index into the mirror (`Param`'s position) is valid for exactly the read
  that made it, which the same borrow guarantees.
- **Reads and writes are on the read view `Plugin<'_>`, not `MainPlugin`.**
  Every call goes through the handle's token-free calls (`AnyThread`: a set goes
  to the plugin's edit ring from any thread), so nothing here needs the main
  thread, and `MainPlugin::view()` lends the same view. `text`, `set_text`,
  `plain` and `set_plain` ask the plugin and may be a round trip (an in-process
  plugin answers from its main thread), so a host does not make them per frame.
- **A unit is checked once.** `typed::<U>` needs the parameter's declared
  `ParamUnit` (`NoUnit` for none), `U::reads(unit)` (`UnitMismatch`: `Db` on a
  hertz parameter) and a declared range `U` holds at both ends and the default
  (`RangeUnfit`: a bipolar percentage is no `Mix`); `get() -> U` is then total
  and `set(U)` refuses a value outside the declared range (`OutOfRange`; no
  silent clamp) and goes through the format's `parameter_normalized`.
  `FromPlain` became `PlainUnit` (`reads`, `from_plain`, `to_plain`) for the way
  back.
- **A gesture is the host's own, and the one owned handle.** `param.edit()? ->
  ParamGesture` begins when it is made (refused up front for a read-only
  parameter), each `set` moves the parameter, and `Drop` ends it, a drop in the
  middle of a drag included. It has no lifetime: a drag is a mouse down, many
  frames each calling `update()` (which takes `&mut Session`), and a mouse up, so
  the guard must span them, while every other handle stays borrowed so that a
  stale one does not compile. A shared `Gate` stands in for the borrow: the
  record ends it when it is retired (`Graveyard::bury`) or its plugin is replaced
  (`adopt`: a crossfade or a reload), and refreshes the declared ids whenever a
  read lands. `ParamGesture::set` returns `PluginParamError::Gone` when the gate
  is ended or the parameter is no longer declared, sending nothing, and the drop
  then sends nothing. The guard holds the plugin's handle, the plugin's own
  notice queue (no second channel) and a `ParamReporter` into the mirror. No hosted
  format has a call that tells a plugin the host holds a parameter (CLAP's and
  VST3's gesture events go from the plugin to the host, VST2 has none, AU's are
  notifications to listeners), so the plugin hears the values and the session
  reports the begin, the changes and the end as `PluginChange::{GestureBegan,
  ParamChanged { recorded: true }, GestureEnded}`, in the order made, with the
  plugin's own. A plain `param.set` is not a gesture and reports no event.
- **`plain` and `set_plain` take a bare `f64`** because a plugin's plain value
  has no tutti unit unless it declares one; `TypedParam<U>` is the typed door.
- **`ParameterInfo`'s fields are private.** Read through `id`, `name`, `unit`,
  `range`, `steps`, `group`, `measures`, `role` and `flag(f)`; the format
  hosts build it with `new` and the `with_*` builders (`with_unit` and
  `with_range` are new), and the `known` mask has no reader (`flag` is `None`
  for a bit never reported). `apply_function_roles` and the tests that set a
  field built a new value instead.
- **`set_automation_mode`** moved to `Params`, with its own error
  `AutomationModeError` (`Unsupported { format }`: no in-process format has the
  channel; `Dropped`: the command queue was full; `PluginDead`) in
  `tutti-plugin-types`, which `AnyThread` and `PluginHandle` return too.
- **`ParamRef`.** `Param::target()` is `ParamRef::plugin(node, id)`, the type a
  native node's `ParamRef::unit` gives, so a route targets either through one
  type. §3.5's `synth.param(ParamId)` on the plugin's key arrived with the key
  (below).
- **The umbrella.** `Params`, `TypedParam`, `ParamGesture` and
  `PluginParamError` are in the prelude (`Param` is not: `tutti_types::Param` is
  the native cell and holds the name); the prelude never held `PluginHandle`,
  `AnyThread` or a `parameter_*` call, and `crates/tutti/examples/plugin_host.rs`
  now lists, sets, reads the text of and drags a parameter through `params()`.
  `Plugin::handle()` stays until step 4 (it still carries the editor and
  presets), and `Plugin::param_values()` is gone.
- **Tests** (`tests/plugin/plugin_params.rs`, and `vst2_params.rs` over the
  reference VST2 plugin in process): a guard dropped mid-drag ends the gesture;
  `typed::<Db>` on a hertz parameter is refused; an unknown id is refused; a
  rescan replaces the declarations and values; a typed value round-trips
  through VST2's own integer-range map.

**As built (the plugin's key).** `Session::add_plugin`, `add_plugin_blocking` and
`Graph::insert_plugin` return `Key<PluginNode>`, `PluginNode` being an
uninhabited marker (`Plugin` is the view, and `tutti_plugin_host::PluginNode` is
the node, so the marker names the kind of key only); every format, in or out of
process, is one. `Graph::host_plugin` (a node the host inserted itself) returns
`Result<Key<PluginNode>, NoSuchNode>` where it returned `bool`. `erase()`,
`Into<NodeKey>` and the ports (`out`, `input`, the event ports, the buses) are
`Key<N>`'s for any `N`, so every call taking `impl Into<NodeKey>` accepts it;
`session.plugin(key)` is unchanged and the view still borrows the session. With
`modulation`, `Key<PluginNode>::param(ParamId) -> ParamRef` is
`Param::target()`'s value (which now calls it): a plain value, no session
borrow, and an id the plugin does not declare is refused where the reference is
resolved (a route's resolution, `Problem`; `Params::get` for the view), as
`ParamRef::plugin` always was. Tests: `plugin_params.rs`
(`a_plugin_key_names_a_parameter_a_route_targets`,
`a_hosted_plugin_key_wires_like_a_native_key`) and the probe's modulation
tests, which name the parameter through the key.

**As built (the typed cause, step 4's, done early).** The cause of a death is
`tutti_plugin_types::PluginFailure`, latched by the bridge where it notices
(`mark_crashed(PluginFailure)`) and the one value behind `Health::crash_cause`,
`PluginStatus::Dead`, `BridgeEvent::Crashed`, `PluginInvalidation::Crashed`,
`RecoveryEvent::Died`, `PluginRenderFault::Crashed` and the session's
`PluginChange::Died` and `PluginLiveness::Dead`. Its
variants are what the call sites produce: `Connect`, `Handshake` (a version skew
is `BridgeError::ProtocolMismatch`, a wrong first message `UnexpectedMessage`),
`StreamClosed`, `ServerExited`, `TimedOut`, `Io`, `Protocol` and `Bridge` (a
stream error, classified once in `PluginFailure::from_stream`),
`Terminated { reason: TerminationReason }` and `Unreported`: only what a backend
or the host latches. The session's reload reasoning is its own type,
`tutti_session::RecoveryFailure` (`ReloadFailed`, the load's `BridgeError`;
`ReplacementDied { cause: PluginFailure }`; `ReplacementPorts { got, was }` over
the session's `PortCounts`; `ReplacementRefused`, whose `source` is the graph's
`CrossfadeRefused`; `ReplacementInProcess`; `ReloadDropped`; `ReloadTimedOut`),
and the events say which cause they hold: `PluginChange::Died` carries a
`PluginFailure`, while `PluginChange::Reloading`, `PluginLiveness::Recovering` and
`GiveUp::last` carry a `ReloadCause` (`Died(PluginFailure)` or
`Attempt(RecoveryFailure)`, a reload follows either), and
`RecoveryEvent::LoadFailed` a `RecoveryFailure`. `tutti-plugin-types` names no
graph type. A `BridgeError` or other error behind a failure is its `source` (held
in an `Arc`, as `BridgeError` is not `Clone`), downcast to the type the caller
knows. `terminate` takes a `TerminationReason`, a closed set (its
only reason today is `Hung`, the session's), not free text. None of these
implements `PartialEq`, since an `io::Error` is not comparable. The server sends no cause, so the wire and `PROTOCOL_VERSION` are
unchanged.

**As built (step 4a, presets and health).** *Presets* are the main thread's
(VST3, VST2 and AU answer them there), so the view is
`MainSession::plugin(node)?.presets()` on `MainPlugin`, not on the read-only
`Plugin<'_>`: `Result<Presets<'_>, PresetError>` (`tutti-session`'s
`plugin_presets` module). `PresetError` (`tutti-plugin-types`) is the three
answers `Option<bool>` kept in one value: `Unsupported` (no preset surface, or
the operation is not the format's), `Refused` (the plugin declined) and `Lost`
(the plugin is gone). `Presets::kind()` is `PresetKind::Browse` (VST2, VST3, AU:
`iter()` of `PresetEntry` with `id`, `name`, `bank` and `load()`; `get(&id)`) or
`PresetKind::File` (CLAP: `load_file(path)`, an empty list); a plugin with no
preset surface (a VST2 declaring no programs) is `Err(Unsupported)` at
`presets()`, and `load_file` on a `Browse` plugin is `Unsupported`.
`current()` is the plugin's own query (`None` where the format has none). The
view reads the list once when it is made (a served plugin's is a round trip).
`PluginHandle::{presets, load_preset}` return the same `Result`s, and
`preset_support` stays on the handle, the low layer the view reads the kind
from. `PresetChanged` (§3.3) is not raised: no format tells the host that the
plugin changed its own preset, and a load the host makes is its own call.
*Health* is `Plugin::health() -> PluginHealth`, named so the prelude does not
hold a second `Health` beside `tutti_plugin_types::Health`, the backend's trait.
`Running`; `Stalled` (alive, blocks abandoned with no reply and no hang
declared yet: `Recovery::is_stalled`, so never read with recovery off);
`Died { cause }` (the bridge has latched a death and the session has not yet
written it off, which it does after its debounce; the reading a host sees for
those updates); `Reloading { attempt, cause }` (the `ReloadCause` and the
attempt number the event carried: the node stays, silent). `GaveUp` is not a
reading: the verdicts that remove the node (recovery off, or its budget spent)
remove the record in the update that reaches them, so no view can hold it and
the events (`PluginChange::{Died, GaveUp}`) are where they are seen. The
session's debounce enum, `PluginLiveness`, is crate-private. The reference CLAP
plugin gained `clap.preset-load`, answered only with `TUTTI_CLAP_PROBE_PRESET_LOAD=1` (a file ending `.tutti-preset` sets Cutoff to
350, any other location is declined) so the CLAP half runs headless; the
test observes it through the plugin's saved state. Tests:
`plugin_presets.rs` (CLAP `load_file` accepted and refused; VST2 list loads by
id, no programs is `Unsupported`), `plugin.rs`'s
`a_killed_server_reads_died_then_reloading` and `plugin_health.rs`.

**As built (step 4b, diagnostics, modes, buses).** `Plugin::diagnostics() ->
Diagnostics<'_>` (`tutti-session`'s `plugin_diagnostics` module) is the corner of
a plugin's panel, and reads the same in either hosting: `server_pid()` (`None`
in process), `block_counts()`, `missed_chunks()` (0 in process, which has no
pipeline to be late), `latency()` and `tail()` (the figures the graph holds for
the node at the last update: out of process the latency is the plugin's report
plus one chunk of the pipeline), `follows_project_meter()` (false only for a
plugin the host inserted itself) and `terminate(TerminationReason)`, which is
the old handle call and ends the server (the plugin then reads
`PluginHealth::Died { cause: Terminated { .. } }`). The counts are
`tutti_graph::Losses`, in events, as the MIDI inlets' are: `midi_in()` is what
never reached the plugin (all `lost`; `refused` stays 0 because the plugin node
has one event input, so no event is addressed to a Group with no port yet: that
count arrives with plugin MIDI by Group), `parameter_points()` is the host's
points the node dropped (`lost` a full queue's grid points, `evicted` the
points that replaced the last of a full queue, `refused` points for a parameter
it has no slot for) and `parameter_edits()` the plugin's edits the host did not
hear (`lost`). Chunks deferred a chunk are chunks, not events, and have no
field. The old `controls()` (`None` in process) and `meter()` (`None` for a
hand-inserted plugin) are gone from the view; their one accessor is
`diagnostics()`. *Modes:* `PluginHandle::set_render_mode` is deleted; the
session's `Phase::RenderMode` calls the backend's `AnyThread::set_render_mode`,
and a host asks the session (`Session::render_mode`). *Buses:*
`Plugin::buses() -> PluginBuses<'_>` hands out the graph's handles from the
plugin's own bus order (which is the node's: `main_input()`, `sidechain()`,
`input(i)`, `output(i)`, `input_count()`, `output_count()`), as `BusRef` and
`BusSource`, the ones `key.bus_in(i)` and `key.bus_out(i)` name:
`connect(kick.bus_out(0), comp.buses().sidechain().unwrap())`. No format marks
a bus a sidechain (they say "aux"), so `sidechain()` is the first input bus past
the main one; a plugin with none answers `None`. The graph tracks `MAX_BUSES`
per side, and a handle past it is `None`. *`Plugin::handle()`* is not on the
read view of a host any more: it, `controls()` and `meter()` are behind
`tutti-session`'s `testing` feature (what only the session's own tests drive: a
plugin's latency cell, the project-meter cell's reclaim, an editor's idle
tick), since the views now cover what a host used it for (parameters, state,
presets, health, diagnostics, the editor); `PluginHandle` stays the layer the
views are built on. The reference CLAP plugin gained
`TUTTI_CLAP_PROBE_PORT_LAYOUT` (every server's port layout, as the existing
`..._RESTART_PORT_LAYOUT` is a replacement's), which the sidechain test uses
(stereo main and a mono aux input). Tests: `plugin_diagnostics.rs` (lost MIDI
as `Losses` in process, the server named and ended, the latency and tail as the
graph holds them, a sidechain wired through the handle and a bus past the
plugin's own refused). Not done in this step: a VST3 aux bus's activation
follows what the graph wires (the node does not learn which ports are wired, so
it stays a `kDefaultActive` wish), VST3 `tresult` and AU `OSStatus` typed into
the diagnostics, and §6.3's transport items.

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
`Origin` (file and `Load`) it serves. (The probe described only the first plugin
of a file until step 6a; it lists every one now, below.) `PluginClient::new` and `open` take a
`&PluginRef` (`PluginRef::file` is the explicit "the file's first"); no
path-only open is left.

**As built (step 6a).** The probe lists every plugin of a file, in every format,
and the catalog holds them all; `Catalog::open_db`, `scan()`, the removal of
`Plugins::open`, `with_fresh_scan` and `Backend`, and `Hosting` are 6b (below).
- **The wire.** `ProbeOutcome::Described` carries a `Vec<PluginDescriptor>` (never
  empty: a file with no plugin is a `Failed`), and the result file's magic is
  version 3 (a version 2 file, one descriptor, is refused by its magic).
  `probe_metadata_with` returns the list, `probe_record_with` puts one `FileRow`
  of it, and the scanner's probe (`tutti-plugin-db`'s `Scanner::new`) returns a
  `Vec`; the db file is version 3, because a row written by a version 2 probe
  lists a file's first plugin only and, up to date by its time, would never be
  read again.
- **Per format.** CLAP: each descriptor of the factory (`ClapLoaded::probe_all`).
  VST3: each audio class of the factory, one entry per class id
  (`Vst3Loaded::probe_all`; a class the factory lists and cannot create is left
  out). AU: each unit the bundle declares in `AudioComponents` that the registry
  has, described from the component and none created
  (`AuLoaded::describe_all`; the probe gains an `au` feature, macOS only, and
  routes `.component`; before this the probe read no AU at all). VST2: a file
  that is no shell is one plugin; a shell (`effGetPlugCategory` is
  `kPlugCategShell`, read with the shell loaded as id 0) is each sub-plugin
  `effShellGetNextPlugin` names (`Vst2Loaded::describe_all`), each described by
  loading it, falling back to the shell's own id, name and vendor with an
  unasked category when it will not load.
- **VST2's `Load`.** A choice is `vst2.<uniqueID>`, which is the descriptor id,
  so `PluginRef::load()` carries the id in every format. The host answers
  `audioMasterCurrentId` with the id chosen while `VSTPluginMain` runs (`0`
  when none: `HostState`'s `current_id`, which replaces the constant `'DAWI'`),
  answers `supportShell` and `shellCategory` yes, and checks that the plugin that
  came up has the id asked for (a shell that does not know an id creates its
  default, which is refused). The `Origin` keeps the choice, so a reopen loads
  the same sub-plugin. VST2 is not served, so a served pick is CLAP's and
  VST3's; a served VST2 load is still `NotOutOfProcess`.
- **Tests.** `tutti-vst2-shell-plugin` (testing; two sub-plugins, nothing
  configures it: its answer is the host's `audioMasterCurrentId`) is the
  shell. The probe suite (`tutti-plugin-contract`'s `probe`) lists the CLAP test
  plugin's two descriptors, the shared plugin's three VST3 classes and the
  shell's two sub-plugins through the binary, loads each by the `PluginRef` of
  its entry (a server for CLAP and VST3, in process for VST2); the probe crate's
  `vst2_shell` loads and refuses by id. Not covered: an AU bundle of several
  units (the AU probe is checked on macOS against the installed bundles); a
  VST3 factory with a class that cannot be created.

**As built (step 6b).**
- **The catalog.** `Plugins` is `Catalog` (`tutti_plugin::catalog::Catalog`,
  in the prelude with `Hosting`). `Catalog::open_db(config)` reads the JSON
  file (feature `json`; `with_db` and `empty` stay for a host's own store),
  `scan() -> Result<ScanResult, ScanBusy>` is the blocking scan (was
  `rescan`), `spawn_scan` and `poll_scan` the background one. `PluginDb` is
  still the one owner of rows: the catalog changes one only through
  `PluginDb::apply` and its `blacklist` and `unblacklist`.
  `with_fresh_scan`, `Plugins::open` and `Plugins::load` are gone, no twin.
- **The catalog opens nothing.** Its one refusal is `Catalog::check(&PluginRef)
  -> Result<(), BridgeError>` (`Blacklisted` with the recorded reason; the check
  is mtime-aware, so a changed file is re-admitted). The load asks it:
  `PluginLoad::with_catalog(&catalog)` takes the catalog's `AudioConfig` and
  its verdict (asked now, the catalog not borrowed past the call, so the load
  stays `Send`), and `run()` answers a refusal as the load's failure, before
  any server or library. A load that names no catalog consults no blacklist,
  which is how a host offers to load a plugin anyway; `Plugin::open` stays the
  file API and consults none.
- **`Hosting`** (`Auto`, `InProcess`, `OutOfProcess`; not 021's draft names
  `Local` and `Isolated`) is a field of `PluginRef` (`with_hosting`,
  `hosting`; serde with `default`), not part of the plugin's identity
  (`is_same_plugin` ignores it: a state saved from one restores into the other).
  `Auto` is the format's default (021 Q3): **CLAP, VST3 and AU out of process,
  VST2 in process**. `InProcess` hosts CLAP, VST3 and AU in the host's process
  too, through the generic `InProcClient<F>` (it needs the main thread and the
  format's `tutti-plugin` feature; AU on macOS). `Plugin::hosting_of(&PluginRef)`
  resolves it, `Plugin::hosting()` reports it, and the private `Backend` enum is
  `Placed`, one arm per in-process format beside the served client.
  `PluginLoad::opens_in_process` follows the resolved hosting.
- **Refusals are the load's, typed and early.** VST2 asked for `OutOfProcess` is
  `BridgeError::NotOutOfProcess`, not moved in process; a format this build has
  no in-process host for (VST2 included, with `vst2` off, even for `Auto`) is the
  new `BridgeError::NotInProcess`. Both come before a server is looked for.
  `Auto` never moves a blacklisted plugin in process (021 Q11): it stays served,
  and the catalog's check refuses.
- **Tests.** The step's test is `tutti-session`'s `plugin_hosting`: a blacklisted
  plugin is refused at load with its reason (no server could be found, so the
  refusal is not a failed launch), admitted by a load with no catalog and once
  unblacklisted. `tutti-plugin`'s `hosting` suite loads the reference CLAP
  plugin as `Auto`, `OutOfProcess` (a server pid) and `InProcess` (none; the main
  thread demanded), VST2 as `Auto` in process and `OutOfProcess` refused, and
  `scan()` over a directory that gains a file between two scans counts only the
  new one. Not covered: VST3 and AU in process through `Plugin` (the contract's
  `hostings` run both formats in process; AU is checked on macOS).

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

1. **Value conversion belongs to the format.** Built.
   - The problem was that `ParamRange::Plain` treated every plain range as linear. `Normalized` was the plugin's own value for VST3 and VST2 and a position tutti invented for CLAP and AU. A CLAP stepped parameter landed one step low (fixed by #652), a VST3 parameter with a non-linear `normalizedParamToPlain` got a wrong plain value and default, and a decreasing map ran backwards.
   - What is built: `Control::parameter_plain` and `parameter_normalized` (and `AnyThread`'s, for the handle), plain as `f64`, `None` for a parameter the format has no plain value for. A consumer asks the format and never computes from a range. `ParamRange::to_plain`, `to_normalized` and `ParameterInfo`'s twins are deleted; `ParamRange` and `bounds()` stay as data for display and for `ParameterInfo::typed`.
   - **One trait ties the four conversions together: `ParamMapping`** (`tutti-plugin-types`). It is one parameter's map between its normalized position and its plain value, as its format defines it: `plain(&self, Normalized) -> f64` and `normalized(&self, f64) -> Normalized`. Each format implements it on its own type, at its own edge, and no shared code computes anything: `Control::parameter_plain` and `parameter_normalized` build the parameter's mapping and ask it (the `Control` and `AnyThread` signatures did not change: both are used as `dyn`, and a boxed mapping accessor was rejected). A format that cannot build a mapping (an id the plugin does not declare, no VST3 controller, a VST2 parameter with no integer range) answers `None` from `Control`, never from the mapping.
   - The trait is total, with no `Option`: CLAP, AU and VST2 cannot fail, and VST3's controller calls return a bare `ParamValue` with no `tresult`, so `ControllerMapping` holds a controller's answer to the laws instead (a non-finite plain value reads as `0.0`; a non-finite plain input is position `0.0` without asking the controller). One signature serves four formats, and the audio-thread callers (CLAP's and AU's automation) take an `f64` or an `f32` with no dead branch.
   - **The laws**, from the specs, in the trait's docs: every position on `0..=1` has a finite plain value; any plain value, finite or not, maps onto `0..=1` (a value past an end reads as that end, NaN is `0.0`); `plain(normalized(p)) == p` for every `p` the map produces, bit for bit for a stepped map and within rounding of the map's own width for a continuous one (AU's is `f32`); the map is monotonic, increasing or decreasing or constant, and `normalized` goes the same way.
   - **`mapping_contract!`** (`tutti_plugin_types::contract`, behind the off-by-default `contract` feature, on the pattern of `tutti-dsp`'s `design_contract!`: a dev-dependency turns it on, and CI lints and documents it with the feature) makes one `#[test]` per law over a sweep of positions (both ends, a hair inside each, sixteenths, points no grid names), the plain values those produce and plain values off the ends (past by one and by `1e300`, the infinities, NaN); a stepped map names its `steps`, each reached exactly and the only values produced. Each case is a plain function over a mapping (`contract::cases`), and `contract::check` runs them all on one mapping built once. The cases are held to account by the crate's own tests, which plant a defect in a toy map and assert the case fails.
   - **Where each impl is, and what runs it:** CLAP's `ParamBounds` (`tutti-clap-host/src/events.rs`; linear and stepped), AU's `ParamBounds` (`tutti-au-host/src/ranges.rs`; macOS), VST2's `IntegerRange` (`tutti-vst2-host/src/param_properties.rs`; increasing, inverted and one point) and VST3's `ControllerMapping` (`tutti-vst3-host`, `Vst3Loaded::mapping(main, id)`, which borrows the plugin and the main-thread token for one id; run through the shared test plugin's real controller on `level` and the decreasing exponential `tone`, `tests/host/param_mapping.rs`). The conversion code beside them is deleted: `ParamBounds::{plain, normalized}` and `to_plain`, `to_normalized`, VST2's `plain_of` and `normalized_of`, and the copies of the CLAP denormalize in the editor instance.
   - Per format:
     - **VST3** asks the controller's `normalizedParamToPlain` and `plainParamToNormalized`, whatever their shape. The listed default is `normalizedParamToPlain(defaultNormalizedValue)`, exact; `min` is the smaller of the plain values at 0 and 1, so a decreasing map has its `min` at normalized 1.
     - **CLAP** converts in its own `ParamBounds` (`tutti-clap-host`), the one place the line over the declared `f64` range is written, with the stepped rounding of #652. CLAP has no normalized value, so the line is the host's and is the format's answer.
     - **AU** converts in its own `ParamBounds` (`tutti-au-host`), linear over `[min, max]`. The display-curve flags are **display scales**: the header gives them no formula, so they stay `DisplayCurve` hints and do not bend the position. The platform's own curve, `AUParameterValueFromLinear` (keyed on the unit), is not called; if a consumer needs AU's slider position, that call is the next step, and the mapping changes in one place.
     - **VST2** has a plain value only from an integer range (`minInteger` to `maxInteger`), the line between the ends (the header fixes no map; an inverted range runs backwards); without one, `None`.
   - **Served:** the conversion is a control-thread query answered by the server's format, so it is two requests, `GetParameterPlain` and `GetParameterNormalized`, with replies carrying `Option<f64>` and `Option<Normalized>`. `PROTOCOL_VERSION` is 30. Closed-form data was not shipped: it would serve CLAP and AU but not VST3, whose map is a function in the plugin.
   - The shared test plugin has a fourth parameter, `tone`, a decreasing exponential (`20000 * 1000^(-n)`); the contract's `parameter_conversion_is_the_formats_own` holds each format to its figures in `spec::Tone`, loaded, active and through the server.
   - Step 3's `TypedParam<U>` sits on `parameter_plain`, not on a range.
2. **A parameter edit is one shared event.** Built.
   - `ParamEdit` is `Begin(ParamId)`, `Change { id, value, record }` or `End(ParamId)` (`tutti_plugin_types`). `Control::take_reported_edits` appends them, oldest first, and `Editor::poll_edits` returns them.
   - **The value is in the `Change` alone.** No format carries a value for a gesture's begin or end, so the type has none for them: a `Begin { value }` would force every format to invent one (from a neighbouring change, or by reading the parameter at some later time), and dropping a marker it could not fill would leave a recording host with an unclosed gesture. As an enum the wrong thing cannot be written: every begin and end the plugin sent is reported, in order, and a host that wants the value a gesture started from reads it from the next change or from the parameter. (`ParamEdit::id()`, `value()` and `record()` are the accessors; a struct with a `kind` field would have kept a value slot for the two ends.)
   - `record`, on a change, is the one field beyond the plan. CLAP's `CLAP_EVENT_DONT_RECORD` (`events.h:32-38`: the event "should not be recorded", for a value another recorded source moved, such as a MIDI CC) cannot be dropped, since the value is the parameter's and a host that shows it follows it, and cannot be reported as an ordinary change, since a host that records automation would record both sources. So the value is reported with `record = false`. No other format has the flag, and `IS_LIVE` is not carried because nothing would read it.
   - Per format:
     - **VST2** queues `audioMasterBeginEdit`, `audioMasterAutomate` and `audioMasterEndEdit` in one bounded queue (`ParameterChange::{Begin, Automate, End}`), so the order is the plugin's. A report from inside the host's own `setParameter` is an echo and none of the three is queued.
     - **VST3** reads `beginEdit`, `performEdit` and `endEdit` from the host's component handler. The two ends are also kept for `poll_plugin_notifications`, which a host that reads notifications still gets. The activated Controller adds what the processor wrote to `outputParameterChanges`, as changes with no gesture (the processor is no hand on a knob); the changes of an editor are still queued to the Processor, the processor's are not.
     - **CLAP** reports from `process`: `ClapProcessor` walks the block's output events and queues each `PARAM_VALUE` (normalized against the declared bounds, as the editor instance does) and gesture event, with `DONT_RECORD`, on an `Edit::Reported` ring to the Controller. `ClapEvent::heard` is the one projection, shared with the editor instance's `params.flush`.
     - **AU** subscribes the gestures of each watched parameter beside its value (`AuParameterListener::watch_gestures`), in the listener's serial queue, so the order holds. AudioToolbox coalesces a parameter's value changes that arrive within the listener's notification interval into the last, so two writes in a row are heard as one; the gesture events are not merged with them.
   - **Served:** the server forwards `take_reported_edits` after each pass as `BridgeMessage::ParameterChanged { edit }` (it dropped VST3's edits unread before), the bridge's inbox keeps the order, and the node's listener fires the handle's `on_parameter_changed`, which takes a `ParamEdit`. The editor instance the host holds (CLAP, VST3) reports through `Editor::poll_edits`; the bridge sends each change on to the audio instance and tells the listener of every edit. `PROTOCOL_VERSION` is 31.
   - The shared test plugin has a gesture notice (`NOTICE_GESTURE_ON_LOAD`, the notice byte's value 2): on each state load it reports one gesture on `aux`, a begin, changes to 0.375 and 0.625, an end (CLAP from the next `process`; the C++ VST3 plugin does the same). The contract's `a_gesture_is_reported_in_order` holds each format to `spec::GESTURE` (begin, change 0.375, change 0.625, end), in process, and `tests/server.rs` holds the three served formats to it across the wire. AU has no build of the shared plugin: its case is a gap, and `tutti-au-host`'s own test notifies a gesture through the listener.
   - Step 2's `ParamChanged { gesture }` and step 3's `param.edit()` carry this type.
3. **The wire carries `f64` parameter values.** Built.
   - `SetParameter`, `ParameterValue` and `ParameterChanged` carry the `Normalized` as the `f64` it is (`wire::normalized`; the `f32` form is gone), still refused on decode outside `0..=1`. `PROTOCOL_VERSION` is 31 for this and for item 2.
   - A served `0.3` reads back as `0.3` (`tests/server.rs`, `*_value_keeps_its_f64_width_across_the_process_boundary`). AU and VST2 hold `f32` in the format, so what they report crosses as that `f32` widened.

The type and range of each value in each format, beside tutti's type for it, are tabulated in [016](016-plugin-hosts.md) §3.5.

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
| VST2 shell plugins (`effShellGetNextPlugin`, a sub-plugin id) make "one plugin per VST2 file" false; `audioMasterCurrentId` answers 0 outside a shell | 6a (done: the probe lists every plugin of a file, a shell's sub-plugins included; the `Load` is `vst2.<uniqueID>`; `audioMasterCurrentId` answers the chosen id, 0 when none) |
| AU's 20 contract gaps are one missing AU shared plugin, not AU limits; the two `f32` round trips want a per-format tolerance, not a gap | AU completeness (the fixture is an in-process registered `AUAudioUnit` subclass) |
| AU input as UMP (`MusicDeviceMIDIEventList`, `HostMIDIProtocol` / `AudioUnitMIDIProtocol`, the Group in each packet); the legacy path keeps the `Narrower`; v2 has no cable routing | AU MIDI input parity (done: `initialize` reads `AudioUnitMIDIProtocol` before `AudioUnitInitialize`; `HostMIDIProtocol` is not written, since writing it breaks withdrawing the legacy output callback (macOS 26.6) and is the output lane's, with the event-list callback; a unit that says MIDI 2.0 gets UMP unchanged, one list per protocol, timestamps are frame offsets, the list built in a buffer reserved at activation (`tutti_au_ffi::EventList`); MIDI 1.0 or no answer keeps `midi1` and `MusicDeviceMIDIEvent`, Group 0 only, other Groups and unnarrowable messages counted. Not covered: no installed Apple unit answers the protocol property, so the UMP path is tested over a sink and the call on AUSampler, not end to end through a unit that takes MIDI 2.0; waits for the AU shared plugin) |
| Project time computed from beats at one tempo (`transport_source.rs`), not from the `TempoMap`; VST2 extrapolates a split block's transport at the block's first tempo | a transport PR beside step 4 |
| A VST3 aux bus without `kDefaultActive` stays inactive though the graph wires it | step 4 (buses as graph handles activate what is wired): the handles are built (4b), the activation is not: the node does not learn which ports are wired |
| VST3 editor: the served editor's run loop not pumped on Linux, in-process resize stubbed, no content-scale update after open | step 2 (`update()` services every editor) |
| AUv3 | after AU completeness (§6.4) |
| A note is `(Group, NoteId)` at the port edge (Group N is port or bus N), so `NoteId` keeps `channel * 128 + note` and gains no Group; the legacy note-expression route lacks port, channel and key | plugin MIDI out by Group |
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
