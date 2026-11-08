# 022: reverb as an interface

Tutti has two reverbs that share nothing: a fixed Freeverb (the General MIDI
send, PR #354) and a convolution node that needs an impulse response. Neither
is modelled: there is no "decay time", "pre-delay" or "damping" that means the
same thing in both, so no UI, preset, automation lane or test can talk about "a
reverb". This design models the domain in tutti's existing units and param
system, puts the processing behind a trait in `tutti-dsp` so algorithms are
interchangeable, and gives them one generic node and one contract test suite. It
follows the stretcher decision of 2026-11-07 (`clean-plan.md`, "Stretch modes: an
interface"): a trait first, an enum at most as a convenience, the choice explicit,
built and prepared on the control side.

Numbering: 019 (bounce), 020 (tempo map) and 021 (plugin hosting) are drafts on
other branches; `docs/design` on `main` ends at 018.

## Summary

- **The trait is a processing contract only** (§3): `prepare`, `process`,
  `reset`, `tail`, `latency`, `fresh`, with an associated `Controls` struct of
  plain tutti units per algorithm. No shared settings struct and no capability
  query: what an algorithm honours is the `ParamSet` of its node.
- **The model is the params** (§2): existing units (`Seconds`, `Hz`, `Db`,
  `Mix`, `Depth`, `StereoWidth`), existing `UnitParam` kinds, and two new kinds
  (`Decay`, `Diffusion`). No new unit.
- **What algorithms share is logic, as `tutti-dsp` building blocks** (§5.1): the
  RT60-to-loop-gain formula, band-damping design, pre-delay, early-reflection
  taps, the all-pass diffuser, width/decorrelation.
- **One generic node, `ReverbNode<R: Reverb>`** in `tutti-nodes` (§4), declaring
  only `R`'s params and its tail from `R::tail`.
- **Decisions of 2026-11-08** (owner): the graph re-reads the tail (§10, step 0b);
  fork is `fresh()` with preroll; `ReverbKind` ships in step 1 beside a second
  algorithm, the FDN; freeze comes with the FDN (§11).
- **Modulation holds up** with one convention and no code change (§9): a tail
  declared at the worst case of the node's own range, controls read once per block
  with coefficient interpolation, and `UnitParam` kinds appended.
- **Build order** (§8): kinds, the graph's dynamic tail, then the trait, suite,
  node and Freeverb, then the FDN with `ReverbKind` and freeze, then convolution,
  the plate and early reflections.

## 1. Current state

Line counts from `main` (`7c30ba335`) and PR #354 (`feat/soundfont-effects-nodes`).

| Piece | Where | Lines | Can express | Cannot express |
|---|---|---|---|---|
| `Freeverb` (8 combs + 4 all-passes per channel) | `tutti-dsp/src/reverb.rs` (#354) | 344 | One fixed room: mono in, stereo out, every figure a constant; `ring_out(floor)` bound | Any control: pre-delay, RT60, band decay, width, size, modulation |
| `ReverbNode` | `tutti-nodes/src/reverb.rs` (#354) | 105 | Wraps `Freeverb`; `ForkByClone`; `Cost::Moderate`; `Tail::Finite` from `ring_out(-60 dB)` | No params (`Controls = ()`), no mix |
| `Convolver` (partitioned FFT) | `tutti-dsp/src/convolver.rs` | 1940 | Any real room exactly; latency = one block; `fresh`, shared `IrSpectra` | Anything parametric: decay, size, damping, pre-delay mean editing the IR |
| `ConvolverNode` + IR helpers | `tutti-nodes/src/convolution/` | 1114 + 87 + 101 + 19 | `mix`, `gain`; per-channel IR layouts (`IrChannelConfig`); dry aligned to latency (`DryAlign`); exact tail `ir_len - 1`; `generate_room_ir` (exponential-noise IR) | Live decay or damping; IR is read-only |
| `DelayLine`, `SweptLines` | `tutti-dsp/src/{delay,swept_delay}.rs` | 268 + 135 | Fractional taps, modulated reads | A network of them |
| all-pass, interpolation | `tutti-dsp/src/{allpass,interp}.rs` | 55 + 46 | First-order all-pass sweeps, Hermite | A Schroeder/Dattorro all-pass with a delay line |
| SVF, crossover (LR), ladder | `tutti-dsp/src/filter/` | 409 + 949 + 178 | Shelving and low-pass for damping, 4th-order band split | Per-line absorption design (small closed form, §5.1) |
| `Ramp` | `tutti-dsp/src/control.rs` | 162 | A per-block linear ramp | n/a |
| `Session::add_general_midi` | `tutti-session/src/general_midi.rs` (#354) | 165 | Unit sends -> `ReverbNode::new()` -> sum | Another reverb |
| `render_load` bench | `tutti-session/benches/` | n/a | A convolver as the scene's reverb | n/a |

No other reverb-like code exists in the workspace (plugin hosts load third-party
reverbs; separate). `UnitParam` already has `RoomSize`, `Damping`, `Wet`,
`DelayTime`, `Feedback`, `Rate`, `Depth`, `Cutoff`, `GainDb`, `StereoSpread`.
It has no kind for a decay time or a diffusion.

What is missing is not an algorithm. It is a shared vocabulary, a decay model
(RT60 per band, not a feedback constant), a tail that follows the controls, and
a place where a test can say "this reverb decays in 2.0 s".

## 2. The domain model

A reverb is a (nearly) linear system whose impulse response has an onset, an
early part, a tail and an output stage. Each parameter is a `Param<U>` of an
existing unit, addressed by a `UnitParam` kind and a `UnitAddr` index when a node
has several of a kind. A node declares only the params its algorithm honours (a
node without the param takes nothing), and `Shape::with_param_addrs` declares
which are modulatable. That is the whole capability story.

| Group | Parameter | Unit | `UnitParam` (index) | Common? |
|---|---|---|---|---|
| Onset | pre-delay | `Seconds` | `DelayTime` | all (a building block) |
| Early | early level | `Db` | `GainDb` (1) | those with early reflections |
| | early pattern | build config (a room shape) | none | the early generator, and stages built from it |
| Tail | decay, mid / low / high RT60 | `Seconds` | `Decay` (0 / 1 / 2), **new** | mid: all; low/high: FDN; high alone: plate, Freeverb (as damping) |
| | band crossovers | `Hz` | `Cutoff` (0 / 1) | FDN |
| | damping (high-band loss without RT60) | `Mix` | `Damping` | Freeverb |
| | size | `Mix` | `RoomSize` | all but convolution |
| | diffusion | `Mix` | `Diffusion`, **new** | FDN, plate |
| | modulation rate / depth | `Hz` / `Depth` | `Rate` / `Depth` | FDN, plate |
| Output | stereo width | `StereoWidth` | `StereoSpread` | all with a stereo output |
| | wet gain | `Db` | `GainDb` (0) | all |
| | insert mix | `Mix` | `Wet` | the node, not the algorithm (§2.3) |
| Hold | freeze | `bool` | **new** `Freeze`, with the FDN only | FDN |

### 2.1 Units: no new unit

CLAUDE.md's "Adding a unit" rule needs a distinct range *or* a distinct algebra.

- **Size** is a `0..1` position that each algorithm maps onto its own range of
  delay lengths. Its algebra is `Mix`-like (a position on a scale; half of a size
  is not a size), so it does not earn a unit. `Mix`'s doc says "a blend position"
  and becomes "a position on a `0..1` scale, a blend among them"; the doc change
  is part of step 0. It is `Mix`, with that caveat, not `Feedback` or `Depth`.
- **Diffusion** is also a `0..1` coefficient and likewise `Mix`.
- **RT60 and pre-delay** are `Seconds`; a negative or non-finite value is
  refused by the param, not by a new type. The checked shape lives where the
  value is built: `Decay`'s `Controls` field is a plain `Seconds`, clamped to
  `[floor, ceiling]` once per block by the node (§4.2), the same way
  `ModDelayNode` clamps a delay time.
- **Early-reflection geometry** (a shoebox in metres) is build config, a small
  checked struct with `f32` metres on the control side, with a comment that no
  length unit exists and the shape never reaches the audio thread.

### 2.2 Param kinds: `Decay` and `Diffusion`

Reusing an existing kind for RT60 would name it wrongly: `DelayTime` is a time
but means a delay, and `Release` is a dynamics envelope. A UI or modulation
router reading a kind needs to know "this is a reverberation time". Propose
`UnitParam::Decay` (unit `Seconds`, index 0 mid, 1 low, 2 high) and
`UnitParam::Diffusion` (`Mix`). `Freeze` is added only with the FDN. The
`UnitParam` table test in `tutti-types` (the `(UnitParam::RoomSize, 8)` list)
takes the rows. The alternative, reusing `Feedback` for diffusion, was rejected:
`Feedback` is a recirculation coefficient with a stability ceiling, which a
diffusion is not.

### 2.3 Wet/dry policy

**Recommendation: the algorithm is wet-only; the node owns mix.** `process`
overwrites its output with the reverberated signal at the wet gain. The node has a
`Mix` (`UnitParam::Wet`, default `WET`, i.e. a send) for insert use and aligns the
dry path to `R::latency()`, never to pre-delay (pre-delay is part of the wet
sound). A send bus is the normal use and the GM setup is one; a mix inside every
algorithm would be four copies of `Mix::blend` and four dry aligns; the existing
`DryAlign` in `convolution/node.rs` solves the alignment once and moves to the
shared node. It also makes "wet-only means no dry leak" a property of the trait.

### 2.4 Channel shape

`Shape::audio(in, out)` is fixed at node construction; width is runtime
(`ChannelLayout`), never a const generic. An algorithm reports its shape at
build. mono -> stereo is the send case (two decorrelated tails, `width` toward
mono); stereo -> stereo feeds each input to both tails through an input matrix;
N -> N is native to an FDN (N output taps) and to the convolver
(`IrChannelConfig`); others refuse the shape at build.

### 2.5 Freeze

Freeze (loop gain 1, input gated) makes the tail unbounded, which does not fit
`Seconds`. It comes with the FDN (step 1b): `UnitParam::Freeze` (`Mix`-free, a
`>= 0.5` toggle encoded as `Mute` is), a `freeze: bool` in the FDN's `Controls`,
and one more method on the trait, `fn holds(&self, controls: &Self::Controls) ->
bool { false }`. The node reports `Tail::Unbounded` through `report_tail` (§10)
when `holds` is true and `Tail::Finite(R::tail(..))` otherwise. Entering freeze
ramps the loop gain and the input gate, so it does not click.

## 3. The trait

A processing contract in `tutti-dsp`, shaped by the owner's direction. It names
no `Param`, no atomic, no graph type.

```rust
pub trait Reverb: Send + Sized {
    /// Plain values in tutti units, one struct per algorithm.
    /// Freeverb: { room: Mix, damp: Mix }. FDN: { decay: [Seconds; 3],
    /// crossovers: [Hz; 2], size: Mix, diffusion: Mix, modulation: { Hz, Depth },
    /// width: StereoWidth, pre_delay: Seconds, wet: Db }.
    type Controls: Copy + Send;

    /// Control side. May allocate. Sample rate, largest block, layouts.
    fn prepare(&mut self, setup: &Setup);

    /// Real time. Planar slices of equal length <= `setup.max_block`; overwrites
    /// `output` with the wet signal. Never allocates or waits.
    fn process(&mut self, controls: &Self::Controls, input: &[&[f32]], output: &mut [&mut [f32]]);

    /// Silence the state; the next block is as from a fresh instance.
    fn reset(&mut self);

    /// Time for the output to fall to `floor` once input stops, at `controls`.
    /// Includes pre-delay and the network's settle time; excludes `latency`.
    fn tail(&self, controls: &Self::Controls, floor: Db) -> Seconds;

    /// Processing latency only. Never pre-delay, never early-reflection spacing.
    fn latency(&self) -> Samples { Samples(0) }

    /// Whether the output never decays at `controls` (freeze). Default false.
    fn holds(&self, controls: &Self::Controls) -> bool { false }

    /// How heavy a block is, for the executor.
    fn effort(&self) -> Effort { Effort::Heavy }

    /// Same algorithm, build config and prepared setup, empty state.
    fn fresh(&self) -> Self;
}
```

| Method | Why |
|---|---|
| `Controls` | The owner's decision: no parallel settings struct. Each algorithm's inputs are exactly the params it honours, so an `Ignored` field cannot exist. tutti-dsp gets plain values per call (its rule), the node does the `Param` reads |
| `prepare(&Setup)` | Control side, like `Node::prepare` and the stretcher. Lines are sized here for rate and `max_block`; `Setup { rate: SampleRate, max_block: MaxBlock, input: ChannelLayout, output: ChannelLayout }` |
| `process(controls, ..)` | Controls arrive per call, so there is no stored state to race and no setter that must be real-time. Overwrite makes "wet-only" structural |
| `reset` | Seek and stop; compared to a fresh instance in the contract |
| `tail(controls, floor)` | The node declares its `Tail` from it. `RT60 * floor/-60` for the slowest band plus pre-delay plus the network's settle time. Freeverb's `ring_out` (a bound, not a measure) is the model |
| `latency` | `Samples`, wrapped in `Latency::new` by the node. A separate method from `tail` so "musical delay is not latency" is checked by type and by test: pre-delay moves `tail`, never `latency`. Default 0; the convolver returns one partition |
| `effort` | `Cost` is `tutti-graph`'s and `tutti-dsp` must not depend on it, so `Effort` is a three-variant enum here and the node maps it with one `match`. A method, not a const, because an FDN's cost depends on its line count. Default `Heavy`, as an undeclared node is |
| `fresh` | Fork rule from the stretcher decision. A fork starts from silence; the graph's `PrerollTable` warms it. `Clone` is not the answer: it would copy the convolver's state and, for a node holding a pool, share it |

`Sized` (for `fresh -> Self` and the associated type) means the trait is not
object-safe, which §4.1 accepts.

### 3.1 Who smooths

The node reads each `Param` once per block, so the controls a block sees are
block-constant; a param that is modulated gets a control lane per modulated
param, which is the existing per-sample path. Gain-like controls (`wet`, loop
gain derived from RT60) ramp in the algorithm across the block using
`tutti-dsp`'s `Ramp`. Anything that is a *length* (size, modulation, a pre-delay
change) must never jump, since a jump is a pitch glitch: the algorithm slews it
or crossfades two taps. So smoothing of values is the param system's, and
smoothing of what the value means (a delay length) is the algorithm's, because
only the algorithm knows it. The contract measures click-freeness (§6) rather
than assuming it. §9.2 settles how the existing nodes divide this.

## 4. The graph node

```rust
pub struct ReverbNode<R: Reverb> {
    inner: R,
    params: R::Params,          // the Param<U> cells this algorithm declares
    mix: Param<Mix>,            // UnitParam::Wet; default WET (a send)
    dry: DryAlign,              // the convolution node's, shared
}
```

`R::Params` is a small per-algorithm struct of `Param` cells plus a `fn
controls(&self) -> R::Controls` that reads them once per block (CLAUDE.md: the
node reads `Param`s, the dsp gets plain values). It is a second associated item
beside `Controls`, held by a helper trait in `tutti-nodes` (`ReverbParams`), so
`tutti-dsp` stays free of `Param`. `ReverbNode<R>` is `Node` + `ParamNode`
inserted through `param_parts`, declares only the params in `R::Params` through
its `ParamSet`, and `Shape::with_param_addrs` for the modulatable ones.

### 4.1 Generic, not boxed

| | `ReverbNode<R: Reverb>` (recommended) | `ReverbNode` over `Box<dyn ..>` |
|---|---|---|
| Trait | owner's shape; `type Controls` | needs an object-safe erasure: a second trait that erases `Controls` (`Any` or a `[f32]` param bag), which is the parallel settings struct the owner ruled out |
| Per-sample loops | monomorphic; no virtual call in the node | one virtual call per block, negligible |
| Params | each `R` declares exactly its own; no "ignored" case | one union param set, with an ignored case, or a runtime `ParamSet` |
| Fork | `R::fresh()`, concrete | `fresh` through a box (`Box<dyn>` has no `Clone`, the stretcher reason) |
| Runtime choice (project file, picker) | a closed `enum` over the handful of `ReverbNode<R>`, or a node-replace by kind | free |
| Compile size and registration | one `param_graph_node!` per `R` (3-5 lines each) | one |

Recommend generic: the cost is a registration line per algorithm and a runtime
chooser written as one enum or a match at the spawn site, and the benefit is that
the type states what is honoured. The runtime-choice cost is real: a picker must
spawn one node type or another. That is the spawn site's `match`, not engine
logic.

A **structural change** (a new IR, another algorithm, a different line count)
builds a new `ReverbNode<R>` on the control side and goes in through the
graph's existing `crossfade` / `crossfade_audio_node`; the audio thread never
swaps an owning handle or frees one. The node's `Fork` is `inner.fresh()` with
the same params, replacing `ForkByClone`; a fork starts from silence.

### 4.2 Shape, tail, status

- `Shape::audio(in, out)`, `.with_cost(inner.effort().into())`,
  `.with_latency(Latency::new(inner.latency()))`.
- **Tail:** `Shape::tail` is the *ceiling* (`R::tail` at the controls' maxima, or
  `Tail::Unbounded` for an algorithm that can freeze); the node reports the
  current tail each block with `io.report_tail(..)` (§10), from `R::tail` at the
  block's controls. The node clamps the live decay to its own `max_decay`
  build parameter (default 8 s), which sets the ceiling.
- `process` returns `Status::Modified`. Returning silent when mix is 0 and the
  state is empty is an optimisation after the first PR.

### 4.3 General MIDI (#354)

`Session::add_general_midi` keeps calling `ReverbNode::new()`, which becomes
`ReverbNode::<Freeverb>::general_midi()`: Freeverb at `{room: 0.5, damp: 0.5}`
as a send (`Mix::WET`). The GM numbers (room 0.5, damp 0.5, `FIXED_GAIN`) stay
constants of the `Freeverb` build; `Controls { room, damp }` are the same two
numbers Freeverb's own code scales (`room * 0.28 + 0.7`, `damp * 0.4`), so a
GM-valued `Controls` produces today's coefficients exactly and the render is
bit-identical by construction. A follow-up `add_general_midi_with` takes another
`ReverbNode<R>` (generic over `R`).

### 4.4 Bevy

`bevy-tutti` composes nothing new: `bevy_tutti::param_graph_node!(ReverbNode<Freeverb>)`
(one line per `R`) and `App::add_audio_param::<U, P>()` per row of §2's table;
the two new `UnitParam` kinds are the only `tutti-types` change. The adapter
rules in `scripts/check-adapter.sh` hold.

## 5. Implementations

Ranked by value over cost. Pure Rust, written from cited papers (no code derived
except Freeverb, already MIT-noticed); no C libraries.

| # | Algorithm | Source | Lines (est.) | Controls it takes | Cannot do |
|---|---|---|---|---|---|
| 1 | **Freeverb** (exists) | rustysynth `reverb.rs` (MIT, in header); Jezar's Freeverb | 344 now, +~60 for the trait | `{ room: Mix, damp: Mix }` (its native pair) | Everything else; bit-identical at GM values |
| 2 | **Convolution** (exists) | `tutti-dsp::Convolver` | adapter ~250 (convolver/node stay) | `{ wet, pre_delay, width }`; the IR is build config. `generate_room_ir` can build an IR from decay and size on the control side (a rebuild) | Live decay, size, damping, modulation. Latency one partition |
| 3 | **Feedback delay network** | Jot and Chaigne, "Digital delay networks for designing artificial reverberators", AES 90th, 1991; Jot 1997; Smith, *Physical Audio Signal Processing*, FDN chapter; Schlecht and Habets on feedback matrices | ~600 | `{ decay [Seconds;3], crossovers [Hz;2], size, diffusion, modulation { Hz, Depth }, width, pre_delay, wet }` and optional freeze | A real room's exact early pattern; sparse at very short RT60 and huge size |
| 4 | **Dattorro plate** | Dattorro, "Effect Design Part 1: Reverberator and Other Filters", JAES 45(9), 1997 | ~350 | `{ decay: Seconds (mid), damp, size, diffusion, modulation, width, pre_delay, wet }` | Three-band decay, N > 2 channels, freeze |
| 5 | **Early-reflection generator** | Allen and Berkley, "Image method for efficiently simulating small-room acoustics", JASA 1979 | ~250 | `{ level: Db, width, pre_delay }`; room shape is build config | A tail. Also a building block inside 3 and 4 |
| 6 | Velvet-noise reverb (later) | Järveläinen and Karjalainen 2007; Välimäki et al., "Fifty years of artificial reverberation", 2012 | ~200 | `{ decay [Seconds;3], wet, width }` | Needs stereo-decorrelation thought; no feedback, so no freeze or modulation |

Not planned: Schroeder (superseded by Freeverb), spring (nonlinear, own model),
shimmer (a pitch shifter in the loop). The FDN maps the domain most directly,
which is why it ranks above the plate; the plate is cheaper and a character
people ask for by name. They are independent.

### 5.1 Shared building blocks in `tutti-dsp`

Algorithms share logic, not a settings struct. Each is a pure function or small
struct over slices and plain values, tested by itself, and extracted when its
second user lands (not before):

| Block | What | First two users |
|---|---|---|
| `loop_gain(delay: Samples, rt60: Seconds, rate) -> f32` | `g = 10^(-3 d / (RT60 fs))`, the RT60-to-loop-gain relation. Freeverb's `ring_out` already uses its inverse | Freeverb tunable mapping (later), FDN, plate |
| `Absorption::design(delay, rt60 [low, mid, high], crossovers, rate)` | Per-line filter coefficients from the three RT60s: gains `g_dc`, `g_ny` as above, a first-order or two-shelf realisation over `filter/` | FDN, velvet |
| `PreDelay` | Fixed-length line with a slewed length, never jumping | all |
| `EarlyTaps::image_source(shape, ..)` / `EarlyTaps::process` | Tap table from a shoebox, and the tapped delay | early generator, FDN, plate |
| `Diffuser` | The Schroeder/Dattorro all-pass with a delay line | plate, FDN input |
| `Width` | Mid/side width and decorrelation of two wet signals | all with a stereo output |

An orthogonal feedback matrix (Householder or Hadamard) is the FDN's own.

## 6. The contract suite

One suite, `tutti-dsp/tests/reverb_contract.rs`, instantiated per implementation
by a `reverb_contract!` macro (the plugin-contract and codec precedent), so a new
algorithm is a line. It takes an algorithm's `Controls` constructors for the
cases ("a controls value with RT60 about X" is a function the macro is given).
Every figure is from physics or the spec, never another implementation's render.
A case needing a control an algorithm does not have is not written for it: the
macro takes the list of cases it applies to.

| Case | Figure | Mutation record |
|---|---|---|
| RT60 matches | Impulse, long render, Schroeder backward integration, T20 extrapolated, within the algorithm's declared tolerance (FDN 5%, Freeverb 25%) | scale loop gain by 1.02 |
| Band decay order | Band-filtered decays ordered as requested (FDN, velvet) | swap the low and high gains |
| Monotone energy | 10 ms windowed energy of the tail non-increasing after build-up within a tolerance | loop gain 1.0005 |
| Pre-delay | First non-zero output sample at `round(pre_delay fs)`; `latency()` unchanged by pre-delay | off by one in the line |
| Declared tail | Measured time to each floor (-40, -60, -90 dB) <= `tail(controls, floor)` | halve `tail` |
| Latency | `latency()` equals the measured shift of the first output sample for an algorithm with pre-delay 0 | return 0 |
| Reset | After `reset`, render bit-identical to a fresh instance's | skip one line |
| Fresh | `fresh()` renders bit-identically to a freshly built instance | share one line |
| Block independence | The same input in blocks of 1, 17, 64, 512 bit-identical (time-domain). Convolution: within round-off, declared | block-dependent ramp |
| Serial vs pooled | Node-level (`tests/parallel.rs` style): bit-identical to serial | n/a (executor's) |
| No allocation | `process` under `assert_no_alloc`, controls changing every block | n/a |
| Wet-only | A Dirac at 0 with pre-delay > 0: exact zero before the pre-delay; wet `-inf` dB is exact zero | add the input to the output |
| Width | mono in, stereo out: inter-channel `Correlation` below 0.3 at `StereoWidth` 1, 1.0 at 0 | give both tails one seed |
| Click-free | Step decay, size, wet and pre-delay mid-tail: max sample-to-sample jump <= steady-state bound x small factor | apply a length unslewed |
| Stability | 60 s of noise at extreme controls (max decay, max size, min of everything, a modulation extreme): finite, bounded, no growth; non-finite controls leave the last good ones | unity-plus loop |

Signal helpers (Schroeder integration, band energy) go in `tutti-dsp`'s
`test_support` (it exists). Measuring RT60 on Freeverb at the GM controls pins
the analytic figure the GM render keeps, with a tolerance; the render is not
compared with the old version as a reference.

## 7. Where things live

| Item | Crate | Layer | Check |
|---|---|---|---|
| `Reverb`, `Setup`, `Effort`, the building blocks (§5.1), `Freeverb`, `Fdn`, `Plate`, `EarlyReflections`, contract suite | `tutti-dsp` | 2 | Allowed deps only (`tutti-types`; `tutti-graph` dev-only, hence `Effort`); no atomics or `Arc`-published state; controls per call |
| `UnitParam::{Decay, Diffusion, Freeze}` | `tutti-types` | 1 | vocabulary only; no new unit |
| `generate_room_ir` | moves to `tutti-dsp` with the convolution implementation | 2 | an algorithm builds its own IR |
| `ReverbNode<R>`, `ReverbParams`, shared `DryAlign`; `ConvolverNode` kept, thinned | `tutti-nodes` | 4 | node holds only the adapter |
| `Session::add_general_midi` | `tutti-session` | 7 | call unchanged; Bevy-free |
| Registration | `bevy-tutti` | 8 | `param_graph_node!` lines; no engine logic |

`Freeverb` keeps its MIT header; the others are written from papers. CLAUDE.md's
`tutti-dsp` paragraph names the trait when step 1 lands.

## 8. Staged plan

Each step is a PR that passes `just ci`. #354 merges first.

| Step | Content | After | Rough size |
|---|---|---|---|
| 0 | `tutti-types`: `UnitParam::{Decay, Diffusion}` (ids 25, 26), the `PINNED` table, the `Mix` doc line | #354 (can start now) | ~80 lines |
| 0b | `tutti-graph` (+ `tutti-export`, `tutti-types`): the dynamic tail, §10; doc 013 change note | none (parallel with 0) | ~500 lines, ~60% tests |
| 1a | `tutti-dsp`: `Reverb`, `Setup`, `Effort`, the shared blocks the Freeverb needs, `reverb_contract!`, `Freeverb` as a `Reverb`; `tutti-nodes`: `ReverbNode<R>`, `ReverbParams`, fork by `fresh`, quiet-input counter and `report_tail`; bevy registration; GM unchanged; the export-fork test that pins `fresh()` + preroll | 0, 0b | ~1,200 lines, ~600 of them the suite |
| 1b | `tutti-dsp`: the FDN (absorption design with `lerp`, loop gain, orthogonal matrix, diffusion, modulation, freeze); `UnitParam::Freeze` (id 27); `ReverbKind { Freeverb, Fdn }` in `tutti-nodes` (spawns the matching node, nothing more); registration; the FDN exercises the three-band RT60 case, size, diffusion, modulation and the unbounded tail | 1a | ~1,000 lines |
| 2 | Convolution as a `Reverb` (`ReverbKind` gains a variant); `DryAlign` shared | 1a | ~400 |
| 3 | Plate (shares the diffuser and blocks from 1b) | 1b | ~500 |
| 4 | Early-reflection generator, then as the early stage of the FDN and plate | 1a; 1b and 3 for the wiring | ~450 |

Step 1 is two PRs because one would be about 2,200 lines of which the suite and
the FDN are each a separate review. 1a ships with one algorithm and no enum
(an enum of one is not a choice); 1b is where the choice becomes real. Parallel
work: 0 and 0b are independent and can start now; 1a needs both (its node calls
`report_tail`, its freeze-less kinds need step 0). After 1a, 1b, 2 and the
generator half of 4 are independent files; 3 waits on 1b only for the shared
blocks.

## 9. What the reverb asks of modulation

The owner asked whether tutti's modulation design survives a real, demanding
node. Each item is answered against the code on `main`; `tutti-mod` is renamed
`tutti-modulation` in a separate PR, so the new name is used here. Verdict: the
design handles all five; the smallest changes are two documented conventions
and appending kinds.

How modulation reaches a node today, from the code: `tutti-modulation`'s
`compile` turns declarations into a `ControlProgram` per target; a
`ControlLaneNode` (one per modulated param, `tutti-nodes`) reads it at its
input's time and sends the target piecewise-linear segments on a grid of
`CONTROL_STEP` (32) frames plus every corner; the executor turns them into a
per-frame value for the node's param port, clamped to the port's `ParamRange`
and ramped from the node's `Node::param_base` (the `Param<U>` cell). The node
reads `io.param(k)`: `ParamInput::Base` (unmodulated, read your own cell) or
`ParamInput::Frames(&[f32])`.

### 9.1 A tail that depends on a modulated param

- **Is `Shape`'s tail fixed at prepare?** Yes. `Node::shape` "must not change
  after `prepare` without the node being re-inserted — a shape change is a
  recompile", and the compiler reads shapes at `prepare`/insert
  (`Editor`'s re-prepare path). A node cannot update its tail at runtime.
- **What does the node declare?** The worst case over the range it can be
  driven to. The node cannot read the `ModRange` (a `bevy-tutti` declaration
  that becomes the port's `ParamRange` in the spec; the node never sees it), so
  it must not trust it: the node declares from its own ceiling (`max_decay`, a
  build parameter) and clamps the value it reads to that ceiling, whatever the
  port's range says. A `ModRange.max` above the ceiling is therefore harmless, not
  an under-declared tail.
- **What does the tail do for a reverb?** The executor parks a node only if it
  returned a silence claim (`Silent`, `Idle`, all-silent masks) *and* latency plus
  tail have elapsed; a node returning `Status::Modified` is never parked. The
  tail is the guard for a reverb that reads silent while still busy (pre-delay
  gaps), and the length export extends by.
- **Park and wake.** `Status::Idle` is "silent and nothing pending". A reverb
  that returns `Modified` forever costs its full block when the input is silent.
  The node keeps a counter of consecutive silent input frames; once it exceeds
  `R::tail(ceiling controls, floor)`, it calls `R::reset()` (the state is below the
  floor, now exactly zeroed) and returns `Idle`; the next block with input
  wakes it by the executor's rule. The counter lives in the node, not the trait,
  so the trait stays a processing contract. Smallest change: none in
  `tutti-graph`. The one test is that an `Idle` reverb equals a fresh one after
  it wakes.
- **If a modulated decay could be known at build** (a lane that is a plain
  automation curve), a tighter tail would be possible by re-inserting the node.
  Not worth it; the ceiling is one number.

### 9.2 Costly-to-apply params

- **Cadence.** Once per block, after the lane's frames are in, is what the
  existing nodes do. `CrossoverNode`/multiband read `block_driven` (the
  port's *last* frame, `ramp.rs`) once per block for the split frequencies and
  redesign there; `DelayLineNode` reads its controls once per block and ramps.
  The lane's own resolution is a point every `CONTROL_STEP` = 32 frames, so a
  per-block read of a 512-frame block takes 1/16 of what a lane could say. For a
  reverb's decay that is plenty: RT60 moves over seconds.
- **Who smooths?** Both, in the same split the existing nodes use. The lane
  supplies a piecewise-linear shape; the executor ramps the base; the *node*
  ramps from where the previous block ended to this block's target
  (`DelayLineNode`: `Ramp::new(from, target, size)` for feedback, cross
  feedback and mix, plus a per-channel delay ramp, with `last` kept across
  blocks and re-primed after `reset`). A `Param<U>` cell does not smooth by
  itself: it is an atomic. The reverb follows it exactly: gains and loop gains
  are `Ramp`ed over the block; a filter redesign is done once at the block's
  target and the coefficients are interpolated across the block
  (`SvfCoefficients::lerp` exists for this; the absorption filter needs the same
  `lerp` on its coefficient set). Three RT60s feeding one design is one design per
  block, not three.
- **Consistent?** Yes, it is the existing convention. Smallest change: none;
  it becomes the documented convention for `Reverb` implementations (§3.1), plus a
  `lerp` on the absorption design.
- **Non-finite writes.** A raw cell accepts NaN; every per-block read goes
  through the nodes' `LastGood` (`ramp.rs`). The reverb's `Controls` builder does
  the same, which is also why the contract's stability case includes non-finite
  controls.

### 9.3 Structural params (size)

- **Bounded maximum at prepare?** The node needs the maximum size when it
  allocates the lines, in `prepare`. The param system does not hand it one:
  `Node::prepare(&Prepare)` carries rate and block size, and a `ParamRange` is a
  per-port clamp in the spec that the node never reads. So the maximum is the
  node's own build parameter, as `DelayLineNode` already does (`max_delay`, then
  `delay_secs` clamped to it, in `delay.rs`). A reverb takes `max_size` the same
  way (`Mix` 0..1 maps onto `[min_length, max_length]` of each line, and
  `prepare` allocates at the maximum). The clamp makes a `ModRange` that exceeds
  it safe. Smallest change: none.
- **Smooth length change.** `DelayLineNode` shows the pattern: a per-channel
  `Ramp` of the delay in samples with fractional reads (`InterpolationMode`), so a
  moving length is a pitch bend, not a click. The reverb slews each line's length
  at a bounded rate (a fixed maximum cents per second, a build constant) and the
  contract's click-free case measures it. A large step takes several blocks to
  arrive, which is the intended behaviour for size.
- **Should the node see its range at prepare?** A tempting change is passing
  each port's `ParamRange` into `Prepare`. Recommend against it: the range is a
  mutable fact of the graph (a `ModRange` edit is "a store into the lane, never a
  plan"), so a node that sized from it would be wrong after an edit. The ceiling
  is the node's, and it is a rebuild to change.

### 9.4 New `UnitParam` kinds

Adding `Decay`, `Diffusion` (and later `Freeze`) touches:

| Place | Change |
|---|---|
| `tutti-types` `UnitParam` | Append variants with the next ids (25, 26, 27); `repr(u8)`, never renumber |
| `TryFrom<u16>` and the `u16::from` side | One arm each |
| `tutti-types` tests | The `PINNED` table (it is `[(UnitParam, u16); 25]` and becomes 27) and the documented count |
| Param docs on each variant | Unit and meaning (`Decay`: RT60, `Seconds`, index 0 mid, 1 low, 2 high) |
| `bevy-tutti` registration | None per kind: `add_audio_param::<U, P>` is generic over the packed address, one call per (unit, address) used; `ModRange`/`ParamRef` take any `UnitAddr` |
| `tutti-modulation`, the session, `tutti-graph` | None: they carry `UnitAddr`, not a kind list |
| Saved files | Ids are persisted and `AudioParam`'s `P` is the id; index 0 of a kind is bit-identical to the id before indices existed, and appended ids do not move any existing one. An older build meets an unknown id as `UnitParamOutOfRange`, which the type documents as "ignore" |

The only compatibility cost is forward: a project saved with `Decay` loses that
param in a build that predates it. Append-only ids are the rule that already
exists.

### 9.5 Bevy and the session

Yes, with no reverb-specific code. `ParamRef::unit_at(entity, UnitParam::Decay, 1)`
names the low-band RT60; `Mods`, `ModRoute` and `ModRange` take any `ParamRef`;
the node declares only the params it has (`ParamSet` builder), and
`Shape::with_param_addrs` lists the modulatable ones as it does for the
multiband compressor's per-band thresholds and the crossover's splits (the
tests `tutti-graph/tests/indexed_params.rs`, `bevy-tutti/tests/indexed_param.rs`
and `tutti-session/tests/common/bands.rs` show the indexed path end to end). The
number of modulatable ports is capped at `MAX_PARAM_PORTS` (32), well above the
FDN's dozen params, so the cap does not bind.

### 9.6 What the exercise found

| Finding | Smallest change |
|---|---|
| The tail is fixed at prepare, the node cannot see a `ModRange` | A ceiling in `Shape::tail`, plus a per-block report (§10, step 0b) |
| `Idle` is the node's call and `Modified` is never parked | A quiet-input counter in the node, reset and `Idle` after the tail |
| Per-block last-frame reads plus the node's own ramp is the cadence | Document it as the `Reverb` convention; add `lerp` to the absorption design |
| A node cannot learn a `ParamRange` at prepare, and should not | The node's own `max_size` / `max_decay` build parameters |
| New kinds are append-only ids with one pinned table | Append `Decay`, `Diffusion` (step 0) and `Freeze` (with the FDN, step 1b) |
| `MAX_PARAM_PORTS` is 32 | Does not bind; an FDN has about twelve params |

## 10. The graph's dynamic tail (step 0b)

Owner decision, 2026-11-08: the graph re-reads the tail. The worst-case
workaround is rejected. This is the smallest change to `tutti-graph` that lets
a node's tail follow its params at runtime, with the plan immutable.

### 10.1 What the graph does with a tail today

Read from the code on `main`:

| Use | Where | Reads |
|---|---|---|
| Declared | `Shape::tail`, via `NodeSpec` and `Editor` (`node.tail = shape.tail`) | the shape at insert/prepare |
| Silence skip | `exec.rs`: `tail_elapsed(rec.park_after, u.quiet)`; `NodeRec::park_after` = latency then tail, copied into the plan (`plan.rs`), verified in `compile/verify.rs` | the plan's copy |
| Graph tail | `tutti_types::graph_tail`, a fold over `TailGraph::tail(node)`, implemented by `Topology` from each node's spec tail | the spec |
| Export | `RenderGraph::reported_tail()` (`tutti-export/src/graph.rs`) is `graph_tail(editor.spec().topology)`; `ExportRequest::with_reported_tail(cap)` resolves it (Unbounded -> `cap`); `RenderPlan::new` fixes `output_length = duration + tail` before the render | the spec, before the render |
| Contract | doc 013: "A tail excludes latency"; `Node::shape` "must not change after `prepare`" | n/a |

The tail has two readers with different needs: the executor (audio thread, per
block, per unit) and export (control side, once). The plan stays immutable
because both can be served by a value that is *not* in the plan.

### 10.2 Options

| | A. `Status` carries the tail | B. Ceiling in `Shape`, node reports each block (recommended) | C. `Tail::Dynamic { max }` + report | D. Tail published as a control cell |
|---|---|---|---|---|
| Change | `Status` is returned by every node; add a field | `Shape::tail` keeps its meaning as an upper bound; `Io::report_tail(Tail)` | A new `Tail` variant in `tutti-types`; same report | A shared atomic in the shape, written by the node, read elsewhere |
| Churn | Every node and `Status` match in the workspace | Additive: nodes that do not report keep today's behaviour | `then`, `beside`, `from_*`, every `match` on `Tail`, serde | The shape holds shared mutable state; `Eq`/`Hash` on shapes |
| Plan | untouched | untouched | `Dynamic` flows into `park_after`, `verify` | untouched, but a cell in each rec |
| Audio thread | free | one store into the unit record | same as B | an atomic load per block per reader, cross-thread cache line |
| Pooled = serial | yes | yes (below) | yes | yes |
| Control-side read | none | via an executor view (10.4) | via the same | direct, but of a value the audio thread wrote and the control side cannot tie to a block |

**Recommend B.** A is a workspace-wide signature change for a figure only a few
nodes vary. C adds a variant that the ceiling already expresses (`Finite(max)`
is "at most this long"). D puts shared mutable state in a value that is
compared and hashed and violates "nodes communicate only through ports", for a
number the executor needs privately.

### 10.3 The mechanism

- `Shape::tail` is the **ceiling**: the longest the node can ever ring, or
  `Tail::Unbounded` if it can hold. All existing readers (compile, `verify`,
  PDC, `graph_tail` over the spec) keep reading it. Nothing in the plan or the
  spec changes; a static reader gets a safe over-estimate.
- `Io::report_tail(&mut self, Tail)` records the node's current tail in its own
  executor unit record (the same record as `u.quiet`, `u.last_quiet`). It is a
  plain field store, no allocation and no atomic: only the task that runs the
  unit touches it, which is why pooled and serial renders agree (the park
  decision is a function of that unit's own inputs and reports). The value is
  clamped to the ceiling (debug-assert on an excess), `Unknown` never narrows.
  It resets to the ceiling at `prepare`, `reset` and on a fork.
- The silence skip becomes `tail_elapsed(min(ceiling, reported).after(latency),
  u.quiet)`. `Tail::Unbounded` reported means never skip, which is how freeze
  rides the same mechanism with no special case.
- **The node's duty** (stated in the `Node` docs): the reported tail must cover
  the ring-out from the last non-quiet input frame under the settings in force
  since. A node that shortens its decay keeps the longer figure until its state
  has decayed (a high-water mark that falls by the frames elapsed). A figure too
  short would let the executor skip a node that still sounds, so the rule errs
  long; a modulated decay reports from the larger end of the block's ramp.
- The reverb node reports `R::tail(controls, floor)` after its `Controls` are
  built, in `process`, every block.

### 10.4 Export: the tail at the render's end

**Recommendation: the tail the graph reports at the programme's end, not the
max.** The max is the ceiling already, and for an FDN that can freeze it is
`Unbounded`, i.e. `cap` for every bounce. What rings after the last programme
frame is what the render must keep.

- The fork's executor exposes a view implementing `TailGraph` (the spec's
  topology with each unit's reported tail in place of its spec tail), so
  `graph_tail` folds it unchanged. Control side, offline: allocation is fine.
- `RenderGraph::reported_tail_now()` asks it once the `duration` frames are
  rendered. `RenderPlan` fixes `total` up front today, so it gains a second
  phase: the tail resolved from that view (against `cap`) extends the plan
  before the pull loop ends. `reported_tail()` (the spec) stays for callers
  that need a figure before rendering, and is the ceiling.
- Exports without a dynamic node behave exactly as now: the view equals the
  spec.
- Unbounded freeze at the end of the programme resolves to `cap`, as an
  unbounded graph does today.

Unresolved by reading: how `RenderPlan`'s pull loop is structured around
`total`; if the second phase is awkward the fallback is the ceiling against
`cap`, today's behaviour. Step 0b settles it with a test.

### 10.5 Doc 013 change note

Step 0b adds a note to doc 013's contract section ("A tail excludes latency"):
`Shape::tail` is an upper bound; a node may narrow it per block with
`Io::report_tail`; the executor and export read the narrowed value, the plan
and spec the bound; the reverb is the first user. It is recorded there with the
step, not before.

### 10.6 Tests for step 0b

A node that reports a shorter then a longer tail than its ceiling; the skip
waits the reported figure (`tests/executor.rs`, beside
`the_skip_waits_for_latency_plus_tail`); an excess report is clamped; `Unbounded`
is never skipped; serial and pooled renders are bit-identical with reporting
nodes (`tests/parallel.rs`); export of a reverb-like node renders the tail at
the end, not the ceiling. Mutations: ignore the report (fails the shorter
case); skip the clamp (fails the excess case).

## 11. Decisions and open questions

Decided by the owner on 2026-11-08, in this doc's terms:

| Question | Decision |
|---|---|
| Tail with a live decay | The graph re-reads the tail: §10, step 0b. The max-decay workaround is rejected as the mechanism (the ceiling remains the bound the node declares) |
| Fork: fresh or cloned state | `fresh()`; the graph's preroll warms it. The export-fork test that pins it ships in step 1a |
| Enum convenience | `ReverbKind` ships in step 1b with the FDN beside Freeverb; it only spawns the matching node. No engine-level "auto" |
| Second algorithm | The FDN, in step 1, because it exercises the model (three-band RT60, size, diffusion, modulation) and the runtime tail |
| Freeze | With the FDN (1b): `UnitParam::Freeze`, and `Tail::Unbounded` through the §10 mechanism |
| Generic or boxed node | Generic `ReverbNode<R>` (§4.1). Owner stated the generic shape; the boxed argument is in §4.1 |

Remaining, **recommended, owner did not object**:

| Question | Recommendation |
|---|---|
| Which algorithms after step 1? | Convolution, plate, early reflections (§5) |
| Chorus and the other modulation effects? | Later, after reverb has two or three users; extract what generalises (the tail report, `DryAlign`, the smoothing split) then |
| `Diffusion` and `Size` as `Mix`? | Yes, with a `Mix` doc line (§2.1) |
| `UnitParam::Decay` and `Diffusion` new kinds? | Yes (§2.2); `Freeze` with the FDN |
| Does `ConvolverNode` stay? | Yes in step 2; thin it onto the same implementation |
| Do `Param<U>` cells smooth? | No: the node ramps from where the last block ended (§9.2) |
