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
- **Modulation holds up** with one convention and no code change (§9): a tail
  declared at the worst case of the node's own range, controls read once per block
  with coefficient interpolation, and `UnitParam` kinds appended.
- **Build order:** trait + contract suite + Freeverb (bit-identical), then
  convolution, FDN, plate, early reflections (§8).

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

Freeze (loop gain 1, input gated) makes the tail unbounded and does not fit
`Seconds`. It is out of the trait for now; if the FDN needs it, `tail` returns
`Option<Seconds>`-like through a documented sentinel or the node declares
`Tail::Unbounded` from a separate `fn holds(&self, &Controls) -> bool`. Open
question (§9).

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
- **Tail:** `Shape` is read at compile, so it declares from `R::tail` at controls
  where each tail-driving param sits at the *top of its declared range* (the
  decay param's `max`, set when the node is built; default 8 s). The node clamps
  the live param to that range. A longer ceiling is a rebuild. The graph does not
  re-read `Shape` after a param change (§9.1).
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

| Step | Content | After |
|---|---|---|
| 0 | `tutti-types`: `UnitParam::{Decay, Diffusion}` and the table test; `Mix` doc | #354 (can start now) |
| 1 | `tutti-dsp`: `Reverb`, `Setup`, `Effort`, `reverb_contract!`, `Freeverb` implements it; `tutti-nodes`: `ReverbNode<R>` and `ReverbParams`, fork by `fresh`; GM unchanged | 0 |
| 2 | Convolution as a `Reverb`; `DryAlign` shared | 1 |
| 3 | The shared blocks (loop gain, absorption, pre-delay, width) with the FDN, the first user pair | 1 |
| 4 | Plate (takes `Diffuser` and the blocks) | 3 for shared blocks; otherwise 1 |
| 5 | Early-reflection generator, then as the early stage of 3 and 4 | 1 (generator), 3 and 4 (wiring) |

After step 1, steps 2, 3 and the generator half of 5 are independent files and
can run in parallel. Step 4 waits on 3 only for the blocks it shares; it may
write its own and be refactored when 3 lands, which is cheap. Step 0 starts now.

## 9. Open questions (recommended answers)

| Question | Recommendation |
|---|---|
| Generic or boxed node? | Generic `ReverbNode<R>` (§4.1): the type states what is honoured and no second settings struct is needed. Cost: a registration line per algorithm and a `match` at a picker |
| Is the enum convenience wanted? | Not in step 1. Add `ReverbKind` when a second algorithm is on `main` and a picker needs it; it spawns the matching `ReverbNode<R>` and nothing else. No engine-level "auto" |
| Which algorithms first? | Freeverb (exists), convolution (exists), FDN, plate, early reflections (§5) |
| Chorus and the other modulation effects? | Later. They share rate/depth/voices/feedback and several algorithms, and `ModDelayNode` is already one node. Extract what really generalises (the tail, `DryAlign`, the smoothing split) after reverb has two or three users; not before |
| `Diffusion` and `Size` as `Mix`? | Yes, per the owner's direction (no new unit). They are positions on `0..1` each algorithm maps to its own scale; `Mix` gets a doc line saying so |
| `UnitParam::Decay` and `Diffusion` new kinds? | Yes (§2.2); `Freeze` only with the FDN |
| Tail with a live decay? | Declare at the node's own ceiling (default 8 s) and clamp; the graph does not re-read `Shape` (§9.1) |
| Freeze and the unbounded tail? | Defer to step 3. Either `tail` returns a documented sentinel or a second method `holds(&Controls) -> bool` lets the node declare `Tail::Unbounded` |
| Fork: fresh or cloned state? | `fresh()`, as for the stretcher; preroll warms the tail. Today's `ForkByClone` copies a ringing tail, so a fork taken mid-reverb now starts silent; an export-fork test pins it |
| Does `ConvolverNode` stay? | Yes in step 2 (per-channel IR layouts, the bench); thin it onto the same implementation; retire only if nothing needs its API |
| Do `Param<U>` cells smooth? | No: the node ramps from where the last block ended, as `DelayLineNode` does (§9.2) |

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
- **What breaks if the tail is under-declared?** Not the live render. The
  executor parks a node only if it returned a silence claim (`Silent`, `Idle`,
  all-silent masks) *and* latency plus declared tail have elapsed; a node
  returning `Status::Modified` is never parked. So a reverb returns `Modified`
  while it rings. A wrong tail affects the *export* extension and the silence
  skip, nothing else. That is why the ceiling is conservative and a longer one is
  a rebuild.
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
| The tail is fixed at prepare, the node cannot see a `ModRange` | Declare from the node's own ceiling and clamp (a convention; no code in `tutti-graph`) |
| `Idle` is the node's call and `Modified` is never parked | A quiet-input counter in the node, reset and `Idle` after the tail |
| Per-block last-frame reads plus the node's own ramp is the cadence | Document it as the `Reverb` convention; add `lerp` to the absorption design |
| A node cannot learn a `ParamRange` at prepare, and should not | The node's own `max_size` / `max_decay` build parameters |
| New kinds are append-only ids with one pinned table | Append `Decay`, `Diffusion`; `Freeze` with the FDN |
| `MAX_PARAM_PORTS` is 32 | Does not bind; an FDN has about twelve params |
