# 026: stretch and pitch lock

A sampler clip that plays at another tempo than it was recorded at must change
its duration without changing its pitch, and keep landing on the beat when the
project's tempo moves. Tutti's stretcher (`tutti_sampler::stretch::Unit`)
cannot do either well: it collapses a stereo image, distorts a non-integer
ratio, drops out at a change of ratio and sounds late, and nothing feeds it the
tempo. This design replaces it with a stretcher written as doc 024's class 6
(multirate or time-scale), built in `tutti-dsp`, plus the pitch lock that
drives it from the transport.

Status: design only; nothing here is implemented. It follows the owner's
decisions of 2026-11-07 (a trait first with an enum at most on top, in-house
first with a listening gate, a power link with caller-declared groups,
precomputed onsets, `Playback` as a source rate and a transpose) and the
triage of 2026-11-10. **Stage 0 is an owner step and gates the rest: the blind
listening test (§8) is pending.** The model is doc 024's, the sampler's voice
wiring is this doc's, and the disk tier waits for doc 025 stage 5.

## Summary

- **The stretcher is doc 024 class 6, not a parallel trait family** (§3). A
  `StretchDesign` maps `(ratio, transpose)` and a structural part (quality tier,
  channel groups) to a `TimeScale` model whose queries are the latency, the
  duration ratio, the read-ahead for a ratio (`reach`) and `Graded` quality. Its
  realizations implement `Realization<TimeScale>` and a position-driven shape,
  `Retime<M>`, which is constraint 4's "variable-rate pull" made concrete.
- **Three realizations of one model** (§4): `Varispeed` (the resample mode, pitch
  tied), `LockedVocoder` (the in-house phase-locked vocoder) and, only if the
  listening test says so, slice and mono modes. A `Transposed<R>` wrapper adds
  pitch to any stretching realization by resampling. An enum, if wanted, only
  builds one of them.
- **The voice gives a position, not a filled FIFO** (§5). The stretcher is handed a
  `Source` (random access, zero outside the file) and a `Warp` (a plain function
  from output frame to source position) and renders on demand, so it is
  centre-aligned, reports **zero latency** to PDC, reads ahead (2144 to 13552
  frames by ratio) and does not depend on the block size.
- **Defects of today's `Unit` become class 6 contract cases** (§7): the stereo
  collapse, the impure non-integer ratio, the hole at a ratio step and the late
  onsets are each an analytic figure with a mutant that reproduces the defect.
- **Pitch lock is a warp** (§6): `Playback` carries a clip BPM; the voice builds
  the beat-to-source map from the block's `Env`; position is a pure function of
  the beat, so it cannot drift. A tempo change inside the stretcher's read-ahead
  is exact only once the engine can show tempo ahead of the block (doc 020
  stages 3 and 4); until then the voice holds the last tempo (§6.3).
- **Stages are PRs that stay green** (§9). The old vocoder is deleted once, never
  moved (§9, §11).

## 1. Today

| Piece | Where | What it does |
|---|---|---|
| `Unit` | `tutti_sampler::stretch` | A frame-in, frame-out filter with plain methods, not a trait. One vocoder per channel, FFT 2048 (`FftSize`), hop a quarter window. `stretch_factor` and `pitch_cents` are cells. |
| `Unit::input_rate` | same | How fast the **caller** must advance its source cursor: `1 / stretch`. The pitch half is internal: `tick` drops or repeats whole fed samples (`intake_rate`). |
| Callers | sampler voice: `PlaybackSlot` (field `stretch`, `prime_stretch`, `render_lanes`), `VoiceCommand::UpdateStretch`, `VoicePool`, `VoiceNode` | A memory voice reads its source at `placed_positions(.., stretch_rate)` and feeds the unit; a disk voice publishes the stretch into its stream's `Cells` (`set_stretch_rate`) and the streamer reads at the composed rate. |
| `Playback` | `tutti_sampler` voice types | `speed: PlaybackRate` (varispeed), `stretch: StretchFactor` (0.25 to 4.0) and `pitch: Cents` as three independent fields. |
| Latency | `Unit::latency_samples` | One window (2048 frames) while processing, 0 bypassing. The voice's `Shape` declares **0**, so PDC sees none and the delay is simply uncompensated. |
| After a jump | `PlaybackSlot::prime_stretch` | A flushed unit holds no window, so a memory voice re-feeds it `refill_frames` of source after every seek or wrap. |
| Tempo | `Run::tempo` | Read by the placed voice for its position; **nothing sets the stretch from the tempo.** |
| Size | `crates/tutti-sampler/src/stretch/` | 1,477 lines of source and 1,831 of tests. |

Doc 024 already files it: inventory row 42 ("Partly fits": the realization
consumes at a variable rate, so it is not an `Effect<M>`), class 6, and
consolidation 8 (move the vocoder to `tutti-dsp`). The loop-wrap hard cut found
in the same audit is already fixed (`WRAP_FADE` in `voice/clock.rs`); it is not
in this doc.

### 1.1 The defects, measured

Measured on `Unit` through the real voice and engine, stereo 48 kHz, ratio =
output duration per source duration. Each is a case in §7.

| Defect | Figure | Cause |
|---|---|---|
| **Stereo collapse** | a two-source mix with L/R correlation 0.692 comes out 0.018 / 0.003 / 0.269 at 0.5 / 1.5 / 2.0 | one independent phase solution per channel |
| **Non-integer ratio** | a 5 kHz sine is -19 dB impure at 1.5 and -9 dB at 0.5, against -60 dB at 2.0; 220 Hz is -44 dB at 1.5 | the intake drops or repeats whole samples, no interpolation |
| **Hole at a ratio step** | 0.8 to 1.0667: level dips 11.6 dB, second difference 0.50 against 0.04; 1.0 to 1.05: dropout to -240 dB | the stream is flushed by a change of intake |
| **Late onsets** | attacks land +19 / +33 / +40 ms at 0.5 / 1.5 / 2.0; click spread 6 to 21 ms against 0.44 in | a window of delay nothing compensates; the transient is smeared by the window |
| **Static stretch loses the beat** | a static-stretch voice after a 120 to 90 tempo change: -100 ms and missing clicks | the stretch is not tied to the tempo |

## 2. What the stretcher must carry

From the in-house prototype (447 lines, outside the repository) and the
comparison runs. These are the requirements; the figures they rest on are
measured and are what §7 asserts.

| Requirement | Figure |
|---|---|
| **Source window** | per frame, 2048 samples of every channel, starting 1024 behind the analysis centre, at an arbitrary integer position; zero outside the file. With an island the centre can sit up to a hop (512) behind nominal. |
| **Read-ahead** | the highest source index touched beyond the ideal position of the output sample being made: 2144 frames at ratio 4.0, 2800 at 2.0, 3307 at 1.5, 5355 at 0.75, 7408 at 0.5, 13552 at 0.25 (45 to 282 ms). Behind: about 1536. These fit `1300 + 3070 / ratio` within 4%; the implementation's `reach` is the contract, not the fit. |
| **Ratio ahead** | a frame is built when its first sample is due and its ratio is sampled at its centre, so the warp is needed up to `window / 2 + hop` = 1536 output frames beyond the frame being made, while `Env` shows one block. |
| **Seek and wrap** | the phase solution restarts, the stream restarts 3 frames early, the onset detector is re-warmed (or fed). |
| **Fork** | a fresh instance per `Editor::fork`: the state is large and has no meaning copied. |
| **Memory** | 121 KiB for one channel, 153 KiB for two, about 38 KiB per extra channel at 16; all allocated in `new`, none after (0 allocations in 600 blocks with the ratio changing every 40). |
| **Determinism** | output independent of the block size (blocks 1, 7, 64, 333, 1000, 4096 against 128: 0 differing samples, with a ratio ramp 0.8 to 1.6) and bit-identical on a repeat. Scalar `RealFft`, `f32` and `sin_cos`; across libms the last bit can differ, so a golden digest is asserted only on its recorded target (CLAUDE.md). |
| **Attack placement** | within 5 ms of ideal at every ratio 0.25 to 4.0 (measured worst 0.15 ms), click spread 0.44 ms (the input's), one envelope peak. |
| **Cost** | 14.0 us for a stereo 128-frame block at the prototype, against 32.0 us for `Unit` and 33.6 us for Signalsmith on the same loaded machine; 20.0 against 35.0 us on a lighter run. 32 stretching stereo voices are 0.64 ms of a 2.667 ms block on one thread; 128 need the worker pool. Absolute microseconds are not trustworthy (load 46 to 100). |
| **Limits** (documented, not requirements) | FFT resolution is poor below about 100 Hz and for two components closer than about 2 bins (47 Hz): 50 + 80 Hz alone is -13 dB impure. An island plays about 53 ms of source at unit rate whatever the ratio, so the read position leads (ratio above 1) or lags (below 1) and slews back at 25% per frame. Two onsets closer than 5 frames (53 ms) are one island. |

Two requirements the prototype could not show and §6 and §8 settle: the ratio
ahead of the block, and the seek and wrap.

## 3. The shape: doc 024's class 6

Doc 024 §2.2 names class 6: **multirate or time-scale**, stateful, periodically
time-varying with an LTI prototype, answering "the ratio, the prototype's
response, the latency, the duration ratio"; §4.2 says it "gets its model traits
with the resampler in step 7", and §4.3 (constraint 4) says a stretcher
implements `Realization<M>` for the lifecycle and the contract and "a different
shape trait for their block". This section writes that out. It adds no trait
beside doc 024's.

### 3.1 The design: `StretchDesign`

```rust
// tutti-dsp, in 024's `model` module. (proposal)
pub struct StretchControls {
    /// The duration ratio the model answers its queries at (output per source).
    /// The render follows the `Warp`; this bounds it and sizes `reach`.
    pub ratio: StretchFactor,                 // 0.25 ..= 4.0, as today
    /// Pitch shift, independent of the ratio in a stretching realization.
    pub transpose: Cents,
}

pub struct StretchDesign {
    quality: Quality,                         // Graded: Draft 1024, Standard 2048, High 4096, Best 8192
    groups: Groups,                           // structural: see 3.3
    channels: Channels,
}

impl Design for StretchDesign {
    type Controls = StretchControls;
    type Model = TimeScaleModel;              // a few dozen bytes: ratio, transpose, window, groups
    type Realization = LockedVocoder;         // the default; `Varispeed`, `Transposed<_>` are others
    type Bounds = StretchBounds;              // window, channels, max block: the bytes of §2
    type Range = ();
    const CADENCE: Cadence = Cadence::Block;  // the warp is a signal, not a control (3.4)
    // channels, blank, design_into, bounds, measure, realization as 024 §4.1
}
impl Graded for StretchDesign { /* quality, with_quality */ }
```

`Quality` is the FFT size: the stretcher's window is the quality choice 024 §4.7
names. A tier changes `reach`, the bytes and `Fidelity`, never `latency` (always
0); changing it is a rebuild through the graph's crossfade, as any structural
change. Only Draft (1024: purity -65 dB at 220 Hz, click spread 0.51 ms at 0.5)
and Standard (2048: -77 dB, 1.78 ms at 0.5 before window switching) are
measured; High and Best are settled by the `Graded` case (§7), and a tier that
fails it is dropped, not shipped.

### 3.2 The model: a class 6 trait

```rust
/// Class 6: a multirate or time-scale system. Shared by the stretchers, the
/// varispeed read and the resamplers. (proposal; the extension is `reach`)
pub trait TimeScale: System {
    /// Output frames per source frame at the nominal setting: the duration ratio.
    fn ratio(&self) -> f64;
    /// **Extension of class 6 (024 adopts):** the source a realization reads to
    /// make output at source position `p`, for a warp whose slope stays within
    /// `slope`: it reads `[p - behind, p + ahead]`. A resampler's is its kernel's
    /// half-length (a few frames); a vocoder's is §2's table.
    fn reach(&self, slope: Range<StretchFactor>) -> Reach;
}
pub struct Reach { pub behind: Samples, pub ahead: Samples }
```

`System::latency` is 0 for every realization here: a position-driven voice has
random access, so a centre-aligned stretcher reads *ahead* instead of delaying
(§5.4). `System::tail` is one window (`Finite`): past the end of the file the
source is zero and the overlap-add rings out in one window. `Fidelity` hangs off
the model as 024 §4.2 says: the stopband of a resampler, the interpolation droop
of the varispeed read, the vocoder's purity floor.

### 3.3 Channel groups: the one structural addition

`Groups` is a partition of the channel indices, declared by the caller, and
lives in the design (not in `Controls`): it changes the state's shape, so
changing it is a rebuild, like a quality tier. A stereo pair is linked by
default (`Groups::of(ChannelTopology)` links a front left and right and leaves
every other channel alone). The prototype measured why this is caller-declared
and not guessed: unrelated channels gain nothing from a link and are exposed to
each other's lobes (§4.2), while a pair that shares a source needs it, and
`ChannelLayout` is a count that carries no placement. **024 adopts `Groups`** as
a field a design may carry beside `Channels`; no other class uses it.

### 3.4 The realization: `Retime<M>`

```rust
// tutti-dsp, beside `Effect<M>`. (proposal) 024 constraint 4: `process` is not in
// `Realization`, because this shape consumes at a variable rate.
pub trait Retime<M: TimeScale>: Realization<M> {
    /// Fill `out` (one slice per channel; frame `i` is output frame `start + i`).
    /// Allocation-free. Frame `start + i` is made from `src` around `warp(start + i)`.
    fn render(&mut self, src: &dyn Source, warp: &dyn Warp, onsets: &Onsets,
              start: Frame, out: &mut [&mut [f32]]);
    /// **Extension of class 6 (024 adopts):** the playhead jumped (a seek, a loop
    /// wrap): continue at the warp's new position without clearing the overlap-add
    /// tail, so the two passes cross-fade over one synthesis window. `reset` stays
    /// 024's (silence, equal to a fresh realization).
    fn splice(&mut self);
}

/// Source samples of every channel from any integer position; zero outside the file.
pub trait Source { fn read(&self, channel: usize, from: i64, into: &mut [f32]) -> Fill; }
/// A plain function of output time: where in the source the output frame sits.
/// Monotone non-decreasing; a constant ratio `r` is `at + frame / r`.
pub trait Warp { fn position(&self, frame: Frame) -> f64; }
/// Source positions of attacks, sorted. Precomputed on the control side; `Unknown` makes
/// the realization scan ahead itself.
pub struct Onsets { /* Arc<[Samples]> or Unknown */ }
```

Why a `Warp` and not a ratio (the owner's words were "`Stretcher` takes only a
ratio"): a ratio is the warp's slope, and the integral of a stale ratio is a
*permanent* position error, while a position map is memoryless in position. A
constant ratio is the warp `at + frame / r`; the contract runs on constant and
ramp warps. The owner confirmed this reading on 2026-11-10 (§10, D6).

`Source::read` is a bulk copy, not a per-sample closure (the prototype's closure
was the cheap way to measure). `Fill` says whether the whole span was resident;
a disk source that is short returns what it has, the realization renders the
missing as zero, and the voice counts it like a streamer underrun (doc 025's
`Starved`). Reverse is a `Source` adapter that mirrors the index, so the warp
stays increasing and `SetDirection` costs the stretcher nothing.

### 3.5 What `DesignNode` is not

A sampler voice has no audio input and is driven by a transport, so it is not
a `DesignNode<D>` (024 §5). It is doc 024 Q12's case: the voice adopts `Design`,
the class 6 traits and the contract, and keeps its own node.

## 4. The realizations

### 4.1 `Varispeed`: the resample mode

`Varispeed` reads the source at `warp(frame)` with the cubic Hermite kernel
(`tutti_dsp::cubic_hermite`) that both sampler tiers already share. Pitch is tied
to the ratio, so `transpose` composes into the warp's slope. Its `reach` is the
kernel's four taps; its `Fidelity` is the kernel's droop and alias floor.

**Varispeed is the stretcher family's, not the export resampler's, and shares
only the model trait.** It is a position-driven read of random-access source with a
time-varying ratio and no latency, the sampler's "playback read" that 024 lists in
class 6. The export `FftResampler` converts a fixed rate ratio of a pushed stream
in chunks with latency and a stopband; the two share `TimeScale` (a ratio, a
latency, a `Fidelity`) and the class 6 contract cases, not a realization. If
024 step 7's resampler PR lands first it writes `TimeScale`; this doc's §3.2 is
the content it adopts. Its realization keeps its push shape (a fixed ratio over
a pushed stream, in chunks, with latency) under `TimeScale`, and is not
expressed as `Retime` over a ring `Source` (D15).

### 4.2 `LockedVocoder`: the in-house stretcher

A phase vocoder with identity phase locking, written as one `Retime` over
`tutti_dsp::RealFft`, scalar and `forbid(unsafe_code)`, with these parts:

- **On-demand output.** A frame is built when its first sample is due and the
  output is drained per sample, so it cannot depend on the block size. The ratio
  (here the warp's slope) is sampled at the frame centre.
- **Phase solution as a rotation.** `theta` advances by `omega * (hs - ha)` per
  frame, algebraically the classic propagation but independent of which channel
  is the reference.
- **Power link** (`Link::Power`). Peaks come from summed *power* over the
  channels of a group; each bin's reference phase comes from its loudest channel
  (switching only if another is 3 dB louder, so a tie cannot flip it); one
  rotation applies to all channels of the group, so each keeps its phase offset to
  the reference. The sum link, which builds the solution from the signed sum,
  loses antiphase content by 27 to 36 dB (L = -R) and the 180-degree partial by
  42 to 67 dB, and drifts quad phase by 24.5 / 25.3 / 15.5 degrees at ratio 1.5.
  The power link keeps both at 0 dB and 0 degrees, and stereo correlation within
  0.03 of the sum link's. It costs 5 to 13% more CPU at 6 and 16 channels.
- **Attack placement** (the main risk, and the reason the old plv failed by
  10 to 29 ms). A time-domain onset detector scans the random-access source ahead
  of the frame being built (1 ms energy of the first difference, rise 6 times the
  last 10 ms, 60 ms refractory; constants untuned). At an onset, five frames
  around the attack run at unit rate with no phase change (an **island**), placed
  so the attack lands at its ideal output time; frames before the island see the
  source masked from the attack on (a truncated analysis window: the window
  switching). Afterwards the read position slews back at 25% per frame.
- **Onsets are an input** (`Onsets`): precomputed on the control side, one scan
  over the whole source with the same detector (one implementation, two drivers,
  as `tutti_measure::detect_onsets` is built over its `step_onset`); the live scan
  is the fallback for a source with no list. A list removes the scan, the 10 ms
  warm-up after a seek, and the false trigger a mid-note seek would give the live
  scan. The detector lives in `tutti-dsp` beside the vocoder: it locates an
  attack to a millisecond for a processor, while `tutti-measure`'s spectral-flux
  detector is a frame-grade measurement; whether the second can seed the first
  is Q4. The same list feeds a slice mode.

Counts of the live detector on the listening excerpts (3.9 to 4.3 per second on
a funk loop, 5.3 to 5.8 on a rap mix, 0.5 to 1.0 on a solo vocal, and only 1 in
six seconds on the electronic mix, the choir and the violin) say it is not
known whether a dense mix has no onsets or the detector is deaf there (Q3).

### 4.3 `Transposed<R>`: pitch on any stretching realization

`Transposed<R: Retime<TimeScale>>` renders `R` at `ratio * k` against the warp
`warp(frame / k)` and resamples its output by `k = 2^(cents / 1200)` with the same
Hermite kernel, so the heard pitch moves by `transpose` and the duration ratio
does not. It is generic, so a later realization that shifts on its own (formant
preserving) implements the wrapper's interface instead and is not resampled twice.
This replaces `Unit`'s internal intake, whose whole-sample drop-or-repeat is the
non-integer-ratio defect. For `Varispeed` there is no wrapper: transpose is
already in the slope. Whether Hermite's alias floor holds for `k` above 1 is Q5.

### 4.4 Modes, and the enum

The owner's list: resample, locked vocoder and slice first; a monophonic mode
(WSOLA or similar) only if vocal excerpts fail the listening test; Signalsmith
behind an `-ffi` crate only if it becomes the fallback. Each is a
`Retime<TimeScale>`. Slice mode (cut at onsets, play each slice at unit rate)
is a second implementor fed by the same `Onsets`; it is not designed here
beyond the fit, and is built only if drums fail the vocoder in the listening
scores. The choice is per clip and explicit; there is no engine-level "auto". If
an enum is added it is a convenience that builds a boxed realization.

## 5. In the sampler voice

### 5.1 `Playback`: a source rate and a transpose

`Playback` gains `rate: PlaybackRate` (source frames per output frame) and
`transpose: Cents`, and loses `speed`, `stretch` and `pitch`. The mode is
explicit:

| Mode | Heard pitch | Duration ratio |
|---|---|---|
| `Resample` (varispeed) | rate x transpose, tied | `1 / rate` |
| a stretching mode (`LockedVocoder` and the rest) | transpose alone | `1 / rate` |

`StretchFactor` stays as the unit of the ratio (0.25 to 4.0). `VoiceCommand`'s
`UpdateSpeed`, `UpdateStretch` and `UpdateReverse` become a rate update, a
transpose update and a direction update; the direction is a `Source` adapter
(§3.4) on the memory tier and the streamer's mirrored mapping on the disk tier,
unchanged. A voice at unity rate and zero transpose bypasses, as a unity stretch
does now (`stretch_values_want_filter` is the threshold today: factor off by
0.001 or 0.5 cent).

### 5.2 Seek, wrap and fork

- **A jump is a `splice`.** The voice calls it on `Run::jump` (a seek, or a
  transport loop wrap), in place of the flush and `prime_stretch`. The
  realization restarts its stream 3 frames before the warp's new position and
  keeps its overlap-add tail, so the old and new passes cross-fade over one
  window. That replaces the voice's `WRAP_FADE` tail on a stretched voice (a
  second stretcher stream for the tail would double the cost for 5 ms). A clip's
  own loop is the `Source`'s (the seam blend `LoopSpan` already makes), not the
  stretcher's. Whether the splice is inaudible at a hit on the loop's first
  frame is Q6.
- **Fork is `fresh`.** `VoicePool::clone` already clones each slot's unit fresh;
  the realization's `fresh` is the same (024 core case "Reset, fresh").
- **Retirement.** The realization is built on the control side and travels in the
  command, as the unit does (`Retired::Stretch`); dropping it is a control-thread
  job.

### 5.3 Zero latency and the read-ahead

The voice's `Shape` keeps `latency` 0 and reports nothing for the stretcher.
This is not the status quo: today's 0 hides a real 19 to 40 ms lateness; the
new realization is centre-aligned and its onsets measure 0. The cost moves to
the source, which must be readable to `position + reach.ahead` (§2). A memory
voice has the whole wave. A disk voice's ring must hold `behind + ahead` around
the read position at the ratio in use: 13552 + 1536 = 15088 frames at 0.25,
about 120 KB per stereo voice and 3.9 MB at 32 voices (arithmetic, not a
measurement). Whether the streamer keeps that, and the cold-seek time to first
sound, are Q7, settled by the disk-voice test of stage 7, after doc 025 stage 5
(`StreamReader`, the positional reader this needs).

## 6. Pitch lock

### 6.1 Clip BPM and the warp

A clip carries a `ClipBpm` (from the file, the user or `tutti_measure::estimate_tempo`).
Per run of the block (`Run`, already cut at each tempo change) the voice's
`BeatWarp` answers

```
position(frame) = origin_source + (beat(frame) - origin_beat) * 60 * file_rate / clip_bpm
```

with `beat(frame)` from `Run::beat_at` (closed form, the host's own beat). Its
slope is `tempo / clip_bpm` source frames per frame, so the duration ratio is
`clip_bpm / tempo`. **Position is a pure function of the beat**, so there is no
accumulated state to drift: the tempo-follow prototype held a 16-step 120 to 88
ramp to 1.1 ms of drift, against 16 to 38 ms when the control thread sets the
stretch (stale for up to a block while the position formula already uses the
new tempo). A static stretch loses the beat entirely (-100 ms and missing clicks
at 120 to 90).

The prototype was 41 lines in `slot.rs` and 5 in `types.rs`: a second arm in
`render_lanes` splitting the block by run, setting the factor per run and calling
the existing `placed_positions`. In this design the arm is `BeatWarp` and the
stretcher's `render`.

### 6.2 Warp markers later

Warp markers are the same map with more segments: a monotone piecewise-linear map
from beat to source frame, whose slope over the tempo is the ratio. The voice
takes a map, not a number, from the first cut (a one-segment map); markers are a
longer segment list, and need no change to `Retime`.

### 6.3 The tempo ahead of the block

A frame is built 1536 output frames before it is heard, so the warp is asked for
positions up to `block end + 1536` while `Env` carries the transport at the
block's first frame and at each change inside it. Two cases:

- **Constant tempo across the lookahead** (the common one): `BeatWarp` extends
  the last run linearly and is exact.
- **A tempo change inside it:** the voice holds the last known tempo beyond the
  block. Frames built early use a position off by up to
  `1536 * |1/ratio_old - 1/ratio_new|` frames (about 8 ms for the 120 to 90 step),
  for the at most three frames in flight; the next block shows the change and the
  warp is exact again. The error is bounded and does not accumulate, but **the
  render at a tempo change then depends on the block size**, which doc 020 §7
  forbids for the map. So this cut is a documented limit, not the end state.

The end state needs the engine to show tempo ahead of the block: doc 020 stage 3
(steps in the walker) and stage 4 (ramps), plus a bounded view of the map beyond
the block (a `Shape` lookahead the engine fills into `Env` from the block's one
`Loan`, so a node never lends the map itself and sees a different publish than
the engine). The map is published since stage 2 of doc 020 (#388), but `Env`
still shows no tempo past the block. That addition is a doc 020 and doc 013
change, listed in §11. Then a tempo change is exact and block-size independent,
and ramps follow.

## 7. The class 6 contract

Doc 024 §6 writes the contract once per class, by group, with a mutant for
each case. Class 6 has one row there today ("ratio exact; latency a whole number
of frames; output frames equal ratio x input; a swept tone's stopband"), which
fits the resampler and not the stretcher. This doc splits it by the axis that
matters: **`TimeScale`** is the common group; **`PitchPreserving`** is a marker
group a stretching realization opts into and `Varispeed` does not (its pitch
follows the ratio, so those cases would fail by design).

```rust
design_contract!(StretchDesign: TimeScale);                       // design cases
realization_contract!(StretchDesign: TimeScale, PitchPreserving); // LockedVocoder
realization_contract!(VarispeedDesign: TimeScale);                // Varispeed
```

Every figure is analytic or from the spec; no case compares one render against
another (CLAUDE.md). The measured value is what the prototype did; the asserted
value leaves headroom and still fails the defect.

### 7.1 Core cases (every class), as they bind here

| Case | Figure for class 6 | Mutant |
|---|---|---|
| Latency | `latency()` = `System::latency()` = 0; the measured shift of a click's onset against `warp` is 0 (the old `Unit`: +19 to +40 ms) | delay the render by a window |
| Reset, fresh | equal to a freshly built realization | a `reset` that keeps the overlap-add tail |
| Block independence | blocks 1, 17, 64, 333, 1000, 4096 bit-identical, **with the warp changing** (a ramp 0.8 to 1.6); a repeat bit-identical | build a frame at the block start instead of when due |
| Stream clock | the ramp of a retarget (transpose) runs from the last reset, not a block edge | restart it per block |
| No allocation | `render` and `splice` under `assert_no_alloc` with the warp changing every 40 blocks, 600 blocks | allocate on a new island |
| Bounds dominate | `bounds()` covers `measure(design())`; 121 KiB mono, 153 KiB stereo, +38 KiB per channel at 16 | undersize by one |

### 7.2 `TimeScale` group

| Case | Figure | Mutant |
|---|---|---|
| Design: ratio and range | `ratio()` within 0.25 to 4.0; `reach` is non-increasing in `ratio` and `ahead` at 0.25 is at least the measured 13552 | clamp the range wrong |
| Reach covers, tight | an instrumented `Source` records the highest and lowest index read; both within `reach`, and `ahead` not above 110% of the measured at the ratios of §2 | read one window further |
| Duration | a marker at source frame `s` appears at output frame `warp^-1(s)` within one frame | drift by a hop |
| Splice | a tone through a seek or wrap: the largest first difference at the seam within 2 times the steady one (the old hard cut: 0.527 against 0.0016); the first frame within 1 dB of steady and 1 cent; a sustained tone seeked mid-note triggers no island | clear the overlap-add tail; start the stream late |
| Ratio step | 0.8 to 1.0667 and 1.0 to 1.05: the level dip under 1 dB (measured -0.3), no frame under -20 dB, the second difference at the step within 2 times the steady one (0.0023 against 0.0021) | flush the stream on a change (the old -11.6 dB and -240 dB) |
| Unity identity | ratio 1.0 and 1.0001 reproduce the input: no sample differs by more than 1e-3 | take the gap against the advanced target (the old one-hop-early stream) |

### 7.3 `PitchPreserving` group

| Case | Figure | Mutant |
|---|---|---|
| Pitch | a sine at 220 Hz, 1 kHz and 5 kHz at 0.5 / 1.5 / 2.0: within 1 cent (measured 0.0, 0.3 worst); a harmonic stack f1 and f8 within 1 cent | resample once more at the intake |
| Purity | impurity of a 5 kHz sine at 0.5 / 1.5 / 2.0 under -45 dB (measured -53 to -57; the old -9 / -19 / -60); a 220 Hz sine under -60 dB (measured -77; old -18 / -44 / -78) | whole-sample intake with no interpolation |
| Attack placement | a click train at 0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0: mean error under 1 ms (requirement 5 ms; measured worst 0.15; old +19 to +55); click spread under 0.6 ms (input 0.44; old 6 to 24); exactly one envelope peak | disable the island; shift it one hop (the 10.7 ms early); ignore the onset list |
| Stereo image | a noise mix of correlation 0.692: out within 0.1 of it at 0.5 / 1.5 / 2.0 (measured 0.72 to 0.77; the old 0.018 / 0.003 / 0.269); ILD within 0.1 dB (7.04 in and out) | one phase solution per channel |
| Antiphase | L = -R: partial 1 within 0.5 dB of the input at 0.75 / 1.5 / 2.0 (the sum link: -33 / -27 / -36 dB); a wide mix with one antiphase partial, that partial within 0.5 dB | a signed-sum link |
| Quad | four channels at 90-degree offsets, channel 0 against 1 / 2 / 3: phase error under 1 degree at 1.5 (the sum link: 24.5 / 25.3 / 15.5) | a signed-sum link |
| Groups | two unrelated channels in different groups: crosstalk under -120 dB (measured under -127) | link across groups |
| Level and ripple | a 10-partial stack level within 0.5 dB, ripple under 0.3 dB (measured 0.20) | a gain error in the overlap-add |
| Two onsets | onsets 3 frames apart: both present in the output; the second no later than one island's unit-rate offset (53 ms) | drop the second onset |

### 7.4 `Graded`

The `quality` case of 024 §6.2 runs over `Draft` to `Best`: `Fidelity` (the
purity floor) non-increasing, `Bounds` of a lower tier dominated by a higher,
`reach` and the bytes non-decreasing, and every tier's `latency()` 0. A tier
that changes nothing fails.

### 7.5 What is not a case

The limits of §2 are not assertions: a case that asserts a known weakness holds
for the wrong reason. They are in the rustdoc and the listening notes: below
100 Hz, two partials within 2 bins, and the island recovery at ratios 3 to 4.
The voice's own tests (not the class's) assert that a stretched placed voice
holds the beat across a step (the lock cases below).

**Pitch lock cases** are the voice's, at `Engine` level, analytic:

| Case | Figure | Mutant |
|---|---|---|
| Hold the beat at a step | a click clip at 96 BPM in a 120 project, then 120 to 90 at beat 4: every click within 2 ms of its beat after the step settles (stretcher latency is 0, so none of the old +33 to +45 ms); no click missing | set the factor from the control thread |
| Ramp | a 16-step ramp 120 to 88: drift within 2 ms (prototype 1.1; host-side 16 to 38) | stair-step by block |
| Pure function of the beat | positions at beat `b` equal for two tempo histories reaching it | accumulate the position |
| Tempo ahead | until doc 020 shows tempo ahead: the held-tempo error at a step under 10 ms and gone within one window; after: bit-identical across blocks 64, 100, 512 | use the old tempo for the whole lookahead |

## 8. The listening gate

Stage 0 is not a PR. The owner scores a blind set of 18 items (six real
excerpts, at 0.75, 1.25 and 2.0): a funk drum loop, two full mixes (a rap beat,
an electronic track), a six-voice choir on held chords, a solo female voice and
a solo violin. Each item is three files rendered by three algorithms, with the
assignment hidden and changing from item to item: `Unit` today, the prototype
with the power link, and Signalsmith at its default preset. They are aligned in
time (each algorithm's own delay is measured on a click train and removed) and
matched in loudness (EBU R128, one target per item, peak under -1 dBTP). The
set, the instructions, the score sheet and the key live outside the
repository; **the test is the owner's and is pending.** The score is per
material (drums, mix, pads, vocal, single line) on transient smear, phasiness,
stereo image, and voice and tone.

The gate, confirmed by the owner on 2026-11-10: the locked vocoder goes ahead if
(a) it is best or tied against `Unit` in every material (a regression check; it
is expected), and (b) against Signalsmith it loses only in materials a mode
covers: drums to slice mode, vocal to a mono mode. If it loses in pads, a mix or a
single line, Signalsmith is the fallback behind a new `-ffi` crate (§12) and the
owner decides the C++ toolchain on CI. The detector's deafness on dense mixes
(Q3), the recovery after an island at ratios 3 to 4 and the window for bass (Q8)
are also read from the set.

## 9. Stages

Each stage is one PR that stays green. 024's steps are named where this depends
on them. **026 stages 1 on need 024 step 1** (`Design`, `Realization`, `Effect`,
`System`, the contract machinery); nothing here builds a parallel set of traits to
avoid that wait.

| # | PR | Content | Guard |
|---|---|---|---|
| 0 | The listening test | Owner step (§8). Nothing is built on the result until it is in. | The score sheet and key, tallied per material. |
| 1 | Class 6 in `tutti-dsp` | `TimeScale`, `Reach`, `Groups`, `Source`, `Warp`, `Onsets`, `Retime<M>`, `Graded` for `StretchDesign`, the `TimeScale` and `PitchPreserving` contract groups of §7 (against test doubles), and the `Varispeed` realization. No sampler change. | 024 core cases on `Varispeed`; the `TimeScale` group on it; each case's mutant fails it. |
| 2 | `LockedVocoder` without attacks | The vocoder, power link, groups, on-demand output, `splice`. No onsets (an island is stage 3). Decides whether it reuses the old `OverlapAdd` and `SampleFifo` or is self-contained (Q9). | The `PitchPreserving` cases except attack placement and two onsets; block independence; no allocation. |
| 3 | Attacks | The time-domain detector, `Onsets::scan` and the live fallback, islands, window switching. | Attack placement and two-onset cases, with the island and the shift-by-a-hop mutants. |
| 4 | The sampler uses it | The memory voice renders through `Retime` with `BeatWarp` at a constant rate; `Playback` takes `rate` and `transpose` and loses `speed`, `stretch`, `pitch`; `Transposed`; `prime_stretch`, `input_rate` and `intake_rate` go; **`crates/tutti-sampler/src/stretch/` is deleted (3,308 lines)**; CLAUDE.md's line on `stretch::Unit` is replaced in this PR; doc 013's `stretch::Unit` rows and doc 024's row 42 and consolidation 8 are updated. | The sampler's stretch tests that assert a figure are rewritten as §7 cases (never edited to pass); `sampler_to_export.rs` pitch and stretch cases; serial/pooled bit-identity; the export-fork test. |
| 5 | Pitch lock | `ClipBpm`, `BeatWarp`, the held-tempo rule of §6.3; the lock cases of §7.5. | Hold-the-beat, ramp, pure-function-of-the-beat. |
| 6 | Tempo ahead | After doc 020 stages 3 and 4: the engine's view of tempo past the block; `BeatWarp` reads it. Warp markers after. | Block-size independence at a step and a ramp; the held-tempo error case retired. |
| 7 | Disk voices | After doc 025 stage 5: a positional `Source` over `StreamReader`; the ring sized to `reach`. | The read-ahead test at 32 voices at 0.25 and 4.0, cold-seek time to first sound (Q7), no allocation. |
| 8 | Other modes | Slice, mono, or `-ffi` Signalsmith, each only if the listening gate (§8) or later use asks. | The class 6 groups, unchanged. |

**Against doc 024.** Stage 1 is a class-6 PR in 024's sense; it is 024 step 7's
"resampler and stretch" item written first for the stretcher, because the
position-driven voice read is the first user of class 6 and the pending listening
decision is this doc's. **Stage 4 deletes the old vocoder, so 024 step 6's
consolidation 8 (move `vocoder.rs` and `buffers.rs` to `tutti-dsp`) is not run
for it.** If step 6 lands before stage 4 the move happens once and stage 4
deletes the moved files; if after, consolidation 8 is struck. The vocoder is
never moved twice, and never moved only to be deleted: the owner struck
consolidation 8 on 2026-11-10 (D16), and doc 024 says so. Stages 4 and 7 order against
doc 025 as above: stage 4 does not touch the disk tier's `StreamReader`, which
stage 7 uses.

## 10. Decisions

| # | Decision | Ground |
|---|---|---|
| D1 | A trait first, an enum at most on top; the choice is per clip and explicit | Owner, 2026-11-07. The enum only builds a realization; no "auto" |
| D2 | Class 6 shape: `TimeScale` model, `Retime<M>` realization, `StretchDesign` | Owner constraint, 024: no parallel family. §3 |
| D3 | Extensions to class 6 that 024 adopts: `reach`, `splice`, `Groups`, and the `TimeScale`/`PitchPreserving` contract split | §3.2 to §3.4, §7; listed in §11 |
| D4 | In-house locked vocoder first; Signalsmith is the benchmark and the fallback | Owner, 2026-11-07; the gate is §8 |
| D5 | Power link with caller-declared groups, stereo linked by default | Owner, 2026-11-07. Sum link fails antiphase and quad (§4.2); unrelated channels gain nothing from a link |
| D6 | The trait takes a **warp** (output frame to source position), not a bare ratio; a ratio is the warp `at + frame / r` | A stale ratio's integral is a permanent error; a position map is memoryless. Owner confirmed this reading of "takes only a ratio", 2026-11-10 |
| D7 | Onsets precomputed on the control side, live scan as the fallback | Owner, 2026-11-07. Removes the scan, the warm-up and the mid-note false trigger |
| D8 | Zero latency to PDC; the voice reads ahead | Owner, 2026-11-07. Reporting the lookahead would hold every other branch back and make a played voice late (§12) |
| D9 | `Playback` holds `rate` and `transpose`; varispeed is the `Resample` mode where they are tied; `speed` goes | Owner, 2026-11-07 |
| D10 | Varispeed is the stretcher family's realization, sharing only the model trait with the export resampler | §4.1 |
| D11 | A seek or wrap is `splice`, keeping the overlap-add tail; `reset` stays 024's silence | 024's core `Reset, fresh` case would fail a `reset` that keeps state |
| D12 | Pitch lock first cut: clip BPM, step tempo, the held-tempo rule; exact lookahead with doc 020 stages 3 and 4; ramps and markers after | Owner order (clean-plan items 30 and 31); §6.3 is the correction that block-size independence needs the lookahead |
| D13 | Disk-voice stretch after doc 025 stage 5, with a 32-voice read-ahead test | Owner, 2026-11-07 |
| D14 | The count-in and punch snippets are examples and tests, kept outside this doc; the app-level features are a note (§13) | Owner, 2026-11-07 |
| D15 | The export resampler shares `TimeScale` and the class 6 contract cases, and keeps its push realization; it is not `Retime` over a ring `Source` (was Q1) | Owner, 2026-11-10. A fixed ratio over a pushed stream needs no position map, and a ring adapter would add a copy and a second flush path for no caller |
| D16 | 024's consolidation 8 is struck: the old vocoder is deleted at stage 4, never moved (was Q10) | Owner, 2026-11-10. Its target is deleted whichever of the in-house path or Signalsmith wins the gate |
| D17 | The listening gate of §8 is the rule as written | Owner, 2026-11-10. The test itself is still the owner's and pending |

## 11. Required changes to doc 024 and others

Small, and made in this PR where they are a sentence:

1. **024 §4.2** ("Class 6 has no trait yet"): points here for `TimeScale` and `Retime`.
2. **024 §6.2**, the Multirate row: split into `TimeScale` and `PitchPreserving`
   as in §7 (the resampler's stopband case stays as a resampler case).
3. **024 §9, consolidation 8, and step 6:** struck (D16).
4. **024 §3, row 42:** the stretcher is a `StretchDesign` with a `Retime`
   realization; the "variable rate" reason it did not fit is now `Retime`.
5. **Not made here, needed:** doc 020, a bounded view of the map past the block
   (§6.3), and doc 013's change note; CLAUDE.md's `stretch::Unit` line at stage 4
   (never in a doc PR).

The class 6 additions (`reach`, `splice`, `Groups`) are the only new shape; `fresh`
(a fresh instance per fork) is already `Realization::fresh`.

## 12. Rejected

- **Signalsmith Stretch** (MIT, C++ header-only): needs a new `-ffi` crate and a C++
  toolchain on every CI target including MSVC; 120 ms latency and 1.0 x `Unit`'s
  CPU; places attacks +90 to +180 ms late at its default, with click spread 6 ms at
  2.0; its SIMD rounding may break serial/pooled bit-identity (unchecked). Kept as
  the benchmark and the fallback.
- **Rubber Band:** GPL 2 or later, a commercial licence otherwise, against the
  workspace's `MIT OR Apache-2.0`.
- **Bungee:** MPL-2.0 (file-level copyleft), C++ with CMake, Eigen and PFFFT, a
  binding crate not vetted; worth a look only if both the in-house and the
  Signalsmith paths fail.
- **The sum link:** loses antiphase content by 27 to 36 dB and drifts quad phase 25
  degrees at 1.5 (§4.2).
- **Patching `Unit`:** the intake and the flush at a ratio change are in its design.
- **Reporting the lookahead as latency:** a latent voice would see the future
  tempo, but PDC would hold every other branch back by the lookahead (32 ms) and a
  played voice would be late. The owner chose zero.
- **A bare ratio in the trait:** D6.
- **An input-fed (pull-sequential) trait:** the voice has random access; the
  FIFO and `prime_stretch` exist only because `Unit` is fed, and a live input that
  would need real latency has no consumer.

## 13. Not in this doc

Count-in, punch in and out, cycle recording, tap tempo, quantize and swing are
app-level: policy over `At`, `Locate` and the recording flags, or edits of note
and clip data. A count-in is about 27 lines over the existing transport and the
metronome's preroll mode (8 clicks exactly on the beat, at block sizes 256 and
480); a punch is a `Node` of about 27 lines gating by `Env::for_each_beat`,
exact to the frame across a tempo change. The two snippets are kept as an
example and a test and are not in the repository yet. Chase on seek and the
external clock are MIDI stage 6. Ableton Link is not researched.

## 14. Unknowns, and how each is settled

| # | Unknown | Settled by |
|---|---|---|
| Q1 | ~~Does 024 step 7's export resampler adopt `Retime` over a ring `Source`, or keep a push shape under `TimeScale`?~~ | Decided: D15, a push shape under `TimeScale` |
| Q2 | Does the vocoder sound as good as Signalsmith on real music, and are islands audible in dense material? | The listening gate (§8) |
| Q3 | Is the detector deaf on dense mixes (1 onset in 6 s of the electronic track) or is there none? | The score for that item; then tune the constants or feed `Onsets` from a better producer |
| Q4 | Can `tutti_measure::detect_onsets` positions, refined in the time domain, replace the stretcher's own detector? | Compare the two on the excerpts as detectors (not renders); keep one if they agree |
| Q5 | Does Hermite's alias floor hold in `Transposed` for `k` above 1? | The purity case with a transpose up a fifth; if it fails, a windowed-sinc kernel from `tutti-dsp`'s resampler (024 consolidation 1) |
| Q6 | Is the `splice` inaudible at a hit on a loop's first frame, at 0.5 and 1.5? | The splice case plus a loop-start click at both ratios; fall back to a second stream for the tail only if it is not |
| Q7 | Does the disk ring cover `reach` at 0.25 and 4.0 with 32 voices; what is the cold-seek time to first sound? | The stage 7 test (`DiskStreamer::manual`) |
| Q8 | Is a 2048 window right for bass and LFE content? | The window is the quality tier: listening on a bass-heavy excerpt; per-band windows only if it fails |
| Q9 | Does the vocoder reuse the old `OverlapAdd` and `SampleFifo`? | Stage 2: reuse only if the on-demand drain fits them; otherwise they die with the old module |
| Q10 | ~~Strike 024's consolidation 8 now?~~ | Decided: D16, struck |
| Q11 | Serial against pooled bit-identity of the vocoder, and across CPUs? | The `modes` suite of the differential tests in the pool; `sin_cos` is libm, so a digest only on its recorded target |
| Q12 | Production CPU through the real voice on a quiet machine? | `cargo bench` on the voice pool, off a loaded machine (load was 46 to 100) |
| Q13 | A ratio change that cuts an island short; a loop wrap inside one? | Cases written with the island in stage 3 |
| Q14 | Do High and Best (4096, 8192) pass the `Graded` case? | The case; drop the tier if not |
| Q15 | The tempo-ahead view: a `Shape` lookahead filled into `Env`, or something else? | Doc 020 stage 3, with the single-`Loan` rule of §6.3 |
