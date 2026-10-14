# Modulation is a noun of the API, not of the engine

Status: **proposed** (2026-10-14). Supersedes the param-modulation design of
[doc 013](013-native-graph.md) ("Item 6 landed: compiler-owned param
modulation" and the control-rate tier it sits beside). Nothing here has
landed; the [campaign](#13-the-campaign) lists the PRs that land it.

All file references are to `main` at `8ae55f4b`.

## Summary

- **The engine models no modulation.** A param is a base value plus a
  sample-accurate event stream (ramps, segments), clamped to its range.
  LFOs, depth, polarity, routes and macros are words of the API.
- **One value rule, everywhere:**
  `value = clamp((automation(t) if the param has a lane with automation, else
  knob) + Σ modulation offsets(t), range)`. Automation replaces the knob;
  modulation adds.
- **The API compiles; a node evaluates.** `tutti-mod` compiles sources and
  routes into one **control program** per target. The API publishes it
  through `RtPublish`. A **control lane node** (the generalised
  `AutomationLaneNode`) reads it once per block and sends `ParamSegment`s
  to the target: a native param port, or a hosted plugin's parameter.
- **Export equals live by construction.** The lane is a pure function of its
  program, its knob, its inputs and the transport. A fork copies all four.
- **Signal-driven sources are ordinary nodes.** An envelope follower or a
  note envelope sends events into the lane's event input. The user still
  writes one route.
- **Plugins get changes only**, from per-parameter tables sized when the
  plugin is bound. There is no cap at 16 parameters, and nothing unchanged
  is re-sent, so an edit made in the plugin's own GUI survives.

## 1. The principle

### SQL, not a query plan

A modulation matrix is what the user *says*: "this LFO wobbles that cutoff
by 30 %". The engine should *run* something smaller: a value, and the events
that move it. SQL and query plans split work the same way. The statement is
the user's language, and the plan is the executor's. Nobody adds `JOIN` to
the storage engine.

So `ModSource`, `ModRoute`, depth, polarity, response curves and clocks are
nouns of the API (`bevy-tutti`'s declarations, `tutti-mod`'s compiler). They
compile down to two engine primitives that already exist:

- a graph node that sends `ParamSegment`s (the lane);
- a param port, or a plugin parameter, that takes them.

### What other engines model

| System | What the engine has | Where depth, LFOs and routes live |
|---|---|---|
| Web Audio `AudioParam` | `computedValue = intrinsic (value or automation timeline) + Σ connected inputs`, clamped to the nominal range | The app: a `GainNode` is the depth |
| CLAP | `PARAM_VALUE`, plus `PARAM_MOD` ("the value heard is param_value + param_mod") | The host's or the plugin's own modulation UI |
| VST3 | Normalised point values in sample-offset queues; the host sends changes | The host |
| JUCE, nih-plug | Params and smoothers; nih-plug applies CLAP modulation events as offsets | The plugin |
| SuperCollider | Control buses; a param reads a bus (`.map`) | The patch, as wiring |
| Bitwig, Vital, Surge, Ableton | Rich modulation matrices | Inside a device or instrument, never in the audio engine. Ableton splits automation (the value) from modulation (a relative offset), as CLAP does |

None of them puts a modulation matrix into the graph runtime. At most they
model "a value plus offsets, clamped". Even that has the same two-part
shape as the rule in §2.

### Why the first design failed

tutti modelled modulation twice, and both models were engine mechanisms:

1. **The graph's fused param step.** It computes
   `clamp(base + Σ shape_j(source_j))` per frame, over at most 16 shaped
   audio or event sources (`crates/core/tutti-graph/src/param.rs:11`,
   `MAX_PARAM_SOURCES` at `:40`, `ShapeLut` at `:210`, `ParamShaping` at
   `:279`).
2. **A control-rate tier beside it:**
   - `tutti-mod`'s `routing` feature;
   - `bevy-tutti`'s per-frame driver and its three delivery tiers;
   - the plugin path's `PluginParamTarget` and `PluginAutomation`.

   It runs on Bevy's frame clock, with its own sum, base, depth convention
   and ownership rules.

Each defect below comes from treating modulation as a runtime mechanism that
something must drive. None of them is a typo.

| # | Defect | Evidence |
|---|---|---|
| M1 | **Depth means different things on different tiers.** `PerFrame` and `PerBlock` scale by the range's span; `PerSample` bakes `shape(x, depth, ..)` with no span. Drive ranged 0..10 at `Depth(0.5)` swings ±5 per frame and ±0.5 per sample. | `crates/core/tutti-mod/src/driver.rs:335-339`, `crates/core/tutti-mod/src/curve_source.rs:182-184` against `crates/dsp/tutti-nodes/src/param_mod.rs:46-55` and `crates/bevy-tutti/src/modulation/audio_rate.rs:300-307` |
| M2 | **Moving a depth slider restarts every free-running LFO.** `Changed<ModRoute>` raises `dirty`, and the rebuild constructs every `Sourced` from phase 0. The driver's own doc says this must not happen. | `crates/bevy-tutti/src/modulation/source.rs:305-331`, `:225-232`; `crates/bevy-tutti/src/modulation/driver.rs:1-6` |
| M3 | **Control-rate and plugin modulation never reach an export.** A fork freezes a plugin target to its authored part, so the export drops its modulation layers. `ParamSet`'s doc claims "the export runs its own modulation", but there is no offline driver. | `crates/plugin/tutti-plugin/src/host/node/param_automation_source.rs:336-353`; `crates/core/tutti-graph/src/controls.rs:14-27`; doc 013 lines 1193-1200 |
| M4 | **Plugin delivery drops parameters past 16.** `add_point` returns `false` past the inline queue count. The plugin node ignores the return. | `crates/plugin/tutti-plugin/src/host/node/automation_node.rs:249-270`, `graph_node.rs:297`, `crates/plugin/tutti-plugin-types/src/automation.rs:175-178` |
| M5 | **Plugin params are re-sent every block, unchanged or not.** This overwrites edits made in the plugin's own GUI. | `automation_node.rs:151-180` |
| M6 | **Free-running modulation on a plugin stops with the transport** (`if t.playing`). Native free-running LFOs do not. | `automation_node.rs:164` |
| M7 | **Nested `RtPublish` reads per point on the audio thread.** The node reads the lanes, then each `PluginParamTarget::value_at` reads its own cell, up to 10 times per lane per block. That breaks "read once per block, never per sample" and "avoid nested reads". | `automation_node.rs:156-170`, `param_automation_source.rs:327-334` |
| M8 | **Automation is an offset, not a value.** `AutomationLaneNode::for_param` sends absolute values, but a param port adds event sources to its base. Automating 200→2000 Hz under a knob at 1000 Hz renders 1200→3000 Hz. | `param.rs:173-185`, `crates/dsp/tutti-nodes/src/automation/lane.rs:191-201` |
| M9 | **Two writers, and a rebuild reseeds the base.** The base of a `PerFrame` param is `ModParamRange.base`, so any route edit reverts a knob written since. | `crates/bevy-tutti/src/modulation/driver.rs` (`rebuild`); `crates/bevy-tutti/src/graph/param.rs:147-174` (`write_param`'s two branches) |
| M10 | **The per-sample source node goes stale and leaks.** It is spawned once, never updated on a `ModSource` change, and never despawned. | `crates/bevy-tutti/src/modulation/audio_rate.rs:149-189` |
| M11 | **The sum is order-dependent in its last bits.** `LayeredCurve` sums in `Vec` insertion order. | `crates/core/tutti-mod/src/layered.rs` (`value_at`) |
| M12 | **Two producers for one job.** `PluginAutomation` samples `dyn Curve` at strides, and `AutomationLaneNode::for_foreign` sends segments to the same plugin input. | `automation_node.rs`, `lane.rs:203-210` |

M3 is the structural one. A mechanism driven from outside the graph cannot
be in a fork, unless a second "offline driver" replays a frame-clocked system
in render time and is kept bit-identical with the first. Every other defect
is a second copy of something the graph already does: a second sum, a second
clock, a second base, a second depth convention.

## 2. The value rule

```text
value(t) = clamp( base(t) + Σ_layers offset_i(t) , range )

base(t) = automation(t)   where the target's lane has automation with a value at t
        = knob            otherwise
```

- **Automation replaces the knob.** Every major DAW, Web Audio and CLAP
  `PARAM_VALUE` do the same.
- **Modulation adds.** An offset is in the target's units:
  `shape(raw, depth, polarity, curve) × span`, with one convention for
  every source kind (it fixes M1). The `span` is `range.max − range.min`.
- **The clamp is applied once**, to the sum, by the lane (§4.4).
- **An automation curve with no value at `t`** (before its first point, or
  disabled) yields to the knob there. The `Option` that `Curve::value_at`
  returns is that distinction.

| Path | Today | After |
|---|---|---|
| Native param, automated (`AutomationLaneNode::for_param`) | knob + curve (M8) | curve |
| Native param, LFO route `PerFrame` | `ModParamRange.base` + LFO × span, at the Bevy frame rate, not exported | knob (or automation) + LFO × span, per the lane's control step, exported |
| Native param, LFO route `PerSample` | knob + LFO (no span: M1) | as above |
| Plugin param, `ModParamRange` | host base + layers, sent every block while playing (M5, M6) | knob (or automation) + Σ layers, changes only, rolling or not |
| Unmodulated, unautomated param | knob | knob (unchanged: no lane, no events) |

**Breaking.** "Automation adds to the base" goes. A consumer that automated a
param whose knob was 0 hears no difference; any other consumer does. See
[§11](#11-migration-notes-and-changelog).

## 3. Where things live

Dependency facts (checked against each `Cargo.toml`):

- `tutti-graph` depends on no tutti crate but `tutti-types`.
- `tutti-mod` depends on `tutti-types` and `audio-automation`. Its floor
  needs no feature, and its `bevy` feature adds reflection only.
- `tutti-nodes` depends on `tutti-mod` and `tutti-graph`.
- `tutti-plugin` depends on `tutti-nodes`.
- `bevy-tutti` depends on `tutti-mod` (optional) and `tutti-nodes`.

| Piece | Crate | Why there |
|---|---|---|
| `ControlProgram` and its evaluation (`value_at`, `next_corner`) | `tutti-mod`, ungated | The compiler produces it, and `tutti-mod` cannot name `tutti-nodes` (the edge points the other way). It holds `Arc<dyn Curve>`, and `Curve` is `tutti-mod`'s. It is not `tutti-graph`'s: the engine models no modulation, and `tutti-graph` may name only `tutti-types`. |
| `Curve`, `CurveSegment` | `tutti-mod`, moved out of the `routing` feature into the floor | Automation needs them with no routing at all |
| The compiler (`tutti_mod::compile`) | `tutti-mod`, ungated | Pure. It compiles with no Bevy and no runtime. |
| `ControlLaneNode`, `LaneControls`, `EnvelopeFollowerNode`, `NoteEnvelopeNode`, `CONTROL_STEP` | `tutti-nodes` (`control` module, re-exported at the root) | They are graph nodes. `tutti-nodes` already owns the lane. |
| A port's event source replaces its base | `tutti-graph` | It is the engine's one rule ("a base plus an event stream") |
| Plugin delivery (changes only, per-param tables) | `tutti-plugin`, `tutti-plugin-types`, `tutti-plugin-server` | Where `ParameterChanges` is built, carried and applied |
| Declarations, the reconcile step | `bevy-tutti` (`modulation` feature) | The API |

## 4. The control program

### 4.1 The type

```rust
// tutti-mod. Ungated; every item re-exported at the crate root.

/// What one target's lane evaluates each block: a base, plus offsets. A
/// description, never a runtime: it holds no clock and no state, and two
/// equal programs evaluate to the same bits.
///
/// Values are the target's raw `f32` (a cutoff's Hz, a gain's linear
/// amplitude, a plugin parameter's normalised 0..1). The lane is erased over
/// the unit, as `ParamSegment` and `ParamRange` are: one lane type serves
/// every param.
#[derive(Clone)]
pub struct ControlProgram {
    /// Replaces the knob wherever it has a value (beat time). `None`: the knob.
    pub automation: Option<Arc<dyn Curve>>,
    /// Time-function layers, summed in `id` order.
    pub layers: Box<[Layer]>,
    /// Event-input layers: signal sources wired into the lane's event input,
    /// matched by `source`; summed in `id` order after `layers`.
    pub inputs: Box<[InputLayer]>,
}

/// Stable across republishes: the lane keys per-layer state on it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LayerId(pub u64);

/// One periodic offset: a wave, one period of it, on a clock, depth baked in.
#[derive(Clone)]
pub struct Layer {
    pub id: LayerId,
    pub clock: Clock,
    pub wave: Wave,
    pub amount: Amount,
    /// The offset scaled by a second wave mapped to `[0, 1]` (an LFO's depth
    /// moved by another LFO). `None`: unscaled.
    pub depth_by: Option<(Clock, Wave)>,
}

#[derive(Clone, Copy)]
pub enum Clock {
    /// Phase = frac(beat / per_cycle + phase): locked to the bar; follows
    /// seeks and loops; holds while the transport is stopped.
    Beats { per_cycle: BeatDuration, phase: PhaseIncrement },
    /// Phase = frac(rate · play_time + phase), on the lane's play clock
    /// (§4.3): runs while stopped; ignores seeks, loops and tempo.
    Seconds { rate: Hz, phase: PhaseIncrement },
}

#[derive(Clone)]
pub enum Wave {
    /// A built-in shape, closed form; raw value in `[-1, 1]`. `Random` and
    /// `RandomSmooth` are position-derived (`hash_bipolar` of the cycle
    /// index and `seed`), so they replay identically after a seek and in a fork.
    Shape { shape: LfoShape, seed: u64 },
    /// One drawn period (a step sequence, a custom LFO): breakpoints over
    /// `[0, 1)`, each with the `CurveType` of the piece it starts.
    Drawn(Arc<[PeriodPoint]>),
}

#[derive(Clone, Copy)]
pub struct PeriodPoint { pub at: f32, pub value: f32, pub curve: CurveType }

/// The one shaping convention (it fixes M1):
/// offset = shape(raw, depth, polarity, curve) × span.
#[derive(Clone, Copy)]
pub struct Amount { pub depth: Depth, pub polarity: Polarity, pub curve: CurveType, pub span: f32 }

/// A signal source's raw value (its events, addressed `ParamAddr::Id(source.0)`)
/// shaped into an offset.
#[derive(Clone, Copy)]
pub struct InputLayer { pub id: LayerId, pub source: SourceId, pub amount: Amount }

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SourceId(pub u32);

/// Where the program is evaluated.
pub struct ProgramTime {
    /// The transport beat at the frame; `None` with no usable tempo.
    pub beat: Option<Beat>,
    /// The beat-clock layers' phase source: false while stopped, so beat
    /// layers hold.
    pub rolling: bool,
    /// Frames on the lane's play clock (§4.3), and their rate. A frame count
    /// rather than `Seconds`: `Seconds` is `f32`, and an hour of play time
    /// needs the `f64` the phase is computed in.
    pub play: Frame,
    pub rate: SampleRate,
}

impl ControlProgram {
    /// The sum before the clamp. `inputs[k]` is the current raw value of
    /// `self.inputs[k]`. Pure, allocation-free. Layers are evaluated in
    /// `f64` and summed from `-0.0` in id order, then rounded to `f32` once.
    pub fn value_at(&self, knob: f32, at: &ProgramTime, inputs: &[f32]) -> f32;
    /// The left limit at `at`: the value a piece ending there approaches. It
    /// differs from `value_at` only at a corner that jumps.
    pub fn value_before(&self, knob: f32, at: &ProgramTime, inputs: &[f32]) -> f32;
    /// The next corner strictly after `at`, on each clock. A corner is a
    /// jump or a slope break the lane must not smooth: an automation
    /// breakpoint, a square's edge, a saw's wrap, a triangle's peak, a random
    /// step.
    pub fn next_corner(&self, at: &ProgramTime) -> Corners;
    /// Whether the program is exactly one curve's segments (no layers, no
    /// inputs): the lane then sends the curve's own shapes, as today.
    pub fn is_pure_automation(&self) -> bool;
}

pub struct Corners { pub beat: Option<Beat>, pub play: Option<Frame> }
```

Notes on the sketch:

- **"One period of a curve, depth baked in."** A layer's wave is one period
  (closed form or drawn), and its `Amount` is fixed in the program. A depth
  edit is a republish of a new program, not a value written into a running
  source. Phase is a function of time (§4.3), never of the program's age, so
  a depth edit cannot reset it (it fixes M2).
- **Why layers are summed at all.** LFOs at unrelated rates have no common
  period, so the API cannot bake them into one curve. That sum is the lane's
  only engine-side concession.
- **Depth-by-LFO falls out cheaply**, so it is in scope. `depth_by`
  multiplies one layer's offset by a second wave mapped to `[0, 1]`. That is
  one more evaluation and one multiply at each control point. Its corners
  join the layer's.
- **No response-curve LUT.** `shape()` runs at control points only, at most
  a few hundred times a block (§5.3), so it is evaluated exactly.
  `ShapeLut`'s bake was only needed for a per-frame sum.

### 4.2 The knob and the range are cells, not program

CLAUDE.md: "Non-scalar state … goes through `RtPublish`. Scalars use
`Param<U>`." So a lane's state splits across three handles, and each is
read once per block:

| State | Handle | Written by | A move costs |
|---|---|---|---|
| Program (automation, layers, inputs) | `RtPublish<ControlProgram>` | the API, on the control thread (`LaneControls::publish`) | an `Arc` and a publish; retired programs are freed on the control thread |
| Knob | an `AtomicF32` cell (the target's raw value) | the API's knob writes (`LaneControls::set_knob`) | a store |
| Range | a `RangeCell` (one `AtomicU64` packing both bounds' bits) | the API's range edits (`LaneControls::set_range`) | a store |

**The range.** `RangeCell` is the packed cell `ParamRanges` already uses
(`param.rs`, "A param range is a control" in doc 013). It moves to
`tutti-types` beside `AtomicF32`, and `ParamRanges` reuses it. It is a plain
cell with no protocol between cells, so it needs no loom model. The lane
clamps every value it computes. A shrink pins the value, and a widen releases
it at the next block, as a port's range does today.

**How the lane's range relates to the port's.** The two are separate:

- **The lane's range is the route's range**, the user's min and max
  (`bevy-tutti`'s range declaration). It applies alike to a native port and
  to a plugin parameter, which has no port.
- **A port's range (`Editor::set_param_range`) stays the engine's guard.**
  That is decision 2's "clamped to its range", for any event source a host
  wires by hand. It defaults to unbounded, and `bevy-tutti` stops setting it.

Clamping twice to the same bounds would be idempotent anyway. A linear piece
between two clamped endpoints stays inside them.

**The knob ramp.** A knob or program change seen at a block's first frame is
not applied as a step. The piece in flight is cut there, and the next piece
runs from the in-flight value to the new value at the next control point.
That is a glide of at most `CONTROL_STEP` frames, with no extra state, and a
declick for free.

### 4.3 Clocks and phase, and the free-running question

**Beats.** `phase = frac(beat / per_cycle + offset)`, from the transport's
beat at the frame.

- The beat is closed-form from the segment's frame count (`FrameClock`).
  Live and offline compute it with the same code, so it lands on the same
  bits whether rendered a frame or a block at a time
  (`crates/core/tutti-core/src/transport/offline.rs:139-147` and its test
  `a_block_on_a_beat_is_that_beat_exactly`).
- Seeks and loop wraps are followed exactly.
- While stopped, the beat is still, so a beat layer holds. That is a synced
  LFO's DAW behaviour.

**Seconds: the play clock.** A free-running layer needs a time that runs
while stopped (M6) and ignores seeks, loops and tempo. That time is the
lane's **play clock**: a `Frame` count the lane keeps itself.

- **It advances one per frame**, rolling or stopped.
- **It restarts at 0 on the frame the transport starts rolling.** That is a
  stopped→playing transition as the lane's input sees it (`Cx::transport_at`;
  the lane remembers whether its last frame rolled). It also restarts on a
  lane's first frame if the transport is already rolling then (a fresh lane
  starts "not rolling").
- **Nothing else restarts it.** Seeks, loop wraps, tempo changes and
  republishes leave it running.

So a seconds layer is "free-running, restarted on play".

**Rate edits keep phase continuous.** A seconds layer whose rate changes in a
republish (same `LayerId`) is re-anchored at that block's first frame:
`phase(t) = φ_anchor + rate · (t − t_anchor)`. The lane keeps
`(id, t_anchor, φ_anchor)` per layer in a fixed array. A play start clears
every anchor. A beat layer is never re-anchored: it stays locked to the bar.

**Does export == live hold for a seconds layer? Yes, with a stated scope.**

- **An export restarts its play clock exactly where a live play start does.**
  - An export renders a fork in `ForkMode::Offline`
    (`crates/core/tutti-graph/src/fork.rs:490`), on an offline timeline that
    is rolling from its first frame
    (`crates/core/tutti-types/src/timeline.rs:54`).
  - The fork's lane is fresh, so it starts "not rolling", sees rolling on
    frame 0, and restarts its play clock there.
  - Live, the lane restarts on the frame play starts.
  - So "play from beat X" and "export from beat X" give the same program,
    knob and inputs the same seconds-layer values, bit for bit, from that
    frame on. The same holds on the grid, which §5.3 anchors to the same
    clock.
- **Where it does not hold:**
  - a live take that started somewhere else;
  - a live take that had a rate edit while rolling;
  - a `ForkMode::Live` duplicate taken mid-play. Its play clock restarts at
    its first block, as if play had just started there.

  In each case the live render had a different time origin, so there is no
  single answer for it to match.
- **Why not `Env::frame`?** It is the executor's frame count, and every
  executor starts it at `Frame::ZERO`
  (`crates/core/tutti-graph/src/exec.rs:701`). A fork's clock and the live
  one therefore differ by however long the live graph has run. Anchoring a
  free LFO there would make every export's phase depend on wall time.

**The cost of this choice** is a phase step at play start for a free layer
that was running while stopped. It is the usual "retrigger on play". The
step lands on the start frame, like any other transport discontinuity.

### 4.4 Event-input layers

A signal source cannot be precomputed (§6). Its node sends
`ParamSegment::foreign(source.0, …)` carrying its **raw** value: `[0, 1]`
for a follower, or the envelope's level. Those events go into the lane's one
event input.

- **Fan-in merges sources deterministically**, by offset and then source
  order (`crates/core/tutti-graph/src/compile/mod.rs:1388-1394`). So any
  number of sources share that one input, and the address tells them apart.
- **Adding a source needs only a new edge, never a new lane shape.** The
  lane's state (its play clock, its anchors) therefore survives the edit.
- **The lane tracks each input layer's current segment** in a fixed array:
  the same `Ramp` arithmetic a param port uses.
- **At each control point it reads each input's current value**, and shapes
  it by the layer's `Amount`.
- **An input that has not sent yet contributes 0.** It does not hold back
  the sum.

### 4.5 Publication

- **One read per block.** The lane calls `RtPublish::read` once, at its
  block's first frame, and drops the borrow before returning. It never
  parks an `RtRef` across blocks, and never nests reads: the program holds
  `Arc`s, not further `RtPublish` cells. That fixes M7.
- **No audio-thread allocation.** Evaluation allocates nothing (§4.1).
  Per-layer and per-input state lives in fixed arrays sized by
  `MAX_LANE_LAYERS = 16` and `MAX_LANE_INPUTS = 8`. The compiler refuses a
  program past either cap, by name. That bound is far above any musical
  use.
- **A republish lands on the next block's first frame**, as a glide (§4.2).
  Edits made while stopped land the same way.

## 5. The control lane node

### 5.1 Shape

| | |
|---|---|
| Audio | none |
| Event inputs | 1. Signal sources' raw values, addressed by `SourceId`. Other events are ignored. |
| Event outputs | 1. `ParamSegment`s addressed to the target: `ParamAddr::Unit(param)` for a native port (`ControlLaneNode::for_param`), or `ParamAddr::Id(id)` for a plugin parameter in its normalised 0..1 (`ControlLaneNode::for_foreign`) |
| Event capacity | `LANE_EVENT_CAPACITY` = **256** (today 128) |
| Tail | `Tail::Unbounded`: a generator, never skipped |
| Latency | none. Musical modulation lag is not latency (§6). |
| Cost | expected `Cost::Light`: work per block is O(points × layers), with no per-frame loop. PR 2 measures it and declares from the measurement, as Phase 6 (b) did. |
| Controls | `LaneControls { program: RtPublish<ControlProgram>, knob, range }`, with `publish`, `set_knob`, `set_range` |
| Fork | `ForkByClone`-like: the current program (an `Arc` clone of what `read` returns), the knob and range values at the gather, fresh state. See §5.6. |

**Arrival.** The lane may now have an input (a follower behind a latent
path), so its compiled arrival can be non-zero. The `debug_assert` that
arrival is zero (`lane.rs:439-443`) goes. The lane evaluates every time
function at its *input's* time (`Cx::transport_at`, `arrival` frames
earlier). The compiler then delays its output to the target's arrival, so
each value lands on the frame it belongs to: the same rule a hosted plugin
reads its playhead by.

### 5.2 One block

1. Read the program, knob and range (once each).
2. If the program is new, or the knob moved, cut the piece in flight at
   frame 0 (§4.2).
3. Walk the block's transport pieces (`Env::segments`). At a start, the play
   clock restarts (§4.3). At a stop, a seek, a tempo change or a loop wrap,
   the frame is a point.
4. **Collect the block's points** in frame order:
   - frame 0;
   - every grid point: each frame whose play-clock count is a multiple of
     `CONTROL_STEP`;
   - every corner the program reports (`next_corner`, beats converted to
     frames in closed form, as `lane.rs`'s `before` does today);
   - every input event's frame;
   - every transport change.
5. **At each point `p`, send one linear piece**: from the value at `p` to
   the left limit at the next point `p'`, with length `p' − p`
   (`ParamSegment::resumed` at position 0). The value at `p` comes from
   `value_at`; the left limit at `p'` from `value_before`, which may lie in
   the next block. Clamp both to the range.
   - A beat at `p'` past the block is extrapolated at the block's tempo,
     from the same `FrameClock`. If the next block jumps, it re-sends anyway.
   - An input's value at `p'` is its current segment extended. Past its end
     it holds.
6. **At frame 0 of a block whose piece is still in flight**, re-send that
   piece resumed at its position. Do not recompute it. The lane keeps the
   piece's `(from, to, start, end)` across blocks, so a continuing piece is
   the same piece whatever the block boundaries. A segment is absolute, so
   re-sending it changes nothing at the port (doc 013, "Curve segments
   landed").

**Exact mode.** When `is_pure_automation()` holds (automation and nothing
else), the lane sends the curve's own shaped segments, breakpoints between
frames included. That is today's `for_param` walk unchanged, and
automation-only fidelity does not regress. When automation has no value at a
frame, the knob's constant takes its place. That is a `Hold` piece, never
"send nothing".

### 5.3 Control resolution: `CONTROL_STEP = 32` frames, on the play clock

The sum of differently shaped layers has no closed form a `SegmentShape` can
carry. So the lane sends it **piecewise-linear**. The pieces end at a fixed
grid of 32 frames, plus every corner, so corners stay exact.

| Step (frames at 48 kHz) | Rate | Worst linear-interpolation error, sine LFO, as a fraction of its depth: `(2π f h)² / 8` | Grid events a 4096-frame block |
|---|---|---|---|
| 16 (0.33 ms) | 3 kHz | 10 Hz: 5.5e-5; 20 Hz: 2.2e-4 | 256: the whole capacity |
| **32 (0.67 ms)** | **1.5 kHz** | **10 Hz: 2.2e-4 (−73 dB); 20 Hz: 8.8e-4 (−61 dB)** | **128, leaving 128 for corners** |
| 64 (1.33 ms) | 750 Hz | 10 Hz: 8.8e-4; 20 Hz: 3.5e-3 | 64 |

**Why 32:**

- **Accuracy.** Its error stays below −60 dB of the swing up to a 20 Hz LFO,
  the top of the modulation range. Faster than that is audio-rate modulation,
  which is out of scope.
- **Corners are exact anyway.** A square's edge or a step lands on its own
  frame whatever the step.
- **It fits the capacity.** At 4096-frame blocks the grid takes half of the
  256 events.
- **Smaller steps buy nothing a plugin can use.** A plugin takes at most 10
  points per chunk (`SegmentPoints`). A native port already evaluates each
  linear piece per frame, so it sees no staircase at any step.

**Why the play clock and not the block.** A grid anchored to block edges
would put points in different places at different block sizes, so the live
render (device blocks) and the export (its own `max_block`) would differ.
The play clock restarts at the same frame in both (§4.3). So do the grid and
every value on it.

**Overload.** A block's points can exceed 256 only with a block longer than
4096 frames, or more than 128 corners in one block. Past the capacity the
lane keeps every corner and thins the grid to every other point (then every
fourth, …) for that block, and counts it (`LaneControls::thinned()`). A
thinned block is the one case where export and live can differ by block
size. It is counted, never silent.

### 5.4 Transport: jumps, loops, stop, start

| Event | Beat layers, automation | Seconds layers | Input layers |
|---|---|---|---|
| Seek or loop wrap (in a block, or at its edge) | a point at the frame, re-evaluated at the new beat | continue (play clock unchanged) | continue |
| Tempo change | a point; beats re-derive at the new tempo | continue | continue |
| Stop | hold at the stopped beat | keep running (it fixes M6) | keep following |
| Start | a point; beats from the start beat | play clock restarts at 0, anchors clear | continue |
| No usable tempo | beat layers and automation hold their last value | continue | continue |

While nothing in the program moves (stopped, beat layers only), the lane
sends one `Hold` piece a block. A native port ignores the repeat, and a
plugin's changes-only filter (§9) drops it.

### 5.5 The graph side: an event source replaces the base

`tutti-graph`'s port rule changes in PR 2. That change is what makes
"automation replaces the knob" reachable for a native param.

- A port takes **at most one event source** (`GraphInvalid` refuses a
  second, by name).
- **The source's value replaces the base** from its first event on. Before
  that, the port reads the base, so "an unconnected port reads its base,
  never 0" still holds.
- The event source's shaping must be `Identity`. A LUT over `[-1, 1]` means
  nothing on an absolute value, so anything else is refused.
- Connecting or disconnecting the source still crossfades over
  `PARAM_DECLICK` (256 frames).
- Audio sources stay offsets until PR 5 deletes them.
- The reference interpreter and the differential proptest get the same rule.

### 5.6 Fork and export

- **The fork copies, it never shares.** The lane's `ForkSource` gathers:
  - the current program (`Arc` clone);
  - the knob and range values;
  - the target address.

  It builds a fresh lane with its own `RtPublish`, so a later republish of
  the live lane does not reach the fork.
- **Fresh state** means:
  - the play clock not rolling;
  - no anchors, no piece in flight;
  - input layers at 0 until their sources send. Those sources are forked
    too: `upstream` walks event edges.
- **Export == live** then holds as §4.3 states. The fork renders the same
  points, the same values, and so the same port frames, from the play start.
- **The lane needs no tap from the audio thread.** Nothing the fork needs is
  audio-thread state.

## 6. Signal-driven sources

A follower or a note envelope depends on the audio or MIDI that arrives, so
no program can contain it. Each is an ordinary graph node with one event
output wired into the lanes of the targets it modulates.

| Node | Inputs | Output | What it sends | Cost |
|---|---|---|---|---|
| `EnvelopeFollowerNode` | audio (any width; the peak or RMS over channels) | 1 event output, capacity 256 | On the play-clock grid (`CONTROL_STEP`): at grid point `g`, a linear piece from `env(g − step)` to `env(g)` over `[g, g + step)`, addressed `ParamAddr::Id(source.0)`, raw in `[0, 1]`. Attack and release are `Seconds` (one-pole, per frame). | `Moderate` (per-frame envelope) |
| `NoteEnvelopeNode` | 1 MIDI event input | 1 event output | An ADSR's stages as shaped segments (linear, or `Exp` for exponential stages) at note-on and note-off and at each stage boundary: exact, with no grid. Mono, last-note priority, retrigger on note-on. | `Light` |

- **The follower runs one grid step late** (32 frames, 0.67 ms). It sends a
  piece only once its audio is in, so its output depends only on audio up to
  each grid point. That makes it independent of block boundaries and equal
  between live and export.
- **That lag is musical delay, not latency.** It is not reported to PDC
  (CLAUDE.md: "Musical delay is not latency").
- **One source can feed many lanes.** The output fans out to every lane it
  modulates, and each lane's program has an `InputLayer` with its own
  `Amount` for that `SourceId`. "Sidechain the pad and the bass from the
  kick" is one follower node.
- **A cycle is refused, never compiled.** A follower that reads a bus
  downstream of its own target makes a cycle. The compiler refuses it
  (`CompileError::Cycle`), and `bevy-tutti`'s reconcile reports it by route.
  An event feedback edge would delay it by `MaxBlock`, which differs between
  live and export, so feedback modulation stays out of scope (§12).

## 7. `tutti-mod` as a pure compiler

```rust
// tutti-mod

pub struct Declarations<'a> {
    pub sources: &'a [(SourceKey, Source)],
    pub routes: &'a [(RouteKey, Route)],
    /// Per target: its range and its automation, if any. The knob is not
    /// here: it is a cell (§4.2).
    pub targets: &'a [(TargetKey, TargetDecl)],
}

pub enum Source {
    Lfo { wave: Wave, clock: Clock },
    /// A constant the user moves; routed like an LFO. Moving it republishes
    /// the programs it reaches.
    Macro { value: f32 },
    Follower { attack: Seconds, release: Seconds },
    NoteEnvelope { adsr: Adsr },
}

pub struct Route {
    pub source: SourceKey,
    /// A target param, or another route's depth (`depth_by`).
    pub to: RouteTo,
    pub depth: Depth,
    pub polarity: Polarity,
    pub curve: CurveType,
}

pub struct Compiled {
    /// One program per target that has a route or automation. A target
    /// absent here has no lane.
    pub programs: BTreeMap<TargetKey, ControlProgram>,
    /// A node for each signal source, with its `SourceId`; the host builds it
    /// (`tutti-nodes` takes the declaration: `EnvelopeFollowerNode::from_decl`).
    pub nodes: BTreeMap<SourceKey, SourceNode>,
    /// Event edges: source node → target lane.
    pub edges: BTreeSet<(SourceKey, TargetKey)>,
}

pub fn compile(decl: &Declarations<'_>) -> Result<Compiled, CompileError>;
```

- **Pure.** It holds no clock, no state, no `RtPublish` and no runtime.
  Equal declarations compile to equal output.
- **Stable layer ids.** `LayerId` is derived from `(RouteKey, depth-by
  RouteKey)`, so a republish keeps each layer's identity (§4.3) and the sum
  order (it fixes M11).
- **Refusals, by name:** a program past `MAX_LANE_LAYERS` or
  `MAX_LANE_INPUTS`; a range with a NaN bound; a `depth_by` chain more than
  one deep (an LFO's depth moved by an LFO whose depth is moved by an LFO).
- **Diffing is the caller's.** `Compiled` is `Eq`, so a host republishes
  only the programs that changed.

**Which modules survive:**

| Module (`crates/core/tutti-mod/src`) | Fate |
|---|---|
| `lfo.rs` (`Lfo`, `SampleHold`, `RandomState`), `modulator.rs` (`Modulator`) | **Kept**: `tutti-nodes`' audio-rate `LfoNode` and `ModulatorNode` use them |
| `shape.rs` (`shape`, `curve_apply`, `LfoShape`, `Polarity`; `fold` deleted) | **Kept**: the one shaping convention |
| `curve.rs` (`Curve`, `CurveSegment`) | **Kept**, moved out of `routing` into the floor |
| `curve_source.rs` | `hash_bipolar` and `BeatLfo`'s position-derived random move into the program's `Wave`. `ShapedCurve`, `EdgeShape`, `CurveModulator` and `BeatLfo` are **deleted**. |
| New: `program.rs`, `compile.rs` | the program (§4) and the compiler (above) |
| `driver.rs`, `layered.rs`, `matrix.rs`, `mod_params.rs`, `param.rs`, `router.rs`, `routing.rs`, `target.rs`, `id.rs` | **Deleted**, along with the `routing` feature and its `dashmap`, `arc-swap` and `atomic_float` dependencies |

## 8. `bevy-tutti`: declarations that compile to lanes

### 8.1 The API

```rust
fn setup(mut mods: Mods, filter: Single<SvfRef>, pad: Single<GainRef>, kick_bus: Single<BusRef>) {
    let wobble = mods.lfo(Lfo::sine().rate(BeatDuration(0.5)));
    mods.route(wobble, filter.cutoff(), Depth(0.3));
    mods.route(mods.follower(*kick_bus), pad.gain(), Depth(-0.6)); // sidechain
}
```

(The decisions' sketch writes `Beats(0.5)`. The unit is `BeatDuration`.)

- **`Mods` is a `SystemParam`** over `Commands`. `lfo`, `macro_`,
  `follower` and `note_envelope` spawn a `ModSource` entity and return its
  `Entity`. `route` spawns a `ModRoute` entity.
- **`filter.cutoff()` is a `ParamRef`** `(Entity, ParamAddr)`. A typed node
  reference (`SvfRef` here) supplies the accessor, and
  `ParamRef::plugin(entity, id)` names a plugin parameter.
- **The components stay the user-facing nouns:**
  - `ModSource { kind }`;
  - `ModRoute { source, target: ParamRef, depth, polarity, curve, enabled }`;
  - `ModRange { param, min, max }`, which has no `base`;
  - `Automation { target: ParamRef, curve: Arc<dyn Curve> }`.

  `ModDelivery`, `per_block`, `per_sample` and `ModParamRange.base` are
  gone: there is one path.
- **A custom source kind** implements
  `ModSourceKind::declare(&self) -> tutti_mod::Source`. It is periodic (a
  `Wave::Drawn`) or a node.

### 8.2 The reconcile step

One system, `modulation::reconcile`, in `GraphReconcileSystems::Spawn`:

1. **Collect** the declarations into `tutti_mod::Declarations`. Each key is
   an `Entity`.
2. **Compile**, and diff against the last `Compiled` (a `CompiledMods`
   resource).
3. **Apply the difference:**
   - **A new target** gets a `ControlLaneNode`, inserted through
     `AudioGraphRes`, with:
     - its knob seeded from the param's current value;
     - its range from `ModRange`, or the param's own range;
     - its event output wired either to the target's param port
       (`connect_param`) or to the plugin node's event input
       (`EventFeeds`).
   - **A changed program** is republished (`LaneControls::publish`). That is
     not a commit.
   - **A target without a program** has its lane removed, and its port
     crossfades back to the node's control.
   - **A new signal source** gets its node (`*::from_decl`) and its edges.
     An edge a source no longer has goes, and so does a node whose source
     entity went (it fixes M10).
4. **A range edit** (`Changed<ModRange>`) is `LaneControls::set_range`: a
   store, with nothing compiled or committed.

**What triggers what:**

| Edit | Cost |
|---|---|
| Depth, polarity, curve, rate, wave, adding or removing an LFO route | recompile that target; republish one program. No commit, no phase reset (M2). |
| Adding a follower route | recompile; republish; one event edge (a commit). The lane's shape is unchanged. |
| Range | a store |
| Knob (`AudioParam<U, P>`, `write_param`) | the node's control **and**, when a lane exists, `LaneControls::set_knob`: two stores. The node's control keeps the knob for when the lane goes. |
| Automation curve | republish |

**The knob has one writer** (it fixes M9). `write_param` loses its modulated
branch and `set_param_snapshot`. `ParamSet` loses `set_authored` and
`authored`, because the live cell is only ever the knob, so a fork reads it
directly.

**A plugin parameter's knob** is the plugin's own value:

- **Seed.** The knob is seeded when the lane is created, from the value
  `bevy-tutti` already mirrors from the plugin.
- **GUI edits.** An edit in the plugin's GUI reaches the host through
  `ParameterChangeSink`. For a parameter with a lane, the reconcile writes it
  into the lane's knob. That moves the base under the modulation.
- **Under automation the edit is overridden**, as it is in any DAW's
  automation read mode, once the curve next moves (§9.3).

**Automation recording.** A take no longer installs `Recorder` as a layer.
While a touch or latch take is open, the reconcile republishes the program
with `automation: None`, so the knob (the gesture) drives the param. On
release it republishes with the finished envelope.

## 9. Plugin delivery (PR 1)

This PR stands alone and can land first. It fixes M4 and M5, and it is the
delivery the lanes need.

### 9.1 Per-parameter tables, sized at bind

**Today.**

- A chunk's `ParameterChanges` holds at most 16 queues inline. A queue is
  found by linear search, and past 16 a parameter is dropped and not counted
  (`automation_node.rs:249-270`, `graph_node.rs:297`).
- The node replaces its pending payload at each chunk (`graph_node.rs:236`)
  and moves it out (`:328-330`).
- The IPC client then assigns it over a pooled payload
  (`crates/plugin/tutti-plugin/src/host/ipc_client/audio/mod.rs:301`).

Any heap capacity there would be freed on the audio thread, which is why the
cap exists.

**After.**

- **The count comes with the load.** `LoadedPlugin` gains
  `param_count: u32`, appended last with `serde(default)`. The server fills
  it at load, when it has the list anyway. This bumps `PROTOCOL_VERSION`
  (`crates/plugin/tutti-plugin/src/protocol/mod.rs:33`, 19 → 20). There is
  no extra round trip, and no blocking call in `bind` or `prepare`.
- **`PluginClient::bind` builds a `ParamTable` on the control thread.** It is
  a fixed-capacity open-addressing map from `ParamAddress` to a slot,
  holding `next_power_of_two(2 × max(param_count, 16))` entries. Each slot
  holds:
  - `last_sent: u32` (`f32` bits; `NaN` means "never sent");
  - `queue: u32` (this chunk's index in `changes.queues`, or none).

  An address not seen yet claims a free slot. Nothing allocates, because the
  capacity is preallocated.
- **The node's `ParameterChanges` is reserved to `param_count` queues at
  bind.** So is every payload in the client's pool. The node clears its
  pending changes **in place** at each chunk instead of replacing them. The
  client **swaps** its changes with the pooled payload's instead of
  assigning them. Capacity then cycles between the node and the pool, and
  nothing is freed or grown on the audio thread.
- **`SegmentPoints` sizes its slots from `param_count`** too: no longer
  `MAX_SEGMENTS = 1024` (`segment_points.rs:54`).
- **The type stays `SmallVec<[ParameterQueue; 16]>`** and is reserved past
  its inline size. Its serde form is a length-prefixed sequence, exactly as
  before, so the wire is unchanged for `ParameterChanges`. PR 1 pins that
  with a byte-equality test. Only `LoadedPlugin` changes on the wire.
- **An address past the table** (a plugin whose parameter list grew after
  load) is dropped and counted in a `PluginControls::dropped_params()`
  counter. It is never silent. See the open questions.

### 9.2 Changes only

`add_point` becomes a method on the table:

- **A point is added only when its value's bits differ from the slot's
  `last_sent`.** Then `last_sent` is updated.
- **The end of a segment is always a point when it changes.** A segment's
  points (`SegmentPoints`) go through the same filter, so a constant
  segment, or a `Hold` piece re-sent every block, sends nothing after its
  first point.
- **`last_sent` is cleared** (every slot back to `NaN`) on `reset`,
  re-prepare, fork, and a plugin state or preset load. The control side bumps
  an epoch cell that the node reads once per block. The next value after any
  of these is then sent even if it equals the old one: after a preset load,
  automation and modulation re-assert.

Changes-only applies to every producer: the lane, and, until PR 5,
`PluginAutomation`. A ramp (`ParamRamp`) is a point at its target, as
before.

### 9.3 The plugin's own GUI

- **Nothing unchanged is ever re-sent** (it fixes M5). An edit in the
  plugin's GUI stands until the host's value for that parameter next
  **changes**.
- **An unautomated, unmodulated parameter has no lane, so it is never
  written.**
- **An automated parameter is written again when its curve moves.** That is
  automation-read behaviour.
- **A modulated parameter takes the GUI edit as its new knob** (§8.2).
- **The limit:** with values only (CLAP `PARAM_MOD` is out of scope), the
  plugin's GUI shows base + modulation. A drag during modulation therefore
  sets the knob to the dragged value, and the modulation adds on top of it.
  `PARAM_MOD` is the real fix, later.

### 9.4 The server side

The server already decodes every block into a fresh
`Box<ProcessAudioData>` (`bincode::deserialize`,
`crates/plugin/tutti-plugin-server/src/transport.rs:88`), so it was never
allocation-free per block. PR 1 does not change that. What PR 1 adds is that
no format host's own lists grow past activation:

| Format host | Today | PR 1 |
|---|---|---|
| VST3 (`tutti-vst3-host`, `com/param_changes.rs:50-67`) | grows its `ParamValueQueueImpl` list the first time a block carries more queues than seen before | reserve `param_count` queue slots at activation (the SDK host's `setMaxParameters`) |
| CLAP (`tutti-plugin-server/src/loaders/clap.rs`) | input-event scratch reserved to 256 (`tutti-clap-host/src/instance/lifecycle.rs:28`) | reserve `256 + param_count × MAX_POINTS` |
| AU (`loaders/au.rs:726-735`) | applies each queue's last point; no list | unchanged |
| In-process VST2 (`format/vst2_in_process/node.rs:55-66`) | keeps MIDI only and drops parameter events | unchanged in PR 1; see the open questions |

### 9.5 Tests and gates

See [PR 1](#pr-1-plugin-parameter-delivery).

## 10. What gets deleted

Everything here goes in PR 5, after PRs 2–4 have moved every user to lanes.

| Crate | Deleted |
|---|---|
| `tutti-graph` | `ParamFrom::Audio`; `ParamShaping`, `ShapeLut`, `SHAPE_LUT_LEN`; `ParamSource` (a port's source is an `EventOut`); `MAX_PARAM_SOURCES` and the per-source arrays (a port has at most one source); `signature`'s source hashing (replaced by present or absent); `GraphInvalid::TooManyParamSources` and `UnsortedParamSources`; `DelayKey::ParamAudio`; the `Σ` in the fused step (`param.rs:11`). `GraphSpec::connect_param(at, from: EventOut)` loses its shaping argument. `ParamSet::set_authored` and `authored`; `ParamFork` reads the live cell. |
| `tutti-mod` | The `routing` feature and every item behind it: `ModTarget`, `AtomicTarget`, `LayeredCurve`, `ModBus`, `ModRouter`, `ModEdge`, `ModRoutingSnapshot`, `ModRoutingTable`, `ModPreFrame`, `ErasedModulator`, `Sourced`, `SourceRate`, `Rate`, `SourceClock`, `ModParams`, `ModMatrix`, `Route`, `RouteTo`, `TargetHandle`, `ShapedCurve`, `EdgeShape`, `CurveModulator`, `BeatLfo`, `LayerKey`, `ModTargetId`, `fold`. Its `bevy` feature then reflects only the vocabulary. |
| `tutti-nodes` | `ParamModShaping` (`param_mod.rs`); the re-exports `AtomicTarget`, `BeatLfo`, `CurveModulator`, `LayerKey`, `LayeredCurve`, `ModParams`, `ModTarget` (`lib.rs:93-95`); `AutomationLaneNode::for_param` and `for_foreign` (now `ControlLaneNode`'s). `AutomationLaneNode::new`, the audio-output lane for meters and audio param sources, stays. |
| `tutti-plugin` | `PluginAutomation`, `AutomationControls`, `AUTOMATION_EVENT_CAPACITY`, `TimedParam`, `PluginParamTarget`, `LfoCurve`, `LfoOffset`, `OffsetCurve`, `param_automation_source.rs`'s `stride_for` and `MAX_POINTS` (they move to `segment_points.rs`, their one user); `PluginControls::automation` and `param_target`; `impl ModParams for PluginClient` (`host/node/mod.rs:650`). |
| `bevy-tutti` | `modulation/driver.rs`, `target.rs`, `audio_rate.rs`, most of `source.rs`; `ModulationMatrix`, `ModTargetRegistry`, `ModBusRes`, `ModParamsHandle`, `ModRateCell`, `CollectedModSources`, `ModSourceSystems`, `AudioRateRoutes`, `ModDelivery`, `ModParamRange` (becomes `ModRange`, without `base`); `write_param`'s modulated branch, `set_param_snapshot`; `plugin_bind_params`, `PluginParamsBound`, `PluginAutomationNode`, `remove_plugin_automation`; `AudioGraphRes::set_param_mod`'s source list. |
| Docs | doc 013's "Modulation does not reach a fork" (lines 1193-1200) and "Item 6 landed" become history notes; CLAUDE.md's "Control-rate modulation resolves on the same set; a sink no node owns … `ModTargetRegistry::insert_target`" line is rewritten. |

**Tests that go with what they test.** PR 5 names each one with its
disposition, and never removes one silently. A property still meaningful is
ported to the lane path:

- `tutti-nodes`' `param_mod_oracle.rs` (LUT against the old chain) is
  obsolete; its shaping-convention check moves to `tutti-mod`'s compiler
  tests;
- `param_set_routes.rs` is ported;
- `bevy-tutti`'s `mod_value_path.rs`, `mod_audio_rate.rs` and
  `graph_value_modulation.rs` are ported to render-level assertions on lanes;
- `tutti-polysynth`'s `AtomicTarget` route tests are ported to `ParamSet`
  writes, which is what they pin.

## 11. Migration notes and CHANGELOG

Each PR logs its own entry under `[Unreleased]`, in the repo's "Was | Now"
style.

| PR | CHANGELOG | Breaking? |
|---|---|---|
| 1 | A plugin receives a parameter only when its value changes. Any number of parameters arrives (no cap at 16). `LoadedPlugin::param_count`; `PROTOCOL_VERSION` 20; `PluginControls::dropped_params`. | Behaviour: a plugin no longer gets unchanged values every block. Wire: host and server must match (they ship together). |
| 2 | `ControlLaneNode`, `LaneControls`, `ControlProgram` and friends; `CONTROL_STEP`; `LANE_EVENT_CAPACITY` 128 → 256; `RangeCell` in `tutti-types`. **A param port's event source replaces its base** (at most one per port, `Identity` shaping). | **Yes.** An event-driven port reads the stream's value, not base + value. Migration: wire automation as before. It now means the value, so remove any knob-to-zero workaround. Code that summed two event sources into one port sums them in a node, or in one lane. |
| 3 | `tutti_mod::compile`, `Declarations`, `Compiled`. `Curve` and `CurveSegment` leave the `routing` feature. | No: additive. |
| 4 | `bevy-tutti` modulation compiles to lanes: `Mods`, `ModRange`, `Automation`, `ParamRef`. `ModDelivery` and `ModParamRange.base` are gone. Plugin targets are lanes. | **Yes.** `.per_block()` and `.per_sample()` are removed (one path). `ModParamRange::with(param, base, min, max)` becomes `ModRange::new(param, min, max)`, and the base is the knob (`AudioParam`). **Depth is span-scaled everywhere**, so a route written for the old `PerSample` tier is `max − min` times stronger. Divide its depth by the span to keep the old sound. |
| 5 | The deletions of §10. | **Yes.** Every deleted item. The migrations: `ModMatrix` or `AtomicTarget` users declare routes (or call `tutti_mod::compile`); `connect_param(at, from, shaping)` becomes `connect_param(at, event_out)`; `ParamSet::set_authored` becomes `set`; `PluginControls::automation` becomes a `ControlLaneNode::for_foreign` per parameter. |

The `audio-automation` pin and the bevy 0.19 line are unaffected.

## 12. Out of scope

- **Audio-rate modulation** (FM, ring modulation). A node takes it on its
  own audio input. `LfoNode` stays an audio generator for that.
- **Per-voice modulation.** It is the instrument's job (the polysynth's MPE
  today), and CLAP per-note modulation is later.
- **CLAP `PARAM_MOD`.** Values only, for now (§9.3 states the GUI
  consequence).
- **An LFO's rate modulated by another LFO.** It would need a generative
  (integrating) layer type. Add it only if wanted. (An LFO's *depth*
  modulated by another LFO is in scope: `depth_by`.)
- **Feedback modulation** (a source downstream of its own target). The
  compiler refuses it (§6).
- **Smoothing modulation itself.** There is no slew per route. A knob or
  program change glides over at most one control step (§4.2), and nothing
  else is smoothed.

## 13. The campaign

Each PR is green on its own, and none is merged by its author. Every new
test carries its mutation record (CLAUDE.md, "Mutation-test new tests").
Bit-for-bit comparisons are serial against parallel, and live against fork,
on one target only. Anything that calls libm (a sine, `powf`, `exp_m1`) is
asserted analytically with a tolerance, or as a golden digest on its
recording target.

### PR 1: plugin parameter delivery

**Scope.** §9:

- `LoadedPlugin::param_count`, and the protocol bump;
- `ParamTable`, the in-place clear, and the swap with the pool;
- changes only, with the epoch clear;
- `SegmentPoints` sized from the count;
- the server's reservations;
- `dropped_params`.

It is independent of PRs 2–5.

**Tests.**

- **More than 16 parameters all arrive.** 200 parameters driven into the
  reference CLAP plugin each reach their value, where today parameter 17 and
  up are dropped.
- **Changes only.**
  - A constant lane sends exactly one point per parameter across 100 blocks.
  - A moving lane sends no point whose bits equal the last sent.
  - After `reset`, and after a state load, the first value is sent again.
- **A GUI edit survives.** Set a parameter plugin-side while a constant lane
  holds another value. It is unchanged after 50 blocks, then overwritten on
  the lane's next change.
- **No allocation.** `clap_node_no_alloc` with 200 moving parameters under
  `assert_no_alloc`, serial and on `GatedPool`, covering chunk hand-off and
  pool swaps.
- **Wire.** `ParameterChanges` with 20 queues serialises to the same bytes
  through `SmallVec` and through a `Vec`, and the server decodes it into a
  table that does not grow.
- **VST3.** `refill_from_changes` with `param_count` queues allocates nothing
  after activation (the existing `refill_from_changes_steady_state_is_allocation_free`,
  extended).
- **Overflow.** An address past the table is counted, not written.

**Exit.** M4 and M5 are closed. M6 still holds, because `PluginAutomation`
still sends only while rolling; PR 5 deletes it. Nothing on the audio thread
allocates for any parameter count up to `param_count`.

### PR 2: the control lane node

**Scope.**

- `ControlProgram` and its evaluation in `tutti-mod` (ungated), with `Curve`
  and `CurveSegment` moved to the floor;
- `RangeCell` in `tutti-types`;
- `ControlLaneNode` and `LaneControls` in `tutti-nodes`: §5's algorithm, the
  play clock, anchors, exact mode, overload thinning, the fork;
- `EnvelopeFollowerNode` and `NoteEnvelopeNode`;
- `tutti-graph`'s port rule (§5.5), with the reference interpreter, the
  verifier and the proptest;
- `AutomationLaneNode::for_param` and `for_foreign` become thin constructors
  of `ControlLaneNode` (deleted in PR 5);
- doc 013's pointer to this doc updated with a "landed" note.

**Tests.**

- **Export == live, bit for bit**, on a probe node that copies its
  `Io::param` to an output, for:
  - (a) `Editor::fork` before the first block;
  - (b) live stopped, play from beat X on a frame that is not a multiple of
    `CONTROL_STEP`, against a `ForkMode::Offline` fork from beat X;
  - live in ragged blocks, the export in 1024-frame blocks, with every layer
    kind: beats, seconds, drawn, random, `depth_by`, an input from a
    follower on a test tone, and automation.
- **Serial == parallel, bit for bit.** 200 lanes into 25 probe nodes (8
  ports each) against pools of 2, 3, 4 and 8, every block schedule, with
  republishes and knob and range moves between blocks.
- **No allocation with 200 targets.** The same graph under
  `assert_no_alloc`, serial and on a gated pool, with a republish of every
  program between gated blocks. That pins the reader's path; the publish is
  the control thread's.
- **LFO phase survives a depth edit.** A seconds sine at 3 Hz, depth doubled
  at frame F. Every sample after F equals twice the pre-edit formula at the
  same play time. That is analytic, within 1e-6 of the swing (libm `sin`).
  Mutation: reset the play clock on republish, and the test fails.
- **A rate edit is continuous.** No step at the edit frame larger than one
  frame's slope.
- **Automation replaces the knob.** Knob 1000, automation constant 500: the
  port reads 500.0 exactly. Add a square LFO of ±100 (IEEE-exact): 400.0 and
  600.0 exactly, edges on their frames. Remove the automation: 900.0 and
  1100.0. Range [0, 550]: 550.0.
- **Exactness of the grid.**
  - A triangle LFO (corners at its peaks, linear between) renders exactly at
    every frame.
  - A sine stays within `(2π f h)² / 8` of its depth.
  - A pure-automation program reproduces `automation_segments.rs`' frame
    values (exact mode).
- **Transport.** For beat layers: seek, loop wrap and tempo change inside a
  block; stop holds the value; start restarts seconds layers at their phase
  offset; a seconds layer keeps moving while stopped.
- **Capacity.** A 4096-frame block with 120 corners drops nothing
  (`Executor::dropped_events` is 0). A 16384-frame block thins and counts.
- **Port rule.** In `tests/param_mod.rs`: an event source replaces the base;
  a second event source is refused; a LUT on an event source is refused;
  connect and disconnect declick; the differential proptest is regenerated
  with value semantics.
- **Inputs.** A stub source sending segments gives an offset of exactly
  `depth × span × x` for IEEE-exact `x`, and a follower's piece lags exactly
  one step.

**Exit.**

- M8 and M11 are closed.
- The lane is the only producer the new path needs, measured and declared at
  its cost class.
- Doc 013's item 6 notes the rule change.

### PR 3: `tutti-mod` as a pure compiler

**Scope.** §7: `Declarations`, `Compiled`, `compile`, the refusals, and
stable `LayerId`s. Macros, followers, note envelopes and `depth_by` routes.
The `routing` feature is untouched, and still used by `bevy-tutti` until
PR 4.

**Tests.**

- **Purity.** Equal declarations give equal `Compiled`, and a shuffled input
  order gives equal output.
- **One depth convention** (it closes M1). For every source kind, polarity
  and curve, the offset at raw ±1 equals `shape(±1, depth, polarity, curve) ×
  span`, through the compiled program evaluated by `value_at`.
- **Locality.** A depth edit changes only that target's program, and keeps
  its `LayerId`s.
- **Refusals.** Each one is named and covered.
- **Macros.** Moving a macro changes exactly the programs it reaches.

**Exit.** Compiling any declaration `bevy-tutti` can express today needs no
runtime, and the crate builds with `--no-default-features` and with
`--features bevy`.

### PR 4: `bevy-tutti` declarations to lanes and edges

**Scope.** §8: `Mods`, `ModRange`, `Automation`, `ParamRef`; the reconcile
step; knob, range and plugin-GUI paths; recorder takes; plugin targets as
lanes. The old driver and tiers are no longer scheduled, but they are not
yet deleted.

**Tests.**

- **The decisions' example** compiles and renders. The cutoff is exactly
  knob + 0.3 × span × sin at analytic frames (tolerance), and the pad's gain
  dips with the kick.
- **A depth slider keeps phase at render level** (it closes M2).
- **Export == live in an app**, bit for bit, on a native param and on the
  reference CLAP plugin's gain. It closes M3: it fails by construction on
  `main`.
- **A despawned source removes its node and its layer** (it closes M10).
- **Edit costs.** A range edit sends no plan. A depth edit sends no plan. A
  follower route sends one commit.
- **The knob has one writer** (it closes M9). A knob written after a route
  edit is still the knob.
- **A GUI edit becomes the knob.** A plugin GUI edit on a modulated
  parameter moves the knob.
- **A take suspends automation**, then republishes the envelope.
- **Cycles.** A follower downstream of its target is refused, by route.

**Exit.** `bevy-tutti` modulation runs on lanes only, and M1, M2, M3, M6,
M9, M10 and M12 are closed on the `bevy-tutti` path.

### PR 5: delete the old system

**Scope.**

- §10's deletions;
- the ported and named test dispositions;
- doc 013 (the fork note, item 6's history, a pointer here);
- CLAUDE.md's modulation lines ("Control-rate modulation resolves on the
  same set …" and the `ModTargetRegistry` mention);
- the CHANGELOG with migration notes.

**Tests.** The full workspace (`cargo nextest run --workspace`, doctests),
`just check-windows`, and the canonical-paths gate. Every suite ported in
PRs 2–4 stays green. Nothing is removed without its named disposition in
the PR body.

**Exit.**

- `grep` finds none of §10's items.
- `tutti-graph`'s param port holds at most one event source.
- `tutti-mod` has no runtime.

## 14. Open questions

1. **A plugin whose parameter list grows after load** (CLAP `rescan`, a
   VST3 `restartComponent` that adds parameters). PR 1 sizes the table from
   the load and counts what does not fit. Should a grown list re-bind the
   node (a replace), or should the table carry fixed headroom? It depends on
   how often real plugins grow, which the code cannot say.
2. **The in-process VST2 node drops parameter events**
   (`format/vst2_in_process/node.rs:55-66`). It could take lanes as
   `setParameter` calls at chunk starts, which is block-accurate only. Is
   that wanted in PR 1, or is it a separate PR?
3. **The largest block a host can request.** §5.3's capacity argument
   assumes blocks up to 4096 frames, with thinning past that.
   `Prepare::max_block` has no stated upper bound in the code. If hosts use
   8192-frame blocks, `LANE_EVENT_CAPACITY` should be 512 instead.
