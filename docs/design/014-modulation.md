# Modulation is a noun of the API, not of the engine

Status: **done** (proposed 2026-10-14). Supersedes the
param-modulation design of [doc 013](013-native-graph.md) ("Item 6 landed:
compiler-owned param modulation" and the control-rate tier it sits beside).
Every PR of the [campaign](#12-the-campaign) has landed, PR 5 (the
deletion) last.

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
| `PlayReading` | `tutti-types` (re-exported as `tutti_graph::PlayReading`) | A value type: the compiler takes one (§7) and names no graph. Moved there in PR 3 |
| `Lfo`, `SampleHold`, `RandomState`, `Modulator` | **`tutti-nodes`**: copied there in PR 3 (landed; `ModulatorNode`/`LfoNode` use the copies); `tutti-mod`'s copies go in PR 5 | Stateful per-sample sources are a runtime. `routing` (`driver.rs`, `matrix.rs`, `curve_source.rs`, `routing.rs`) and `bevy-tutti` use `tutti-mod`'s until PR 5, and `tutti-mod` cannot name `tutti-nodes`, so the new nodes use the copies while the old system keeps its own. |
| `ControlLaneNode`, `LaneControls`, `EnvelopeFollowerNode`, `NoteEnvelopeNode`, `CONTROL_STEP` | `tutti-nodes` (re-exported at the root) | Graph nodes; `tutti-nodes` owns the lane |
| The play clock (`Env::play: PlayClock`, `Env::play_at`, `Cx::play_at`) | `tutti-graph` | Transport plumbing every lane reads the same (§4.3) |
| A port's event source replaces its base | `tutti-graph` | The engine's one rule |
| `RangeCell` (packed `AtomicU64`) | `tutti-types`, beside `AtomicF32` | A plugin lane's own range; `ParamRanges` reuses it |
| Plugin delivery | `tutti-plugin{,-types,-server}` | Where `ParameterChanges` is built, carried, applied |
| The reconcile (`Modulation`, `ModHost`) | `tutti-nodes` (re-exported at the root), Bevy-free: PR 4a, landed | It builds `tutti-nodes`' lanes and signal-source nodes and edits a plain `Editor`; a headless host needs it as much as an app. `tutti-nodes` already names `tutti-mod` and `tutti-graph`, and `bevy-tutti` names `tutti-nodes` |
| Declarations (`Mods`, `ModRange`, `ParamRef`) and their adapter onto `Modulation` | `bevy-tutti` (`modulation`): PR 4b | The API: entities to keys, `ModHost` over `AudioNode`s |

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
pub struct PeriodPoint { pub at: Phase, pub value: f32, pub curve: CurveType }

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

**As landed (PRs 2b and 3).** The sketch above, as the code has it:
`ControlProgram`'s fields are private behind `ControlProgram::new`, which
sorts by id and refuses past the caps, duplicate ids, a bad drawing and
(PR 3) a NaN anywhere in a layer, so a program is `Eq`. `value_at` and
`value_before` take the knob as `Option<f32>`, the span, and inputs as
`Option<f32>`s; `ProgramTime` carries the beat, the play clock as a
`SamplePosition`, its epoch and the rate. PR 3 changed two types:

- **`Layer::depth_by` is an `Option<DepthBy { clock, wave, amount }>`**:
  the scaling wave carries its route's `Amount`, and the offset is
  multiplied by `shape(raw, amount)`. `DepthBy::UNIT` (full depth,
  unipolar, linear) is the mapping PR 2b landed, `(raw + 1) / 2` in `f64`,
  computed the same way to the bit for raw in [-1, 1] (a drawn depth wave
  beyond ±1 is now clamped to ±1, as `shape` clamps); a curved bipolar amount kinks at the
  scaling wave's zero crossings, which are corners as a layer's are.
  Without the amount, a depth route's own depth, polarity and curve
  (`RouteDecl`, §7) would have meant nothing.
- **`Wave::Const(f32)`**: one raw value at every time, whatever the clock
  (a macro). It has no corners, and a layer of it (scaled by nothing or by
  another constant, `Layer::is_constant`) does not move: the lane counts
  only moving layers when it decides whether it is steady, so a target
  whose layers are all macros sends one held value, not a grid piece every
  32 frames. A one-point drawn wave on a 0 Hz clock would have kept a
  stopped lane on the grid (a seconds clock moves) and a rolling one too.
- **No response-curve LUT.** `shape()` runs at control points only (§5.3),
  so it is exact.

### 4.2 Knob and range: one cell each, by target kind

| | Native param port | Plugin parameter |
|---|---|---|
| Knob | **The node's own `Param` cell** (`ParamSet::cell`, `crates/core/tutti-graph/src/controls.rs:113`), cloned into the lane at creation and loaded once per block. `write_param` writes it; nothing else does. | A lane-owned `AtomicF32`, written by `LaneControls::set_knob` (the API's writes and the plugin's GUI edits, §8.2) |
| Range | **The port's range cell** (`Editor::set_param_range`, already a store with no commit, `editor.rs:904`). The port clamps per frame. The lane holds a read handle to the same cell (`ParamRanges` moves onto `RangeCell`; PR 2 adds a public accessor) and loads it once per block for the span only. | A lane-owned `RangeCell`, loaded once per block for the span and for the clamp, which splits pieces at crossings (§2) |
| Program | `RtPublish<ControlProgram>` (`LaneControls::publish`) | same |

**As landed (PR 2a).** `Editor::param_range_cell(at)` hands out a
read-only view of the port's cell (`ParamRangeReader`, `load` only). The
spec is the range's one writer: `set_param_range` stores into the spec and
the cell together, and every commit rewrites the cells from the spec, so a
store into the cell alone would be undone by the next commit. The cells are
kept per key while the units placed there declare the same params; a
commit that places a unit with another list gives the key fresh cells. So
**PR 4's reconcile fetches the view again after it rebinds a target**
(`Changed<AudioNode>`, or a re-declaration of the key's params), or the
lane reads a range nothing writes.

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

**As landed (PR 2b).** Two refinements keep the glide from depending on
where blocks fall. A glide that would send the piece already in flight
again (the same end value to the bit and the same next point; in exact
mode also the same start, shape and length) sends nothing, so a republish
that moves nothing changes no frame. And in exact mode a changed program
**jumps** at the block's first frame rather than gliding: an exact piece
is the curve's own segment, which has no in-flight start to glide from.

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
- **As landed (PR 3): the anchor is the source's.** `Compiled::clocks`
  holds each LFO's dated clock, and every layer and `depth_by` the LFO
  drives uses it. So a route added after a rate edit, a route moved to
  another LFO (which takes that LFO's own clock, not a continuation of the
  first's phase) and a route whose `depth_by` changes (a new `LayerId`)
  all read the anchor the LFO's other layers read, and one LFO stays
  phase-coherent across its targets.
- **As landed (PR 3): a second edit while an anchor is pending.** One anchor
  holds one rate change, so R0 → R1 → R2 cannot be expressed exactly. If
  the pending anchor `a` is still at least `2 × max_block` (8192 frames)
  ahead, the second edit reuses its frame (the publish has room to land
  before it). Nearer, the edit re-anchors at `now + margin`, past `a`, at
  the old clock's exact phase there with `prev_rate` R1. That agrees with
  the old clock from `a` on; a lane that switches to the new program at
  frame `s < a` steps once by `(R1 − R0) · (a − s)`, under 8192 frames'
  worth of the rate difference. Reusing a near `a` would instead step by
  `(R2 − R1) · lag` for any lane the publish reaches late, unbounded.

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
| Cost | `Cost::Moderate`, measured (§5.3, "As landed"); `Light` was the estimate |
| Controls | `LaneControls`: `publish`, the knob (a native `Param` cell, or `set_knob`), `set_range` for a plugin lane, `thinned()`, `truncated()`, `unranged()` |
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
5. **A piece in flight at frame 0 with nothing changed** is left alone,
   not recomputed: the lane keeps the piece and its next point across
   blocks, so a piece is the same whatever the block boundaries.
   **As landed (PR 2b):** it is not re-sent either. A re-send would be
   bit-exact at a native port only while the segment's `f32` position
   rounds alike in every block, and a plugin's delivery would see an extra
   point at each block edge; sending nothing is exact by construction. A
   seek or a play start between blocks is detected (the transport rolled
   one frame from the last block's last frame, the play clock one frame
   on, and the frame count, against this block's first frame) and is a
   point.
   **The cost of not re-sending:** a target the lane is newly wired to (a
   re-wire, or its target node replaced) hears nothing while the lane is
   steady, and holds its own base until something moves.
   `LaneControls::resync()` sets a flag the next block reads: it sends a
   point (not a glide) at frame 0. **PR 4's reconcile must call it after
   it re-wires a lane or replaces its target.**

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

*As landed (PR 2b):* `SegmentPoints` also sends a segment's value on each
frame in flight whose play count is a multiple of 32, as a grid point. A
lane's pieces run grid frame to grid frame, so it adds nothing to a lane;
it keeps a segment scheduled by hand (`ParamSegment` on the event input,
which no lane cuts into pieces) within 32 frames of its curve, as the old
stride did, on a grid that does not move with the chunk.

**CPU.** At 48 kHz the grid is 1,500 points a second per lane; each point
evaluates each layer once (the left limit at `p'` is the value at `p'`
except at a jump). 200 lanes × 2 layers is about 600k layer evaluations a
second, roughly a sine and a `shape()` each: tens of nanoseconds, so on the
order of 1–3 % of one core in all, and about a microsecond per lane per
512-frame block. That is `Light`; PR 2 measures it and declares from the
measurement, as Phase 6 (b) did.

**As landed (PR 2b): `Moderate`.** Measured on the 4-core container, 200
lanes of one moving layer each (every layer kind in turn) into 25 eight-param
probes at 512-frame blocks: about 2.3–3.3 ms a block in all, of which
the lanes' own `process` is about a third (callgrind), about 3.7 µs per
lane per block, about 7 ns a frame. The time goes to a point's transport
roll (`Cx::transport_at`, a `FrameClock` advance) and the program's
evaluation at both ends of each piece and its next corner, about 2 300
instructions a point. The ports that render the pieces per frame are most
of the rest, and are the executor's. `Moderate` (200 + 8 a frame) matches
the measurement; `Light` (100 + 1 a frame) would under-price a lane
fivefold and pack hundreds into one task. With no layers (a held value)
the whole 200-lane graph took about 0.74 ms a block.

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
`PARAM_DECLICK`. **As landed (PR 2a):** the crossfade covers the block of
the connect; a first event that lands in a later block is not declicked (a
first `ParamSegment` jumps from the base on its frame; a first `ParamRamp`
starts from the base). PR 2b's lane sends on its first block, at frame 0,
so a lane wired with its target never meets this; a hand-wired source
that waits does. Audio sources stay offsets until PR 5 deletes them. The
reference interpreter and the differential proptest take the same rule.

**Fork.** The lane's `ForkSource` gathers the last published program from
`LaneControls` (§4.5), the knob and range values, and the address, and
builds a fresh lane with its own cell, so a later live republish does not
reach the fork. Fresh state: no piece in flight, inputs at 0
until their (also forked) sources send. With a fresh lane sending at its
first frame and replace semantics, the fork needs nothing from the
audio thread, which is why `ParamTap` and the fork seeds go in PR 5 (§10).
**As landed (PR 2b):** a fresh lane has nothing in flight, so it sends on
its first frame (the lane never re-sends at a block edge, §5.2); the fork
reads its value at frame 0 that way. A live lane re-wired to another
target is not fresh, and sends nothing until something moves: its owner
calls `LaneControls::resync()` (§5.2 step 5). **For PR 4:** `fork_fresh`
gives a native lane's knob a fresh cell, a copy of the live `Param` cell's
value at the gather, so the fork's knob is not the forked node's own
`Param` cell; a reconcile that writes a fork's knob writes it through the
fork lane's `LaneControls`, not the node's `ParamSet`.

## 6. Signal-driven sources

| Node | In | Sends | Cost |
|---|---|---|---|
| `EnvelopeFollowerNode` | audio, any width (peak over channels; as landed, below) | On the play-clock grid: at grid point `g`, a linear piece `env(g − step) → env(g)` over `[g, g + step)`, raw in `[0, 1]`, addressed `ParamAddr::Id(source.0)`. One-pole attack and release in `Seconds`, per frame. Capacity 256. | `Moderate` |
| `NoteEnvelopeNode` | 1 MIDI event input | An ADSR's stages as linear segments at note-on, note-off and each stage boundary: exact, no grid. Mono, last-note priority, retrigger. | `Light` |

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
- **A cycle is refused**, reported by route.
  Feedback through an event feedback edge would delay by `MaxBlock`, which
  differs live and offline, so feedback modulation is out of scope.
  **As landed (PR 3):** the compiler sees no audio graph, so it cannot see
  a follower downstream of its own target and has no `Cycle` refusal. PR 4
  refuses that case by route when it adds the edges, and the graph refuses
  any cycle at commit regardless.
  **As landed (PR 4a):** `Modulation::apply` refuses it
  (`ReconcileError::Cycle { route }`, the first such route in key order)
  before it changes anything: the reconciler wires every source's input
  itself, from `ModHost::feed`, so both ends are known there. The walk is
  the spec's direct audio, event and param edges less the reconciler's own
  nodes, plus each source's feed and each wanted edge as source → target
  node.
- **As landed (PR 3):** the compiler gives each signal source a
  `SourceNode { id, kind }`: a pure description (`SignalKind::Follower
  { attack, release }` or `SignalKind::NoteEnvelope { adsr }`) carrying
  everything construction needs and nothing runtime. The nodes and their
  `from_decl` constructors are PR 3b (§12).
- **As landed (PR 3b)** in `tutti-nodes` (`src/signal/`), with these
  choices the table left open:
  - **Peak over channels**, not RMS: a frame's level is `max |x_c|`,
    clamped to `[0, 1]` (a NaN sample reads 0). A frame's RMS over
    channels is a width-dependent average, not a level in time: a
    hard-panned kick reads `1/√2` of itself in stereo and all of itself in
    mono. The peak reads the same at any width, as a compressor's linked
    sidechain does. Time smoothing is the one-pole's job.
  - **The one-pole** is `env ← c · env + (1 − c) · level`, with
    `c = exp(−1 / (τ · rate))` in `f64` through `libm`, and the envelope
    is kept in `f64` too (in `f32`, `c` rounds to 1 past about 640 s at
    48 kHz and the envelope never decays), `τ` the attack
    while the level is above the envelope and the release otherwise; a `τ`
    of zero follows exactly. Below `1e-7` the envelope is 0, so a release
    reaches silence in finite time, and a silent follower sends nothing (a
    piece from 0 to 0 after one that ended at 0 is skipped; a lane reads an
    input that has not sent as 0). `env(g)` counts the audio before `g`.
  - **The note envelope's stages are linear segments** (`SegmentShape::
    Linear`, in frames from `Seconds::to_samples`), so a lane carries them
    exactly: the peak lands on frame `on + attack`, the sustain on
    `on + attack + decay`. A note-on (re)starts the attack from the current
    level; a note-off of the last held note releases from the current level
    over the full release. Same-frame segments coalesce, and a segment the
    event buffer refuses is re-sent, resumed, on the next block's first
    frame. MIDI 1.0 and 2.0 note messages are read; a note is its group,
    channel and key.
  - **Play start** is read as an epoch change of `Cx::play_at` (the input's
    time, so a latent path's start lands on the frame its input was
    rendered at), frame by frame only in a block whose play clock jumps,
    and each frame once. A fresh node reads no start on its very first
    frame (it is idle already), but does on any later frame of its first
    block.
  - **`ForkByClone`**: neither node holds a cell, a handle or anything a
    clone would share, and neither has controls, so a `ForkSource` would
    gather nothing; the fork (the node as inserted, cloned, reset) starts
    idle, as a live play start leaves it.
  - **`from_decl`** is `EnvelopeFollowerNode::from_decl(&SourceNode,
    ChannelLayout)` (the width is the audio graph's, which the compiler
    does not see) and `NoteEnvelopeNode::from_decl(&SourceNode)`, each
    `None` for the other kind. They live in `tutti-nodes` because
    `tutti-nodes` already names `tutti-mod` (§3), never the reverse.
  - **Capacity**: 256 events each. A follower sends one piece per grid
    point, so a block past 8192 frames drops pieces; each later piece
    carries both its ends, so the next one sent is exact again.
  - **A held note envelope reads 0 on a newly wired lane.** The note
    envelope sends only on a note, a stage boundary or a play start, and a
    lane reads an input that has not sent as 0. So a lane whose new
    program starts reading a note envelope already in its sustain (a route
    added while a key is held) reads 0 until the next note or boundary;
    with §7's sweep, so does a lane whose program drops the source and
    takes it back. A follower recovers within a grid step (it sends a
    piece on every grid point while it is not silent). The cure is a
    source-side resync beside `LaneControls::resync`: on request the source
    re-sends where it stands, as a resumed segment, as it already does
    after a refused segment. Deferred to PR 4, where the host knows which
    routes it adds (§12).
    **As landed (PR 4a):** `EventKind::Resync`, a generic event: a request
    to an event source to re-send where it stands. The note envelope
    answers it on its input (a resumed segment, or 0 when idle);
    `Modulation::commit` schedules one (`Editor::schedule`, `At::NextBlock`)
    into each envelope a lane's program newly reads (a diff of the sources
    the published and the new program read). A scheduled command waits for
    the commit sent before it, so the re-send meets the edges that commit
    placed; nothing crosses outside the ports, so the envelope stays
    `ForkByClone`. A follower has no event input and needs none. A source
    rebuilt at the same id (a declaration edit) is covered too: a follower
    is built `replacing()` (it sends its first grid piece even while
    silent), and a note envelope is owed a resync (it sends the 0 it stands
    at).

## 7. `tutti-mod` as a pure compiler

```rust
// tutti-mod
pub struct Declarations<'a> {
    pub sources: &'a [(SourceKey, Source)],
    pub routes: &'a [(RouteKey, RouteDecl)],
    /// Per target: range and automation. The knob is a cell (§4.2).
    pub targets: &'a [(TargetKey, TargetDecl)],
}

/// Keys are the host's (an entity's bits, a hash): plain values, ordered.
pub struct SourceKey(pub u64);
pub struct RouteKey(pub u64);
pub struct TargetKey { pub node: u64, pub param: ParamAddr } // ParamAddr: Ord

pub enum Source {
    /// The clock carries no anchor; anchors are the compiler's.
    Lfo { wave: Wave, clock: Clock },
    /// A constant the user moves; moving it republishes what it reaches.
    Macro { value: f32 },
    Follower { attack: Seconds, release: Seconds },
    NoteEnvelope { adsr: Adsr },
}

/// Existing units: sustain is a level, `Amplitude` (polysynth's
/// `EnvelopeConfig` precedent).
pub struct Adsr { pub attack: Seconds, pub decay: Seconds,
                  pub sustain: Amplitude, pub release: Seconds }

pub enum RouteTarget { Param(TargetKey), Depth(RouteKey) }

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

/// The range is `min..=max`; the knob is a cell (§4.2). `automation` must
/// be frozen (`Curve::frozen`), so the program can compare it by identity.
pub struct TargetDecl { pub min: f32, pub max: f32,
                        pub automation: Option<Arc<dyn Curve>> }

/// What PR 3b builds a signal source's node from: no runtime state.
pub struct SourceNode { pub id: SourceId, pub kind: SignalKind }
pub enum SignalKind { Follower { attack: Seconds, release: Seconds },
                      NoteEnvelope { adsr: Adsr } }

pub struct Compiled {
    /// One program per target with a route or automation; absent: no lane.
    pub programs: BTreeMap<TargetKey, ControlProgram>,
    /// A node per signal source, with its `SourceId`; built by the host
    /// (`EnvelopeFollowerNode::from_decl`).
    pub nodes: BTreeMap<SourceKey, SourceNode>,
    /// Event edges, source node → target lane.
    pub edges: BTreeSet<(SourceKey, TargetKey)>,
    /// The next `SourceId` to give a new source (PR 3b): ids are never
    /// given twice.
    pub next_source: SourceId,
    /// Each LFO's dated clock (its rate anchor, §4.3): the anchor is the
    /// source's, not a layer's.
    pub clocks: BTreeMap<SourceKey, Clock>,
}

/// `prev`, `now` (the executor's next play reading) and `rate` are read
/// only to keep signal sources' ids and to date rate-edit anchors (§4.3);
/// the function stays pure.
pub fn compile(decl: &Declarations<'_>, prev: Option<&Compiled>,
               now: PlayReading, rate: SampleRate)
    -> Result<Compiled, CompileError>;
```

**As landed (PR 3)** in `compile.rs`, with these choices the sketch left
open:

- `compile` takes `rate: SampleRate`: a new anchor's phase is the old
  clock's cycles at the anchor frame, and a `Seconds` clock needs the rate
  to count them.
- **A macro** compiles to a `Wave::Const(value)` layer on a fixed beat
  clock. A one-point drawn wave on a 0 Hz clock would have kept the lane on
  its grid; a constant layer has no corners and needs no clock, so a lane
  of constants is steady (§4.1).
- **A rate edit** (a `Seconds` clock whose `hz` changed) anchors at
  `now.after(RATE_ANCHOR_MARGIN)` (12 288 frames: two of the largest block
  plus publish slack), with `phase` the old clock's cycles there and
  `prev_rate` the old rate. A pending anchor at least 8192 frames ahead is
  kept and shifted; a nearer one is replaced (§4.3 states the step that
  remains). A live anchor (this epoch) is carried while the rate holds,
  and dropped at the next play start. Anchors are dated per LFO, from
  `prev.clocks`, never per layer (§4.3).
- **Signal sources' ids** come from `prev`: a source keeps its `SourceId`
  across compiles. *Changed in PR 3b:* a new one takes the next id never
  given (`Compiled::next_source`, carried from `prev`), in key order, so a
  freed id is not given again (after 2^32 ids the counter wraps, skipping
  ids still held).
- **The stale-slot hazard (PR 3, closed in PR 3b).** A lane kept a removed
  source's last segment in its input slot (`take_input` overwrote a slot
  whose source was unread only when another source needed it), so a later
  compile giving the freed id to a new source routed to that lane made the
  lane read the stale segment until the new node first sent: under one
  grid step for a follower, until the first note for a note envelope.
  Closed twice over: **the lane** frees every slot whose source its
  program no longer reads when it adopts a new program generation, and
  **the compiler** never gives an id twice, which covers the case the lane
  cannot see (a program replaced twice before the lane's next block).
  The sweep has one cost: a lane re-reading a note envelope that is held
  reads 0 until its next segment (§6, "A held note envelope reads 0"),
  which PR 4's source-side resync closes.
  Tests: `a_reused_source_id_starts_from_nothing` (`tutti-nodes`) and
  `a_signal_source_keeps_its_id` (`tutti-mod`).
- **`layer_id(route, depth_by)`** is a fixed 64-bit mix of the keys; two
  layers of one target that collide are refused.

- **Pure**: no clock, no state, no `RtPublish`. Equal declarations compile
  equal; `Compiled` is `Eq`, so a host republishes only what changed.
- **Stable `LayerId`s** from `(RouteKey, depth-by RouteKey)`: identity and
  sum order survive a republish (it fixes M11).
- **Refusals, by name** (`CompileError`): past `MAX_LANE_LAYERS`
  (`TooManyLayers`) or `MAX_LANE_INPUTS` (`TooManyInputs`); a NaN range
  bound (`NanRange`); a `depth_by` chain deeper than one (`DepthByChain`);
  an automation curve that is not frozen (`LiveCurve`). As landed, also: a
  duplicate key (`DuplicateSource`, `DuplicateRoute`, `DuplicateTarget`); a
  route naming an undeclared source, route or target (`UnknownSource`,
  `UnknownRoute`, `UndeclaredTarget`); a source or route outside its domain
  (`BadSource`, `BadRoute`); an LFO declared with an anchor
  (`AnchorDeclared`); a depth route from or to a signal source
  (`DepthBySignal`); two depth routes to one route (`TwoDepthBy`); a
  `LayerId` collision (`LayerIdCollision`). The cycle refusal is PR 4's
  (§6).

| Module (`crates/core/tutti-mod/src`) | Fate |
|---|---|
| `shape.rs` (`shape`, `curve_apply`, `LfoShape`, `Polarity`) | kept; `fold` deleted |
| `curve.rs` | kept, moved into the floor (PR 2b, landed) |
| `curve_source.rs` | `hash_bipolar` and the position-derived random moved into `Wave` (`program.rs`, PR 2b, landed); the rest deleted in PR 5 |
| `lfo.rs`, `modulator.rs` | **copied to `tutti-nodes`** in PR 3 (landed: `tutti-nodes/src/modulator.rs`; `ModulatorNode` and `LfoNode` use the copies) for the new nodes; deleted from `tutti-mod` in PR 5 with `routing`, which uses them until then. `tutti-mod` then keeps no runtime. |
| new `program.rs`, `compile.rs` | §4, §7: `program.rs` landed in PR 2b, `compile.rs` in PR 3 |
| `driver.rs`, `layered.rs`, `matrix.rs`, `mod_params.rs`, `param.rs`, `router.rs`, `routing.rs`, `target.rs`, `id.rs` | deleted, with the `routing` and `std` features and `dashmap`, `arc-swap`, `atomic_float` |

## 8. `bevy-tutti`: declarations that compile to lanes

### 8.1 The API

```rust
fn setup(mut mods: Mods, scene: Res<Scene>) {
    let cutoff = ParamRef::unit(scene.filter, UnitParam::Cutoff);
    let gain = ParamRef::unit(scene.pad, UnitParam::Volume);
    mods.range(cutoff, 0.0, 1_000.0);
    mods.range(gain, 0.0, 1.0);
    let wobble = mods.lfo(Lfo::sine().rate(BeatDuration(0.5)));
    mods.route(wobble, cutoff, Depth(0.3));
    let kick = mods.follower(scene.kick_bus);
    mods.route(kick, gain, Depth(-0.6)); // sidechain
}
```

(As landed in PR 4b: there are no typed node refs such as
`filter.cutoff()`; a `ParamRef` is built from the entity and the param.
A route needs a `ModRange` on its target. The example is
`tests/mod_lanes.rs`'s `the_8_1_example_renders_its_analytic_values`.)

- **`Mods` is a `SystemParam`** over `Commands`: `lfo`, `macro_`,
  `follower`, `note_envelope` and `source` spawn a `ModSource` and return
  its `Entity`; `route`, `range` and `automate` spawn a `ModRoute`, a
  `ModRange` and an `Automation`.
- **A `ParamRef` is `{ entity, param: ParamAddr }`**:
  `ParamRef::unit(entity, UnitParam)` names a native param,
  `ParamRef::plugin(entity, id)` a plugin parameter.
- **Components:** `ModSource { kind }`; `ModRoute { source, target:
  ParamRef, depth, polarity, curve, enabled }`; `ModRange { param, min, max }`
  (no `base`); `Automation { target: ParamRef, curve: Arc<dyn Curve> }`.
  `ModDelivery`, `per_block`, `per_sample` and `ModParamRange.base` go.
- **A custom source kind** implements `ModSourceKind::declare(&self) ->
  tutti_mod::Source`.

### 8.2 The reconcile step

**As landed (PR 4a): the reconcile is Bevy-free**, `tutti_nodes::Modulation`
(`src/reconcile.rs`). `apply(&Declarations, &mut Editor, &dyn ModHost)`
compiles against the last `Compiled` it holds, diffs per target and
applies the rules below to a plain `Editor`, reporting whether a commit is
needed (`Applied::commit`); `commit` commits and sends the resyncs (§6).
The one thing it cannot derive is the host's seam, `ModHost`: which graph
node a `TargetKey` names (`Target::Native { node, knob }`, the knob the
node's `Param` cell, or `Target::Foreign { node, port, knob }`, a plugin
node's event input) and what feeds a signal source (`Feed::Audio(node)`,
`Feed::Events(out)`). A native target resolves once the last plan
committed (`Editor::base`) has a unit at its node declaring the params
the node declares now. A node new since the last commit has no range cell
yet; a node replaced by a unit with another param list has the old unit's,
which the commit replaces with fresh cells (§4.2). Either way the target
is reported unresolved and loses its lane, and the host re-applies after
the commit while `Applied::unresolved_targets` is not empty; the lane
rebuilt then fetches the range view the commit handed out. A replace with
the same list keeps its cells, so its lane is rebuilt at once, with no
frame on the bare knob. A rebuilt lane is fresh and sends at its first
frame (§5.2), so no `LaneControls::resync` is needed. (A rebind applied
between insert and commit used to bind the old cell for good: every later
range edit was lost. Review of PR 4a.) Two targets the host resolves to
one param port are refused (`ReconcileError::SharedPort`); a port's event
source from one of the reconciler's own lanes is its own, so a target
moved to another key on the same port in one `apply` takes the port. A follower with no feed, or whose feed node is
gone, is unresolved; a note envelope whose MIDI node is gone keeps its
node, unfed. A native port another event source already drives is
refused (`ReconcileError::PortTaken`): the graph allows one per port.
Built nodes get keys from `NodeKey::fresh`, past any the editor holds,
and each records its generation: a key a host later takes (a replace or a
removal) is followed, the wiring around it undone and the node rebuilt at
a fresh key.
`bevy-tutti`'s system (PR 4b) collects the declarations, implements
`ModHost` over its entities and calls it.

**As landed (PR 4b).** `modulation::reconcile`, in
`ModulationSystems::Apply` (after `GraphReconcileSystems::Spawn`, before
`Params`), collects the declarations, keyed by entity
(`Entity::to_bits`: `SourceKey`, `RouteKey`, `TargetKey::node`), and
calls `Modulation::apply` on the graph's editor
(`AudioGraphRes::modulate`), whose `ModHost` maps an entity to its
`AudioNode`. The `ModulationState` resource holds the `Modulation`, the
standing refusal and what is unresolved. A staged commit goes with the
frame's `commit_graph`; `after_commit` (`ModulationSystems::AfterCommit`)
re-applies and commits again while a target is unresolved (a node new or
rebound this frame resolves only from a committed plan, so the lane plays
from the node's first frame), commits a host's own staged insert for the
same reason, and sends the resyncs owed. Every `ReconcileError` is
surfaced: logged, sent once as a `ModulationRefused` message (with the
route or target entity it names) and held in
`ModulationState::refusal`. A target takes one `ModRange` and one
`Automation`: a second of either on the same `ParamRef` is refused as
`DuplicateTarget`, never merged (which one won would be query order). A
target still unresolved after 240 applies is logged once. A follower's feed is `Feed::Audio` (its input
entity's node); a note envelope's is `Feed::Events` (its MIDI entity's
`EventOut`). A plugin target is `Target::Foreign` on the plugin node's
event input port 0, its knob seeded from `PluginParamValues` (the
plugin's values, read off the frame thread at load; the target waits
unresolved until they land). The old driver is in
`modulation::legacy`, unscheduled (`LegacyModulationPlugin`), and
`plugin_bind_params` is no longer scheduled.

The rules, as designed:

- **A new target** gets a `ControlLaneNode` through `AudioGraphRes`. A
  native one takes its node's `Param` cell (`GraphNode::params`) as its
  knob and is wired with `connect_param`; a plugin one gets its own knob
  cell, seeded from the mirrored plugin value, and is wired into the plugin
  node's event input (port 0).
- **A changed program** is republished: no commit.
- **A target without a program** loses its lane; its port crossfades back
  to the node's control.
- **A new signal source** gets its node and edges; a gone one loses them
  (it fixes M10).
- **Diff per target, not the whole `Compiled`** (as landed in PR 3): a rate
  edit of an LFO no route reads changes `Compiled::clocks` and no program,
  so the reconcile compares `programs`, `nodes` and `edges` entry by entry
  and republishes only the programs that differ.

| Edit | Cost |
|---|---|
| Depth, polarity, curve, rate, wave; an LFO route added or removed | recompile one target, republish one program; no commit, no phase reset |
| A follower route added | republish; one event edge (a commit) |
| Range (`Changed<ModRange>`) | a store: `Editor::set_param_range` (native) or `set_range` (plugin) |
| Knob (`AudioParam`, `write_param`) | one store into the node's `Param` cell; a plugin lane's `set_knob` |
| Automation curve | republish |

**One writer per knob** (it fixes M9): `write_param` loses its modulated
branch and `set_param_snapshot`; `ParamSet` loses `set_authored`. (As
landed in PR 4b, `ParamSet::authored` stayed, as the value a fork of the
node started from, which `set` wrote with the live cell. PR 5 deleted it:
the cell is that value, a fork's template shares it
(`ParamFork::fork_node` is the template's `fork_fresh`), and `get` reads
it. One cell per param, one writer.)

**A plugin's GUI** edit reaches the host through `ParameterChangeSink`; for
a parameter with a lane, it is written into the lane's knob. (As landed:
`PluginParamValues::read` installs the handle's `on_parameter_changed`
callback, which queues each report; `plugin_param_edits`, in
`GraphReconcileSystems::Spawn`, writes them into the mirror and the lane's
knob, `ModulationState::set_knob`. Under automation the curve decides the
base, so the knob shows again when the automation or its take goes.)

**As landed (PR 4b): VST2 only.** Only VST2 reports a value the plugin
moves itself (`audioMasterAutomate`, in process and through the
plugin-server's `ParameterChanged`). CLAP, VST3 and AU emit no
`ParameterChanged` through the server (`tutti-plugin-server`'s
`Plugin::poll_async_events`, `src/plugin.rs:290-355`, sends only latency,
tail and list or value invalidations for them), so a GUI edit on one of
them moves the plugin's own value, the lane's next delivery replaces it,
and the knob does not move. That gap is open: closing it needs a
per-value report from each of those formats through the server.

**An echo is not an edit.** A plugin that reports the value the host just
set, from inside `setParameter`, would make the lane's modulated value
(`knob + Σ offsets`) the knob, and the next delivery would add the offsets
again, ratcheting the knob to the clamp. The VST2 host drops a report made
on the thread inside its own `setParameter` call (`tutti-vst2-host`'s
`host_write`, a thread-local mark), so an echo never reaches the
parameter callback; an edit from the plugin's editor thread during the
call is still reported. A format that gains per-value reports needs the
same filter.

With values only (no CLAP `PARAM_MOD`), the GUI shows base + modulation, so
a drag during modulation sets the knob to the dragged value.

**Recording.** While a touch or latch take is open, the reconcile
republishes with `automation: None`, so the knob (the gesture) drives the
param; on release it republishes with the finished envelope. (As landed:
an `AutomationTake` component on the `Automation`'s entity, holding a
`Recorder`; `close_takes`, before the reconcile, swaps a finished take's
envelope into the `Automation` and removes the take.)

## 9. Plugin delivery: requirements

PR 1 is in flight separately and is reviewed against these, not against an
implementation here. Requirements 1–5 are PR 1's; 6 lands with PR 2.

1. **Every listed parameter** can be delivered in every chunk, with no
   allocation on the audio thread, for any parameter count the plugin lists
   (it fixes M4). Each has a queue reserved off the audio thread; its points
   are stored as the point-storage note below says.
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
6. **Point capacity:** each parameter's queue has a logical room of
   `ceil(chunk / CONTROL_STEP) + 1 + C` points per chunk, where
   `C = LANE_EVENT_CAPACITY` (256) bounds the corners a lane sends in one
   block; `SegmentPoints` sends only lane pieces' start and end points plus
   the chunk-edge value (§5.3). The storage for those points is reserved
   off the audio thread as one shared pool per parameter set (the
   point-storage note below), so the memory follows the points a chunk
   carries, not the parameter count. Past a queue's room (more corners than
   `C` in one chunk), or past the pool, surplus grid points are dropped
   before corners, and every drop is counted in the delivery statistics.
   **When the pool is spent.** The room is logical and per queue; the
   storage behind it is shared: every queue owns a floor of
   `FLOOR_POINTS` (2) points (and one kept for the bridge's carried value),
   and a queue past its floor borrows one of the pool's `POOLED_QUEUES`
   (256) blocks (`ParameterChanges::make_room`). With every block lent, a
   queue past its floor keeps its floor, and the same rule applies within
   it: a grid point offered to it is dropped, a corner evicts its earliest
   grid point (both counted in `ParamDeliveryStats::grid_points_dropped`),
   and once it holds only corners a corner takes the latest point's place,
   the one full-queue policy (`PointBuf::replace_latest`), counted in
   `points_merged`. A parameter's last point in a chunk is a corner (the
   chunk's last frame, an arrival, a ramp, or a segment starting on the
   last frame, which `SegmentPoints` sends as a corner even on a grid
   frame), so every parameter still ends the chunk on its latest value.
   The plugin node keeps a grid mark a point (`GridBits`), laid out as the
   points are: a word for each queue's floor and its span (16 bytes a
   queue) and a block's worth for each pool block, 2064 × 16 + 256 × 7 × 8
   bytes = 47.4 KB at 2064 queues and a 4096-frame chunk. A buffer of
   another room never reaches it: the bridge's pool drops one whose queues
   or point room differ from its reservation (`PayloadPool::return_params`).
   **As landed (PR 2b):** a point is a grid point by its frame alone (its
   play count a multiple of 32, `param_input::take`), so a lane corner
   that lands on a grid frame is dropped as a grid point. It loses nothing
   a grid point would not: the lane's piece starting there carries the
   same value. A ramp is always a corner.

**Point storage (the arena follow-up, landed).** A queue's logical room
(`ParameterQueue::chunk_room`, requirement 6) is decoupled from its storage,
and no storage on the parameter path can grow. A queue's points are a
`PointBuf`: a boxed slice of points and a length, its capacity fixed when it
is built (off the audio thread) and nothing on it able to grow (`try_push`
hands a point back when it is full). `ParameterChanges::reserved(queues,
room)` gives every queue a floor of `FLOOR_POINTS + 1` (3) points of its
own, and the set a shared pool (`PointPool`) of `POOLED_QUEUES` (256) blocks
of `room + 1` each (the one more is the carried value's, below). A queue
that spends its floor takes a block (`ParameterChanges::make_room`: its
points move into the block, and its own storage is kept aside); `clear`
gives the blocks back for the next chunk, each from the queue that took it
(the pool records the holders by index, with a flag a holder). So the set
holds `queues × 3 + 256 × (room + 1)` points
(`ParameterChanges::point_budget`), not `queues × room`, and still carries
any parameter's points as one contiguous, time-ordered queue. That is why
this layout was chosen over a flat arena with per-queue ranges: every
consumer reads the same per-queue slice it read before, so CLAP's
flattening and time sort, VST3's per-parameter `IParamValueQueue`s and the
IPC wire need no change (a `PointBuf` serializes as the same sequence a
`Vec` did; the pool is not serialized; the bytes are pinned by
`a_block_encodes_as_before_smallvec_was_removed`). There is one layout for
every set. A set is pooled only where that at least halves its storage
(`point_layout`); otherwise every queue keeps `room + 1`, since just past
the break-even the pool would
save little while letting only 256 queues reach their room. The VST3
host's input list lends from the same `PointPool` type, so there is one
pool implementation.

The budget: 256 queues moving at full density in one chunk (a point every
`CONTROL_STEP` frames, plus the corners) is past any ordinary automation
pass, a mixer's worth of lanes all drawn at once, while every other
parameter still takes a value and a ramp end (its floor, with the carried
value's point). The cost is `256 × (room + 1) × 16` bytes a set, a fixed
figure, beside a few headers and the floor a parameter. The pool is not configurable: the plugin-server
reserves its own lists by the same layout for the room it is activated
with, and a host-chosen budget would have to cross the wire to reach it.
If one is ever needed, the route is a field on `LoadPlugin`, which the
server already reads before it activates the plugin.

**Nothing grows, and one policy for a full queue.** Every way of building a
set or a queue states its capacity: `ParameterChanges::reserved` and
`reserved_bridged` (below), and `ParameterQueue::with_capacity`. `ParameterChanges::new()` is the empty set
with room for nothing, what a block with no changes carries. Two builders
size from data, both off the audio thread: wire decode allocates a queue
exactly the decoded length (`ParameterQueue::from_points`), as the
per-block decode always did, and `ParameterChanges::add_queue` pushes a
prebuilt queue. Past a queue's room with the pool spent, every site applies
the one policy, `PointBuf::push_or_replace_last`: the point replaces the
queue's latest-in-time point if it is no earlier, so the parameter ends the
chunk on its latest value whatever order its points arrived in, and the
site counts it with its own counter. `add_change` counts in
`points_overflowed` (an address past every queue is dropped and counted
there too); the IPC node's delivery in `points_merged`; the CLAP output
grouping in `output_param_points_dropped`; the VST3 host's input queues in
`Vst3Active::param_points_dropped` (with the `IMidiMapping` route's merge
of host automation and mapped controllers), and its output queues, where
the plugin's `addPoint` past a queue's room returns `kResultOk` with the
index of the point it replaced, or `kResultFalse` when it was earlier than
the queue's latest and dropped, in `output_param_points_dropped`.

**The carried value.** The IPC bridge prepends a skipped block's carried
value to a queue (`ParameterChanges::prepend_carried`, on the bridge
thread). It takes a real slot: the host's sets (the node's delivery and the
bridge's buffers) are built `reserved_bridged`, the one layout with one
point of every queue and every block kept back from the producers, so they
stop at `room` and the carried value always fits. With no slot (a set built
otherwise, full) it is dropped and counted: it is older than every point
the queue holds. With no queue for the parameter and no spare queue to
open, it is dropped and counted as well, and there it is the parameter's
latest value; that is unreachable from the bridge, because every buffer
holds the delivery's slot count (the parameters + 64) and every id it
carries is one of those slots. (Before, the prepend grew a floor queue past its two
points on the bridge thread.) A chunk can therefore arrive with
`point_budget` points, and everything that receives one reserves that: the
CLAP input event list's parameter room, the VST3 input list (the same
layout, `ParameterChangesImpl::with_queues` with the chunk's
`point_layout`), and the VST3 host's `IMidiMapping` route scratch
(`reserved`), which copies the chunk queue by queue (a queue opened for
each, linear in the chunk), so a floor queue carrying the value fits its
own floor and the blocks go to the queues at full room, wherever they are
in the list. The VST3 input list and route are sized for the chunk's
queues, the parameters and `ParameterChanges::SPARE_QUEUES` (64).
Within it, every point the host's chunk carries reaches the plugin. Past
it, which only a set nothing bounded reaches, the CLAP list keeps a
parameter's latest value in place of its previous event once the room is
spent, but a parameter with no event in the list yet when the room runs out
loses its points; the VST3 list keeps each queue's latest value. Both are
counted (`param_points_dropped`, `Vst3Active::param_points_dropped`). These
counters are read on the format host's instance; the plugin-server does not
report them. The server's own copy of the plugin's parameter output
(`ProcessOutput::reserve_params`) is reserved at load, `reserved` for the
plugin's parameters at the activated block's `chunk_room`, and again, at
that room, when a plugin says its parameter list changed (a VST3
`kParamTitlesChanged` or reload, an AU parameter-list change) and it grew.
That re-reserve runs on the server's audio-phase thread between blocks,
sized by the format's count query (`PluginInstance::parameter_count`), not
the descriptor list. It helps a VST3 reload (the new instance's lists are
sized at its activation) and an AU change; a VST3 `kParamTitlesChanged`
without a reload keeps the instance's activation-time output and emitted
queue counts, so the instance drops (and counts) points past them first.

Per instance, at requirement 6's `chunk_room` (273 points at a 512-frame
chunk, 385 at 4096; PR 2b raised it from 20 and 132). The figures are point storage and queue headers (a queue 32
bytes, a point 16, a spare slot 32): a set is
`queues × 112 + 256 × 16 × (room + 1)` (112 = the queue, its spare slot and
its 3-point floor). The IPC node holds `PARAM_BUFFERS + 1` sets with
`queues` the parameters plus 64 spare queues; the CLAP input list's
parameter room is at 72 bytes an event; the CLAP output list's parameter
room at 64 bytes and its set; the VST3 input list's points at 16 bytes
(the parameters plus 64 queues); the VST3 `IMidiMapping` route scratch is
a set for the parameters plus 64, held only while a mapping is installed;
the plugin-server's output a set for the parameters. The non-parameter
event scratch (256 events) and the pool's bookkeeping (about 14 KB a set,
and a byte a queue) are left out.

| Site | 2000 @ 512 | 2000 @ 4096 | 16k @ 512 | 16k @ 4096 |
|---|---|---|---|---|
| IPC node: `PARAM_BUFFERS` + 1 sets | 12.18 MB | 16.31 MB | 26.68 MB | 30.81 MB |
| CLAP input event list | 5.48 MB | 7.55 MB | 8.59 MB | 10.65 MB |
| CLAP output list and queues | 6.22 MB | 8.51 MB | 10.59 MB | 12.89 MB |
| VST3 input points | 1.22 MB | 1.68 MB | 1.91 MB | 2.37 MB |
| VST3 `IMidiMapping` route (while mapped) | 1.35 MB | 1.81 MB | 2.96 MB | 3.42 MB |
| plugin-server output (`ProcessOutput`) | 1.35 MB | 1.81 MB | 2.96 MB | 3.42 MB |

Only the pool term (`256 × 16 × (room + 1)` bytes a set: 1.12 MB at 512
frames, 1.58 MB at 4096) grew with the room; the floor and headers did not.
At the room before PR 2b (20 and 132) the rows were: the IPC node 2.85,
6.98, 17.35 and 21.48 MB; the CLAP input list 0.82, 2.88, 3.93 and
5.99 MB; the CLAP output 1.04, 3.33, 5.41 and 7.70 MB; the VST3 input
0.19, 0.64, 0.88 and 1.33 MB; the route 0.32, 0.78, 1.93 and 2.39 MB; the
server's output 0.31, 0.77, 1.92 and 2.38 MB. So a CLAP plugin of 2000
parameters (the node's buffers, its host's lists and the plugin-server's
output) costs about 25.2 MB at 512 frames and 34.2 MB at 4096 (5.0 MB and
14.0 MB before PR 2b), a VST3 plugin (the node's buffers, the input points
and the server's output; the route only while mapped) about 14.8 MB and
19.8 MB (3.35 MB and 8.4 MB). The totals first given here, 23.9 / 32.4 MB
for CLAP and 13.4 / 18.0 MB for VST3 (and 4.7 / 13.2 and 3.0 / 7.6 before),
left out the server's output row. With every queue at the raised room, the
node's buffers and the CLAP lists alone would be about 168 MB and 235 MB.

Every set has the one layout. The figures in the rest of this paragraph
are at the room before PR 2b. The cost of that over a plain layout (a
2-point floor and blocks of `room`) on the sets that never carry a value,
`16 × (queues + 256)` bytes a set: the CLAP output row was 0.86, 3.15,
4.08 and 6.37 MB (its event room is the same budget, so it pays twice),
the server's output 0.27, 0.73, 1.65 and 2.11 MB, and the VST3 host's
emitted set (`MIN_POINT_ROOM`, 16 points a queue) 0.26 MB at 2000
parameters and 1.64 MB at 16k, now 0.29 and 1.90 MB. The VST3 input and
route rows grew by the 64 spare queues they now hold. The IPC row was
2.52, 6.65, 14.95 and 19.08 MB with a 2-point floor and blocks of `room`
(before the carried value had a slot). Before the pool, at 4096 frames, the
first four were about 43 MB, 19 MB, 21.5 MB and 4.3 MB at 2000
parameters, and about 340 MB, 157 MB, 172 MB and 35 MB at 16k. The 16k
figures are mostly the per-parameter headers; the pool term is the same at
any parameter count. At requirement 6's room the pool term is about
1.6 MB a set at 4096 frames, so the IPC node's buffers hold about 16.3 MB
at 2000 parameters (the table above), against about 118 MB at
`params × room`.

Nor does a queue *count* grow on the audio thread: the VST3 input list's
refill drops the queues past its own, counted, and the output list's
`addParameterData` past its queues returns null (the failure the SDK's
samples check for), counted; for an id already in the list it returns that
queue, as the SDK's host does. The bridge drops a carried value for a
parameter with no spare queue left, counted, rather than pushing a queue;
only the placeholder for a block with no changes gets carried-value queues
built on the bridge thread (dropped on return).

Not covered: a CLAP plugin's `params.rescan` reaches no event in the
plugin-server, so its output stays reserved for the load's parameter count;
points of parameters past it are dropped, counted.
The in-process VST2 node reserves through the same `ParameterChanges` and
so gets the pool, though it applies only each queue's last point.

A plugin **ignores a ramp's duration**: a `ParamRamp` reaches it as one
point at its target value on its last frame, as today. Only lane pieces
(segments) carry their shape, as points.

## 10. What gets deleted (PR 5)

| Crate | Deleted |
|---|---|
| `tutti-graph` | `ParamFrom::Audio`, so **`ParamFrom` collapses to `EventOut`** and `connect_param(at, from: EventOut)` loses its shaping; `ParamShaping`, `ShapeLut`, `SHAPE_LUT_LEN`; `ParamSource`; `MAX_PARAM_SOURCES` and the per-source arrays; `signature`'s source hashing; `GraphInvalid::TooManyParamSources`, `UnsortedParamSources`; `DelayKey::ParamAudio`; the `Σ` of the fused step (`param.rs:11`). **`ParamTap` and its seqlock** (`param.rs:930-1060`) and **the fork seeds** (`PendingFork::seeds`, `fork.rs:207`; `Editor::param_seeds`, `seed_params`, `editor.rs:1539-1633`): a fresh lane (a fork's) has nothing in flight and sends at its first frame (§5.6, as landed in PR 2b: a lane never re-sends at a block edge), so a fork's port needs no seed. The one user outside the lane path is a host's hand-wired event source. The requirement after PR 5: **a fresh (forked) instance sends its current value on its first block**; the lane meets it by sending whenever nothing is in flight. Two classes need more: (1) **ramp-based sources**: re-sending a `ParamRamp` restarts it, so re-send the in-flight move as `ParamSegment::resumed`, or the held value as a step; (2) **sources driven by live input that cannot be replayed** (MIDI CC or OSC bridges, UI-driven event nodes): a `ForkByClone` fork does not know the held value, so either write the target's `Param` cell (the knob, which a fork reads) instead of an event source, or capture the current value in the node's `ForkSource::gather`. `ParamSet::set_authored`, `authored`. |
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

**As landed (PR 5).** Every item in the table is deleted, with these
differences from it:

- **`tutti-graph`:** `ParamFrom` is gone as a type, not collapsed to one
  variant: `connect_param(at, from: EventOut) -> Option<EventOut>`
  replaces the port's source and returns the one it had, and
  `ParamMod { range, source: Option<EventOut> }` holds it. The plan's
  `ParamSourceOp` and `ParamSlot` went with the source list; a
  `ParamPortOp` names its one `from` and `slot`. `set_authored` went in
  PR 4b and `ParamSet::authored` here (§8.2): a fork's template shares the
  node's cells, so a fork starts from the knob. The compiler's arrival and
  `tutti_types::latency::plan` both count a param port's source as a
  predecessor (`a_param_source_counts_toward_arrival_in_both_solves`).
- **`tutti-mod`:** its runtime copies of `Lfo`, `Modulator`,
  `RandomState` and `SampleHold` went too (not in the table): `tutti-nodes`
  holds the ones its nodes run (since PR 3), and `tutti-mod` has no
  runtime.
- **`tutti-plugin`:** `MAX_POINTS`'s one remaining role, the room a
  delivery has before `prepare`, is `param_delivery`'s private
  `UNPREPARED_POINTS` (10, kept small for a plugin of thousands of
  parameters). The plugin node reads a foreign lane's `u32` in its own
  address model (a VST2 index, or an opaque id); an address of the other
  model is no longer refused, since a lane carries a bare `u32`.
- **`bevy-tutti`:** `CapturedControls::for_params` is deleted outright, not
  only its modulation role: a node's params are addressed at insert by
  `GraphNode::params`, and `captured` is a hosted plugin's only.
  `AudioGraphRes::set_param_mod` takes `source: Option<AudioNode>` (the
  node's event output 0). `write_param`'s modulated branch and
  `set_param_snapshot` went in PR 4b.
- **Params that were cells, not ports, became ports.** The driver
  modulated any `ParamSet` cell by writing it; a lane drives a port, and
  `Modulation::apply` binds only a param the node declares
  (`reconcile.rs`, `NoSuchParam`). So every param a node addressed but did
  not declare would have lost its modulation. Each is declared now and read
  where the node read its cell, once per block, as the port's last frame
  (`block_driven` in `tutti-nodes`' `src/ramp.rs`; the synth's and the sampler's own
  one-liners), the setter's clamp applied to a driven value as to a
  written one: `ModDelayNode` (chorus and flanger: rate, depth, feedback,
  mix, `MOD_DELAY_PARAMS`), `PhaserNode` (the same four), the convolver's
  mix, `PolySynth`'s volume and, with a unison engine, detune and spread
  (`SYNTH_PARAMS`), a `VoiceNode`'s and a standalone `MemorySource`'s gain
  (`VOICE_PARAMS`), `LfoNode`'s depth and, free-running, rate
  (`LFO_PARAMS`; a beat-synced LFO's rate cell holds beats, which its
  `ParamSet` does not address either); and the rest of the compressor
  (ratio, attack, release, makeup), the gate (attack, release), the
  limiter (release), the delay (mix) and the SVF (gain). An LFO node's
  rate as a port is not §11's "LFO rate by LFO": that is a compiled
  layer's rate, which still needs an integrating layer; the node
  integrates its own phase.
- **A driven value gets its setter's clamp at the point of use**, through
  one function the setter and the block's read share, so the two cannot
  drift: `AttackRelease::clamp_time` (compressor and gate, floored at 0),
  the limiter's `clamp_release` (1 ms: below it the follower freezes and
  gain reduction never releases), `LfoDrive::clamp_rate` (0.01 Hz),
  `ModDelayConfig::clamp_depth`, `Depth::new_clamped` (phaser and LFO
  depth), `CompressionRatio`, `Feedback` and `Mix`'s `new_clamped`, and
  the synth's `clamp_volume` and `clamp_detune` (floored at 0). None
  hides a NaN from the node's `LastGood` read (a `max` floor comes after
  it), so a NaN from a lane reads as the last good value; the limiter
  gained one here, and the LFO node, which has none, reads its cell for a
  non-finite drive. Where a clamp also applies to the cell's value, it
  changes nothing a setter wrote (only a raw `ParamSet::set`). The rate
  floor is the exception, driven-only: a negative rate written straight to
  the effects' LFO cell runs it backwards, which their tests pin. A param
  whose setter does not clamp (the SVF's gain, the compressor's makeup,
  the LFO node's rate) is driven unclamped too, beyond the port's range.
  The phaser's all-pass also holds its sweep inside `1 Hz..0.49 sr`,
  which a negative depth (`set_depth` allows −1) could leave: its
  coefficient then exceeds 1 in magnitude and the stages diverge, a
  defect older than the ports.
- **Kept knob-only:** `BusStripNode`'s `Mute`, a toggle (`>= 0.5` is
  muted) the strip folds in as a factor it never modulates. A route onto it
  is refused with `NoSuchParam`. Nothing else in the table is kept;
  `AutomationLaneNode::new` (the audio lane) and `LfoNode` stay, as the
  table and §11 say.

**The fork-requirement audit** (every `connect_param` and every event
source into a param port outside the lane, at PR 5): the one production
`connect_param` is `tutti_nodes::Modulation`'s, wiring a control lane,
which sends on a fresh instance's first block and is re-wired with
`LaneControls::resync`. `EnvelopeFollowerNode` and `NoteEnvelopeNode` feed
a lane's event inputs, not param ports; the note envelope answers
`EventKind::Resync`, and the follower renders from its input, which a fork
renders too. The contract kit's `ParamPulse` (`param_row`) is a function of
the absolute frame and sends from its first block. `bevy-tutti`'s
`AudioGraphRes::set_param_mod` is the host's hand-wiring API, and states
the requirement. No other source writes a param port. Pinned by
`a_lane_fed_fork_reads_its_value_at_frame_0` twice: in `tutti-graph`'s
`param_mod.rs` (a hand-wired source that sends on its first block, the
requirement as a host meets it) and in `tutti-nodes`' `control_lane.rs`
(the lane); and by `graph_param_mod.rs`'s
`a_fork_modulates_as_the_live_graph_does`.

**`EventKind::Resync` is how a hand-wired event source meets the fork
requirement above** (as landed in PR 4a, §6). A `ForkByClone` fork of a
source driven by live input cannot know the value it holds; a source that
answers a `Resync` on an event input by re-sending where it stands (a
ramp's in-flight move as `ParamSegment::resumed`, a held value as a step)
can be asked to, by the host, into the fork's executor as into the live
one (`Editor::schedule`), with nothing crossing outside the ports. Every
param port and every node that sends nothing a sink could miss ignores it.

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
and PR 4 makes the in-process VST2 node deliver parameter events (§13;
landed).

**Follow-up to PR 1: parameter-change memory (landed).** The shared point
pool of §9's point-storage note replaced the reservations below; its new
per-instance figures are there. What it replaced: PR 1 reserves every
parameter-change buffer at `params × points_for(chunk)` points, and the
bridge holds `PARAM_BUFFERS` of them: about 43 MB for 2000 parameters at a
4096-frame chunk, about 340 MB for a 16k-parameter VST3. The plugin-server
reserves for the same points again per instance: the CLAP host's input
event list (64-byte events plus an 8-byte sort key, uncapped) about 19 MB
at 2000 parameters and 157 MB at 16k, the VST3 host's input queues about
35 MB at 16k (4096 frames). The CLAP host's output side (PR #92) reserves
the same again per instance, for the plugin's own parameter output: the
output event list (64-byte events, a parameter room of `params × points`
beside 256 for notes and MIDI) about 16.9 MB, plus the reserved output
queues (`out_param_changes`) about 4.6 MB, so about 21.5 MB at 2000
parameters and 4096 frames, and about 172 MB at 16k. Replace all of them
with a shared per-chunk point arena, or a small per-queue room with a
counted, shared overflow, so the memory follows the points a chunk carries
rather than the parameter count; the follow-up covers the input and the
output side alike. The in-process VST2 node reserves the same room
and applies only each queue's last point (VST 2.4 has no sample-accurate
parameter events), so its per-queue point room is mostly wasted: about
400 KB per node at 200 parameters and a 4096-frame block; one point per
queue would do there.

**PR 2b raised the room, and the pool absorbs it (landed together).**
Requirement 6 moves the room from `ceil(chunk / 32) + 4` to
`ceil(chunk / 32) + 1 + 256`, and every reservation uses that one formula
(`ParameterQueue::chunk_room`): 132 points to 385 at a 4096-frame chunk,
20 to 273 at 512. Reserved per queue, as PR 1 did, that would have been
about 235 MB per CLAP instance of 2000 parameters at 4096 frames and
168 MB at 512 (about 940 MB in the node's buffers alone at 16k). PR 2b
landed on the shared pool instead, so only the pool's blocks grow with the
room: per instance at 2000 parameters a CLAP plugin holds about 34.2 MB at
4096 frames and 25.2 MB at 512, a VST3 plugin about 19.8 MB and 14.8 MB,
the plugin-server's output included
(§9's table has every site). The 256 corners a queue may take are now a
block's, lent only to a queue that moves.

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

**PR 2b: landed** (branch `claude/control-lane-node`), but for the signal
sources:

- `tutti_mod::ControlProgram` (§4.1) with layers, input layers, rate
  anchors, `value_at` / `value_before` and `next_corner`; `Curve` and
  `CurveSegment` are ungated (no `routing` feature needed).
- `tutti_nodes::automation::ControlLaneNode` and `LaneControls` (§5),
  `CONTROL_STEP` = 32 and `LANE_EVENT_CAPACITY` = 256: the grid of the
  play clock, every corner, exact mode for a native target's pure
  automation, the plugin lane's clamp and split, thinning and truncation
  (counted), steady holds, the glide, the fork. Constructors:
  `ControlLaneNode::for_param(key, program, knob, range)` (the knob a
  node's `Param` cell, the range `Editor::param_range_cell`) and
  `for_foreign(id, program, knob, (min, max))`.
  `AutomationLaneNode::for_param` and `for_foreign` return a
  `ControlLaneNode` of automation alone; `AutomationLaneNode` itself is
  the audio-output lane only.
- The plugin side (§9 requirement 6): `ParameterQueue::chunk_room` is
  `ceil(chunk / 32) + 1 + 256` (`CORNER_POINTS`), on the input side and
  #92's output side alike, as each queue's logical room over the shared
  point pool (§9's point-storage note: a floor of 3 a queue, 256 blocks
  of `room + 1`). A queue is full at its room, or at its floor with the
  pool spent; a full queue drops a grid point offered to it and a corner
  evicts the queue's earliest grid point, both counted in
  `ParamDeliveryStats::grid_points_dropped`, and only a queue of corners
  keeps its latest value in its latest point's place (`points_merged`).
  The node writes through `ParameterChanges::make_room` and `push_point`,
  and marks its grid points in `GridBits`, laid out as the points are. A
  segment starting on a chunk's last frame is a corner. The VST3 host's
  `IMidiMapping` merge and input refill sort a queue with
  `PointBuf::sort_by_offset` (stable, in place, allocation-free): std's
  stable sort allocates past 256 points, under the raised room. A point is a grid point when its frame's play count is a
  multiple of 32 (`param_input::take`, through `Cx::play_at`).
  `SegmentPoints` sends a segment's start, its value on each play-grid
  frame in flight (a grid point), each chunk's last frame and its arrival,
  never the chunk-relative stride points. A lane's pieces run grid frame
  to grid frame, so they have no grid frame inside; a segment scheduled by
  hand is held at most 32 frames.
- Changed tests, each marked "Changed in doc 014 PR 2b" in place:
  `clap_process_no_alloc.rs` (#92's room, refusal and flood figures follow
  the new `chunk_room` in the one layout: 274 a queue with the set's one
  more, 4384 for the output list; the flood is 2200 points a parameter,
  8250 lost a block, so two parameters stay wholly past the room, as in
  #92); `tutti-clap-host`'s `pooled_chunk` test helper (a point a frame,
  so a 273-point room fits a 512-frame block);
  `segment_points.rs`' `a_segment_sends_points_until_it_lands` (the play
  grid replaces `stride_for(64)`); and `clap_automation.rs`' `follows`,
  whose upper bound for a lane is the curve at the next grid frame, not at
  the frame: a lane's chunk-edge point is its `Linear` piece's value there,
  on the chord from `curve(g)` to `curve(g + 32)`, which on a convex
  rising curve is above `curve(f)` (2.9e-5 relative at frame 255) and, the
  curve being monotone, never above `curve(g + 32)`.
- Deviations from the text above, each recorded where it applies: no
  re-send at a block edge (§5.2 step 5); an unchanged glide is skipped and
  exact mode jumps on a republish (§4.2); the lane is `Moderate`, measured
  (§5.3); a fresh lane, a fork's included, sends at its first frame
  (§5.6).
- **Not in 2b, left for a follow-up:** `EnvelopeFollowerNode` and
  `NoteEnvelopeNode` (§6), with their properties below (export == live for
  a follower and a note envelope; the follower's one-step lag and
  play-start restart); a sine within the bound for each bounded curve
  other than `Linear`, and the unbounded curves' measured errors pinned;
  a curved bipolar layer's zero-crossing points checked at the lane (the
  program's own tests cover them as corners); and the plugin live ==
  export points at one chunk size, which is PR 4's test.

**Scope.** `ControlProgram` in `tutti-mod` with `Curve` in the floor;
`RangeCell` in `tutti-types`; `Env::play: PlayClock`, `Env::play_at`,
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

- PR 2a, `crates/core/tutti-graph/tests/param_mod.rs` (each marked
  "Changed in doc 014 PR 2a"):
  - `an_event_sources_held_value_survives_another_source_joining`:
    reshaping the event source is now refused
    (`ShapedEventParamSource`); it asserts the refusal, then reconnects
    `Identity` and checks the held value survives.
  - `a_segment_sums_with_other_modulators_through_its_shaping`, **renamed**
    `a_segment_replaces_the_base_and_audio_offsets_add`: the segment
    replaces the base, audio sources still add.
  - `a_shrinking_range_rewrites_neither_the_base_nor_a_ramp`: the ramp now
    starts from the base, so the analytic figure moves; the property (the
    clamp rewrites neither) stands.
  - the differential proptest (`modulated_graphs_match_the_reference`): the
    generator follows the rule; one regression seed recorded.
  - `crates/core/tutti-graph/src/history.rs`,
    `a_counting_host_keeps_only_the_jumps_whatever_the_reach`: the size
    bound moves to 24..=28 KiB (a history piece carries the play reading).
- **Unchanged** though listed when this was planned: `a_fork_carries_its_event_sources_ramps`,
  `crates/core/tutti-graph/tests/parallel.rs:288` and
  `crates/core/tutti-graph/tests/rt_no_alloc.rs:712`. Their ports' base is
  0, so base + event and event alone render the same; the fork test is
  still ported in PR 2b (below), when `ParamTap`'s seed stops being its
  mechanism.
- PR 2b, `crates/dsp/tutti-nodes/tests/automation_segments.rs`:
  `an_unrepresentable_piece_is_linear_between_block_edges` and
  `a_curve_without_pieces_takes_the_fallback` assert block-edge sampling,
  which moves to the play-clock grid (§5.4).
- PR 2b, `crates/dsp/tutti-nodes/tests/rt_no_alloc_modulation.rs:248`: its
  lanes become `ControlLaneNode`s and its elastic and sine lanes emit grid
  points; the no-alloc property stands.

**Breaking (CHANGELOG):** an event-driven port reads the stream's value,
not base + value; at most one event source per port, `Identity` shaping.
Migration: remove knob-to-zero workarounds; sum two event sources in one
lane or a node. Also: `LANE_EVENT_CAPACITY` 256, `CONTROL_STEP`, `Env::play`.

**Exit.** M8 and M11 closed; the lane measured and declared `Light`.
**As landed:** measured and declared `Moderate` (§5.3).

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
previous program. **Breaking**, as landed (CHANGELOG): `Layer::depth_by`
became `DepthBy` (a depth route has its own amount, §4.1);
`ProgramError` gained `NotANumber` (a program refuses NaN so `Compiled` is
`Eq`); and `tutti_nodes::{Lfo, Modulator}` became `tutti-nodes`' own
copies (§3), so they are no longer `tutti-mod`'s types.

**Exit.** Compiling anything `bevy-tutti` expresses needs no runtime; the
crate builds with and without `--features bevy`.

**As landed.**

- `compile.rs` (ungated) holds the keys, `Source`, `RouteDecl`,
  `RouteTarget`, `TargetDecl`, `Declarations`, `Adsr`, `SourceNode`,
  `SignalKind`, `Compiled`, `CompileError`, `layer_id` and `compile`
  (§7). Each property above is a test in `compile/tests.rs`, and
  `what_bevy_tutti_expresses_compiles_without_a_runtime` is the exit.
- `depth_by` became `DepthBy { clock, wave, amount }` (§4.1): a depth route
  carries its own depth, polarity and curve. The identity amount
  (`DepthBy::UNIT`) computes `(raw + 1) / 2` exactly as before, bit for
  bit for raw in [-1, 1] (a drawn depth wave beyond ±1 is now clamped); the existing `depth_by` tests changed only their construction
  syntax. A curved or bipolar amount adds its zero-crossing corners.
- `Wave::Const` for macros, not a one-point drawn wave on a 0 Hz clock
  (§7). `ProgramError::NotANumber`: a program refuses a NaN, so it is `Eq`.
- `Adsr::sustain` is `Amplitude` (a 0..1 level, as polysynth's
  `EnvelopeConfig`): `Mix` is a blend position, `Velocity` a strike and
  `Depth` bipolar.
- `PlayReading` moved to `tutti-types` (re-exported from `tutti_graph`), so
  the compiler names no graph. `ParamAddr` derives `Ord`.
- `Lfo`, `SampleHold`, `RandomState` and `Modulator` are copied into
  `tutti-nodes`, and `ModulatorNode`/`LfoNode` were switched to the copies:
  nothing in `routing` or `bevy-tutti` relies on them sharing `tutti-mod`'s
  types.
- No `Cycle` refusal: the compiler sees no audio graph (§6). PR 4 refuses
  it by route.
- EnvelopeFollowerNode / NoteEnvelopeNode and `from_decl`: PR 3b, before
  PR 4, with §6's properties (one-step lag, play-start reset, export ==
  live for a follower).

### PR 3b: the signal-source nodes

**Scope.** `EnvelopeFollowerNode`, `NoteEnvelopeNode` and their
`from_decl` constructors (§6); §7's stale-slot hazard.

**Landed** (branch `claude/mod-signal-nodes`), with §6's "As landed (PR
3b)" choices: peak over channels, linear ADSR segments, play start read as
an epoch change at the input's time, `ForkByClone`, `from_decl` in
`tutti-nodes`. The hazard closed in both the lane and the compiler (§7).

- **Properties, each a test** (`tutti-nodes/tests/signal_sources.rs`
  unless named): the follower's one-step lag against the analytic one-pole
  `1 − c^(g − t₀)`, the same events bit for bit at block sizes 1, 7, 32,
  100, 128 and ragged; both nodes reset on the start's frame inside a
  block; the note envelope's stages land on their frames (the peak on
  `on + attack`), with retrigger, last-note priority and a mid-attack
  note-off released from the current level; `from_decl` builds its own
  kind only; the kick ducks the pad through a lane, analytically, with the
  peak on the second channel; export == live through lanes, for a follower
  and a note envelope, with a start off the executor's grid; serial ==
  parallel bit for bit and no allocation, serial and on a gated pool
  (`rt_no_alloc_modulation.rs`); the reused-id hazard (`control_lane.rs`,
  `compile/tests.rs`). From review: a fresh note envelope resets at a
  start inside its first block; MIDI 2.0 notes and both protocols'
  velocity-0 note-ons; zero-length stages and the sustain clamp; a
  refused segment re-sent resumed, bit for bit against 1-frame blocks; a
  1000 s release decaying at its own rate.
- **Breaking** (CHANGELOG): `Compiled` gained `next_source`, and a new
  source's id is no longer the lowest free one.

### PR 4: `bevy-tutti` to lanes and edges

**Split in two (coordinator's decision).** **PR 4a** is the reconcile,
Bevy-free, in `tutti-nodes`: everything §8.2 does to an `Editor`, the
source-side resync (§6) and the cycle refusal, with a headless host. **PR
4b** is `bevy-tutti`: the declarations, the adapter implementing `ModHost`
over entities, the knob and plugin-GUI paths, takes, and the properties
that need an app or a plugin.

**PR 4a: landed** (branch `claude/mod-reconciler`):

- `tutti_nodes::Modulation` (`src/reconcile.rs`, §8.2 "As landed"):
  `apply(&Declarations, &mut Editor, &dyn ModHost) -> Result<Applied,
  ReconcileError>`, `commit`, `send_resyncs`, `lane`, `source`,
  `compiled`. The seam is `ModHost` (`target`, `feed`) with `Target`
  and `Feed`. `ReconcileError` names `Compile`, `Cycle { route }`,
  `WrongTarget`, `NoSuchParam`, `NoSuchEventInput`, `PortTaken`,
  `SharedPort` and `WrongFeed`, and
  a refused `apply` changes neither the editor nor the reconciler.
  `Applied` reports `commit`, `resync` (resyncs owed: commit even when
  nothing else needs it), the `republished` targets and the targets and
  sources left unresolved. A resync the editor refuses for now stays owed;
  one refused for good is dropped and counted (`resyncs_dropped`).
- `EventKind::Resync` and `Event::resync` (`tutti-graph`): every
  exhaustive match on `EventKind` names it (param ports, the plugin
  nodes, the reference). The note envelope answers it (§6 "As landed
  (PR 4a)"); `EnvelopeFollowerNode::replacing` covers a rebuilt follower.
- `ParamSegment::unit(UnitParam, …)` and `ControlLaneNode::for_unit`:
  the erased constructors a reconcile holding a `ParamAddr` needs, which
  also reach params with no `ParamKey` (`Pan`, `RoomSize`, …).
  `for_param::<U>` is a thin wrapper over `for_unit`, so there is one
  construction path.
- A headless example, `tutti-nodes/examples/headless_modulation.rs` (an
  LFO and a follower on an SVF's cutoff and Q), and a doctest on
  `Modulation`.
- **Properties, each a test** (`tutti-nodes/tests/modulation_reconcile.rs`
  unless named): the §8.1 example's analytic cutoff and the pad's
  analytic dip; a depth edit keeps phase (M2) with no plan and no commit;
  range and depth edits need no commit and a follower route exactly one;
  a removed source leaves no node and no layer (M10); a knob written
  after a route edit is the knob (M9); a held note envelope newly routed
  reads its level from the first grid piece; a route dropped and re-added
  between commits resyncs (the trigger is the program's read set); a
  follower takes no resync; a follower downstream of its target, directly
  and through a bus, is refused by route with the editor and the
  reconciler unchanged; export == live through a fork on a native param;
  `for_param::<Hz>` and `for_unit(Cutoff)` send the same segments, and a
  `Pan` lane reaches its port; a foreign target's lane delivers on its
  event input and takes a range edit as a store; a host's literal key is
  never taken, and one it takes later is followed; a rebind to a node
  with another param list waits for its commit, so a later range edit
  lands (the review's repro); a removed feed node leaves its source
  unresolved rather than every apply refused; a port another event source
  drives is refused with nothing changed; two targets on one port are
  refused with nothing changed, a target moved to another key on its port
  takes it, a replace with the same param list keeps modulating, and
  undoing a taken lane's wiring keeps the host's own edges; and
  no allocation on the audio thread across an apply that republishes
  (`rt_no_alloc_modulation.rs`).
- **Left for PR 4b:** export == live on the reference CLAP plugin's gain
  at a pinned chunk size (a plugin is `tutti-plugin`'s, which
  `tutti-nodes` cannot name; the foreign seam is exercised here on a
  capture node standing in for a plugin), a plugin GUI edit becoming the knob, takes,
  `Mods`/`ModRange`/`ParamRef` and the breaking changes below.

**Scope (PR 4b).** §8: `Mods`, `ModRange`, `Automation`, `ParamRef`, the
adapter onto `Modulation`, knob and plugin-GUI paths, takes, plugin
targets as lanes. The old driver is unscheduled, not yet deleted.

**Properties.** The §8.1 example renders the analytic cutoff within
tolerance and the pad dips with the kick; a depth slider keeps phase at
render level (M2); export == live in an app on a native param and on the
reference CLAP plugin's gain at a pinned chunk size (M3: fails on `main`);
a despawned source leaves no node or layer (M10); range and depth edits send
no plan, a follower route one commit; a knob written after a route edit is
the knob (M9); a route added to a note envelope that is held reads the
envelope's level at once (a source-side resync beside
`LaneControls::resync`: the source re-sends where it stands, resumed, as
it does after a refused segment; §6); a plugin GUI edit becomes the knob; a take suspends then
restores automation; a follower downstream of its target is refused by
route.

**Breaking:** `.per_block()`/`.per_sample()` go;
`ModParamRange::with(param, base, min, max)` becomes
`ModRange::new(param, min, max)` with the knob as base; depth is
span-scaled everywhere, so an old `PerSample` route is `max − min` times
stronger (divide its depth by the span).

**Exit.** `bevy-tutti` modulation runs on lanes only; M1–M3, M6, M7, M9,
M10, M12 closed there.

**PR 4b: landed** (branch `claude/mod-bevy-adapter`):

- `bevy_tutti::modulation`: `Mods`, `ModSource` (`Lfo`, `Macro`,
  `Follower`, `NoteEnvelope`, or a host's `ModSourceKind::declare`),
  `ModRoute`, `ModRange::new(param, min, max)`, `Automation`,
  `AutomationTake`, `ParamRef` (`unit`, `plugin`), `ModulationState`,
  `ModulationRefused` and `TuttiModulationPlugin`, whose systems are
  `ModulationSystems::Apply` and `AfterCommit` (§8.2 "As landed (PR 4b)").
- Plugin targets are lanes on the plugin's event input
  (`Target::Foreign`, port 0), their knobs seeded from
  `PluginParamValues` and moved by the plugin's GUI
  (`plugin_param_edits`). `plugin_bind_params` and the frame-rate driver
  (`modulation::legacy`, `LegacyModulationPlugin`) are no longer
  scheduled; PR 5 deletes them.
- `ParamSet::set_authored` is gone; `AudioGraphRes::contains` answers
  for every node in the graph's spec (a lane included), and
  `event_sources` lists every edge on the port (a lane's included).
- **Properties, each a test in an app** (`bevy-tutti/tests/mod_lanes.rs`,
  `tests/mod_plugin.rs`, and `src/modulation/tests.rs` for the ones that
  read the plan sent): the §8.1 example's analytic cutoff and the pad's
  analytic dip; a depth slider keeps phase at render level with no plan
  (M2); range and depth edits send no plan, a follower route exactly one;
  export == live bit for bit on a native param (ragged live blocks) and
  on the reference CLAP plugin's gain at the pinned 1024-frame chunk
  (M3: on `main` 7bc1d6f3 the export rendered the authored gain, 0.0631,
  against live's modulated 0.5012); a despawned source leaves no node and
  no layer (M10); a knob written after a route edit is the knob (M9); an
  authored write to a modulated param moves the knob; a held note
  envelope newly routed reads its level at once; a plugin GUI edit
  becomes the knob (the in-process VST2 probe); a take suspends the
  automation, then its envelope plays; a follower downstream of its
  target is refused by route and surfaced once; an entity rebound to a
  node with another param list is modulated from the node's first frame;
  a crossfaded plugin keeps its lane. Each records the mutations run
  against it.
- **Breaking** (CHANGELOG): the declarations above replace
  `ModParamRange`, `ModDelivery` and `per_block`/`per_sample`; depth is
  span-scaled everywhere (divide an old `PerSample` route's depth by the
  span); `set_authored` and `set_param_snapshot` go; the old types live
  in `modulation::legacy`.
- **Left for PR 5:** deleting `modulation::legacy`, `plugin_bind_params`
  and the legacy suites that run against them; `crossfade_plugin_node`
  does not re-read `PluginParamValues` or refresh `PluginEmitter` for the
  incoming plugin (its lanes keep their knobs and stay wired, but the
  incoming plugin's GUI edits are not reported).

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

**PR 5: landed** (branch `claude/mod-delete-old`). **The campaign is done.**
Every item of §10's table is deleted, crate by crate, with the corrections
recorded under §10's "As landed (PR 5)": tutti-graph's param port is
`ParamMod { range, source: Option<EventOut> }` and its fused step is
`clamp(held, range)` (`ParamFrom`, `ParamShaping`, the shape LUT, `ParamTap`
and the fork seeds gone; `ParamSet::authored` with them); tutti-mod keeps
only the compiler (§7) and its vocabulary, the runtime copies of `Lfo`,
`Modulator`, `RandomState` and `SampleHold` being `tutti-nodes`'; tutti-nodes
drops `ParamModShaping`; tutti-plugin drops `PluginAutomation` and its
source, a plugin parameter being a `ControlLaneNode::for_foreign`;
bevy-tutti drops `modulation::legacy`, `LegacyModulationPlugin`,
`plugin_bind_params` and its kin, and `CapturedControls::for_params` (a
node's params are addressed at insert by `GraphNode::params`).
`AudioGraphRes::set_param_mod` takes an `Option<AudioNode>` event source
and a range. Every param a node addresses is a port a lane can drive,
except the bus strip's `Mute` (§10 "As landed"). Audio-rate modulation (`ParamFrom::Audio`) went with nothing
in its place but §11's: a node's own audio input.

Tests went with what they tested. Ported: bevy-tutti's
`capture_controls` → `params_at_insert` (four tests, the param set
addressed by every insert path and a crossfade), four of the driver's
suites' properties → `mod_lanes` (two routes summing and a disabled one
adding nothing, a host's own source kind, a route declared before its
target's node, a bus strip's volume and pan), `graph_wire`'s param-mod
wire to an event source; tutti-nodes' `param_set_routes` and
`graph_param_mod` to lanes; tutti-plugin's CLAP and VST2 delivery,
no-alloc and parameter suites to lanes on the plugin's event input;
tutti-graph's `param_mod` suite and polysynth's three modulation tests to
the collapsed port; tutti-sampler's
`a_fork_starts_from_the_gain_last_set_and_shares_nothing` loses its half
that stored a driver's composite into the live cell (the cell is the gain
last set now) and gains a sharing mutation. New with the ports that
replaced written cells: `param_set_routes`'
`a_lane_modulates_a_chorus_rate_around_its_own_cell` (the claim the old
`chorus_rate_route_moves_the_nodes_own_cell` made, on a lane) and
`a_lane_drives_an_lfo_nodes_depth_and_rate`, `graph_param_mod`'s
block-rate cases, `tutti-polysynth`'s `param_lanes` (volume, detune,
spread), the sampler's lane tests on a voice's and a memory source's gain,
and `mod_lanes`' `a_chorus_rate_and_a_phaser_mix_take_lanes`; and
`removing_the_last_route_returns_the_param_to_its_knob`, the counterpart
of `mod_value_path`'s `removing_a_route_returns_the_param_to_its_base`.
Deleted with their subject: tutti-mod's runtime suites
(`curve_source`, `driver`, `id`, `layered`, `lfo`, `matrix`, `modulator`,
`param`, `router`, `routing`, `matrix_public_api`, the fold tests in
`shape.rs`), tutti-plugin's `automation_node` and
`param_automation_source`, tutti-nodes' `param_mod_oracle` (its convention
is the shape tests'), tutti-graph's `ParamTap` ring and LUT tests and
`a_fork_carries_an_in_flight_segment` (a fork carries no seed), and
bevy-tutti's `mod_value_path`, `mod_audio_rate` and
`graph_value_modulation` where no lane counterpart applies: the LFO-rate
cascade (§11), host-supplied sinks and curve delivery (gone with
`ModTargetRegistry`; a lane is sample-accurate), reflection and kind
registration, and the claim set. Every ported or new test carries its
mutation record, run in this PR. The fork-requirement audit is under §10.

## 13. Decided questions

No questions remain open. These were open in review and are now decided:

1. **A plugin whose parameter list grows after load** (CLAP `rescan`, VST3
   `restartComponent`): rely on PR 1's spare headroom and count overflow
   (requirement 3). Re-binding the node on the plugin's rescan notice is a
   follow-up.
2. **The in-process VST2 node drops parameter events**
   (`crates/plugin/tutti-plugin/src/format/vst2_in_process/node.rs:55-66`):
   fixed in a separate small PR right after PR 1, before PR 4. **Landed**:
   the node runs PR 1's delivery (requirements 1–5, with requirement 4 as
   amended: its fork is seeded from the live record), with the block as
   its chunk and each parameter's last point in the block set before the
   plugin processes it, as the out-of-process VST2 loader does with a
   chunk (doc 013, "In-process VST2 parameter delivery").
3. **The largest block:** exports render in blocks of at most 4096 frames,
   through `Limits::max_block`, and `LANE_EVENT_CAPACITY` stays 256. A live
   block past 4096 frames thins and counts (§5.3).
