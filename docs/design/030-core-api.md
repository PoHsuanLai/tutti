# 030: the core API, from the user's side

This document lists the friction a user meets when writing an app against
tutti's core surfaces through the `tutti` umbrella, and orders the lanes that
remove it. The surfaces are the session, the device, the graph, the transport,
tempo, modulation, the sampler, the codec, export, and writing a custom node.
[Doc 029](029-plugin-api.md) does the same for plugin hosting; the MIDI
session's plan (lanes L1e to L1h) does it for MIDI. The three share one set of
conventions (§2).

Status: inventory and plan; nothing in §4 is implemented. Written 2026-11-12
against `main` at `f583038f6`, from three read-only sweeps whose full findings
(each with today's code, the friction, the fix and a size) are kept outside the
repo in `~/tutti-audit/api-inventory/`. Already landed or in flight, and not
repeated here:

- #629: `Bounce` is typed by its master, so the result is `Written` or
  `Rendered`. A cancel is `BounceError::Cancelled`, and the transport's fields
  are behind `settings()` and `motion()`.
- #632:
  - `Graph::connect` and `chain`;
  - the session transport verbs and `schedule(impl Into<At>, MotionEvent)`;
  - the `Clip` builder;
  - one `tutti::Error`;
  - a feature-gated prelude;
  - `Range` from `a..b`;
  - `DecodeError` without a feature;
  - the bounce policies renamed off `Latency`/`Tail`;
  - `#[must_use]` on `update()`'s events.
- Doc 013, "The session graph's surface": typed `Key<N>`, `OutPort`/`InPort`,
  one wiring vocabulary, the `taps()`/`meters()` facets, plumbing made
  `pub(crate)`, and one failure rule.

## 1. What a user writes today

An app that loads a sample, plays it through a filter and reverb with an LFO on
the cutoff, and bounces it is about 40 lines after #632 (its
`examples/song_bounce.rs`). A live app adds a device, a mic and a playhead, and
there the friction is worst:

- **No timeline position for a take.** A recording carries none, so placing it
  under the music is guesswork.
- **A torn, stepped playhead.** It is two reads that can disagree, and it moves
  in block-sized steps.
- **A stream that stops silently.** `let (s, _) = build_device(..)` drops the
  `Device`, and the output goes quiet.
- **No JACK through the umbrella.** Every device and mic opener hard-wires the
  default host.
- **A recorder that hangs.** `Recorder::wait()` never returns on a live source.

Writing a custom node takes 11 lines for a fixed gain and about 30 for a
modulatable one. A param'd node lists its params four times, and forgetting one
of them (`param_base`) silently disables modulation.

## 2. Conventions (shared with doc 029 and the MIDI plan)

1. **Events nest by area**, pulled from `update()`: `SessionEvent::Plugin {
   node, event }`, `SessionEvent::Midi(MidiEvent)`, and core's own as it
   touches them. Both levels are `#[non_exhaustive]`. There are no callbacks;
   where a push is truly needed, `subscribe(f) -> Subscription` (dropped to
   unsubscribe).
2. **One error variant per area** in `tutti::Error`, with the specific errors
   as sources and `From` for each. Event payloads carry typed errors, never a
   `String`.
3. **Typed keys.** `Key<N>` gives a node's ports and its native params as
   methods returning `ParamRef`. A plugin's key gives `param(ParamId) ->
   ParamRef`. A MIDI route targets a `Key<N>`.
4. **Borrowed views** for per-item operations (`session.plugin(key) ->
   Plugin<'_>`). Owned handles a host keeps are only for audio-thread or
   cross-thread control, and they must say so.
5. **Blocking jobs go through the session's spawner**
   (`SessionBuilder::spawn_with`, default `std::thread`) and land in
   `update()`. The bounce moves onto it after the plugins session adds it.
6. **The prelude holds the common path**, gated per feature. Low-layer and
   tuning types are reached by path.
7. **Types over checks** (CLAUDE.md): a value that can be wrong is refused
   where it is made, with `new -> Option` or `TryFrom` and a reason. A
   setter returns `Result` when its target can be absent, and nothing
   silently clamps, falls back or ignores.

## 3. Findings

Grouped by surface; the full text of each is in the inventory. **Bug** marks a
finding where the API produces wrong results today, not only friction.

### 3.1 Session and device

| # | finding | fix | size |
|---|---|---|---|
| S1 | `build_device` and every `MicIn::open*` hard-wire `AudioHost::Default`, so JACK is unreachable. A rate-changing restart leaves the host to redo the mic, the inlet and the wiring. `add_inlet(ring, rate)` passes the rate apart from the ring | `SessionBuilder::host(..)`; a `MicConfig` (`Default` plus struct update); a ring that carries its rate; `Session::add_mic`, which the restart reopens | M-L |
| S2 | Restart is spelled four ways (`restart`, `restart_following`, `restart_wanted`, `restart_on`), with a separate `&mut Device` any session accepts. `watch_devices` takes `want`/`running_on` by hand. Events carry `String` errors | one `DeviceRestart` value with a `follow` hook; `watch_devices` reads the device; typed errors in events | M |
| S3 | **Bug**: `let (s, _) = build_device(..)` drops the `Device` and silences the stream. `#[must_use]` does not fire on a destructure | return a named `#[must_use]` struct (`LiveSession { session, device }`) that also pairs the device with its session for S2 | S |
| S4 | `InstrumentHandle` and `LiveInput` duplicate `note_on`/`note_off`/`send`/`send_at`/`clock`/`dropped` over `SharedKeys`. `note_on` takes a bare `u16` velocity | one shared keys handle (with the MIDI plan's `MidiKeys`), and a velocity type | S-M |
| S5 | **Bug**: `Recorder::wait()` hangs forever on a live source. There are three ways to finish, and `Drop` only logs a sink error | sources say finite or live, and `wait` exists only for finite ones; `#[must_use]`; one finish path with one error shape | M |
| S6 | `SessionBuilder::inputs`, `outputs_floor` and `render_workers` take a bare `usize` (0 workers means "one per core"). `build_device_on` is a test seam in the public API | `ChannelLayout`; a `Workers` enum; hide the seam behind `testing` | S |
| S7 | Removal is spelled `remove`, `remove_instrument`, `GeneralMidi::remove`, `unroute` and `close_*`, returning `bool` or `()`. `insert` against `add` is a trap (`insert` skips the reclaim sweep). `open_event_inlet` panics on a bad capacity | one `remove(key)` on typed keys, returning `Result`; `insert` folded into `add`; a checked capacity | S-M |
| S8 | The heard tap's `Backpressure` is a transient error mixed in with permanent ones. `LoadMeter::take` is `Clone`, so two readers split the peaks. tutti-cpal lists devices five ways | split the error; one reader, not `Clone`; one listing | S |

### 3.2 Transport, tempo, modulation

| # | finding | fix | size |
|---|---|---|---|
| T1 | **Bug**: a route onto a target with no `range()` is accepted. The next apply refuses with `CompileError::UndeclaredTarget` and freezes *all* modulation, reported only by an event | the range is part of the route: `route(src, target.within(lo..hi), depth)`, or a typestate target that has a range | M-L |
| T2 | `Mods::range`, `Take::record`, `Trajectory::with_range` and `Graph::set_param` take bare `f32` for params that have units. Min > max is not refused | unit-typed targets from `Key<N>` param methods; a checked range | M |
| T3 | `Graph::set_param` returns `()` and ignores a missing node or param, although `ParamSet::set` returns `bool`. `set_node_params` is plumbing a user can forget | `Result` and `#[must_use]`; the plumbing made `pub(crate)` | S-M |
| T4 | A route cannot be found again. `RouteKey`/`SourceKey` are opaque with public `u64`s, there is no `routes()` or `routes_to(target)`, and edits are `route_mut` plus public fields. `route` hard-codes polarity and the curve | private minted keys; iterators; a route builder (`.polarity(..).curve(Law::..)`) | M |
| T5 | "Why is nothing modulating" has three channels (`ModState::refusal`, `unresolved_*`, a `SessionEvent`), and the errors name opaque keys | one `problems()` iterator whose `Display` names the node and the param | M |
| T6 | `TempoMap` has 8 queries, each doubled by a `_with(&mut Cursor)` twin, and `rate` is passed on every call. `TempoPoint` has no constructors. `tempo::Shape` clashes with `graph::Shape` | a `TempoView` holding rate and cursor; `TempoPoint::{step, ramp_time, ramp_beats}`; a rename; a builder where a trailing ramp cannot be written | M |
| T7 | **Bug**: `Law::PowerIn/PowerOut/PowerInOut/Exp(f32)` silently fall back to linear, or clamp, on NaN, p <= 0 or \|k\| > 64 | `Exponent` and `Curvature` range-checked types (`new -> Option`) | S-M |
| T8 | `LoopRange::new` returns `Option` with no reason, and the session has no loop call | `TryFrom<Range<Beat>>` with a reason; `Session::set_loop` | S |
| T9 | `Depth` has a public field, and `-1..1` is unchecked. `ProjectMeter::set_meter`/`ClickSettings::set_meter` take a bare `Arc` where `set_tempo_map` takes `Into`. `OfflineTimeline::advance` takes `usize`. `in_preroll` is a public cell. Click volume clamps. `Lfo::rate` takes beats only. `Mods::record` returns an unmarked `bool`. `Session::mods()` marks the session edited even for a read | each to the conventions; a read view `mods()` and an edit `mods_mut()` | S |
| T10 | Name clashes: `graph::Transport` and `transport::Transport`, modulation `Wave` and codec `Wave`, `UnitParam::Depth` and `Depth` | rename the less used side (`TransportPosition`, `Waveform`) | S |

### 3.3 Sampler, codec, export, custom nodes

| # | finding | fix | size |
|---|---|---|---|
| N1 | A param'd node lists its params four times (`with_params`, `param_base`, `Io::param(k)`, `ParamSet`), and forgetting `param_base` silently disables modulation | one declaration the others are derived from; a derive (a new proc-macro crate) if a value cannot do it | M |
| N2 | A user's `ParamNode` needs a hand-written `SessionNode` before `Graph::add` takes it (the macro the built-in nodes use is private) | a `Params(node)` wrapper, beside `ForkByClone` and `Unforkable` | S |
| N3 | `Clone` means opposite things to `ForkByClone` (a fork) and to `param_parts` (a shared cell), so `ForkByClone(param'd node)` compiles and forks onto the live cell | `Params(node)` (N2) is the only way in for a param'd node; refuse the other by type | M |
| N4 | `Shape`, `Io::channel(c).map(..)` and the shortest node bodies are not in the docs | a node-writing guide in `tutti-graph`'s crate docs | S |
| A1 | `VoicePoolHandle` is only `send(VoiceCommand::..)`. `AddVoice` on a disk voice silently drops its loop and direction. The `stretch` argument is never `Some` | handle methods; a streaming voice type; the dead argument removed | M |
| A2 | `VoiceNodeHandle` sets only gain and placement, and the caller must keep the disk `Stream` alive or the voice goes silent | loop, speed and play/stop on the handle; the handle owns the `Stream` | M |
| A3 | Five loop setters, `crossfade_frames: usize`, a public unchecked `LoopSetting::On`, and the checked `LoopSpan` hidden | one checked `Loop` value in `Samples` | M |
| A4 | Disk voices take `Beat` plus `Option<BeatDuration>` where memory voices take `VoiceWindow`. `tutti_sampler::Source` and `Status` are unused and collide with other crates' names | one window type; delete the unused | S |
| A5 | `DiskStreamer::open` can only be polled in a sleep loop. `take_disk_streamer` and the detached session | a blocking open on the spawner, landing in `update()`; the take kept until `bevy-tutti` un-parks | S |
| A6 | `Clip` (from #632) covers memory only | `Clip` also streams from disk past a size threshold, or with `.streamed()` | M |
| C1 | **Bug**: `Wave::read` silently assumes 44.1 kHz when the container has no rate | refuse with `DecodeError`, or take the rate as an argument | S |
| C2 | Saving a `Wave` takes a hand-built `Spec` and hand interleaving, and `Rendered` and `Wave` do not convert | `Wave::write(path, format)`, `layout`, `interleaved`, and `From<Rendered> for Wave` | M |
| C3 | `Spec` holds a `layout` and an optional `topology` that must agree. Codec constructors disagree on argument order and error type | `Spec` from a topology alone; one constructor shape | S |
| C4 | "Finite positive rate" is re-checked in four places and skipped in two. `Wave` counts in `usize` | check once at `SampleRate` construction if the unit rule allows (a distinct range); `Samples` in `Wave` | L |
| E1 | `Bounce` takes `format` at runtime (`Missing(Format)`) and accepts file-only knobs (`NormalizeBuffers`) on a buffer master | `master(path, format)`; the file knobs only on `Bounce<_, FileMaster>` | M |
| E2 | Stem accessors consume `self`, so `done.stems()[0].file()` does not compile | borrowing accessors and `into_*` for the owned ones | S |

### 3.4 Live recording and the playhead (owner-approved 2026-11-12)

| # | finding | fix | size |
|---|---|---|---|
| R1 | A take carries no position on the timeline, input and output latency are not compensated, and a recording starts on wall time | `s.record(input, At) -> Take`: the start frame and beat, the compensation, punch in and out through `At`, with monitoring and recording opened in one call | L |
| R2 | The playhead is two atomics (`beat()`, `is_playing()`) that can disagree, and it moves in block steps | one `s.playhead() -> Playhead { beat, frame, seconds, playing, at }` (a sequence lock), and `beat_now(Instant)` extrapolated from the block clock | S-M |

## 4. Lanes, in order

1. **Bugs first** (S each, one PR): S3, S5 (the `wait` hang), T1's freeze
   (until the typed fix, an undeclared range is refused when the route is
   made), T7, C1.
2. **Graph surface 1** (task #58, doc 013): typed `Key<N>`, `OutPort`/`InPort`,
   one wiring vocabulary, no `Silence`. Folds in T2, T3, N2, N3 and S7's
   `remove`. The plugins session's view (doc 029 step 2) builds on its
   branch.
3. **Node authoring**: N1 (one param declaration), N4 (the guide).
4. **Modulation**: T1's typed range, T4, T5, T9's `Depth` and `mods()`/`mods_mut()`.
5. **Graph surface 2** (task #59): facets, plumbing, the failure rule; S8.
6. **Playhead**: R2.
7. **Device**: S1, S2, S6, and typed event errors under convention 2.
8. **Takes**: R1 (a short design first: it spans cpal, session and
   transport).
9. **Sampler**: A1 to A6.
10. **Codec and export**: C2, C3, E1, E2; C4 last, since it touches every rate.
11. **Tempo and names**: T6, T8, T10.
12. **Instruments**: S4, with the MIDI session's keys handle.

The parked `bevy-tutti` is not updated by any lane; un-parking ports it.

## 5. Rejected

- **Operator wiring** (`a >> b`): it hides the summing and bus rules the graph
  enforces.
- **Unit literal extensions** (`120.bpm()`): the unit rule names the quantity
  where the number is written, and `Bpm(120.0)` does.
- **Commit on drop** for an edit batch: it hides work in `Drop` and cannot
  report an error. `update()` stays explicit, and a bounce reads the spec as it
  stands (#632).
- **Default `reset` on `Node`**: it would hide a fork that keeps state.
- **`Result` wrappers on `Session::update` and a main-thread token on it**:
  `update` cannot fail as a whole, and its events carry what went wrong.
