# 025: streaming, the edge traits and the disk streamer

Two things in tutti are called a stream. At the edge of the engine a **source**
or **sink** moves frames in or out on a clock the graph does not keep: a
microphone, a file, a tap, a WAV encoder. Inside the sampler a **disk stream**
keeps a ring of decoded frames ahead of a voice that reads it by position, at
any rate, backwards if it likes. They share a name and nothing else, and today
they are described by one vocabulary that fits the first and not the second.
This design gives each its own shape, removes the runtime rules a type can
carry, and orders the work as PRs that each stay green.

Status: design, stages 0 to 4 landed (§8); stage 5 is specified as seven cuts,
cuts 1 to 4 built. The sections that describe a defect (§1) describe the tree
before stage 3. The decisions of 2026-11-07 and the stage 5 decisions of
2026-11-10, which fold in a read-only review of the disk streamer
(`streamer/`), are the owner's, taken as recommended, and are recorded with
their grounds in §9.

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
- **A stream is a value with an identity, a phase and a status.** The
  streamer keeps one record per open stream, driven by a pure `step`; every
  command names its stream; what the control side can observe (an open
  failure, a fault, an end, underruns, a dead streamer) is a typed status
  (§2.5 to §2.7).
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
  (`crates/tutti-plugin-ipc/src/transport/shm/slab.rs`), because it is planar;
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

### 1.6 What the review of the streamer found

A read-only review of `streamer/` after the rename (every claim from reading
the code) found that the plan map and the handoff are the visible part of a
wider problem: a stream's state is spread over about six places (the plan
entry, `Link`, `RegionMap`, `Ring.reader` and `placed`, `StreamRecord.ended`,
`StreamPin`) that must agree, and a command is fire-and-forget. The bugs, and
where each stands:

| | Defect | Status |
|---|---|---|
| B1 | A re-`Stream` on a streaming channel overwrote the `Link` and leaked the old region | **Fixed, #420** (the replaced stream frees its region); **restated, cut 4**: a dropped `Stream` frees its region in the step that handles the close (`a_close_frees_the_region_in_that_step`, `a_dropped_stream_is_freed`) |
| B2 | `StreamPin` decremented an entry it never incremented | **Fixed, #420** (a pin releases its own count) |
| B3 | A `take_disk_voice` after a re-`Stream` is sent but before the streamer applies it succeeds on the *old* link: the new voice binds to a ring that is no longer fed, and the record says the stream ended | **Confirmed real by reading** (`control.rs`: `take_streaming_unit` reads whatever `plan.link` is now; `NotStreaming` cannot tell "never opened" from "reopening"). **Fixed, cut 3** (§8): the reader is the reply of one open, so there is no lookup to read the old stream's ring; its test is `two_opens_each_yield_the_reader_of_their_own_stream` |
| B4 | `SetVarispeed` and `Seek` carry only a channel, so one queued for the old stream applies to the next file, or to an idle plan and is inherited by the next stream | **Fixed, cut 4** (§8): every command names a stream, the streamer ignores an id it does not hold, and a stream's commands go through its `Stream` |
| B5 | No panic containment on the streamer thread: a decoder panic freezes every ring and the only symptom is `StreamerGone` on the next send | Open. Cut 5 |
| B6 | Two seeks inside one block replay a stale target (the epoch and target pair) | Dies with the free-running reader, cut 1 |

The structural findings are the decisions of §2.5 to §2.9 and the cuts of §8.

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
  from every caller's `pump_once` into `MicIn::pull`. As built, the rule is
  `tutti_rt::FaultLatch::drain` (§8, stage 3), which `MicIn` calls through
  `StreamFaults`.
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

`pump` was behaviour sitting in `tutti-types`, which is meant to be vocabulary.
Stage 3 moved it out (§10, question 6): it is a private function of
`tutti-session`'s recorder module, one step of `PumpLoop::pump_once`.

### 2.3 The disk streamer (`tutti-sampler`)

```rust
impl DiskStreamer {
    pub fn new(rate: impl Into<SampleRate>, config: DiskStreamerConfig) -> Result<Self, SpawnError>;
    pub fn open(&self, path: &Path, place: Placement) -> Opening;   // control thread, never blocks
}
pub struct Placement { .. }    // where in the file the reader starts
impl Placement {
    pub fn new(start: SamplePosition) -> Option<Self>;   // refuses negative and non-finite
}
pub struct Opening { .. }
impl Opening {
    pub fn poll(self) -> Open;                // consumes: the reader can be received once
}
pub enum Open { Pending(Opening), Ready(Stream, StreamReader), Failed(OpenError) }
pub enum OpenError {
    Io(std::io::ErrorKind),   // the file could not be opened or read
    Format,                   // not audio this build can decode
    Placement(PlaceError),    // a channel no speaker names (today's silent refusal)
    StreamerGone,
}

pub struct Stream { .. }       // control side. Drop ends the stream and frees its ring.
impl Stream {                  // every command is addressed to this stream (its StreamId)
    pub fn id(&self) -> StreamId;
    pub fn set_loop(&mut self, setting: LoopSetting) -> Result<(), StreamerGone>;
    pub fn set_direction(&mut self, direction: Direction) -> Result<(), StreamerGone>;
    pub fn status(&self) -> StreamStatus;   // a level read: phase, applied loop, fault, underruns
    pub fn file(&self) -> &StreamFile;      // path, file rate, resident wave: what a fork reads
}

pub struct StreamReader { .. } // audio side, !Clone, moved into the voice
impl StreamReader {            // (wraps PosReader and Reader<RingMap>)
    pub fn block(&mut self, play: u64, reads: [Range<u64>; 2]) -> Block<'_>;
}
pub struct Block<'r> { .. }    // !Send; borrows the reader; slot claim and Loan<RingControl>
impl Block<'_> {
    pub fn holds(&self, pos: u64) -> bool;
    pub fn frame(&self, pos: u64) -> PosFrame<'_>;   // PosClaim::frame today
    pub fn control(&self) -> &RingControl;           // the map, the direction, the applied loop
}
```

Positions on this surface are typed (`Samples`, `SamplePosition`) and a bad
one is refused, never clamped (§9, decision 22). There is no `seek`, no
`set_speed`, no channel number and no public command enum: the streamer's
commands are private to the module and each carries a `StreamId`.

| Rule | Mechanism |
|---|---|
| A stream has one reader | `StreamReader` is not `Clone` (`PosReader` already is not); `Opening::poll(self)` hands it out once; no `Mutex<Option<PosReader>>` |
| A reader is the reply of one open | the open's reply cell is written by the streamer and read once by `poll`; no lookup by channel, so a re-open cannot hand out the old stream's reader (B3) |
| A block's frames are not read after the next block starts | `Block<'r>` borrows `&mut StreamReader` |
| The view stays on the audio thread | `!Send` via `PhantomData<*const ()>` |
| Lend the control once per block, never park it | the `Loan<RingControl>` lives inside `Block`, so it ends with the borrow; "forgot to lend" and "parked the loan" become unrepresentable |
| A dropped `Stream` ends the stream | `Drop for Stream` sends `Close(id)`; the streamer drops the stream's one record, and with it the region, in the same step |
| A command reaches the stream it was made for | every command carries a `StreamId`; an id the streamer no longer holds is ignored (B4) |
| A pull's verdict is not ignored | `#[must_use]` on `Pull` |
| Registering a `Reader` allocates, so it happens on the control side | `Opening` registers the ring's control reader before returning `Ready` |
| A file that cannot be opened is an error | `Open::Failed(OpenError)`, where `handle_stream_file` returns silently today |

Each row that is a compile-time property gets a `compile_fail` doctest with a
compiling twin, and a mutation (remove the borrow, the `PhantomData`, the
`must_use`, the `self` receiver) that makes it fail.

**Why there is no `seek`.** Voices do not seek: they pass a new `play` to
`claim`, a jump is detected from the position, and `holds(pos)` already answers
"cached". An earlier draft put `Stream::seek` here and called the free-running
reader's `Ring::request_seek` (epoch and target) "the one seek that exists". It
is the other way round: the free-running reader has no production caller, so
it is deleted, not ported (§2.8, decision 16).

**Why `Opening` and not a blocking `open`.** The ring's width comes from the
file header, so the ring is built on the streamer's side after the probe, and a
file that cannot seek is decoded whole (`load_wave`) before it can be. A
blocking `open` would do that on the caller's thread. A polled open needs no
thread of its own on the control side. The whole-file decode does not run on the
shared streamer thread either (§2.5, decision 21).

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

### 2.5 The streamer's side: one value per open stream

Today a stream's state is "plan entry exists" x "`link` is `Some`" x "region
registered" x "`Ring.reader` taken" x "`placed`" x "`record.ended`" x "pin
held", mutated in sequence by `handle_stream_file`, and one invalid combination
was reachable (B1). Deleting the plan map would only re-key that split by
region id. The streamer side becomes one value per open stream, held by the
region map and keyed by `StreamId`:

```rust
enum Phase { Opening, Live, Closing }           // nothing else
struct Record {                                  // one per stream; Drop frees the region
    id: StreamId, phase: Phase,
    out: RegionOut,                              // writer, decoder, resident wave
    file: Arc<StreamFile>, loop_: LoopSetting,   // as applied
    direction: Direction, pin: StreamPin,
    status: Arc<StatusCell>,                     // the reply path, §2.6
}
// The pure core, in the Voicing mould: no thread, no clock, no file.
fn step(streams: &mut Streams, cmd: Cmd, env: &Env) -> &[Effect];
enum Effect { Publish(StreamId, Status), Decode(StreamId), Free(StreamId), Warn(..) }
```

- **`Drop for Record` frees the region and releases the pin in the same step**
  that removes the record, so "link gone but region kept" cannot be written.
  `Drop for Stream` (the control handle) sends `Close(id)`; the streamer's
  removal is that step.
- **`step` is pure.** It decides what a command does to the table and returns
  the effects (a status to publish, a decode to start, a stream to free); the
  driver performs them. It is tested with no thread and no hand-stepped
  streamer. `StreamerCycle::step` today drains, reclaims, polls the preroll
  table, refills and measures in one function, so "testable with no thread" is
  true only because of the manual driver.
- **The driver is a typestate**, not two runtime flags and asserts: a streamer
  is built, then either stepped by hand or running on its thread, and the
  type says which (the `Lifecycle` shape of design 016). It is the cut after
  the core exists (cut 4).
- **Containment is per stream.** A refill, an open and a decode each run under
  `catch_unwind` per stream; a panic faults that stream (`StreamFault::Panicked`,
  §2.6) and drops its record, and the others keep filling. A panic that escapes
  anyway drops the whole table, and each record's drop publishes
  `Faulted(StreamerGone)`, so a voice is told (decision 20).
- **`DiskStreamer::new` is fallible.** The thread spawn returns
  `Err(SpawnError)` where it `expect`s today. A stream command with no
  consumer would otherwise stall silently, which is the reason the `expect`
  gives; an `Err` at construction is the same loud failure in the caller's
  hands.
- **A whole-file decode is off the shared thread.** A file that cannot seek
  (mp3, ogg) is decoded whole today inside the command drain, so every other
  stream stops refilling for its duration and the bounded queue back-pressures
  the caller. It moves to one decode thread, started on first use and fed one
  job at a time (a burst of opens costs one core, never the streamer's); the
  result comes back as `Decoded(id, wave)` and the stream goes `Opening` to
  `Live`. A spawn failure is `OpenError::Io` (decision 21).

### 2.6 What the control side can observe

Every outcome the streamer reaches is today a log line or a silent return
(`handlers.rs`, "no caller left to report to"), and `take_underruns` has no
production caller although its doc says the streamer drains it. The outcomes,
and what carries each:

| Outcome | Carried by |
|---|---|
| Open failed | `Open::Failed(OpenError)` from `Opening::poll` |
| Loop applied (it can differ from the one asked: a lead-in that cannot be read downgrades it) | `StreamStatus::loop_`, the loop as applied |
| End of file | `StreamStatus::phase == Ended` (the voice already reads silence past the end) |
| Decode fault | `StreamStatus::fault = Some(StreamFault::Decode)`; a panic is `Panicked` |
| Underruns | `StreamStatus::underruns`, read from the stream's health cell: the first reader of what `report_underrun` counts |
| Streamer gone | `Err(StreamerGone)` from a command, and `Faulted(StreamerGone)` in the status (the record's drop publishes it) |

`StreamStatus` is a **level read**, not a queue: a small cell the streamer
writes and `Stream::status` reads (phase and fault in one word, the applied
loop beside it, the underrun count). A slow reader loses nothing it needs (it
sees the current level), nothing allocates, and no overflow policy exists to
decide. The fault is set before the phase, as `FaultLatch` orders a fault
before the ring (§2.2). The loop downgrade stops being reported by mutating a
`Mutex<LoopSetting>` that a fork later reads.

### 2.7 Control cells: what is published, what is a scalar, what is gone

The ring's control cells are today `preroll`, `origin`, `fade_frames` and the
seek epoch and target, each a `Relaxed` atomic outside `tutti-rt`, beside the
`ArcLend<RingMap>` the audio thread lends once per block. A reader can see a new
`preroll` with an old `fade_frames`, and the seek pair is a hand-rolled
sequence protocol with no loom model (B6). The disposition of each:

| Cell | Disposition |
|---|---|
| seek epoch and target, `origin`, `placed` | **deleted** with the free-running reader (§2.8) |
| `preroll` | **deleted** (§2.9): the compiler aligns a graph voice |
| `fade_frames` | the config's constant, set once when the ring is placed: an immutable field, not a cell |
| the map, and the direction and applied loop that were separate | **one published `RingControl`**, through the ring's existing `ArcLend` (the one lend a block already pays), reclaimed on the cycle tick |

So the published value is `RingControl { map: RingMap, direction: Direction,
loop_: LoopSetting }`, a `Copy`-shaped control block. The streamer is its one
publisher (publish is control-side only, and the streamer is the thread that
already publishes the map); `Stream::set_direction` reaches it as a
stream-addressed command, and the voice reads direction from the `Loan` it
lends, not from a shared cell. The fork reads the loop as applied from the same
cell (below). `refill_one` returns the fill level (`Fill { fraction, wrote }`)
instead of writing a cell the same thread reads next.

`RtState` is per channel and shared by every voice taken from the channel, and
two things in it have two writers (the queued `SetVarispeed` from the streamer
thread, and `DiskVoice::set_speed` and `set_direction` from the control side
or the audio drain). It splits per stream, one writer per knob:

| `RtState` field | After | Writer |
|---|---|---|
| gain | a field of `DiskVoice` (a voice control with no streamer reader) | the voice's command drain |
| direction | `RingControl.direction` | the streamer, on `SetDirection(id)` from `Stream::set_direction` |
| speed, stretch rate | per-stream scalar cells (`Param`-style atomics, owned by the stream, not the channel): the streamer reads them for the fill rate (`read_rate`), and they flow from the audio thread, which cannot publish | the voice's command drain (`apply_speed`, `set_stretch`), the sole writer |
| `src_ratio` | per-stream scalar cell | the streamer alone, derived from `SessionRate` when it applies a rate event; no longer written from two threads under the plan entry's lock |
| underrun count | the stream's health cell (§2.6) | the voice (audio thread) |
| `buffer_fill_level` | the return of `refill_one` | n/a: it was a streamer-to-streamer value in a shared cell |
| `buffer_margin` | deleted: a constant 1.0 threaded through six signatures | n/a |

`SetVarispeed` (the public `Command::SetSpeed`) is deleted: nothing in the
workspace sends it outside tests, the pool already routes speed and reverse
through the voice (`UpdateSpeed`, `UpdateReverse`), and its comments say a host
"never forks streaming speed onto a streamer command".

**Does the pool's crossfade drive gain through `RtState`? No.** The crossfade
is `Graph::crossfade` (`tutti-session/src/graph.rs:1042`), a graph-level swap of
the node with its own fade, and gain reaches `RtState` only through the pool's
`UpdateGain` (`voice/pool.rs:858`) on one voice. So review S3 is narrower than
suspected: a crossfade does not move both gains. What is real is the sharing:
`take_streaming_unit` hands the channel's one `Arc<RtState>` to every voice
taken from it (`control.rs:74`), so after a restart a host write of gain,
speed, direction or stretch to the new voice also reaches the old voice that is
still fading out. Per-stream cells end it.

**Where the fork's data lives once `StreamRecord` goes.** `StreamRecord`
holds `path`, `file_rate`, a `Mutex<LoopSetting>` and `ended`, shared by the
link and every voice through `StreamOrigin`, and the pool keeps a fourth copy
of the loop with `roll_back` to reconcile it. After:

- **`StreamFile { path, file_rate, resident: Option<Arc<Wave>> }`**, immutable,
  built by the open and held in an `Arc` by `Stream` and by `StreamReader`
  (the fork is made from the node, which has the reader and not the handle).
- **The loop as applied** is `RingControl.loop_`, one owner (the streamer),
  read by a fork through a control-side registered reader of the ring's cell.
  A fork made on the control thread may allocate to register.
- **`Stream::set_loop`** replaces `forward_loop` and the pool's
  `StreamTable`/`forwarding` mutexes: it sends, and updates its own copy only
  after the send succeeds. The voice-side loop is a second value on the audio
  side of the queue, so one undo remains, inside the pool and in one function
  (queue the voice command, then `set_loop`; on `StreamerGone` re-queue the
  previous loop). The two-mutex, two-phase update across threads is gone.
- A stream that ends is `status().phase`, not `record.ended`; a fork of an
  ended or faulted stream fails naming the voice, as `describe()` returning
  `None` does today.

### 2.8 The free-running reader is deleted, not ported

`Command::Seek`, `StreamerCommand::SeekStream`, `handle_seek_stream`,
`Ring::request_seek`, `seek_epoch`, `seek_target`, `origin`, `Ring.placed`,
`DiskSource`'s `position`, `applied_seek`, `playing` and `play`/`stop`/
`is_playing`, and `take_free_running` exist for tests only. Verified by
searching the whole tree, `crates/bevy-tutti` included (`git grep` for each
name):

- `take_free_running` is `#[cfg(test)]` (`ports.rs:412`); the one production
  taker, `take_disk_voice*`, always passes `placed = true` (`ports.rs:387`).
- `Command::Seek` is documented a no-op for a placed voice (`ports.rs:95`), the
  only kind there is. Its only senders are tests (`streamer/mod.rs`,
  `voice/disk_voice/live_loop.rs`, `tests/tier_parity.rs`, where the send is
  redundant beside the clock seek next to it); no non-test caller exists.
- `DiskSource::play` and `stop` are called only by tests
  (`voice/disk_voice/tests.rs`, `voice/disk_voice/live_loop.rs`).
- `Command::SetSpeed` has no sender outside `ports.rs`'s own test.

The `placed` flag exists only to ignore the seek, and the free-running reader
is the only reader of the seek pair, so deleting it also removes the
store-after-take window on `placed` and B6. `StreamReader` then has one kind.
If a free-running source is wanted later it is its own type with its own
control block, not a flag on the ring. The tests that drive the feature are
deleted with it; the two in `tier_parity.rs` lose only the redundant send.

### 2.9 Preroll: dead for a graph voice

The question (review F9): `PrerollTable` is `Plan::compensation()` per output
channel, "for a source outside the graph" (`tutti-graph/src/plan.rs:1089`), and
the streamer applies `table[channel_index]` to a stream's reader. A disk voice
is now a `tutti_graph::Node`. Do the preroll and the compiler's alignment
double-count? **Yes for a voice that feeds an output directly, and the figure is
unrelated to the voice for any other wiring.** The evidence:

1. **The figure is the compiler's own output delay.** `delays` computes
   `channels[c] = arrival(c).align_to(total)` (`tutti-graph/src/latency.rs:330`)
   and `outputs` is exactly those channels, nonzero. The compiler applies it:
   `let d = compensation[channel]` inserts a ring of `d` frames on output
   channel `c` (`tutti-graph/src/compile/mod.rs:915`, solved at `:635`). The
   session republishes the same vector as the preroll table on every commit
   (`tutti-session/src/session.rs:978-982`) and hands the table to the streamer
   (`tutti-session/src/builder.rs:336`).
2. **The streamer applies the same amount again.** It opens the stream at
   `offset - preroll` (`streamer/handlers.rs:314-318`), and the placed voice
   reads straight position `clock - preroll` (`voice/disk_voice.rs:766-770`).
   Reading an earlier position plays what the file held before, so the
   voice is delayed by `P`. A voice feeding output channel `c` is therefore delayed
   by `P` at the reader and by `P` again by the compiler's ring on `c`: twice.
3. **Otherwise the figure is the wrong number.** A voice wired through another
   node (into a mixer, or a latent plugin's input) is aligned by the compiler on
   *that input edge* (`inputs`, which `channels` does not contain); the streamer
   still looks up `table[channel_index]`, where `channel_index` is the
   streamer's own key and not an output channel at all. It reads an unrelated
   entry, or none.
4. **The tiers disagree.** `MemorySource` applies no preroll (no `preroll` in
   `voice/memory_source.rs`), so a disk voice and a memory voice feeding the
   same latent plugin differ by the preroll. And the export fork reads the file
   at the placed position without it (`voice/disk_voice.rs:826`), so a live
   disk voice and its own export differ too.
5. **A disk voice has no latency of its own.** Its shape is audio with no
   declared latency (`voice/node.rs:283`), so it arrives at zero and the
   compiler needs nothing from the streamer to align it.

The preroll was right when a stream was a source outside the graph; it is
not for a node. It is not "complementary": either it equals the compiler's
delay (double) or it is unrelated. **Decision 23 deletes it from the streamer**
(cut 2): `PdcFeed`, `apply_pdc_updates`, `pdc_preroll`, `Ring.preroll`,
`DiskStreamerConfig::pdc`, `with_pdc`, and the session's wiring of the table
into the streamer. `PrerollTable` and `Session::compensation` stay for a host
that displays the figure (their docs already say so); whether they survive
the un-parking of `bevy-tutti`, which holds the only other consumer
(`ChannelCompensation`), is that migration's call. The cut lands the test that
shows the defect first: a disk voice and a memory voice of one file into the
same latent node, asserted aligned (it fails today with a nonzero preroll), and
passes once the preroll is gone. This is the one cut that changes what a host
hears, and only for a graph with nonzero compensation on a streamer channel's
index.

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
| End and fault | typed end of file; a fatal error latches | no verdict on the reader today (this design adds `Pull` on the edge and a `StreamStatus` level for the stream, §2.6) |
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
polling with `yield_now`, and the `Mutex<Option<PosReader>>` handoff, plus the
free-running reader (the seek epoch pair, `origin`, `Command::Seek`), which has
no production caller (§2.8). Replacing
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

The `tutti-io` dissolution has merged; stage 3 was written against its end
state:

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
| `Opening` | the polling loop on `NotStreaming`, `TakeVoiceError::{NotStreaming, ReaderTaken}`, `Mutex<Option<PosReader>>`, `Ring::take_reader`, `take_disk_voice*`, the silent returns of `handle_stream_file` |
| `Stream` and the streamer's `Record` | the channel-keyed `DashMap<usize, ChannelPlan>`, `Link`, `take_streaming_unit`, `StreamRecord`/`StreamOrigin`/`StreamFile`, `stop_streaming`, the public `Command` enum, `forward_loop`/`roll_back` and the pool's `StreamTable` |
| `StreamStatus` | the log-and-continue outcomes; the first reader of `take_underruns` |
| `RingControl` and per-stream cells | `RtState` (per channel), `SetVarispeed`, the loose `Relaxed` cells |
| `Block` | the separate claim and `map.lend()` in `LiveRead::render` |
| deletion, no replacement | `placed`, the seek epoch pair, `origin`, `DiskSource`'s free-running mode (§2.8); `preroll`, `PdcFeed` (§2.9) |

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
rule. Decided 2026-11-10: `Dither` alone, as a `Sink` adapter at stage 7 (§10, decision A).

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
block-sized. With `Pull`, `GraphSource` would be a `Source` that never returns
`Starved`, and `check_health` maps onto `Failed`. Whether stage 3 folds
`FrameSource` into `Source` depended on where `lost_events` and `max_block` live
(they are the driver's, not the source's). Decided at stage 3: it does not
(§10, unknown 5).

### 5.5 Stage 3 fixes the docs that overclaim

Stage 3 rewrote, in the same PR (done):

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
  `StreamFile`; stage 5 folds the record and the origin into `Stream` and
  `StreamFile` (the name moves to the new immutable value), and `StreamPin`
  remains.
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
- The streamer thread asks for `Raised` and logs the outcome
  (`streamer/thread.rs`). A refusal is harmless. Grounds: Ardour's butler sets no
  CPU priority and LinuxSampler's disk thread runs at nice -10; on Linux
  `Highest` is at most nice -20 under `SCHED_OTHER`, `EPERM` unprivileged, never
  real time. Linux I/O priority (`ioprio_set`, as Ardour's butler) is added
  only if a load test shows it helps.
- The ALSA MIDI backend blocks in `poll` on its fd (`backend/alsa/poll.rs`).

**Not done, the disk stream's part.**

- The streamer loop still waits on its command queue with `recv_timeout`, 1 ms
  idle and 3 ms healthy, and yields while a ring is below threshold
  (`streamer/loop_body.rs`, `step.rs`). It should sleep on a `Doorbell`, with the
  wait timed to the earliest ring deadline instead of a fixed tick.
- A **slow backstop tick** stays, because `Phase::Reclaim`-style `reclaim()` of
  the ring's `ArcLend` cells must still run on a cell that goes quiet
  (CLAUDE.md, "Publishing to the audio thread"). The doorbell must not mean a
  quiet streamer never scans.
- `step(now) -> Wake` becomes testable with no thread and no manual mode: the
  step has no opinion on how long to sleep, only the wake it returns. (Stage 5
  makes the command step pure; this makes the pacing step pure.)
- A stress test at normal priority pins that a refusal costs nothing.

The review of the streamer widens that list. Stage 6 also does:

1. **Name every wake source and who rings it.** The doc said a voice rings
   when a ring crosses its refill threshold. The streamer is also woken today
   by a command (`recv_timeout` returns), a jump outside the window
   (`refill_one` polls `play`; that is not a threshold crossing), a direction
   change (`apply_directions` polls), a session-rate change, shutdown, and, until
   stage 5 removes it, a preroll change. After stage 5 only some remain:

   | Source | Rung by | After |
   |---|---|---|
   | a command (open, close, loop, direction) | `Commands::send`, after the push | must ring; see 2 |
   | a voice's ring below threshold, or a jump outside the window | the voice, on the audio thread, through `Doorbell` | one predicate, see 3 |
   | session-rate change | `SessionRate::set` | rings |
   | shutdown | the one flag | rings, see 5 |
   | the reclaim backstop | the timeout | stays |

2. **`Commands::send` rings, and `pending` reads `!queue.is_empty() ||
   shutdown`.** Replacing `recv_timeout` with `Doorbell::wait_timeout` loses
   the property `a_send_wakes_a_waiting_streamer` pins; it is kept as a test.
3. **One shared pure predicate for "needs a refill".** The voice's ring decision
   and `refill_one` share `needs_refill(play, window, frames, end) -> Refill`,
   including a reader that jumped outside the window, or the voice and the
   streamer disagree about "below threshold". `StepOutcome::Stalled` (a ring
   sized from the whole file never reaches its threshold at end of file) is the
   insight to keep: it becomes a `Wake` value.
4. **`Idle` means no live stream, and `Pacing` is not inverted.** `Idle` is
   returned only when `plans.is_empty()` (`step.rs`), which no longer holds once
   any channel has streamed, because plans are never removed: a quiet streamer
   reports `Healthy` and wakes at 3 ms, while before the first stream it wakes at
   1 kHz forever (the quieter state polls faster). Stage 5 removes the plans;
   stage 6 defines `Idle` as "no record is `Live`" and deletes `Pacing`.
5. **One shutdown mechanism.** The `shutdown` flag, the `Shutdown` command and
   the `rx` drop are three, and `Shutdown` is handled twice (`step.rs` and the
   empty arm in `handlers.rs`). One flag and a ring.
6. **A test that a ring from a jump wakes a streamer parked on the backstop.**
   `Doorbell` has its loom model; the ring condition does not need one, but the
   wiring needs this test.
7. **A send that cannot block the control thread.** `Commands::send` blocks the
   caller when the 256-slot queue is full, and the streamer can stall far longer
   than a frame inside one stream's work. After stage 5 the decode is off the
   thread, but a full queue still blocks a Bevy frame: the send returns a typed
   `Full`, or level commands (`SetDirection`) coalesce into the stream's record.
8. **Correct the stale comments in the same PR.** "Maximum priority" (wrong
   since #341) in `loop_body.rs`, `step.rs` and the doc on `thread.rs`'s
   `start`; "the async loop" and `streamer_loop_async`, which do not exist,
   in `step.rs` and `tests/tier_parity.rs`; and `streamer/mod.rs`'s "one SPSC
   ring per channel; the audio thread only ever pops" (the ring is
   position-indexed, one per stream, never popped).

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
3. **Done.** **`Pull`, `Source`, `Sink`** in `tutti-types`, replacing `AudioIn`,
   `AudioOut`, `OnEmpty`; the generic `S` dropped. **This followed the
   `tutti-io` dissolution PR** and was written against the homes in §5.1:
   `MicIn`, `FileIn`, `TapReader` as sources, `FileOut` as the sink, `Recorder`
   and `PumpLoop` in `tutti-session`, preludes, about twenty test impls, plus the
   doc fixes of §5.5. Includes the flag-before-ring rule in `MicIn::pull`, with
   a loom model beside `pos_ring_loom.rs` (unknown 3). Roughly -80 production
   lines. Breaking for consumers (`pump`, the traits and `OnEmpty` are
   re-exported by the sampler, `bevy-tutti` and `tutti`), so one workspace-wide
   PR. Decides `pump`'s home and `FrameSource` (§10).

   *As built.* `bevy-tutti` is parked out of the workspace and was not touched:
   its `io` module still names the old traits and will not build until it is
   revived.
   - **The traits** are `Pull<V = Samples>`, `Stop`, `Source` and `Sink` plus the
     `SinkFailed` marker, as in §2.2, in `tutti-types/src/io.rs` and the root and
     prelude. `Source` keeps `take_lost` (a defaulted method: the frames a
     bounded source dropped, which `Recorder::lost` reads), which §2.2 did not
     list. `frames_or_silence` was not built: nothing on the audio side calls it
     yet (`ready`, `map` and `stop` have tests but no production caller
     either). The root `Source` is `io::Source`; the graph edge origin stays
     `graph::Source`, module-qualified (a file that needs both aliases one).
   - **Implementors**: `MicIn`, `FileIn`, `TapReader` (sources), `FileOut` (the
     sink); `PumpLoop` and `Recorder` are no longer generic over a sample type;
     `PumpPass` is unchanged. `FileIn::pull` answers `Failed` where a clean end
     read before stage 0, and `Ended` only with no fault. The sampler's `WaveIn`
     stays an inherent reader (it never ends, so it has no verdict to give) and
     lost its page of rationale. The twenty-odd test impls moved to `Pull`; the
     fixtures that lived in `io.rs` for `pump`'s tests moved to the recorder
     tests with `pump`.
   - **`ON_EMPTY` became the verdict itself.** A source answers `Starved`,
     `Ended` or `Failed` from every `pull`, so there is no per-type constant to
     state and no second poll. The mistake the const guarded against (a mic that
     "ends") is now a source writing `Pull::Ended` where it means `Starved`,
     which the recorder tests pin per source: `TapReader`'s empty tap is
     `Starved` (`tap_audio_in`, `tap_to_wav_roundtrip`), `MicIn`'s is `Starved`
     and `Failed` only on an empty ring (`mic/tests.rs`), `FileIn`'s end is
     `Ended` and its fault `Failed` (`file_in/tests.rs`, `file_in_pump.rs`).
   - **The flag-before-ring rule** is `tutti_rt::FaultLatch` (`Release` raise,
     `Acquire` read, `drain(pop)` reading the flag first). The ordering
     primitive moved to `tutti-rt` because the rule has to be checked under loom
     and `tutti-cpal` cannot be a loom target (it links cpal); `StreamFaults`
     holds a `FaultLatch` for its disconnect and `MicIn::pull` goes through
     `StreamFaults::drain`. The model, `tests/fault_latch_loom.rs`, runs the
     shipped `FaultLatch` over `edit_ring` (a shipped, loom-checked ring) as
     the stand-in for a capture ring: the claim it checks is the order of the
     two reads, which is the same whatever ring is behind the pop. It does not
     model `ringbuf`'s own atomics (that is the ring's claim, not this rule's).
     Four mutations fail it (record in the file). It runs in `cargo xtask loom`
     and CI.
   - **The doc fixes of §5.5** are in: `io.rs`'s module docs, `channels.rs`
     (a plugin is a node, `AudioBuffer` is not an edge), the slab (a transport,
     not an edge), the `Cargo.toml` comments that called the plugin crates'
     `tutti-types` edge the I/O vocabulary, and the root and crate READMEs.
4. **Rename butler to disk streamer** (§6). Done: mechanical, net 0, with
   `DiskStreamer` in `streamer/mod.rs`.
5. **`Stream`, `Opening`, `StreamReader`, `Block`, and the streamer's own
   state** (§2.3, §2.5 to §2.9), in seven cuts. Each is one PR that stays
   green; each lists what it deletes and what pins it. The order is the
   doc's smallest-useful-cut idea (the open and the by-value reader first,
   the plan map behind it, then delete the map), with the dead machinery
   deleted first so that nothing is ported that has no caller. B1 and B2 are
   fixed on main (#420); B3 by cut 3. Line figures are not
   promised: recount each cut with `git diff --stat`.

   | Cut | Lands | Deletes | Pins |
   |---|---|---|---|
   | **1** | The free-running reader and the queued speed command go (§2.8) | `Command::Seek`, `SeekStream`, `handle_seek_stream`, `Ring::request_seek`, `seek_epoch`, `seek_target`, `origin`, `Ring.placed` and its `take_reader` argument, `DiskSource`'s `position`, `applied_seek`, `playing`, `play`, `stop`, `is_playing`, `take_free_running`, `Command::SetSpeed`, `SetVarispeed`; B6 | The tests that drive the feature go with it (they pin only it); `tier_parity.rs` keeps its clock seeks and loses the redundant send. A grep gate in the PR body: none of the names remains. Re-run the whole sampler suite and the differential seek tests unchanged |
   | **2** | Preroll leaves the streamer (§2.9) | `PdcFeed`, `apply_pdc_updates`, `ChannelPlan.pdc_preroll`, `Ring.preroll` and `set_preroll`, `DiskStreamerConfig::pdc`, `with_pdc`, `manual_preroll`, the session's `pdc: Some(..)` wiring, the `- preroll` in `live_place` and `DiskSource::render` | The test of the defect, in the same PR as the deletion (it cannot land red; the PR body shows it failing with the `- preroll` restored): a disk voice and a memory voice of one file into one latent node with nonzero compensation on the streamer's channel index, asserted aligned to the frame. Mutation: put the `- preroll` back, it fails. The two live-loop preroll tests are deleted with the feature. **A behaviour change; says so in the PR** |
   | **3** | `Placement`, `OpenError`, `Opening`, `Open`, `StreamReader` by value; B3 | `Command::Stream` and the channel number on the open, `TakeVoiceError::{NotStreaming, ReaderTaken}`, `Mutex<Option<PosReader>>`, `Ring::take_reader`, `take_streaming_unit`, `take_disk_voice*`, every `yield_now` poll loop, the silent returns of `handle_stream_file`, `WaveIn` if still one-caller. The plan map stays behind it, keyed by the private `StreamId` the control side mints (it stands in for the channel number, so `Loop` and `Stop` still work until cut 4) | **The B3 test**, in its new form: two opens on one channel, the second not yet stepped; each `Opening` yields a reader on the ring of its own open (`StreamReader::id`), the first reader is never the second's, and the first ring stops being fed and is freed when its stream closes. Mutation: make `poll` read the channel's current link, it fails. Failure tests per variant: a missing path (`Io`), a file that is not audio (`Format`), a wave with an unnamed channel (`Placement`) each give `Failed`, where today each is `NotStreaming` for ever. `Placement::new` refuses negative and non-finite (`check_range_contract`). `compile_fail`: `Opening::poll` used twice; `StreamReader` cloned; each with a compiling twin. Mutations: drop the `self` receiver, derive `Clone` |
   | **4** | `Stream` handle, stream addressing, the streamer's `Record` and pure `step` (§2.5); B1 restated, B4 | the channel-keyed `DashMap`, `ChannelPlan`, `Link`, `stop_streaming`, the public `Command` enum and `Commands`, the second command mapping, `RegionMap`'s separate index (the `Record` is keyed by `StreamId`); the driver becomes a typestate (`Built`, stepped, running) and its runtime asserts and their `should_panic` test | `step` table tests with no thread: open to `Live`; `Close` frees the region **in that step** (the B1 test restated with a `Weak<Ring>`); a command for an unknown id is ignored and reaches no other stream (the B4 test: speed and loop for the old stream behind a re-open never touch the new one); a re-open is independent of the old stream. `Drop for Stream` frees the ring (a free-detecting test). Mutation: remove the `Close` send in `Drop`, remove the id check in `step` |
   | **5** | Status, faults, containment, fallible construction, decode lane (§2.5, §2.6) | the `expect` on the thread spawn, the log-only outcomes (they become status), whole-file decode in the command drain, `Metrics`' `Mutex` on thread-confined data | `StreamStatus` per outcome: a decode fault (a corrupt file mid-stream) is `Faulted(Decode)` and the voice reads silence; end is `Ended`; underruns are read from the status (the first reader of `take_underruns`); a dropped streamer publishes `Faulted(StreamerGone)` to a live stream. B5: a panicking decode (a test hook on `RegionOut`) faults only that stream, the others keep filling, on the hand-stepped and the threaded driver. `DiskStreamer::new` returns `Err` when the spawn fails (a test seam for the spawn). A non-seekable file opens while another stream keeps refilling (a throughput test with the decode held open). Mutations: remove the `catch_unwind`, decode on the shared thread, drop the status write |
   | **6** | `RingControl`, per-stream cells, `StreamFile`, `Stream::set_loop` (§2.7) | `RtState`, `StreamRecord`, `StreamOrigin`, `StreamFile`'s old shape, `forward_loop`, `roll_back`, the pool's `StreamTable` and `forwarding` mutex, `SetDirection` polling in `apply_directions` (a command now), `fade_frames` as a cell (an immutable field), `buffer_margin`; every voice-build call site in the sampler changes (`bevy-tutti` stays parked and is not touched) | Per knob, one writer: a test that two voices opened in sequence on one file share no cell (set gain, speed, direction, stretch on one, read the other: the S3 test); direction published once and read from the lent control; the fork reads the applied loop (a downgraded loop is the downgrade in the fork, not the request); `set_loop` leaves the stream unchanged when the send fails. A loom model only if a new lock-free structure appears: `RingControl` rides the existing `ArcLend`, which has its model, and a test that the cycle's reclaim frees a retired `RingControl` held across a replacement (as today's map test does) |
   | **7** | `Block`; sentinels and units | the separate claim and lend in `LiveRead::render`, `Content.switch_at == 0` (an `Option`), `usize::MAX` for an unknown length, bare `u64`/`usize` frame counts on the streamer's fields (`Samples`), the per-cycle `Vec` in `apply_directions` | Settles §10 Q2 by porting `LiveRead::render` on the cut's own branch. `compile_fail`: a second `block` while the first is alive; a `Block` sent to another thread (`!Send`); a `Loan` held past its `Block`. Mutations: remove the borrow, the `PhantomData`, the `#[must_use]`. The existing differential and loop tests pass unchanged (the render is bit-identical) |

   *As built, cut 1.* The free-running reader is gone, with the names the
   table lists: `Command::Seek` and `Command::SetSpeed` (and their
   `StreamerCommand`s, `SeekStream` and `SetVarispeed`), `handle_seek_stream`,
   `Ring::request_seek` and `seek_request`, the seek epoch and target,
   `Ring.origin` and the `origin` argument of `RegionBuffer::place`,
   `Ring.placed` and `take_reader`'s argument (it takes none now),
   `take_free_running`, and from `DiskSource` the `position`, `applied_seek`,
   `playing`, `play`, `stop` and `is_playing`. Without them `DiskSource` had
   nothing left to render by itself, so its `render`, its `Node` and
   `IntoNode` impls, its `sample_rate` and `LiveRead::idle` (reached only
   from the stopped source) went too: it is the ring's reader inside a
   `DiskVoice` and not a graph node. B6 dies with the pair it was about.
   Tests that pinned only the feature went with it (a relayed seek, play and
   stop, a seek that must not move a placed voice's window, a seek while
   paused, the streamer's backward-seek test); the ones that also pin what
   survives (unity step, gain and its clone, the width-six stride, reset,
   `sever`) were ported to a placed `DiskVoice`, and `tier_parity.rs` lost
   only the two redundant sends. Removing `DiskSource`'s fields shrank
   `DiskVoice` under clippy's `large_enum_variant` bound for `VoiceSource`;
   that enum carries an `allow` with its reason (boxing the memory tier
   would put a pointer in front of every read).

   *As built, cut 2.* The preroll is gone from the streamer: `PdcFeed`
   (and `streamer/preroll.rs` with it), `apply_pdc_updates`,
   `ChannelPlan.pdc_preroll`, `Ring.preroll` and its accessors,
   `DiskStreamerConfig::pdc` and `StreamerThread::with_pdc`, the cycle's
   `Local.pdc`, `manual_preroll`, the `- preroll` in `DiskVoice::live_place`
   and in the window start of `handle_stream_file` (`RegionBuffer::place`
   lost its `preroll` argument), and the session builder's
   `pdc: Some(..)` wiring. (`DiskSource::render`, which the table lists, went
   in cut 1.) Each link of §2.9's chain was re-read in the code before the
   cut: the compiler's output ring (`compile/mod.rs`, `compensation[channel]`),
   the session's republish (`publish_sent`), the builder's hand-off, the
   window start and the placed read. `PrerollTable` and `Session::compensation`
   stay, for a host that displays the figure, with their docs rewritten; Q11's
   open part stands. The test is `tutti-session`'s `tier_alignment`: a stereo
   voice of one file at beat 1, as a disk stream and in memory, on two outputs
   beside a limiter's latent path on a third, must sound file frame `k` on
   output frame `24 000 + L + k` (`L` the declared compensation); the disk tier
   fails with the preroll put back (it reads `L` early on top of the compiler's
   ring) and passes without. Two existing tests encoded the feature, not the
   spec: the `live_loop` test's preroll row (a 12 000-frame table entry read
   as a half-beat early read) was dropped, its entry-past-the-loop row kept
   as `an_entry_past_the_loop_lands_where_the_memory_tier_plays`; and the
   `pdc_subscription_is_shared_not_copied` and
   `a_table_subscribed_after_a_hand_step_reaches_the_cycle` tests, which only
   pinned the subscription, went with it. **Behaviour change:** a streamed disk
   voice no longer plays late by its output channel's compensation.

   *As built, cut 3.* An open is a request and a reply, and the reader is the
   reply. `DiskStreamer::open(&Path, Placement) -> Opening` mints a `StreamId`
   on the control side, queues `StreamerCommand::Open { id, file_path, start,
   width, reply }` and returns an `Opening` holding the reply's one cell (a
   bounded-1 channel); `Opening::poll(self)` consumes it and answers `Open`
   (`Pending(Opening)`, `Ready(StreamReader)` or `Failed(OpenError)`). A
   dropped reply sender, the streamer's thread gone, is `Failed(StreamerGone)`
   at the first poll. `StreamReader` is the audio side's one reader: the ring's
   `PosReader`, the `Reader<RingMap>` the open registered on the control side
   (so a voice built from it registers nothing), the `Place`, the voice's
   width, the file's rate and the `StreamOrigin`; it is not `Clone`, and
   `DiskVoice::from_reader(reader, start_beat, duration)` moves it into the
   voice. Both rules have a `compile_fail` doctest with a compiling twin
   (`Opening` polled twice, `StreamReader` cloned) and a recorded mutation
   (`poll(&self)`, an `impl Clone`). Gone: `Command::Stream`,
   `TakeVoiceError` (both variants and `Place`: the placement is now an open's
   failure), `Ring`'s `Mutex<Option<PosReader>>` and `take_reader`,
   `take_streaming_unit`, `Status::take_disk_voice{,_into}` and the `Status`
   plan map, `handle_stream_file`'s silent returns and its `yield_now` poll
   loops (the doc example's, the tests'), `WaveIn` (one caller: it is the
   function `read_wave` now), `ChannelPlan::stop_streaming`, `Link.consumer`
   and `Local`'s region-id mint. `Open::Failed` carries `Io(ErrorKind)` for a
   file that cannot be opened, `Format` for one that opens and is not audio
   this build decodes, `Placement(PlaceError)` for a channel no speaker names
   or an unordered narrowing, and `StreamerGone`; a test per variant.
   `Placement::new` refuses a negative or non-finite position
   (`check_range_contract`, with an optional `serde` feature the crate's own
   dev-dependency turns on) and names its flooring of a fractional frame.

   Departures from §2.3's sketch, each because the code made the sketch
   impossible or worse at this cut:
   - **`Open::Ready(StreamReader)`, not `Ready(Stream, StreamReader)`.** The
     `Stream` handle is cut 4's. Until then a stream is stopped and looped by
     its id: `Command::Loop { stream, setting }` and `Command::Stop { stream }`
     replace the channel number, `StreamReader::id()` and `Opening::id()` are
     where an id comes from, and **`StreamId` is public**, an opaque `Copy`
     with no constructor (the row says private, but a public `Command` names
     it; cut 4 makes it the `Stream`'s id). It is also the plan map's key and
     the `RegionMap`'s: `RegionId` is renamed, not kept beside it.
   - **The width is a second entry, not a field of `Placement`.**
     `Placement` is only where the reader starts, so it can be a clean range
     type; `DiskStreamer::open_into(path, place, width)` asks for another
     voice width than the file's, and `open` is its `None`. The placement is
     built on the streamer's thread after the probe, before any ring exists,
     so a refusal leaves nothing to free (it used to be a `take_disk_voice`
     error that left the reader in the plan).
   - **`StreamReader` has no `block`** (cut 7's), and `DiskVoice` is built
     with `DiskVoice::from_reader`, an inherent function, not by a `Status`
     method; `Status` holds only the session rate.
   - **A re-open on a channel is not a thing.** With no channel number each
     open is a stream of its own and the plan map is keyed by the open's id, so
     B1's "replaced stream" cannot occur; its test, `a_replaced_stream_is_freed`,
     became `a_stopped_stream_is_freed` (a `Weak` of the ring, dead in the step
     that applies the `Stop`), and the cut 4 row's restatement stands on
     `Close`. `Voice::channel_index`, the pool's number for a stream, went with
     it: a disk voice knows its stream (`DiskVoice::stream`) and the pool
     forwards its loop there. `StreamAddr`'s `Option` and the pool's
     `StreamTable` remain, as cut 6's.
   - **An abandoned open is closed.** Dropping an `Opening` abandons the open:
     the streamer that answers into nobody removes the stream it opened.
   - **The reply cell is a bounded channel, not a hand-built cell.** A
     `crossbeam_channel` of one is already "written once, read once" and its
     disconnect is the `StreamerGone` verdict; nothing new is lock-free.

   B3's test is `two_opens_each_yield_the_reader_of_their_own_stream`
   (`streamer/open/tests.rs`): two opens, the second made before the streamer
   has run, are both `Pending`; after a step each yields the reader of its own
   open (`StreamReader::id`), each plays its own file (dyadic constants, to the
   bit), and the first ring is freed in the step that stops its stream.
   Mutation (run): `poll` receiving from one reply shared by the streamer's
   opens, newest overwriting (what a channel lookup was) makes the first poll
   take the second stream's answer and the test fails. The old scenario cannot
   be written: a reader comes only from its own `Opening`.
   `tier_parity`, `live_loop`, `offline_disk_voice`, `tier_alignment`,
   `block_length_invariance` and the disk voice tests pass with no assertion
   changed; they were ported in how they obtain a voice. Four tests of
   deleted code went: a second take refused (`a_stream_serves_one_live_voice`,
   `a_ring_serves_one_live_reader`: now the two doctests), the region-id mint's
   monotonic test and `ChannelPlan::stop_streaming`'s reset test. The
   `Place`-refusal-leaves-the-reader step of the 5.1 test is now "a refusal
   leaves nothing, and the file opens at stereo and at its own width". The
   other ports: `a_fork_of_a_stream_that_is_gone_plays_nothing` stops the
   stream where it restarted the channel, and
   `a_loop_for_a_replaced_stream_leaves_the_new_one_alone` is
   `a_loop_for_a_stopped_stream_leaves_the_others_alone`.

   *As built, cut 4.* A stream has a control side and a streamer side, and
   neither is shared. `Open::Ready(Stream, StreamReader)`: `Stream` is the
   control thread's handle (`id`, `set_loop(&mut self, LoopSetting) ->
   Result<(), StreamerGone>`), not `Clone` (a `compile_fail` doctest and
   compiling twin), and its `Drop` sends `Close(id)`. A command for an id the
   streamer does not hold is ignored. The streamer holds one `Record` per
   stream in `Streams`, a plain `Vec` it alone touches (found by a scan of ids
   that are minted once: the parallel refill is a `par_iter_mut` over the
   records, and `RegionMap` and its index are gone); `Drop for Record` frees the
   region, releases the cache pin and ends the fork's record. The pure step is
   `record::step(&mut Streams, Cmd) -> &[Effect]`: `Cmd` is `Open(Record)`,
   `Opened(id)`, `Close(id)`, `Loop(id, Option<LoopAsk>)` and `Effect` is
   `Live`, `Freed`, `Loop`; what reads a file (building an open's record,
   capturing a loop) is the driver's, `handlers.rs`, before and after the step.
   It is tested with no thread and no queue (`record/tests.rs`).
   Deleted: the plan map, `ChannelPlan`, `Link`, `LoopConfig`, `RegionMap`,
   `StreamerCommand::StopStreaming` (now `Close`), `Command`, `Commands` and its
   `send_all`, and the per-cycle `Vec` of `apply_directions`; the driver's two
   runtime flags, its asserts and their `should_panic` test.

   Departures from the row and §2.5, each because the code made the sketch
   impossible or worse at this cut:
   - **`Phase` is `Opening` and `Live`; `Closing` is not stored.** A close
     removes the record in the step that handles it, so a `Closing` record
     could never be observed. A record is `Opening` from its build until its
     reader is delivered (`Opened`), `Live` after; only `Live` is refilled and
     takes a loop, and an abandoned open is a close of an `Opening` record.
   - **The pool keeps forwarding by id, through `StreamerPort`.** The pool's
     voices hold a reader, not the handle, and a `Stream` cannot live in a voice
     (its drop sends on a bounded queue, which the audio thread must not do),
     so until cut 6 replaces `forward_loop` and the pool's `StreamTable` the
     pool sends the id-addressed loop command over a clone of the port a
     `Stream` sends by. `StreamerPort` is public and opaque (it names a
     streamer and has no method a host can call:
     `DiskStreamer::port()`, `VoicePool::with_streamer(port)`); the
     public command surface is the `Stream` methods alone. A host keeps the
     `Stream` for as long as the voice plays.
   - **`step` does not itself read the file.** §2.5 puts `Decode` among the
     effects; at this cut the decode is still the open's, on the streamer's
     thread, so there is no `Decode` effect until cut 5 moves it.
   - **The session rate reaches each stream's ratio through a registry in
     `SessionRate`**, `Mutex<Vec<(file rate, Weak<RtState>)>>`, not a shared
     map: the ratio must be re-derived at once on `set_sample_rate` (a test pins
     it) and the streamer owns no channel back from the control side for it.
     One lock orders a stream's registering against a rate change. Cut 6 makes
     the ratio the streamer's own, derived on a rate event, and the registry
     goes with `RtState`.
   - **The driver typestate is `DiskStreamer<D = Running>`** with `Built`
     (private), `Running` (a thread; `Drop` stops it) and `ByHand`
     (`test-support`; `step_once`, `step_until_settled`). The manual/hand-stepped
     driver stays: it is how a test counts cycles instead of waiting for them, and
     the pure step does not replace the refill. Calling `step_once` on a running
     streamer does not compile (`compile_fail` doctest and twin); a built
     streamer that is never started is just dropped (its queue disconnects).
     Build time `Built::start` still panics on a failed spawn: the fallible
     `new` is cut 5's.
   - **`dashmap`'s last user is `streamer/cache.rs`'s `LruCache`**, which the
     export fork reads from the control thread (`control.rs`'s `describe`).
     Cut 6's `StreamFile` removes that read, and with it the dependency.
   - **`Stream` has no `status()` or `file()`** (cut 5 and 6), and no
     `set_direction` (cut 6: direction is still `RtState`'s, polled by
     `apply_directions`).
   - **`StreamReader::id` stays public**, beside `Stream::id`: the voice
     (`DiskVoice::stream`) and the pool address the stream by it.

   Tests. B4: `a_loop_set_through_one_stream_leaves_the_other_unchanged` (two
   streams, a loop through each handle reaches that stream and not the other),
   `a_stopped_streams_handle_commands_nothing` (a handle for a closed stream
   sends and is ignored), `a_command_for_an_unknown_stream_reaches_no_other`
   (the step), `a_stream_names_itself_in_every_command` (the send side).
   B1: `a_close_frees_the_region_in_that_step` (a `Weak<Ring>` dead when the
   step returns), `a_dropped_stream_is_freed` (through the handle and the
   driver). Step table: `an_open_is_opening_until_its_reader_is_delivered`,
   `streams_are_independent`, `a_duplicate_open_does_not_shadow`. Free-detecting
   (§10 Q4): the three in `tests/rt_no_free.rs`. Mutations (each run, each
   failed its test, written beside it): the `Close` send in `Drop for Stream`
   removed, `Close` not removing the record, `Drop for Record` not ending the
   fork's record, `step` answering a loop for any live stream, `handle_loop`
   applying to the first record, the duplicate check, the phase set in `Opened`,
   the phase check on a loop, `retire` dropping in the drain, the replacing adds
   removed from the full-channel test, `step_once` moved to every state (the
   `compile_fail` doctest), `Stream` made `Clone`. Ported, assertions
   unchanged: `tier_parity`, `live_loop`, `offline_disk_voice`,
   `tier_alignment`, `block_length_invariance`, `export_fork_sampler`,
   `disk_placement`, `voice_pool`, `block_render` obtain a `Stream` with the
   reader (`settle` returns both) and hold it while the voice plays;
   `a_loop_for_a_stopped_stream_leaves_the_others_alone` drops the handle where
   it sent `Stop`. Deleted with the surface: `Commands`' `send_all` tests, the
   every-command-reports-a-dead-streamer test (now
   `a_stream_on_a_gone_streamer_reports_it`), and
   `a_hand_stepped_streamer_refuses_to_also_start_a_thread` (unrepresentable).

   Cuts 1 and 2 are independent of the rest and of each other and may land in
   either order, or after 3. Cut 3 is the smallest useful cut and is the first
   that changes the public surface (every voice-build call site in the sampler
   and `session`; `bevy-tutti` is parked). Cut 4 needs 3; cut 5 needs 4; cut 6
   needs 4 (it needs the stream-addressed commands and the `Record`); cut 7
   needs 6 (the `Block` lends `RingControl`). The free-detecting test of §10 Q4
   lands with cut 4 for `Stream` and with cut 7 for `StreamReader`.

6. **Doorbell wake and backstop tick** (§7), including the review's items:
   every wake source named and rung, `Commands::send` rings, one shared
   `needs_refill`, `Idle` redefined, one shutdown mechanism, a non-blocking
   send, the wake test, and the stale comments. After stage 5, because the
   `Record` and the pure `step` are what a `Wake` is returned from.
7. **The `Dither` sink adapter** (decision A, §10): export and the recorder
   share it. `Resample` and the `Stage` marker wait for a pipeline that needs
   them.
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
   failed open. (Its clause that a non-seekable file decodes on the streamer's
   thread is amended by decision 21.)
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

**Stage 5 decisions.** Decided 2026-11-10, as recommended unless a line says it
departs. The grounds were read from the code; the departures say what the code
showed.

13. **One value per open stream on the streamer's side.** A `Phase` enum
    (`Opening`, `Live`, `Closing`), a pure `step(cmd) -> effects` testable with
    no thread, and `Drop` on the record freeing the region in the same step
    (§2.5). Grounds: a stream's state is six places that must agree, and one
    invalid combination was reachable (B1); deleting the plan map alone would
    only re-key the split.
14. **A reply and health path as a level, not a queue.** `StreamStatus`
    (phase, applied loop, fault, underruns) read through `Stream::status`;
    `OpenError` is `Io(ErrorKind)`, `Format`, `Placement(PlaceError)` and
    `StreamerGone` (§2.6). Grounds: every outcome beyond an open is a log line
    today and `take_underruns` has no reader; a level needs no overflow policy
    and cannot lose what the reader needs.
15. **One published `RingControl`, smaller than the review sketched.** It is the
    existing `RingMap` plus `direction` and the applied loop, through the ring's
    `ArcLend`, reclaimed on the cycle tick (§2.7). **Departs from the review:**
    `preroll`, `origin` and the seek pair do not move into it, because decisions
    16 and 23 delete them, and `fade_frames` is written once at placement, so it
    is an immutable field. Grounds: three of the five loose atomics have no
    reason to exist; publishing them would carry dead state into the new cell.
16. **The free-running reader is deleted, not ported.** `Command::Seek`,
    `request_seek`, `placed`, `origin`, `DiskSource::playing` and
    `take_free_running` have no production caller (checked by search over the
    whole tree, `bevy-tutti` included; §2.8), so `Stream::seek` and "the one
    seek that exists" are corrected in §2.3. Grounds: porting a feature only
    tests use keeps half the control machinery and a `placed` flag whose only
    job is to ignore it.
17. **`RtState` splits per stream, one writer per knob** (§2.7). Gain moves into
    the voice; direction into `RingControl`; speed, stretch rate and `src_ratio`
    become per-stream scalar cells. **Departs from the review's "delete the
    queued `SetVarispeed` and let `DiskVoice` own speed" in one respect:** the
    streamer reads speed and stretch (through `read_rate`) to size each fill,
    and they come from the audio thread, which cannot publish, so they stay
    scalar atomics rather than move into the published value. `SetVarispeed` is
    deleted all the same (no sender outside tests; the pool routes speed through
    the voice). The pool's crossfade does **not** drive gain through `RtState`
    (it is `Graph::crossfade`), so review S3 is only the sharing: the channel's
    one cell reaches every voice taken from it.
18. **Every remaining command is addressed to a stream.** Commands carry a
    `StreamId` minted on the control side; an id the streamer no longer holds
    is ignored; there is no channel number and no public command enum (B4).
    Grounds: today only `Loop` carries a stream guard, and a `SetVarispeed` for
    the old stream lands on the next file.
19. **The fork's data lives in an immutable `StreamFile` and the applied loop.**
    `StreamFile { path, file_rate, resident }` is held by `Stream` and by
    `StreamReader`; the loop is `RingControl.loop_`; `Stream::set_loop` replaces
    `forward_loop`, `roll_back` and the pool's `StreamTable` (§2.7). **Departs
    from the review:** the review puts the loop in the `Stream` handle, but a
    fork is made from the node, which holds the reader and not the handle, so
    the fork reads the published value. One undo remains in the pool (the voice
    side of the loop is a second value across the audio queue).
20. **`DiskStreamer::new` is fallible, and panics are contained per stream.**
    `Err(SpawnError)` replaces the `expect`; a refill, open or decode runs
    under `catch_unwind` per stream and faults that stream
    (`StreamFault::Panicked`); a record's drop publishes `Faulted(StreamerGone)`
    when everything goes (B5).
21. **A non-seekable whole-file decode leaves the shared thread.** One lazily
    started decode thread, jobs one at a time, the wave returned as a message.
    Amends decision 2 and §2.3. Grounds: a multi-second decode inside the
    command drain stops every other stream's refill and back-pressures the
    caller. `Opening::poll` still reports readiness only; progress waits for a
    caller that wants it (§10, question 8).
22. **Units on the new surface.** Positions are `SamplePosition` and counts
    `Samples`; `Placement::new` refuses a negative or non-finite position and
    names its flooring of a fractional frame, and the old "clamped rather than
    rejected" doc and `max(0.0) as u64` casts go. Grounds: the range-checked
    type rule (refuse, never clamp), and a negative offset silently clamped to 0
    plays the wrong audio with no error.
23. **Preroll is deleted from the streamer.** For a voice that feeds an output
    it doubles the compiler's alignment, and for any other wiring it reads an
    unrelated table entry (§2.9, with file and line evidence). `PrerollTable`
    and `Session::compensation` stay for a host. A behaviour change, in its own
    cut, with a test that a disk and a memory voice into one latent node align.
24. **B3 is recorded and fixed by cut 3.** It is the one open bug the new shape
    makes unrepresentable; its test is stated in §8 cut 3 and cannot land
    red.

Not adopted: creek (§3), a `tutti-stream` crate (§4), a per-sample `Iterator`
(wrong for the graph), `async` (wrong for the audio thread), and a lent
`&[f32]` from the ring (§2.3). Also not models: rodio's per-sample `Source`,
kira's `decode -> Result<Vec<Frame>>` (it allocates, EOF undocumented) and
Firewheel (disk streaming not implemented). No new dependency is proposed.

## 10. What is not decided, and how each is settled

**A. Do `Dither` and `Resample` become shared adapters?** Decided by the owner
on 2026-11-10: **`Dither` alone becomes a `Sink` adapter at stage 7, after stage 3.**
Export uses it in place of its private `DitherState`, and the recorder opts in
with one line. A test checks that a live 16-bit take and an export of the same
signal agree when dithered alike. `Resample` waits for a pipeline that needs it;
a recorder has none, and pushing it now would change the Ogg block split for one
user. A recorder does not dither by default; that stays a product call.
The rejected options were to keep waiting with the recorder undithered, or to
push `Resample` and `Dither` together and accept the Ogg change.

1. **Is the gap on a jump outside the window audible or late enough to need a
   `Cue`?** Unmeasured. Settle with a streamer test that jumps on a
   non-resident file during a transport loop and counts the frames until the new
   side sounds.
2. **Does `Block` (the claim plus `Loan<RingMap>`) fit `LiveRead::render`?**
   Today the map is lent after the claim and used while the claim is alive.
   Settle by porting `render` on a branch.
3. **Is the fault-flag ordering sound under real interleavings?** The rule is
   "read the flag, then the ring, `Failed` only if the ring was empty".
   Single-threaded tests cannot show it. **Settled at stage 3: yes.** A loom
   model (`tutti-rt/tests/fault_latch_loom.rs`, 4,961 executions) runs the
   shipped `FaultLatch` and checks that a `Failed` is never read before the
   frames pushed ahead of the raise; reading the ring first, or weakening
   either ordering, fails it. The ordering primitive lives in `tutti-rt` (see
   §8, stage 3).
4. **Is a `StreamRecord` ever dropped on the audio thread?** *Settled by cut
   4's free-detecting tests (`tests/rt_no_free.rs`), which count deallocations
   per thread.* **Not by a `Stream`**: its handle is the control side's, never
   part of a voice, and dropping it only sends a close; the ring frees in the
   streamer's step that handles it, and only if the voice's reader is already
   gone (`dropping_a_stream_frees_the_ring_in_the_streamers_step`). **The ring's
   last holder can still be a voice**, and then the answer is the pool's
   (`PoolDrops::freed_in_callback`): on the remove path the drain frees nothing
   and the control thread's `collect_retired` frees the ring
   (`a_removed_disk_voice_frees_its_ring_on_the_control_thread`); on the
   degraded path, a full retirement channel, the callback frees it, counted
   (`a_removed_disk_voice_on_a_full_retirement_channel_frees_its_ring_in_the_callback`).
   So the two comments each held half: "never on the audio thread" is true
   except on that one counted path, and neither is about `Stream`. The same
   holds for `StreamReader`, the voice's own part (cut 7's test is then the
   `Block`'s).
5. **Does stage 3 fold export's `FrameSource` into `Source`?** (§5.4.) Settle by
   trying the fold on `GraphSource` and seeing what is left over: if
   `lost_events` and `max_block` have no home on `Source`, they stay on a driver
   type and `FrameSource` shrinks to them. **Decided at stage 3: no,
   `FrameSource` stays as it is.** The fold was worked through on `GraphSource`
   and `PlaneSource` on paper, not built. What a `Source` would carry (a runtime
   layout and a verdict per pull) the driver does not read: it renders to a
   count it already knows, and it hands the encoder a typed `InterleavedMut`
   block, not a bare slice. What `FrameSource` carries beyond `fill`
   (`produced`, `max_block`, `lost_events`, `check_health`) is the driver's and
   has no home on `Source`, and the trait is used as a `dyn` object by `drive`
   and `pump_blocks`. So a fold would add a second impl per source type and
   leave `FrameSource` whole. A `Source` adapter over a graph can be added by
   whoever has a consumer that polls a graph until dry; none does.
6. **Where does `pump` live?** It is behaviour in the vocabulary crate. Settle
   in stage 3: into `PumpLoop` (`tutti-session`) if nothing else calls it, or
   it stays as the one primitive if `FileOut`'s tests and `bevy-tutti` still do.
   **Decided at stage 3: into `tutti-session`, as a private function of the
   recorder module that `PumpLoop::pump_once` calls.** Its only callers in the
   workspace were `PumpLoop` and one `tutti-cpal` test, which now pulls and
   writes by hand (a test of the mic cannot depend on `tutti-session`);
   `FileOut`'s tests never called it, and `bevy-tutti`, which did, is parked. It
   stays a separate function rather than inline so that the debug width
   assertion has a test (`PumpLoop::new` refuses a mismatch first, so only the
   function can be driven with one).
7. **Does any test or consumer pin Ogg bytes?** Not searched. It decides whether
   the block-size change in §5.3 matters.
8. **`Opening` for a file that cannot seek.** The file is decoded whole
   (`load_wave`), on a decode thread after stage 5 (decision 21), not the shared
   streamer's. Whether `poll` reports progress or only readiness is
   unsettled; settle with the first caller that wants a progress bar.
9. **Other platforms.** The measurements here ran on Linux x86_64. Windows,
   macOS and the golden digests are unchecked; stage 3's loom model and stage 5's
   gates run in CI.
10. **creek under load.** Its thread-per-stream and 1 ms poll costs are from
    its source, not measured. They matter only if the decision in §3 is
    reopened.
11. **Does preroll by channel index still have a role for a disk voice that is a
    graph node?** **Settled 2026-11-10: no** (§2.9, decision 23). It doubles the
    compiler's output delay for a voice that feeds an output and reads an
    unrelated entry otherwise; the memory tier and the export fork already apply
    none. Not run: the finding is from reading `tutti-graph`'s solve and
    compile, the session's republish and the streamer's use, and cut 2's test, `tier_alignment`,
    is what ran it (it fails with the preroll restored). Open only: whether `PrerollTable` survives the un-parking of
    `bevy-tutti`, its one other consumer.
