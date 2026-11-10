# 025: streaming, the edge traits and the disk streamer

Two things in tutti are called a stream. At the edge of the engine a **source**
or **sink** moves frames in or out on a clock the graph does not keep: a
microphone, a file, a tap, a WAV encoder. Inside the sampler a **disk stream**
keeps a ring of decoded frames ahead of a voice that reads it by position, at
any rate, backwards if it likes. They share a name and nothing else, and today
they are described by one vocabulary that fits the first and not the second.
This design gives each its own shape, removes the runtime rules a type can
carry, and orders the work as PRs that each stay green.

Status: design. Stages 0, 1 and 2 have landed (§8). Stage 3 waits for the
`tutti-io` dissolution PR (§8); the rest is not started. Nothing in this doc
changes code. The decisions are the owner's of 2026-11-07, all taken as
recommended, and are recorded with their grounds in §9.

## Summary

- **Two shapes.** The sequential edge is `Pull` plus `Source` and `Sink`,
  replacing `AudioIn`, `AudioOut` and `OnEmpty`. The disk side is positional:
  `Stream` (control), `StreamReader` (audio, handed out by value) and `Block`
  (one block's claim).
- **`Pull` carries the verdict.** `Frames`, `Starved`, `Ended` or `Failed` come
  back from the call, so `ON_EMPTY`, `is_failed` and the second poll in the
  pump go.
- **The reader is created at open and handed back by value.** That removes
  `TakeVoiceError::ReaderTaken` and `NotStreaming` as runtime states, and with
  them a silent failure: a file that cannot be opened leaves a stream
  `NotStreaming` forever.
- **No `tutti-stream` crate.** `Pull` lives in `tutti-types`, the streamer
  stays in `tutti-sampler`.
- **creek is not adopted.** It is the right model for a cued, forward, 1x
  stream and for ownership by message passing; it is not a streamer for
  varispeed, reverse and looped voices.
- **Adapters (`Resample`, `Dither`) wait** for a second pipeline. `Cue` waits
  for a measured gap.
- **The streamer is named for what it is** (the `butler` rename, §6), the
  priority it asks for is honest and already landed, and it sleeps on a
  doorbell instead of polling (§7).

## 1. The problem

### 1.1 One vocabulary, two jobs

`tutti_types::AudioIn` and `AudioOut` (`crates/tutti-types/src/io.rs`) are
traits over a flat interleaved `&[S]` with a runtime `ChannelLayout`, counted
in frames. They work for a source that is pulled in order. Their module docs
say more than that: every source and sink in tutti speaks them, "mic, decoded
file, disk stream, plugin boundary, WAV encoder". Four of those do not:

- the **disk stream** has its own positional reader (`PosReader`, `PosClaim`);
- the **plugin shm slab** says in its own docs that it does not implement them
  (`crates/tutti-plugin/src/util/transport/shm/slab.rs`), because it is planar;
- `tutti-plugin-types`' `channels.rs` frames a plugin as "an `AudioOut`
  back-to-back with an `AudioIn`", though in the graph a plugin is a node and
  the slab is a transport;
- **export** does not use them: it has a private `FrameSource` of its own
  (§5.4).

The "neural generator" and "network socket" examples describe nothing that
exists. What implements the traits today is `MicIn`, `TapIn` and `FileIn`
(sources) and `WavOut` (the one sink), plus about twenty test and doc types.

### 1.2 The verdict is in three places

An empty poll means nothing by itself. Whether the source is merely behind,
finished, or broken is read from a `const ON_EMPTY: OnEmpty` on the type and a
separate `is_failed()` method, and `PumpLoop::pump_once` polls a second time
when the fault flag is set, because a live source can push its last frames
between the empty poll and the check. Three signals, one of which is an
ordering obligation the caller discharges for every source.

`FileIn` showed the cost. It kept a decode fault in `error()`, returned the
frames before it and then 0, and did not override `is_failed`, so a corrupt
file read as a clean end through the trait. That one is fixed (§8, stage 0);
the shape that allowed it is not.

### 1.3 The reader handoff is a runtime rule

A disk stream has one reader. Today that is enforced at run time:
`Ring` holds `reader: Mutex<Option<PosReader>>`, `Ring::take_reader` returns
`TakeVoiceError::ReaderTaken` the second time, and `take_streaming_unit`
returns `NotStreaming` until the streamer thread has installed the channel's
`Link`. The caller polls (`streamer.rs`' own example loops on `NotStreaming`
with `yield_now`). Worse, `handle_stream_file` returns silently when
`load_wave` cannot turn the file into audio ("there is no caller left to report
to on this thread"), so a bad path is `NotStreaming` for ever, not an error.

The ring's channel width comes from the file header, so the ring can only be
built on the streamer thread, after the probe. The handoff cannot be a plain
return value from `open`; it has to be a value the control side receives later.

### 1.4 The streamer is keyed by a channel number

A stream is found by `channel_index` in a `DashMap<usize, ChannelPlan>`, holds
a `Link` and a `StreamRecord`, and is ended by `Link::drop` plus
`StreamRecord::end`. The plan map, the second command mapping and
`StreamOrigin` exist to connect a voice, a channel number and a stream. A
stream has no identity of its own, and dropping it does not end it.

### 1.5 What a stream is not

A voice reads by position at a varying rate, four taps wide for the Hermite
interpolator, in reverse, with a second range for a crossfade continuation
(`LiveRead::render` in `voice/live_read.rs` claims `[(lo, hi), old_range]` and
then lends the ring's map). It cannot be written as `pull(n)`. A sequential
`pull` on the ring has no production caller and would be dead code.

## 2. The design

### 2.1 The boundary, in one sentence

An **edge** (`Source`, `Sink`) is a stream on a clock other than the graph's,
read by pulling interleaved frames that may starve or end. A
`tutti_graph::Node` is called by the graph's clock, planar, one block at a
time. The rings in `tutti-graph` are the one place an edge meets the graph: the
plan tap going out, the inlet coming in, and the engine's heard tap.

### 2.2 The sequential edge (`tutti-types`)

```rust
#[must_use]
pub enum Pull<V = Samples> {
    Frames(V),   // at least one frame
    Starved,     // nothing yet; more may come
    Ended,       // nothing more, ever
    Failed,      // broken; frames before it came with earlier pulls
}
pub enum Stop { Starved, Ended, Failed }
impl<V> Pull<V> {
    pub fn ready(self) -> Result<V, Stop>;            // the stable stand-in for `?`
    pub fn map<U>(self, f: impl FnOnce(V) -> U) -> Pull<U>;
    pub fn stop(&self) -> Option<Stop>;
}

pub trait Source {
    fn layout(&self) -> ChannelLayout;
    fn pull(&mut self, out: &mut [f32]) -> Pull;       // frames, never samples
}
pub trait Sink {
    fn layout(&self) -> ChannelLayout;
    fn write(&mut self, interleaved: &[f32]) -> Result<(), SinkFailed>;
    fn finalize(self) -> std::io::Result<()>;          // the detail of the first failure
}
```

Gone: `ON_EMPTY`, `OnEmpty`, `is_failed` on both traits, the generic `S`, and
the pump's second poll.

The rules:

- `Frames` and a verdict never arrive together: frames now, `Failed` or `Ended`
  on the next call. `FileIn` returns `Failed` where a clean end read before
  stage 0.
- A live source reads its fault flag **before** it reads the ring, and returns
  `Failed` only if the ring was empty. This moves the one ordering obligation
  from every caller's `pump_once` into `MicIn::pull`.
- A synchronous source (`FileIn`, export's planes, a graph render) never
  returns `Starved`. One enum serves both: two types would make every adapter
  generic over both, and the cost of one is a match arm that cannot occur,
  which `ready()` maps to `Err`.
- `Sink::write` returns a marker and `finalize` the detail, because a sink is
  synchronous and on the caller's thread: "write, then ask `is_failed`" is
  exactly `Result` with no ordering subtlety. `WavOut` (becoming `FileOut`)
  keeps its first `io::Error` for `finalize`.
- `?` on `Pull` needs the unstable `Try`. The two stable forms are
  `pull.ready()?` into `Result<V, Stop>` for control-side loops and
  `let Pull::Frames(v) = reader.pull(n) else { silence; return };` on the audio
  side. No macro.

`Sink` stays a trait with one production impl: it keeps the recorder generic
over its destination, and a second sink is likely (§9, decision 4).

Dropping the generic `S`: every production impl is `f32`, only tests use
another element type, and the twenty test impls change anyway.

`pump` is behaviour sitting in `tutti-types`, which is meant to be vocabulary.
It is small (poll a block, write it, return frames), but stage 3 is where to
decide whether it moves into `PumpLoop` in `tutti-session` or stays as the one
primitive (§10).

### 2.3 The disk streamer (`tutti-sampler`)

```rust
impl DiskStreamer {
    pub fn open(&self, path: &Path, place: Placement) -> Opening;   // control thread, never blocks
}
pub struct Opening { .. }
impl Opening {
    pub fn poll(self) -> Open;                // consumes: the reader can be received once
}
pub enum Open { Pending(Opening), Ready(Stream, StreamReader), Failed(OpenError) }

pub struct Stream { .. }       // control side. Drop ends the stream and frees its ring.
impl Stream {                  // commands to the streamer
    pub fn seek(&self, frame: u64) -> Result<(), StreamerGone>;
    // set_loop, set_speed, set_direction likewise
}

pub struct StreamReader { .. } // audio side, !Clone, moved into the voice
impl StreamReader {            // (wraps PosReader and Reader<RingMap>)
    pub fn block(&mut self, play: u64, reads: [Range<u64>; 2]) -> Block<'_>;
}
pub struct Block<'r> { .. }    // !Send; borrows the reader; slot claim and Loan<RingMap>
impl Block<'_> {
    pub fn holds(&self, pos: u64) -> bool;
    pub fn frame(&self, pos: u64) -> PosFrame<'_>;   // PosClaim::frame today
    pub fn map(&self) -> &RingMap;
}
```

| Rule | Mechanism |
|---|---|
| A stream has one reader | `StreamReader` is not `Clone` (`PosReader` already is not); `Opening::poll(self)` hands it out once; no `Mutex<Option<PosReader>>` |
| A block's frames are not read after the next block starts | `Block<'r>` borrows `&mut StreamReader` |
| The view stays on the audio thread | `!Send` via `PhantomData<*const ()>` |
| Lend the map once per block, never park it | the `Loan<RingMap>` lives inside `Block`, so it ends with the borrow; "forgot to lend" and "parked the loan" become unrepresentable |
| A dropped `Stream` ends the stream | `Drop for Stream`, replacing `Link::drop` and `StreamRecord::end` |
| A pull's verdict is not ignored | `#[must_use]` on `Pull` |
| Registering a `Reader` allocates, so it happens on the control side | `Opening` registers `ring.map_reader()` before returning `Ready` |
| A file that cannot be opened is an error | `Open::Failed(OpenError)`, where `handle_stream_file` returns silently today |

Each row that is a compile-time property gets a `compile_fail` doctest with a
compiling twin, and a mutation (remove the borrow, the `PhantomData`, the
`must_use`) that makes it fail.

**Why `seek` is on `Stream` and there is no `Seeked`.** Voices do not seek:
they pass a new `play` to `claim`, a jump is detected from the position, and
`holds(pos)` already answers "cached". The one seek that exists, the
free-running reader's, goes through `Ring::request_seek` (epoch and target) as
a command from the control side.

**Why `Opening` and not a blocking `open`.** A file that cannot seek is decoded
whole on the streamer thread (`load_wave`); a blocking `open` would do that on
the caller's. A polled open also needs no thread of its own.

**Why not a sequential `pull(n)` on the ring, or a lent `&[f32]`.** The first
has no caller. The second cannot be lent without `unsafe` in `tutti-rt` (the
protocol would make it sound, since a claimed range is never written) and the
gain is small: copying 512 stereo frames out of the claim costs about 110 ns.
Not recommended.

**Combinators on the view.** `copy_to` (a kernel that finds the slot once and
walks at most two contiguous strips) earns its place if a bulk reader exists.
A frame iterator over the view, measured, runs about six times slower at
stereo and three and a half to four at six channels (about 680 ns against
110 ns per 512-frame block): bounds checks and a division per frame, not the
slot arithmetic. In absolute terms that is 0.006 percent of a 10.7 ms block, so
the choice is simplicity: the voices use `PosFrame` per tap and need no
iterator. On the result, `ready`, `map`, `stop` and `frames_or_silence(out)`
(starved, ended and failed all become silence) earn a place.

### 2.4 `Cue`: deferred

A `Cue` would own a decoded start block of a file, so a jump outside the ring's
window sounds at once. It does not duplicate preroll (a read offset) and not
`StreamPin` (which pins a whole decoded file for the stream's life, so a
resident file already seeks instantly). It matters only for a jump on a large
non-resident file: a transport loop, a clip start. The reader already hides the
gap by playing the old side on until the new one sounds, and the gap has not
been measured, so there is no demonstrated defect. If it is added, it is an RAII
pin in the existing `LruCache`, keyed by range, not a third cache. Not in the
first stages (§10, unknown 1).

## 3. creek, and why not

creek (`creek 1.2.3`) is the closest model: a read side over `rtrb` queues, an
IO server thread, a cache that is a named, caller-managed cue, `seek` returning
whether it hit. We compared it with the streamer by reading its source (not
running it).

| | creek | the streamer |
|---|---|---|
| Memory | blocks moved by value through two queues; the audio side gets real planar slices | one shared ring of per-sample `AtomicU32`; no `&[f32]` exists to lend |
| Threads | one OS thread per stream, `sleep(1 ms)` poll | one thread for all streams |
| Read shape | forward, 1x, sequential; `read(frames)` copies into a read buffer | by position, any order, any rate |
| Cached start | `cache(index, frame)`, `seek(frame, mode)` returning a hit | four frames of history behind the reader; whole-file `LruCache` and `StreamPin` |
| End and fault | typed end of file; a fatal error latches | no verdict on the reader (this design adds `Pull` on the edge, the streamer logs once) |
| Underrun | silence, a `TODO` | counted, ramped back in |
| Drop | `Drop` sends the heap data to the server thread to free | `Arc<Ring>` freed where the graph frees units |

creek is simpler where ownership by message passing needs no protocol proof (no
fences, no loom model, no `unsafe`), where the reader gets real slices, and
where the cache is a named cue. It is **not adoptable**: a varispeed or reverse
voice would `seek` on nearly every block and creek's seek is a cache lookup or
a refill, not a window move; loops with crossfades, PDC preroll and Hermite
taps would be rebuilt above it; and a thread per stream does not suit a
session of many clips. It remains the model for `Cue` and for by-value
ownership, both taken here.

What the streamer carries that creek omits is the feature set, and it does not
shrink by adopting anything: reverse, varispeed and rate conversion by position
reads (`PosRing`, `live_read.rs`), loops and crossfades written into the ring
(`loops.rs`), the rate, direction and underrun cells (`rt_state.rs`), PDC
preroll, the shared file cache, the region map, and a fork of a live voice for
an offline render. Where it is overbuilt is the plumbing: the channel-keyed plan
map, `Link`, the second command mapping, `take_streaming_unit`,
`Ring::take_reader`, `TakeVoiceError`, `placed`, most of `StreamRecord`, the
polling with `yield_now`, and the `Mutex<Option<PosReader>>` handoff. Replacing
those with `Stream`, `Opening` and `StreamReader` removes an estimated 300 to
450 code lines (about 10 to 15 percent of the module).

## 4. Why no `tutti-stream` crate

- `Pull`, `Source` and `Sink` replace the traits in `tutti-types`, which holds
  `AudioIn` now; they need only `ChannelLayout` and `Samples`.
- The streamer stays in `tutti-sampler`. It and the voices import each other:
  from the streamer into the voice side (`LoopSetting`, `voice::types`,
  `voice::interp`, `loop_span`, `disk_voice`), and from the voices into the
  streamer about twenty references (`GUARD_FRAMES`, the command type, `RtState`,
  `StreamOrigin`). A separate crate would need `LoopSetting`, `Direction`, the
  interpolation taps and `RtState` pushed down first, for no consumer that wants
  the streamer without the sampler.
- If adapters are ever pushed (§5.3), they need `tutti_dsp::FftResampler`. A
  crate holding them would depend on `tutti-types` and `tutti-dsp` only and sit
  at layer 3 of `scripts/check-layout.sh`, above `tutti-dsp` and below
  `tutti-session` (where the recorder will live). Until then they are one
  module in the crate that uses them. The layout table is not edited here.

## 5. Where each type lands, and what stays open

### 5.1 Homes after the `tutti-io` dissolution

Another lane is dissolving `tutti-io` (branch `refactor/dissolve-tutti-io`, not
merged when this was written). Stage 3 is written against its end state:

| Today | After |
|---|---|
| `TapIn` | deleted; `tutti_graph::TapReader` implements the edge trait |
| `WavOut` | `tutti_codec::FileOut`, a `Sink` for any `Format` |
| `MicMonitorNode`, `MicRing` | a `tutti-graph` inlet opened by `Graph::open_inlet` |
| `Recorder`, `PumpLoop`, `PumpPass` | `tutti-session`, behind `audio-io` |
| `PumpDriver`, `ManualDriver`, `FinalizeStatus` | deleted |
| `MicIn` (`tutti-cpal`), `FileIn` (`tutti-codec`) | stay; both become `Source` |

### 5.2 What each type removes

| Type | Replaces |
|---|---|
| `Pull` | `OnEmpty`, `ON_EMPTY`, `AudioIn::is_failed`, the second poll in `pump_once`, `WaveIn`'s page of rationale |
| `Source` | `AudioIn<S>`: three production impls (`MicIn`, the tap reader, `FileIn`) |
| `Sink` | `AudioOut<S>`: one production impl (`FileOut`) |
| `Opening` | the polling loop on `NotStreaming`, `TakeVoiceError::{NotStreaming, ReaderTaken}`, `Mutex<Option<PosReader>>`, `Ring::take_reader`, `placed` |
| `Stream` | the channel-keyed `DashMap<usize, ChannelPlan>`, `Link`, `take_streaming_unit`, `StreamRecord`/`StreamOrigin`/`StreamFile` |
| `Block` | the separate claim and `map.lend()` in `LiveRead::render` |

`WaveIn` (the sampler's one-caller wrapper that discards the count) goes or
becomes a plain struct. `FrameSource` is §5.4.

### 5.3 Source adapters: wait

Stacking `gated`, `downmixed`, `resampled` and `dithered` over `Source`, with a
`Stage` marker type so that dithering before resampling fails to compile
(E0271), was prototyped against export's pipeline. The results:

- sample-identical to today's on five configurations; no allocation in a pull,
  flush included; the stack is a nameable type and a struct field with no
  `Box<dyn>`;
- **line-neutral**: 306 code lines before, 302 after (329 with a downmix that
  has no user), because the saving in `pump_blocks` moves into a larger
  `Resample` that owns flush, starvation and failure;
- **one user** (`pump_blocks` and `resample_rendered`, both in export);
- **it changes the writes the encoder sees**: today's resample path calls
  `write` 470 times, 440 of them with zero frames, with non-empty blocks of 320,
  1,280, 1,280 and 640 frames; the stack yields 512-frame blocks. The Ogg
  writer's bytes depend on the split, so a port changes Ogg output unless the
  old split is reproduced, which would reproduce an accident.

**The trigger may already be met.** A recording that writes integer PCM is a
second pipeline: `Recorder` into `WavOut` (`FileOut` after the dissolution)
with `Pcm::Int16` goes through the codec's `Encoder`, whose quantizer
(`Pcm::quantize`) neither dithers nor resamples (`tutti-codec` takes blocks
already at the `Spec`'s rate and width). So a live 16-bit take is undithered,
while an export of the same signal is dithered by `DitherState` in
`tutti-export/src/process/dither.rs` (unless its config says `Dither::Off`).
`WavOut`'s docs call the difference deliberate: noise shaping is a decision a
capture path should not make silently. What is open is whether `Dither`
(and `Resample`) become adapters that both export and the recorder use,
replacing export's private chain; as `Sink` adapters (wrapping the file sink) a
recorder opts in with one line, and as `Source` adapters export keeps its order
rule. This is an **owner decision, not settled** (§10, decision A).

Decision of 2026-11-07: wait for a second pipeline (resample on record, import to the project
rate, a bounce path) or for acceptance of the Ogg change. If pushed, push
`Resample` and `Dither` with the `Stage` marker only: `Gate` is `drive`'s job
(it also owns progress and cancel) and `Downmix` has no user. A rate converter
cannot be a graph node, since it changes the frame count per block; only
`Downmix` overlaps the graph's fold. None of this runs on the audio thread.
`Source` and `Pull` are the prerequisite either way.

### 5.4 Export's `FrameSource` and `pump_blocks`

`tutti-export` has a private `FrameSource` (`render/driver.rs`: `fill`,
`produced`, `max_block`, `lost_events`, `check_health`) with two impls,
`GraphSource` and `PlaneSource`, and its own `pump_blocks` in `encode/mod.rs`.
That is the offline pull and a pump: a third source shape beside `AudioIn` and
`WaveIn`, deliberately not an `AudioIn` today because it is infallible and
block-sized. With `Pull`, `GraphSource` is a `Source` that never returns
`Starved`, and `check_health` maps onto `Failed`. Whether stage 3 folds
`FrameSource` into `Source` depends on where `lost_events` and `max_block` live
(they are the driver's, not the source's). Left open (§10, unknown 5).

### 5.5 Stage 3 fixes the docs that overclaim

Stage 3 rewrites, in the same PR:

- the module docs of `crates/tutti-types/src/io.rs`: the traits are the edge
  vocabulary (§2.1), not every source and sink; drop "disk stream, plugin
  boundary" and the "neural generator" and "network socket" examples, or mark
  them as possible future sources;
- `crates/tutti-plugin-types/src/channels.rs`: a plugin is a node in the graph
  and the slab is a transport; drop the "back-to-back" framing and the
  `AudioIn`/`AudioOut` links in `Sample`'s docs;
- the slab's docs, which say why it does not implement the traits, shrink to
  "not an edge".

## 6. The rename: butler to disk streamer

The module `butler/` and its names (`ButlerCommand`, `ButlerThread`,
`ButlerSender`, `ButlerGone`, the comments) become **disk streamer**, matching
the public `DiskStreamer`. One mechanical PR, net zero lines.

Count (case-insensitive, whole tree, on the tree this was written against,
before this doc):

```bash
git grep -Io -i butler | wc -l      # occurrences
git grep -Il -i butler | wc -l      # files
```

gives **1,089 occurrences in 68 files**; `-- '*.rs'` gives 981 in 57 files and
the rest is Cargo manifests, READMEs, `CLAUDE.md` and other design docs. (The
record's figure was about 1,000 in 66.) Recount before the PR; do not trust
these figures after the tree moves.

Name notes:

- `DiskStreamer` is the engine handle and stays; the new `Stream` is a stream
  it runs, so `DiskStreamer::open(..) -> Opening -> (Stream, StreamReader)`
  reads in order.
- `butler/streamer.rs` holds `DiskStreamer`; after the rename that would be
  `streamer/streamer.rs`. Put `DiskStreamer` in `streamer/mod.rs` instead.
- `Stream` sits beside `StreamRecord`, `StreamPin`, `StreamOrigin` and
  `StreamFile`; stage 5 folds the first, third and fourth into `Stream`, and
  `StreamPin` remains.
- The internal streamer command type and the public `Command` are already a
  pair and stay one.
- Land it when no other lane edits the module, immediately before stage 5, so
  stage 5 is written in the new names.

## 7. Waiting and priority

The lane that came first has landed. What is there, and what the disk stream
still needs:

**Landed.**

- `tutti_rt::Doorbell` (`crates/tutti-rt/src/doorbell.rs`), a wake-up for one
  parked thread with a loom model (`tests/doorbell_loom.rs`), from the worker
  pool's sleeper (#338). The `WorkerPool` sleeps on it; the plugin bridge's
  channels sleep on it too (#341), which removed the bridge's `try_lock` and its
  1 ms backstop.
- `tutti_rt::request_priority(PriorityRequest) -> PriorityOutcome`
  (`priority.rs`, #341): `Highest` (nice -20, `HIGHEST`) and `Raised` (about
  nice -10, `ABOVE_NORMAL`), never a real-time class, and it reports what the
  OS granted. The plugin server uses `Highest`.
- The butler thread asks for `Raised` and logs the outcome
  (`butler/thread.rs`). A refusal is harmless. Grounds: Ardour's butler sets no
  CPU priority and LinuxSampler's disk thread runs at nice -10; on Linux
  `Highest` is at most nice -20 under `SCHED_OTHER`, `EPERM` unprivileged, never
  real time. Linux I/O priority (`ioprio_set`, as Ardour's butler) is added
  only if a load test shows it helps.
- The ALSA MIDI backend blocks in `poll` on its fd (`backend/alsa/poll.rs`).

**Not done, the disk stream's part.**

- The butler loop still waits on its command queue with `recv_timeout`, 1 ms
  idle and 3 ms healthy, and yields while a ring is below threshold
  (`butler/loop_body.rs`, `step.rs`). It should sleep on a `Doorbell`, rung by a
  voice when a ring crosses its refill threshold, with the wait timed to the
  earliest ring deadline instead of a fixed tick.
- A **slow backstop tick** stays, because `Phase::Reclaim`-style `reclaim()` of
  the ring's `ArcLend` cells must still run on a cell that goes quiet
  (CLAUDE.md, "Publishing to the audio thread"). The doorbell must not mean a
  quiet streamer never scans.
- `step(now) -> Wake` becomes testable with no thread and no manual mode: the
  step has no opinion on how long to sleep, only the wake it returns.
- The comments in `loop_body.rs`, `step.rs` and `thread.rs` still say the thread
  "runs at maximum priority"; they are wrong since #341 and are corrected with
  the rename.
- A stress test at normal priority pins that a refusal costs nothing.

## 8. Stages

Each stage is one PR that stays green. Line figures are estimates.

0. **Done (#282).** `FileIn::is_failed` returns `self.fault.is_some()`, with a
   test. `FileIn` now overrides it (`tutti-codec/src/decode/file_in.rs`).
1. **Done (#283).** `WavOut::write_folding` (no production caller) and
   `RegionOut`'s `AudioOut` impl (its `write` threw the landed count away; the
   streamer calls `push_interleaved`) deleted with their tests. Confirmed: no
   `write_folding` anywhere, and `AudioOut` has no sampler impl.
2. **Done (#315, #321, #331).** `tutti-codec` (one `Encoder` over WAV, AIFF,
   FLAC and Ogg Vorbis; `Wave`, `FileIn` and the decoders moved in). `Pull`
   is written once in the final files.
3. **`Pull`, `Source`, `Sink`** in `tutti-types`, replacing `AudioIn`,
   `AudioOut`, `OnEmpty`; the generic `S` dropped. **This follows the
   `tutti-io` dissolution PR** and is written against the homes in §5.1:
   `MicIn`, `FileIn`, `TapReader` as sources, `FileOut` as the sink, `Recorder`
   and `PumpLoop` in `tutti-session`, preludes, about twenty test impls, plus the
   doc fixes of §5.5. Includes the flag-before-ring rule in `MicIn::pull`, with
   a loom model beside `pos_ring_loom.rs` (unknown 3). Roughly -80 production
   lines. Breaking for consumers (`pump`, the traits and `OnEmpty` are
   re-exported by the sampler, `bevy-tutti` and `tutti`), so one workspace-wide
   PR. Decides `pump`'s home and `FrameSource` (§10).
4. **Rename butler to disk streamer** (§6). Mechanical, net 0, with
   `DiskStreamer` in `streamer/mod.rs`.
5. **`Stream`, `Opening`, `StreamReader`, `Block`.** Removes the plan map,
   `Link`, `take_streaming_unit`, `Ring::take_reader`, `TakeVoiceError`,
   `placed`, `StreamRecord`/`StreamOrigin`. About -150 to -250 production lines
   net; every voice-build call site in the sampler and `bevy-tutti` changes.
   Smallest useful cut: `Opening` and the by-value reader first, keeping the plan
   map behind it, then delete the map.
6. **Doorbell wake and backstop tick** (§7).
7. **Adapters** (`Resample`, `Dither`, `Stage`) on the trigger in §5.3, which
   the recorder's integer-PCM path may already meet (decision A, §10).
8. **`Cue`** only with a measured gap.

The smallest useful version of the whole design is stages 0, 1 and 3, and the
first two are in.

## 9. Decisions

Taken by the owner on 2026-11-07, all as recommended.

1. **Positional `StreamReader` and `Block`**, not a sequential `pull(n)` on the
   ring and not both. The voices need position; a sequential pull has no
   production caller.
2. **`Opening` hands the reader out by value**, not a blocking `open` and not
   the current poll-and-runtime-error. It removes `ReaderTaken` and the silent
   failed open, and a non-seekable file decodes on the streamer's thread.
3. **Adapters wait**; land `Source` and `Pull` only. One pipeline, line-neutral,
   and a visible Ogg change (§5.3).
4. **Keep `Sink`.** One production impl, but a recorder generic over its
   destination and a second sink likely.
5. **Drop the generic `S`.** Every production impl is `f32`.
6. **Defer `Cue`** until a gap is measured.
7. **Delete `write_folding` and `RegionOut`'s `AudioOut` impl.** Done (#283).
8. **No `tutti-stream` crate; `Pull` in `tutti-types`** (§4).
9. **`tutti-codec` before `Pull`**, so `Pull` is written once in the final
   files. Done (§8).
10. **`DiskStreamer` lives in `streamer/mod.rs`** after the rename.
11. **Rename `butler` to "disk streamer"**, matching the public `DiskStreamer`.
12. **Waiting and priority**: a `Doorbell` for the wake, one priority call that
    reports what it got, disk streaming never real time at about nice -10.
    Landed except the streamer's own part (§7).

Not adopted: creek (§3), a `tutti-stream` crate (§4), a per-sample `Iterator`
(wrong for the graph), `async` (wrong for the audio thread), and a lent
`&[f32]` from the ring (§2.3). Also not models: rodio's per-sample `Source`,
kira's `decode -> Result<Vec<Frame>>` (it allocates, EOF undocumented) and
Firewheel (disk streaming not implemented). No new dependency is proposed.

## 10. What is not decided, and how each is settled

**A. Do `Dither` and `Resample` become shared adapters?** An owner decision.
Decision 3 waited for a second pipeline; the recorder writing integer PCM is
one (§5.3), undithered by design today. Options: (a) keep waiting and leave the
recorder undithered; (b) push `Resample` and `Dither` with the `Stage` marker as
`Sink` adapters, used by export and opt-in by the recorder, accepting the Ogg
block-size change; (c) push `Dither` alone as a sink adapter, since a recorder
has no resample. Recommendation: (c) at stage 7, after stage 3, with a test that
a live 16-bit take and an export of the same signal agree when dithered alike;
`Resample` waits for a pipeline that needs it. Whether a recorder should dither
by default is a separate product call and stays off.

1. **Is the gap on a jump outside the window audible or late enough to need a
   `Cue`?** Unmeasured. Settle with a streamer test that jumps on a
   non-resident file during a transport loop and counts the frames until the new
   side sounds.
2. **Does `Block` (the claim plus `Loan<RingMap>`) fit `LiveRead::render`?**
   Today the map is lent after the claim and used while the claim is alive.
   Settle by porting `render` on a branch.
3. **Is the fault-flag ordering sound under real interleavings?** The rule is
   "read the flag, then the ring, `Failed` only if the ring was empty".
   Single-threaded tests cannot show it. Settle with a loom model (flag store,
   push, claim, flag load) beside `pos_ring_loom.rs`, as stage 3 requires.
4. **Is a `StreamRecord` ever dropped on the audio thread?** Two comments
   disagree. `butler/prefetch.rs` says a ring's last handle is never dropped on
   the audio thread, because retired units travel back to the control thread
   (`Editor::collect`). `butler/control.rs` says a voice, the last holder of a
   `StreamRecord`, can drop it on the audio thread. Reading the tree narrows it
   without settling it: a disk voice is held by the sampler's `VoicePool`,
   which retires removed slots over a channel to the control thread, but a
   disconnected or full retirement channel degrades to dropping in the
   callback, counted as `PoolDrops::freed_in_callback`. So the "never" holds
   except on that counted path, and neither comment says so. Settle with a
   free-detecting allocator test over a voice's drop in the graph, covering the
   remove path and the degraded one; stage 5 deletes `StreamRecord`, so the
   outcome decides only whether `Stream`'s drop needs the same gate.
5. **Does stage 3 fold export's `FrameSource` into `Source`?** (§5.4.) Settle by
   trying the fold on `GraphSource` and seeing what is left over: if
   `lost_events` and `max_block` have no home on `Source`, they stay on a driver
   type and `FrameSource` shrinks to them.
6. **Where does `pump` live?** It is behaviour in the vocabulary crate. Settle
   in stage 3: into `PumpLoop` (`tutti-session`) if nothing else calls it, or
   it stays as the one primitive if `FileOut`'s tests and `bevy-tutti` still do.
7. **Does any test or consumer pin Ogg bytes?** Not searched. It decides whether
   the block-size change in §5.3 matters.
8. **`Opening` for a file that cannot seek.** The streamer decodes it whole
   (`load_wave`). Whether `poll` reports progress or only readiness is
   unsettled; settle with the first caller that wants a progress bar.
9. **Other platforms.** The measurements here ran on Linux x86_64. Windows,
   macOS and the golden digests are unchecked; stage 3's loom model and stage 5's
   gates run in CI.
10. **creek under load.** Its thread-per-stream and 1 ms poll costs are from
    its source, not measured. They matter only if the decision in §3 is
    reopened.
