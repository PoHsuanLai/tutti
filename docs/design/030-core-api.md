# 030: the core API, from the user's side

This document lists the friction a user meets when writing an app against
tutti's core surfaces through the `tutti` umbrella, and orders the lanes that
remove it. The surfaces are the session, the device, the graph, the transport,
tempo, modulation, the sampler, the codec, export, and writing a custom node.
[Doc 029](029-plugin-api.md) does the same for plugin hosting; the MIDI
session's plan (lanes L1e to L1h) does it for MIDI. The three share one set of
conventions (§2).

Status: inventory, two designs (§4, §5) and the one lane (§6); the lane is built through step 12. Written 2026-11-12
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
| S1 | `build_device` and every `MicIn::open*` hard-wire `AudioHost::Default`, so JACK is unreachable. A rate-changing restart leaves the host to redo the mic, the inlet and the wiring. `add_inlet(ring, rate)` passes the rate apart from the ring | `SessionBuilder::host(..)`; a `MicConfig` (`Default` plus struct update); a ring that carries its rate; `Session::add_input`, which the restart reopens | M-L (built, step 7) |
| S2 | Restart is spelled four ways (`restart`, `restart_following`, `restart_wanted`, `restart_on`), with a separate `&mut Device` any session accepts. `watch_devices` takes `want`/`running_on` by hand. Events carry `String` errors | one `DeviceRestart` value with a `follow` hook; `watch_devices` reads the device; typed errors in events | M (built, step 7) |
| S3 | **Bug**: `let (s, _) = build_device(..)` drops the `Device` and silences the stream. `#[must_use]` does not fire on a destructure | return a named `#[must_use]` struct (`LiveSession { session, device }`) that also pairs the device with its session for S2 | S |
| S4 | `InstrumentHandle` and `LiveInput` duplicate `note_on`/`note_off`/`send`/`send_at`/`clock`/`dropped` over `SharedKeys`. `note_on` takes a bare `u16` velocity | one shared keys handle (with the MIDI plan's `MidiKeys`), and a velocity type | S-M (built, step 12) |
| S5 | **Bug**: `Recorder::wait()` hangs forever on a live source. There are three ways to finish, and `Drop` only logs a sink error | sources say finite or live, and `wait` exists only for finite ones; `#[must_use]`; one finish path with one error shape | M (built: the kinds in #636, step 8's `Drop`) |
| S6 | `SessionBuilder::inputs`, `outputs_floor` and `render_workers` take a bare `usize` (0 workers means "one per core"). `build_device_on` is a test seam in the public API | `ChannelLayout`; a `Workers` enum; hide the seam behind `testing` | S (built, step 7) |
| S7 | Removal is spelled `remove`, `remove_instrument`, `GeneralMidi::remove`, `unroute` and `close_*`, returning `bool` or `()`. `insert` against `add` is a trap (`insert` skips the reclaim sweep). `open_event_inlet` panics on a bad capacity | one `remove(key)` on typed keys, returning `Result`; `insert` folded into `add`; a checked capacity | S-M (built, 2d) |
| S8 | The heard tap's `Backpressure` is a transient error mixed in with permanent ones. `LoadMeter::take` is `Clone`, so two readers split the peaks. tutti-cpal lists devices five ways | split the error; one reader, not `Clone`; one listing (built, §6 item 5) | S |

### 3.2 Transport, tempo, modulation

| # | finding | fix | size |
|---|---|---|---|
| T1 | **Bug**: a route onto a target with no `range()` is accepted. The next apply refuses with `CompileError::UndeclaredTarget` and freezes *all* modulation, reported only by an event | the range is part of the route: `route(src, target.within(lo..hi), depth)`, or a typestate target that has a range | M-L |
| T2 | `Mods::range`, `AutomationTake::record`, `Trajectory::with_range` and `Graph::set_param` take bare `f32` for params that have units. Min > max is not refused | unit-typed targets from `Key<N>` param methods; a checked range | M |
| T3 | `Graph::set_param` returns `()` and ignores a missing node or param, although `ParamSet::set` returns `bool`. `set_node_params` is plumbing a user can forget | `Result` and `#[must_use]`; the plumbing made `pub(crate)` | S-M |
| T4 | A route cannot be found again. `RouteKey`/`SourceKey` are opaque with public `u64`s, there is no `routes()` or `routes_to(target)`, and edits are `route_mut` plus public fields. `route` hard-codes polarity and the curve | private minted keys; iterators; a route builder (`.polarity(..).curve(Law::..)`) | M (built, step 4) |
| T5 | "Why is nothing modulating" has three channels (`ModState::refusal`, `unresolved_*`, a `SessionEvent`), and the errors name opaque keys | one `problems()` iterator whose `Display` names the node and the param | M (built, step 4) |
| T6 | `TempoMap` has 8 queries, each doubled by a `_with(&mut Cursor)` twin, and `rate` is passed on every call. `TempoPoint` has no constructors. `tempo::Shape` clashes with `graph::Shape` | a `TempoView` holding rate and cursor; `TempoPoint::{step, ramp_time, ramp_beats}`; a rename; a builder where a trailing ramp cannot be written | M (built, step 11) |
| T7 | **Bug**: `Law::PowerIn/PowerOut/PowerInOut/Exp(f32)` silently fall back to linear, or clamp, on NaN, p <= 0 or \|k\| > 64 | `Exponent` and `Curvature` range-checked types (`new -> Option`) | S-M |
| T8 | `LoopRange::new` returns `Option` with no reason, and the session has no loop call | `TryFrom<Range<Beat>>` with a reason; `Session::set_loop` | S (built, step 11) |
| T9 | `Depth` has a public field, and `-1..1` is unchecked. `ProjectMeter::set_meter`/`ClickSettings::set_meter` take a bare `Arc` where `set_tempo_map` takes `Into`. `OfflineTimeline::advance` takes `usize`. `in_preroll` is a public cell. Click volume clamps. `Lfo::rate` takes beats only. `Mods::record` returns an unmarked `bool`. `Session::mods()` marks the session edited even for a read | each to the conventions; a read view `mods()` and an edit `mods_mut()` | S (built, step 4) |
| T10 | Name clashes: `graph::Transport` and `transport::Transport`, modulation `Wave` and codec `Wave`, `UnitParam::Depth` and `Depth` | rename the less used side (`TransportPosition`, `Waveform`) | S (built, step 11) |

### 3.3 Sampler, codec, export, custom nodes

| # | finding | fix | size |
|---|---|---|---|
| N1 | A param'd node lists its params four times (`with_params`, `param_base`, `Io::param(k)`, `ParamSet`), and forgetting `param_base` silently disables modulation | one declaration the others are derived from; a derive (a new proc-macro crate) if a value cannot do it | M |
| N2 | A user's `ParamNode` needs a hand-written `SessionNode` before `Graph::add` takes it (the macro the built-in nodes use is private) | a `Params(node)` wrapper, beside `ForkByClone` and `Unforkable` | S |
| N3 | `Clone` means opposite things to `ForkByClone` (a fork) and to `param_parts` (a shared cell), so `ForkByClone(param'd node)` compiles and forks onto the live cell | `Params(node)` (N2) is the only way in for a param'd node; refuse the other by type | M |
| N4 | `Shape`, `Io::channel(c).map(..)` and the shortest node bodies are not in the docs | a node-writing guide in `tutti-graph`'s crate docs | S |
| A1 | `VoicePoolHandle` is only `send(VoiceCommand::..)`. `AddVoice` on a disk voice silently drops its loop and direction. The `stretch` argument is never `Some` | handle methods; a streaming voice type; the dead argument removed | M (built, step 9) |
| A2 | `VoiceNodeHandle` sets only gain and placement, and the caller must keep the disk `Stream` alive or the voice goes silent | loop, speed and play/stop on the handle; the handle owns the `Stream` | M (built, step 9) |
| A3 | Five loop setters, `crossfade_frames: usize`, a public unchecked `LoopSetting::On`, and the checked `LoopSpan` hidden | one checked `Loop` value in `Samples` | M (built, step 9) |
| A4 | Disk voices take `Beat` plus `Option<BeatDuration>` where memory voices take `VoiceWindow`. `tutti_sampler::Source` and `Status` are unused and collide with other crates' names | one window type; delete the unused | S (built, step 9) |
| A5 | `DiskStreamer::open` can only be polled in a sleep loop. `take_disk_streamer` and the detached session | a blocking open on the spawner, landing in `update()`; the take kept until `bevy-tutti` un-parks | S (built, step 9) |
| A6 | `Clip` (from #632) covers memory only | `Clip` also streams from disk past a size threshold, or with `.streamed()` | M (built, step 9) |
| C1 | **Bug**: `Wave::read` silently assumes 44.1 kHz when the container has no rate | refuse with `DecodeError`, or take the rate as an argument | S |
| C2 | Saving a `Wave` takes a hand-built `Spec` and hand interleaving, and `Rendered` and `Wave` do not convert | `Wave::write(path, format)`, `layout`, `interleaved`, and `From<Rendered> for Wave` | M (built, step 10) |
| C3 | `Spec` holds a `layout` and an optional `topology` that must agree. Codec constructors disagree on argument order and error type | `Spec` from a topology alone; one constructor shape | S (built, step 10) |
| C4 | "Finite positive rate" is re-checked in four places and skipped in two. `Wave` counts in `usize` | check once at `SampleRate` construction if the unit rule allows (a distinct range); `Samples` in `Wave` | L (built, two PRs: "as built" below) |
| E1 | `Bounce` takes `format` at runtime (`Missing(Format)`) and accepts file-only knobs (`NormalizeBuffers`) on a buffer master | `master(path, format)`; the file knobs only on `Bounce<_, FileMaster>` | M (built, step 10) |
| E2 | Stem accessors consume `self`, so `done.stems()[0].file()` does not compile | borrowing accessors and `into_*` for the owned ones | S (built, step 10) |

**C4: plan (built).** Surveyed at step 10 and first not built, because the conversion is too wide for one PR. `SampleRate(pub f64)` is constructed with the tuple at 1283 sites in 484 files (tutti-session 86 files, tutti-nodes 71, tutti-graph 39, tutti-sampler 38, tutti-dsp 33, tutti-export 28, tutti-plugin 20, tutti-midi-runtime 18, tutti-plugin-host 16, tutti-engine 16, then the rest), most of them tests and benches. About twenty places re-check "finite and positive": `tutti-export/src/job.rs` (`is_rate`), `tutti-midi-runtime/src/clock_map.rs`, `tutti-plugin-types/src/lifecycle.rs`, `tutti-plugin-server/src/session.rs`, `tutti-dsp` (`model/reach.rs`, `stft/geometry.rs`, `filter/crossover/legacy.rs`), `tutti-measure` (`silence.rs`, `align.rs`, `tempo.rs`, `yin.rs`), `tutti-nodes/src/reverb.rs`, `tutti-types/src/value/timeline.rs`, `tutti-plugin-host` (`transport_walk.rs`, `transport_source.rs`), `tutti-plugin-ipc/src/client/audio/dispatch.rs`, `tutti-clap-host/src/instance/processor.rs`, `tutti-clap-ffi/src/plugin.rs` and `tutti-modulation/src/compile.rs`. The plan is two PRs. First, move callers to `SampleRate::new`/`of` or a named constant while the field is still public. Second, make the field private (`new -> Option`, `of`, serde through `TryFrom`, a `check_range_contract` test, the range-types gate) and delete the re-checks. The `Wave` change (`len()` and its frame counts in `Samples`) goes alongside, in the second PR.

**C4, as built.** `SampleRate` has a private field: `new -> Option` (finite and above zero, nothing more: a rate of `f64::MIN_POSITIVE` is a rate), `const of`, `from_hz(u32) -> Option`, `get`, `TryFrom<f64>` (`SampleRateOutOfRange` carries the raw), serde through `try_from = "f64", into = "f64"` (so the plugin wire refuses a non-rate on decode, where the server used to re-check it), no `Default` and no `Unit` impl (a `from_raw` could not refuse). A rate another thread reads is `AtomicSampleRate`, which has no fallback: only a `SampleRate` is stored. The re-checks listed above are gone with the errors that carried them, and `Prepare::new`, `Reach::at`, `ClockMap::new` and `ClockMap::set_rate` no longer fail. What was kept guards something else: a header's whole-Hz range (`Spec::header_rate`), the resampler's 0.5 Hz to 2^32 Hz range, the loudness meter's 8 to 192 kHz, a C ABI's raw `f64` (`tutti-clap-ffi`, `tutti-clap-host`, the AU host, `tutti-plugin-types`' transport). `Wave::len()` and its frame counts are `Samples`, and `WaveMetadata::sample_rate` is a `SampleRate` (a container that states 0 is `NoSampleRate`). A SoundFont sample that states rate 0 starts no voice and is counted (`SoundFontUnit::unrated_starts`).

### 3.4 Live recording and the playhead (owner-approved 2026-11-12)

| # | finding | fix | size |
|---|---|---|---|
| R1 | A take carries no position on the timeline, input and output latency are not compensated, and a recording starts on wall time | `s.record(input, At) -> Take`: the start frame and beat, the compensation, punch in and out through `At`, with monitoring and recording opened in one call | L (built, step 8) |
| R2 | The playhead is two atomics (`beat()`, `is_playing()`) that can disagree, and it moves in block steps | one `s.playhead() -> Playhead { beat, frame, seconds, playing, at }` (a sequence lock), and `beat_now(now)` extrapolated from the block clock on `tutti_rt::monotonic_ns` | S-M (R2 built, §6 item 6) |

## 4. Design: declicked wiring

**Built (core lane step 2b).** See doc 013, "As built (declicked wiring)", for
where the build chose: the end of a fade is reported on its own ring rather than
in the commit box, a removed node stays until its output has faded, and a lane
the modulation drives is not faded.

Today a plain `connect` or `disconnect` steps at a block boundary (doc 013,
"No automatic declick of audio wiring"). The workaround is to wire at weight 0
and ramp the edge gain, which every host forgets. Decided 2026-11-12: the
**session** graph declicks wiring by default, while `tutti_graph::Editor` stays
explicit, so the engine's own tests and the differential suite keep their
hard steps.

- **What fades.** An edge `connect` adds enters at weight 0 and ramps to its
  weight over `EDGE_DECLICK` frames. An edge `disconnect` (or `remove` of an
  endpoint) removes ramps to 0 over the same span, and only then leaves the
  plan. `EDGE_DECLICK` is `MODE_DECLICK` (256 frames), one figure for every
  declick in the graph.
- **How.** It uses the edge-gain lane that already exists (doc 013, "Edge
  gain"). The compiled ramp is from the weight the last block was given to this
  block's, so no new op is needed. A fading-out edge stays in the plan as an
  edge whose target weight is 0. When its ramp has ended, the executor reports
  the edge in the returned commit box, and the next `update()` drops it from
  the spec, the way a crossfade's retiree is collected today. Its delay ring
  lives until then, so the tail of the old source plays out. That is correct,
  because what is heard is the fade, not the old source's past audio replayed.
- **Opting out.** `.hard()` on the edge `connect` returns (from lane step 2's
  vocabulary), and `disconnect_hard` for a cut that must be instant (a test, or
  an analysis graph).
- **What does not change.** Event edges are never faded: a note-off is a
  message, not a level. Bus edges fade the same way, per lane. An offline
  render (a bounce) hears the same fades, counted in frames, so it is
  deterministic.
- **Interplay.** A crossfade (`replace`) or a mode change already fades, and
  wiring that lands in the same commit fades alongside it. A latency change
  on a fading path moves its delay ring by the existing rule.
- **Tests.** A connect's first block ramps from exactly 0, the edge reaches
  its weight at `EDGE_DECLICK`, and a disconnect's last nonzero frame is
  inside the span. `.hard()` reproduces today's step bit for bit, against
  `Editor`.

## 5. Design: recording takes

A take is a recording that knows where it belongs on the timeline. Today's
`Recorder` copies a ring to a file and knows nothing of the timeline (R1).

### 5.1 Record from the graph, not from the device ring

A take records a **plan tap on the input's inlet node**, not `MicIn`'s
recording ring. The tap runs on the graph's clock, so every frame it hands out
belongs to a known graph frame. Stamping a take is then exact, with no
OS-time-to-frame mapping at the edge. One capture path feeds what you hear
and what you keep. A dropout in the capture reaches the take as the silence
the graph played, at its place, and is counted, so the take stays aligned.

Two changes to `tutti-graph`:

- `TapReader::next_frame() -> Frame`: the graph frame of the next frame the
  reader will hand out. The writer pushes each block's start frame beside its
  samples, and the reader keeps the count, so a lost block (`take_lost`)
  advances it too.
- Taps already ride the commit queue and survive edits; nothing else is
  needed.

**Built** (§6 item 8, first half). The writer pushes a run (start frame, frames
taken) into a small ring before each block's samples, so a reader that sees a
sample sees its run; a block the lanes had no room for leaves no run, and the
reader's clock jumps to the next run's start. `TapReader::contiguous` is the
frames readable before that jump, so a consumer that places every frame never
reads across a gap. A `pair` writer (an inlet's producer, the engine's heard tap)
has no graph clock and numbers its frames from zero. An aligned tap is on the
output's clock.

### 5.2 Where the take is placed

A sample the inlet emits at graph frame `F` was played by the performer in
answer to output the device played `out_latency` after it was rendered, and
reached the graph `in_latency` plus the inlet's lead after it was sung. The
take's first frame is placed at

```
start = F - (in_latency + out_latency + inlet_lead) + offset
```

- `in_latency` and `out_latency` are what `tutti-cpal` reports for the open
  streams (cpal's callback timestamps: capture against callback for input,
  callback against playback for output). This is a new `Device::latency() ->
  DeviceLatency { input: Samples, output: Samples }`, re-read after a restart.
- `inlet_lead` is the inlet's fixed ring lead (`InletNode`'s declared prefill).
- `offset: Samples` is the user's manual trim, for a device that misreports,
  set once per input.

A `Take` stores `F`, each term and `start`, so a host can show and adjust the
figure. Nothing is resampled or shifted in the file. Placement is metadata, and
a `Clip` made from the take starts at `start`.

### 5.3 Punch

`At` gives the punch: `Punch::at(At::Beat(..))`, `.until(At::Beat(..))`, or
open-ended. The control side turns a beat into a frame through the project's
`TempoMap`, at the frame the transport will reach. The pump discards tapped
frames before `in` and stops at `out`, using `next_frame`. Pre-roll needs
nothing special, because the tap is opened when the take is armed, so frames
before `in` exist and are dropped. A punch in the past starts at once, and the
take reports the real `F`.

### 5.4 Surface

```rust
let input = s.add_input(InputConfig::default().channels(&[0, 1]))?; // S1: MicIn + inlet, reopened on restart
s.graph().connect(input.node(), eq)?;                              // monitoring is ordinary wiring
let take = s.record(&input, Punch::at(Beat(16.0)).until(Beat(48.0)), "take1.wav")?; // Take<Armed>
// update() reports SessionEvent::Take(TakeEvent::{Started, Finished, Failed}) for it
let done = take.finish()?;                       // Recorded: path, start, the terms, lost frames
s.graph().replace(input.node(), done.clip()?)?;  // the take, where it was played; a bounce can fork it
```

- `Take` is typestate (`Armed`, `Rolling`, `Done`). `finish` exists on the
  live states and stops at once; a set `until` ends it by itself.
- The pump runs on the session's spawner (convention 5), as `PumpLoop` into a
  `FileOut`. A recording that dithers wraps its sink as today.
- `SessionEvent::Take` follows the nesting rule, and its error is
  `tutti::Error::Take(TakeError)`.
- `Recorder` stays as the plain source-to-sink pump for an app that does not
  want placement, with S5's fix (`wait` only on a finite source).

### 5.5 What it needs first

Lane step 6 (the playhead), for the frame and beat on one read, and lane step
7 (the device), for `add_input` and `Device::latency`. MIDI recording is the
MIDI session's, through an event tap, which already carries absolute frames
(`TapEvent::frame`); it uses the same placement terms.

## 6. The lane

Everything core has in flight or planned is **one lane**, worked in this
order, one PR per step, each deleting what it replaces. It supersedes tasks
#58 to #62 and the separate test-audit and API lanes.

0. **Land what is open:** #632 (prelude, `connect`, transport verbs, `Clip`,
   `tutti::Error`), and the test-audit PRs #626 (`PROPTEST_CASES`, nightly),
   #627 (no-alloc gates) and #628 (anti-oracles).
1. **Bugs:**
   - S3: a named `#[must_use]` live session;
   - S5: `wait` only on a finite source;
   - T1: an undeclared range is refused when the route is made;
   - T7: checked `Law` parameters;
   - C1: no assumed 44.1 kHz.
2. **Graph surface 1** (doc 013), in four PRs. The plugins session's view
   (doc 029 step 2) builds on 2a and takes 2c's `ParamRef`.
   - 2a (#639, built): typed `Key<N>`, `OutPort`/`InPort`, one wiring
     vocabulary, no `Silence`.
   - 2b (#644, built): declicked wiring (§4).
   - 2c (#648, built): event ports (`key.events_out()`, the `Event` wire;
     `set_event_sources` and `EventSource` deleted; event input 0 is the main
     one, where a chase delivers) and typed params (T2, T3, N2). N3 is partial:
     `Params(node)` is the way in, but Rust cannot refuse `ForkByClone(param
     node)` by type. `AutomationTake::record` and `Trajectory::with_range` are not typed yet.
   - 2d (built): removal (S7). Every removal and close is a `Result` naming what
     was not there (`RemoveError`, `CloseError`, `HeardTapError::NotOpen`,
     `NoSuchModSource`, `NoSuchModRoute`, `NoSuchMidiRoute`); `Graph::add`
     (a `SessionNode`) is the one way a node enters; `open_event_inlet` takes an
     `EventCapacity`. Doc 013, "As built (removal)".
3. **Node authoring** (3b, built): N1's rest, N3, N4.
   - N1 (built, pulled before 2d): a node's params are declared once, next to
     its `Param<U>` cells, by `tutti_graph::declare_params!`, which writes the
     `HasParam`/`HasParams` impls (the unit is the cell's, checked), the
     node's `ParamSet` and its shape's param ports (`declared_set`,
     `declared_ports`, `declared_base`); the `DesignNode` families' controls
     are one `control_set!` each. `HasParam`, `HasParams`, `ParamUnit` (was the
     session's `ParamValue`) and `Scalar` moved to `tutti-graph`; the session's
     `declare_params!` table, `Declared` and the `*_PARAMS` lists are gone. The
     port order a node's `process` reads (`Io::param(k)`) is the declaration's
     row order.
   - N1's rest (3b, built): `process` reads a driven param through the
     declaration too: the macro writes `declared_param::<{ UnitParam::K as u16
     }>(&io)` and `declared_param_at::<..>(&io, index)` (a `ParamInput`; a kind
     the node does not declare does not compile, and a row after a `;` or an
     absent `Option` cell reads `Base`), which resolve the kind to its port by
     walking the rows, once per call. Every engine node moved to it; the
     `block_driven(io, k)` helpers are `ParamInput::last_frame`. Associated
     port consts were weighed and left: an `Option` row makes a port's index a
     runtime fact, so a const would be wrong for the node that has it absent.
   - N3 (3b, as far as types reach): no bound refuses `ForkByClone(param
     node)`. It would need "`N` declares no params", a negative bound; a
     sealed marker the macro implements can be required but not forbidden. The
     rule stays documented (`Params`, CLAUDE.md) and no runtime check was added.
   - N4 (3b, built): `tutti-graph`'s crate docs have "Writing a node": the
     shortest node, `Shape`, a param'd node, events in and out, `Status` and
     `report_tail`, and what not to do, every example a doctest.
4. **Modulation** (built): T4, T5, T9 (T1's typed range landed in 2c).
   - T4: a source and a route are named by `ModSourceId` and `ModRouteId`,
     minted by `Mods` and never built; `Session::mods()` reads (`ModsView`:
     `routes()`, `routes_to(target)`, `sources()`), `mods_mut()` edits; a route
     is declared by `RouteSpec::new(source, target, depth).polarity(..).curve(..)`
     and edited by `route_of(id)?`'s setters, so `route_mut` and the public
     fields are gone. `tutti_modulation::{SourceKey, RouteKey}` stay the
     compiler's and a host's own minting.
   - T5: `ModState::problems()` is the one place: a `Problem` (`Refused`,
     `Unresolved`, `Unfed`) whose `Display` names the node and the param, and
     the event carries the same value (`SessionEvent::Modulation`).
     `ModState::refusal`, `unresolved_targets`, `unresolved_sources` and the two
     modulation events are gone.
   - T9: `Depth` is range-checked (hand-written; `new -> Option`, `of`,
     `clamped`, serde through `try_from`; no `+` or `*`, which could leave the
     range) and the compiler's `BadRoute` refusal went with the NaN it
     refused; `ProjectMeter::set_meter` and `ClickSettings::set_meter` take
     `impl Into<Arc<MeterMap>>`; `OfflineTimeline::advance` takes `Samples`;
     `in_preroll` is a method; `ClickSettings::set_volume` returns
     `Result<(), VolumeOutOfRange>`; `Lfo::rate` takes a `BeatDuration` or an
     `Hz` (`hz` is gone); `Mods::record` and `set_source` are `Result`s.
5. **Graph surface 2** (doc 013), built: the facets (`g.taps()`, `g.meters()`;
   the heard tap stays the session's, its writer being in the engine's output
   buffer), the plumbing behind tutti-session's `testing` feature
   (`Graph::testing()`, `take_audio_side`, `AudioSide`) or `pub(crate)`, and the
   failure rule (every query, wire removal and modulation call that names a node
   that is not there is a `Result`); S8: `HeardOpen::Retry` for a busy audio thread,
   a `LoadMeter` with one reader, and tutti-cpal's one listing,
   `DeviceHost::snapshot`. Then the deferred chase injection (built): a chase whose
   takeover is delayed by a fade is held in the queued crossfade, its injection
   sized with the plan, and delivered when the fade ends (`end_fades`), not dropped;
   `chase_skipped_count` is gone.
6. **Playhead** (built): R2. `Session::playhead() -> Playhead` is one read of
   the block clock's cell (`tutti-engine`'s `block_clock`, one sequence-lock cell, no
   second copy): the audio thread writes it once per render call, from the
   transport of the call's first frame after the commands due on it, so
   `beat`, `frame`, `seconds` and `playing` are one instant. `at` is the call's
   `tutti_rt::monotonic_ns`, the clock the MIDI edge's `ClockMap` already uses, and
   the snapshot also carries the tempo in force and the call's length.
   `Playhead::beat_now(now)` (`Session::beat_now()` reads the clock itself) is
   the beat plus the tempo's beats over the time since `at`, held at the beat
   when stopped, before a call has rendered or for a `now` before `at`, and
   **clamped to one call's length** past `at`, so a stalled audio thread
   leaves the playhead one block on, never running away. Through a tempo ramp
   the line is at the tempo of the call's first frame: wrong by the ramp's change over
   one block at most. A session with no engine rendering reads
   `Playhead::default()` (stopped at the origin, `at` is `None`); an offline
   `Bounce` renders a fork and never moves it. `TransportSettings::beat` and
   `MotionFsm::{is_playing, is_stopped}` are gone from the public surface (the
   transport's own `Timeline` reads of beat and rolling stay, for the code
   that is the transport's), and `MotionFsm::motion` is crate-private. The
   example of §1: `let p = s.playhead(); draw(p.beat_now(tutti_rt::monotonic_ns()), p.playing())`.
7. **Device** (built): S1, S2, S6, `Device::latency`, typed event errors.
   - S1: `SessionBuilder::host(AudioHost)` is the host of the output and of
     every input; `Session::add_input(InputConfig) -> Input` opens a
     `tutti_cpal::MicIn` (opened by a `MicConfig`: host, device, inputs by
     `Fold::select`, the monitor ring's depth) and its inlet in one call, and a
     restart reopens each at the graph's new rate into the same inlet node
     (`Input::node()`, a `Key<InletNode>`; nothing rewired). The ring carries
     its rate: `tutti_graph::InletRing::pair(width, capacity, rate)`, and
     `Graph::add_inlet(ring)` refuses one whose rate is not the graph's
     (`InletError::RateMismatch` stays). `MicIn::open*` are one
     `open(&MicConfig, rate)` and `open_with_monitor`.
   - S2: one `DeviceRestart` value (`DeviceRestart::to(device)`, `.max_block`),
     applied through `LiveSession::restart(request) -> Result<RestartOutcome,
     RestartError>` (the pair, so no foreign `Device` can be passed;
     `RestartOutcome::Stale` is a stale request of the policy's, dropped,
     `Restarted(DeviceRestarted)` the rates before and after). The host's
     hook for what it holds itself is `LiveSession::on_restart`, set once and run
     at the end of every restart's hook, and `LiveSession::watch_devices(source,
     policy)` reads what the device was opened with and runs on. `restart_following`,
     `restart_wanted` and `Session::restart` are gone; `restart_on`,
     `Session::watch_devices` (four arguments) and `build_device_on` are behind
     `testing`. Device events are `SessionEvent::Device(DeviceEvent)`,
     `#[non_exhaustive]` at both levels: `Fault`, `Lost`, `Appeared`,
     `DefaultChanged`, `RestartWanted`, `Restarted`, `RestartFailed(RestartError)`
     (a shared `Error`, its `source()`; `Error::Restart` for `?`),
     `InputReopenFailed { input, error }`, `OutputsListed`,
     `OutputsListFailed(tutti_cpal::Error)` and `OutputsListSlow`.
   - S6: `inputs` and `outputs_floor` take a `ChannelLayout` (so a count no layout
     holds is unwritable, and `root_width`'s project width is one);
     `render_workers(Workers)`: `Workers::PerCore`, `Workers::Exactly(NonZeroUsize)`,
     `Workers::SERIAL` (a constant and the default, so there is one spelling of
     the callback alone: `Exactly(1)` is it) and `Workers::of(n)`.
   - `Device::latency() -> DeviceLatency { input, output: Option<Samples> }`, the
     streams' callback timestamps (output: playback against callback; input:
     callback against capture, `tutti_cpal::StreamLatency` cells the callbacks
     record into) at the graph's rate, `None` for a direction not yet reported
     (after a restart too) and never a zero. `Session::latency()` stays the
     plan's compensation.
8. **Takes** (built): §5 (`TapReader::next_frame`, `Take`, `Punch`). Two PRs:
   `next_frame` (with `TapReader::contiguous`) on its own, then the session's take.
   - `Session::record(&Input, Punch, TakeTarget) -> Result<Take<Armed>, TakeError>`
     (features `device` and `audio-io`): a plan tap (raw) on the input's inlet
     node, and a pump on the spawner (`submit_stream`) into a `FileOut` or a
     sink of the caller's. A gap the tap lost is silence in place, counted in
     `Recorded::lost`, so the take stays aligned (a plain `Recorder` closes up).
   - `Punch::at(impl Into<At>)`, `.until(..)` refused where it is made when it
     is not after the start; a beat is a frame through the project's tempo map
     from the playhead, and a beat while the transport is stopped is
     `TakeError::TransportStopped`. A punch in the past starts at once.
   - Placement: `Terms { in_latency, out_latency: Option<Samples>, inlet_lead,
     offset: Trim }`, read when the first frame arrives; an unreported latency
     is `None` and `Terms::is_complete` says so; `Start::{At, BeforeOrigin}` keeps
     a start before the origin rather than clamping it. `offset` is per input
     (`InputConfig::offset`, so it survives a restart), signed (`Trim`).
     `inlet_lead` is `OpenedInput::lead`, declared by the opener (the sound
     card's ring is not prefilled: 0).
   - `Take<Armed>` -> `Take<Rolling>` (`rolling()`, once the first frame is in)
     and `finish()` on both; dropping the handle stops the take. `Recorded`:
     path, `Placement` (first frame, terms, start, start beat), frames, lost,
     and `clip()` (feature `sampler`).
   - `SessionEvent::Take(TakeEvent::{Started, Finished, Failed})`,
     `Error::Take(TakeError)`, both `#[non_exhaustive]`.
   - A restart during a take is not followed: its frames after the restart are on
     the new rate.
   - S5: the `Live`/`Finite` recorder kinds had landed (#636); a live
     recorder's `Drop` stops, joins and finalizes, and now logs a failed sink
     at `error` with the frames lost; `Recorder` stays source-to-sink, off the
     spawner (it owns no session).
9. **Sampler** (built): A1 to A6, one PR. As built:
   - A3: `Loop`, a span of whole frames in `Samples` (`Loop::new(a..b)` refuses an
     empty or reversed span, `with_crossfade` one longer than the loop:
     `LoopError::{EmptySpan, CrossfadeTooLong}`) replaces `LoopSetting`, the five
     setters and `crossfade_frames: usize`. Every tier takes `Option<Loop>`
     (`MemorySource::set_loop`, `Stream::set_loop`, a pool's and a node's handles) and the
     streamer's `LoopError::Position` is gone: a position that is not a frame cannot
     be written. `Loop` keeps the file-bound derivation (`LoopSpan`: the fade
     after the clamp to the file, the fade into the head, where a wrap lands)
     as the one internal read of it; `LoopSpan` stayed because the derived figures
     depend on the file's length, which a `Loop` made on the control side may not
     know yet.
   - A4: `VoiceWindow` is the one window (kept: it was the memory voices' and is
     already in the prelude): `DiskVoice::from_reader(reader, VoiceWindow)`,
     `set_window`, `MemorySource::placed(wave, VoiceWindow)`. `tutti_sampler::Source`
     and `Status` were used by nothing and are deleted (`DiskStreamer::sample_rate()`
     replaces `status()`); `tutti-sampler`'s `voice` module is private and the crate
     joins the per-module path gate.
   - A1: `VoicePoolHandle` is methods over a private `VoiceCommand`: `add` takes a
     `MemorySource`, `add_streaming` a `DiskVoice` and its `Stream` and a `Playback`,
     so a disk voice cannot be added without the stream that steers its loop and
     direction (the add that dropped both is not writable). `VoicePool::insert`
     is the same for a pool not yet in a graph. `SendError` carries nothing.
   - A2: `VoiceNodeHandle` sets window, loop, speed and play/stop beside gain, and
     `VoiceNode::streaming(DiskVoice, Stream, Playback)` hands the stream to the
     handle, which owns it (an owned cross-thread handle, convention 4): it ends with the
     last clone, and the session's reclaimer keeps a clone for as long as the node is in the
     graph. A disk voice's loop is set on the stream; `stop` is a gate on the node (silent,
     nothing read), which a free-running voice resumes from where it stopped.
   - A5: `Session::open_stream(path, Placement) -> Result<SamplerLoad, SamplerError>`
     opens on the spawner (`Opening::wait` sleeps on the reply) and lands
     `SessionEvent::Sampler(SamplerEvent::Opened { id, stream, reader } | OpenFailed { id, error })`;
     `Error::Sampler(SamplerError)` is the one error variant. `take_disk_streamer` stays
     until `bevy-tutti` un-parks; `disk_streamer()` had one user, which moved to the new call.
   - A6: `Clip` is a `VoiceNode` over either tier (`Clip::new(wave)` resident,
     `Clip::streaming(reader, stream)`), its controls the node's handle.
     `Session::load_clip(path, Residency)` decides the tier for a file: `Auto` streams
     past `STREAM_ABOVE_BYTES` (32 MiB of decoded `f32`, about 95 s of 44.1 kHz stereo:
     a few such files would take the streamer's 1 GiB cache for audio read once
     through) and lands `SamplerEvent::ClipLoaded`. The force is the `Residency` argument,
     not `.streamed()` on the clip: the choice is made in the load job, before there is a clip
     to call it on. `Recorded::clip()` is unchanged (resident).
10. **Codec and export:** C2, C3, E1, E2 built; C4 built after the lane, in two PRs (see its row).
   - C3: `Spec` has private fields and one statement of width, its topology
     (`Spec::new(format, topology, rate)`; `Spec::of_width(format, layout, rate)` is the
     engine's order for the width, unassigned where it has none, as the old `None`
     topology was; `layout()` is derived). `SpecError::TopologyWidth` is gone with the
     disagreement. The codec's constructors are `(spec, target)`: `Encoder::create(spec,
     sink)`, `create_file(spec, path)`, `create_file_planned(spec, path, frames)` and
     `FileOut::create(spec, path)` (it was `(path, spec)`); the error is `tutti_codec::Error`
     for writing (`FileOut::create` and `MicIn::matching_sink` return it, no longer an
     `io::Error`, and `TakeError::Open` carries it) and `DecodeError` for reading.
   - C2: `Wave::write(path, Format)` writes at the wave's own rate and topology (staged
     beside the path, as `Encoder::create_file` does); `Wave::layout()`; `Wave::interleaved()`
     is an iterator (a wave is held whole, so a `Vec` would double it; `write` reads it in
     chunks). `From<Rendered> for Wave` is infallible: `Rendered::new` now refuses more
     planes than a layout holds (`RenderedError::TooWide`), the one thing a `Wave` could
     not take. A render keeps no topology, so the wave's is the engine's order for the
     width, as `Wave::new`'s.
   - E1: `Bounce::master(path, format)` makes a `FileMaster` that always has its format;
     `Bounce::format` and `BounceConfigError::Missing(Field)` (and `Field`) are gone.
     `resample`, `dither`, `normalize` and the format-sharing `stem(node, path)` are on
     `Bounce<_, FileMaster>` alone, so a buffer master cannot be given them (a
     `compile_fail` doctest). A file stem always has its format
     (`Stem::new(node, path, format)`; `Stem::format` is gone), so a buffer master can still
     write file stems; those stems take the default dither and no resample. A stem's kind is
     in its type (`Stem<N, FileStem>` has `.gain(StemGain)`, `Stem<N, BufferStem>` has none; a
     `compile_fail` doctest), so `NormalizeBuffers` and `Bounce::normalize_stems` are gone.
     `FileTarget` and `BufferTarget` hold one width, a count or a topology
     (`with_topology(topology)` replaces the count), so the render-start refusal of a topology
     of another width is deleted.
   - E2: `Produced::file` and `buffers` borrow (`Option<&Written>`, `Option<&Rendered>`),
     `into_file` and `into_buffers` own; `Outcome::finished` borrows and `into_finished`
     owns.
11. **Tempo and names:** T6, T8, T10 (built, step 11).
   - T6: `TempoMap::view(rate) -> TempoView` owns the rate and the lookup hint; its methods are
     the frame queries without the `_with` twin or the rate (`seconds_at`, `beat_at_seconds`,
     `tempo_at`, `tempo_at_seconds`, `tempo_slope`, `tempo_at_frame`, `beat_at_frame`,
     `frame_at_or_after`, `next_change`, `chord_cell`). The map keeps the four rate-free
     queries; `Cursor`, the twins, the rate-taking frame queries and the free `chord_cell`
     are gone. A view borrows the lent map, so it is made per block (a hint lost between
     blocks costs one binary search). `tutti_tempo::Shape` is `Transition` (the law of the
     tempo across a segment; `Law` is `tutti-curve`'s and `Shape` the graph's).
     `TempoPoint::{step, ramp_time, ramp_beats}` take only the point's own beat and tempo,
     since a ramp's target is the next point's. `TempoMap::builder(first)` writes
     `step_to` / `ramp_time_to` / `ramp_beats_to`, each adding a step point, so the last point
     never ramps (a `compile_fail` doctest holds that a ramp has no spelling at the end);
     `TempoMap::new` still refuses a list that ends on one (`RampWithoutEnd`).
   - T8: `LoopRange::try_from(a..b)` is the check, with a `LoopRangeError` (`NotFinite`,
     `Empty`, `Reversed`, each carrying the beats given); `new` stays an `Option` over it
     (the range-checked table's shape, and many callers), and now also refuses a non-finite
     bound. `Session::set_loop(LoopRange)` arms a loop over a range and `clear_loop()` disarms it
     (the bounds are kept, as `LoopEdit` keeps them), two verbs rather than a
     `set_loop(Option<..>)`, so that clearing is not written as a value that might be absent;
     each is a `LoopEdit` at `At::NextBlock` and returns `ScheduleFull`.
   - T10: `tutti_graph::Transport` (about 70 explicit paths against 360 for the transport's
     own) is `TransportPosition`; modulation's `Wave` is `Waveform`. `UnitParam::Depth` is
     kept: the variant is the param kind of the `Depth` unit by the same name as every other
     kind (`Cutoff` for `Hz`, `Mix` for `Mix`), it is used for a phaser's and a delay's depth as
     well as an LFO's, so `ModDepth` would be wrong for two of three, and it is namespaced by
     the enum, where nothing reads it beside the type.
   - **A beat play on a stopped transport** (not a doc 030 row): an `At::Beat` command waits
     until playback reaches the beat, and a stopped transport has no frames per beat, so a
     `Play` (or a locate that then rolls) timed at a beat while stopped waits for the playback
     only it can start and never fires. `Session::schedule` refuses it with
     `ScheduleError::BeatWhileStopped { beat, event }` (`ScheduleError` is now the call's error,
     `Full(ScheduleFull)` the other case; `Error::Schedule` carries it), as `TakeError::TransportStopped`
     does for a punch. A stop or a plain locate at a beat still waits for the beat, which is
     meaningful. The check is the control side's word: the last motion the session sent
     for the next block (`play`, a rolling locate, `stop`), or else what the playhead last
     rendered (a transport started through `transport()`), so a play at a beat straight
     after `play()` is taken.
   - The `tutti` umbrella has a `tempo` module (`TempoMap`, `TempoMapBuilder`, `TempoPoint`,
     `Transition`, `TempoView`) and `TempoMap` in its prelude, since `Session::set_tempo_map` takes one.
12. **Instruments** (built): S4, with the MIDI session's keys handle.
    - `InstrumentHandle` holds a `tutti_midi_runtime::MidiKeys` (`keys()`, `keys_mut()`) and no
      longer sends, plays notes, reads the clock or the losses itself; `SharedKeys`, `LiveInput`
      and the session's `SendError` are gone (`MidiKeys` answers `Refused`, which
      `MidiError::Send` carries). A handle is not `Clone`: the keys are an inlet's one producer,
      so a host that plays from two threads wraps the handle in its own lock or moves it, and
      this reverses doc 017 E4's "stay `Clone`". `Session::remove_instrument` and
      `GeneralMidi::remove` take their handle by value, so a second removal through it is unwritable. `Session::add_instrument` returns
      `AddedInstrument { handle, controls }` and wires the strip to the master;
      `add_instrument_unwired` leaves the strip for a mix that routes it (`add_general_midi`
      does), a second method over a flag so each reads at the call site. `instrument` and
      `instrument_plugin` still take a node already in the graph and leave it unwired.
    - `s.midi().input(target) -> MidiKeys` replaces `live_input`: the inlet is opened once, its
      keys are handed out once (`MidiError::InputTaken`), and `record` opens the inlet
      without taking them. A live input is moved with `Midi::retarget_input(from, to)`
      (`MidiError::NoLiveInput`), since it has no node to name; `retarget` is clips only.
    - `tutti_ump::NoteVelocity` is the M2 note velocity field, the whole `u16`, in
      `Voice2::NoteOn`/`NoteOff`, `Voice2::note_on`/`note_off`, `Message::note_on2`/`note_off2`,
      `MidiKeys::note_on`, `SoundFontUnit::note_on` and `PairedNote::velocity`;
      `tutti_midi_protocol::widen_velocity(U7)` and `narrow_velocity(NoteVelocity)` are the
      M2-115 scaling. `tutti-voicing`'s `Command`s and `Patch` keep their internal `u16`.
13. **Test leftovers** (built, #641 by the DSP session): `tutti-nodes`'
    `width_generic_golden.rs` replaced by analytic properties per node.

After the lane, the older plan items resume: export features (plan 24),
correctness odds (plan 35), structure passes (plans 43 to 49), the SAFETY
lint and the `SampleRate` regression (0b.5, 0b.7), and the doc fixes (0b.2,
0b.3).

The parked `bevy-tutti` is not updated by any step; un-parking ports it.

## 7. Rejected

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
