# Modulation is a noun of the API, not of the engine

Status: **proposed** (2026-10-14). Supersedes the param-modulation design of
[doc 013](013-native-graph.md) ("Item 6 landed: compiler-owned param
modulation" and the control-rate tier it sits beside). Nothing here has
landed; the [campaign](#12-the-campaign) lists the PRs that land it.

All file references are to `main` at `8ae55f4b`.

## Summary

- **The engine models no modulation.** A param is a base value plus a
  sample-accurate event stream, clamped to its range. LFOs, depth,
  polarity, routes and macros are words of the API.
- **One value rule:** `value = clamp((automation(t) if the target's lane has
  automation, else knob) + Σ modulation offsets(t), range)`. Automation
  replaces the knob; modulation adds.
- **The API compiles; a node evaluates.** `tutti-mod` compiles sources and
  routes into one **control program** per target, published through
  `RtPublish`. A **control lane node** reads it once per block and sends
  `ParamSegment`s to a native param port or a hosted plugin's parameter.
- **Export equals live** from the same start beat, under the conditions
  §4.3, §5.3 and §6 state. A lane is a pure function of its program, its
  knob, its inputs, the transport and one graph-wide play clock, and a fork
  copies all of them.
- **One cell per fact.** A native target's knob is its node's `Param` cell
  and its range is its port's range cell; the lane reads both and mirrors
  neither.
- **Signal-driven sources are ordinary nodes** (an envelope follower, a note
  envelope) that send into the lane's event input.
- **Plugins get changes only**, for every listed parameter, in every chunk.

## 1. The principle

A modulation matrix is what the user *says*; the engine should *run* a value
and the events that move it, as SQL splits from its query plan. No engine
surveyed (Web Audio `AudioParam`, CLAP `PARAM_VALUE` + `PARAM_MOD`, VST3,
JUCE, nih-plug, SuperCollider, Bitwig, Ableton) puts a matrix in its graph
runtime; at most it models "a value plus offsets, clamped", which is §2's
shape. tutti modelled modulation twice, both times as an engine mechanism:
the graph's fused step `clamp(base + Σ shape_j(source_j))`
(`crates/core/tutti-graph/src/param.rs:11`), and a control-rate tier on
Bevy's frame clock (`tutti-mod`'s `routing`, `bevy-tutti`'s driver, the
plugin path's `PluginAutomation`). M3 is the structural defect: a mechanism
driven from outside the graph cannot be in a fork. The rest are second
copies of something the graph already does.

| # | Defect | Evidence |
|---|---|---|
| M1 | **Depth means different things per tier.** `PerFrame`/`PerBlock` scale by the span; `PerSample` does not. Ranged 0..10 at `Depth(0.5)` swings ±5 against ±0.5. | `crates/core/tutti-mod/src/driver.rs:335-339`, `curve_source.rs:182-184` against `crates/dsp/tutti-nodes/src/param_mod.rs:46-55`, `crates/bevy-tutti/src/modulation/audio_rate.rs:300-307` |
| M2 | **A depth edit restarts every free LFO.** `Changed<ModRoute>` rebuilds every `Sourced` from phase 0. | `crates/bevy-tutti/src/modulation/source.rs:225-232`, `:305-331`; `driver.rs:1-6` |
| M3 | **Control-rate and plugin modulation never reach an export.** A fork freezes a plugin target to its authored part; there is no offline driver. | `crates/plugin/tutti-plugin/src/host/node/param_automation_source.rs:336-353`; `crates/core/tutti-graph/src/controls.rs:14-27`; doc 013 lines 1193-1200 |
| M4 | **Plugin delivery drops parameters past 16**, uncounted. | `crates/plugin/tutti-plugin/src/host/node/automation_node.rs:249-270`, `graph_node.rs:297`, `crates/plugin/tutti-plugin-types/src/automation.rs:175-178` |
| M5 | **Plugin params are re-sent every block**, overwriting GUI edits. | `automation_node.rs:151-180` |
| M6 | **Free modulation on a plugin stops with the transport.** | `automation_node.rs:164` |
| M7 | **Nested `RtPublish` reads per point** on the audio thread. | `automation_node.rs:156-170`, `param_automation_source.rs:327-334` |
| M8 | **Automation is an offset, not a value.** 200→2000 Hz under a 1000 Hz knob renders 1200→3000 Hz. | `param.rs:173-185`, `crates/dsp/tutti-nodes/src/automation/lane.rs:191-201` |
| M9 | **Two writers; a rebuild reseeds the base** from `ModParamRange.base`. | `crates/bevy-tutti/src/modulation/driver.rs` (`rebuild`); `crates/bevy-tutti/src/graph/param.rs:147-174` |
| M10 | **The per-sample source node goes stale and leaks.** | `audio_rate.rs:149-189` |
| M11 | **The sum is order-dependent** (`Vec` insertion order). | `crates/core/tutti-mod/src/layered.rs` (`value_at`) |
| M12 | **Two producers for one job** into a plugin input. | `automation_node.rs`, `lane.rs:203-210` |

## 2. The value rule

```text
value(t) = clamp( base(t) + Σ_layers offset_i(t) , range )
base(t)  = automation(t)  where the lane's automation has a value at t
         = knob           otherwise
```

- **Automation replaces the knob**, as in every major DAW, Web Audio and
  CLAP `PARAM_VALUE`. Where the curve has no value (before its first point,
  disabled), the knob holds: `Curve::value_at`'s `Option`.
- **Modulation adds**, in the target's units:
  `offset = shape(raw, depth, polarity, curve) × span`, one convention for
  every source kind (it fixes M1). Depth is a fraction of the range; `span =
  range.max − range.min` is derived by the lane from the live range cell,
  read once per block, so a range edit rescales every route on the next
  block with no republish (§4.2).
- **The clamp is per frame and applied once.** A native target's port
  clamps each frame to its range cell (`param.rs`, `ParamRanges`); the lane
  sends the unclamped sum. A plugin target has no port, so its lane clamps:
  a piece that crosses a bound is split at the crossing, which is a corner
  (§5.2). Clamping only a piece's endpoints would be wrong: a 0→10 ramp in
  `[0, 5]` must reach 5 halfway and hold, not ramp to 5.

| Path | Today | After |
|---|---|---|
| Native param, automated | knob + curve (M8) | curve |
| Native param, LFO `PerFrame` | `ModParamRange.base` + LFO × span, Bevy frame rate, not exported | knob (or automation) + LFO × span, per the control step, exported |
| Native param, LFO `PerSample` | knob + LFO, no span (M1) | as above |
| Plugin param | host base + layers, every block while playing (M5, M6) | knob (or automation) + Σ layers, changes only, rolling or not |
| Unmodulated, unautomated | knob | knob (no lane, no events) |

**Breaking:** "automation adds to the base" goes (§12, PR 2).

## 3. Where things live

Dependency facts, from each `Cargo.toml`: `tutti-graph` names only
`tutti-types`; `tutti-mod` names `tutti-types` and `audio-automation`;
`tutti-nodes` names `tutti-mod` and `tutti-graph`; `tutti-plugin` names
`tutti-nodes`; `bevy-tutti` names `tutti-mod` (optional) and `tutti-nodes`.

| Piece | Crate | Why there |
|---|---|---|
| `ControlProgram` and its evaluation | `tutti-mod`, ungated | The compiler produces it; it holds `Arc<dyn Curve>`, which is `tutti-mod`'s; `tutti-mod` cannot name `tutti-nodes`; `tutti-graph` models no modulation and names only `tutti-types` |
| `Curve`, `CurveSegment` | `tutti-mod`, moved out of `routing` into the floor | Automation needs them without routing |
| The compiler (`tutti_mod::compile`) | `tutti-mod`, ungated | Pure: no Bevy, no runtime |
| `Lfo`, `SampleHold`, `RandomState`, `Modulator` | **`tutti-nodes`**: copied there in PR 3; `tutti-mod`'s copies go in PR 5 | Stateful per-sample sources are a runtime. `routing` (`driver.rs`, `matrix.rs`, `curve_source.rs`, `routing.rs`) and `bevy-tutti` use `tutti-mod`'s until PR 5, and `tutti-mod` cannot name `tutti-nodes`, so the new nodes use the copies while the old system keeps its own. |
| `ControlLaneNode`, `LaneControls`, `EnvelopeFollowerNode`, `NoteEnvelopeNode`, `CONTROL_STEP` | `tutti-nodes` (re-exported at the root) | Graph nodes; `tutti-nodes` owns the lane |
| The play clock (`Env::play`, `play_epoch`, `Cx::play_at`) | `tutti-graph` | Transport plumbing every lane reads the same (§4.3) |
| A port's event source replaces its base | `tutti-graph` | The engine's one rule |
| `RangeCell` (packed `AtomicU64`) | `tutti-types`, beside `AtomicF32` | A plugin lane's own range; `ParamRanges` reuses it |
| Plugin delivery | `tutti-plugin{,-types,-server}` | Where `ParameterChanges` is built, carried, applied |
| Declarations, reconcile | `bevy-tutti` (`modulation`) | The API |

## 4. The control program

### 4.1 The type

```rust
// tutti-mod. Ungated; re-exported at the crate root.

/// What one target's lane evaluates: a base, plus offsets. A description:
/// no clock, no state; equal programs evaluate to equal bits. Values are
/// the target's raw `f32` (Hz, linear gain, a plugin's normalised 0..1):
/// the lane is erased over the unit, as `ParamSegment` is.
#[derive(Clone)]
pub struct ControlProgram {
    /// Replaces the knob where it has a value. Always a frozen curve
    /// (`Curve::frozen`): no `RtPublish` cell inside, so no nested read.
    pub automation: Option<Arc<dyn Curve>>,
    /// Time-function layers, summed in `id` order.
    pub layers: Box<[Layer]>,
    /// Event-input layers, summed in `id` order after `layers`.
    pub inputs: Box<[InputLayer]>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LayerId(pub u64);

#[derive(Clone)]
pub struct Layer {
    pub id: LayerId,
    pub clock: Clock,
    pub wave: Wave,
    pub amount: Amount,
    /// Offset scaled by a second wave mapped to `[0, 1]` (depth moved by an
    /// LFO). `None`: unscaled.
    pub depth_by: Option<(Clock, Wave)>,
}

#[derive(Clone, Copy)]
pub enum Clock {
    /// Phase = frac(beat / per_cycle + phase): bar-locked; follows seeks
    /// and loops; holds while stopped.
    Beats { per_cycle: BeatDuration, phase: PhaseIncrement },
    /// Phase = frac(rate · play + phase), on the graph's play clock (§4.3),
    /// continued through `anchor` after a rate edit in the same play epoch.
    Seconds { rate: Hz, phase: PhaseIncrement, anchor: Option<RateAnchor> },
}

/// Where a rate edit takes effect (§4.3). Phase is f64 cycles: an hour of
/// play needs more than `PhaseIncrement`'s f32.
#[derive(Clone, Copy)]
pub struct RateAnchor { pub epoch: u32, pub at: Frame, pub phase: f64, pub prev_rate: Hz }

#[derive(Clone)]
pub enum Wave {
    /// Closed form, raw in `[-1, 1]`. `Random`/`RandomSmooth` derive from
    /// `hash_bipolar(cycle, seed)`, so they replay after a seek and in a fork.
    Shape { shape: LfoShape, seed: u64 },
    /// One drawn period: breakpoints over `[0, 1)`, each with its piece's curve.
    Drawn(Arc<[PeriodPoint]>),
}

#[derive(Clone, Copy)]
pub struct PeriodPoint { pub at: f32, pub value: f32, pub curve: CurveType }

/// offset = shape(raw, depth, polarity, curve) × span. Depth is a fraction
/// of the range; the span is not here: the lane reads it from the live range
/// cell each block (§4.2), so a range edit needs no republish.
#[derive(Clone, Copy)]
pub struct Amount { pub depth: Depth, pub polarity: Polarity, pub curve: CurveType }

/// A signal source's raw value (events addressed `ParamAddr::Id(source.0)`),
/// shaped into an offset.
#[derive(Clone, Copy)]
pub struct InputLayer { pub id: LayerId, pub source: SourceId, pub amount: Amount }

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SourceId(pub u32);

pub struct ProgramTime {
    /// The transport beat at the frame; `None` with no usable tempo.
    pub beat: Option<Beat>,
    pub rolling: bool,
    /// The play clock (§4.3), its epoch, and its rate. A frame count, not
    /// `Seconds`: `Seconds` is `f32`, and an hour of play needs `f64` phase.
    pub play: Frame,
    pub play_epoch: u32,
    pub rate: SampleRate,
}

impl ControlProgram {
    /// The sum before the clamp; `inputs[k]` is `self.inputs[k]`'s raw value.
    /// Pure, allocation-free; layers in `f64`, summed from `-0.0` in id
    /// order, rounded to `f32` once.
    pub fn value_at(&self, knob: f32, at: &ProgramTime, inputs: &[f32]) -> f32;
    /// The left limit at `at`; differs from `value_at` only at a jump.
    pub fn value_before(&self, knob: f32, at: &ProgramTime, inputs: &[f32]) -> f32;
    /// The next corner strictly after `at` on each clock (§5.3 lists them).
    pub fn next_corner(&self, at: &ProgramTime) -> Corners;
    /// Exactly one curve's segments: no layers, no inputs (§5.4).
    pub fn is_pure_automation(&self) -> bool;
}

pub struct Corners { pub beat: Option<Beat>, pub play: Option<Frame> }
```

- **A depth edit is a republish, never a reset.** Phase is a function of
  time, not of the program's age (it fixes M2).
- **Why layers are summed in the lane.** LFOs at unrelated rates share no
  period, so no single curve holds their sum.
- **Why the lane holds `Amount` rather than a baked offset curve.** One
  signal source feeds many lanes, each at its own depth, so depth is per
  route and must sit beside the shared input; and `shape()` with a
  nonlinear curve does not commute with the sum, so it cannot be baked
  into a program ahead of the input's values.
- **`depth_by`** costs one evaluation and one multiply per point; its
  corners join the layer's.
- **No response-curve LUT.** `shape()` runs at control points only (§5.3),
  so it is exact.

### 4.2 Knob and range: one cell each, by target kind

| | Native param port | Plugin parameter |
|---|---|---|
| Knob | **The node's own `Param` cell** (`ParamSet::cell`, `crates/core/tutti-graph/src/controls.rs:113`), cloned into the lane at creation and loaded once per block. `write_param` writes it; nothing else does. | A lane-owned `AtomicF32`, written by `LaneControls::set_knob` (the API's writes and the plugin's GUI edits, §8.2) |
| Range | **The port's range cell** (`Editor::set_param_range`, already a store with no commit, `editor.rs:904`). The port clamps per frame. The lane holds a read handle to the same cell (`ParamRanges` moves onto `RangeCell`; PR 2 adds a public accessor) and loads it once per block for the span only. | A lane-owned `RangeCell`, loaded once per block for the span and for the clamp, which splits pieces at crossings (§2) |
| Program | `RtPublish<ControlProgram>` (`LaneControls::publish`) | same |

There is no mirror to reconcile ("derive at the point of use"). A range
edit is a store in both columns; a shrink pins the value and a widen
releases it at the next block, and the span every route scales by follows
on the same block. A route needs a finite range: the reconcile always sets
one (`ModRange`, else the param's declared range); a lane that reads a
non-finite span sends the base alone and counts it.

**The glide.** A knob, range or program change seen at a block's first frame is
not a step: the piece in flight is cut there and the next piece runs from
the in-flight value to the new value at the next point. That is a declick of
at most `CONTROL_STEP` frames with no extra state.

### 4.3 The play clock, and phase

**Beats.** `phase = frac(beat / per_cycle + offset)` from the transport's
beat at the frame. The beat is closed form from the segment's frame count
(`FrameClock`), live and offline alike (`crates/core/tutti-core/src/transport/offline.rs`,
`OfflineTimeline::advance` and `beat`, with the test
`a_block_on_a_beat_is_that_beat_exactly`), so it lands on the same bits at
any block size. Seeks and loop wraps are followed; stopped, the beat holds.

**Seconds: one graph-wide play clock.** A free layer needs a time that runs
while stopped (M6) and ignores seeks, loops and tempo. It is **`Env::play`**,
a `Frame` the executor keeps:

- it advances one per frame, rolling or stopped;
- the executor resets it to 0 on the frame of a stopped→playing transition
  (a start inside a block is in `Env`'s segments) and bumps `Env::play_epoch`,
  which counts play starts;
- every executor, a fork's included, starts it at 0; epochs come from one
  process-wide counter, so no fork's epoch equals a live one;
- nothing else resets it.

**As landed (PR 2a).** `Env::play` is a `PlayClock`, not a bare `Frame`
plus `Env::play_epoch`: `{ start: PlayReading, next_epoch }`, the reading
at the block's first frame and the epoch the block's first in-block start
takes (the `k`-th takes `next_epoch + k`, reserved by the executor before
the block runs). `Env::play_at(offset)` answers at any frame of the block
through the starts in `Env::changes`, as `Env::transport_at` does, so no
node walks the changes itself. A `PlayReading { epoch, play }` packs into
one `u64` as 24 bits of epoch and 40 of play (`2^40` frames is 265 days at
48 kHz); epochs are compared for equality only and wrap after `2^24` play
starts. The control side reads `Editor::play_reading()` (and
`Executor::play_reading()`), the next block's first reading. An `Env` built
by hand uses `PlayClock::ZERO` (epoch 0, which no executor takes: the
counter starts at 1). The reference interpreter keeps its own clock.

**Read at the input's time.** A node reads it through **`Cx::play_at(offset)`**,
which answers at the input's arrival time, as `Cx::transport_at` does
(`crates/core/tutti-graph/src/node.rs:875`, through
`Env::transport_before(.., arrival, history)`). `TransportHistory` is
extended to carry `(play_epoch, play)` beside the transport, so a play start
up to `arrival` frames back, in the previous block, is answered correctly.
Every lane and signal source reads the same clock this way, so one LFO
routed to several targets stays phase-coherent across them. `Env::frame` is unsuited:
it counts from the executor's construction (`exec.rs:701`), so a free
phase anchored there would depend on how long the live graph had run.

**Rate edits: a future-dated anchor.** Phase is a pure function of the
program and the play clock; the lane keeps no phase state.

- The executor publishes `(play_epoch, play)` once per block in one
  `AtomicU64` for the control side (`Executor::play_reading() -> PlayReading`).
- On a rate edit the compiler (given the previous program and that
  reading, §7) sets `t_anchor = play_estimate + margin`, with `margin ≥
  2 × max_block` plus publish slack, and `φ_anchor` = the previous
  program's phase at `t_anchor`, so phase is continuous.
- The seconds layer carries `RateAnchor { epoch, at: t_anchor, phase:
  φ_anchor, prev_rate }`: `phase(t) = φ_anchor + prev_rate · (t − t_anchor)`
  before `t_anchor`, and `φ_anchor + rate · (t − t_anchor)` from it.
- Every lane that reads the program before `t_anchor` switches on the same
  frame. One that reads it late (past the margin) jumps once to the common
  phase.
- An anchor whose epoch is not the current `play_epoch` is ignored: phase is
  `frac(rate · play + phase)`, so a play start (or a fork) restarts it at the
  declared phase.

A beat layer is never anchored: it stays locked to the bar.

**Export == live, and where it does not hold.** An export renders a
`ForkMode::Offline` fork (`crates/core/tutti-graph/src/fork.rs:490`) on a
timeline rolling from its first frame (`crates/core/tutti-types/src/timeline.rs:54`),
whose play clock is 0 there, as a live play start's is. So "play from beat
X" and "export from beat X" give equal program, knob and inputs equal
values, bit for bit, from that frame. It does not hold for:

- a live take that started elsewhere;
- a rate edit while rolling;
- a `ForkMode::Live` duplicate taken mid-play (its clock starts at 0 there);
- **a count-in**: the transport rolls during it, so live resets the play
  clock count-in frames before the export does. Remedy: export with the same
  count-in, or start the export at the count-in's start;
- **a lane or source created while rolling**: live, it crossfades in over
  `PARAM_DECLICK` and a follower's envelope starts from 0 there; the export
  has had it from the start;
- a thinned block (§5.3), or a plugin target at a different chunk size
  (§5.3);
- signal sources outside §6's conditions.

The cost is a phase step at play start for a free layer that ran while
stopped: the usual retrigger on play.

### 4.4 Event-input layers

A signal source's node sends `ParamSegment::foreign(source.0, …)` with its
**raw** value into the lane's one event input. Fan-in merges by offset, then
source order (`crates/core/tutti-graph/src/compile/mod.rs:1388-1394`), and
the address tells sources apart, so adding a source is an edge, never a new
lane shape. The lane tracks each input's current segment in a fixed array
(the port's `Ramp` arithmetic) and reads it at each point; an input that has
not sent contributes 0. There is **no new `EventKind`**: `ParamSegment` and
the 32-byte event are unchanged.

### 4.5 Publication

- **One read per block**, at the first frame, dropped before returning. The
  program holds frozen `Arc`s, not further cells, so there is no nested read
  (it fixes M7).
- **The fork's copy comes from the control side.** `RtRef` only `Deref`s
  (`crates/core/tutti-types/src/rt/publish.rs:786`) and must not leave the
  block, so `LaneControls` keeps the last published `Arc<ControlProgram>`
  beside the cell; the fork gathers that.
- **No audio-thread allocation.** Per-layer and per-input state lives in
  arrays sized `MAX_LANE_LAYERS = 16` and `MAX_LANE_INPUTS = 8`; the
  compiler refuses past either, by name.
- **A republish lands at the next block's first frame**, as a glide.

## 5. The control lane node

### 5.1 Shape

| | |
|---|---|
| Audio | none |
| Event inputs | 1: signal sources' raw values by `SourceId`; other events ignored |
| Event outputs | 1: `ParamSegment`s to `ParamAddr::Unit(param)` (`ControlLaneNode::for_param`) or `ParamAddr::Id(id)` in normalised 0..1 (`for_foreign`) |
| Event capacity | `LANE_EVENT_CAPACITY` = **256** (today 128) |
| Tail | `Tail::Unbounded` |
| Latency | none; modulation lag is musical delay (§6) |
| Cost | `Cost::Light` (§5.3's estimate), confirmed by PR 2's measurement |
| Controls | `LaneControls`: `publish`, the knob (a native `Param` cell, or `set_knob`), `set_range` for a plugin lane, `thinned()` |
| Fork | the last published program (`Arc`), knob and range values at the gather, the address; fresh state (§5.6) |

**Arrival.** With an input behind a latent path, the lane's arrival can be
non-zero, so the `debug_assert` at `lane.rs:439-443` goes. The lane
evaluates at its input's time (`Cx::transport_at`) and the compiler delays
its output to the target, so each value lands on its own frame.

### 5.2 One block

1. Load the program (one `read`), the knob and the range (the span; on a
   plugin lane, also the clamp).
2. Walk the transport pieces (`Env::segments`); a start, stop, seek, tempo
   change or loop wrap is a point; the play clock comes from
   `Cx::play_at` (§4.3).
3. **Points**, in frame order:
   - frame 0, **only** if no piece is in flight, or the program, knob or
     range changed (the glide);
   - every grid point: a frame whose `Env::play` count is a multiple of
     `CONTROL_STEP`;
   - every corner from `next_corner` (beats converted in closed form, as
     `lane.rs` does today);
   - every input event's frame;
   - every transport change;
   - for a plugin lane, every frame where a piece crosses a range bound.
4. **At each point `p`, send one linear piece** from the value at `p` to the
   left limit at the next point `p'` (`ParamSegment::resumed` at position
   0); `p'` may lie in a later block (beats extrapolated at the block's
   tempo; inputs extended from their current segment, held past its end).
   Clamp only on a plugin lane, and only at the split points of step 3.
5. **A piece in flight at frame 0 with nothing changed** is re-sent resumed
   at its position, not recomputed: the lane keeps `(from, to, start, end)`
   across blocks, so a piece is the same whatever the block boundaries. A
   segment is absolute, so the re-send changes nothing at the port.

### 5.3 Control resolution: `CONTROL_STEP = 32` frames on the play clock

The sum of differently shaped layers has no closed form a `SegmentShape`
carries, so it goes **piecewise-linear**: pieces end on a 32-frame grid of
the play clock plus every corner.

**Corners** (exact, whatever the step): an automation breakpoint; a
square's edge, a saw's wrap, a random step; a triangle's peak; a drawn
point; and **every zero crossing of a bipolar layer with a non-Linear
curve**, because `shape()` curves `|x|` (`crates/core/tutti-mod/src/shape.rs:175-189`),
which breaks the slope at each crossing. Closed-form waves give crossings
analytically; a drawn piece's crossing is solved once per piece.

**The error bound.** Between corners an offset is smooth, and linear
interpolation over a step `h` errs by at most `h² · max|f''| / 8`. For a sine
at `f` shaped by a curve `g` on `[0, 1]` with `|g'| ≤ a` and `|g''| ≤ b`,
`|f''| ≤ (a + b)(2πf)²` per unit of depth, so the bound, as a fraction of
depth, is `(a + b)(2πfh)² / 8`:

| Curve | `a + b` | 10 Hz, h = 32 at 48 kHz | 20 Hz |
|---|---|---|---|
| `Linear` | 1 | 2.2e-4 (−73 dB) | 8.8e-4 (−61 dB) |
| `PowerIn(k)`, `PowerOut(k)`, k ≥ 2 (`PowerInOut(k)`: 2k² − k) | k² | k = 2: 8.8e-4; k = 5: 5.5e-3 | k = 2: 3.5e-3 |
| any other curve with bounded `g'`, `g''` | its own `a + b` | scale the `Linear` row | |
| `PowerIn(k)`, `PowerOut(k)` with 1 < k < 2; `Logarithmic` (√t, a vertical tangent); `Circular`, `Back`, `Elastic`, `Bounce`, `Bezier` | unbounded (`g''` unbounded at an end, `Circular`'s vertical tangent, `Bounce`'s kinks) | no bound from this formula | |

For the last row the grid is the approximation, as it is today at block
edges; PR 2 pins each one's measured error at 10 Hz as a figure. **A
triangle or a saw renders exactly only with a `Linear` curve**, the only one
that keeps them linear between corners; under another curve they fall under
the table. A square and a random step are constant between corners, so they
are exact with any curve.

| Step | Rate | Error, Linear sine, 20 Hz | Grid events per 4096 frames |
|---|---|---|---|
| 16 | 3 kHz | 2.2e-4 | 256: the whole capacity |
| **32** | **1.5 kHz** | **8.8e-4 (−61 dB)** | **128, leaving 128 for corners** |
| 64 | 750 Hz | 3.5e-3 | 64 |

**Why 32:** below −60 dB of the depth up to 20 Hz, the top of the
modulation range (faster is audio-rate, out of scope); it fits the capacity;
and a plugin receives the same grid (below), so the step is one trade of
accuracy against event budget for both target kinds.

**Why the play clock.** A grid on block edges would move with block size,
so live (device blocks) and export (`max_block`) would differ. The play
clock is equal in both (§4.3), so the grid is too.

**Plugin targets.** `SegmentPoints` today sends stride points relative to
each chunk plus each chunk's last frame
(`crates/plugin/tutti-plugin/src/host/node/segment_points.rs:10-14`,
`stride_for`), at most `MAX_POINTS = 10` per parameter per chunk, replacing
the last past that (`param_automation_source.rs:365-378`); the chunk is the
device quantum live and `MaxBlock` in an export (`batcher.rs:40-45`). So
what a plugin hears depends on the chunk. PR 2 changes `SegmentPoints` to
send **only each lane piece's start and end points**, never stride points,
plus the in-flight value at a chunk's last frame (the formats hold the last
point to the chunk's end). That needs at least
`ceil(chunk / CONTROL_STEP) + 1 + corners` points per parameter per chunk
(§9, requirement 6). The chunk-edge point is the one chunk-dependent value,
so **plugin export == live holds at the same chunk size**, and PR 4's test
pins the chunk.

**CPU.** At 48 kHz the grid is 1,500 points a second per lane; each point
evaluates each layer once (the left limit at `p'` is the value at `p'`
except at a jump). 200 lanes × 2 layers is about 600k layer evaluations a
second, roughly a sine and a `shape()` each: tens of nanoseconds, so on the
order of 1–3 % of one core in all, and about a microsecond per lane per
512-frame block. That is `Light`; PR 2 measures it and declares from the
measurement, as Phase 6 (b) did.

**Overload.** Points exceed 256 only past 4096-frame blocks or past 128
corners in one block. The lane then keeps every corner, thins the grid
(every other point, then every fourth, …) and counts it
(`LaneControls::thinned()`). A thinned block can differ between live and
export; it is counted, never silent. Exports therefore render in blocks of
at most 4096 frames (§13).

### 5.4 Exact mode

When `is_pure_automation()` holds **and the target is a native port**, the
lane sends the curve's own shaped
segments, breakpoints between frames included: today's `for_param` walk, so
automation-only fidelity does not regress. Pieces with no `SegmentShape`
(`Circular`, `Back`, `Elastic`, `Bounce`, `Bezier`) and pieces a clamp cuts
are today sampled at block edges (`lane.rs:43-49`, `:181-186`), which
depends on block size; they go **on the play-clock grid** instead, as linear
pieces between grid points and breakpoints. Where the curve has no value,
the knob's constant is sent as a `Hold` piece.

**A plugin (foreign) target never uses exact mode.** A plugin receives
points, not shapes, so a shaped piece's endpoints alone would be a linear
ramp on VST3 and a step on CLAP. A foreign lane always sends the play-clock
grid, so a shaped automation piece reaches the plugin as 32-frame linear
pieces. **A CLAP parameter now steps every ≤ 32 frames** (CLAP values do not
interpolate), where today's `SAMPLE_STRIDE` steps it every 8 at short
chunks; VST3 interpolates between points, so it hears the pieces.

### 5.5 Transport

| Event | Beat layers, automation | Seconds layers | Inputs |
|---|---|---|---|
| Seek, loop wrap | a point, re-evaluated at the new beat | continue | continue |
| Tempo change | a point | continue | continue |
| Stop | hold | keep running (it fixes M6) | keep following |
| Start | a point | play clock 0, new epoch (stale anchors ignored) | per §6 |
| No usable tempo | hold | continue | continue |

When nothing moves, the lane sends one `Hold` piece a block; a native port
ignores it and a plugin's changes-only filter drops it.

### 5.6 The graph side

**An event source replaces the base** (PR 2): a port takes at most one
event source (`GraphInvalid` refuses a second, by name); its value replaces
the base from its first event, and before that the port reads the base;
its shaping must be `Identity`; connect and disconnect crossfade over
`PARAM_DECLICK`. Audio sources stay offsets until PR 5 deletes them. The
reference interpreter and the differential proptest take the same rule.

**Fork.** The lane's `ForkSource` gathers the last published program from
`LaneControls` (§4.5), the knob and range values, and the address, and
builds a fresh lane with its own cell, so a later live republish does not
reach the fork. Fresh state: no piece in flight, inputs at 0
until their (also forked) sources send. With a lane re-sending at frame 0
of every block and replace semantics, the fork needs nothing from the
audio thread, which is why `ParamTap` and the fork seeds go in PR 5 (§10).

## 6. Signal-driven sources

| Node | In | Sends | Cost |
|---|---|---|---|
| `EnvelopeFollowerNode` | audio, any width (peak or RMS over channels) | On the play-clock grid: at grid point `g`, a linear piece `env(g − step) → env(g)` over `[g, g + step)`, raw in `[0, 1]`, addressed `ParamAddr::Id(source.0)`. One-pole attack and release in `Seconds`, per frame. Capacity 256. | `Moderate` |
| `NoteEnvelopeNode` | 1 MIDI event input | An ADSR's stages as shaped segments at note-on, note-off and each stage boundary: exact, no grid. Mono, last-note priority, retrigger. | `Light` |

- **The follower runs one grid step late** (32 frames), so its output
  depends only on audio up to each grid point, never on block boundaries.
  That lag is musical delay, not latency, and is not reported to PDC.
- **One source fans out** to every lane it modulates, each with its own
  `InputLayer` `Amount`.
- **Export == live for these needs two conditions.** (1) The source's input
  is locked to the transport: a clip, a sequenced track or a MIDI clip, not
  a live input or a hardware keyboard. (2) Both nodes **reset at play
  start**: on the frame `Env::play` resets, the follower's envelope goes to
  0 (its grid is the play clock's, so it restarts too) and the note
  envelope goes idle with every stage released. A fork's nodes start in that
  state, so a live play from beat X and an export from beat X agree. The
  follower's reset is an **intended retrigger**: on a sustained sidechain it
  is audible as a dip at play start, then the attack.
- **A cycle is refused** (`CompileError::Cycle`), reported by route.
  Feedback through an event feedback edge would delay by `MaxBlock`, which
  differs live and offline, so feedback modulation is out of scope.

## 7. `tutti-mod` as a pure compiler

```rust
// tutti-mod
pub struct Declarations<'a> {
    pub sources: &'a [(SourceKey, Source)],
    pub routes: &'a [(RouteKey, RouteDecl)],
    /// Per target: range and automation. The knob is a cell (§4.2).
    pub targets: &'a [(TargetKey, TargetDecl)],
}

pub enum Source {
    Lfo { wave: Wave, clock: Clock },
    /// A constant the user moves; moving it republishes what it reaches.
    Macro { value: f32 },
    Follower { attack: Seconds, release: Seconds },
    NoteEnvelope { adsr: Adsr },
}

/// Named so it cannot collide with `routing`'s `Route`/`RouteTo` root
/// exports (`crates/core/tutti-mod/src/lib.rs:45`) while both exist (PRs 3–4).
pub struct RouteDecl {
    pub source: SourceKey,
    /// A target param, or another route's depth (`depth_by`).
    pub to: RouteTarget,
    pub depth: Depth,
    pub polarity: Polarity,
    pub curve: CurveType,
}

pub struct Compiled {
    /// One program per target with a route or automation; absent: no lane.
    pub programs: BTreeMap<TargetKey, ControlProgram>,
    /// A node per signal source, with its `SourceId`; built by the host
    /// (`EnvelopeFollowerNode::from_decl`).
    pub nodes: BTreeMap<SourceKey, SourceNode>,
    /// Event edges, source node → target lane.
    pub edges: BTreeSet<(SourceKey, TargetKey)>,
}

/// `prev` and `now` (the executor's last `(play_epoch, play)` reading) are
/// used only to date rate-edit anchors (§4.3); the function stays pure.
pub fn compile(decl: &Declarations<'_>, prev: Option<&Compiled>, now: PlayReading)
    -> Result<Compiled, CompileError>;
```

- **Pure**: no clock, no state, no `RtPublish`. Equal declarations compile
  equal; `Compiled` is `Eq`, so a host republishes only what changed.
- **Stable `LayerId`s** from `(RouteKey, depth-by RouteKey)`: identity and
  sum order survive a republish (it fixes M11).
- **Refusals, by name:** past `MAX_LANE_LAYERS` or `MAX_LANE_INPUTS`; a NaN
  range bound; a `depth_by` chain deeper than one; an automation curve that
  is not frozen.

| Module (`crates/core/tutti-mod/src`) | Fate |
|---|---|
| `shape.rs` (`shape`, `curve_apply`, `LfoShape`, `Polarity`) | kept; `fold` deleted |
| `curve.rs` | kept, moved into the floor |
| `curve_source.rs` | `hash_bipolar` and the position-derived random move into `Wave`; the rest deleted |
| `lfo.rs`, `modulator.rs` | **copied to `tutti-nodes`** in PR 3 for the new nodes; deleted from `tutti-mod` in PR 5 with `routing`, which uses them until then. `tutti-mod` then keeps no runtime. |
| new `program.rs`, `compile.rs` | §4, §7 |
| `driver.rs`, `layered.rs`, `matrix.rs`, `mod_params.rs`, `param.rs`, `router.rs`, `routing.rs`, `target.rs`, `id.rs` | deleted, with the `routing` and `std` features and `dashmap`, `arc-swap`, `atomic_float` |

## 8. `bevy-tutti`: declarations that compile to lanes

### 8.1 The API

```rust
fn setup(mut mods: Mods, filter: Single<SvfRef>, pad: Single<GainRef>, kick_bus: Single<BusRef>) {
    let wobble = mods.lfo(Lfo::sine().rate(BeatDuration(0.5)));
    mods.route(wobble, filter.cutoff(), Depth(0.3));
    let kick = mods.follower(*kick_bus);
    mods.route(kick, pad.gain(), Depth(-0.6)); // sidechain
}
```

- **`Mods` is a `SystemParam`** over `Commands`: `lfo`, `macro_`,
  `follower`, `note_envelope` spawn a `ModSource` and return its `Entity`;
  `route` spawns a `ModRoute`.
- **`filter.cutoff()` is a `ParamRef`** `(Entity, ParamAddr)`;
  `ParamRef::plugin(entity, id)` names a plugin parameter.
- **Components:** `ModSource { kind }`; `ModRoute { source, target:
  ParamRef, depth, polarity, curve, enabled }`; `ModRange { param, min, max }`
  (no `base`); `Automation { target: ParamRef, curve: Arc<dyn Curve> }`.
  `ModDelivery`, `per_block`, `per_sample` and `ModParamRange.base` go.
- **A custom source kind** implements `ModSourceKind::declare(&self) ->
  tutti_mod::Source`.

### 8.2 The reconcile step

`modulation::reconcile`, in `GraphReconcileSystems::Spawn`: collect the
declarations (keys are entities), compile, diff against the `CompiledMods`
resource, then apply:

- **A new target** gets a `ControlLaneNode` through `AudioGraphRes`. A
  native one takes its node's `Param` cell (`GraphNode::params`) as its
  knob and is wired with `connect_param`; a plugin one gets its own knob
  cell, seeded from the mirrored plugin value, and is wired into the plugin
  node's event input (`EventFeeds`).
- **A changed program** is republished: no commit.
- **A target without a program** loses its lane; its port crossfades back
  to the node's control.
- **A new signal source** gets its node and edges; a gone one loses them
  (it fixes M10).

| Edit | Cost |
|---|---|
| Depth, polarity, curve, rate, wave; an LFO route added or removed | recompile one target, republish one program; no commit, no phase reset |
| A follower route added | republish; one event edge (a commit) |
| Range (`Changed<ModRange>`) | a store: `Editor::set_param_range` (native) or `set_range` (plugin) |
| Knob (`AudioParam`, `write_param`) | one store into the node's `Param` cell; a plugin lane's `set_knob` |
| Automation curve | republish |

**One writer per knob** (it fixes M9): `write_param` loses its modulated
branch and `set_param_snapshot`; `ParamSet` loses `set_authored` and
`authored`.

**A plugin's GUI** edit reaches the host through `ParameterChangeSink`; for
a parameter with a lane, the reconcile writes it into the lane's knob. Under
automation it is overridden when the curve next moves (automation read).
With values only (no CLAP `PARAM_MOD`), the GUI shows base + modulation, so
a drag during modulation sets the knob to the dragged value.

**Recording.** While a touch or latch take is open, the reconcile
republishes with `automation: None`, so the knob (the gesture) drives the
param; on release it republishes with the finished envelope.

## 9. Plugin delivery: requirements

PR 1 is in flight separately and is reviewed against these, not against an
implementation here. Requirements 1–5 are PR 1's; 6 lands with PR 2.

1. **Every listed parameter** can be delivered in every chunk, with no
   allocation on the audio thread, for any parameter count the plugin lists
   (it fixes M4).
2. **Changes only**, compared with the last *delivered* value. A value that
   cannot go now is owed to the next chunk, never dropped (it fixes M5).
3. **Unknown addresses** (not in the list at bind) are bounded and counted,
   never silent.
4. **The delivered record is what the host last delivered to this plugin
   instance.** Nothing clears it: it persists across a reset, a re-prepare
   and a state or preset load, so a held host value is not re-sent over an
   edit made in the plugin's editor or a loaded preset; a moving host value
   re-asserts itself as it moves. A fork's delivery is seeded with the live
   node's record (read before the live state is saved), so an export
   renders what live playback plays. (Amended in PR 1, which first said the
   record is cleared on all four; clearing it made an export overwrite an
   editor edit that live playback kept. PR 4, which makes an editor edit
   write the lane's knob, may revisit this.)
5. **No format host's lists grow after activation** (VST3's queue list,
   CLAP's input-event scratch).
6. **Point capacity:** each parameter can carry at least
   `ceil(chunk / CONTROL_STEP) + 1 + C` points per chunk, reserved off the
   audio thread, where `C = LANE_EVENT_CAPACITY` (256) bounds the corners a
   lane sends in one block; `SegmentPoints` sends only lane pieces' start
   and end points plus the chunk-edge value (§5.3). Past the reservation
   (more corners than `C` in one chunk), surplus grid points are dropped
   before corners, and every drop is counted in the delivery statistics.

A plugin **ignores a ramp's duration**: a `ParamRamp` reaches it as one
point at its target value on its last frame, as today. Only lane pieces
(segments) carry their shape, as points.

## 10. What gets deleted (PR 5)

| Crate | Deleted |
|---|---|
| `tutti-graph` | `ParamFrom::Audio`, so **`ParamFrom` collapses to `EventOut`** and `connect_param(at, from: EventOut)` loses its shaping; `ParamShaping`, `ShapeLut`, `SHAPE_LUT_LEN`; `ParamSource`; `MAX_PARAM_SOURCES` and the per-source arrays; `signature`'s source hashing; `GraphInvalid::TooManyParamSources`, `UnsortedParamSources`; `DelayKey::ParamAudio`; the `Σ` of the fused step (`param.rs:11`). **`ParamTap` and its seqlock** (`param.rs:930-1060`) and **the fork seeds** (`PendingFork::seeds`, `fork.rs:207`; `Editor::param_seeds`, `seed_params`, `editor.rs:1539-1633`): a lane re-sends at frame 0 of every block, so a fork's port needs no seed. The one user outside the lane path is a host's hand-wired event source. The requirement after PR 5: **a fresh (forked) instance sends its current value on its first block**; re-sending every block is one simple way to meet it. Two classes need more: (1) **ramp-based sources**: re-sending a `ParamRamp` restarts it, so re-send the in-flight move as `ParamSegment::resumed`, or the held value as a step; (2) **sources driven by live input that cannot be replayed** (MIDI CC or OSC bridges, UI-driven event nodes): a `ForkByClone` fork does not know the held value, so either write the target's `Param` cell (the knob, which a fork reads) instead of an event source, or capture the current value in the node's `ForkSource::gather`. `ParamSet::set_authored`, `authored`. |
| `tutti-mod` | The `routing` feature and its items: `AtomicTarget`, `LayeredCurve`, `ModBus`, `ModRouter`, `ModEdge`, `ModRoutingSnapshot`, `ModRoutingTable`, `ModPreFrame`, `ErasedModulator`, `Sourced`, `SourceRate`, `Rate`, `SourceClock`, `ModParams`, `ModMatrix`, `Route`, `RouteTo`, `TargetHandle`, `ShapedCurve`, `EdgeShape`, `CurveModulator`, `BeatLfo`. Also the ungated `ModTarget`, `LayerKey`, `ModTargetId` and `fold`; the `std` feature; the `bevy` feature's implied `routing`. The README and the lib doctest are rewritten for the compiler. |
| `tutti-nodes` | `ParamModShaping`; the re-exports at `lib.rs:93-95`; `AutomationLaneNode::for_param`/`for_foreign` (now `ControlLaneNode`'s). `AutomationLaneNode::new` (the audio lane) stays. |
| `tutti-polysynth` | The `ModParams` impl's `AtomicTarget::with_mirror` (`polysynth.rs:1221`); its params are reached through `ParamSet` |
| `tutti-plugin` | `PluginAutomation`, `AutomationControls`, `AUTOMATION_EVENT_CAPACITY`, `TimedParam`, `PluginParamTarget`, `LfoCurve`, `LfoOffset`, `OffsetCurve`, `stride_for`, `MAX_POINTS`; `PluginControls::automation`, `param_target`; `impl ModParams for PluginClient` (`host/node/mod.rs:650`) |
| `bevy-tutti` | `modulation/driver.rs` (with its `ParamKey`), `target.rs` (with `ModTargetResolver`), `audio_rate.rs`, most of `source.rs`; `ModulationMatrix`, `ModTargetRegistry`, `ModBusRes`, `ModParamsHandle`, `ModRateCell`, `CollectedModSources`, `ModSourceSystems`, `AudioRateRoutes`, `ModDelivery`, `ModParamRange`; the modulation role of `CapturedControls::for_params` (`graph/capture.rs:109`, its row in the table at `:14`); `write_param`'s modulated branch, `set_param_snapshot`; `plugin_bind_params`, `PluginParamsBound`, `PluginAutomationNode`, `remove_plugin_automation`; `AudioGraphRes::set_param_mod`'s source list |
| Docs | doc 013's fork note (lines 1193-1200) and item 6 become history; CLAUDE.md's "Control-rate modulation resolves on the same set … `ModTargetRegistry::insert_target`" line is rewritten |

Tests go with what they test, each named with its disposition in the PR
body. `param_mod.rs`'s `a_fork_carries_its_event_sources_ramps` is
**ported** to "a lane-fed fork reads its value at frame 0": the property (a
fork hears the live value from its first frame) still matters, and only its
mechanism, `ParamTap`'s seed, goes. `param_mod_oracle.rs` is obsolete (its convention check moves to the
compiler's tests); `param_set_routes.rs` is ported; `bevy-tutti`'s
`mod_value_path.rs`, `mod_audio_rate.rs`, `graph_value_modulation.rs` are
ported to lanes; `tutti-polysynth`'s `AtomicTarget` route tests become
`ParamSet` writes.

## 11. Out of scope

- **Audio-rate modulation** (FM, ring): a node's own audio input; `LfoNode`
  stays an audio generator for it.
- **Per-voice modulation**: the instrument's (MPE today); CLAP per-note later.
- **CLAP `PARAM_MOD`**: values only for now (§8.2's GUI consequence).
- **An LFO's rate moved by another LFO** (needs an integrating layer).
  Depth by LFO is in scope (`depth_by`).
- **Feedback modulation**: refused (§6).
- **Slew per route**: only the ≤ one-step glide (§4.2).

## 12. The campaign

Each PR is green alone and none is merged by its author. Every new test
carries its mutation record. Bit-for-bit comparisons are serial against
parallel and live against fork, on one target; anything calling libm is
asserted analytically with a tolerance, or as a golden digest on its
recording target. Each PR logs its own CHANGELOG entry under `[Unreleased]`
in the "Was | Now" style. The `audio-automation` pin and the bevy 0.19 line
are unaffected. PR 1 is §9's requirements 1–5. **PR 2 lands after PR 1**:
it edits `segment_points.rs`, which PR 1 rewrites. A small PR between PR 1
and PR 4 makes the in-process VST2 node deliver parameter events (§13).

**Follow-up to PR 1: parameter-change memory.** PR 1 reserves every
parameter-change buffer at `params × points_for(chunk)` points, and the
bridge holds `PARAM_BUFFERS` of them: about 43 MB for 2000 parameters at a
4096-frame chunk, about 340 MB for a 16k-parameter VST3. The plugin-server
reserves for the same points again per instance: the CLAP host's input
event list (64-byte events plus an 8-byte sort key, uncapped) about 19 MB
at 2000 parameters and 157 MB at 16k, the VST3 host's input queues about
35 MB at 16k (4096 frames). Replace both with a
shared per-chunk point arena, or a small per-queue room with a counted,
shared overflow, so the memory follows the points a chunk carries rather
than the parameter count.

### PR 2: the control lane node

**Split in two (coordinator's decision).** **PR 2a, the floor: landed**
(branch `claude/control-lane-floor`): `RangeCell` in `tutti-types` with a
loom model, `ParamRanges` on it and `Editor::param_range_cell`; the play
clock (`Env::play: PlayClock`, `Env::play_at`, `Cx::play_at`,
`TransportHistory` carrying it, `Editor::play_reading`, forks at 0, see
§4.3 "As landed"); and the port rule (§5.6) in the executor, the
reference, `GraphSpec::validate` (`GraphInvalid::TooManyEventParamSources`,
`ShapedEventParamSource`), the verifier's rule 9 and the differential
proptest. The rule was checked against main before it changed: no
production path wires an event source into a param port (`bevy-tutti`'s
`set_param_mod` connects audio sources only, which stay offsets), so it
breaks no live feature. **PR 2b** is the rest of the scope below:
`ControlProgram`, the lane, the signal sources, `SegmentPoints`, the
automation constructors and the campaign tests.

**Scope.** `ControlProgram` in `tutti-mod` with `Curve` in the floor;
`RangeCell` in `tutti-types`; `Env::play`, `Env::play_epoch`,
`Cx::play_at`, the play clock in `TransportHistory` and the published
`PlayReading` in `tutti-graph`; rate anchors; `ControlLaneNode`
and `LaneControls` (§5), exact mode on the grid, thinning, the fork;
`EnvelopeFollowerNode`, `NoteEnvelopeNode` with play-start reset; the port
rule (§5.6) with interpreter, verifier and proptest; `SegmentPoints` on
piece endpoints and the point capacity (§9.6); `AutomationLaneNode::for_param`
and `for_foreign` become thin `ControlLaneNode` constructors; doc 013's
pointer gets a "landed" note.

**Properties tested.**

- Export == live, bit for bit, at the play start, on a probe copying
  `Io::param` to an output: for every layer kind (beats, seconds, drawn,
  random, `depth_by`, automation) and for a follower and a note envelope on
  transport-locked input, with live in ragged blocks and the export in
  1024-frame blocks, starting on a frame that is not a grid multiple.
- Serial == parallel, bit for bit, for 200 lanes into 25 probes against
  pools of 2, 3, 4, 8, with republishes and knob and range moves.
- No allocation with 200 lanes, serial and gated, a republish of every
  program between gated blocks.
- One LFO on two targets is phase-coherent: equal phase at every frame.
- Two lanes that read a rate-edit publish on different blocks are
  phase-identical at every frame, before, at and after `t_anchor`; a stale
  epoch's anchor is ignored after a play start.
- A range edit rescales every route's depth on the next block, with no
  republish.
- A plugin lane under shaped automation sends grid pieces, never exact
  mode.
- Phase survives a depth edit (analytic, within 1e-6 of depth); a rate edit
  has no step larger than one frame's slope.
- Automation replaces the knob, and the clamp is per frame: knob 1000 with
  automation 500 reads 500.0; a ±100 square gives 400.0 and 600.0 on their
  frames; a linear 0→10 piece in `[0, 5]` reaches 5.0 at its midpoint frame
  and holds, on a native port and (split at the crossing) on a plugin lane.
- A `Linear` triangle is exact at every frame; a sine with each bounded
  curve stays within §5.3's bound; the unbounded curves' measured errors are
  pinned; a curved bipolar layer has a point on each zero crossing.
- Exact mode (native targets) reproduces `automation_segments.rs`' frame values, and its
  fallback pieces land on grid points at any block size.
- Capacity: a 4096-frame block with 120 corners drops nothing; a
  16384-frame block thins and counts.
- An input's offset is exactly `depth × span × x` for IEEE-exact `x`; a
  follower lags exactly one step and restarts at play start.
- A plugin receives the same points at equal chunk sizes in live and export.

**Existing tests whose assertions change, with the reason:**

- `crates/core/tutti-graph/tests/param_mod.rs`, the event-source cases
  (e.g. `an_event_sources_held_value_survives_another_source_joining`,
  `a_fork_carries_its_event_sources_ramps`, the differential proptest): they
  assert base + event, now the event replaces the base, and a second event
  source is refused; regenerate the proptest's regressions.
- `crates/core/tutti-graph/tests/parallel.rs:288`: the rig puts an audio and
  an event source on `cut`; still legal, but its rendered values move to
  replace semantics (the serial == parallel assertion stands).
- `crates/core/tutti-graph/tests/rt_no_alloc.rs:712`: the same port shape;
  the no-alloc gate stands, and its connect/disconnect step now also crosses
  the base ↔ event handover.
- `crates/dsp/tutti-nodes/tests/automation_segments.rs`:
  `an_unrepresentable_piece_is_linear_between_block_edges` and
  `a_curve_without_pieces_takes_the_fallback` assert block-edge sampling,
  which moves to the play-clock grid (§5.4).
- `crates/dsp/tutti-nodes/tests/rt_no_alloc_modulation.rs:248`: its lanes
  become `ControlLaneNode`s and its elastic and sine lanes emit grid points;
  the no-alloc property stands.

**Breaking (CHANGELOG):** an event-driven port reads the stream's value,
not base + value; at most one event source per port, `Identity` shaping.
Migration: remove knob-to-zero workarounds; sum two event sources in one
lane or a node. Also: `LANE_EVENT_CAPACITY` 256, `CONTROL_STEP`, `Env::play`.

**Exit.** M8 and M11 closed; the lane measured and declared `Light`.

### PR 3: `tutti-mod` as a pure compiler

**Scope.** §7 (`RouteDecl`, `RouteTarget`), refusals, stable ids, macros,
followers, note envelopes, `depth_by`; `Lfo`, `SampleHold`, `RandomState`,
`Modulator` **copied** into `tutti-nodes` for the new nodes. `routing`, and
`tutti-mod`'s own copies it uses, stay untouched until PR 5.

**Properties.** Equal declarations (in any order) compile equal; for every
source kind, polarity and curve the offset at raw ±1 is
`shape(±1, …) × span` (it closes M1); a depth edit changes only its target's
program and keeps its ids; each refusal fires by name; a macro move changes
exactly the programs it reaches; a rate edit anchors continuously from the
previous program. Not breaking: additive.

**Exit.** Compiling anything `bevy-tutti` expresses needs no runtime; the
crate builds with and without `--features bevy`.

### PR 4: `bevy-tutti` to lanes and edges

**Scope.** §8: `Mods`, `ModRange`, `Automation`, `ParamRef`, reconcile,
knob and plugin-GUI paths, takes, plugin targets as lanes. The old driver is
unscheduled, not yet deleted.

**Properties.** The §8.1 example renders the analytic cutoff within
tolerance and the pad dips with the kick; a depth slider keeps phase at
render level (M2); export == live in an app on a native param and on the
reference CLAP plugin's gain at a pinned chunk size (M3: fails on `main`);
a despawned source leaves no node or layer (M10); range and depth edits send
no plan, a follower route one commit; a knob written after a route edit is
the knob (M9); a plugin GUI edit becomes the knob; a take suspends then
restores automation; a follower downstream of its target is refused by
route.

**Breaking:** `.per_block()`/`.per_sample()` go;
`ModParamRange::with(param, base, min, max)` becomes
`ModRange::new(param, min, max)` with the knob as base; depth is
span-scaled everywhere, so an old `PerSample` route is `max − min` times
stronger (divide its depth by the span).

**Exit.** `bevy-tutti` modulation runs on lanes only; M1–M3, M6, M7, M9,
M10, M12 closed there.

### PR 5: delete the old system

**Scope.** §10, every test disposition named, doc 013 history notes,
CLAUDE.md's modulation lines, the CHANGELOG's migrations (`ModMatrix` /
`AtomicTarget` users declare routes or call `compile`;
`connect_param(at, from, shaping)` → `connect_param(at, event_out)`;
`set_authored` → `set`; `PluginControls::automation` → a
`ControlLaneNode::for_foreign` per parameter; a hand-wired event source
sends its current value on a fork's first block, per §10's two classes).

**Properties.** The workspace, doctests, `just check-windows` and the
canonical-paths gate are green; every ported suite stays green.

**Exit.** `grep` finds none of §10; a param port holds at most one event
source; `tutti-mod` has no runtime.

## 13. Decided questions

No questions remain open. These were open in review and are now decided:

1. **A plugin whose parameter list grows after load** (CLAP `rescan`, VST3
   `restartComponent`): rely on PR 1's spare headroom and count overflow
   (requirement 3). Re-binding the node on the plugin's rescan notice is a
   follow-up.
2. **The in-process VST2 node drops parameter events**
   (`crates/plugin/tutti-plugin/src/format/vst2_in_process/node.rs:55-66`):
   fixed in a separate small PR right after PR 1, before PR 4.
3. **The largest block:** exports render in blocks of at most 4096 frames,
   through `Limits::max_block`, and `LANE_EVENT_CAPACITY` stays 256. A live
   block past 4096 frames thins and counts (§5.3).
