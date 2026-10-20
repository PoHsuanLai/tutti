# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Docs — `ScopedNoDenormals` soundness

- `ScopedNoDenormals` documents its `# Soundness` (the FP-environment rule it departs from, the audit of what runs under it, why the parallel render stays bit-identical); no behaviour change.

### Changed — RT primitive cleanups: `RtScratch` dropped, `Capped` folded into `RtVec`, `AudioThread` renamed `RtScope`

| Was | Now |
|---|---|
| `tutti_types::RtScratch<T>` (also re-exported from `tutti_core`) | removed. Own a `Box<[T]>` sized off the audio thread and slice it at the use site (`&mut buf[..len]`), deciding there what an oversized request does. |
| `tutti_types::RtScratchOverflow` | removed, with `RtScratch`. |
| `RtScratch::active` clamping an oversized length in release builds | each former user states its own behaviour: `meter_output` meters the first 8192 frames of an oversized buffer (debug builds still panic); `stretch::Unit::process` renders a block past its 8192-frame scratch as consecutive runs instead of truncating the bypass copy and panicking mid-vocoder; the VBAP panner's scratch is always used whole. |
| `tutti_types::rt::capped` (crate-private `Capped<T, N>`) | folded into `RtVec`, which now holds its `ArrayVec` and overflow flag itself. `RtVec`'s API is unchanged. |
| `tutti_types::AudioThread` | `tutti_types::RtScope` |
| `tutti_types::AudioThreadGuard` | `tutti_types::RtScopeGuard` |
| `AudioThread::enter()` | `RtScope::enter()` |
| `AudioThread::is_current()` | `RtScope::is_inside()` |
| `AudioThread::check_not_current(what)` | `RtScope::check_outside(what)` |
| `tutti_types::rt::audio_thread` | `tutti_types::rt::scope` |

Migration: rename per the table; `AudioThreadCell` is unchanged. A former
`RtScratch::new(n)` is `vec![T::default(); n].into_boxed_slice()`;
`active(len)` / `active_ref(len)` are `&mut buf[..len]` / `&buf[..len]`
(which panic past the length in every build), `try_active(len)` is
`buf.get_mut(..len)`, and `capacity()` is `len()`.

`ScopedNoDenormals` stays: the `no_denormals` crate's only API is an
`unsafe fn` taking a closure, which `tutti-graph`'s
`#![forbid(unsafe_code)]` cannot call, and it is a `compile_error!` on
any architecture but x86, x86_64 and aarch64, where `ScopedNoDenormals`
compiles to nothing (see the PR for the verification).

### Changed — design 015 PR 7: rules, gates and a pinned toolchain

- **`scripts/check-adapter.sh`**, run in `just ci` and CI's paths job,
  enforces design 015 §8 rules 3 and 4.
  - `crates/bevy-tutti/src` may not name `Editor`, `Executor`,
    `Modulation`, `TuttiDriver`, `AudioCallbackState`, `Engine` or
    `RenderGraph`. Tests and comments are not checked. A hit names the
    session item that owns the type.
  - `crates/tutti-session/src` may not re-export a crate whole. That check
    moved here from `check-bevy-free.sh`, and now parses each `pub use`
    as a use tree, so nested groups and root groups are read.
- **`bevy-tutti` runs the session's phases in `PHASES` order.**
  `SessionPhaseSystems(Phase)` is a system set per phase. They are chained
  from `PHASES` and placed in the `GraphReconcileSystems`.
  - `Phase::Collect` now runs first in the frame, in its own system,
    `collect_graph`, before `Spawn`. Before, `commit_graph` ran it just
    ahead of the commit. With a device, a retired node's free can slip one
    frame, when the audio thread has not yet picked up the previous commit.
    That is benign: the next frame's collect frees it, on the main thread.
  - With `plugin` and `export`, `start_exports` (the live graph's fork) now
    runs before the frame's `Params` and `Commit`, because
    `plugin_render_mode` runs after it, in
    `SessionPhaseSystems(Phase::RenderMode)`. Before, the fork was unordered
    against them.
  - `Phase::MidiOut` (the clock pump) was unordered against the commit. It
    now follows `AfterCommit`.
  - A test derives the expected order from `PHASES` and checks it against
    the phases the session ran.
- **The `tutti` prelude gains the session's request types**, behind the
  features that gate them:
  - `ExportRequest` and `ExportOutput` (`session` + `export`);
  - `PluginLoadRequest` (`session` + `plugin`) and `PluginId` (`plugin`);
  - `ParamRef` (`session` + `modulation`);
  - `GraphSource`, `ExportConfig`, `MidiFile`, `TimedMidiEvent`,
    `OfflineTimeline` and `OfflineTimelineConfig`.
- **The two session examples moved to `tutti`** and use
  `tutti::prelude::*`:
  - `cargo run -p tutti --features modulation --example headless_modulation`;
  - `cargo run -p tutti --features clap,export --example plugin_midi_export`.

  `tutti-session` cannot dev-depend on `tutti`, which depends on it.
- **Reproducible builds.**
  - `Cargo.lock` is committed. Every cargo command in CI and `just` passes
    `--locked`, so a drifted lock fails rather than resolving newer
    dependencies.
  - `rust-toolchain.toml` pins Rust 1.99.1 with clippy and rustfmt. CI
    installs it with `rustup toolchain install` in place of
    `dtolnay/rust-toolchain@stable`. Miri names `+nightly`.
  - A consumer that pins this repo as a git dependency resolves with its
    own lock, so the `audio-automation` rev and the bevy 0.19 line still
    have to stay in step.
- **`bevy-tutti --features plugin,modulation` builds its tests without dead
  code.** `tests/mod_plugin.rs` needs `export` or `vst2` to load a plugin,
  and is now gated on them. CI's dark-features job lints that combination,
  and lints and tests `tutti --features clap,export,modulation`.
- **Breaking.** The table below gathers design 015's renames, PRs 4–7, for
  a host that skips straight to this release. Each row is checked against
  the code.

  | Was | Now |
  |---|---|
  | `bevy_tutti`'s crate-private `GraphRuntime` (`graph/runtime.rs`) behind `AudioGraphRes` | `tutti_session::Graph`, inside the `Session` that `AudioGraphRes` wraps. A headless host uses `Session::graph()` |
  | `GraphNode` | `SessionNode` (`bevy_tutti::graph::SessionNode`, also in the prelude) |
  | `TuttiPlugin`'s `inputs`, `outputs`, `render_workers` and `output_device` building the engine in `bevy-tutti` | the same fields, unchanged, passed to `Session::builder()`: `.inputs`, `.outputs_floor`, `.render_workers` and `.build_device(output_device)`. A headless host calls the builder |
  | `GraphDirty`, set by hand after an edit | none: an edit through `AudioGraphRes` is committed by that frame's `Phase::Commit` |
  | Bevy's `ExportRequest` component | unchanged for a Bevy app. `start_exports` translates it into `tutti_session::ExportRequest`. A headless host builds that type and calls `Session::export` |
  | `bevy_tutti::export::RenderGraph` (a re-export of `tutti_export::RenderGraph`) | removed: name it as `tutti_export::RenderGraph` |
  | `commit_graph` ran `Phase::Collect`, then `Phase::Commit` | `collect_graph` runs `Collect` first in the frame, in `SessionPhaseSystems(Phase::Collect)`. `commit_graph` runs only `Commit` |
  | `cargo run -p tutti-session --example {headless_modulation,plugin_midi_export}` | `cargo run -p tutti --example …`, with the same features |

### Changed — `Param64` replaces `AtomicSamplePosition` and `AtomicReadRate`

- **One `f64` cell beside `Param`** (issue #75). `AtomicSamplePosition` and
  `AtomicReadRate` were the same `AtomicU64`-of-bits wrapper under two
  names; `tutti_types::Param64<U: Unit<Raw = f64>>` (re-exported from
  `tutti_core`) is the one cell. `load` and `store` take and return the unit,
  with `Param`'s `Acquire`/`Release` pairing, and construction does not
  allocate. `Param` stays the `f32` cell. The old names are removed, with no
  alias. Neither old type's `Default` was used; `Param64` has none, so the
  initial value is always named.

  | Was | Now |
  |---|---|
  | `AtomicSamplePosition::new(p)` | `Param64::<SamplePosition>::new(p)` |
  | `AtomicReadRate::new(r)` | `Param64::<ReadRate>::new(r)` |
  | `cell.load(order)` | `cell.load()` (`Acquire`) |
  | `cell.store(v, order)` | `cell.store(v)` (`Release`) |
  | `AtomicSamplePosition::default()` | `Param64::new(SamplePosition::new(0.0))` |
  | `AtomicReadRate::default()` | `Param64::new(ReadRate::UNITY)` |

### Changed — design 015 PR 1: MIDI and sample pieces to their owning crates

- **MIDI file decoding, the inbound route compile and the memory voice move
  out of `bevy-tutti` into the crates that own them** (design doc 015,
  PR 1), so a headless host gets them without Bevy. `bevy-tutti` calls
  them; its behaviour is unchanged.
  - `tutti_midi_file::MidiFile` is the sniffed file (SMF or MIDI 2.0 Clip
    File) that was `bevy_tutti::midi::MidiFileContents`. New: `timed()`
    yields `(Beat, MidiEvent)` for both containers, normalized to MIDI 2.0
    (an SMF's 7-bit values widened), with `tempo()` (the tempo at beat 0),
    `tempo_map()` (every Set Tempo as `(Beat, Bpm)`, in time order),
    `end()`, `kind()`, `read(path)` and `EXTENSIONS`. Its SMF variant is
    `Smf(SmfFile)`: the tracks and the merged event stream from one parse,
    behind `tracks()` and `stream()`, so `timed()` carries controllers,
    bends and program changes.
  - **Fixed:** `tutti_midi_file::ParsedMidiFile::tempo_bpm` was the first
    Set Tempo in *track* order, wherever it sat in time. It is now the tempo
    at beat 0: a Set Tempo at tick 0, else the SMF specification's 120. New
    field `tempo_changes: Vec<(Beat, Bpm)>` holds every change in time
    order (breaking for a struct literal; none exists outside the crate).
  - `tutti_midi_file::Error` gains `Clip(ClipFileError)` and
    `UnknownFormat`, the two errors of `MidiFileLoaderError` it lacked.
  - `tutti_midi_runtime::compile_routes(rules, fallback)` is the pure route
    compile (rules to input-node ports, the fallback taking only uncovered
    ports) that `bevy_tutti::midi::rebuild_midi_routes` ran inline;
    `RouteRule { channel, targets }` is its input.
  - `tutti_sampler::{memory_voice, voice_width}` were
    `bevy_tutti::sampler`'s. `bevy_tutti::prelude` still re-exports both.

  | Was | Now |
  |---|---|
  | `bevy_tutti::midi::MidiFileContents::Smf(tracks)` | `tutti_midi_file::MidiFile::Smf(smf)`, `smf.tracks()` (and `smf.stream()`) |
  | `ParsedMidiFile::tempo_bpm`: the first track's first Set Tempo | the tempo at beat 0; changes in `tempo_changes` |
  | `bevy_tutti::midi::MidiFileContents::Clip(clip)` | `tutti_midi_file::MidiFile::Clip(clip)` |
  | `MidiFileAsset::from_bytes(bytes)` | `MidiFile::from_bytes(bytes)`, the asset's `contents` |
  | `MidiFileAsset::EXTENSIONS` | `MidiFile::EXTENSIONS` |
  | `bevy_tutti::midi::MidiFileLoaderError` (`Io`, `Smf`, `Clip`, `UnknownFormat`) | `tutti_midi_file::Error` (`Io`, `Clip`, `UnknownFormat`; an SMF's parse errors are its own variants) |
  | `bevy_tutti::sampler::{memory_voice, voice_width}` | `tutti_sampler::{memory_voice, voice_width}` (also in `bevy_tutti::prelude`) |

### Fixed — `AudioThreadCell` checks its one-borrow contract in release builds

- **Soundness: safe code could create two `&mut T` through an
  `AudioThreadCell` in a release build.** The cell is `Sync` for `T: Send`
  and its `borrow`/`borrow_mut` are safe, but the "one borrow at a time"
  check ran only under `debug_assertions`. `tutti_core::MotionFsm` is a
  public `Clone` handle whose safe `drain` and `complete_declick` borrow its
  FSM, so a clone driven from a UI thread while the callback drained was a
  data race with no `unsafe` in sight. The check is now unconditional, as
  `atomic_refcell`'s: one uncontended compare-exchange per borrow, a store
  on the guard's drop, no lock, no allocation, nothing that waits. A second
  conflicting borrow panics, in every build, with the same message and call
  site; the new `try_borrow_mut` and `try_borrow` return `None` instead. A
  `loom` model (`tutti-types/tests/cell_loom.rs`, in `just loom` and CI)
  checks the state word against the shipped code.
- **`borrow` is a real shared borrow.** The state word is a writer bit plus
  a reader count, so any number of `borrow`s are live at once, and none
  while a `borrow_mut` is. Before, `borrow` took the same exclusive flag, so
  two threads reading one cell (a VST3 plugin reading its parameter queues
  from two threads, which COM allows) would have panicked. `borrow` and
  `try_borrow` need `T: Sync`, since the readers share one `&T`; the cell
  itself stays `Sync` for `T: Send`, so `borrow_mut` keeps working on a
  `!Sync` `T` (the engine's graph holds `Box<dyn Node>`).
- **The audio thread and the COM entry points never panic on a held
  borrow.** The VST3 host's `IParamValueQueue`, `IParameterChanges` and
  `IEventList` methods take the `try_` forms and return their failure value
  (`0`, null, `kResultFalse`, `kNoParamId`) on a conflict: a panic cannot
  unwind out of an `extern "system"` thunk, and would abort the plugin
  server. On the audio side, `MotionFsm::drain` leaves the event queued and
  `complete_declick` leaves the declick pending when the control side holds
  the FSM (both now return `bool`, `true` when the work ran), a scheduled
  command refused the FSM lands on a later block (counted late), and
  `Engine::process`/`process_segment` render a silent block when the control
  side holds the graph. Control-side callers keep the panicking `borrow_mut`.
- **The guards no longer make `T` shareable.** `BorrowGuard` and
  `BorrowRef` were `Sync` for any `T: Send` (they inherited the cell's
  auto traits), so a `&BorrowRef<Cell<_>>` shared between two threads gave
  each a `&Cell` at once. `BorrowGuard` now carries `&mut T`'s auto traits
  and `BorrowRef` `&T`'s. No caller in the workspace shared one.
- **`tutti-types`' tests run in release in CI** (`cargo nextest run
  --release -p tutti-types`, in the `test` job), so a check that only a
  debug build makes cannot pass for one every build makes.
  `io::tests::pump_rejects_a_width_mismatch` tests a `debug_assert` and is
  gated to debug builds.
- **`reset_owner` is gone.** It was a no-op (the cell pins no owner thread;
  CoreAudio migrates its callback between threads), and `MotionFsm`,
  `Engine` and `AudioCallbackState` forwarded to it, the last called on
  every `TuttiDriver` restart.
- **`Engine::settle_graph` is safe.** It was `unsafe fn` only because the
  cells it borrows from the control side checked a racing callback in debug
  builds alone; with the check always on, a race panics instead, so the
  `unsafe` contract is now a documented panic. `Stopped::settle_graph` is
  unchanged and remains the way in that cannot race.

  | Was | Now |
  |---|---|
  | `AudioThreadCell::borrow`/`borrow_mut` checked a live borrow only with `debug_assertions` (release: unchecked aliasing) | checked in every build: a conflicting live borrow panics (same message, `#[track_caller]` site) |
  | `AudioThreadCell::borrow()` was exclusive, for any `T` | shared (any number of readers, no writer); needs `T: Sync` |
  | — | `AudioThreadCell::try_borrow_mut() -> Option<BorrowGuard<'_, T>>`, `None` while any borrow is live |
  | — | `AudioThreadCell::try_borrow() -> Option<BorrowRef<'_, T>>` (`T: Sync`), `None` while a `borrow_mut` is live |
  | `BorrowGuard<'_, T>`: `Sync` when `T: Send` | as `&mut T`: `Send` when `T: Send`, `Sync` when `T: Sync` |
  | `BorrowRef<'_, T>`: `Sync` when `T: Send` | as `&T`: `Send` and `Sync` when `T: Sync` |
  | `MotionFsm::drain(&self)`, `MotionFsm::complete_declick(&self)` (tutti-core) | return `bool`: `false` when the FSM was held and the work was left for a later call; never panic |
  | `AudioThreadCell::reset_owner()` (tutti-types) | removed; delete the call (it did nothing) |
  | `MotionFsm::reset_owner()` (tutti-core) | removed; delete the call |
  | `Engine::reset_owners()` (tutti-core) | removed; delete the call |
  | `AudioCallbackState::reset_owners()` (tutti-cpal) | removed; delete the call |
  | `unsafe fn Engine::settle_graph(&self) -> bool` | `fn Engine::settle_graph(&self) -> bool`: drop the `unsafe` block; it panics if a callback renders the engine at the same moment |
  | `AudioThreadCell::new` is `const` | still `const`, except under `--cfg loom` (loom's atomics are not) |

### Changed — design 015 PR 3: the audio pump is `tutti_io::Recorder`

- **One pump loop.** `bevy-tutti`'s `AudioPump` ran its own copy of
  `tutti_io::Recorder`'s thread loop; it now holds a `Recorder`, and
  `Recorder` gained what the component had: any sink and element type, a
  capacity, a park, and a non-blocking end. **Breaking** for code that names
  `ThreadDriver` as a value, implements `PumpDriver` or `RunningPump`, or
  names `PumpLoop<I>`. `AudioPump::start` and its systems keep their
  signatures.

  | Was | Now |
  |---|---|
  | `Recorder::{start, start_with}` took a `WavOut` | any `O: AudioOut<S>` over any `S: Default + Copy + Send` (inferred; `WavOut` callers compile unchanged) |
  | the scratch was always 1024 frames | `Recorder::start_with_capacity(src, sink, capacity: Samples, driver)`; zero is one frame |
  | `ThreadDriver` (unit struct) parked a private 5 ms | `ThreadDriver::default()` parks `tutti_io::IDLE_PARK` (now public); `ThreadDriver::with_park(Duration)`, `ThreadDriver::park()`. Replace a `ThreadDriver` value with `ThreadDriver::default()` |
  | `PumpLoop<I>` | `PumpLoop<I, O, S = f32>` |
  | `PumpDriver::drive<I: AudioIn + Send>(self, PumpLoop<I>, ..)` | `drive<I, O, S>(self, PumpLoop<I, O, S>, ..)` with `I: AudioIn<S>`, `O: AudioOut<S>`, `S: Copy`, all `Send + 'static` |
  | `RunningPump { join }`; `Recorder` was `!Send` | `RunningPump: Send + Sync { join, is_finished }`; `Recorder` is `Send + Sync`. A custom running handle adds `is_finished` (never blocks) |
  | `Recorder` could only be ended by consuming it (`stop`, `wait`) or dropping it | also `request_stop(&self)`, `is_running(&self)`, `is_finished(&self)`, and `shutdown(&mut self) -> Option<io::Result<()>>` (joins in place; `None` once collected) |
  | — | `impl From<tutti_io::Error> for std::io::Error` (`Io` unwraps; `ChannelWidthMismatch` becomes `InvalidInput` with the typed error as its source) |
  | `AudioPump::start` with mismatched widths ran anyway (only `pump`'s `debug_assert` objected) | it starts no thread, and the drain reports `PumpFinished { result: Err(InvalidInput) }` carrying `ChannelWidthMismatch` |
  | an `AudioPump` dropped with its world (no `Remove` observer) detached its thread unfinalized | the `Recorder` inside finalizes in its `Drop` |
  | `tutti-io` was an optional `bevy-tutti` dependency (`audio-io`, `sampler`) | it is always one: `graph::pump` is always compiled. `wav`/`flac`/`mp3`/`ogg` now turn on `tutti-io`'s codec unconditionally |
  | the pump's thread panic read "audio pump thread panicked" | "recording thread panicked", `Recorder`'s message |

### Changed — design 015 PR 4: `tutti-session`, the Bevy-free API layer

- **New crate `tutti-session`**: the cross-crate composition both
  umbrellas use. A `Session` is a `Graph` (the editor, the nodes inserted
  through it and their event feeds), its transport, and the engine side it
  renders on, built by a `SessionBuilder` offline (`build_offline`, then
  `Session::render`), detached (`build_detached`) or on a device
  (`build_device`, `build_device_on`; feature `device`, giving back a
  `!Send` `Device`). `Session::update()` runs a frame's phases in `PHASES`
  order and commits iff the graph was edited. Every module is private, it
  has no `bevy` feature, and it re-exports no crate whole
  (`scripts/check-bevy-free.sh`, now in `just ci` and CI).
- **`tutti` re-exports it** as `tutti::session`, with
  `default = ["session"]` (the one non-empty default; it adds no device and
  no ALSA). Each feature forwards to the session's feature of the same
  name with `?`. The prelude gains `Session`, `SessionBuilder`,
  `SessionEvent`, `Phase` and, with `device`, `Device`.
  `examples/headless_engine.rs` is now 19 lines on `Session`.
- **`bevy-tutti` runs on the session.** `AudioGraphRes` wraps a `Session`,
  `engine/build.rs` calls the builder, and the device restart is the
  session's. **Breaking:**

  | Was | Now |
  |---|---|
  | `bevy_tutti::graph::GraphDirty` (set by hand after an edit) | gone: every edit through `AudioGraphRes` marks the session graph edited (`AudioGraphRes::is_edited`), and `commit_graph` commits iff it is set |
  | `GraphNode` (trait; `GraphNode::params`) | `SessionNode` (`bevy_tutti::graph::SessionNode`, also in the prelude); `param_graph_node!` unchanged for callers |
  | `GraphNode::captured(&self) -> CapturedControls` (default: nothing captured) | gone: the session's trait cannot name a Bevy type. Every spawn path binds `CapturedControls::default()`; a plugin's capture is `CapturedControls::for_plugin` / `for_plugin_meter`; a host with controls of another shape builds a `CapturedControls` and calls `bind(entity, node)` itself |
  | `EventFeeds` resource (`set`, `remove`, keyed by `Entity`) | `AudioGraphRes::{set_feed, feed, move_feeds}`, keyed by `AudioNode`; removing a node purges it from every feed |
  | `TuttiDriver` resource (`bevy_tutti::TuttiDriver`) | `Device`, a non-send resource (`NonSend<Device>`); `Device::devices()` enumerates |
  | `DeviceRestart { device: Option<usize>, .. }` (`Copy`) | `DeviceRestart { device: DeviceSelector, .. }` (`Clone`); `DeviceSelector::from(Option<usize>)` converts |
  | `TuttiDriver::{restart, restart_with}(Option<usize>, ..)` | take `impl Into<DeviceSelector>` (an `Option<usize>` still converts) |
  | `bevy_tutti::Error` (exhaustive) | `tutti_session::Error`, re-exported, `#[non_exhaustive]` |
  | `AudioGraphRes` held the editor and runtime state itself | it wraps a `Session`; new methods `add`, `crossfade`, `crossfade_plugin`, `is_edited`, `set_feed`, `feed`, `move_feeds` |
  | an edit staged through `AudioGraphRes` after `commit_graph` could be committed early by modulation's `after_commit` | it waits for the next frame's commit; `after_commit` commits only its own resolution, and only while the graph is otherwise clean |

  A crossfade refused `Busy` (a re-prepare in flight) is parked and
  retried by the session; `crossfade_audio_node` binds the new node's
  controls when it lands (`PendingCrossfades`, resolved from
  `SessionEvent::CrossfadeLanded`). `EngineNodes` is inserted only when the session has a click. A live MIDI
  input is fed once its entity has an `AudioNode`. The inbound routes, MIDI
  clips and live MIDI inputs follow a target re-bound to a new node, whether
  the new `AudioNode` replaces the old one in place or is inserted after the
  old one was removed: each re-asserts its feed on the entity's current node
  when its `AudioNode` changes (the old node's feeds go with it).

### Changed — design 015 PR 5: modulation and MIDI in `tutti-session`

- **The session runs the modulation loop** (feature `modulation`):
  `Phase::Modulation` applies the declarations (a host's, passed as
  `PhaseCx::with_mods(ModInput)`, or the session's own), and
  `Phase::AfterCommit` re-applies while a target waits for its node,
  commits what that built (publishing compensation on `Sent`) and sends the
  resyncs owed. A target still unresolved after 4 s of `PhaseCx::now`
  (`SessionBuilder::unresolved_warning`) is
  `SessionEvent::ModulationUnresolved`; a refusal is
  `SessionEvent::ModulationRefused(ModRefusal)`, once per change.
- **`Session::mods()`** declares modulation without a host: `lfo`,
  `macro_`, `follower`, `note_envelope` and `source` mint `SourceKey`s,
  `route` mints `RouteKey`s, `range`/`automate`/`record` name a
  `ParamRef` (`unit`, `plugin`); `Session::mod_state()` reads what
  applied. `Take` is the recorder `AutomationTake` wraps.
- **`Session::midi()`** (feature `midi`): `live_input`, `play_clip`,
  `stop_clip`, `clip`, `retarget`, the hardware input's `route`,
  `unroute`, `set_route_fallback` and host-keyed `set_routes`, and the
  hardware out: `set_hardware_out(Option<UmpOut>)`, `set_jr_stamping`,
  `hardware_out()` (the router), `drain_out`, `out_drops`.
  `Phase::MidiOut` drains the engine's hardware-out node and reports a
  rising drop count as `SessionEvent::MidiOutDropped`, at most once per
  `MIDI_OUT_DROP_REPORT` (1 s of `PhaseCx::now`). The routes have one
  declarer: `set_routes` (which returns a `Result`) is refused while the
  session's own rules or fallback exist, and `route`/`set_route_fallback`
  while a host's `set_routes` declares any, with
  `Error::TwoRouteDeclarers`.
- **Two `bevy-tutti` MIDI behaviours changed:**
  - A `LiveMidiInput` on a node with no event input gets no `LiveMidi`
    (before, a queue node fed into nothing): the attach is warned of and
    tried again when the entity's `AudioNode` changes. The attach waits
    for `AudioGraphRes` (a `run_if`), so one declared before the graph
    exists is attached when it arrives.
  - The track MIDI-out pump with no graph (no engine) still drains its
    mailbox, and counts what it drops in `MidiOutRes::unrouted_drops`,
    warning once; with a graph the session counts them
    (`AudioGraphRes::midi().out_drops()`).
- **`examples/headless_modulation.rs` moved** from `tutti-nodes` to
  `tutti-session` (design 015 §5.1): `cargo run -p tutti-session
  --features modulation --example headless_modulation`.
- **Breaking:**

  | Was | Now |
  |---|---|
  | `bevy_tutti::modulation::ModulationState` resource (`Res<ModulationState>`) | a view: `AudioGraphRes::modulation()` (`ModulationState<'_>`), whose reads return owned values (`lane` → `Option<(NodeKey, LaneControls)>`; `unresolved_targets` and `unresolved_sources` iterate a copy); `set_knob` unchanged |
  | `bevy_tutti::midi::UmpOutRes` resource | `bevy_tutti::midi::UmpOut` (`tutti_session::UmpOut`), connected with `AudioGraphRes::midi().set_hardware_out(Some(out))` |
  | `bevy_tutti::midi::MidiOutDrops` resource | the session's count, `AudioGraphRes::midi().out_drops()`; `MidiOutDrops` is a plain type (re-exported) |
  | `MidiOutRouter { out: Option<&mut UmpOutRes>, .. }`, `out_active` | `MidiOutRouter { out: Option<&mut UmpOut>, .. }`; `out_active` gone (`AudioGraphRes::midi().hardware_out()` hands the router) |
  | `ClockMasterRes { master, out }`, `ClockMasterRes::new(master, out)` | `ClockMasterRes { master }`, `new(master)`: the out ring is the session's (`Session::midi_nodes().out_controls`), drained by `Phase::MidiOut` |
  | `bevy_tutti::midi::RoutedTargets` resource | gone: the session tracks what its routes fed |
  | `SequencedClips(HashMap<Entity, (AudioNode, MidiClipControls)>)` | `SequencedClips(HashMap<Entity, AudioNode>)`; `node(entity)` unchanged |
  | `pump_clock_out_system`, `pump_midi_out_system` took `MidiOutDrops`/`UmpOutRes` | take `AudioGraphRes` and `JrStamperRes` |
  | `graph::commit::commit_and_publish` (crate-private) | gone: `Session::commit` publishes |
  | `tutti_session::Graph::{modulate, send_mod_resyncs}` (public) | `pub(crate)`: run `Phase::Modulation`/`AfterCommit` (`Session::run_phase`) |
  | `AudioNode`: `Eq + Hash` | also `PartialOrd + Ord` |

  **Migration.** Read modulation state through
  `world.resource::<AudioGraphRes>().modulation()` (or a
  `Res<AudioGraphRes>` in a system). Connect a JR output with
  `graph.midi().set_hardware_out(Some(UmpOut::new(sink, rate)))` instead
  of inserting `UmpOutRes`; `JrStamperRes` still switches stamping. Read
  the clock's out ring, in a test, from the session's MIDI nodes. A
  headless host that called `Graph::modulate` passes its declarations as
  `PhaseCx::with_mods` to `run_phase`, or declares through `s.mods()`.

### Changed — design 015 PR 6: plugins and export in `tutti-session`

- **Hosted plugins and exports are the session's** (design doc 015, PR 6),
  so a headless host loads a plugin, plays a MIDI file into it and bounces
  it without Bevy (`crates/tutti-session/examples/plugin_midi_export.rs`).
  `bevy-tutti` runs the session's `Phase::Plugins` and `Phase::RenderMode`
  and translates its `ExportRequest` component into the session's.
  **Breaking** for code that names the removed `bevy-tutti` items below.
  - The load is a job: `Session::plugin_load(PluginLoadRequest)` →
    `PluginLoad` (`Send`), `run()` → `PluginLoaded`, then
    `Session::add_plugin` on the control thread. The session keeps a
    `HostedPlugin` per node (`Session::plugin`, `plugins`): its handle,
    controls, meter, liveness, last snapshot and parameter mirror.
  - `Phase::Plugins` debounces each plugin's health and removes a dead one
    (`SessionEvent::PluginDied`), hands a moved latency to the graph
    (`PluginLatencyChanged`), writes GUI edits to the lanes' knobs, and asks
    for a state snapshot every `SNAPSHOT_INTERVAL` (8 s,
    `SessionBuilder::snapshot_interval`) as a job
    (`PluginSnapshotDue`, landed with `Session::land_snapshot`).
  - `Session::export(ExportRequest)` → `ExportJob`, `run()` on any thread →
    `ExportRendered`, `Session::finish_export` on the control thread. One at
    a time: a second is `Error::ExportInFlight`. Plugins hear Offline from
    `export` and Realtime from `finish_export`, or from the next update when
    a job or its return is dropped. What an export leaves behind drops on the
    control thread (`Phase::Collect`, `finish_export`,
    `drop_export_leftovers`).
  - `tutti_midi_file::MidiFile::seconds_at(Beat) -> f64`: the time to a
    beat over the file's tempo map.
  - Behaviour changes: a crossfade to another plugin moves its health and
    latency watch to the incoming one; a plugin inserted with
    `AudioGraphRes::insert_plugin` is health-watched too; the state snapshot
    is every 8 s of wall clock, not every 512 frames; `plugin_health_poll` is pinned
    to the main thread and runs in `GraphReconcileSystems::Spawn`.
  - **Fixed:** every plugin load panicked in a debug build of a
    multi-threaded Bevy app: `plugin_load_promote` ran on a pool thread and
    `add_plugin` asserts the main thread. It, `plugin_state_snapshot` and
    `plugin_render_mode` are now pinned to the main thread
    (`NonSendMarker`), and an in-process (VST2) plugin opens in
    `add_plugin` rather than in the load job, which ran on the task pool
    (`Plugin::opens_in_process`). CI and `just ci` run the plugin host on
    a threaded executor. That open (the library's init, the state restore
    and the parameter read) stalls the frame it lands in, so
    `plugin_load_promote` opens at most one in-process plugin per frame.
  - **Fixed:** `plugin_render_mode` was never scheduled in a `TuttiPlugin`
    app (the hosting plugin is added before `ExportPlugin`, which it checked
    for). `ExportPlugin` schedules it, after `poll_exports` and
    `start_exports`.
  - Back-to-back Bevy exports stay Offline: `poll_exports` finishes with
    `Session::finish_export_unannounced` and the frame's
    `plugin_render_mode` announces the level once. `finish_export` and
    `export_blocking` still announce.
  - `Graph::replace` and `Graph::crossfade` to a native node drop the
    plugin's record, so a later `PluginDied` cannot remove the native node
    and the record no longer keeps the old plugin server alive. A crossfade
    or `replace_plugin` to another plugin gives it a fresh liveness, no
    snapshot (a save in flight from the old plugin is refused) and a mirror
    read from the new plugin. The read is a job
    (`SessionEvent::PluginParamsDue`, landed with `Session::land_params`),
    not a round trip per parameter on the control thread; until it lands
    the mirror is not ready.
  - An export token held past `EXPORT_STALL_WARNING` (an hour) is warned
    about once (`tracing::warn!`) from `Phase::Collect`; it holds Offline
    until it drops. Each export starts its own clock.
  - `tutti_plugin::PluginControls::{missed_chunks, chunk_in_flight_answered}`
    report what the node played: chunks played unanswered while the bridge
    lived, and whether the chunk in flight is answered yet.

  | Was | Now |
  |---|---|
  | `bevy_tutti::plugin_host::plugin_latency_poll` (the `latency` module) | the session's `Phase::Plugins`, run by `plugin_health_poll` |
  | `bevy_tutti::plugin_host::{read_plugin_params, plugin_param_edits, PluginParamRead, PluginParamValues}` (`plugin_host::params`) | the session's mirror: `AudioGraphRes::with_plugin(node, \|p\| p.param_values())`; GUI edits reach the knobs in `Phase::Plugins` |
  | `bevy_tutti::plugin_host::{PluginRenderMode, RenderModeAnnounced, plugin_render_mode_drive}` | `plugin_render_mode` (runs `Phase::RenderMode`); `AudioGraphRes::render_mode()` |
  | `bevy_tutti::plugin_host::PluginLiveness` (Bevy's enum) | `tutti_session::PluginLiveness`, re-exported at the same path, same variants |
  | `bevy_tutti::export::ExportDrops` | none: the session drops an export's leftovers (`poll_exports` calls `drop_export_leftovers`) |
  | `bevy_tutti::export::{ExportTarget, ExportOutput, ExportClock, PreparedGraph, PrepareEdit}` (Bevy's own) | the session's, re-exported at the same paths |
  | `tutti_session::{PendingExport, AudioNodeTarget}`, `Graph::gather_for_export` | crate-private: `Session::export` |
  | `tutti_session::ModKeys::plugin_value` | deleted: the session reads its own mirror |
  | — | `Session::{plugin_load, add_plugin, add_plugin_blocking, plugin, plugins, land_snapshot, render_mode, export, finish_export, export_blocking, export_in_flight, drop_export_leftovers}`, `Graph::host_plugin`, `SessionBuilder::snapshot_interval`, `Error::ExportInFlight`, `SessionEvent::{PluginDied, PluginLatencyChanged, PluginSnapshotDue}` |
  | — | `AudioGraphRes::{with_plugin, render_mode}`, `CapturedControls::for_hosted` |
  | `bevy-tutti/plugin` enabled `tutti-plugin/bevy` | it does not: no plugin entity holds a `PluginParamValues` |
  | `plugin_load_promote`, `plugin_state_snapshot`, `plugin_render_mode` on any thread | pinned to the main thread |
  | a VST2 plugin opened in the load task | opened in `add_plugin`, on the main thread, stalling that frame for the library's init, the state restore and the parameter read; Bevy opens at most one per frame; `PluginLoaded::error()` is `None` until then |
  | `plugin_render_mode` scheduled by the hosting plugin, if `ExportPlugin` was already added | scheduled by `ExportPlugin`, after both export systems |
  | back-to-back Bevy exports: Offline → Realtime → Offline | Offline throughout (`finish_export_unannounced`) |
  | `Graph::replace`/`crossfade` from a plugin to a native node kept the plugin's record | drop it |
  | a plugin→plugin crossfade kept the old snapshot, snapshot token and mirror | fresh token, no snapshot, mirror read from the new plugin by a job (`PluginParamsDue`, `land_params`) |
  | a leaked export token: Offline, silently | Offline, warned once past `EXPORT_STALL_WARNING` |
  | — | `Session::{finish_export_unannounced, land_params}`, `PluginLoad::opens_in_process`, `PluginParamRead`, `PluginParamsRead`, `SessionEvent::PluginParamsDue`, `tutti_session::EXPORT_STALL_WARNING`, `tutti_plugin::Plugin::opens_in_process`, `PluginControls::{missed_chunks, chunk_in_flight_answered}` |

### Added

- **`tutti_types::{SeqWriter<W>, SeqReader<W>}`**, a sequence lock over
  `W` atomic words: the one writer overwrites them in place, any thread
  copies them whole, neither side allocates or locks, and a read never mixes
  two writes (loom model `tests/seq_cell_loom.rs`). The writer is not
  `Clone` and writes through `&mut self`, so a second writer cannot exist
  (two writers would let a reader accept words of two writes);
  `SeqWriter::reader` hands out `Clone` read sides, and `SeqWriter::last` is
  the writer's own copy of its last write. For a value the audio thread
  writes each block; the in-process and out-of-process VST2 transport
  snapshot is its first user, replacing a hand-rolled `unsafe` seqlock in
  `tutti-vst2-host`. `SEQ_CELL_READ_RETRIES` is how many overlapped copies a
  read retries before it returns `None`.

- **A cost model and coarsening for the parallel executor** (design doc
  013, "Phase 6, part (b)"). The compiler prices every op from what its
  node declares, packs sibling tasks too cheap to dispatch alone into
  larger ones (never lengthening the critical path), and totals the work and the critical path; `Plan::schedule`
  turns those into the decision whether a pool spreads a block, from which
  block length, over how many participants. A graph too cheap to spread
  (sixteen gains and a sum) now renders serially on a pool instead of
  losing to it. A speed decision only: the parallel render stays bit for
  bit the serial one. **Breaking** only for code that builds a
  `tutti_graph::Shape` with a struct literal (it has a new field); every
  `Shape::audio(..)` builder keeps compiling and declares `Cost::Heavy`.
  `Cost` and `Schedule` are `#[non_exhaustive]`: match them with a
  wildcard arm.

  | Was | Now |
  |---|---|
  | — | `Shape::cost`, `Shape::with_cost(Cost)`; `tutti_graph::Cost::{Light, Moderate, Heavy}` (default `Heavy`: a node that declares nothing is assumed worth a thread) |
  | — | `Plan::schedule(participants) -> Schedule` (`Serial`, or `Parallel { wake, from }`); `Plan::work()`, `Plan::critical_path()` (`tutti_graph::Work`, affine in the block length) |
  | a pool spread every plan of two or more tasks, waking `Plan::task_width()` participants | a pool spreads what `Plan::schedule` says pays, from its `from` block length up, waking its `wake` participants; `Executor::is_parallel()` reports that verdict |
  | `Plan::tasks()` were fused chains | tasks are fused chains with cheap siblings (one predecessor set) packed together; a task's ops are ascending but need not be ordered by the DAG (the verifier checks every dependency) |
  | a graph whose nodes declare nothing spread whenever it had two tasks | its nodes are still each worth a task (`Heavy`), but its copy and event ops are priced cheap and may pack, the wake count is `max(work / critical, widest level)` (at least 2), and a plan whose only parallelism is in those ops may run serially |
  | — | `tutti-nodes`' filters, oscillators, dynamics, delays and modulation declare `Cost::Moderate`; its sums, downmix, brickwall and test stimuli `Cost::Light`; the bus strip `Cost::Moderate` (it was `Light`: measured 3.7 ns per frame and channel); convolution declares nothing (`Heavy`) |
  | — | the other crates' nodes declare from measured per-frame costs: the MIDI nodes (clip, harmony, clock, queue, input, out) `Cost::Light`; `MemorySource`, `DiskSource`, `DiskVoice`, a plain `VoiceNode`, `VbapPannerNode`, `ClickNode` and `MicMonitorNode` `Cost::Moderate`; `PolySynth` `Moderate` up to 16 oscillator lanes (`max_voices` × unison) and `Heavy` past them; a stretched `VoiceNode`, `VoicePool`, `SoundFontUnit` and `HrtfBinauralNode` `Heavy` (stated); plugin nodes and `PluginAutomation` still declare nothing (`Heavy`) |
  | packing ran once, so cheap siblings each wired to their own outputs could pack to more tasks than undeclared | passes 2 and 3 (pack siblings, fuse chains) repeat to a fixed point; the critical path is still never longer than chain fusion alone gives |

- **The parallel executor** (design doc 013, "Phase 6, part (a)"). A
  block's independent nodes run across a pool's threads, bit for bit the
  render the serial executor makes; the serial walk is the one-participant
  case, and a pool is opt-in.

  | Was | Now |
  |---|---|
  | blocks render on the calling thread only | `Executor::set_pool(Some(pool))` spreads each block's tasks over `pool`; `Executor::is_parallel()` |
  | — | `tutti_graph::Pool` (the trait a pool implements: `participants`, `run(wake, job)`), re-exported as `tutti_core::Pool` |
  | — | `tutti_core::{WorkerPool, WorkerPoolBuilder, OnStart}`: helper threads that spin, yield, then park; `on_start` hook for RT priority / workgroups |
  | — | `tutti_types::{JobGate, TaskGraph, Participant, SplitMut, SplitRw, RwView, Held, ClaimTable, Claimed, ClaimedRun, ClaimConflict}` (loom-checked runtime primitives) |
  | — | `Plan::task_width()` (the task DAG's widest level; since part (b) one input to the wake count, see above) |
  | — | `RenderGraph::{set_pool, is_parallel}` (tutti-export) |
  | — | `TuttiPlugin::render_workers` (bevy-tutti; default 1, `0` = one per core); an export forks onto a pool of its own |

- **Curve-segment automation events** (design doc 013, decision 7,
  "Curve segments landed"). A param port evaluates a non-linear automation
  shape per frame inside the block, sample-accurate, with no sub-chunking
  at breakpoints. **Breaking** for code that matches `EventKind`
  exhaustively (a new variant).

  | Was | Now |
  |---|---|
  | automation reached a param as linear `ParamRamp`s only | `EventKind::Segment(ParamSegment)`, `Event::segment`: an absolute move from a start value to an end value along a `SegmentShape`, with a fractional position and length in frames (`ParamSegment::{new, foreign, resumed}`) |
  | — | `tutti_graph::SegmentShape::{Linear, Hold, PowerIn(p), PowerOut(p), PowerInOut(p), Exp(k)}` and `SegmentShape::eval` (a closed, `Copy` set; `Event` stays 32 bytes) |
  | `AutomationLaneNode` had an audio output only | `AutomationLaneNode::{for_param, for_foreign}`: the lane sends its curve as segments on an event output, one per breakpoint (and at each block edge, loop wrap and transport change); `tutti_nodes::automation::{segment_shape, LANE_EVENT_CAPACITY}` |
  | — | `ParamSegment::{raw_value_after(Samples), arrives_after}`: its value `j` frames after its event, and the frame it lands its end value on, for a node that takes point values. A length that is not finite and above zero is a step; a negative position is a delayed start |
  | — | `tutti_mod::{CurveSegment, Curve::segment_at}` (default `None`: a producer samples `value_at`); `AutomationEnvelope` answers it from its breakpoints |
  | `ParamRamp` stored its duration as `usize` | stored as `u32`, so an `Event` stays 32 bytes: `ParamRamp::duration()` still returns `Samples`, but a duration past `u32::MAX` frames (about a day at 48 kHz) now reads back saturated at `u32::MAX`. The executor already counted a ramp in `u32` while the reference interpreter used the full `usize`; both now see the same saturated length, so they agree on such a ramp |

  **Migration.** A `match` on `EventKind` needs a `Segment(_)` arm. A node
  that reads param automation from its own event input (rather than
  through a declared param port) reads a segment's value at a frame with
  `ParamSegment::raw_value_after(j)`. The hosted plugin node takes points,
  so it follows a segment on its event input itself: points along it in
  every chunk (the `PluginAutomation` stride, at most 10 a chunk) and its
  end value on the frame it arrives (`arrives_after`); a later ramp or
  segment on the parameter stops it. It follows a segment on every
  parameter the plugin has (slots allocated in `prepare`), each landing on
  its own frame (see "Plugin parameters are delivered as changes only"
  below for the chunk's room). `Editor::schedule` refuses a segment into
  a node that does not honour offsets sample-accurately, as it refuses a
  ramp.

### Design 015 PR 2: plugin, SoundFont and mod-source pieces to their owners

Engine logic that lived in `bevy-tutti` moves into the one crate that owns
it, so a headless host gets it without Bevy (design doc 015, §9 "PR 2").
`bevy-tutti`'s systems call the moved code. **Breaking** where noted.

| Was | Now |
|---|---|
| `bevy_tutti::plugin_host::PluginParamValues` (a `bevy-tutti` component) | `tutti_plugin::PluginParamValues`, a `Component` behind `tutti-plugin`'s new `bevy` feature (which `bevy-tutti`'s `plugin` enables); still re-exported from `bevy_tutti::plugin_host` |
| `PluginParamValues::read(&handle)` spawned the read on `AsyncComputeTaskPool` | `PluginParamValues::start(&handle) -> (PluginParamValues, ParamRead)`: installs the report callback; `ParamRead` is a `Send` job (`run() -> HashMap<u32, f32>`), and `land(values)` fills the mirror. Nothing in `tutti-plugin` spawns. Bevy: `bevy_tutti::plugin_host::read_plugin_params(&handle)` returns the mirror and a `PluginParamRead` (the task) to insert together |
| the mirror's edits were applied by `plugin_param_edits` reaching its fields | `PluginParamValues::drain_edits(apply)`: applies the reported values once the read has landed, each into the mirror and then to `apply` |
| `plugin_health_poll` declared a plugin dead on its 3rd consecutive unhealthy frame | `tutti_plugin::{Liveness, Verdict}`: `Liveness::observe(healthy, now) -> Verdict`, dead after at least `Liveness::DEATHS` (3) consecutive unhealthy observations spanning at least a window (`Liveness::DEFAULT_WINDOW`, 50 ms; `Liveness::with_window`), reset by a healthy one, latched once dead. `plugin_health_poll` feeds it the wall clock: a crashed plugin is now written off on the first unhealthy frame at least 50 ms after the first (the 4th at a steady 60 Hz, later under jitter), not on the 3rd, and back-to-back frames no longer shrink the window |
| `needs_announcement` private to `bevy-tutti`'s render-mode system | `tutti_plugin::needs_announcement(mode_changed, told)` |
| `bevy-tutti` built a SoundFont unit with `SoundFontUnit::new` at its own `i32` rate cast, then `program_change` on promotion | `SoundFontUnit::with_preset(font, rate: SampleRate, channel, preset)`; the off-thread build now applies the preset, and `PendingSoundFontUnit` carries only the task |
| `bevy_tutti::modulation::{Lfo, LfoRate, Macro}` defined in `bevy-tutti` | `tutti_mod::{Lfo, LfoRate, Macro}` (same builders), still re-exported from `bevy_tutti::modulation`; each converts with `Source::from` |
| `bevy_tutti::modulation::Follower { input, attack, release }` | `Follower { input, envelope: tutti_mod::Follower }`; `Follower::new(input)`, `.attack(..)`, `.release(..)` unchanged. `tutti_mod::Follower` (attack and release, default 5 ms / 100 ms) is the source half |
| `bevy_tutti::modulation::NoteEnvelope { midi, port, adsr }` | `NoteEnvelope { midi, port, envelope: tutti_mod::NoteEnvelope }`; `NoteEnvelope::new(midi, adsr)` unchanged. `tutti_mod::NoteEnvelope { adsr }` is the source half |

**Migration.** Code that named `bevy_tutti::modulation::{Lfo, LfoRate,
Macro}` or `bevy_tutti::plugin_host::PluginParamValues` keeps compiling. A
struct literal or field access on `Follower`'s `attack`/`release` or
`NoteEnvelope`'s `adsr` goes through `envelope` (`f.envelope.attack`,
`n.envelope.adsr`), or uses the constructors. A headless host reads a
plugin's values with `let (mut mirror, read) = PluginParamValues::start(&handle);`,
runs `read.run()` on a thread of its own, and calls `mirror.land(values)`;
it debounces a plugin's death with one `Liveness` per plugin, observing
`handle.status()` with its own `Instant`. A test that drove
`plugin_health_poll` with back-to-back frames to a death spaces them over
the window (or asserts on `Liveness` with explicit times).

### Changed

- **The old modulation system is deleted** (design doc 014, PR 5).
  Control lanes are the one modulation path: a param port has at most one
  source, an event output, and the fused step is `clamp(held, range)`;
  `tutti-mod` is the pure compiler and holds no runtime. Audio-rate
  modulation through a param port goes (FM, ring and the like are a node's
  own audio input, doc 014 §11; `LfoNode` stays an audio generator for it).
  The fork seeds go with `ParamTap`: a fresh (forked) source sends its
  current value on its first block, which a control lane does. **Breaking**
  for every user of the items below.

  | Was | Now |
  |---|---|
  | `tutti_mod`'s `routing` feature: `ModMatrix`, `AtomicTarget`, `ModRouter`, `ModBus`, `ModEdge`, `ModRoutingTable`, `ModRoutingSnapshot`, `ModPreFrame`, `ErasedModulator`, `Sourced`, `SourceRate`, `Rate`, `SourceClock`, `Route`, `RouteTo`, `TargetHandle`, `LayeredCurve`, `ShapedCurve`, `EdgeShape`, `CurveModulator`, `BeatLfo` | declare sources, routes and targets and call `tutti_mod::compile` (headless: `tutti_nodes::Modulation` applies the result to an `Editor`), or declare them in Bevy (`bevy_tutti::modulation`'s `Mods`, `ModRoute`, `ModRange`); a `ControlLaneNode` per target runs its program |
  | `tutti_mod::{ModParams, ModTarget, LayerKey, ModTargetId, fold}`; the `std` and `routing` features (`bevy` implied `routing`) | gone: a node's params are its `ParamSet`, a lane's knob one of its cells; `tutti-mod` has `default = []` and `bevy` |
  | `tutti_mod::{Lfo, Modulator, RandomState, SampleHold}` | `tutti_nodes::{Lfo, Modulator, RandomState, SampleHold}` (the copies the nodes run since PR 3) |
  | `spec.connect_param(at, ParamFrom::Events(out), ParamShaping::Identity)` | `spec.connect_param(at, out)`, which replaces the port's source and returns the one it had; `disconnect_param(at, out)` |
  | `ParamFrom` (`Audio`, `Events`), `ParamShaping`, `ShapeLut`, `SHAPE_LUT_LEN`, `ParamSource`, `MAX_PARAM_SOURCES`, `GraphInvalid::{TooManyParamSources, UnsortedParamSources}`, `DelayKey::ParamAudio`; `ParamMod { range, sources }` | `ParamMod { range, source: Option<EventOut> }`; a port with no source reads its base under the range |
  | `Plan::param_sources()`, `ParamSourceOp`, `ParamSlot`; `ParamPortOp { sig, sources, .. }` | `ParamPortOp { port, param, from: EventOut, slot }`, the port's one source |
  | `Editor::param_seeds`, `seed_params`, a fork's param seeds | none: a fork's fresh source sends its value on its first block (doc 014 §10). **A hand-wired event source must too**, and one driven by live input it cannot replay (a MIDI CC or OSC bridge) either writes the target's `Param` cell (the knob, which a fork reads) or answers `EventKind::Resync` by re-sending where it stands |
  | re-wiring a steady source to a port | a new source starts at rest: a control lane whose knob is still must be asked to resend once wired (`LaneControls::resync`) |
  | `ParamSet::set_authored(p, v)`, `ParamSet::authored(p)` | `ParamSet::set(p, v)`, `ParamSet::get(p)`: the knob is the node's one cell, which a fork starts from |
  | `tutti_nodes::{param_mod, ParamModShaping}`; `tutti_nodes::{AtomicTarget, BeatLfo, CurveModulator, LayerKey, LayeredCurve, ModParams, ModTarget}` | gone; `tutti_mod::shape` is the one shaping function |
  | `AutomationLaneNode::for_param(key, curve)`, `AutomationLaneNode::for_foreign(curve, id)` | `ControlLaneNode::for_param(key, ControlProgram::from_automation(curve), knob, range)`, `ControlLaneNode::for_foreign(id, program, knob, (min, max))` |
  | `tutti_plugin`: `PluginControls::automation(params)` (and `PluginClient::automation`, `Plugin::automation`, the in-process VST2 client's), `PluginAutomation`, `AutomationControls`, `AUTOMATION_EVENT_CAPACITY`, `TimedParam`, `PluginParamTarget`, `LfoCurve`, `LfoOffset`, `OffsetCurve`, `stride_for`, `MAX_POINTS`, `PluginControls::param_target`, `impl ModParams for PluginClient`, `handles::{LfoShape, ModParams, ModTarget}` | a `ControlLaneNode::for_foreign(id, program, knob, (0.0, 1.0))` per parameter, event-wired into the plugin node's event input 0; the plugin reads the lane's `u32` in its own model (a VST2 index, or an opaque id). An id of the other model is no longer refused: a lane carries a bare `u32` |
  | `bevy_tutti::modulation::legacy` (the frame-rate driver: `ModulationMatrix`, `ModTargetRegistry`, `ModTargetResolver`, `ModBusRes`, `ModParamsHandle`, `ModRateCell`, `CollectedModSources`, `ModSourceSystems`, `ModSourceAppExt`, `AudioRateRoutes`, `ModDelivery`, `ModParamRange`, `ModClock`, `ParamKey`, `drive`, `rebuild`), `LegacyModulationPlugin` | `TuttiModulationPlugin` and its declarations (`Mods`, `ModSource`, `ModRoute`, `ModRange`, `Automation`, `ParamRef`) |
  | `bevy_tutti::plugin_host::{plugin_bind_params, PluginParamsBound, PluginAutomationNode, remove_plugin_automation}` | a lane on the plugin's event input: `ParamRef::plugin(entity, id)` |
  | `CapturedControls::for_params`; `param_graph_node!` and the sampler's nodes capturing their `ParamSet` | gone: a node's params are addressed by `GraphNode::params` at insert, and `captured` is a hosted plugin's only |
  | `AudioGraphRes::set_param_mod(node, param, &[(AudioNode, ParamShaping)], range)` | `set_param_mod(node, param, source: Option<AudioNode>, range)`: the source node's event output 0, or none |
  | a param a node addresses in its `ParamSet` but did not declare as a port, which the frame-rate driver modulated by writing its cell (a lane needs the port, so a route onto one would be refused with `NoSuchParam`) | declared as ports, each read once per block (a driven port's last frame) where the node reads its cell: new `MOD_DELAY_PARAMS` (chorus and flanger rate, depth, feedback, mix), `PHASER_PARAMS` (the same four), `CONVOLVER_PARAMS` (mix), `tutti_polysynth::SYNTH_PARAMS` (volume; detune and spread with a unison engine), `tutti_sampler::VOICE_PARAMS` (a `VoiceNode`'s gain, and a standalone `MemorySource`'s), `LFO_PARAMS` (depth; rate while free-running); and widened: `COMPRESSOR_PARAMS` (threshold, now also ratio, attack, release and makeup `GainDb`), `GATE_PARAMS` (threshold, now also attack and release), `LIMITER_PARAMS` (ceiling and threshold, now also release), `DELAY_PARAMS` (feedback and delay time, now also mix), `SVF_PARAMS` (cutoff and Q, now also gain). **Breaking** for code that matches a const's length or a shape's param list |
  | `BusStripNode`'s `Mute`, modulated by the driver like any cell | knob-only: a toggle, not a continuous control, and the strip folds it in unmodulated. A route onto it is refused with `NoSuchParam`; write it with `ParamSet::set` or `set_muted` |

- **`bevy-tutti` modulation runs on control lanes** (design doc 014, PR
  4b). Sources, routes, ranges and automation are declarations on
  entities; `TuttiModulationPlugin` applies them every frame through
  `tutti_nodes::Modulation`, so each modulated param gets one control lane
  (sample-accurate on a 32-frame grid) and each follower or note envelope
  a node, all graph nodes an export forks: an export renders the
  modulation live playback plays, on native params and on hosted plugins'
  parameters alike (on `main` a plugin's modulated gain exported at its
  authored value). A depth, range, curve or automation edit is a store,
  never a new plan, and an LFO keeps its phase across one. **Breaking**
  for every user of `bevy_tutti::modulation`; the old frame-rate driver was
  kept, unscheduled, in `modulation::legacy` until PR 5 deleted it (above).

  | Was | Now |
  |---|---|
  | `ModSource::new(LfoShape::Sine)` plus `ModSourceRate::beat_synced(b)` / `free_running(hz)` | `ModSource::new(Lfo::sine().rate(BeatDuration(b)))` / `.hz(Hz(f))` (`Lfo::{new, sine, triangle, square, saw}`, `.phase(..)`); also `Macro(value)`, `Follower::new(input_entity)`, `NoteEnvelope::new(midi_entity, Adsr)`, or a host's own `ModSourceKind` (`declare(&self) -> tutti_mod::Source`, `feed`) |
  | `ModRoute::new(source, entity, ParamAddr::Unit(p))` | `ModRoute::new(source, ParamRef::unit(entity, p))`; a plugin parameter is `ParamRef::plugin(entity, id)`; `with_depth`, `with_polarity`, `with_curve` and `enabled` as before |
  | `ModRoute::per_block()`, `per_sample()`, `deliver(ModDelivery)` | gone: every route is on the param's lane |
  | `ModParamRange::default().with(param, base, min, max)` on the node's entity | `ModRange::new(ParamRef, min, max)` on an entity of its own; no base: the base is the param's knob (its `Param` cell, written by `AudioParam` and `write_param`; a plugin parameter's lane knob, started at the plugin's value) or its `Automation` |
  | — | `Mods` (a `SystemParam`: `lfo`, `macro_`, `follower`, `note_envelope`, `source`, `route`, `range`, `automate`, each spawning an entity and returning it); `Automation::new(ParamRef, Arc<dyn Curve>)`; `AutomationTake` (a touch, latch or write take on an `Automation`'s entity: the knob plays while it is open, its envelope becomes the curve when it closes) |
  | `ModulationMatrix`, `ModTargetRegistry`, `ModClock`, `ModBusRes`, `drive`/`rebuild` | `ModulationState` (`lane(ParamRef)` with its `LaneControls`, `set_knob`, `source_node`, `refusal`, `unresolved_targets`, `unresolved_sources`) and `ModulationSystems::{Apply, AfterCommit}`; a refused declaration is a `ModulationRefused` message (`route()`, `target()`) and stays in `ModulationState::refusal` |
  | `write_param(graph, matrix, entity, node, param, value)` | `write_param(graph, node, param, value)`: one writer per knob, the node's cell, modulated or not |
  | `ParamSet::set_authored(param, value)` | gone: `ParamSet::set` writes the live cell and the authored value together |
  | a plugin parameter was modulated per block through `plugin_bind_params` (`PluginControls::automation`), which an export did not fork | a lane on the plugin's event input; `PluginParamValues` (on a loaded plugin's entity) mirrors its values and seeds the knob; an edit in the plugin's GUI moves the knob (`plugin_param_edits`; VST2 only for now, since CLAP, VST3 and AU send no per-value report through the plugin-server). `plugin_bind_params` is no longer scheduled |
  | — | a second `ModRange` (or `Automation`) on one `ParamRef` is refused, `ModulationRefused` with `CompileError::DuplicateTarget`, never merged |
  | the old types at `bevy_tutti::modulation::*` | at `bevy_tutti::modulation::legacy::*`, with `LegacyModulationPlugin` to schedule them |
  | `AudioGraphRes::contains(node)` saw only nodes inserted through it | every node in the graph's spec, a modulation lane or signal node included |
  | `AudioGraphRes::event_sources(sink, port)` listed only sources inserted through it | every edge on the port, a plugin parameter's lane included |
  | the prelude's `ModClock`, `ModDelivery`, `ModParamRange`, `ModSourceRate`, `ModTargetRegistry`, `ModulationMatrix` | `Automation`, `Lfo`, `ModRange`, `Mods`, `ModulationRefused`, `ModulationState`, `ParamRef` (with `ModRoute`, `ModSource`, `TuttiModulationPlugin`) |

  **Migration.** Declare a range for every routed param
  (`ModRange::new(param, min, max)`), and move each route's base into the
  knob: write it with `AudioParam`/`write_param` (a plugin parameter's
  knob is its own value). **Depth is span-scaled everywhere:** a route's
  depth is a fraction of `max − min`, as a per-block route's was, so an
  old `PerSample` route is `max − min` times stronger; divide its depth
  by the span. `per_block()`/`per_sample()` calls go. A plugin parameter
  declared in `ModParamRange` on the plugin's entity becomes
  `ModRange::new(ParamRef::plugin(entity, id), min, max)`. A host that
  wired a sink with `ModTargetRegistry::insert_target` names a
  `ParamRef` instead. `TuttiModulationPlugin` must still be added
  explicitly, after `TuttiPlugin`.

- **The modulation reconcile, Bevy-free** (design doc 014, PR 4a):
  `tutti_nodes::Modulation` applies declared sources, routes and targets
  to a plain `Editor`. It compiles them against the last compile, builds a
  control lane per target and a node per signal source with their edges,
  republishes a changed program without a commit, stores a range edit,
  removes what is no longer declared, refuses a follower downstream of its
  own target by route, and resyncs a note envelope a lane newly reads.
  **Breaking** for code that matches `EventKind` exhaustively (a new
  variant).

  | Was | Now |
  |---|---|
  | a host wired lanes, signal sources and their edges itself | `tutti_nodes::Modulation::{new, apply(&Declarations, &mut Editor, &dyn ModHost) -> Result<Applied, ReconcileError>, commit, send_resyncs, lane, source, compiled, resyncs_owed}`; a refused `apply` changes nothing |
  | — | `tutti_nodes::ModHost` (the host's seam: `target(TargetKey) -> Option<Target>`, `feed(SourceKey) -> Option<Feed>`), `Target::{Native { node, knob }, Foreign { node, port, knob }}`, `Feed::{Audio(NodeKey), Events(EventOut)}` |
  | — | `tutti_nodes::Applied { commit, republished, unresolved_targets, unresolved_sources, resync }`; `tutti_nodes::ReconcileError::{Compile, Cycle { route }, WrongTarget, NoSuchParam, NoSuchEventInput, PortTaken, SharedPort { first, second }, WrongFeed}`; `Modulation::resyncs_dropped`. A native target resolves once the last committed plan has a unit at its node declaring the params it declares now (a node new since, or replaced with another param list, waits for its commit): a host re-applies after each commit while `unresolved_targets` is not empty |
  | a note envelope newly read by a lane read 0 until its next note or stage boundary | `tutti_graph::EventKind::Resync` and `Event::resync(offset)`: a request to an event source to re-send where it stands. `NoteEnvelopeNode` answers it on its MIDI input (a resumed segment, or 0 when idle); `Modulation::commit` schedules one into each envelope a lane newly reads. Param ports, the plugin nodes and the other nodes ignore it |
  | a lane on a built-in param needed a typed `ParamKey<U>` | `tutti_graph::ParamSegment::unit(UnitParam, from, to, duration, shape)` and `ControlLaneNode::for_unit(UnitParam, program, knob, range)`, which also reach params with no `ParamKey` (`Pan`, `RoomSize`, …); `ControlLaneNode::for_param::<U>` is a wrapper over `for_unit` (its `'static` bound is gone) and `ParamSegment::new` over `unit`, bit for bit |
  | a follower rebuilt for the same `SourceId` stayed silent until its input was not | `EnvelopeFollowerNode::replacing()`: sends its first grid piece even while silent, so the lanes reading it drop the old node's last value |

  **Migration.** A `match` on `EventKind` needs a `Resync` arm (a node
  that sends nothing a sink could miss ignores it). A headless host
  implements `ModHost` and calls `Modulation::apply`, then
  `Modulation::commit` when `Applied::commit` or `Applied::resync` says
  so, re-applying after a commit while `Applied::unresolved_targets` is
  not empty (`tutti-nodes/examples/headless_modulation.rs`; design 015
  PR 5 moved it to `tutti-session/examples`, where a session does this).

- **Signal-driven modulation sources** (design doc 014, PR 3b): an
  envelope follower and a note envelope, graph nodes that send a raw value
  in `[0, 1]` into the control lanes reading their `SourceId`, built from
  the compiler's `SourceNode` descriptions. Both reset on the frame the
  play clock restarts, so an export from beat X is the live render from
  beat X, and both are inserted `ForkByClone`. Doc 014 §7's stale-slot
  hazard is closed: a source id freed by a compile no longer brings a
  removed source's last value back. **Breaking** only for code that builds
  `tutti_mod::Compiled` with a struct literal (it has a new field), or that
  relied on a new source taking the lowest free id.

  | Was | Now |
  |---|---|
  | — | `tutti_nodes::EnvelopeFollowerNode::new(source, attack, release, input: ChannelLayout)`: the peak over its input's channels through a one-pole attack/release (in `f64`, so a long time constant decays at its own rate), sent as linear pieces on the play-clock grid (`CONTROL_STEP`), one step late; `Cost::Moderate`, `FOLLOWER_EVENT_CAPACITY` (256) |
  | — | `tutti_nodes::NoteEnvelopeNode::new(source, adsr)`: an ADSR on one MIDI event input (MIDI 1.0 or 2.0 notes), its stages as exact linear segments at note-on, note-off and each stage boundary; mono, last-note priority, retrigger from the current level; `Cost::Light`, `NOTE_ENVELOPE_EVENT_CAPACITY` (256) |
  | the host built a signal source's node from `Compiled::nodes` itself | `EnvelopeFollowerNode::from_decl(&SourceNode, ChannelLayout)` and `NoteEnvelopeNode::from_decl(&SourceNode)`, each `None` for the other kind; `source()` on both |
  | a new signal source took the lowest free `SourceId`, so a later compile could give a removed source's id to a new one | ids are never given twice: `Compiled::next_source` (carried from `prev`) is the next one; after 2^32 the counter wraps, skipping ids still held. Migration: a struct literal of `Compiled` adds `next_source: SourceId::default()` (or carries the previous compile's) |
  | a control lane kept a removed source's last segment in its input slot until another source needed the slot | a lane frees every input slot its newly adopted program does not read, so a source reading a reused id starts from 0, not the removed source's last value |
  | — | `tutti_mod::SourceId` derives `Default` (id 0) |

- **`tutti-mod` as a pure compiler** (design doc 014, PR 3): `compile`
  turns declared sources, routes and targets into a `ControlProgram` per
  target, a `SourceNode` per signal-driven source and the event edges
  between them. It is a pure function with stable layer ids, rate-edit
  anchors and refusals by name, and it needs no runtime. Nothing calls it
  yet: `bevy-tutti` moves to it in PR 4, and the follower and
  note-envelope nodes it describes come in PR 3b. **Breaking** for code
  that builds a `Layer` with a `depth_by`, matches `ProgramError`
  exhaustively, or relies on `tutti_nodes::{Lfo, Modulator}` being
  `tutti-mod`'s types.

  | Was | Now |
  |---|---|
  | — | `tutti_mod::compile(&Declarations, prev: Option<&Compiled>, now: PlayReading, rate: SampleRate) -> Result<Compiled, CompileError>`, with `Declarations { sources, routes, targets }`, `Source::{Lfo, Macro, Follower, NoteEnvelope}`, `RouteDecl { source, to: RouteTarget::{Param, Depth}, depth, polarity, curve }`, `TargetDecl { min, max, automation }`, the keys `SourceKey`, `RouteKey`, `TargetKey { node, param }`, `Adsr` (sustain is an `Amplitude`), `Compiled { programs, nodes, edges, clocks }` (`clocks`: each LFO's dated clock, so a rate anchor is the source's and one LFO stays phase-coherent across its targets), `SourceNode { id, kind: SignalKind }`, `layer_id(route, depth_by)` and `RATE_ANCHOR_MARGIN` (12 288 frames). `CompileError` has a variant per refusal (doc 014 §7); a route's depth and a constant wave must be finite. A signal source keeps its `SourceId`; an id freed by a compile is not reused in that compile |
  | `Layer::depth_by: Option<(Clock, Wave)>` | `Option<DepthBy>`, where `DepthBy { clock, wave, amount }` carries the depth route's own `Amount`. Migration: `Some((clock, wave))` becomes `Some(DepthBy { clock, wave, amount: DepthBy::UNIT })`, which renders bit for bit as before for a depth wave's raw value in [-1, 1]. A drawn depth wave beyond ±1 is now clamped to ±1 before it scales (it scaled past full depth before). A curved or bipolar amount adds its zero-crossing corners |
  | — | `Wave::Const(f32)`: a constant raw value, with no corners and no clock. A lane whose layers are all constant, with no automation, is steady (a macro costs nothing per block) |
  | `ControlProgram::new` accepted a NaN in a layer or an input's depth | it returns `ProgramError::NotANumber(layer)`, so `ControlProgram` is `Eq`. Add an arm if you match `ProgramError` exhaustively |
  | `tutti_graph::PlayReading` | defined in `tutti_types::PlayReading`, so the compiler names no graph; `tutti_graph::PlayReading` re-exports it, so existing paths still compile |
  | `ParamAddr` was `Eq + Hash` only | it also derives `PartialOrd + Ord` (every `Unit` before every `Id`), so `TargetKey` can be a map key |
  | `tutti_nodes::{Lfo, Modulator}` re-exported `tutti-mod`'s | they are `tutti-nodes`' own copies, used by `ModulatorNode` and `LfoNode`, and `tutti_nodes::{SampleHold, RandomState}` are newly exported. Migration: code that hands a `tutti_nodes::Lfo` to `tutti-mod`'s routing (anything bounded on `tutti_mod::Modulator`, such as `Sourced`) names `tutti_mod::Lfo` instead. `tutti-mod`'s copies go with `routing` in PR 5 |

- **The control lane** (design doc 014, PR 2b): one node per target that
  evaluates the target's `ControlProgram` (automation or the knob, plus
  every modulation offset) on a 32-frame grid of the play clock and every
  corner, and sends it as segments, equal live and in an export, serial
  and parallel. Plugin parameter delivery makes room for it. **Breaking**
  for `AutomationLaneNode::for_param` / `for_foreign` callers (they get a
  `ControlLaneNode`), code naming `LANE_EVENT_CAPACITY` (now 256, in
  `ControlLaneNode`'s module), and a plugin host's memory (below).

  | Was | Now |
  |---|---|
  | — | `tutti_mod::ControlProgram` (automation, `Layer`s on a `Clock::{Beats, Seconds}` with an optional `RateAnchor`, `InputLayer`s; `Wave`, `Amount`, `PeriodPoint`, `ProgramTime`, `Corners`, `ProgramError`, `MAX_LANE_LAYERS` = 16, `MAX_LANE_INPUTS` = 8). `Curve` and `CurveSegment` no longer need the `routing` feature |
  | — | `tutti_nodes::automation::{ControlLaneNode, LaneControls, CONTROL_STEP}`: `ControlLaneNode::for_param(key, program, knob, range)` and `for_foreign(id, program, knob, (min, max))`; `LaneControls::{publish, publish_arc, program, resync, set_knob, knob, set_range, range, thinned, truncated, unranged}`. Declared `Cost::Moderate` (measured) |
  | `AutomationLaneNode::for_param(curve, key)` / `for_foreign(curve, id)` returned an `AutomationLaneNode` with an event output | they return a `ControlLaneNode` of the curve alone; `AutomationLaneNode` is the audio-output lane only |
  | `LANE_EVENT_CAPACITY` = 128 (`AutomationLaneNode`'s) | 256, the control lane's: a 4096-frame block's grid and as many corners; past it a lane thins its grid (`thinned()`), then drops its last points (`truncated()`) |
  | a piece no `SegmentShape` carries (`Elastic`, a curve without pieces) was sampled at block edges, so it moved with the block size | it is linear between grid points of the play clock and the breakpoints: the same frames at any block size |
  | `ParameterQueue::chunk_room(chunk)` = `max(ceil(chunk / 32) + 4, 10)`; a full queue replaced its last point (`points_merged`) | `ceil(chunk / 32) + 1 + CORNER_POINTS` (256). A queue is full at its room, or at its floor with the chunk's pool spent (more than 256 parameters past their floor in one chunk); then a grid point (a frame whose play count is a multiple of 32) is dropped, or a corner evicts the queue's earliest grid point, both counted in `ParamDeliveryStats::grid_points_dropped`; only a queue of corners keeps its latest value in place of its latest point (`points_merged`) |
  | `ParameterQueue::MIN_POINTS` (10), the floor of `chunk_room` | removed: `chunk_room` is never under 258 for a chunk of at least one frame (`chunk_room(0)` is 257). Use a literal room where a test wanted a small one |
  | — | `PointBuf::remove(index)` and `PointBuf::sort_by_offset()`, a stable in-place sort that never allocates (std's stable sort allocates past 256 points, under a chunk's room); the VST3 host's `IMidiMapping` merge and its input queue refill sort with it |
  | `SegmentPoints` sent a point every stride frames of each chunk | it sends a segment's start, its value on each 32-frame play-grid frame, each chunk's last frame and its arrival; the lane's pieces are the points |
  | a plugin node's and plugin-server's parameter buffers reserved each queue's room of 132 points at a 4096-frame chunk (20 at 512) | the logical room is 385 (273 at 512); the storage stays the shared pool (below): a floor of 3 a queue and 256 blocks of `room + 1`, so only the pool term grows. Per instance at 2000 parameters (the node's buffers, its host's lists and the plugin-server's output together): a CLAP plugin about 34.2 MB at 4096 frames and 25.2 MB at 512 (14.0 MB and 5.0 MB at the old room), a VST3 plugin about 19.8 MB and 14.8 MB (8.4 MB and 3.35 MB). These totals first left out the server's output (1.81 / 1.35 MB). Doc 014 §9 lists every figure; with every queue at the full room the node's buffers and the CLAP lists alone would have been about 235 MB and 168 MB. The bridge's pool now drops a returning buffer of another point room as well as of fewer queues (`PayloadPool::return_params`), so a re-prepare never hands the node a buffer laid out for another chunk |

  Migration:
  - Wire `AutomationLaneNode::for_param(curve, key)`'s result as before: it is a `ControlLaneNode`, inserted the same way. For a knob under the automation, or modulation, build `ControlLaneNode::for_param` with a `ControlProgram` and the target's `Param` cell and `Editor::param_range_cell`.
  - Read `LaneControls::thinned()` / `truncated()` where a block over 4096 frames may carry more than 256 points; render exports in blocks of at most 4096 frames.
  - Replace `ParameterQueue::MIN_POINTS` with the room the code meant (it was 10).
  - Expect a CLAP parameter under automation to step every 32 frames or less, at play-clock grid frames.
  - `AutomationLaneNode::for_param` / `for_foreign` now freeze the curve when they build (`ControlProgram::from_automation` takes `Curve::frozen`). A curve that reads state written later (a `LayeredCurve`, whose layers a router writes) no longer sees those writes: publish a new program through the lane's `LaneControls` when it changes, or express the modulation as program layers.
  - After re-wiring a lane's output or replacing its target, call `LaneControls::resync()`: a steady lane does not re-send at block edges, so a new target hears nothing until something moves.
- **`tutti_io::MicRing` is a plain consumer, not a shared one.** It owns
  its ring's `HeapCons` instead of an `Arc<AudioThreadCell<_>>`, and is no
  longer `Clone` (nothing cloned it: the ring has one consumer, the
  `MicMonitorNode` that owns it). **Breaking** only for code that cloned a
  `MicRing`: hand the one ring to the node instead. Its constructor is
  `MicRing::new(consumer)`, which allocates nothing.

  | Was | Now |
  |---|---|
  | `tutti_io::share_mic_ring(consumer) -> MicRing` (allocated an `Arc`) | `MicRing::new(consumer)` |
  | `MicRing: Clone` | not `Clone`: one ring, one consumer |
- **The in-process VST2 node copies each block once each way, not twice.**
  The node's `ProcessScratch` staging is deleted; `process_f32` reads and
  writes the graph's own buffers through stack channel tables. No API
  change. **Fixed** with it: a plugin wider than the tables (sixteen
  channels) left the graph's outputs past them holding whatever their
  buffers last held (another node's audio); they are silent now.
- **tutti-plugin's bridge thread buffers unsolicited plugin events itself**
  (a preallocated buffer it owns) instead of in a lock-free queue shared
  with no other thread. Listener timing is unchanged: after each command.
  Unused dependencies dropped: `ringbuf` (tutti-plugin),
  `crossbeam-channel` and `parking_lot` (tutti-plugin-server).

  **Behaviour change: state events are never dropped.** The old queue held
  128 events of every kind and, when full, refused the incoming event and
  then dropped the oldest (two lost per overflow), so more than 128
  parameter changes inside one command (a `LoadState` or a preset load)
  could evict a latency, tail or resync event, and PDC never heard of the
  new latency. Now a latency, a tail and each resync kind is a latest-wins
  slot nothing evicts; only parameter events queue (128), the oldest is
  dropped when full, each drop is counted and raises a
  `Resync(ParamValues)` (delivered as `PluginRefresh::ParamValues`) so the
  host re-reads them all. Two latency reports in one command arrive as
  one, with the second value. A listener hears, per command:
  `Resync(Reloaded)` and `Resync(Io)` first, since a plugin that rebuilds
  itself or its buses and then reports parameters reports them against the
  new table; then the parameter events in arrival order; then the latency
  and the tail; then `Resync(ParamTitles)` and `Resync(ParamValues)` last,
  so a re-read (including the one a drop raises) wins over every queued
  value. A parameter event sent *before* a structural change in the same
  command is delivered after it; the `Reloaded` re-read covers it.

  | Was | Now |
  |---|---|
  | a flood of parameter events could evict latency, tail and resync events | they are latest-wins and never dropped; parameter events drop oldest-first, counted, with a `ParamValues` resync |
  | events in arrival order across kinds | `Reloaded`, `Io`, the parameter events in arrival order, latency, tail, `ParamTitles`, `ParamValues` |
  | — | `PluginClient::dropped_parameter_events()`, `PluginHandle::dropped_parameter_events()` |
  | `HostParams` had no drop count | **Breaking** for a custom backend: `HostParams::dropped_parameter_events(&self) -> u64` is required. A backend that cannot lose a plugin's parameter reports returns `0`; the in-process VST2 backend returns the `audioMasterAutomate` reports its bounded queue refused (`Vst2Instance::dropped_param_changes`) |

- **Plugin parameter changes draw their points from a shared pool, in
  fixed-capacity storage** (design doc 014, §9's point-storage note; §12's
  memory follow-up). A reserved `ParameterChanges` held every queue at its
  full room, so a plugin's parameter-change memory grew with
  `params × room`: about 43 MB in the IPC node's buffers and 40 MB in a
  CLAP instance's lists at 2000 parameters and a 4096-frame chunk, ten
  times that at 16k. Now each queue keeps a small floor and the set a pool
  of 256 full-room blocks (6.98 MB and 6.2 MB at that size; the per-instance
  figures are in doc 014 §9). A queue's points are a `PointBuf`, whose
  capacity is fixed when it is built, so no point storage on the parameter
  path can grow, on any thread. A queue's logical room
  (`ParameterQueue::chunk_room`), the wire bytes and the delivery semantics
  are unchanged. **Breaking**: `ParameterQueue::points` is a `PointBuf`,
  not a `Vec`; nothing grows a set or a queue past the capacity it was
  built with; `ParameterQueue::new` and `ParameterChanges::with_capacity`
  are removed; `ParameterChanges::new()` has room for nothing.

  | Was | Now |
  |---|---|
  | `ParameterQueue::points: Vec<ParameterPoint>` | `ParameterQueue::points: PointBuf` (`tutti_plugin_types::PointBuf`, re-exported where `ParameterQueue` is): a boxed slice and a length. It derefs to `[ParameterPoint]` (`len`, `iter`, indexing, `first`/`last`, `sort_by_key` and `&queue.points` in a `for` all compile as before); `try_push(p) -> Result<(), ParameterPoint>`, `try_push_front(p) -> Result<(), ParameterPoint>`, `clear`, `capacity`, `has_room`, `push_or_replace_last(p) -> bool`, `replace_latest(p) -> Option<usize>` (the slot it took). It serializes as the same sequence. `PointBuf` and `ParameterPoint` are `Default` |
  | `ParameterQueue::new(id)` (an empty `Vec`) | removed: `ParameterQueue::with_capacity(id, points)`, or `ParameterQueue::from_points(id, vec)` (exactly the given points, off the audio thread) |
  | `ParameterQueue::add_point(..)` grew the queue | `add_point(..) -> bool`: at its capacity the one full-queue policy, `PointBuf::push_or_replace_last` (the point replaces the queue's latest-in-time point if it is no earlier, so the queue ends on its latest value), and `false` for the caller to count |
  | `ParameterChanges::new()` grew as points were added; `with_capacity(queues)` reserved queues | `new()` is the empty set with room for nothing (`add_change` on it stores nothing, counted); `with_capacity` removed: build `ParameterChanges::reserved(queues, points)` |
  | `ParameterChanges::reserved(queues, points)`: every queue with room for `points` | one layout for every set: every queue with a floor of `ParameterChanges::FLOOR_POINTS + 1` (3), the set with a `PointPool` of `ParameterChanges::POOLED_QUEUES` (256) blocks of `points + 1`, where that at least halves the storage; otherwise every queue at `points + 1` (`ParameterChanges::point_layout(queues, points) -> (floor, blocks)`). The one more point is the bridge's carried value's, so a queue can reach `points + 1` |
  | — | `ParameterChanges::reserved_bridged(queues, room)`: `reserved`, with one point of every queue and block kept back for `prepend_carried`, so its producers stop at `room`; `prepend_carried(i, point) -> bool` (with no slot the value is dropped, counted: it is older than the queue's points; past the queues, where no queue holds the parameter, it is dropped and counted too, and is its latest value: unreachable from the IPC bridge, whose buffers hold a queue for every slot of the delivery); `push_point(i, offset, value) -> bool`; `ParameterChanges::SPARE_QUEUES` (64), the queues a host's chunk holds past the parameter list; `PointPool::new(blocks, room, holders)` |
  | a producer pushed while `queues[i].has_room()` | `ParameterChanges::make_room(i) -> bool`: `true` when the queue has room of its own or took a block from the pool |
  | — | `PluginInstance::parameter_count() -> usize`: the parameter count without building the descriptors; the default is `get_parameter_list().len()`, and the VST3, CLAP and VST2 loaders answer from the format's count query. An existing implementation needs no change |
  | `ParameterQueue::has_room()`: room in the queue's own storage | removed: a producer asks `ParameterChanges::make_room(i)` (the queue's room or a pool block), and a sizing check compares `point_storage()` with `point_budget`; the queue's own storage alone is `queue.points.has_room()` |
  | `add_change(..)` returned nothing, and grew any set past its room | `add_change(..) -> bool`, never grows: past the queue's room the one full-queue policy, and an address past every queue is dropped; either returns `false` and is counted in `ParameterChanges::points_overflowed()`. It costs O(queues) a call (a linear scan; no allocation) |
  | `add_change` on a set built `new` or `with_capacity` grew it (the unbounded path) | removed: there is no unbounded path. `add_queue` (off the audio thread) still pushes a prebuilt queue |
  | `point_capacity()`: the smallest room among the spare queues | removed: compare `point_storage()` (the points a set holds now, in its queues, spares and pool) with `ParameterChanges::point_budget(queues, points)` (the points `reserved` holds) |
  | the IPC bridge grew a floor queue by the carried value it prepends, and pushed a queue for a carried value with no spare queue | the node's delivery and the bridge's buffers are built `reserved_bridged`, so the value takes a real slot; with no spare queue it is dropped, counted in the buffer's `points_overflowed`, which loses the parameter's latest value but is unreachable: every buffer holds the delivery's slot count, the parameters + 64, and every carried id is one of those slots (only the empty placeholder of a block with no changes gets queues built). Their storage is `point_budget` (a floor of 3, not 2, and blocks of `room + 1`): 6.98 MB, not 6.65 MB, at 2000 parameters and 4096 frames |
  | `ProcessOutput::emit_param_point(..)` grew queues from a private pool of retired ones | `ProcessOutput::reserve_params(queues, points)` sizes `param_changes` (the plugin-server does at load, and again at that room, sized by `PluginInstance::parameter_count`, when a VST3 or AU plugin says its parameter list changed and it grew (a VST3 `kParamTitlesChanged` without a reload still keeps the instance's activation-time output queues); a CLAP `params.rescan` reaches no event there, so its output stays at the load's count, counted past it); `emit_param_point(..) -> bool` is `add_change` within that room. The private `retired` pool is removed |
  | the CLAP host's input list reserved and bounded at `params × (room + 1)` events | at `ParameterChanges::point_budget(params, room)`, so a chunk the host's pool bounded reaches the plugin whole. Past it, as before, a parameter's point replaces its previous event in the list, and a parameter with no event in the list yet when the room runs out loses its points; counted (`ClapActive::param_points_dropped`) |
  | the CLAP host's output list's parameter room `queues × room` | `ParameterChanges::point_budget(queues, room)`, the output queues the same (the one layout: a queue and a block hold `room + 1`); past it the one full-queue policy, counted (`ClapActive::output_param_points_dropped`) |
  | the VST3 host's input queues each reserved a full room; a queue past its room kept its first `room − 1` points and its last | the one layout, for the parameters and the 64 spare queues, lent from the same `PointPool` per block; past its room a queue keeps its latest value (the one policy) |
  | the VST3 host's output queues grew past their room as the plugin called `addPoint` | fixed at their room: past it `addPoint` keeps the latest value, counted: a point no earlier than the queue's latest replaces it and returns `kResultOk` with that slot's index; an earlier one is dropped and returns `kResultFalse` |
  | the VST3 host's input and output lists grew their queue count on the audio thread past the queues they were built with | fixed: a refill drops the queues past the list's, and `addParameterData` past them returns null (the failure the SDK's samples check for), both counted in `param_points_dropped` / `output_param_points_dropped`. `addParameterData` for an id already in the list returns that queue and its index, as the SDK's host does |
  | the VST3 `IMidiMapping` route's scratch in the plain layout, seeded through `add_change` (a scan of the open queues a point); its merge replaced the last point *added* | the one layout (`reserved`) for the parameters and the 64 spare queues, so the host's chunk reaches it whole, seeded a queue opened for each of the chunk's, linear in it; the merge keeps the latest value in time, host or controller |
  | — | `Vst3Active::param_points_dropped()`, `Vst3Active::output_param_points_dropped()`: read on the instance only; the plugin-server does not report them, as it does not report the CLAP host's |

  Migrating: build a queue with `ParameterQueue::with_capacity(id, n)` (or
  `from_points`) where it was `ParameterQueue::new(id)`, and a set with
  `ParameterChanges::reserved(queues, points)` where it was `new()` or
  `with_capacity(..)` and then filled. Code that pushed onto
  `queue.points` calls `try_push` (or `add_point`) and decides what a full
  queue means; code that read it needs no change. A producer that filled a
  reserved set with `has_room` checks calls `make_room(i)` in their place.
  A `ProcessOutput` the caller fills with `emit_param_point` is reserved
  once with `reserve_params`. Code that relied on a set or a queue growing
  past its room reserves the room it needs, or reads `points_overflowed`.

- **Sampler real-time fixes** (tutti-sampler; design doc 013, "Sampler
  real-time fixes"). A voice pool's drain no longer blocks on a stalled
  butler, no longer grows its voice vector in the callback, and counts
  what it drops instead of logging it. A standalone `VoiceNode`'s
  placement is a published value rather than a queue. **Breaking** for
  `VoiceNodeHandle::set_placement`, which returns `()`.

  | Was | Now |
  |---|---|
  | a streaming voice's loop forward blocked `process` while the butler's queue was full | it is refused, counted (`PoolDrops::loops_butler_full`), and not recorded in the voice's `Playback`; a host re-sends it. `Commands::send` (control thread) still blocks |
  | the drain's dropped loops and ignored `ReplaceWave`s went to `tracing::warn!` | `VoicePoolHandle::drops() -> PoolDrops` (`loops_butler_full`, `loops_butler_gone`, `loops_no_butler`, `wave_swaps_ignored`, `voices_refused`, `freed_in_callback`); `#[non_exhaustive]` |
  | an `AddVoice` past 64 resident voices reallocated the voice vector in the callback | it is refused, counted (`voices_refused`), and the voice handed back for `collect_retired` to free |
  | a value the full retirement channel could not take was freed in the callback silently | still freed there, now counted (`freed_in_callback`) |
  | `VoiceNodeHandle::set_placement(..) -> Result<(), SendError>` (a 64-entry queue, `Full` when it filled) | `-> ()`: the window is published, latest wins, from the node's next block. **Migration:** drop the `?`/`.unwrap()`; two placements between blocks play the second |
  | the butler's command queue was a `smol` channel, whose send locked a listener list (and allocated it on the first send) | a lock-free (not wait-free: on a full queue a send can spin briefly behind the butler's in-progress pop), allocation-free fixed queue whose send unparks the butler. A butler thread that exits, however it exits, closes it and drops what was queued, and every later `Commands::send` returns `ButlerGone`; a command sent as it exits either is popped (run, or dropped by that drain) or reports `ButlerGone` |
  | an `AddVoice` whose voice carried a loop recorded it in `Playback` even when the butler refused the forward | the loop is recorded only once the butler takes it |
  | a control-thread `insert_voice` of a streaming voice with a loop | waits for room in the butler's queue (retrying every 100 µs) instead of refusing, as `Commands::send` does; it returns early only when the butler is gone (counted as `loops_butler_gone`). It waits as long as a stalled butler stalls, and for good on an unstarted controller or a `DiskStreamer::manual` stepped on the calling thread: step or start it before inserting a looping voice into a full queue |
  | `loops_no_butler` counted every loop change on a streaming voice with no butler, `Off` included | it counts only a lost `On`; an `Off` there has nothing to clear |
  | `tutti-sampler` depended on `ringbuf`, `smol` and `futures-lite` | it depends on none of them (`crossbeam-queue` instead) |

- **A hardware MIDI input ring has one producer, by ownership**
  (tutti-midi-hardware; design doc 013, "Hardware MIDI input ring"). Each
  port's ring is split once: `create_input_port` returns its only
  `InputProducerHandle`, which owns the producer half, is not `Clone`, and
  pushes through `&mut self`; the port keeps the consumer. The `unsafe`
  `SpscRing` is deleted. A disconnect retires its port, and a new port takes
  the lowest free slot, so the port set no longer grows with every hot-plug
  cycle. A port is named by a `PortId` (its slot and a generation), so an
  id kept past its port's removal reaches nothing, not the port that took
  its slot. **Breaking:**

  | Was | Now |
  |---|---|
  | `HardwareMidiInputs::create_input_port(name) -> usize` | `-> InputProducerHandle`; the port is `handle.port_id()`, a `PortId` |
  | `HardwareMidiInputs::get_input_producer_handle(index)` | removed: keep the handle `create_input_port` returned. There is no second handle to a ring |
  | `HardwareMidiInputs::push_input_event(index, event)` | removed: `handle.push(event, Instant::now())` |
  | `InputProducerHandle: Clone`, `push(&self, ..)` | not `Clone`, `push(&mut self, ..)`: a driver callback owns its handle (`move` it into an `FnMut`, or the pump thread) |
  | ports were only appended; a `PortInfo::index` was never reused | `HardwareMidiInputs::remove_input_port(id: PortId) -> bool`; a removed port's slot is reused by the next port, under a new generation. `MidiSession` retires a port on disconnect, on a failed open, and when the session drops |
  | a port named by `usize` index: `PortInfo::index`, `get_port_info(type, index)`, `set_port_active(type, index, on)`, `is_port_active(type, index)` | by `PortId`: `PortInfo::id`, and each of those takes a `PortId`. A stale id (its port removed) is a no-op: `None` / `false`. `PortId::index()` is still the slot, for display |
  | `cycle_start_read_all_inputs[_at]`'s visitor: `FnMut(usize, MidiEvent)` | `FnMut(PortId, MidiEvent)`. A drain started from inside a visitor returns 0 and visits nothing; the outer drain still hands out every event |
  | `set_sample_rate` wrote an audio-thread cell (a concurrent drain could trip it) | an atomic, safe from any thread; a drain reads it once per block |
  | — | `HardwareMidiInputs::collect() -> usize`: frees a removed port whose ring a drain still held when it was removed, without waiting for the next port change; returns how many retired sets are still held |
  | `tutti_midi_hardware::crossbeam_channel` | removed (and the dependency): depend on `crossbeam-channel` directly |
  | `tutti_types::RtEventBuf` / `tutti_core::RtEventBuf` | removed (its one user was this crate): `AudioThreadCell<RtVec<T, N>>` |
  | `ClockNode` dropped events past `CLOCK_EVENT_CAPACITY` in a block silently | it still drops them, and counts them: `ClockMaster::dropped_events()` |

  `tutti-midi-runtime` no longer depends on `parking_lot` or `dashmap`
  (nothing used them).

- **VST3 host callbacks are bounded and allocation-free** (design doc 013,
  "VST3 host callbacks bounded"). Plugins call `performEdit` and
  `restartComponent` from their process thread, where the host's unbounded
  channels allocated; the plugin server's restart poll, in its audio phase,
  collected every queued edit into a `Vec` to keep only the restart flags.
  Restart flags are now one atomic word, and edits, progress and unit news
  share one preallocated queue whose overflow is counted, and a progress
  description is held inline, so no callback allocates. **Breaking** for
  `tutti-vst3-host` callers of the notification poll.

  | Was | Now |
  |---|---|
  | `Vst3Loaded::poll_plugin_notifications() -> PluginNotifications` (`param_edits`, `progress`, `units` as fresh `Vec`s, `restart`) | `poll_plugin_notifications(\|n: PluginNotification\| ..) -> RestartOutcome`: each event goes to the closure in arrival order across all three kinds; nothing is collected. `news.restart` → the return value; `news.param_edits` → match `PluginNotification::Edit(e)`, `news.progress` → `Progress(e)`, `news.units` → `Unit(e)` |
  | `PluginNotifications` | removed; `tutti_vst3_host::PluginNotification` (`Edit`, `Progress`, `Unit`) is the queued item |
  | `ParameterEditEvent::RestartComponent(i32)` (never surfaced: the poll folded it into `restart`) | removed; restart requests arrive only as the coalesced `RestartOutcome`, and carry no position among the events |
  | — | `Vst3Loaded::take_restart() -> RestartOutcome`: the restart flags alone, wait-free and allocation-free (the plugin server's `poll_restart` reads only this) |
  | unbounded queues | `NOTIFICATION_CAPACITY` (512) unread notifications; past it the newest is dropped, counted in `Vst3Loaded::dropped_notifications()`, and reported once by the next poll as `RestartOutcome::notifications_overflowed` (re-read every parameter value) |
  | `RestartOutcome` had only restart flags | new field `notifications_overflowed`; a struct literal needs it (or `..Default::default()`) |
  | `ProgressEvent::Started { description: String }` | `description: ProgressDescription`, inline, at most `PROGRESS_DESCRIPTION_CAPACITY` (128) UTF-8 bytes, cut at a character boundary. `description.as_str()`, or deref to `&str`; `.to_string()` for an owned copy |
  | — | `Vst3Loaded::discard_notifications()` (drop every queued notification, lock-free; the plugin server's `poll_restart` calls it) and `Vst3Loaded::pending_notifications()` |
  | `poll_editor_resize_request` drained a channel | the same method reads a latest-wins cell; behaviour unchanged (it returned the last size) |

- **`smallvec` is gone from the workspace** (design doc 013, "SmallVec
  removed"). A type whose inline storage bought nothing is a `Vec` (or a
  boxed slice); a type whose inline storage was a hard cap is an
  `arrayvec::ArrayVec`, which refuses a push past the cap instead of
  spilling to the heap. **Breaking** for code that names the aliases or the
  fields below. On the wire nothing changes: bincode writes each of these as
  a sequence (a `u64` length, then the items), whichever container, and
  tests pin those bytes.

  | Was | Now |
  |---|---|
  | `tutti_plugin_types::BusChannels` / `BusTopologies`: `SmallVec<[_; 4]>` | `Vec<ChannelLayout>` / `Vec<Option<ChannelTopology>>`. `BusChannels::from_slice(s)` → `s.to_vec()`; `smallvec![..]` → `vec![..]` |
  | `MidiEventVec` (`tutti_plugin_types`, re-exported by `tutti_plugin::protocol`): `SmallVec<[MidiEvent; 256]>`, spilling past 256 | one type, `ArrayVec<MidiEvent, MIDI_STACK_CAPACITY>` (256). `push`, `extend` and `collect` **panic** past the cap: use `try_push`, or `ProcessOutput::emit_midi` |
  | `tutti_plugin::protocol::IpcMidiEventVec`: `SmallVec<[IpcMidiEvent; 256]>` | `Vec<IpcMidiEvent>` |
  | — | `tutti_plugin::protocol::midi_from_wire(&[IpcMidiEvent]) -> MidiEventVec`: converts a wire list, keeping the first 256 events |
  | `NoteExpressionVec`: `SmallVec<[_; 8]>` | `Vec<NoteExpressionValue>` |
  | the harmony containers' fields (chords, scales, note-expression text and int changes): `SmallVec<[_; 4]>`, unbounded | `ArrayVec<_, HARMONY_CAPACITY>` (4, newly exported). `add_change` returns `bool` (`#[must_use]`): `false` when the container is full, and the change is not kept |
  | `ProcessOutput.midi_events` filled with `extend` | `ProcessOutput::emit_midi(events) -> usize` keeps what fits and returns how many were dropped; `ProcessOutput::midi_dropped()` totals them for the block |
  | `ProcessOutput.retired`: `SmallVec` | `Vec<ParameterQueue>` (private). **A server's first blocks allocate a little more:** the pool starts empty and a queue `emit_param_point` builds on a pool miss has no point room, so each parameter a plugin first emits allocates its pool slot and its points, where the inline rooms (16 queues, 10 points) did not. `queues` was already a `Vec`, so that first block allocated before too. Not reserved up front: the server's `AudioPipeline` is built before any plugin is loaded, knowing neither a parameter count nor a block size, and the pool is warm from the next block on |
  | `ParameterQueue::INLINE_POINTS` (10) | `ParameterQueue::MIN_POINTS` (10), the floor of `chunk_room`. `ParameterQueue::new` has **no** point room (its first point allocates): reserve with `with_capacity` or `ParameterChanges::reserved`. `ParameterChanges::point_capacity()` is 0 for a set with no spares |
  | `ParameterChanges::add_change` built a new queue for a new parameter | it takes a spare queue first, as `open_queue` does, so a reserved set cleared each block reuses its point storage |
  | `tutti_vst2_host::MidiEventVec`: `SmallVec<[MidiEvent; 256]>` | `Vec<MidiEvent>`, reserved once (the drain into it is gated against allocating) |
  | `tutti_vst3_host::ProcessOutput.midi_events`: `SmallVec<[MidiEvent; 64]>` | `Vec<MidiEvent>` |
  | `tutti_vst3_host::{ChordValue, ScaleValue, NoteExpressionText}::to_vst3_event(&mut SmallVec<[u16; 256]>)`: `pub` | `pub(crate)`. The text arena they intern into is now a crate-private type with a fixed room, so the methods can no longer be named outside the crate. Pass the values in `Vst3InputEvents` (`chords`, `scales`, `expr_texts`) to `process`, which interns them into its event list's own arena |
  | — | `Vst3Active::event_text_dropped()`: chord, scale and note-expression texts refused because a block's event-list text arena was full. The arena reserves its room (64 texts of 256 UTF-16 units) once and never grows, because the plugin holds pointers into it for the whole `process` call |
  | `tutti_types::ChannelTopology`'s positions: `SmallVec<[Speaker; 8]>` (private) | `Vec<Speaker>` (private) |
  | `tutti_types::Retire<T>` | removed. Nothing in production used it. A type that must not be freed on the audio thread calls `tutti_types::AudioThread::check_not_current("what")` in its `Drop` |

  **Decoding past a cap.** `ArrayVec`'s serde rejects a sequence longer than
  its capacity, where `SmallVec` accepted any length. The harmony containers
  decode with a truncating visitor instead: the first four items are kept
  and the rest are read and discarded, so the stream stays in step and one
  over-long list does not fail the whole `ProcessAudio` message. MIDI crosses
  the wire as `Vec<IpcMidiEvent>`, which decodes at any length, and is cut to
  the first 256 events when it is converted (`midi_from_wire`, on both the
  host and the server); the server logs how many plugin events it dropped.

  Migration:
  - Replace `SmallVec`/`smallvec![]` for these aliases with `Vec`/`vec![]`,
    or with `try_push` for the `ArrayVec` ones.
  - Rename `ParameterQueue::INLINE_POINTS` to `MIN_POINTS`.
  - Check the `bool` from a harmony `add_change`.
  - Replace a `Retire<T>` with a newtype whose `Drop` calls
    `AudioThread::check_not_current`.

- **The control-lane floor** (design doc 014, PR 2a): the port rule for
  event sources, the graph's play clock and a shareable range cell.
  **Breaking** in three ways. A param port's event source *replaces* the
  base instead of adding to it. A port refuses a second event source, or a
  shaped one. And `tutti_graph::Env` has a new field, so a struct literal
  must add `play`.

  | Was | Now |
  |---|---|
  | an event-driven param port read `clamp(base + shape(event value) + Σ audio)`, its event value starting at 0 | it reads `clamp(held + Σ shape(audio))`: `held` is the event source's value from its first event on, and the base before it. A first `ParamRamp` starts from the base on its frame. Audio sources are still offsets, on whichever holds |
  | any number of event sources per port, each with any `ParamShaping` | at most one, with `ParamShaping::Identity`; `GraphSpec::validate` and `Editor::commit` refuse a second (`GraphInvalid::TooManyEventParamSources`) or a shaped one (`GraphInvalid::ShapedEventParamSource`), and the plan verifier checks both (rule 9) |
  | `Env { frame, sample_rate, block_len, transport, changes }` | adds `play: PlayClock`. An `Env` built by hand (a test, a node driven outside a graph) uses `PlayClock::ZERO` |
  | — | the play clock (doc 014 §4.3): `Env::play_at(offset)`, `Cx::play_at(offset)` (at the input's time, through `TransportHistory`), `tutti_graph::{PlayClock, PlayReading}`, `Editor::play_reading()`, `Executor::play_reading()`. It counts one per frame, rolling or stopped, and restarts at 0 in a new epoch on every play start. Every executor, a fork's included, starts it at 0 |
  | — | `tutti_types::RangeCell`: a `(min, max)` pair in one atomic word that is never torn, with a loom model. The param ports' live ranges sit on it, and `Editor::param_range_cell(at)` hands out a read-only view of the cell a port clamps to (`tutti_graph::ParamRangeReader`; writes go through `Editor::set_param_range`) |

  Migration:
  - Remove any knob-to-zero workaround that kept the base from adding to automation.
  - Wire two event streams to one param by summing them in one node (or one lane) first.
  - Shape an event stream before it reaches the port, in the node that sends it.
  - Add `play: PlayClock::ZERO` to every hand-built `Env`.

- **The in-process VST2 node delivers parameter events** (design doc 014
  §13, decided question 2; doc 013, "Plugin parameter delivery"). Its
  event input took MIDI only and dropped every parameter ramp and curve
  segment. It now delivers them through the out-of-process node's
  delivery (`host/node/param_delivery.rs`, and the event walk both nodes
  now share, `param_input.rs`): a slot per parameter the plugin declared
  at load (no spares: an index past them is refused and counted in
  `points_dropped`), **changes only** against the value last
  delivered to the instance, a curve segment as points along it. The block
  is the chunk: before the plugin processes a block, each parameter that
  moved in it is set to its last value there with `setParameter`, under
  the instance lock, as the out-of-process VST2 loader sets a chunk's (VST
  2.4 has no sample-accurate parameter events, so a point inside the block
  lands on its first frame). A block the audio thread finds the instance
  locked for owes its values to the next. Nothing allocates on the audio
  thread with 200 parameters moving every block. A fork (an export) is
  seeded with what the live node delivered, read before the live state is
  saved, so an export plays an edit made in the plugin's editor as live
  playback does. `Vst2Instance::{get_parameter, set_parameter}` no longer
  allocate (their bounds check cloned the plugin's `Info`, name and vendor
  strings included, on every call; this reached the out-of-process VST2
  loader's per-chunk parameter writes too). **Breaking:**

  | Was | Now |
  |---|---|
  | `IntoNode for InProcessVst2Client`: `type Controls = PluginMeter` | `type Controls = InProcessVst2Controls` (`tutti_plugin::InProcessVst2Controls`, also in `tutti_plugin::handles`): `set_meter`, `clear_meter`, `has_meter`, `meter()` as before, plus `param_delivery() -> ParamDeliveryStats` and `automation(params)` |
  | — | `InProcessVst2Client::{controls, param_delivery, automation}` |
  | `Plugin::automation(params) -> Option<PluginAutomation>`, `None` for an in-process VST2 plugin | `-> PluginAutomation`, for either backend (a VST2 plugin's addresses are `ParamAddress::Index`) |
  | `InProcessVst2Client: Clone` (a clone drove the same `AEffect`) | not `Clone`: its delivery bookkeeping describes the one instance, so two copies would disagree about what it holds. A second node on the plugin is a fork (`IntoNode`'s fork, or `fork_instance`) |

  **Migration.** Code that used the inserted node's controls as a
  `PluginMeter` calls the same methods on `InProcessVst2Controls`, or
  `.meter()` for the cell itself. Drop the `Option` from
  `Plugin::automation`'s result. A host that drove an in-process VST2
  plugin's parameters through its handle only can wire a
  `PluginAutomation` (or any parameter event source) to the node's event
  input instead; values set through the handle still reach the plugin and
  are not overwritten by a host value held still. Code that cloned an
  `InProcessVst2Client` forks it instead, or loads the plugin again.

- **Plugin parameters are delivered as changes only, with no 16-queue
  cap** (modulation PR 1; design doc 013, "Plugin parameter delivery").
  The hosted plugin node reads the plugin's parameter list once, at
  `PluginClient::new`, and reserves a slot per parameter (plus 64 spare)
  at bind and, at prepare, a chunk with a queue per slot and room in each
  for a point every 32 frames of the chunk plus 4 corners; the bridge's
  parameter buffers are reserved the same and circulate with the node's,
  so any number of parameters can move in one chunk and nothing is
  allocated on the audio thread. Before, a chunk held 16 queues and the
  points of a 17th parameter were dropped with nothing counted. The node
  now sends a parameter only when the host's value differs from the last
  one it delivered: a value held still is sent once, so it no longer
  overwrites an edit made in the plugin's own editor, while a value that
  moves replaces it (the rule is in `host/node/param_delivery.rs`'s
  docs). A chunk whose changes cannot go (no free buffer, a refused
  block) sends each parameter's latest value on the next chunk's first
  frame. The VST3 host keeps its parameter queues pooled at the plugin's
  parameter count instead of trimming and regrowing them every block, each
  with room for a chunk's points at the activated block size
  (`ParameterQueue::chunk_room`), and the CLAP host reserves its event
  list for every parameter's points at its block size (past that room a
  point replaces its parameter's previous one, counted by
  `ClapActive::param_points_dropped`) and its output queues for the
  parameter count. The CLAP host's `InputEventList::sort_by_time` no
  longer allocates (std's stable sort took a scratch buffer past a small
  list, on the server's audio thread): it sorts `(time, index)` keys in a
  scratch reserved with the list and applies the order in place, keeping
  same-frame events in insertion order. **Memory:** the parameter buffers scale with the
  parameter count: about 43 MB for 2 000 parameters at a 4096-frame
  chunk, 340 MB for a 16 384-parameter VST3, on the host; the
  plugin-server reserves for the same points again: the CLAP host's event
  list about 19 MB at 2 000 parameters and 157 MB at 16 384 (4096
  frames, uncapped), the VST3 host's queues about 35 MB at 16 384 (the
  follow-up is in doc 014 §12). **Breaking:**
  - `ParameterChanges::queues` is a `Vec<ParameterQueue>` (was
    `SmallVec<[ParameterQueue; 16]>`), and the struct has a private field
    (spare queues, not serialized), so it is no longer built with a
    struct literal. The wire format is unchanged.
  - **Migration.** Build with `ParameterChanges::new()`,
    `with_capacity(queues)` or, for a real-time producer,
    `reserved(queues, points)` with `open_queue` and
    `ParameterQueue::has_room`; code that indexed or iterated `queues`
    is unchanged. A `SmallVec`-specific call (`inline_size`, `spilled`)
    on `queues` has no replacement: use `capacity()` / `has_room()`.
  - `tutti_clap_host::InputEventList::add_param_changes` takes its
    `ranges` **sorted by id** (it binary-searches them); an unsorted list
    finds the wrong range or none. Added
    `InputEventList::{set_param_point_room, param_points_dropped}` and
    `ClapActive::param_points_dropped`.
  - Added `tutti_plugin_types::ParameterQueue::{chunk_room,
    INLINE_POINTS, POINT_SPACING, CORNER_POINTS}`: the points one
    parameter can carry in a chunk, which the plugin node and the format
    hosts reserve.
  - Added `PluginControls::param_delivery() -> ParamDeliveryStats`
    (`points_sent`, `chunks_deferred`, `points_dropped`, `points_merged`;
    `#[non_exhaustive]`), exported from `tutti_plugin::handles`.
  - Behaviour: a plugin no longer receives the host's value every block.
    A host that relied on that to undo an editor edit sends a new value
    instead. The node's record of what it delivered survives a reset, a
    re-prepare and a state or preset load, and an export's fork is seeded
    with it, so an export plays an edit made in the plugin's editor as
    live playback does. Addresses learned into spare slots (not in the
    plugin's list at load) are not in that record, so an export re-sends
    their held value.
  - Behaviour: a CLAP plugin that lists no parameters now has a parameter
    room of 0, so parameter points sent to it are dropped and counted in
    `ClapActive::param_points_dropped` instead of passed through.

- **An export's fork is built off the main thread** (design doc 013,
  "Recorded for later": the fork ran on the main thread). `Editor::fork` is
  split into a control-thread **gather** and a **build** that runs on any
  thread; bevy-tutti gathers in the frame the export starts and builds on the
  compute pool, so a hosted plugin's fork (a `plugin-server` launch and its
  state load, half a second or more) no longer stalls that frame. The fork is
  still the live graph at the moment the export starts. **Breaking** for
  bevy-tutti's `prepare` hook and `ExportInFlight`'s internals; additive in
  tutti-graph and tutti-export.

  | Was | Now |
  |---|---|
  | `Editor::fork(target, mode, prepare)` only | unchanged, and now `Editor::fork_gather(target, mode, prepare) -> PendingFork` (control thread: reads the spec, the param ramps and each source's gather) then `PendingFork::build() -> Result<(Editor, Executor), (ForkError, ForkRemains)>` (any thread) |
  | `ForkSource::fork` built every unit on the forking thread | new provided `ForkSource::gather(mode) -> Result<ForkBuild, ForkCause>`: `ForkBuild::pinned` (built and prepared in the gather; the default, so existing sources keep their behaviour), `ForkBuild::ready` (built in the gather, prepared in the build), `ForkBuild::deferred(closure)` (built in the build, on any thread). Override it to move work off the control thread; read anything live in `gather`, never in the closure |
  | — | `ForkBuild`, `PendingFork` (`Send + 'static`; `deferred()` counts the units still to build), `ForkRemains` (what a failed build had made: drop it where a pinned unit belongs) |
  | `ForkByClone` / `ParamFork` units were prepared on the forking thread | `ready`: prepared in the build |
  | out-of-process plugin fork: state save and `plugin-server` launch in one step | its `gather` saves the live state and reads its rate and meter; the launch and the state load are a deferred build. `PluginClient::fork_instance` still does all of it on the calling thread |
  | in-process VST2 fork | declares itself `pinned`: built, restored and prepared (`effSetSampleRate`) in the gather, on the main thread |
  | — | tutti-export `RenderGraph::gather(live, target, mode, rate) -> Result<PendingFork>` and `RenderGraph::build(pending) -> Result<RenderGraph, (Error, ForkRemains)>` |
  | bevy-tutti `ExportRequest::with_prepare(\|prepared, world\| ..)`, run on the main thread on the built fork | `with_prepare(\|world: &World\| move \|prepared: PreparedGraph<'_>\| ..)`: the hook reads the world in the frame the export starts and returns the edit, which runs on the compute pool once the fork is built. Move the world reads to the outer closure and the graph edits to the inner one; annotate the inner closure's argument |
  | `PrepareGraph = Box<dyn Fn(PreparedGraph, &World) + Send + Sync>` | `Box<dyn Fn(&World) -> PrepareEdit + Send + Sync>`, `PrepareEdit = Box<dyn for<'a> FnOnce(PreparedGraph<'a>) + Send>` |
  | bevy-tutti forked in `start_exports`, on the main thread | `start_exports` gathers; the task builds, edits, commits and renders |
  | the rendered graph was handed back with the result; a cancelled export's graph dropped on the pool | everything the task holds goes to the new `ExportDrops` resource wherever it drops (finished, failed or cancelled) and `poll_exports` drops it on the main thread. `ExportPlugin` inserts it |
  | — | CI's threaded-pool step also runs `export_fork.rs`'s `off_thread` tests |
  | `ForkError::Source` named the first failing source in key order | the first source whose **gather** fails, else the first deferred **build** that fails (gathers all run first): with key 1 failing in its build and key 2 in its gather, the error names key 2 |
  | — | a pinned unit dropped off its gathering thread while that thread unwinds a panic (a deferred build, a render) is leaked, not torn down there |
  | — | a `prepare` edit runs on the pool: it must not insert a main-thread-only unit (in-process VST2). One that never runs (failed build, cancelled export) is dropped on the main thread |

- **A param range is a control, not plan** (design doc 013, "A param
  range is a control"). Moving a modulated param's range no longer
  recompiles: the running unit reads each port's range once per block
  from a cell the editor writes. Through `Editor::set_param_range` a move
  costs a store, as the old `ClampBounds` did; a range-only `commit`
  sends no plan but still validates and clones the spec (linear in the
  graph, on the control thread). The clamp is applied as the value is
  read: a range that shrinks never rewrites the base or an event source's
  ramp, and widening it again releases the value to where they are. A
  range move steps, with no declick, as a recompiled range did.
  **Breaking** for `ParamPortOp`.

  | Was | Now |
  |---|---|
  | a range change was a commit, and recompiled | `Editor::set_param_range(at, range) -> Result<(), GraphInvalid>` lands on the next block with no commit (refuses a NaN bound or a key with no node); a commit whose spec differs only in ranges (`GraphSpec::set_param_range` through `spec_mut`) validates, writes the ranges, and sends no plan |
  | `ParamPortOp::range` | removed: the plan carries no range (`ParamPortOp` is now `Eq`). Read a port's range from the spec (`GraphSpec::params`) |
  | — | `Reference::set_param_range`, the reference interpreter's mirror of the editor's |
  | bevy-tutti: a `ModParamRange` bounds edit recompiled the graph | `AudioGraphRes::set_param_mod` with the sources the graph already holds moves only the range, through `Editor::set_param_range`: a store, no commit |

- **A node behind a latent path reads the transport its input was rendered
  at** (design doc 013, "Arrival-aligned transport landed"). **Breaking**
  for code that builds a `tutti_graph::Cx` by hand. The executor keeps a
  `TransportHistory` of the transport's jumps (a fixed 256, about 24 KiB,
  whatever the latency) and hands it to every node;
  `Cx::transport_at(offset)` is the transport `Cx::arrival` frames before
  `offset`, exact through seeks, transport changes and loop wraps in
  earlier blocks. A hosted plugin is
  now sent that transport for each chunk, so a tempo-synced plugin behind a
  latent path is no longer off by the upstream latency.

  | Was | Now |
  |---|---|
  | `Cx { env, arrival }` | `Cx { env, arrival, history }`: pass `TransportHistory::none()` where nothing is kept, or a `TransportHistory::new()` you `observe` every block |
  | a node behind a latent path read `cx.env.transport_at(offset)` (its own frame) | `cx.transport_at(offset)` (its input's frame) |
  | — | `Env::transport_before(offset, back, &history)`, `TransportHistory` (`JUMPS`, `CHECKPOINT`) |

- **An in-process VST2 plugin forks by state transfer**, so an export of a
  graph holding one renders it (design doc 013, "In-process VST2 forks by
  state transfer"). **Breaking** for `Plugin::into_client`. Inserting an
  `InProcessVst2Client` hands the editor a fork source: a second instance
  from the same library, restored from the live instance's current program
  and saved state (its chunk, or its parameter values), reading its
  transport from the graph's `Env`, told its rate by its own `prepare`, and
  told it renders offline in an offline fork (a live fork keeps the live
  instance's mode). Fork and drop it on the main thread. A plugin that will not save,
  will not load a second time, or refuses the state fails the fork by name
  (`ForkError::Source`, a `PluginForkError`; bevy-tutti's
  `ExportError::ForkSource`).

  | Was | Now |
  |---|---|
  | `Plugin::into_client() -> Result<Box<PluginClient>, Box<dyn tutti_graph::Node>>` | `-> Result<Box<PluginClient>, Box<Plugin>>`: the in-process VST2 plugin comes back whole; insert `*plugin` through its `IntoNode` (`editor.insert(key, kind, plugin)`), which carries its fork source. `tutti_graph::Unforkable(node)` around the old `Err` would refuse every fork |
  | `IntoNode for InProcessVst2Client` handed no fork source; a fork was `ForkError::NotForkable` | hands one; new `InProcessVst2Client::fork_instance(ForkMode)` |
  | bevy-tutti: an export through an in-process VST2 plugin was `ExportError::NotForkable` | renders; `NotForkable`'s message no longer lists it |
  | — | `tutti_vst2_host::Vst2Instance::program_count` |
  | dropping a `Vst2Instance` anywhere | debug-asserts the main thread once one is marked (`effMainsChanged(0)`, `effClose`), as `load` does |
  | tutti-export `render_to_file` / `render_to_buffers` / `render_normalized_to_file` took `RenderGraph` | take `impl BorrowMut<RenderGraph>`: by value as before, or `&mut` to keep the graph and drop it on the main thread |
  | bevy-tutti dropped the rendered graph on the compute pool | hands it back; `poll_exports` (now main-thread-pinned) drops it |
  | tutti-export `Error::NotForkable`'s message listed in-process VST2 | no longer does |
  | — | `tutti-vst2-test-plugin`: `TUTTI_VST2_PROBE_STATE_DC` (the instance's state as an output DC, `config::state_dc`), `TUTTI_VST2_PROBE_UNIQUE_ID`, and `tutti_vst2_probe_set_refuse_chunk_load` |

- **`AudioUnit` and `tutti-node` are deleted** (design doc 013, "Phase 5,
  part 3"). **Breaking.** fundsp's node contract had no implementor left
  but the sampler's internal stretch filter. The node contract is
  `tutti_graph::Node`.

  | Was | Now |
  |---|---|
  | crate `tutti-node`, `tutti::node` | removed |
  | `tutti_core::{AudioUnit, BufferRef, BufferMut, BufferVec, Signal, SignalFrame, MAX_BUFFER_SIZE, Real, Sample, F32, F64}`, `tutti_core::prelude::{AudioUnit, BufferRef, BufferMut, SignalFrame}` | removed: implement `tutti_graph::Node`; planar data is `&[&[f32]]` / `&mut [&mut [f32]]` |
  | `tutti_core::{FaultLatch, RenderFault}` | removed (the latch is tutti-sampler's, crate-private) |
  | `tutti_core::Real` (the filters' float bound) | `tutti_nodes::Real` (`f32`, `f64`) |
  | `tutti_core::{mnemonic, assert_unique}`; `tutti_plugin::backend::{PLUGIN_CLIENT_ID, route_with_latency}` | removed (`get_id` fingerprints and `route` went with `AudioUnit`) |
  | `tutti_sampler::stretch::Unit` implements `AudioUnit` | inherent `reset`, `set_sample_rate`, `tick`, `process(size, &[&[f32]], &mut [&mut [f32]])`, `tail`; `latency_samples`, `input_rate`, `filter_lanes` unchanged |
  | `just miri` / CI `miri` ran `tutti-node` | removed (its planar buffers were the subject) |

- **The fundsp fork is deleted, and the latency solve is one pass**
  (design doc 013, "Phase 5, part 2"). **Breaking.**
  `crates/vendor/fundsp-tutti` is gone. `tutti_types::latency` no longer
  inserts anything: `delays(&g)` returns the per-port and per-output delays
  as a value, a global input is aligned at a merge point, and event and
  param sources count toward a node's arrival — the compiler's solve, so
  `latency::plan(&spec)` equals the compiled plan's compensation.

  | Was | Now |
  |---|---|
  | `crates/vendor/fundsp-tutti` (package `fundsp-tutti`) | removed |
  | `latency::{DelayInsertion, compensate(&mut G)}` | `latency::delays(&G) -> Delays<N>` (`inputs()`, `outputs()`, `compensation()`) |
  | `LatencyGraph::inputs -> Iterator<Item = Option<N>>` | `-> Iterator<Item = Feed<N>>` (`Node`, `Outside`, `None`); new `LatencyGraph::other_sources` (default none) |
  | `latency::plan` left a global input undelayed and ignored event edges | aligns a global input; counts `other_sources` (`GraphSpec` lists event and param sources) |
  | — | `impl LatencyGraph for tutti_graph::GraphSpec` |
  | CI installs `libfontconfig1-dev` | not needed (the fork's `plotters` dev-dependency was the only edge) |

- **Nothing uses fundsp's `Net`** (design doc 013, "Phase 5, part 1").
  **Breaking.** `AudioNode` wraps a `NodeKey`; the `Topology → Net`
  compile route and the `dsp` re-exports are gone. `Editor::commit` with
  nothing changed since the last commit sends nothing and compiles nothing.

  | Was | Now |
  |---|---|
  | `tutti_core::dsp::{Net, NodeId, Source}`, `tutti::dsp`, `tutti_core::prelude::NodeId` | removed: build a graph with `tutti_graph::{GraphBuilder, Editor}`; a node's key is `NodeKey` |
  | `AudioNode(NodeId)`, `AudioNode::id() -> NodeId`, `From<NodeId>` | `AudioNode(NodeKey)`, `AudioNode::key()`, `AudioNode::fresh()`, `From<NodeKey>` |
  | `NodeId::new()` (a fresh key) | `NodeKey::fresh()` |
  | `tutti_core::topology::{compile, Catalog, CompileError, Compiled}` | removed: `tutti_graph`'s compiler (`Editor::commit`) |
  | `tutti_core::{unit_param, PdcDelay, PDC_DELAY_ID, Setting}` | removed: params are `ParamSet` ports; PDC delays are the plan's |
  | bevy: `PluginShadow::{new, node}`, `ModParamsHandle::{new, of, node}` over `NodeId` | over `NodeKey` |
  | tutti-sampler `VoicePoolNode(NodeId)` | `VoicePoolNode(NodeKey)` |
  | tutti-sampler example `profile_stretch_clone`, `just profile-stretch` | removed (its subject was `Net::commit`; its findings are in doc 013) |
  | tutti-graph bench `graph_render`'s `net` rows | removed; the `graph` rows stay |
  | `Editor::commit` always compiled and sent a plan | an unchanged commit returns `Ok` and sends nothing |

- **"Native" is dropped from the graph's names** (design doc 013, item 9).
  With `Legacy` gone every node is a `tutti_graph::Node`, so "native graph"
  (as against `Net`) and "native node" (as against a `Legacy` one) no
  longer distinguish anything. **Breaking** for `contract::NativeIsolateRow`
  only; the rest is crate-private, test or bench naming. Other senses of
  "native" (a plugin's native window or module, native f64, MIDI 2.0 native
  per-note) are unchanged, as is doc 013's filename.

  | Was | Now |
  |---|---|
  | `tutti_graph::contract::NativeIsolateRow` | `tutti_graph::contract::IsolateRow` |
  | bevy-tutti `graph/native.rs`, `NativeGraph`, `NATIVE_MAX_BLOCK`, `AudioSide::native` (crate-private) | `graph/runtime.rs`, `GraphRuntime`, `LIVE_MAX_BLOCK`, `AudioSide::live` |
  | tutti-graph bench `graph_render`'s `native` row | `graph` |
  | `tutti-polysynth/tests/native_node.rs` | `tests/graph_node.rs` |

- **`Legacy` is deleted; every graph node is a `tutti_graph::Node`**
  (design doc 013, "Legacy deleted"). **Breaking.** An `AudioUnit` can no
  longer go into a graph: a host's own node implements `Node` (and
  `ParamNode` for params) and says how it forks (`ForkByClone`,
  `Unforkable`, `param_parts`, or its own `IntoNode`). The engine renders
  every block in one pass (no 64-frame chunk-major mode).

  | Was | Now |
  |---|---|
  | `tutti_graph::{Legacy, LegacyControls, LegacyForkHook, Delivery, LEGACY_CHUNK, LEGACY_SETTINGS_CAPACITY}`, `Shape::legacy`, `Plan::has_legacy` | removed: implement `Node` |
  | `GraphBuilder::{add_unit, add_pure_unit, chain_unit}` | `GraphBuilder::{add, chain, add_with_controls}` over an `IntoNode` |
  | `contract::{Row::legacy, IsolateRow, assert_isolate_snapshots}` | `contract::NativeIsolateRow` (a node's fork snapshot) |
  | `EnvClock`, `BEAT_PORTS`, `beat_from_ports`, `TransportClock::split_beat`; bevy `EngineNodes::clock` | removed: a node reads the beat from its block's `Env` (`Env::for_each_beat`, `Env::transport_at`) |
  | `ParamFeed`, `AudioUnit::{param_feed, param_base}` | removed: a param port is `Io::param(k)` |
  | `AudioUnit::render_fault` | removed: nothing read it once `Legacy` went; a node's fork hands its health probe to the graph (`ForkHealth`). `RenderFault` / `FaultLatch` stay (the sampler's disk voice latches one). `AudioUnit::forkable` stays for `Net`'s forwarding only |
  | `impl ModParams` for the tutti-nodes nodes and `PolySynth` (`mod_params.rs`) | a route resolves to the node's `ParamSet` cell. The compressor's makeup is addressed only as `UnitParam::GainDb`; a route on `Makeup` does not bind (its doc says so). Plugins keep `ModParams` |
  | bevy: `spawn_audio_node` / `insert_audio_node` / `crossfade_audio_node` over an `AudioUnit`, and the `spawn_graph_node` / `crossfade_graph_node` / `SpawnGraphNode` twins | one family, `spawn_audio_node` / `insert_audio_node` / `crossfade_audio_node` over `N: GraphNode` |
  | bevy: `AudioGraphRes::{insert (AudioUnit), insert_node, insert_boxed, replace (AudioUnit), replace_node, inspect}`, `Boxed` | `AudioGraphRes::insert<N: IntoNode>` and `replace<N: IntoNode>`; `replace_plugin` crossfades when the shapes fit, like any node |
  | bevy: `CapturedControls::capture(&dyn AudioUnit)`, `ModTargetRegistry::register::<T>()` | `CapturedControls::for_params` / `for_plugin`; `ModTargetRegistry::insert_target` |
  | bevy: `param_graph_node!` crate-private | exported (`bevy_tutti::param_graph_node!(MyNode)`), the one line a host writes for its own `ParamNode`; `ForkByClone` and `Unforkable` in the prelude |

- **Every node is a native graph node** (design doc 013, "Items 8 and 9: the
  per-node port"; the rest of tutti-nodes, tutti-spatial, tutti-sampler,
  the click, the mic and the instruments). **Breaking.** No node in the
  tree implements `AudioUnit` any more (`Legacy` remains for a host's own
  units until it is deleted). A node is prepared by its graph (no
  placeholder rate), takes its params on param ports, is set through the
  controls it is inserted with, forks without `isolate`/`rebind_offline`,
  and a clip reader reads its transport from its block's `Env` — so
  sampler clips enter, exit, seek and loop on their own frame, not the next
  64-frame chunk.

  | Was | Now |
  |---|---|
  | `LadderFilterNode`, `CompressorNode`, `GateNode`, `LimiterNode`, `BrickwallLimiterNode`, `DistortionNode`, `BusStripNode`, `DelayLineNode`, `ModDelayNode`, `PhaserNode`, `ConvolverNode`, `LfoNode`/`ModulatorNode<M>`, `PolySynth`, `MemorySource` implement `AudioUnit` | `tutti_graph::Node` + `ParamNode`, inserted through `param_parts` with a `ParamSet` as controls |
  | `DownmixNode`, `ChannelSumNode`, `AutomationLaneNode`, `SoundFontUnit`, `InProcessVst2Client` (f32 and f64), `MicMonitorNode` implement `AudioUnit` | `tutti_graph::Node` + `IntoNode` (fork by clone; the lane's `Curve::frozen`; the SoundFont's template; VST2 and the mic unforkable) |
  | `VbapPannerNode`, `HrtfBinauralNode` implement `AudioUnit`; position through the node kept (`Clone` shared the cells) | `Node` + `IntoNode` with `VbapPannerControls` / `HrtfBinauralControls` (position, spread, width, blend: no `UnitParam` addresses them); bevy-tutti feature `hrtf` (in `full`) |
  | `VoiceNode::with_commands`, `VoicePool::new() -> (pool, handle)`, `DiskVoice`, their `Arc<dyn Timeline>` / `BeatCursor`, `replace_transport`, `Voice::{isolate, rebind_offline}` | `Node`s with `VoiceNodeHandle` / `VoicePoolHandle` / `DiskVoiceControls`; the transport from `Env`; forks through `VoiceNodeFork` / `PoolFork` / `DiskVoiceFork` (a live fork of a disk voice is refused: `LiveDiskFork`). `MemorySource::placed(..)`, `window_position(&Transport)`, `take_disk_voice(channel, start, dur)`; bevy `spawn_voice(voice, width)`, `VoiceCommands = NodeControls<VoiceNodeHandle>` |
  | `ClickNode::with_transport(transport, settings, rate)` with two beat inputs wired from the clock; `LfoNode` beat-synced and `AutomationLaneNode` with `BEAT_PORTS` inputs | no inputs: the beat is read from the block's `Env` (`Env::for_each_beat` / `for_each_piece_beat`); `ClickNode::new(&transport, settings)`, controls `Arc<ClickSettings>`. A start or stop inside a block now gates the click on its frame |
  | `impl AudioUnit for TransportClock`, `TRANSPORT_CLOCK_ID`, `CLICK_NODE_ID`, `MIC_MONITOR_ID` | removed (the engine drives the clock) |
  | `AudioUnit::set(unit_param::setting(p, v))` | `ParamSet::set(p, v)`. The delay's `DelayTime` by address is channel 0's; the compressor's makeup is addressed as `UnitParam::GainDb` (a control-rate route on `Makeup` no longer binds); the LFO's `Rate` only while free-running |
  | `set_sample_rate` before the first `process`; `tick`; `param_feed` | the graph's `prepare`; a one-frame block; the param port `Io::param(k)` |
  | `PolySynth::queue_midi`, `SoundFontUnit::queue_midi`, `InProcessVst2Client::queue_midi`, the synths' `fork_source`, `tutti_plugin::in_process_vst2` | MIDI on the event input only; `in_process_vst2_client`; `Plugin::into_client`'s `Err` is `Box<dyn tutti_graph::Node>` |
  | `ConvolverNode`: IR spectra per channel and per fork; `convolution` pulls `fft-convolver` | one read-only `IrSpectra` behind an `Arc` shared by channels and forks (bit-identical output); `convolution` pulls `realfft` |
  | bevy-tutti: `spawn_audio_node` / `AudioGraphRes::insert` / `ModTargetRegistry::register::<Node>()` for these nodes; `impl GraphNode for PolySynth {}` | `spawn_graph_node` / `insert_node` + `set_node_params`; no registration (`CapturedControls::for_params`); `param_graph_node!(..)`, so an `AudioParam` on a spawned synth reaches it and its fork |

  New test support in `tutti_graph::contract`: `drive_in` (a block under a
  given `Env`, events in, `Driven` audio and events out), `Direct`'s
  events, transport, block length and `block_in(&Env)`, `BlockRig::block_in`,
  `NativeIsolateRow` (the fork-snapshot row for a native node, every cell
  it reads) and `Row::with_lead`. **Fixed with it:** `SvfFilterNode` and
  `EqBandNode` declare `Tail::Unknown` as they reported under `Legacy`
  (#60 had left the shape's `None`); a beat-synced audio-rate modulation
  source no longer reads beat 0 forever (its beat inputs were never
  wired); `tutti-core`'s `alloc_budget` no longer counts another thread's
  allocations (it failed about 1 run in 4 under CPU load: libtest's main
  thread allocating inside the window). A time-stretched memory voice is
  no longer silent for its stretch filter's refill (a window times the
  stretch, ~4 100 frames at 2x) after a seek or loop wrap: the jump primes
  the flushed filter with the source leading up to the new position
  (`verify-audio`'s `seek_while_stretched` passes). The sampler's two
  seek-flush tests had parked their "silent" playhead in the loud half
  (they forgot the 2x stretch) and passed only inside that gap; they now
  park it in the silent half, and still fail without the flush. A disk
  voice still refills after a seek: its ring reads forward only.

- **Nodes are ported to the native graph contract, one at a time; the
  first are the SVF and the EQ band** (design doc 013, "Items 8 and 9: the
  per-node port"). **Breaking** for those two. A ported node is a graph node
  with its params addressed by `UnitParam`, and a fork that shares nothing
  with it.

  | Was | Now |
  |---|---|
  | `SvfFilterNode` / `EqBandNode` as `AudioUnit`s: `tick`, `process(size, &BufferRef, &mut BufferMut)`, `set_sample_rate`, `set(Setting)`, `isolate`, `param_feed` | `tutti_graph::Node` (`prepare`, `process` over `Io`, its cutoff and Q on param ports) and `IntoNode` with a `ParamSet` as its controls; `ParamNode::fork_fresh` is the isolate |
  | a host setting a param by address through `Legacy::controlled`'s settings ring and shadow | `tutti_graph::ParamSet::{set, set_authored, get, authored, cell}` over the node's own cells; a fork (`param_parts`' `ParamFork`) starts from the authored values |
  | bevy-tutti: `spawn_audio_node(SvfFilterNode::..)`, `crossfade_audio_node(.., Box::new(..))` | `spawn_graph_node(..)` (the prelude now carries `SpawnGraphNode` and `GraphNode`), `crossfade_graph_node(..)`; `AudioParam` and `AudioGraphRes::set_param` reach it through its `ParamSet` (`GraphNode::params`, `AudioGraphRes::set_node_params`); modulation targets it with no `ModTargetRegistry` entry (`CapturedControls::for_params`) |
  | `build_vbap_mix(&mut Net, ..)`, `VbapMixParts::insert_into(&mut Net, &[NodeId])`, `VbapSource<N = NodeId>` | `build_vbap_mix(&mut GraphBuilder, ..)`, `insert_into(&mut GraphBuilder, &[NodeKey])`, `VbapSource<N = NodeKey>` (its LFE low-pass is the SVF, which a `Net` cannot hold) |

  New in `tutti-graph`: `ParamSet`, `ParamNode`, `ParamFork`, `param_parts`,
  `Editor::replace_or_swap` (a fade when the unit fits, else a swap on the
  next commit), `Solo` (one node through a graph), and in the `contract`
  test support `drive`, `prepared`, `assert_param_fork`, `BlockRig` and
  `Direct`. `tutti_types::AtomicF32` is re-exported. `tutti-nodes`'
  `live_value_survives_commit` (the `Net` frontend/backend rule, fixture
  `EqBandNode`) is now `live_controls_reach_the_node` (the rule for a node
  the executor owns), and `svf_through_legacy_settings` pins `Legacy`'s
  ring on the ladder filter as `legacy_settings_ring`.

- **The MIDI shells are deleted: MIDI is event ports end to end** (design
  doc 013, "Rewrite order" item 5). **Breaking.** Hardware input, the MIDI
  clock and MIDI out are graph nodes; a synth, plugin or SoundFont hears
  exactly what is wired to its MIDI event input, and nothing reaches a unit
  around the graph any more.

  | Was | Now |
  |---|---|
  | `MidiPreBlock` / `MidiPostBlock` run around each block by the device callback (`AudioCallbackState::with_pre_block` / `with_post_block`, tutti-cpal's `midi` feature) | `MidiInputNode` (hardware in, 16 channel ports plus `CHANNELLESS_PORT`, `input_ports(channel)`), `ClockNode` (ticks `ClockMaster` from the graph's transport) and `MidiOutNode` (`MidiOutControls::poll_into` / `receiver()`), inserted like any node. tutti-cpal's `midi` feature is gone |
  | `MidiBus`, `MidiInPort`, `MidiMailbox` keyed by `MidiUnitId`, `MidiRoutingTable` / `MidiRoute` / `MidiRoutingSnapshot`, `MidiRouter`, `MidiUnitIn`, `MidiUnitId`, `MidiOutSink`, `BlockClock`, `OfflineRebind` | a wire from the source's event output to the sink's event input (event inputs fan in, merged by frame). A control-thread producer uses `MidiQueueNode` (`MidiSender` controls) or `MidiMailbox::pair()` / `with_capacity(n)`, with no unit id |
  | `MidiClipSource`, `MidiSnapshot`, `MidiSnapshotReader`, `TimedClipEvent` | `MidiClipNode` (item 4); `TimedMidiEvent` stays |
  | `ClockMaster::new(transport, sample_rate, sender)`; `tick(block)` sent into a mailbox | `ClockMaster::new(sample_rate)`; `tick(&transport, block_size, &mut emit)`. `ClockNode` drives it and re-rates it in `prepare` |
  | `PolySynth` / `SoundFontUnit`: `set_midi_source` / `clear_midi_source`, `midi_port`, `midi_sender`, `midi_unit_id`; `fork_instance(ForkMode)` and `Error::MidiSource` | the event input in a graph; `queue_midi(&[MidiEvent])` when driven by hand as an `AudioUnit`; `fork_instance()` |
  | tutti-plugin: `PluginClient::{set_midi_out, clear_midi_out, set_midi_source, clear_midi_source, midi_sender, midi_port, midi_unit_id}`, `PluginHandle::midi_sender`, `MidiInView` / `MidiOutView`, `PluginForkError::MidiSource` | the plugin node's MIDI event input and (for a plugin that sends MIDI) event output; `takes_midi()` / `sends_midi()`. In-process VST2 is a native node (`queue_midi` when driven by hand) |
  | bevy-tutti: `MidiBusRes`, `MidiRoutingRes`, `MidiTargetRegistry` / `MidiTarget` / `MidiTargetResolver`, `MidiRegistrationPlugin` / `MidiRegistered`, `MidiOutSinkRes`, `InstalledMidiSources`, `AudioGraphRes::{insert_with, replace_with}`, MIDI capture in `capture` | `MidiEngineNodes` (the input, clock and out nodes), `MpeModeRes`, `LiveMidiInput` → `LiveMidi` (a keyboard's queue wired to its entity), route rules compiled into `EventFeeds` of `EventSource { node, port }`, `RoutedTargets`. `AudioGraphRes::set_event_sources(sink, port, &[EventSource])`; `render_frame_at(&transport, out)` |
  | `ClockMasterRes::new(master, receiver)` | `ClockMasterRes { master, out: MidiOutControls }` |

  A fork of the input, queue or out node is silent (an export hears no
  hardware or keyboard); a clip node forks with its clip. Tests of the
  deleted routing (`midi_bus_routing`, `midi_routing_table`,
  `frame_exact_clip`, `outbound_block_path`, the pre/post-block no-alloc
  gates, `clip_source_shared`, the export's `host_midi` module) went with
  it; their properties are pinned on the nodes (`edge_nodes`,
  `midi_routes`, `rt_no_alloc`, the synths' `a_graph_block_plays_its_event_input`).

  **Fixed with it:** the first block on a cold thread no longer allocates
  (recorded, not fixed, below). The allocation was arc-swap's per-thread
  slots on `MidiInPort`'s first load; the port is gone, and
  tutti-polysynth's `a_first_block_on_a_cold_thread_is_allocation_free` is
  no longer `#[ignore]`d.

- **Smart types 2: four footguns found in Phase 3/4 review, made
  unwritable** (design doc 013 §5). Each was a rule a doc stated and nothing
  enforced; each is now a type, or a named error where the mistake is made.

  | Was | Now |
  |---|---|
  | `ForkMode::Offline(&dyn Any)`, downcast by every unit; a context of any other type rebound nothing, silently | `ForkMode::Offline(&OfflineTransport)`. A wrong type does not compile (`E0308`) |
  | `AudioUnit::rebind_offline(&mut self, ctx: &dyn Any)`; `MidiUnitIn::rebind_offline(unit, ctx: &dyn Any)`; `MidiInPort::rebind_offline_into(fork, ctx: &dyn Any)`; fundsp-tutti's `PendingClone::isolate_for_offline(ctx: &dyn Any)` | each takes `&OfflineTransport`; drop the `downcast_ref::<OfflineTransport>()` |
  | `tutti_core::transport::{Timeline, OfflineTransport}` defined in tutti-core | defined in tutti-types (`tutti_types::{Timeline, OfflineTransport}`), so tutti-graph can name them; tutti-core re-exports both at the old paths |
  | `Timeline` had `beat`, `tempo`, `is_rolling` | adds a **required** `segment_generation() -> u64`: moves on at every seek (even to the same beat), tempo or rate change, loop wrap and play start. A timeline that never jumps returns a constant. `FrameClock::generation` / `mark_play_start`, `TransportSettings::segment_generation` (the live cell, written by the engine's clock before the beat) |
  | the sampler's `Seat` re-seated on a new beat only, so a seek to the same beat (or a one-block transport loop) kept stepping | it re-seats on a new beat **or** a new segment generation, in both tiers (`MemorySource`, a forked `DiskVoice`) |
  | `OfflineTransport = Arc<dyn Timeline>` (an alias: any timeline, the live transport included, could be the offline context) | a newtype, `OfflineTransport::new(Arc<T>)` for `T: OfflineClock` (new marker trait, implemented by `OfflineTimeline`; the live `Transport` cannot be, by the orphan rule). Passing the live transport is `E0277`. It derefs to `dyn Timeline`; `.timeline()` hands out the `Arc<dyn Timeline>` a node keeps. Build one with `OfflineTransport::new(Arc::new(OfflineTimeline::new(..)))` rather than `as OfflineTransport`; a test clock used as a render's timeline implements `OfflineClock`. bevy-tutti's `ExportClock::timeline` / `ExportRequest::on_timeline` now bound `T: RenderClock + OfflineClock` |
  | `TransportSettings::beat` and `segment_generation` public cells; `TransportSettings::set_beat` public | crate-private (an outside store is `E0616`); read through `TransportSettings::beat()` / `segment_generation()` or `Timeline`. A host or test that moves the playhead by hand takes the transport's writer: `transport.clock_links()?.set_playhead(beat)` (new), which counts as a jump (the generation moves on) and is refused while an engine holds the writer |
  | `Transport::clock_links() -> ClockLinks`, callable any number of times; `ClockLinks`'s writer fields (`seek`, `position_writeback`, `steady_time`, `tempo_in_force`) public | `clock_links() -> Result<ClockLinks, PlayheadClaimed>`: the playhead writer is handed out **once** per transport (shared by its clones), given back when the last holder drops. The writer fields are private (a hand-built writer is `E0451`); `ClockLinks::writes_playhead()` |
  | a second `Engine::new` over one transport built, and two clocks both consumed every seek and wrote the playhead | refused: `GraphEngineError::PlayheadClaimed` |
  | `impl<N: Node> IntoNode for N` and `impl IntoNode for Box<dyn Node>`: any node inserted with no controls and no fork source, silently (#38's B1) | removed. Insert a bare node as `ForkByClone(node)` (forkable by a clone taken at insert) or `Unforkable(node)` (new: refuses every fork, by name; also takes a `Box<dyn Node>`), or give it its own `IntoNode`. Inserting a bare `Node` is `E0277` |
  | `IntoNode::into_node` required, `into_parts` defaulted to no fork source (a wrapper that forgot to forward it dropped its inner node's source) | `into_parts` required, `into_node` provided; `IntoNode::kind()` names a fork wrapper's inner node, so `GraphBuilder::add(ForkByClone(x))` records `x`'s type name as before |

  `rebind_offline` tests that passed a wrong-typed context (`&42u32`,
  `&"not a transport"`) and checked it was ignored are now `compile_fail`
  doctests: the case cannot be written. tutti-plugin's `Rebind::Sever` (an
  offline context that was not an `OfflineTransport`) is gone with it.
  Every insert site in the workspace (tests, benches, examples) was
  converted mechanically to `Unforkable(..)`, which is exactly what the
  blanket impl did.

- **Compiler-owned param modulation** (design doc 013, "Rewrite order"
  item 6). A node declares the params the graph may modulate
  (`Shape::with_params(&[UnitParam])`), a spec drives any of them from
  audio or `ParamRamp` event outputs (`GraphSpec::connect_param`,
  `disconnect_param`, `set_param_range`), and the compiler fuses each one
  into a step of the node's op: the node's own control (its base, read
  through the new `Node::param_base` once per block and ramped across it)
  plus every source through its `ParamShaping`, summed in source order and
  clamped once. The node reads it through `Io::param(k)`: per-frame values,
  or `ParamInput::Base` when nothing modulates it — **an unconnected param
  reads its base, never 0**, so modulation is connected and disconnected
  by any commit, crossfaded over `PARAM_DECLICK` frames rather than
  stepped. Param sources are PDC-aligned like inputs
  (`DelayKey::ParamAudio` / `ParamEvent`); a delay that appears or grows
  on an audio source holds its last value rather than dropping to 0. An
  event source's held ramp is the source's, kept across another source
  joining or leaving. A unit's first block (an insert, a replace, a fork)
  starts at its modulated value, undeclicked, and `Editor::fork` (so an
  export) copies the modulation, forks its sources and starts each event
  source at the ramp the live unit holds. A crossfade's two units must
  declare the same params (`CommitError::FadeShape` otherwise); the base
  across a crossfade ramps over one block, as any control move does. A
  NaN range bound is refused (`GraphInvalid::BadParamRange`). A range
  change recompiled when this landed (the old `ClampBounds` moved without
  a commit); it no longer does ("A param range is a control", above).
  This replaces the per-param
  sub-graph (`AtomicSourceNode → ParamSumNode ← ParamShaperNode × N`) and
  the extra input ports a node had to be born with. What changes for a
  caller:

  | Was | Now |
  |---|---|
  | `SvfFilterNode::with_param_inputs(.., mod_cutoff, mod_q)` and the same on `LadderFilterNode`, `DelayLineNode`, `DistortionNode`, `CompressorNode`, `GateNode`, `LimiterNode`, `BrickwallLimiterNode`, `BusStripNode`; their `cutoff_port()` / `q_port()` / `drive_port()` / `feedback_port()` / `delay_time_port()` / `threshold_port()` / `ceiling_port()` / `volume_port()` / `pan_port()` | removed. Build the node with its plain constructor (`with_channels`, `new`, …): every one of them is modulatable by the graph, and its arity is its audio width. Each declares its params as a `*_PARAMS` list (`SVF_PARAMS`, `DELAY_PARAMS`, …) in port order |
  | `tutti_nodes::ParamPorts` (`param_port(UnitParam) -> Option<usize>`) | removed: the graph asks the node's shape (`Shape::params`; `AudioGraphRes::declares_param` in bevy-tutti) |
  | `ParamShaperNode`, `ParamSumNode`, `AtomicSourceNode`, `ClampBounds`, `ParamModChain`, `ParamModParts`, `ParamModPart`, `ParamModEdge`, `param_mod_parts`, `build_param_mod`, `wire_param_mod` | removed. A modulation edge is `GraphSpec::connect_param(ParamIn { node, param }, ParamFrom::Audio(out), shaping)`; `ParamModShaping::shaping()` bakes a route's depth / polarity / curve into the `ShapeLut` (bit for bit the old shaper's table: `tests/param_mod_oracle.rs`); the range is `GraphSpec::set_param_range`; the base is the node's own control |
  | an `AudioUnit` read a modulated param from an input channel | an `AudioUnit` with a `ParamFeed` (new, `tutti_node`; `AudioUnit::param_feed` / `param_base`) is fed its modulated params per 64-frame chunk by `Legacy`, and reads its own control when a param is not fed. A forwarding wrapper must forward both methods |
  | `Op::Node { .., in_place }` | gains `params: Span` (into `Plan::param_ports`, each with its sources in `Plan::param_sources`) |
  | `GraphSpec`, `ValidGraph` | gain `params` (`ValidGraph::params`); `GraphInvalid` gains `UnknownParamNode`, `UnsortedParamSources`, `TooManyParamSources`, `BadParamRange`; `CompileError` gains `UnknownParam`, `ParamSourceOutOfRange`; `CycleEdge` gains `Param`; `DelayKey` gains `ParamAudio`, `ParamEvent`; `Shape` gains `params`; `ParamSourceOp` carries its `from` |
  | `ClampBounds` moved a range with an atomic store | `Editor::set_param_range` (no commit), or `GraphSpec::set_param_range` and a commit that sends no plan (see "A param range is a control") |
  | `ParamFeed::new` / `ParamPorts::new` only panicked on a list they cannot carry | `ParamFeed::try_new` (`ParamFeedError`) and `ParamPorts::try_new` (`ParamPortsError`) name it; `new` panics with that error. `MAX_FED_PARAMS == MAX_PARAM_PORTS` is asserted at compile time |
  | bevy-tutti: `ParamPortMap`, `DeclareParamPorts::with_param_ports` | removed: nothing to declare at spawn, the node's shape says it |
  | bevy-tutti: `AudioRateChains` (`base_cell`, `ParamChain`), `ShaperShaping` | `AudioRateRoutes` (`AudioRateParam`: the node, each route's source node and shaping, the range, the base). The modulation lives in the graph value: `AudioGraphRes::param_mod`, set with `set_param_mod` (which returns `Result<(), GraphInvalid>`, refusing a NaN bound, more than `MAX_PARAM_SOURCES` sources or a node listed twice) / `clear_param_mod`. Two routes from one `ModSource` onto one param sum into one table (`ParamModShaping::summed`); routes past `MAX_PARAM_SOURCES` source nodes are dropped with a warning; a NaN range keeps the route on the value path |
  | bevy-tutti: `write_param(graph, matrix, chains, ..)` sent an audio-rate param's write to the chain's base cell | `write_param(graph, matrix, ..)`: an audio-rate param's base is the node's own control, so the write goes through `set_param` like an unmodulated one — and so reaches a fork, which the base cell never did |

  New tests: `tutti-graph`'s `tests/param_mod.rs` (base-only equals the
  plain param, sums and clamps, base ramps, declicked connect/disconnect,
  sample-exact steps and ramps, a node without a base, a differential
  proptest against the reference interpreter), an `Excite::Param` row in
  the sample-accuracy contract (`param_echo`), a no-alloc gate, `Legacy`'s
  feed bridge; `tutti-nodes`' `tests/graph_param_mod.rs` (each node's fed
  params are the ones its DSP reads, at widths 1, 2 and 6, through the
  graph) and `tests/param_mod_oracle.rs` (the old chain as an oracle,
  pinned as digests). The tests of the deleted chain
  (`born_with_ports.rs`, `param_writer_ownership.rs`,
  `audio_rate_param_mod.rs`, `idle_chain_cost.rs`, the `param_port_wire`
  module) are replaced by those; bevy-tutti's modulation tests keep their
  properties, asserted on the graph value and on what renders.

- **A hosted out-of-process plugin is a native graph node, bound by
  typestate** (design doc 013, rewrite-order item 4, the plugin half).
  `PluginClient` is no longer a fundsp `AudioUnit` (neither `<F32>` nor
  `<F64>`) and runs through no `Legacy` adapter: a loaded client is
  `PluginClient<Unbound>`, `bind()` makes it `PluginClient<Bound>`, and only
  that is an `IntoNode` — inserting it hands back its `PluginControls` and a
  fork source (a fork by state transfer), so an unbound plugin cannot be
  inserted or processed (a `compile_fail` doctest pins it). Its transport is
  a pure function of the block's `Env` and the meter: nothing polls a
  timeline, so a plugin has the right transport from its first block, live
  and in an export's fork. Its latency is declared in its `Shape`
  (`PluginControls::declared_latency`: the plugin's figure plus the pipeline's
  chunk); a runtime change reaches PDC at the next commit through
  `Editor::set_latency`. The in-process VST2 client is unchanged (still an
  `AudioUnit`, still polls its transport). What changes for a caller:

  | Was | Now |
  |---|---|
  | `PluginClient` as a `Box<dyn AudioUnit>` (`Net::push`, `Legacy::new`, `GraphBuilder::add_unit`) | `editor.insert(key, kind, client.bind())` (or `GraphBuilder::add_with_controls(client.bind())`), which returns its `PluginControls` |
  | `IntoNode for PluginClient` (`Controls = ()`, through `Legacy`) | `IntoNode for PluginClient<Bound>` (`Controls = PluginControls`, with its fork source), and `PluginClient<Bound>` is itself the `Node` (`Unforkable(client)` would insert it without either, so do not); `PluginClient<Unbound>` is neither |
  | `PluginClient: Clone` | not `Clone`: a node exists once, in the graph. Keep `PluginControls` / `PluginHandle` (shared handles) instead |
  | `Plugin::into_unit()` / `Plugin::into_parts()` (a `Box<dyn AudioUnit>`) | `Plugin` is itself an `IntoNode` (`Controls = Option<PluginControls>`, `None` for in-process VST2): `editor.insert(key, kind, plugin)`. Clone `plugin.handle()` first to keep it. `into_client()` still returns the (unbound) `PluginClient`, or the VST2 node boxed |
  | `PluginClient::set_transport_source(reader, meter)`, `clear_transport_source`, `PluginControls::{set_transport_source, has_transport_source, clear_transport_source}`, `TransportView::{set_source, clear}` | the transport needs no install. Give the meter: `PluginClient::set_meter`, `PluginControls::{set_meter, clear_meter, has_meter}`, `TransportView::{set_meter, clear_meter}`. `Plugin::set_transport_source(reader, meter)` stays; the subprocess backend takes only the meter |
  | the declared latency was `AudioUnit::latency()` (`route`) | `PluginControls::declared_latency()`, what the node's `Shape` declares; `PluginClient::latency()` is still the plugin's own figure |
  | offline, the transport snapshot's loop was reported off | the loop is the render's. The continuous-sample counter is a per-node monotonic count of rendered frames (it does not jump on a re-prepare) |
  | `PluginRenderFault` had `Crashed` and `TimedOut` | also `LatencyChanged { planned, now }`: a fork whose plugin moved its latency after the fork's graph was compiled reports it (`ForkFaultKind::Failed`) rather than exporting misaligned. A fork's `prepare` first waits for a latency its rate or render-mode change caused (`PluginBridge::settle`, new), so the plan holds it |
  | `PluginHandle::from_client(&PluginClient)` | generic over the state: `from_client(&PluginClient<S>)` |
  | `tutti_plugin::backend::route_with_latency` from `host::node` | the same function, re-exported from `util::node` (only the in-process VST2 node still uses it) |
  | tutti-graph: `Transport` had no recording flag | `Transport::recording()` and `with_recording` (additive; the field is private, like the position); tutti-core's engine sets it from the transport settings, and `Env::transport_at` keeps it |
  | bevy-tutti: `plugin_bind_transport`, `PluginTransportBound` | `plugin_bind_meter`, `PluginMeterBound` (installs `MetronomeRes`'s meter; the transport is the graph's) |
  | bevy-tutti: `CompensatedLatency` | removed: the latency poll compares the plugin's `declared_latency` with the editor's own figure (`AudioGraphRes::node_latency`), which is the record of what PDC was planned against |
  | bevy-tutti: a plugin captured by `CapturedControls::capture(&dyn AudioUnit)` (downcast), `PluginClient` registered with `MidiTargetRegistry` | `CapturedControls::for_plugin(&client)`: the shadow and the MIDI target, typed; `register_plugin_node_types` is removed |
  | bevy-tutti: `crossfade_audio_node(.., Box::new(plugin_client))` | `crossfade_plugin_node(commands, entity, client)`; `AudioGraphRes::{insert_plugin, replace_plugin}` are public, and `ReplaceRefused` is generic over what it hands back (`ReplaceRefused<U = Box<dyn AudioUnit>>`) |
  | bevy-tutti: `AudioGraphRes::inspect` on a plugin node read its `Legacy` shadow | `None`: a plugin node has no shadow; its controls are the entity's `PluginShadow` |

  **A live plugin's latency is now one device block** (plus its own), which
  is how DAWs host out-of-process plugins. The IPC pipeline ships one device
  callback per chunk, so the plugin-server has a whole device period to
  answer. Doc 013's decision 8 had kept a fixed 64-frame chunk; that left
  the server microseconds for every chunk after a callback's first, and live
  plugin audio was 93–99% silent blocks at 441-, 480- and 1024-frame
  callbacks (0% now). The chunk is `Prepare::quantum` when the host knows
  its callback size, else `MaxBlock`, at most 4096 frames; tick mode and
  `TickStorage` are gone from the batcher.

  | Was | Now |
  |---|---|
  | a plugin declared its latency + 64 | + one device callback (live), + the graph's `MaxBlock` (a render, or a host that does not know its callback) |
  | tutti-graph: `Prepare` carried rate and `MaxBlock` | also an optional device quantum: `Prepare::with_quantum`, `Prepare::quantum` (additive) |
  | tutti-cpal opened every stream with the backend's default buffer | a device reporting a buffer range is opened with a fixed 512-frame buffer (`PREFERRED_QUANTUM`, clamped into the range), reported as `OutputSpec::quantum` (new field, `OutputSpec::with_quantum`); a device reporting none keeps its default buffer and `quantum: None` |
  | bevy-tutti prepared the graph for rate and `MaxBlock` | also the opened stream's quantum, at build and again on a device restart (`AudioGraphRes::prepared` reads it) |
  | the plugin-server's shared slab held 64 frames per channel | `MAX_CHUNK` (4096) or the host's smaller `max_buffer_size` |
  | parameter automation sampled a point every 8 frames | every 8 frames up to 10 points a block, wider for a longer block, so a device-callback block never spills a queue to the heap on the audio thread |

  **Fixed: ragged blocks corrupted plugin audio** (also on main before this).
  Each call was submitted as its own chunk and collected into the next
  call's frames, so wherever consecutive lengths differed (an export's
  100-frame blocks; a 441- or 480-frame device quantum rendered in 64-frame
  passes plus a remainder) frames were dropped or zero-padded: 2 303 of
  3 399 samples wrong at 100-frame blocks. The batcher now fills a FIFO and
  ships only whole chunks, reading output from a ring, so a plugin's output
  is its input delayed by exactly the declared latency however the blocks
  are cut. The plugin's MIDI, parameter automation, harmony and
  note expression still arrive through `InputSlot`s (event ports come
  later), so the node still declares `Shape::legacy` and a graph holding it
  is still rendered in 64-frame blocks.

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

- **The audio thread no longer starves a parallel-executor helper on its
  own core** (`tutti-types`, `tutti-core`). Its two waits on a helper
  inside a block, `TaskGraph::work` as `Participant::Caller` and
  `JobGate::run`'s close, spun without bound and never yielded. Under
  `SCHED_FIFO`, which does not time-slice, a helper queued behind the audio
  thread on its core could then not run until the kernel migrated it: a
  same-core probe with both at one FIFO priority stalled until its 2 s
  watchdog in every run. On Linux and Android, each wait now yields its
  core once per 250 µs in which the counter it waits on has not moved, and
  otherwise spins as before. The probe's wait drops to about 1.8 ms, the
  rest of the task. A healthy block never waits that long: the executor
  bench's caller does not yield at all, apart from blocks whose helper was
  off its core. On other platforms the caller only spins, as before:
  Windows' and macOS's yields can hand the core to a lower-priority thread,
  and both schedulers round-robin equal-priority real-time threads. No API
  change. **Hosts:** a `WorkerPool`'s helpers must run at the audio
  callback's priority (`WorkerPoolBuilder::on_start`). A helper below it
  still waits for RT throttling (about 950 ms), because a yield only gives
  way to threads of the caller's own priority (doc 013, "The caller yields
  to a same-core helper").

- **A phaser at a negative depth no longer diverges** (`tutti-nodes`).
  `PhaserNode::set_depth` allows down to −1, which swept the all-pass
  below 0 Hz, past where its coefficient stays inside `-1..=1`; the stages
  grew without bound. The sweep is now held inside `1 Hz..0.49 sr`, which
  every depth in `0..=1` already was.

- **A VST2 plugin's echo of a host write is no longer reported as its own
  edit** (`tutti-vst2-host`). A plugin that calls `audioMasterAutomate`
  from inside `setParameter` with the value it was just given used to
  reach `Vst2Instance::drain_param_changes` (and the plugin-server's
  `ParameterChanged`) as an edit; a host taking reported values as the
  user's, such as `bevy-tutti`'s modulation lanes, adopted each modulated
  value as the knob and ratcheted it to the clamp. A report made on the
  thread inside the host's own `setParameter` call is now dropped; one from
  the plugin's editor thread meanwhile is still reported.

- **The VST3 host no longer allocates on the audio thread for a block of
  more than 73 events** (tutti-vst3-host). The input event list was sorted
  with std's stable sort, which heap-allocates its scratch past 4 KiB (73
  events). The list also grew past its 256 reserved events, and the
  plugin's `addEvent` grew the output list.
  - **Sort.** Both lists are sorted by `tutti_plugin_types::StableOrder`
    (reserved scratch, `n log n`, stable: events on one frame keep source
    order).
  - **Cap.** Both lists hold 528 events (256 MIDI, 256 per-note
    expressions, four of each harmony kind). Past that, events are
    dropped and counted: `Vst3Active::events_dropped` and
    `output_events_dropped` (new). A note-off past the room replaces the
    latest-staged event that is not a note-off, so no note hangs (unless
    the list is full of note-offs). Per-note expression values have their
    own budget of 256, so a dense expression block cannot spend the room
    the harmony kinds were given.
  - **Plugin-visible:** a plugin's `addEvent` past the output list's room
    now returns `kResultFalse` (it used to grow the list).
  - **New API.** `tutti_plugin_types::{insertion_sort_by_key, StableOrder}`
    are the shared allocation-free stable sorts. `PointBuf::sort_by_offset`
    (the VST3 parameter queues' refill and the `IMidiMapping` merge, whose
    room is past 256 points at every chunk size) delegates to
    `insertion_sort_by_key`, which now carries its binary-search insertion,
    so there is one insertion sort. The CLAP input list's `sort_by_time`
    and `tutti-plugin`'s three copies of the MIDI insertion sort use them
    too. Nothing is removed, so there is no migration.

- **A MIDI input driver can no longer push into a freed ring**
  (tutti-midi-hardware). The producer handle was a raw pointer into a ring
  the port manager owned, so an ALSA pump thread still running while a
  session dropped (the manager before the connections) could write through
  a dangling pointer, and two handles to one port were two producers. The
  handle now owns its half of the ring; the session closes its inputs before
  the rest of its state, in a stated order.

- **The sampler no longer frees a decoded wave on the audio thread**
  (tutti-sampler). A pool's `VoiceCommand::ReplaceWave` drain dropped the
  slot's old `Arc<Wave>` in the callback, which freed the whole decoded
  buffer when the slot held its last reference; a `ReplaceWave` aimed at a
  missing or streaming slot dropped its own wave the same way. The wave the
  drain lets go of now leaves through the retirement channel and is freed
  by `VoicePoolHandle::collect_retired`, which now counts it. As before, a
  full retirement channel (a host that never collects) degrades to the free
  in the callback. No API change.

- **Breaking: `VoiceNodeHandle::send` is removed; a standalone `VoiceNode`
  can no longer be sent a command it would free on the audio thread**
  (tutti-sampler). `VoiceNodeHandle::send` accepted
  any `VoiceCommand`, and the node's drain applied `UpdatePlacement` and
  dropped every other one in the callback, freeing what it carried: an
  `AddVoice`'s boxed `Voice`, a `ReplaceWave`'s wave (the last reference
  frees the decoded buffer), an `UpdateStretch`'s filter. The node's queue
  now carries placements only, by type, so such a send does not compile.
  **Breaking:** `VoiceNodeHandle::send` is removed. **Migration:** a
  placement is `VoiceNodeHandle::set_placement(start, duration)` (same
  `SendError`, which still hands back the lost edit as
  `VoiceCommand::UpdatePlacement`); gain is `set_gain`. The other commands
  never took effect on a node: a host changing a node's wave, speed or
  pitch respawns it, or uses a `VoicePool`.

- **The CLAP host no longer drops and regrows the plugin's output
  parameter queues on the audio thread** (tutti-clap-host).
  `OutputEventList::fill_param_changes` emptied its set with
  `queues.clear()`, dropping every queue each block, and built a new one
  per parameter: a plugin emitting more than ten points for one parameter
  in a block allocated and freed on the audio thread (and, before the
  parameter-change rework, more than sixteen parameters did too). It now
  empties the set with `ParameterChanges::clear` and takes each queue with
  `open_queue`, so the queues keep their storage. Activation (and a
  block-size grow) reserves the output set with
  `ParameterChanges::reserved`: one queue per parameter, at least 16, each
  with `ParameterQueue::chunk_room(max_frames)` points (about 0.3 KB per
  parameter at 512 frames, 2 KB at 4096). Past that room nothing grows: a
  point for a full queue replaces its last one, so the parameter still ends
  the block on its latest value, a point for a parameter with no queue left
  is lost, and both are counted.
  - Added `ClapActive::output_param_points_dropped() -> u64`.
    `OutputEventList::fill_param_changes` returns the points it dropped
    (`u64`; it returned `()`).
  - **Behaviour:** `fill_param_changes` fills only within the room its
    `ParameterChanges` was reserved with, so a set built with `new()` or
    `default()` now receives nothing. **Migration:** build it once, off the
    audio thread, with `ParameterChanges::reserved(queues, points)`; or call
    `to_param_changes()` off the audio thread, which still keeps every
    point.
  - Fixed: the `fill_midi_events` and `fill_note_expressions` docs called
    their buffers pooled `SmallVec`s; they are fixed-capacity `RtVec`s.
  - `fill_param_changes`' returned count is `#[must_use]`.

- **The CLAP plugin's `try_push` no longer grows the output event list on
  the audio thread, and a parameter flood no longer starves note events**
  (tutti-clap-host). `try_push` pushed every event unconditionally into a
  list reserved for 256, so a plugin pushing more in one block made the
  host allocate inside the plugin's `process`. The list now has two
  separate rooms, both reserved off the audio thread at activation:
  - **Note, MIDI and other non-parameter events** get their own 256
    (`OutputEventList::reserve(n)`). A parameter flood cannot use it, so
    note-offs still arrive. Past it `try_push` returns `false` (CLAP lets
    it fail; a plugin must handle that) and counts the refusal
    (`OutputEventList::events_refused`, `ClapActive::output_events_refused`).
  - **`PARAM_VALUE` events** get every parameter's
    `ParameterQueue::chunk_room(max_frames)` points (at least 16
    parameters), set with the new `OutputEventList::set_param_room(points,
    ids)`. Past that room a point is folded into a preallocated
    latest-value table keyed by the plugin's sorted parameter ids (a binary
    search, no scan), so each parameter still ends the block on its latest
    value; `fill_param_changes` counts the points that replaced in the
    table as dropped. Only a point for an id the plugin never declared is
    refused. The room is grow-only: a smaller block keeps the larger room.
  - **Memory:** the output side is about 16.9 MB of event list plus about
    4.6 MB of `out_param_changes`, about 21.5 MB, at 2 000 parameters and a
    4096-frame block, and about 172 MB at 16 000 parameters, on the
    plugin-server side, beside the input list's figure (design doc 014
    §12). The arena follow-up there covers both sides.
  - **Behaviour:** `reserve(n)` now bounds the non-parameter events to `n`
    (it reserved `n` more); a list never reserved is unbounded, as before.
    A list never given a parameter room is unbounded on that side too.

- **Two audio-thread reads could free on the audio thread** (design doc
  013, "nothing the audio thread reads is an `ArcSwap`"). An `ArcSwap`
  guard held while a control thread stores is settled into a strong count,
  and its drop can then be the last one. Both cells are now `RtPublish`,
  whose read only borrows:
  - `tutti-midi-hardware`'s `HardwareMidiInputs`: a hotplug
    (`create_input_port`) during the per-block drain could free the old
    port list in the callback. Two concurrent `create_input_port` calls
    could also lose one of the ports (load, clone and store was not one
    step); creation is now serialized, and each caller's port lands at the
    index it is handed.
  - `tutti-plugin`'s `PluginMeter`: a `set_meter` or `clear_meter` during
    playback, replacing a meter cell the host did not keep, freed the whole
    cell in the callback.

  No public signature changed. `PluginControls::set_tail` and
  `PluginClient::set_tail` take a short lock now (the tail is a `Mutex`; no
  audio thread reads it), and their docs no longer claim they are RT-safe:
  the old store allocated. `arc-swap` is no longer a dependency of any
  engine crate (only `tutti-types`' bench baseline keeps it).

- **A live in-process VST2 plugin behind a latent path was told the wrong
  playhead** (tutti-plugin; design doc 013, "In-process VST2 live transport
  from `Cx` landed"). The live node polled the installed `Transport` once
  per block, so `audioMasterGetTime` answered with the block's own time,
  not the time its input audio was rendered at. It now reads its block's
  `Cx::transport_at`, as its fork and the out-of-process plugin node do, so
  live and fork are one path. With no meter installed the plugin is now told
  the graph's transport in 4/4 (it was told a stopped default until a
  reader was installed). bevy-tutti now gives an in-process VST2 the
  project meter, as it does an out-of-process plugin: it read 4/4 in a 7/8
  project. **Breaking** for code that called the in-process
  client's transport install directly.

  | Was | Now |
  |---|---|
  | `InProcessVst2Client::set_transport_source(reader, meter)`, `clear_transport_source()` | the transport needs no install. Give the meter: `InProcessVst2Client::{set_meter(meter), clear_meter(), has_meter(), meter()}` (`&self`; shared across the node's clones) |
  | `IntoNode for InProcessVst2Client`: `Controls = ()` | `Controls = PluginMeter` (new, `tutti_plugin::handles::PluginMeter`: `set_meter`, `clear_meter`, `has_meter`; the node's meter cell). `GraphBuilder::add(client)` no longer compiles: use `add_with_controls`. `PluginControls::meter()` hands the out-of-process node's cell as the same type |
  | `Plugin::set_transport_source(reader, meter)` (`reader` unused by both backends) | **deprecated**: `Plugin::set_meter(meter) -> bool` (`&self`), `Plugin::clear_meter()`, `Plugin::meter() -> PluginMeter` |
  | bevy-tutti: an in-process VST2 was never given the project meter (no `PluginShadow`) | captured with `CapturedControls::for_plugin_meter`; `PluginShadow::meter_only`, `PluginShadow::meter_for`; `controls_for` is `None` for an in-process VST2 |

- **A live disk voice on a looped stream played 576 frames and then
  silence, and every live reposition cost frames and drift** (tutti-sampler;
  design doc 013, "The live disk loop and its repositions (#48)"). The
  butler compared the reader's consumed-frame count against the loop's file
  frames and flushed the ring on every cycle past the loop's end; its RT loop
  crossfade replayed the tail it faded out; and every reposition (a seek, a
  PDC change) flushed a FIFO ring, which dropped what it had just refilled,
  replayed its fade-in, restarted the reader from a zero history, and left
  the voice further behind its clock each time. The streaming tier's ring is
  now **indexed by position**:
  - The butler writes what each straight position holds under the stream's
    mapping — the file, a loop's sequence (fade blended in, wrapping to
    `resume`), or the file mirrored in reverse — and keeps the window filled
    ahead of where the reader says it plays. Nothing is flushed.
  - A `DiskVoice` seats on the clock exactly as the memory tier does and
    reads its taps from the ring by position through the memory tier's tap
    layout, so it plays the memory tier's samples at the same clock frame, bit
    for bit, at any rate (a 44.1 kHz file at 48 kHz included). The 2-frame
    lag the old reader's history added at unity is gone.
  - A seek, a transport loop, a varispeed change or a PDC change costs no
    frame and never drifts: the reader plays on from a rendered continuation
    of what it was playing while the butler moves the window, then crossfades
    to the new position (the seek crossfade,
    `BufferConfig::seek_crossfade_frames`). Fades chain without a step, a
    continuation that runs out ramps out, and the ring ramps back in after an
    underrun. A PDC change now moves from where the reader plays.
  - A loop or direction edit switches the ring where the old and new
    mappings part — exactly there when that is ahead of the reader (heard as
    on the memory tier), otherwise just past the block the reader is in,
    crossfaded from a record of the old loop. Setting the same loop again, or
    one that changes nothing the ring holds, is free. Reverse ignores the loop
    and mirrors the file, as on the memory tier and a fork; past the file's end
    both are silent.
  - A loop change reads only its fade's lead-in (and a short loop's body,
    kept so a streamed refill does not seek once per wrap), never the whole
    file, and never while holding the plan map. A lead-in that cannot be read
    plays the loop hard, is logged, and is what a fork of the stream plays too.
  - The wave cache refuses a single wave larger than its whole byte budget;
    a stream of a format that cannot seek holds its file itself.
  - **`Status::take_disk_voice` returns `Result<DiskVoice, TakeVoiceError>`**
    (was `Option`): `NotStreaming` where it returned `None`, and
    `ReaderTaken` for a second voice on one stream — a stream serves one live
    voice. `Command::Seek` moves a free-running reader only; a placed voice
    follows its clock.
  - **tutti-types / tutti-core: `PosRing`**, the position-indexed ring the
    stream's reader and the butler share (one writer, one reader, a window of
    positions, reads by position that no write ever lands under), with a loom
    model of its protocol (`tests/pos_ring_loom.rs`, in CI's loom job).
    `PosRing::new` returns its one `PosWriter` and one `PosReader`; a read
    goes through the `PosClaim` a claim returns, and a reader that reads
    nothing this block calls `PosReader::idle`.
  - The live read keys a jump on the clock's `segment_generation` (a
    free-running source's seek epoch) as well as on its position, so a seek
    to where the clock already stands is crossfaded as a jump.
  - Removed (all crate-private but one): `DiskVoice::seek` — a placed voice
    follows its clock, so seek the clock; `Command::Seek` still moves a
    free-running reader. Also `LoopStatus`, the ring flush and seek-request
    epochs, the seeking flag, the seek and loop crossfaders, and the reader's
    consumed-frame counter.

- **tutti-graph: an event PDC delay no longer refuses input against room
  its own due backlog is about to free.** `EventFifo::run` queued the
  block's input before emitting what was already due, so a FIFO a
  recompile retuned shorter (its backlog all due at once) refused new
  events past its limit that the same block would have delivered — the
  executor dropped an event the reference interpreter delivered
  (`differential::recompiles_preserve_state_identically`, seed
  3280887136571273968, which = 3, failing on main). The backlog now goes
  out first. Kept as the plain test
  `recompile_regression_3280887136571273968` (the regressions file stores
  proptest's RNG seeds, not generated values) and pinned in
  `kernels::tests::a_due_backlog_makes_room_for_the_blocks_input`.

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
  - **Not fixed here: a live disk voice on a looped stream.** Fixed since:
    the entry above.

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

- **MIDI on event ports, first part (doc 013 item 5): `MidiClipNode`, and
  `PolySynth` as a native node.**
  - `tutti_midi_runtime::MidiClipNode`: a clip as a graph node with one MIDI
    event output (`CLIP_EVENT_CAPACITY` per block). It places each event on
    the frame playback reaches its beat in the block's `Env`, segment by
    segment, so a seek, a loop wrap or a tempo change inside a block lands
    exactly, and it keeps no play cursor: an offline fork plays the clip on
    its render's `Env` with nothing rebound. It ends the notes it started
    when playback stops, jumps (a seek, a loop wrap) or the clip is replaced.
    Its controls, `MidiClipControls`, replace the clip (`set_events`,
    `clear`) through `RtPublish`; a fork plays the clip as it was when forked.
  - `PolySynth` implements `Node` and `IntoNode` (controls `()`, a native
    fork source): stereo out, one MIDI event input, the release as its tail.
    Wire a clip to it with `GraphSpec::connect_events`; its note sounds on
    its frame in the block the clip writes it. The synth's own MIDI port
    (`midi_sender`, an installed source) is still read each block and merged
    with the event input by offset, the port's first at an equal offset.
    Inserting the synth through `Legacy` (bevy-tutti's path) is unchanged.
  - tutti-midi-runtime depends on tutti-graph (was a dev-dependency).
  - A hosted plugin's node declares a MIDI event input: what reaches it is
    sent with the chunk its frames go into, on its frame, beside its own
    port's MIDI (`Chunks::take`).
  - bevy-tutti: `SpawnGraphNode::spawn_graph_node` inserts a node that brings
    its own `IntoNode` (a `MidiClipNode`, a `PolySynth` as a graph node),
    keeping its controls on the entity as `NodeControls<C>` and capturing a
    synth's MIDI port as before (`GraphNode::captured`,
    `CapturedControls::for_midi_port`). `EventSources` on a sink entity
    declares what feeds its event input (a list: event inputs fan in);
    `GraphEventsPlugin` (part of `GraphReconcilePlugin`) writes it into the
    graph. `AudioGraphRes::{insert_node, set_event_sources, event_sources,
    node_event_inputs}` are the imperative forms.
  - A hosted plugin's node no longer declares `Shape::legacy`: a plan
    holding plugins and graph nodes renders whole blocks instead of 64-frame
    passes. Its timeline-polling inputs are read when a chunk begins and
    re-based to it, which is right in either mode.
  - tutti-soundfont: `SoundFontUnit: Node + IntoNode` (stereo out, one MIDI
    event input at `Resolution::Frames(8)`, a graph-node fork). As a node it
    follows its graph's rate: `prepare` rebuilds the synthesizer at the
    prepared rate, keeping preset and port, so a device restart at a new rate
    no longer leaves it playing at the old one. bevy-tutti's SoundFont
    promotion inserts it this way.
  - tutti-midi-runtime: `MidiInPort::gather`, the port's events merged with a
    node's event input by offset (the port's first on a tie), for a node that
    reads both; the synth and the SoundFont node use it.
  - A hosted plugin that declares `Features::MIDI_OUT` has a MIDI event
    output carrying its MIDI-out where its audio output plays the frame it
    was emitted at. The reply is taken with its chunk's audio
    (`PluginBridge::take_replies`; `submit` no longer takes a MIDI-out
    buffer), and an export waits for it. The reference CLAP plugin's `Notes`
    mode echoes the notes it receives.
  - **Breaking:** a hosted plugin's parameter automation is an event source
    node. `PluginControls::automation(params)` / `PluginClient::automation` /
    `Plugin::automation` make a `PluginAutomation` (controls:
    `AutomationControls::{set_params, clear}`) to insert and wire to the
    plugin node's event input; `set_param_automation_source`,
    `clear_param_automation_source`, `has_param_automation_source` and
    `ParamAutomationSource` are removed. An address of the other model than
    the plugin's is refused. bevy-tutti: `plugin_bind_params` keeps a
    `PluginAutomationNode` per plugin entity; `EventFeeds` is keyed by feeder
    (`set`, `remove`, `nodes`).
  - tutti-graph: `EventKind::Harmony(Harmony)` (`Harmony::{chord, scale}`,
    `HarmonyKind`, `Event::harmony`): a chord or scale taking effect.
  - tutti-midi-runtime: `HarmonyNode` (`HarmonyControls::{set, clear}`,
    `TimedHarmony`) sends chord and scale lanes on their frames and
    re-states the context in force wherever playback jumps. The clip node's
    playback walk is shared with it.
  - **Breaking:** a hosted plugin takes chords and scales on its event input
    (for a plugin that takes sequencer context). `HarmonySource`,
    `TimedChord`, `TimedScale`, `set_harmony_source`, `clear_harmony_source`,
    `HarmonyView`, `NoteExpressionSource`, `set_note_expression_source`,
    `clear_note_expression_source`, `NoteExpressionView` and
    `Plugin::{set_harmony_source, set_note_expression_source}` are removed;
    `PluginClient::takes_harmony` / `Plugin::takes_harmony` answer whether to
    wire a `HarmonyNode`.
  - bevy-tutti: a `MidiSourceInstall` whose target has an event input (a
    graph-node synth, a hosted plugin) plays through a `MidiClipNode` of its
    own wired to it (`SequencedClips`, `EventFeeds`), edited in place; a
    target inserted as an `AudioUnit` still gets a `MidiClipSource` on its
    port.

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
