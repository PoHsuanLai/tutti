# 019: the bounce API

Status: proposal for the owner's review. No code changes in this document's
PR. Lane E4, phase 1.

Dependencies on other lanes (this doc assumes they land first or alongside):

| Lane | What this doc needs from it |
|---|---|
| E1 | Correctness. **Landed:** the fork-health check before the file lands (`Error::ForkFailed`), `Error::Unmeasurable` on silent or below-gate audio, clip counting (`Written::clipped_samples`) and integer headroom for normalize (`tutti-export/src/normalize.rs`). **Not found in the code:** a chunked measurer (step 2's cancel points in the measurement need one) and a serial-versus-parallel bit-identity test of an export; steps 2 and 6 still carry them. |
| E2 | **Landed:** `tutti-export` has `default = []`, and the umbrellas carry `export-*` features. |
| E3 | Tempo. The live transport is single-tempo too and nothing in the engine reads `TempoMap`; the owner chose a separate design doc ([020](020-tempo-map-transport.md), tempo maps) for one tempo model shared by live playback and offline render. This doc does not design it. It needs only a *project tempo source*: today the single transport tempo, later `TempoMap`, and an offline clock built from the same source. |
| E5 | **Landed:** `DitherNoise` and `StreamingResampler` are in `tutti-dsp`; the pipeline calls them. |

## 1. The problem

The audit (`export-audit/report.md`, main `a25e8e7d`) found an exporter whose
algorithms are right and whose entry points are wrong.

| Finding | Evidence | Consequence |
|---|---|---|
| About 25 concepts to export a project | `ExportConfig`, `RenderConfig`, `EncodeConfig`, `ExportRequest`, `ExportClock`, `Flow`, `Pass`, `Outcome`, three `_with` twins | A host re-derives what the session knows. |
| The defaults render the wrong thing | Clock is `Frozen` (`session/export.rs:164-175, 296`); latency and tail are opt-in; `RenderConfig::default` has duration 0 | Silent output from transport-driven sources, leading silence, cut reverbs, empty files reported as success. |
| Tempo is not followed | `OfflineTimeline` has one fixed tempo; three beat-to-seconds copies | A ramp or step drifts against playback (lane E3). |
| Stems cost a fork, a pass and a plugin-server launch each | `session/graph.rs:840`: a node stem also ignores the root declaration | Stems are not sample-aligned with the master or each other. |
| bevy mirrors the request field by field | `bevy-tutti/src/export/request.rs:105-193` | Two places to change per option. |
| The fork machinery is public | `RenderGraph::{gather, build, set_pool, set_root_topology, editor_mut, commit}`; `build` skips the pairing check `fork` does | Mechanism is API; an unpaired graph is reachable. |
| Progress has no stage and no single fraction | `Pass` counts two passes in different units; cancel is not checked in `finish` or the measurement | A UI cannot say what it is doing; a cancel waits. |

Goal: a project bounce is one call whose every figure is derived from the
session, with an explicit method for each override; `tutti-export` is only the
pipeline that call drives.

## 2. The user-facing API

`Bounce` lives in `tutti-session` (it composes tutti-transport, tempo,
tutti-graph, tutti-export and the plugin hosts, so no lower crate can own it,
doc 015). Both umbrellas re-export its vocabulary from their preludes.

### 2.1 Headless

Master only:

```rust
use tutti::prelude::*;

let done = session.bounce(
    Bounce::project()
        .format(Format::Flac { depth: IntDepth::Int24, level: FlacLevel::Standard })
        .master("out/master.flac"),
)?;                                   // Outcome<Bounced>
```

Master plus stems, one pass:

```rust
session.bounce(
    Bounce::project()
        .format(Format::Wav(Pcm::Int24))
        .master("out/master.wav")
        .stem(drums, "out/drums.wav")
        .stem(bass, "out/bass.wav")
        .stem_with(Stem::new(pad, "out/pad.wav").channels(ChannelLayout::MONO)),
)?;
```

Normalized (master measured, stems left as rendered unless asked):

```rust
session.bounce(
    Bounce::project()
        .format(Format::Flac { depth: IntDepth::Int16, level: FlacLevel::Standard })
        .master("out/master.flac")
        .normalize(Normalize::lufs(Db(-14.0)))
        .stem(drums, "out/drums.flac")
        .normalize_stems(StemGain::Shared),   // the master's gain, so stems still sum
)?;
```

Cancel with progress, on a worker thread (the control-thread halves stay on
the session thread, as `export`/`finish_export` do today):

```rust
let job = session.bounce_start(bounce)?;       // gathers the fork now
let handle = job.handle();                      // Clone + Send + Sync
let worker = std::thread::spawn(move || job.run());
while !worker.is_finished() {
    if let Some(p) = handle.progress() {
        ui.show(p.stage(), p.fraction());       // Stage::Rendering, 0.37
    }
    if ui.cancel_pressed() { handle.cancel(); }
    session.update();
}
match session.finish_bounce(worker.join().unwrap())? {
    Outcome::Finished(b) => report(b.targets()),
    Outcome::Cancelled => {}                    // every target untouched
}
```

### 2.2 Bevy

Same value, entities for nodes. `Bounce<Entity>` is `tutti_session::Bounce<N>`
at `N = Entity`; the adapter resolves entities to `NodeKey` once, when the
bounce starts (`Bounce::try_map`).

```rust
commands
    .spawn(BounceRequest::new(
        Bounce::project()
            .format(Format::Wav(Pcm::Int24))
            .master("out/master.wav")
            .stem(drums_entity, "out/drums.wav")
            .normalize(Normalize::peak(Db(-1.0))),
    ))
    .observe(|done: On<BounceDone>| match &done.event().result { /* Outcome<Bounced> or BounceError */ });

fn ui(q: Query<&BounceInFlight>, mut cancel: MessageWriter<CancelBounce>) {
    for b in &q {
        if let Some(p) = b.progress() { /* p.stage(), p.fraction() */ }
        // b.cancel() cancels; despawning the entity does too.
    }
}
fn on_stage(on: On<BounceStage>) { /* fired when the stage changes */ }
```

`BounceRequest` holds the `Bounce<Entity>` plus the `World`-reading edit hook
(`PrepareGraph`) and nothing else: no mirrored fields. `BounceDone` carries
`Result<Outcome<Bounced>, BounceError>` with node keys already mapped back to
entities in the error. bevy's prelude re-exports the whole config vocabulary
(`Bounce`, `Stem`, `Range`, `Format`, `Normalize`, `StemGain`, `Dither`,
`Resample`, `Latency`, `Tail`, `Progress`, `Stage`, `Outcome`).

## 3. Derived defaults and their overrides

A `Bounce` is data; `bounce`/`bounce_start` derive the rest from the session
at start. Every derivation has one explicit method. No setter is needed for the
common case.

| Quantity | Derived from | Override |
|---|---|---|
| Range | The transport's loop if set, else `Range::Project` (an error without `.range` or `.until`, section 10). Beats are converted to frames by the project tempo source at the render rate, in one session function (`Session::beats_to_frames`, private); today that reads the single transport tempo, and doc 020 (tempo maps) switches it to `TempoMap` internally with no API change. | `.range(Range::Loop \| Beats(a, b) \| Seconds(a, b) \| Frames(a, b))` |
| Clock | An offline timeline built from the same project tempo source (so it matches playback), started at the range's start beat, with the loop region off. Transport-driven nodes read the same tempo path playback does. | `.clock(ExportClock::frozen())` for an effect or patch bounce; `.clock(ExportClock::timeline(t))` for a hand-built one |
| Render rate | The session's rate. | `.sample_rate(r)`: the graph is forked and prepared at `r`. |
| File rate | The render rate. | `.resample(Resample::to(rate))` |
| Latency | The compiled plan's total latency, trimmed from the head of every target. | `.latency(Latency::Keep \| Frames(n))` |
| Tail | `GraphTail::resolve(cap)` with `cap` = 10 s, plus any untrimmed latency; added to the audible span. | `.tail(Tail::None \| Frames(n) \| Graph { cap })` |
| Root topology | The session's declared root topology (`declare_root_topology`), for the master. A stem takes its own (§7.3), never the master's. | `.root_topology(Option<ChannelTopology>)` |
| Master width | The root's width. Wider-than-file folds through `Fold`, never truncates. | `.channels(layout)`, `.file_topology(t)` |
| Plugin forks | State-transfer fork per hosted plugin, one `plugin-server` per plugin for the whole bounce (not per target); Offline announced at start, Realtime at finish. | None: the bounce cannot render without it. |
| Worker pool | The session's render pool, when its cost model says parallel pays. | `.serial()` forces the serial render. |
| Dither | `Dither::Triangular` for every integer width (24-bit included); ignored by float and Vorbis targets. | `.dither(Dither::Off \| Rectangular)` |
| Event loss | `Fail`: a dropped event fails the bounce. | `.on_event_loss(EventLossPolicy::Allow)` |
| Normalize | None. | `.normalize(n)`, `.normalize_stems(StemGain)` |
| Format | **Required.** `Bounce` has no default format; a missing one is `BounceError::Config(Missing)` at start, before any fork. | `.format(f)`, per-target `Stem::format(f)` |
| Edit of the fork | None. | `.edit(PrepareEdit)`; runs on the job thread as today. |

The tail row owns what [022](022-reverb.md) §12.4 deferred: the phase that resolves
the tail at the programme's end, from what the graph reports then, instead of the
ceiling. Step 0b shipped the fallback (export spends the ceiling against `cap`),
and this design's `Tail::Graph { cap }` is where the second phase goes; the
executor view and `reported_tail_now()` of 022 §12.4 are built with it.

Rule: a derived figure is computed once, at one point (job start or the fork
build), and stored in the `Plan` the bounce hands the pipeline; nothing
re-derives it later.

## 4. The pipeline API (`tutti-export`)

`tutti-export` becomes the pipeline a bounce drives, and stays usable alone.
Every type has private fields, a validating constructor, and getters. None has
a `Default` for rate, format or duration.

```rust
pub fn render(
    graph: &mut RenderGraph,
    job: &Job,
    clock: &dyn RenderClock,
    outputs: Vec<Output>,
    control: &mut Control<'_>,
) -> Result<Outcome<Vec<Produced>>>;
```

This differs from the brief's `render(graph, &Job, clock, Target, Control)` in
one place: stems need several sinks in one pass, so the fourth argument is a
list of `Output`s, each a `Target` plus the feed it reads (see section 10). A single target
is `vec![Output::master(target)]`.

| Type | Fields (all private; getters) | Constructor / notes |
|---|---|---|
| `Job` | `rate: SampleRate`, `length: Samples`, `latency: Samples`, `tail: Samples`, `on_event_loss: EventLossPolicy` | `Job::new(rate, length, latency, tail) -> Result<Job, JobError>`; refuses a non-finite or non-positive rate, a zero `length`, and a total (latency + length + tail) that overflows or exceeds `MAX_FRAMES`. `with_event_loss(..)`. In frames only: no seconds, no beats. |
| `Output` | `feed: Feed`, `target: Target` | `Output::master(target)`, `Output::tap(reader, placement, target)` |
| `Feed` | `Master` (the graph's outputs, the fold to the file width) or `Tap { reader: TapReader, lanes: Placement }` | `Placement` is `{ topology: ChannelTopology }`, built by the session and checked against the target's layout (§7.3). |
| `Target` | `File(FileTarget)` or `Buffers(BufferTarget)` | |
| `FileTarget` | `path`, `format: Format`, `channels: ChannelLayout`, `topology: Option<ChannelTopology>`, `resample: Option<Resample>`, `dither: Dither`, `normalize: Option<NormalizeSpec>` | `FileTarget::new(path, format, channels)`; dither defaults to `Triangular`; validates the format/width pair (Vorbis topology rules) at construction. |
| `BufferTarget` | `channels: ChannelLayout`, `topology: Option<ChannelTopology>` | Memory sink; no resample (as today). |
| `NormalizeSpec` | `mode: Normalize`, `gain: GainSource` where `GainSource` is `Own` or `FromOutput(index)` | `Own` measures this output; `FromOutput(i)` applies the gain measured on output `i` (`StemGain::Shared`). |
| `Normalize` | unchanged: `Peak { target }`, `Lufs { target, ceiling }` | Policies from E1: silent or below-gate input is `Error::Unmeasurable`; integer targets keep one LSB plus the dither amplitude of headroom below the ceiling. |
| `Control<'a>` | `cancel: &'a CancelToken`, `progress: &'a mut dyn FnMut(&Progress)` | `Control::new(&token)`, `.on_progress(&mut f)`, `Control::none()` (a token never cancelled). Replaces every `_with` twin, `Flow` and `Pass`. |
| `CancelToken` | `Arc<AtomicBool>` | `Clone + Send + Sync`; `cancel()`, `is_cancelled()`. The session's `ExportHandle` wraps one. |
| `Progress` | `stage: Stage`, `fraction: f32` in `[0, 1]`, `output: Option<usize>`, `frames: Option<(Samples, Samples)>` | Read-only; `Copy`. Never decreases within a render. |
| `Stage` | `Preparing`, `Rendering`, `Measuring`, `Writing`, `Finalizing` | `#[non_exhaustive]`. |
| `Outcome<T>` | `Finished(T)`, `Cancelled` | Unchanged. |
| `Produced` | `File(Written)` or `Buffers(Rendered)` | One per `Output`, in order. |
| `Written` | `path`, `bytes`, `frames: Samples`, `dropped_events: u64`, `clipped_samples: u64`, `gain: Option<Db>`, `measured: Option<Loudness>` | Private fields, getters. `clipped_samples` counts samples the quantizer clamped, and non-finite ones (E1, built). |
| `Rendered` | `planes`, `rate`, `dropped_events` | `Rendered::new(planes, rate, dropped) -> Result<_, RenderedError>` checks equal plane lengths, at least one plane, valid rate. `frames()`, `channels()`, `planes()`, `into_planes()`. No public fields. |
| `RenderGraph` | an installed, paired `(Editor, Executor)` plus `Setup { pool, root }` | `RenderGraph::assemble(editor, executor, Setup) -> Result<_>` checks pairing and rate. `prepare(rate)`, `reported_latency()`, `reported_tail()` stay. The rest of the fork machinery is not here (§5). |

Built today, for comparison: `Written` has `path`, `bytes`, `dropped_events`,
`clipped_samples` and `normalized` (no `frames`), and `Rendered` has public
`planes`, `sample_rate` and `dropped_events`. `Job`, `Output`, `Feed`, `Target`,
`Stage`, `CancelToken` and `render` do not exist yet; `Control`'s role is played
by `Flow` and `Pass`.

What leaves `tutti-export`'s public surface: `ExportConfig`, `RenderConfig`,
`EncodeConfig`, the five `render_*` functions and `write_buffers`,
`render_normalized_to_file{,_with}`, `Flow`, `Pass`, `beats_to_seconds`,
`duration_to_frames`, `GRAPH_MAX_BLOCK`'s role as a caller constant (the
session owns the tap capacity rule, §7.4), `RenderGraph::{new, fork, gather,
build, set_pool, set_root_topology, root_topology, editor, editor_mut, commit,
is_parallel}`.

`render` stays one synchronous call: the caller supplies the thread.

## 5. Ownership map

| Concern | Owner | Why |
|---|---|---|
| Range from transport/loop, beats to frames | `tutti-session` (one private conversion over the project tempo source) | Reads session and transport state; `Range::Beats` stays in the API. The tempo model is doc 020 (tempo maps). |
| Offline clock from the tempo source | `tutti-transport` (doc 020) | It is the clock. |
| Latency and tail figures | `tutti-graph` (plan latency) and `tutti-types` (`graph_tail`); `tutti-session` resolves them at the fork | The plan owns them; the cap and trim are bounce policy. |
| Fork gather/build, pool, root topology, edit hook, plugin Offline/Realtime, export token | `tutti-session` (private `Fork`) | Needs the live graph and hosted plugins. |
| Opening stem taps on the fork, naming them | `tutti-session` | Needs node keys and the fork's editor; uses `Editor::open_tap`. |
| Stem placement and channel fold construction | `tutti-session` builds `Placement`; `tutti_types::Fold` does the fold | Control-side build, no allocation on render (CLAUDE.md "Buffers"). |
| Block loop, tap draining, fold to file, resample, dither, normalize, encode, temp-file commit, event-loss and fork-health checks | `tutti-export` | The pipeline. |
| Progress fraction, stage, cancel checks | `tutti-export` (one `Control`) | The crate knows the passes; hosts never weight them. |
| Dither and resampler algorithms | `tutti-dsp` (E5) | CLAUDE.md: pure algorithms in `tutti-dsp`. |
| Codecs, `Format` | `tutti-codec` | Unchanged; pure Rust, no new C libraries. |
| `Session::{bounce, bounce_start, finish_bounce}`, `BounceJob`, `BounceHandle`, `BounceError` | `tutti-session` | |
| Entity to `NodeKey` resolution, `BounceDone`, `BounceStage`, `BounceInFlight`, the `World` hook | `bevy-tutti` | The adapter adds only what needs Bevy. |
| `ExportClock`, `PrepareEdit`, `PreparedGraph` | `tutti-session` (kept) | They are session vocabulary; `PreparedGraph` exposes only `fresh_key` and the timeline, not the `RenderGraph`. |

## 6. Progress and cancellation

### 6.1 Stages and fraction

`Progress::fraction()` is owned by `tutti-export`. Weights are constants in one
table; a host never combines passes.

Without normalize (streaming; encoding happens during the render):

| Stage | Fraction range | Advances by |
|---|---|---|
| Preparing | 0.00 to 0.05 | Session steps (gather, fork build, edit, tap open, commit); reported as discrete steps, `frames: None`. |
| Rendering | 0.05 to 0.95 | Rendered frames of `latency + length + tail`; includes the per-block encode of every output. |
| Finalizing | 0.95 to 1.00 | Outputs finished (resample flush, encoder finish, fork-health check, rename), one step per output. |

With normalize on any output (render spills, then measures, then encodes):

| Stage | Fraction range | Advances by |
|---|---|---|
| Preparing | 0.00 to 0.03 | As above. |
| Rendering | 0.03 to 0.55 | Rendered frames. |
| Measuring | 0.55 to 0.62 | Frames measured of the measured outputs (chunked). |
| Writing | 0.62 to 0.95 | Frames written across all outputs (resample plus gain plus encode), total = sum of output lengths at the file rate. |
| Finalizing | 0.95 to 1.00 | As above. |

Rules: the fraction is monotonic; it reaches 1.0 only after the last rename;
`output: Some(i)` names the output being measured, written or finalized, and is
`None` while rendering (one render serves all). A stem count does not change a
stage's range: a render pass is one pass for N outputs; a write pass spans all
outputs by total frames.

Delivery: the pipeline calls the progress callback at least every 4096 frames
or 50 ms of wall time, whichever is later, and at every stage change (not every
64 frames). The session stores the latest `Progress` in the handle (a
`Mutex<Progress>`, no waiting on the render); the bevy adapter also triggers
`BounceStage` on a stage change.

### 6.2 Cancellation points

`Control::cancel` is checked at each point; a cancel returns
`Outcome::Cancelled` and removes every temporary file, leaving every target path
as it was.

| Point | How |
|---|---|
| Before the gather, after the gather, after the fork build (plugin-server launch), after the edit hook | Session, between steps. A gather already done goes back to the main thread to drop. |
| Every rendered block | Pipeline block loop. |
| Between stages | Render to measure to write to finalize. |
| During the normalize measurement | The measurement runs in chunks of at most 1 s of audio and checks between chunks (needs E1's chunked `Measurer`). |
| During the resample flush and the write pass | Per chunk of the converter and per written block. |
| Before each output's encoder finalize | Between outputs. A single encoder's finalize (header patch, FLAC trailer, last Vorbis pages) is not interruptible: every format's finalize is bounded work independent of file length (a flush, not a re-encode), so it is documented as the one wait. |
| Before the commit | A last check after every output is finalized and fork health is confirmed, before the first rename. |

The commit is the one step cancel does not reach: it is the renames of the
finished temporaries into place. Fork health and event loss are checked before
it (audit item 3).

## 7. Stems via plan taps

### 7.1 One pass, one fork

One fork of `ForkTarget::Master`. After the edit hook runs, the session opens
one plan tap per stem on the fork's editor (`Editor::open_tap`), then commits.
The pipeline reads each tap's `TapReader` after every block and writes it
through that stem's own chain (placement fold, optional resample, optional gain,
dither, encoder). The master reads the graph's outputs as today. A node a stem
names must exist in the fork, which holds exactly the nodes an output reaches;
a node no output reaches is refused at start (`StemNotAudible`). A stem is the
node's own output: what downstream buses or the master apply is not in it (the
meaning `ExportSource::Node` had).

### 7.2 Alignment and PDC

| Question | Answer |
|---|---|
| Sample-aligned with the master? | Yes. Stem taps use `TapAlign::ToOutput`: the compiler delays each lane by the same compensation an output channel of the same sources gets, so every stem and the master leave the plan at the plan's output time. One latency trim (the plan's total latency) applies to all targets; one length and one tail apply to all. |
| State-aligned? | Yes: one fork, one set of units, one reset, one clock. A plugin renders once. |
| A source later than every output | Cannot be aligned (it is read raw). That happens only for a node no output reads, which `StemNotAudible` already refuses; the check reuses the fork's reachability, not a second rule. |
| PDC changes after the taps open | None can: the fork's topology is fixed once committed; the edit hook runs before the taps open. |
| Tail | One tail, from the master's graph tail plus untrimmed latency. A stem's own decay is inside it, because a stem is a subset of the master's signal. |
| Equal lengths | After the same resample ratio every target has the same frame count (floor-checked in a test). |

### 7.3 Placement: how a stem's channels are declared

A stem names a node and, by default, all of its output channels, in port order.
The placement is declared on the `Stem`:

```rust
pub struct Stem<N = NodeKey> {
    node: N,
    path: PathBuf,
    channels: Option<Vec<u16>>,       // node output channels to tap; None = all
    layout: Option<ChannelLayout>,    // file width; None = the tapped width
    topology: Option<ChannelTopology>, // what the tapped channels are; None = assumed(width)
    format: Option<Format>,           // None = the bounce's format
    // normalize via the bounce, not here
}
```

| Declaration | Meaning |
|---|---|
| `Stem::new(node, path)` | Every output channel of `node`, file width = that width, topology = `ChannelTopology::assumed(width)` (mono, stereo, quad, 5.1, 7.1, 7.1.4), else discrete by index. |
| `.channels([0, 1])` | Tap a subset or reorder; the tap has that many lanes, one `Source::Node(PortRef { node, port })` each. A channel past the node's width is refused at open (`MissingSource`, surfaced as `StemChannelMissing`). |
| `.topology(t)` | States what the tapped channels are (`t.width()` must equal the lane count); the one place a stem says "these two are Ls, Rs". |
| `.channels_layout(layout)` / `.layout(layout)` | The file's width. If it differs from the tapped width, the session builds `Fold::widths` or `Fold::to_width` from `topology` to `layout` at start. A fold that cannot place (`FoldError`) is `ConfigError::Placement { target }` at start, not at render; a narrowing with no placement is an error, never a truncation. |

The master's root declaration is never applied to a stem: the root describes
the graph's output bus, not a node's output (audit item 12).

### 7.4 Tap lanes and widths

| Item | Rule |
|---|---|
| Lanes | One `TapId` per stem; one lane per tapped channel; the `TapReader` reads all lanes together (they cannot tear). |
| Ring capacity | Each lane holds `2 x max_block` frames (the fork's `Prepare::max_block`), because the pipeline drains right after each block on the same thread. |
| Loss | `TapReader::take_lost()` is read after every block. A non-zero count means the ring was undersized; the bounce fails with `Error::TapOverrun { output, lost }` instead of writing a hole. With the rule above it is unreachable on a correct pipeline; it is the guard, not a mode. |
| Pre-fader | A tap sees the signal before the engine's declick fader and the fold to the device width. A bounce has no fader; the master's fold to the file is the pipeline's. |
| Closing | The taps live on the fork; dropping the fork drops them. A fork carries none from the live graph. |
| Master | Not a tap: it reads the executor's output buffer. A master-only bounce opens no taps. |

### 7.5 Normalize: per target versus shared

Normalize needs the whole signal, so a normalized bounce renders once into a
float spill (memory up to a budget, then a temporary file beside the targets;
section 10), measures, then runs the write pass over the spill. It never re-renders.

| Mode | Behaviour | Use |
|---|---|---|
| `.normalize(n)` | Applies to the master only. | The default for the master. |
| `StemGain::None` (default) | Stems are written as rendered. | A stem set for a DAW that adds its own gain. |
| `StemGain::Shared` | Each stem takes the master's measured gain (`GainSource::FromOutput(0)`). | Stems that still sum to the master. Valid only when the master is the plain sum of the stems; a graph with master-bus processing is the caller's responsibility, documented. |
| `StemGain::Each(n)` | Each stem is measured and normalized on its own. | Independent stem libraries. Breaks balance by design. |

A clipped sample is counted per output in `Written::clipped_samples`. E1's headroom
rule applies per output at its own integer width.

## 8. Errors

Two enums, split by where the fault is known.

`tutti_export::Error` (pipeline; `#[non_exhaustive]`; no raw `f64` or `u16`
fields):

| Variant | Meaning |
|---|---|
| `Io(io::Error)` | |
| `Encode(tutti_codec::Error)` | |
| `Resample(ResampleError)` | |
| `Job(JobError)` | `Job::new` refused: rate, zero length, overflow. |
| `Rendered(RenderedError)` | `Rendered::new` refused. |
| `Target { output, why: TargetError }` | Format/width pair the encoder cannot take, topology the format cannot place (`Placement`). |
| `Unmeasurable { output, why: Unmeasured }` | Built today as `Unmeasurable { channels, sample_rate, reason: Unmeasured }` with `Unmeasured::{Format, BelowGate, Silent}`. This design adds `output` and folds `Format` into the width and rate of the output it names. |
| `TapOverrun { output, lost }` | §7.4. |
| `EventsDropped { count }` | |
| `ForkFailed { key, kind, cause }` | Checked before the commit. |
| `NotCommitted { landed: Vec<PathBuf>, cause }` | A rename failed after earlier ones landed; the files that did land are listed (see section 10). |

Removed from it: `UnsupportedChannels` (only ever 0), `ResampleStalled` stays
as a `Resample` cause, `ConfigError`'s `UnpairedGraph` (the sealed constructor
makes it unreachable) and the session-only variants.

`tutti_session::BounceError` (`#[non_exhaustive]`):

| Variant | Meaning |
|---|---|
| `InFlight` | Another export or bounce holds the token. |
| `Config(BounceConfigError)` | See below. |
| `Fork(ForkError)` / `NotForkable { node }` | A node cannot be forked. |
| `Export(tutti_export::Error)` | The pipeline's. |

`BounceConfigError` (session-only; moved from export's `ConfigError`):
`Missing(Field)` (format, a target), `GraphHasNoOutputs`, `NoRange`, `EmptyRange`,
`StemNotInGraph { node }`, `StemNotAudible { node }`, `StemChannelMissing { node,
channel }`, `DuplicatePath(PathBuf)` (two targets name one file, checked before
any work), `Placement { target, why }`, `PrepareHookUncommittable(CommitError)`.

bevy maps `NodeKey` back to `Entity` in these; it adds none of its own except
the entity that failed to resolve.

## 9. Migration

### 9.1 Removed or made private

| Item | Fate |
|---|---|
| `tutti-export`: `ExportConfig`, `RenderConfig`, `EncodeConfig`, `render_to_file{,_with}`, `render_to_buffers{,_with}`, `render_normalized_to_file{,_with}`, `write_buffers` | Removed; replaced by `render` plus `Job`/`Output`/`Target`. |
| `Flow`, `Pass`, the `_with` twins | Removed; `Control`. |
| `RenderGraph::{new, fork, gather, build, set_pool, set_root_topology, root_topology, editor, editor_mut, commit, is_parallel}` | Gone from the public surface; the session owns a private `Fork` with the same operations; `RenderGraph::assemble` is the one constructor. |
| `tutti_export::beats_to_seconds`, `duration_to_frames` | Retired (no outside callers); the session's single beats-to-frames conversion replaces them. |
| `tutti_export::ConfigError` session variants | Move to `BounceConfigError`. |
| `Rendered` public fields | Private. |
| `tutti-session`: `ExportRequest`, `ExportSource`, `ExportTarget`, `ExportOutput`, `Session::{export, export_blocking, finish_export{,_unannounced}}`, `ExportJob`, `ExportRendered`, `ExportHandle::default` | Replaced by `Bounce`, `Session::{bounce, bounce_start, finish_bounce}`, `BounceJob`, `BounceHandle`. `ExportClock`, `PrepareEdit`, `PreparedGraph` stay. |
| `bevy-tutti`: `ExportRequest`, `ExportPlugin`'s mirror fields, `ExportDone`, `ExportInFlight` | `BounceRequest`, `BounceDone`, `BounceInFlight`, `BounceStage`; the plugin is renamed `BouncePlugin`. |

### 9.2 Callers

| Caller | Change |
|---|---|
| `crates/tutti/examples/headless_export.rs` | Builds a `Session` and calls `session.bounce(..)`; the buffers and mono-fold cases use `Bounce::master_buffers()`. Drops its five-crate import list. |
| `crates/tutti/examples/plugin_midi_export.rs` | `Bounce` with `.edit(..)` for the MIDI clip; range from the clip's beats. |
| `crates/tutti-export/examples/export.rs`, `render_export_cases.rs`, `benches/offline_render.rs` | Standalone pipeline use: build `RenderGraph::assemble`, `Job::new`, `render(.., Control::none())`. Stays the proof that the crate works without the session. |
| `crates/tutti-export/tests/*` (cancel, dither_stats, event_loss, oracle_*, roundtrip, sampler_to_export, surround_export, graph_source, render) | Same assertions through `render`; cancel tests gain the new cancellation points; event-loss and cancel tests use `CancelToken`. No assertion is relaxed. |
| `crates/tutti-session/tests/{export_cancel,root_topology,plugin}.rs` | Through `bounce_start`/`BounceHandle`; `root_topology` asserts the master's declaration and a stem's own. |
| `crates/bevy-tutti/tests/{export_cancel,export_fork,export_surface}.rs`, `plugin_capture`, `mod_lanes`, `mod_plugin` | `BounceRequest`; the `plugin` tests need `cargo build -p tutti-plugin-server` as today. |
| `crates/bevy-tutti/examples/offline_export.rs` | `BounceRequest`. |
| `crates/tutti-plugin/tests/clap_fork.rs` | Uses `RenderGraph::assemble` with a session-supplied fork, or moves to the session's bounce. |
| `README.md` files (tutti-export, bevy-tutti) | Rewritten to the new surface; the stale "does not normalize" and "no builder" lines go. |
| `CLAUDE.md`, doc 015 §4.3 | The export lines in the workspace table and the "offline render" role update in the last PR. |

## 10. Decisions and open questions

Decided by the owner, 2026-11-08 (the answers to the questions this section
first asked):

| # | Decision |
|---|---|
| Q1 | With no loop set, nothing is guessed: `NoRange` is an error unless `.range(..)` or `.until(Beat)` is given. bevy may fill `until` from its clips. |
| Q2 | `render` takes a list of `Output`s, with a one-target helper (this is the deviation in section 4, confirmed). |
| Q3 | A normalized render keeps its audio in memory up to 512 MiB, then spills to a temp f32 file beside each target. It never renders twice. |
| Q4, Q8 | A stem is the node's own dry output, with no downstream sends; a bus stem is a stem of the bus node. The default `StemGain` is `None`; `Shared` and `Each` are opt-in. |
| Q5 | `bounce` blocks on the caller's thread; `bounce_start` is for threads. |
| Q6 | The tail cap is 10 s. |
| Q7 | No rollback on a partial rename: return `NotCommitted { landed }`. |

No question is open.

## 11. Implementation plan

Each step is one PR, keeps `cargo xtask ci` green, and passes its own tests; E1, E2
and E3 land first or are rebased on. Steps 1 to 4 are `tutti-export` and can
proceed in parallel with E3.

| # | PR | Content | Keeps green by |
|---|---|---|---|
| 1 | `Job`, `Rendered`, `Written` | Add `Job` (validating), seal `Rendered`, extend `Written` (`clipped_samples` is built; `gain` and `measured` are not: `Written::normalized` carries the gain today). | The old config types call `Job::new` internally; callers are unchanged except `Rendered` field access, migrated in the PR. |
| 2 | `Control`, `Progress`, `Stage`, `CancelToken` | Replace `Flow`/`Pass`/`_with`; add the weight table; cancel checks in measure, resample flush, finalize (needs E1's chunked measurer). Session's `ExportHandle` wraps `CancelToken`. | The five `render_*` functions keep their names and lose the twins in the same PR; callers migrate in it. |
| 3 | `Output`, `Feed`, `Target`, `render` | The multi-sink pass, tap draining, the shared-gain normalize and the spill; `render` replaces the `render_*` functions, which are deleted with their callers' migration (tests, examples, session). | One PR, because the old and new entry points cannot both own the block loop. |
| 4 | Sealed `RenderGraph` | Move `gather/build/set_pool/set_root_topology/editor_mut/commit` into a private session `Fork`; add `RenderGraph::assemble`; move the session-only `ConfigError` variants to the session. | Session is the only caller of the machinery; `clap_fork.rs` migrates here. |
| 5 | `Bounce`, master only | `Bounce`, `Range`, `Latency`, `Tail`, `Stem` (declared, rejected as unsupported until step 6), `Session::{bounce, bounce_start, finish_bounce}`, `BounceHandle`, `BounceError`; derived defaults including the project-tempo clock; `ExportRequest` and friends deleted in the same PR with `tutti` examples migrated. bevy still compiles against a temporary thin wrapper over `Bounce`. | The wrapper is removed in step 7. |
| 6 | Stems | Tap opening on the fork, `Placement`, `StemGain`, `TapOverrun`, alignment test (stem equals master when the stem is the only source; stems sum to the master within one LSB), serial-versus-parallel bit identity with stems, cancel tests for every point in §6.2. | Additive; a bounce without stems is unchanged. |
| 7 | bevy | `BounceRequest`, `BounceDone`, `BounceInFlight`, `BounceStage`, `BouncePlugin`, entity resolution, prelude re-exports; delete the mirror and the wrapper; tests and example migrated. | |
| 8 | Docs and cleanup | READMEs, CLAUDE.md workspace table, doc 015 §4.3 note, this doc's status line, `check-adapter.sh` exception review. | |

Test policy applies throughout: every new case is mutation-tested (break the
property, watch it fail, restore, note the mutation on the case); no existing
test is relaxed; the parallel path is compared to the serial render bit for bit;
golden digests, if any, go through `assert_golden`.
