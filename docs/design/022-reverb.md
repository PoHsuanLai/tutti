# 022: reverb as an interface

Tutti has two reverbs that share nothing: a fixed Freeverb (the General MIDI
send, PR #354) and a convolution node that needs an impulse response. Neither
is modelled: there is no "decay time", "pre-delay" or "damping" that means the
same thing in both, so no UI, preset, automation lane or test can talk about "a
reverb", and nothing can say whether a reverb is stable or how long it rings
without running it.

This design models what a reverb *is* as a pure function, and leaves how it is
run to each algorithm. It follows the stretcher decision of 2026-11-07
(`clean-plan.md`, "Stretch modes: an interface"): a trait per algorithm, an enum
at most as a convenience, the choice explicit, built and prepared on the control
side. And it applies the lesson of [doc 014](014-modulation.md) §1: modulation
was modelled twice as an engine mechanism and failed both times; the fix was "a
noun of the API, not of the engine: the API compiles, a node evaluates". Here
the model is the *noun* (a value, with analytic queries), and the kernel is what
evaluates. Nothing in the model forces a runtime shape on a kernel.

Numbering: 019 (bounce), 020 (tempo map) and 021 (plugin hosting) are drafts on
other branches; `docs/design` on `main` ends at 018.

## Summary

- **Every algorithmic reverb is a feedback delay network in state-space form**
  (§2): `s(n+m) = A·G(s(n)) + B·u(n)`, `y(n) = C·s(n) + D·u(n)`. Freeverb, Jot's
  FDN, the Dattorro plate, a Schroeder all-pass and early reflections are
  instances; convolution is the limiting case.
- **The pure part is `design(controls, rate) -> Network`** (§3): a plain,
  fixed-capacity value, with analytic queries written once (`rt60`, `tail`,
  `is_stable`, densities) and a `bounds()` worst case for allocation.
- **An algorithm is a `Design` (its `Controls` type and design function) plus
  its own `Kernel`** (§4). The `Network` is a *description for design and
  analysis, not an instruction stream*: no engine interprets every network, so a
  kernel stays free to specialise (fixed-N matrices, SIMD across lines, fused
  comb loops, Freeverb's exact float order).
- **The link that keeps purity honest is a contract test, not a shared
  runtime** (§8): a kernel's measured behaviour must match its `Network`'s
  analytic queries.
- **Controls are existing units and `UnitParam` kinds** (§2.2), plus two new
  kinds (`Decay`, `Diffusion`) and `Freeze`. No new unit.
- **One generic node, `ReverbNode<D: Design>`** (§5): reads its `Param` cells
  once per block, designs, retargets its kernel, reports its tail from
  `Network::tail` through the graph's dynamic tail (§12, step 0b).
- **Decisions of 2026-11-08** (owner): the graph re-reads the tail; fork is
  `fresh()` with preroll; `ReverbKind` ships beside a second algorithm, the FDN;
  freeze comes with the FDN (§13).

## 1. Current state

Line counts from `main` (`7c30ba335`) and PR #354 (`feat/soundfont-effects-nodes`).

| Piece | Where | Lines | Can express | Cannot express |
|---|---|---|---|---|
| `Freeverb` (8 combs + 4 all-passes per channel) | `tutti-dsp/src/reverb.rs` (#354) | 344 | One fixed room: mono in, stereo out, every figure a constant; `ring_out(floor)` bound | Any control: pre-delay, RT60, band decay, width, size, modulation |
| `ReverbNode` | `tutti-nodes/src/reverb.rs` (#354) | 105 | Wraps `Freeverb`; `ForkByClone`; `Cost::Moderate`; `Tail::Finite` from `ring_out(-60 dB)` | No params (`Controls = ()`), no mix |
| `Convolver` (partitioned FFT) | `tutti-dsp/src/convolver.rs` | 1940 | Any real room exactly; latency = one block; `fresh`, shared `IrSpectra` | Anything parametric |
| `ConvolverNode` + IR helpers | `tutti-nodes/src/convolution/` | 1114 + 87 + 101 + 19 | `mix`, `gain`; per-channel IR layouts (`IrChannelConfig`); dry aligned to latency (`DryAlign`); exact tail `ir_len - 1`; `generate_room_ir` | Live decay or damping; IR is read-only |
| `DelayLine`, `SweptLines` | `tutti-dsp/src/{delay,swept_delay}.rs` | 268 + 135 | Fractional taps, modulated reads | A network of them |
| all-pass, interpolation | `tutti-dsp/src/{allpass,interp}.rs` | 55 + 46 | First-order all-pass sweeps, Hermite | A Schroeder/Dattorro all-pass with a delay line |
| SVF, crossover (LR), ladder | `tutti-dsp/src/filter/` | 409 + 949 + 178 | Shelving and low-pass for damping, 4th-order band split | Per-line absorption design |
| `Ramp` | `tutti-dsp/src/control.rs` | 162 | A per-block linear ramp | n/a |
| `Session::add_general_midi` | `tutti-session/src/general_midi.rs` (#354) | 165 | Unit sends -> `ReverbNode::new()` -> sum | Another reverb |

No other reverb-like code exists in the workspace (plugin hosts load
third-party reverbs; separate). `UnitParam` has `RoomSize`, `Damping`, `Wet`,
`DelayTime`, `Feedback`, `Rate`, `Depth`, `Cutoff`, `GainDb`, `StereoSpread`,
and no kind for a decay time or a diffusion.

Missing is not an algorithm. It is the shared model, so that "stable", "RT60 of
2 s" and "rings for 3.4 s" are facts about a value that a test, a UI and the
graph can all read.

## 2. The model

### 2.1 One form, many algorithms

A reverb is a bank of delay lines whose outputs are mixed, filtered and fed back.
In state-space form, with `m` the line lengths, `A` the feedback matrix, `G` the
per-line absorption filters, `B` the input gains, `C` the output gains and `D`
the direct path:

```text
s(n + m) = A · G(s(n)) + B · u(n)        y(n) = C · s(n) + D · u(n)
```

| Reverb | `m` | `A` | `G` | `B`, `C`, `D` | Notes |
|---|---|---|---|---|---|
| Schroeder all-pass | 1 line | `-g`/`g` (1x1) | none | unity | a one-line network |
| Freeverb | 8 comb lengths per channel | diagonal | one-pole low-pass | mono in, 8 lines each to L and R | plus 4 series all-passes per channel |
| Jot FDN | N primes | orthogonal (Householder, Hadamard) | band absorption from RT60s | N in, N out | the general case |
| Dattorro plate | 2 loop lines, modulated | cross-coupled pair | one-pole damping | 14 output taps | all-passes nested in the loop |
| Early reflections | taps | 0 | none | sparse `C` | no feedback |
| Convolution | the IR's length | dense FIR | none | the IR is the system | the limiting case |
| Freeze | any of the above | lossless loop (`G` = 1) | unity | input gated | tail unbounded |

The model is a *description of behaviour*. It says what a network does; it does
not say how a kernel computes it.

### 2.2 Controls: existing units and kinds

Each algorithm's `Controls` is a plain struct in tutti units (§4). The
parameters it can draw from, each a `Param<U>` addressed by a `UnitParam` kind
and a `UnitAddr` index when a node has several of a kind:

| Group | Parameter | Unit | `UnitParam` (index) | Used by |
|---|---|---|---|---|
| Onset | pre-delay | `Seconds` | `DelayTime` | all (a building block) |
| Early | early level | `Db` | `GainDb` (1) | those with early reflections |
| | pattern | build config (a room shape) | none | the early generator, and stages built from it |
| Tail | RT60 mid / low / high | `Seconds` | `Decay` (0 / 1 / 2), **new** | mid: all; low/high: FDN; high alone: plate, Freeverb (as damping) |
| | band crossovers | `Hz` | `Cutoff` (0 / 1) | FDN |
| | damping | `Mix` | `Damping` | Freeverb |
| | size | `Mix` | `RoomSize` | all but convolution |
| | diffusion | `Mix` | `Diffusion`, **new** | FDN, plate |
| | modulation rate / depth | `Hz` / `Depth` | `Rate` / `Depth` | FDN, plate |
| Output | stereo width | `StereoWidth` | `StereoSpread` | all with a stereo output |
| | wet gain | `Db` | `GainDb` (0) | all |
| | insert mix | `Mix` | `Wet` | the node, not the algorithm (§2.3) |
| Hold | freeze | toggle, `>= 0.5` | **new** `Freeze` (1c) | FDN |

**No new unit** (CLAUDE.md, "Adding a unit": a distinct range *or* algebra).
Size and diffusion are `0..1` positions each algorithm maps onto its own scale,
so they are `Mix` (`Mix`'s doc gains the line "a position on a `0..1` scale, a
blend among them"). RT60 and pre-delay are `Seconds`, crossovers and
modulation rate `Hz`, depth `Depth` (read as its absolute value), width
`StereoWidth`, wet `Db`. Early-reflection geometry (a shoebox in metres) is
build config, a small checked struct of `f32` metres on the control side, with a
comment that no length unit exists.

**New `UnitParam` kinds**, appended: `Decay` (`Seconds`, index 0 mid, 1 low, 2
high; ids 25), `Diffusion` (`Mix`; 26), `Freeze` (27, with the FDN). Reusing
`DelayTime` or `Release` would name RT60 wrongly, and `Feedback` is a
recirculation coefficient with a stability ceiling, which a diffusion is not.

### 2.3 Wet/dry policy

**The algorithm is wet-only; the node owns mix.** A kernel overwrites its output
with the reverberated signal at the wet gain. The node has a `Mix`
(`UnitParam::Wet`, default `WET`, a send) for insert use and aligns the dry path
to `Kernel::latency()`, never to pre-delay (pre-delay is part of the wet sound).
The existing `DryAlign` in `convolution/node.rs` solves the alignment once and
moves to the shared node. `Network` carries no direct path (`D = 0`), so "wet-only
means no dry leak" is a property of the model, and the kernel test checks the
kernel keeps it.

### 2.4 Channel shape

`Shape::audio(in, out)` is fixed at node construction; width is runtime
(`ChannelLayout`), never a const generic. A `Design` reports its shape.
mono -> stereo is the send case (two decorrelated outputs, `width` toward mono);
stereo -> stereo feeds each input through `B`; N -> N is native to an FDN
(`C` has N rows) and the convolver (`IrChannelConfig`); a design refuses a shape
it cannot make.

### 2.5 Freeze

Freeze is a property of the network: a lossless loop. The design sets every
absorption gain to unity (`G = 1`) with an orthogonal `A` and gates `B`; the
network is then marginally stable and `Network::tail` is `Unbounded`, with no
special flag in the trait. Entering it ramps the loop gain and the input gate in
`retarget`, so it does not click. Only the FDN freezes (1c); Freeverb's one-pole
and the plate's damping are never unity at all frequencies.

## 3. The `Network`

A plain value in `tutti-dsp`: `Clone`, no `Arc`, no allocation to build, no
atomics. Fixed capacity (`Inline<T, N>`, a stack array with a length), because
`design` runs on the audio thread once per changed block and must not allocate;
`MAX_LINES` is 64, a few kilobytes on the stack.

```rust
pub struct Network {
    pub rate: SampleRate,
    pub pre_delay: f32,                  // frames, fractional
    pub stages: Inline<Stage, MAX_STAGES>,   // in series
    pub width: StereoWidth,
    pub wet: Db,
}

pub enum Stage {
    /// Sparse taps: early reflections. A = 0.
    Taps(Taps),                          // delays, gains, pans
    /// A series Schroeder/Dattorro all-pass, per output channel.
    AllPass(AllPass),                    // delay per channel, g, modulation
    /// Delay lines with feedback: A, G, B, C.
    Fdn(Fdn),
    /// A dense FIR, the limiting case. Carries what analysis needs, not the data.
    Fir(FirSummary),                     // length, per-band decay, energy
}

pub struct Fdn {
    pub lines: Inline<Line, MAX_LINES>,
    pub matrix: Matrix,                  // structured kind, not a bag of floats
    pub input: Gains,                    // B
    pub output: OutputMap,               // C (channel rows)
}
pub struct Line {
    pub length: f32,                     // centre length, in frames
    pub modulation: Modulation,          // depth in frames, rate Hz
    pub absorption: Absorption,
    pub diffuser: Option<AllPass>,       // nested in the loop (the plate)
}
pub enum Matrix { Diagonal, Householder, Hadamard, Circulant(Gains), Dense(Inline<f32, MAX_DENSE>) }
pub enum Absorption {
    Gain(f32),                           // flat
    OnePole { gain: f32, damp: f32 },    // Freeverb, plate
    Bands { low: f32, mid: f32, high: f32, crossover: [Hz; 2] }, // Jot
}
```

A `FirSummary` is what the analysis needs of an impulse response (length, a
Schroeder-integrated RT60 per band, energy), computed on the control side when
the `Design` is built. The IR itself stays in the convolution kernel's spectra,
so the `Network` never carries an `Arc` into the audio thread.

### 3.1 Analytic queries, written once

All are pure functions of the `Network`, with no kernel and no signal.

| Query | Definition |
|---|---|
| `rt60(band)` | Per line, `g = 10^(-3·m/(RT60·fs))` inverted: `RT60 = -3·m / (fs·log10 g)` for the band's gain `g` and effective length `m`. For a diagonal `A` the slowest line governs. For a mixing `A` (orthogonal) the length-weighted figure `-3·Σm / (fs·Σ log10 g_i)`, exact when the design sets every line from one RT60. A `Fir` stage reports its summary. Series all-passes add none |
| `tail(floor) -> Ring` | `Ring::Finite(Seconds)`: pre-delay plus the stages' transit plus the slowest band's `RT60 · |floor| / 60`. `Ring::Unbounded` when the loop is lossless. This is what the node reports (§12) |
| `is_stable()` | Every band's loop gain `< 1` (or the lossless freeze case, marginal) and the matrix orthogonal: `Householder` and `Hadamard` are by construction; `Dense` is checked on the control side by a Frobenius bound that is sufficient, not necessary |
| `is_lossless()` | Every band's gain is 1 within a tolerance and `A` orthogonal: freeze |
| `modal_density()` | The total delay in seconds, `Σ m / fs`: modes per hertz |
| `echo_density(t)` | From the line count and mean length and the matrix: a diagonal `A` never densifies beyond its line count per mean length; a mixing `A` grows with the mixing (Jot's analysis). A function used for ordering and comparison, not an absolute figure |
| `latency()` | Zero for time-domain stages; a partition for `Fir` |

**Modulated lengths.** The queries use the *centre* lengths, and say so.
Modulation moves a length by a few frames around the centre, so the RT60 and
tail shift by a relative amount of the order of the excursion over the length
(a fraction of a percent at typical depth) and this is inside the kernel test's
tolerance. Stability is stated for the centre design; the time-varying case is
a property the kernel contract measures (a modulated delay with an interpolating
read only loses energy for linear interpolation, and an all-pass interpolator
can overshoot slightly, so the contract's stability case runs with modulation at
its maximum). A nested all-pass inside a loop line is lossless, so it adds no
decay; it adds its low-frequency group delay to the line's *effective* length,
which is what `rt60` uses. Open question 13.3.

### 3.2 `bounds()`

`Design::bounds(&self, ceiling: &Ceiling) -> Bounds` is the worst case of the
network over the declared ranges of the controls: the most lines, the longest
total delay and each line's maximum (size at 1, modulation at its maximum), the
longest IR, and the longest tail (the decay param's ceiling, `max_decay`,
default 8 s). The kernel allocates from it in `prepare`, and the node declares
`Shape::tail` (the graph's ceiling, §12) from `bounds().tail`. A design test
asserts that `bounds()` dominates `design()` for every reachable control, which
is what makes "no allocation after `prepare`" a checked fact.

## 4. `Design` and `Kernel`

```rust
// tutti-dsp. Pure: no Arc-published state, no atomics, no graph types.
pub trait Design: Clone + Send {
    /// Plain values in tutti units, one struct per algorithm.
    /// Freeverb: { room: Mix, damp: Mix }. FDN: { decay: [Seconds; 3],
    /// crossovers: [Hz; 2], size: Mix, diffusion: Mix, modulation: ..,
    /// width: StereoWidth, pre_delay: Seconds, wet: Db, freeze: bool }.
    type Controls: Copy + PartialEq + Send;
    type Kernel: Kernel;

    /// Pure and allocation-free. The same controls always give the same network.
    fn design(&self, controls: &Self::Controls, rate: SampleRate) -> Network;

    /// Worst case over the controls' ranges (§3.2).
    fn bounds(&self, ceiling: &Ceiling) -> Bounds;

    /// An unprepared kernel for this design. Control side.
    fn kernel(&self) -> Self::Kernel;
}

pub trait Kernel: Send + Sized {
    /// Control side. Allocates from `bounds`; nothing allocates afterwards.
    fn prepare(&mut self, bounds: &Bounds, setup: &Setup);
    /// Real time, once per block whose network changed: slew lengths,
    /// interpolate coefficients, set ramps. Never allocates or waits.
    fn retarget(&mut self, network: &Network);
    /// Real time. Planar slices of equal length <= `setup.max_block`;
    /// overwrites `output` with the wet signal.
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]);
    fn reset(&mut self);
    /// Processing latency only, never pre-delay or tap spacing.
    fn latency(&self) -> Samples { Samples(0) }
    fn effort(&self) -> Effort { Effort::Heavy }
    /// Same kernel and prepared setup, empty state: the fork instance.
    fn fresh(&self) -> Self;
}
```

`Setup { rate: SampleRate, max_block: MaxBlock, input: ChannelLayout, output:
ChannelLayout }`. `Effort` is a three-variant enum here because `Cost` is
`tutti-graph`'s and `tutti-dsp` must not depend on it; the node maps it with one
`match`.

| Method | Why |
|---|---|
| `Design::design` | The functional mapping, the pure part. It is called with `Copy` controls so a test, a UI and the node call the same function |
| `Design::bounds` | Allocation and the graph's ceiling come from one analysis, not from a kernel's private constants |
| `Kernel::prepare(bounds, setup)` | Control side, like `Node::prepare` and the stretcher. A kernel sizes itself from `bounds`; it never inspects a `Network` to allocate |
| `Kernel::retarget(&Network)` | The only coupling between model and runtime: a kernel reads what it wants from the network and moves toward it. Freeverb copies `feedback` and `damp` into its combs; the FDN slews its lengths and interpolates its absorption coefficients; convolution reads nothing but pre-delay and gain |
| `process` | Controls are not an argument: they are in the kernel via `retarget`. Planar slices fit the node's `Io` and keep width runtime |
| `reset`, `fresh` | Seek/stop, and the fork rule from the stretcher decision. A fork starts from silence; the graph's `PrerollTable` warms it |
| `latency` | `Samples`, wrapped in `Latency::new` by the node; a separate method so "musical delay is not latency" is checked by type and test |

### 4.1 Why not one engine that runs every network

An engine that interprets any `Network` would be the fixed-shape trap that old
`tutti-modulation` fell into, in a new place. It would block: fixed-N matrix kernels and
const generics (a Hadamard of 8 lines is a handful of butterflies, not a generic
matrix-vector product); SIMD across lines; fused comb loops (Freeverb's
`process` runs one comb over a whole block); and Freeverb's exact float order,
which the GM render has to keep. It would also make every algorithm pay for the
union of all shapes. So the model is a description and each algorithm owns a
kernel; `retarget` is the only crossing, and the kernel decides what to read.

The *link* between description and runtime is a contract test (§8), not a
shared mechanism: a kernel's measured RT60 per band, pre-delay placement, tail
coverage and stability must match its `Network`'s queries within a tolerance the
kernel declares. Analysis and runtime cannot drift without a failure, and
nothing constrains how the kernel is written.

### 4.2 A generic engine as a convenience?

**Recommend yes, as an opt-in, later, and never as the default or the oracle.**
A `Generic` kernel interprets any `Network` sample by sample with the same
building blocks. It earns its keep as (a) the kernel for a new or experimental
`Design` before anyone writes a fast one (a design can be tried by ear the day
its `design` function exists) and (b) the way to check a `Network` by listening
and by the kernel contract before a specialised kernel exists. It does not force
anything: the shipped algorithms do not use it, `type Kernel = Generic` is a
choice, and it is not an oracle (CLAUDE.md: no new test compares a render
against another reference render; its output is held to the same analytic
queries as any kernel). It costs about 300 lines and a standing obligation to
support every `Stage`, so it comes after the FDN, not before (§9, step 4).

### 4.3 Shared building blocks (tutti-dsp)

Kernels are assembled from optimisable pieces; the pieces do not know the
`Network`.

| Block | What |
|---|---|
| `DelayLine` (exists) | Fractional read, modulated length with a bounded slew |
| `AllPass` delay form | Schroeder/Dattorro, with an optionally modulated delay |
| one-pole, band absorption | One-pole low-pass; first-order or two-shelf realisation with `lerp` on its coefficient set (`SvfCoefficients::lerp` is the model) |
| structured matrix kernels | Householder O(N), Hadamard O(N log N), diagonal, circulant; const-generic in N where it pays |
| `loop_gain(delay, rt60, rate)` | `g = 10^(-3 d / (RT60 fs))`: used by `design` and by `rt60`'s inverse |
| tap reader | A sparse-FIR reader over a delay line |
| `PreDelay` | A line whose length slews, never jumps |
| `Width` | Mid/side width and decorrelation of two wet signals |
| `Convolver` (exists) | The partitioned convolver |

A block is extracted when its second user lands, not before.

## 5. The graph node

```rust
pub struct ReverbNode<D: Design> {
    design: D, kernel: D::Kernel,
    params: D::Params,          // the Param<U> cells this algorithm declares
    last: Option<D::Controls>, network: Network,
    mix: Param<Mix>, dry: DryAlign,
    quiet: u64, high_water: Samples,   // §12 duty
}
```

Per block: read the `Param` cells once (through `LastGood`), build `D::Controls`;
if it differs from `last` (bitwise `PartialEq`), `design` into `network` and
`kernel.retarget(&network)`; `kernel.process`; then
`io.report_tail(network.tail(TAIL_FLOOR))` with the high-water rule of §12.
`D::Params` and its `fn controls(&self) -> D::Controls` live in a helper trait
in `tutti-nodes` (`ReverbParams`), so `tutti-dsp` holds no `Param`. The node is
`Node` + `ParamNode` inserted through `param_parts`, declares only the params in
`D::Params` through its `ParamSet`, and `Shape::with_param_addrs` for the
modulatable ones. `Shape::tail` is the ceiling from `design.bounds(..).tail`.

### 5.1 Generic, not boxed

`ReverbNode<D>` and not a box: `type Controls` and `type Kernel` make `Design`
not object-safe by design, and erasing them would reintroduce a settings bag. The
per-sample loops are monomorphic. The cost is a registration line per algorithm
and a runtime chooser (below). A structural change (a new IR, another algorithm,
a different line count) builds a new `ReverbNode<D>` on the control side and goes
in through the graph's `crossfade`; the audio thread never swaps an owning
handle. The node's fork is `design.clone()` with `kernel.fresh()` and the same
params, replacing `ForkByClone`; a fork starts from silence and preroll warms it
(pinned by an export-fork test in 1b).

### 5.2 `ReverbKind`

```rust
pub enum ReverbKind { Freeverb, Fdn(FdnConfig) /* later: Plate, Convolution(ir) */ }
impl ReverbKind { pub fn spawn(self, ..) -> SpawnedReverb }  // builds the matching ReverbNode<D>
```
In `tutti-nodes` (ships in 1c beside the FDN). It only spawns the matching node;
there is no engine-level "auto", and it holds no policy.

### 5.3 General MIDI (#354)

`Session::add_general_midi` keeps calling `ReverbNode::new()`, which becomes
`ReverbNode::<Freeverb>::general_midi()`: Freeverb at `{room: 0.5, damp: 0.5}`
as a send. `Freeverb`'s design computes `room * 0.28 + 0.7` and `damp * 0.4` in
the same `f32` operations as today's constants, the network carries exactly
those numbers, and `retarget` copies them into the combs, so the GM render is
bit-identical by construction (Freeverb's kernel is today's code: a diagonal
comb bank, its loop untouched). A follow-up `add_general_midi_with` takes
another `ReverbNode<D>`.

### 5.4 Bevy

`bevy-tutti` composes nothing new: `param_graph_node!(ReverbNode<Freeverb>)`
(one line per `D`) and `App::add_audio_param::<U, P>()` per row of §2.2; the
`UnitParam` kinds are the only `tutti-types` change. `scripts/check-adapter.sh`
holds.

## 6. Implementations

Ranked by value over cost. Pure Rust, written from the cited papers (no code
derived except Freeverb, already MIT-noticed); no C libraries. Each row is a
`Design` (a `design` function and `Controls`) and its own `Kernel`.

| # | Algorithm | Network shape | Kernel | Source | Lines (est.) | Cannot do |
|---|---|---|---|---|---|---|
| 1 | **Freeverb** (exists) | `Fdn` diagonal, `OnePole`, 8 lines per channel, then 4 `AllPass` per channel; `Controls { room, damp }` | Today's code unchanged; `retarget` copies two numbers | rustysynth `reverb.rs` (MIT, in header); Jezar's Freeverb | 344 now, +~150 (design, queries) | Three-band decay, freeze, modulation; RT60 accurate only to its declared tolerance |
| 2 | **Feedback delay network** | `Fdn`, `Bands` absorption, Householder or Hadamard, optional modulation and diffusion | Const-generic N, O(N)/O(N log N) matrix, SIMD across lines, interpolating reads | Jot and Chaigne, AES 90th, 1991; Jot 1997; Smith, *Physical Audio Signal Processing*; Schlecht and Habets on matrices | ~600 kernel + ~250 design | A real room's exact early pattern; sparse at very short RT60 and huge size |
| 3 | **Convolution** (exists) | `Fir(FirSummary)` + pre-delay | `Convolver`; `retarget` reads pre-delay and gain | `tutti-dsp::Convolver` | adapter ~250 | Live decay, size, damping, modulation. Latency one partition |
| 4 | **Dattorro plate** | `Fdn` of 2 lines, `OnePole`, nested `AllPass` diffusers, modulated lengths | Nested all-pass loops, tuned, as the paper | Dattorro, JAES 45(9), 1997 | ~350 + ~150 design | Three-band decay, N > 2 channels, freeze |
| 5 | **Early reflections** | `Taps`, A = 0 | Tap reader | Allen and Berkley, JASA 1979 | ~250 | A tail. Also a stage inside 2 and 4 |
| 6 | Velvet-noise reverb (later) | `Fir` of sparse taps | Sparse FIR | Järveläinen and Karjalainen 2007; Välimäki et al. 2012 | ~200 | Stereo-decorrelation needs thought; no freeze or modulation |

Not planned: Schroeder (a one-line network, superseded by Freeverb), spring
(nonlinear, its own model), shimmer (a pitch shifter in the loop).

## 7. Where things live

| Item | Crate | Layer | Check |
|---|---|---|---|
| `Network`, `Stage`, `Design`, `Kernel`, `Setup`, `Bounds`, `Effort`, the building blocks, `Freeverb`, `Fdn`, `Plate`, `EarlyReflections`, both contract suites | `tutti-dsp` | 2 | Allowed deps only (`tutti-types`; `tutti-graph` dev-only, hence `Effort`); no atomics or `Arc`-published state; controls per call; `design` is a pure function |
| `UnitParam::{Decay, Diffusion, Freeze}` | `tutti-types` | 1 | vocabulary only; no new unit |
| `generate_room_ir` | moves to `tutti-dsp` with the convolution design | 2 | a design builds its own IR |
| `ReverbNode<D>`, `ReverbParams`, `ReverbKind`, shared `DryAlign`; `ConvolverNode` kept, thinned | `tutti-nodes` | 4 | node holds only the adapter |
| `Session::add_general_midi` | `tutti-session` | 7 | call unchanged; Bevy-free |
| Registration | `bevy-tutti` | 8 | `param_graph_node!` lines; no engine logic |

`Freeverb` keeps its MIT header; the others are written from papers. CLAUDE.md's
`tutti-dsp` paragraph names `Design`, `Kernel` and `Network` when 1a lands.

## 8. The contract suites

Two suites, so the model and the runtime are each tested against physics and the
spec, and against each other through the model's own queries. Every figure is
analytic or from the spec, never another implementation's render.

### 8.1 Design tests (no kernel), `design_contract!`

Run in `tutti-dsp` over every `Design`; they need no audio.

| Case | Figure | Mutation record |
|---|---|---|
| Stable | `is_stable()` for every reachable control (a grid plus random draws within the ranges, the corners included) | raise a band gain above 1 |
| RT60 matches | `rt60(band)` equals the requested RT60 per `g = 10^(-3m/(RT60 fs))` within 1e-3 relative (the design is exact; tolerance is rounding) | scale a gain by 1.02 |
| Band order | `rt60(low)`, `rt60(mid)`, `rt60(high)` ordered as the controls ask (designs with bands) | swap two gains |
| Tail monotone | `tail(floor)` non-decreasing in decay and size; lower floor means longer; pre-delay adds exactly its length | drop the pre-delay term |
| Lossless iff freeze | `is_lossless()` exactly when `freeze`; `tail` is `Unbounded` then, and only then | leave one band at 0.999 |
| Density | `modal_density` grows with size; `echo_density` grows with diffusion and mixing; ordered, not absolute | ignore `size` |
| Bounds dominate | `bounds()` >= `design()` in every length, line count and tail, for every reachable control | undersize `bounds` by one line |
| Pure | Same controls, same `Network`; non-finite controls leave the last good ones; `design` under `assert_no_alloc` | read a clock |
| No direct path | `D = 0` in every network | add a direct gain |
| Pre-delay | `pre_delay` in frames equals `round(pre_delay · fs)` | off by one |

### 8.2 Kernel realises network, `kernel_contract!`

Run over every `Kernel` with its `Design`'s networks, in `tutti-dsp`. The kernel
declares a tolerance per query (FDN 5%, Freeverb 10%).

| Case | Figure | Mutation record |
|---|---|---|
| RT60 | An impulse's Schroeder-integrated decay, per band, matches `network.rt60(band)` within the kernel's tolerance | scale the kernel's loop gain by 1.02 |
| Tail covers | Measured time from input end to each floor (-40, -60, -90 dB) <= `network.tail(floor)` | halve `tail` |
| Pre-delay | First non-zero output at `network.pre_delay` frames; `latency()` unchanged by it | off by one in the line |
| Stability | 60 s of noise at the extreme controls (max decay, max size, modulation at its maximum): finite and bounded when `is_stable()`; freeze: neither growth nor loss beyond a stated figure | unity-plus loop |
| Band order | Measured band decays ordered as `network` says | swap the shelves |
| Width | mono in, stereo out: inter-channel `Correlation` below 0.3 at width 1, 1.0 at 0 | give both outputs one seed |
| Wet-only | A Dirac at 0 with pre-delay > 0: exact zero before the pre-delay; wet `-inf` is exact zero | add the input to the output |
| Click-free retarget | Step decay, size, wet and pre-delay mid-tail: max sample-to-sample jump <= the steady-state bound x a small factor | apply a length unslewed |
| Reset | After `reset`, bit-identical to a fresh instance's render | skip one line |
| Fresh | `fresh()` renders bit-identically to a freshly built kernel | share one line |
| Block independence | Same input in blocks of 1, 17, 64, 512 bit-identical (time-domain kernels; the convolver within round-off, declared) | block-dependent ramp |
| Latency | `latency()` equals the measured shift of the first output sample with pre-delay 0 | return 0 |
| No allocation | `retarget` and `process` under `assert_no_alloc`, networks changing every block | n/a |

Serial versus pooled is node-level (`tests/parallel.rs` style: bit-identical to
serial). Signal helpers (Schroeder integration, band energy) go in
`tutti-dsp`'s `test_support`. Measuring RT60 on the GM Freeverb pins the analytic
figure the GM render keeps, with a tolerance; the render is not compared with the
old version.

## 9. Staged plan

Each step is a PR that passes `cargo xtask ci`. #354 merges first.

| Step | Content | After | Rough size |
|---|---|---|---|
| 0 | `tutti-types`: `UnitParam::{Decay, Diffusion}` (ids 25, 26), the `PINNED` table, the `Mix` doc line | #354 (can start now) | ~80 lines |
| 0b | `tutti-graph` (+ `tutti-export`, `tutti-types`): the dynamic tail, §12; doc 013 note | none (parallel with 0) | ~500 lines, ~60% tests |
| 1a | `tutti-dsp`: `Network`, `Stage`, `Inline`, the analytic queries, `Design`, `Bounds`, `design_contract!`, the building blocks Freeverb needs, `Freeverb`'s design (pure; no kernel change) | none (parallel with 0 and 0b) | ~1,000 lines, ~350 of them tests |
| 1b | `Kernel`, `Setup`, `Effort`, `kernel_contract!`, `Freeverb` as a `Design` + `Kernel`; `tutti-nodes`: `ReverbNode<D>`, `ReverbParams`, quiet-input counter and `report_tail`, fork by `fresh`; bevy registration; GM unchanged; the export-fork test | 0, 0b, 1a | ~1,100 lines, ~450 of them tests |
| 1c | `Fdn` (design with band absorption, orthogonal kernels, diffusion, modulation, freeze), `UnitParam::Freeze` (id 27), `ReverbKind { Freeverb, Fdn }`, registration; the FDN exercises three-band RT60, size, diffusion, modulation and the unbounded tail | 1b | ~1,200 lines |
| 2 | Convolution as a `Design` + `Kernel` (`ReverbKind` gains a variant); `DryAlign` shared | 1b | ~400 |
| 3 | Plate | 1c (shared blocks) | ~500 |
| 3b | Early-reflection generator, then as the early stage of the FDN and plate | 1b; 1c and 3 for wiring | ~450 |
| 4 | `Generic` kernel (opt-in, §4.2) | 1c | ~300 |

### Step 1a as built

Where the code made a decision the text above leaves open; the rest is as
written.

- **`Design` has no `Kernel` yet.** `type Kernel` and `fn kernel` arrive with
  `Kernel` in 1b (a placeholder trait with no methods would say nothing).
  Freeverb's design is `FreeverbDesign` with `FreeverbControls { room, damp }`;
  `Freeverb` stays the processor and is the 1b kernel.
- **`Ceiling` is `{ rate, max_decay, floor }`** (checked: `new` refuses,
  including a `max_decay` above 60 s, since bounds become kernel allocations),
  because lengths in `Bounds` need the rate and the tail needs the floor.
  A design with no decay control (Freeverb) ignores `max_decay`.
- **`design` is total**: a control out of range is clamped and a non-finite
  one takes the algorithm's default, so the node's `LastGood` is a second line,
  not the only one.
- **BLOCKER for step 3b: the series chain cannot express a parallel branch.**
  Early reflections added to a late tail (the usual shape) are not `Taps` then
  `Fdn` in series, which would convolve them. Step 3b cannot start until the
  model has a parallel `Stage` (a sum of branches) or a second list; 1a leaves
  it open and its queries (`tail`, `min_delay`, the densities) assume series.
- **No direct path is a path query**, not a field: `Network::min_delay` (a
  series all-pass counts 0, as its `-g` term is direct) and `has_direct_path`.
- **`Network` is about 56 KB** (`MAX_STAGES` times the largest stage, 7 KB
  whatever its variant), not "a few kilobytes"; a test holds it under 64 KiB.
  It is a **control-side value**, never built on an audio thread (stacks of
  512 KB): this overrides section 3's "design runs on the audio thread once per
  changed block". The node designs on the control side and hands the kernel
  what `retarget` needs; step 1b settles how.
- **Freeverb's all-pass is not the Schroeder lattice.** It computes
  `out = buf - in; buf = in + g buf` (read `D` frames late), that is
  `H = ((1 + g) z^-D - 1) / (1 - g z^-D)`, not unit magnitude (gain 1 at DC for
  `g = 0.5`, `(2 + g) / (1 + g)` where `D w = pi`), with a direct term of `-1`.
  `AllPass` carries an `AllPassForm::{Lattice, Freeverb}` so the network
  describes Freeverb's filter and not a neighbour of it; step 1b's kernel
  contract can then hold the kernel to its network exactly. Queries that assume
  unit magnitude (lossless loops, the nested-diffuser delay) accept only the
  lattice; the direct path counts each form's own term. The network still
  leaves out the comb's and all-pass's flush of values under 1e-6.
- **`rt60` is exact only for a diagonal `A` or one RT60 on every line**; for a
  mixing `A` it is the length-weighted mean, an estimate that undercuts the
  true decay when the lines differ (a simulated 40- and 70-frame pair set for
  3 s and 1 s: estimate 1.32 s, state below -60 dB only after 1.9 s). `tail` and
  `Bounds.tail` therefore use `rt60_bound`, the slowest line's per-frame decay
  `max_i g_i^(1/m_i)` (times `max(||A||, 1)`), with the lag of one longest line:
  the proof is a weighted-energy argument written on `Fdn::rt60_bound`. Lines
  with a nested all-pass are read at its peak group delay, a conservative
  reading outside the proof.
- **`is_stable` is a sufficient test with named failure modes closed:** a
  one-pole needs a finite `|damp| < 1` (its `gain()` reads infinity otherwise),
  every line a finite positive length (no delay-free loop), a nested all-pass a
  lattice with `|g| < 1`, and the orthogonality tolerance (1e-6) is folded into
  the bound (`sqrt(1 + n e)`) rather than ignored.
- **A one-pole's three bands** are its response at DC, a quarter of the rate
  and Nyquist. `tail` counts an all-pass by its geometric train (a delay times
  the trips to the floor), more than `Freeverb::ring_out`'s once-count, and
  takes the transit from the longest effective line.
- **`Tap` has a `channel`, not a pan** (`Pan` is a reading, not a control).
- **Units:** non-negative gains per trip are `Amplitude`, whole frames
  (`pre_delay`) `Samples`; `f32` stays for fractional frames, signed
  coefficients (taps, all-pass gain, the matrix, FDN input and output gains)
  and where `Feedback`'s 0.99 cap cannot hold a loop gain of 1.
- **`echo_density(t)` is echoes per second** from the loops alone: a diagonal
  sums `1/tau` over the lines that have come round; a mixing matrix gives
  `N^k / tau_mean` after `k` trips, capped at the rate.
- **The no-allocation half of "pure"** lives in `tests/network_alloc.rs`
  (a counting allocator needs `unsafe`, which the crate's unit tests cannot
  have). The other cases are `network::contract` (crate-private, `cfg(test)`),
  run over Freeverb and a test-only `ToyFdn` that has the controls Freeverb
  lacks; each case also has a deliberately defective design it must fail.

### Step 1b as built

Where the code made a decision the text above leaves open, or overrode it; the
rest is as written. The modulation findings the owner asked for are the last
list.

**API.** `tutti-dsp`: `Kernel` (`prepare(&Bounds, &Setup)`, `retarget(&Network)`,
`process(&[&[f32]], &mut [&mut [f32]])`, `reset`, `latency`, `effort`,
`fresh`), `Setup { rate, max_block, input, output }`, `Effort`, `Channels`;
`Design` gains `type Kernel`, `fn kernel`, `fn channels` and `fn design_into`;
`Network::clear` and `Inline::clear` build a network in place; `Freeverb` is
`FreeverbDesign`'s `Kernel` (the type itself, not a wrapper: one copy of the
comb and all-pass loops). `tutti-nodes`: `ReverbNode<D: ReverbDesign>`,
`ReverbDesign` (a `Design` and its `Params`), `ReverbParams` (the `Param`
cells, their addresses, how a block reads them), `FreeverbParams` and
`type FreeverbNode = ReverbNode<FreeverbDesign>`. `ReverbNode::new(design,
controls)` is a live node, `fixed` one whose controls never change, and
`general_midi()` the fixed Freeverb at room 0.5 and damping 0.5 that the
SoundFont player's `add_general_midi` inserts (`ReverbNode::new()` with no
arguments is gone; its three callers say `general_midi()`). `DryAlign` moved
from the convolver's file to `tutti-nodes/src/dry_align.rs` and takes a
latency of zero. `tutti_graph::contract::Driven` gains `status`, so a test can
see `Status::Idle`.

**Deviations from the text.**

- **`design_into` is the required method and `design` the provided one.** A
  `Network` is 56 KB, so a design that returns one by value cannot run on an
  audio thread (§9 step 1a said so, and said step 1b would settle how). It
  writes into a network the node owns, pushing stages and lines into it
  (`Network::clear` first); `tests/network_alloc.rs` runs it on a 96 KiB stack,
  where building a `Network` value (about three times its size in an unoptimised
  build) overflows. The default `design` builds the value for the control side,
  a test and a UI.
- **The audio thread designs; nothing is published through `ArcLend`.** The
  brief was to design on the control side and publish. It does not survive
  modulation: a lane writes the node's *port*, never its cell, so the audio
  thread is the only place that ever sees a modulated value. The cells are also
  written by anyone (`Graph::set_param`, a UI, a fork's snapshot) with no hook
  to run a control-side design on. A control-side tick that polled the cells
  would make a render depend on when `update()` ran and would not see a lane at
  all. So the node reads its cells and ports once per block, designs *in place*
  when the controls changed (a bitwise compare with the last block's), and
  retargets; the network is the node's, never shared, so there is nothing to
  publish, retire or reclaim, and `SessionNode::reclaimer` is not implemented
  (CLAUDE.md: "derive at the point of use rather than mirror and reconcile").
  `ArcLend` will be right for a network that cannot be derived from the
  controls (a convolution's IR), which is step 2's.
- **A live node and a fixed one.** The ceiling of §10.1 is the design's worst
  case (`bounds().tail`), and for Freeverb that is room 1: 12.7 s at -60 dB, eight
  times the General MIDI room's. A send with no cells cannot reach it, and a
  SoundFont export would be stretched for nothing. So a `fixed` node declares
  the exact tail of its one network and offers the mix alone to the graph, a
  `new` node offers the room and the damping and declares the worst case.
  `FreeverbDesign` still has no range to configure (`Ceiling::max_decay` is the
  build parameter that would, when a design has a decay).
- **Freeverb declares `RoomSize` and `Damping`, not `Decay` and `Diffusion`.**
  It has neither control (§9 step 1a says so); the two kinds land with the FDN
  in 1c. `ReverbParams` is where an algorithm lists its own, and the mix is the
  node's (port 0, `UnitParam::Wet`), never an algorithm's.
- **The tail floor stays -60 dB, and the node now cuts there.** The node parks:
  past its tail it resets the kernel and returns `Status::Idle` (§10.1). The
  General MIDI send has always declared its ring-out to -60 dB, and
  `tutti-session`'s `every_stage_declares_its_own_tail` pins the figure between
  1 and 2 s, so the floor is not deeper; the cost is that the room is cut off at
  -60 dB re the level it carried, where it used to ring on to the flush. Measured
  against the old Freeverb at `input_end + tail`, what the cut discards is more
  than 100 dB below the rendered peak (peak -1 dBFS; a 2 s noise burst leaves
  about -112 dBFS RMS, a 50 ms burst -128 dBFS, an impulse nothing). A node that wants a deeper floor takes it from
  `TAIL_FLOOR`; the ceiling and the reported tail follow. `tail` is the network's
  `rt60_bound` figure, about 1.2 times the old `ring_out`.
- **The node cuts at `tail + latency`, to the frame.** `assert_honest_tail`
  wants exact silence after the reported tail, and the executor's own skip is
  block-granular and never calls `reset`, so the node scans its input for the last
  non-zero frame, owes `ring` frames after it (plus the kernel's latency, since a
  tail excludes latency), renders only that many, zeroes the rest of the block and
  resets the kernel. The reported figure is held at the longer of the old and
  new decay until it has been heard out (`ring`, §12.3's high-water rule), and a
  modulated block reports from the room that rings longest anywhere in it
  (`ReverbParams::longest`), not the last frame's.
- **Freeverb's `retarget` steps.** It copies the comb loop gain and damping off
  the network's first one-pole line into the 16 combs at once, with no ramp: the
  output is read from the past, so a step in the gain is a step in what the *next
  trip* carries, and the contract's click-free case holds it to that. It ignores
  the network's pre-delay, width and wet gain (`FreeverbDesign` never sets them)
  and refuses a loop gain outside `[0, 1)`, a damping outside `(-1, 1)` or a
  network without a one-pole.

**The kernel contract (`kernel_contract!`, §8.2).** Crate-private and
`cfg(test)` like the design contract; `tests/network_alloc.rs` holds the one
case that needs a counting allocator. Each case has a `Defect` it must fail
(`Defective`/`Flawed`, a real kernel with one defect), run by
`a_defective_kernel_fails_its_case`.

| Case | Figure | Defect it fails |
|---|---|---|
| `rt60` | Tone burst per band (DC, a quarter of the rate, Nyquist) with raised-cosine fades, then silence; the energy slope 40 to 65 dB down gives an RT60 within the kernel's tolerance of `network.rt60(band)` | every loop gain x1.05 (+39%) |
| `band_order` | Measured bands ordered as the network's, where it separates them by more than twice the tolerance | the damping dropped |
| `tail_covers` | Time from the input's end to -40, -60 and -90 dB of the loudest windowed RMS the render reached is within `network.tail(floor)` | every loop gain x1.03 (2.4 times as long at room 0.9) |
| `arrival` | A unit impulse first sounds at exactly `pre_delay + min_delay + latency()` | output one frame late; `latency()` one frame too many |
| `latency` | `latency()` equals the network's | `latency()` one frame too many |
| `wet_only` | Noise in from frame 0: exact zero before the shortest path | the input added to the output |
| `decorrelated` | Zero-lag correlation of the two channels under 0.3 | channel 1 a copy of channel 0 |
| `stability` | 60 s of noise at room 1 (damping 0 and 1): finite and under an L1 bound from the network (486 at room 1) | a gain growing 0.01% a frame (a loop gain over 1 is refused by `retarget`, so `LoopGain` cannot make it unstable) |
| `click_free` | Noise through a step of the controls: the largest jump after it within 1.5 times the larger steady state's | a gain of 4 applied whole at the second retarget |
| `line_lengths` | For a diagonal first stage followed by series all-passes only: an impulse's first echo of every line lands at `pre_delay + length` at the input gain's size (a line another line plus an all-pass delay would cancel is read by the others) | the longest comb 5% long, inside the RT60 tolerance; `arrival` sees only the shortest line |
| `reset`, `fresh` | Bit-identical to a freshly built kernel at the same controls | `reset` doing nothing; `fresh` keeping the state |
| `block_independent` | Blocks of 1, 17, 64 and 512 bit-identical | a gain that depends on the block length |

What differs from §8.2: **the tolerance is 15%, not 10%.** A bank of combs of
1116 to 1640 frames decays at the slowest comb's rate only in the end, so the
energy slope 40 to 65 dB down measures 0.89 to 0.96 of the network's `rt60` (the
slowest comb's). Deeper fits measure the kernel's flush of values under 1e-6,
which the network does not model; 15% still fails a loop gain 5% high (+25%
measured). **The tail's reference is the loudest windowed level the render
reached, not the level at the stop**: a short burst has not filled the lines, and
the output *rises* after the input stops while what is in flight comes round, so
the level at the stop understates what the network carried (measured: 8% over
the tail at -40 dB against that reference, within it against the other). The
**pre-delay, latency and wet-only cases are one arrival and one silence figure**,
and the **freeze, width, wet-gain and pre-delay directions are vacuous for
Freeverb**, which has none of those controls.

**Bit-identity of the General MIDI send.** `Freeverb::process`'s loops are
untouched and `retarget` writes the same bits the constructor does
(`retargeting_to_the_general_midi_network_changes_no_coefficient`, and 1a's
`the_general_midi_controls_give_todays_coefficients_to_the_bit`). The node-level
figure is a digest of its answer to 24 000 frames of seeded noise, recorded from
`Freeverb::process` at `f30a9d1db` before any of this
(`the_general_midi_send_renders_what_it_always_did`); the render has no `libm`
call, so it needs no golden guard. The existing `general_midi_effects` and
`tutti-session` `general_midi` suites pass unchanged (the declared tail is now
the network's -60 dB figure, the old one).

**What the reverb found in the modulation design (§10 against the code).**

- **Design cost is small, so designing every modulated block is fine.** In
  release, `design_into` is 0.2 us, `Network::tail` 1.1 us (libm `exp10` per band
  per line) and `retarget` 5 ns. A reverb under a lane that changes every block
  pays about 1.3 us per block, against 2.3 us to render 64 frames: it matters at
  tiny blocks and not at 512. `tail` is what dominates, and could be skipped in
  a block that cannot lengthen it.
- **The audio thread must be able to design.** §3 and §5 said so; 1a's 56 KB
  `Network` took it away; this step gave it back by writing in place. A
  modulation design where only the control side can compile a node's model could
  not drive this node.
- **The cadence of §10.2 held without change.** `block_driven` (the port's last
  frame) is the value of a block; the lane's grid of 32 frames and a reverb
  decay that moves over seconds agree; Freeverb needed no ramp. The ramp in
  `retarget` §10.2 asked for is the FDN's to need.
- **A tail that follows a modulated decay needs more than the last frame.**
  §12.3 says a modulated decay reports from the larger end of the block's ramp.
  The port gives every frame, but the *direction* in which a param lengthens the
  tail (more room, less damping) is the algorithm's knowledge, so each
  `ReverbParams` writes it (`longest`). That is one more method per algorithm,
  not a change to the graph.
- **A ModRange above the ceiling is harmless, as §10.1 argued, but the clamp is
  the design's, not the node's.** `design_into` clamps every control, so a port
  past 1 designs the room at 1, whose tail the bounds hold.
- **`report_tail` alone is not enough to park a reverb.** The executor never
  calls `reset`, a `Modified` node is never parked, and the skip is
  block-granular, so the node has to run its own silence scan, its own cut and
  its own `reset` before it returns `Idle`. The contract harness gives a node no
  silence masks (`SilenceMask::NONE`), so it cannot rely on `Io::silent()`
  either. All of that is the node's `process`, about 40 lines; the executor
  needed no change, and 0b's `assert_honest_tail` is what checks it.
- **Park and wake are testable without a graph** once `drive_input` returns the
  `Status` (a small addition to the contract harness), and a lying design
  (`tests/reverb_node_generic.rs`: an echo that rings past its claim, with a
  latency) is the way to test the cut and the dry alignment that Freeverb, with
  no latency and a tail longer than a block, cannot show.
- **Open for 1c.** `Network::tail` costs more than the rest of the redesign; the
  FDN's three-band `Absorption::from_rt60` and its `lerp` (§10.2) are untried;
  and `Ceiling::max_decay` is not yet a node build parameter (the node uses the
  default 8 s).

### Step 1c as built

The FDN, `UnitParam::Freeze` (id 27) and `ReverbKind`. Where the code made a
decision the text leaves open, or overrode it; the last two lists are what the
FDN teaches about the trait shape, for doc 024's step 1.

**API.** `tutti-dsp`: `FdnDesign` (`new(lines, max_decay, freezable)`, `lines`
even from 2 to 16, `Default` is 8 lines, 8 s, not freezable), `FdnControls`
(`decay: [mid, low, high]`, `size`, `diffusion`, `pre_delay`, `modulation`,
`rate`, `width`, `wet`, `freeze`; `DEFAULT`), `FdnKernel`, and
`Matrix::Reflection`. `tutti-nodes`: `FdnNode = ReverbNode<FdnDesign>`,
`FdnParams`, `ReverbKind { Freeverb, Fdn(FdnDesign) }` with `spawn()` giving a
`SpawnedReverb`, and the designs and controls that sit in a node's signature
re-exported (`FdnDesign`, `FdnControls`, `FreeverbDesign`, `FreeverbControls`).
`ReverbParams::addrs` is a method and `ReverbParams::new` moved to
`ReverbDesign::params(&self, initial)`, because the FDN's ports depend on the
design (the freeze is a port only where the design can freeze). The session
registers `FdnNode` (`tutti-session` `param_session_node!`).

**Deviations from the text.**

- **Diffusion is a reflection vector, not a Hadamard switch; Hadamard is not
  shipped.** `A = I - 2 v v^T` with `v` a unit vector that leans from one axis
  (`diffusion` 0: no line feeds another, the network says `Diagonal`) to the flat
  `1/sqrt(N)` (`diffusion` 1: `Householder`), `Matrix::Reflection(v)` between.
  It is orthogonal at every setting, so a loop never gains and a freeze is
  lossless anywhere, and it is continuous, so a diffusion step ramps (a Hadamard
  has no path from the identity that stays orthogonal). The model gained one
  variant: orthogonality is measured (`4 ||v|^2 - 1| max v_i v_j`) and the norm
  is exact (`max(1, |1 - 2|v|^2|)`), not the generic `sqrt(1 + n e)`, which at
  `e` = a rounded `f32` entry's error made a unit `v` look 5% slower to decay.
  `echo_density` still jumps between `Diagonal` and any mixing matrix; the
  contract orders it (0 against 0.5 and 1), as 1a's does.
- **The crossovers are fixed (300 Hz, 4 kHz), not `Cutoff` controls.** They
  are in the network (`Absorption::Bands.crossover`) and the kernel reads them
  from it, so a design that sets them is one line; none does yet.
- **Mono in, stereo out; even lines to the left, odd to the right.** The width
  is the kernel's mid/side on the two outputs, so `0` is exact mono and `1` is
  the unwidened sum.
- **Freeze is opt-in (`freezable`).** A design that can freeze has an unbounded
  tail ceiling (`Bounds::tail` is `Unbounded`), so every export of its node
  spends the whole cap (§12.4: export spends the ceiling). A reverb that cannot
  freeze keeps a finite ceiling, and its freeze control is not a port.
  `Bounds::tail` carries a margin for the rounding of the reflection vector:
  its entries are rounded twice (stored, then divided by the norm), 2^-23
  relative, so `|v|^2` is 1 to within 2^-22 and `|A| <= 1 + 2^-21`; `rt60_bound` applies the norm every frame (its
  proof's step), against a decay of `3 / (RT60 fs)` decades a frame, so the
  bound reads a decay slower by `x = 2 e / ln 10 * RT60 fs / 3` and the tail
  grows by `1 / (1 - x)`: 0.5% at 8 s and 44.1 kHz, 11% at 192 kHz
  (`FdnDesign::rounding_margin`, computed, not a flat figure).
- **Line lengths are whole frames** (rounded from a table of values near primes,
  scaled by `0.5 + size` and the rate), so a settled network delays by whole
  frames and a frozen one reads no fraction. Freezing turns the modulation off
  for the same reason.
- **The node's tail follows a thaw.** An unbounded ring that stops being
  unbounded rings out from that block (`ring = current`, `since = 0`), where 1b's
  rule would have kept `u64::MAX - elapsed` for good and never parked.

**The kernel.** `N` delay lines read with linear interpolation, a three-band
absorption in each loop (`y = g_high x + (g_mid - g_high) L2(x) + (g_low - g_mid)
L1(x)`, `L1` and `L2` two cascaded one-poles at the crossovers: linear in the
gains, so a ramp of the gains is a ramp of the filter, and the identity when
every gain is 1), the reflection, and the pre-delay, width and wet gain read
from the network. The wet signal is the lines' own outputs before their
absorption, so nothing sounds before the shortest line has gone round.
`effort` is `Moderate`: 2.8 us for 64 frames in release (22 ns a frame and
channel), against Freeverb's 2.4 us.

**Retargeting ramps (`retarget` only sets targets).** Every sounding parameter
(each line's three loop gains, the reflection vector, the input and output
gains, the wet gain, the width, the modulation depth) is a `Slew` that moves to
its target in 20 ms of whole 32-frame chunks and lands on it exactly (so a freeze
is exactly lossless once it has arrived). The reflection vector is renormalised to
its length every chunk (a straight line between two unit vectors dips to
`|v|^2` of 0.68 at 8 lines, and `I - 2 v v^T` of it is a contraction), so the
matrix is orthogonal at every step of a ramp and a frozen tail keeps its energy
through a diffusion move (`a_diffusion_ramp_keeps_a_frozen_tails_energy`); a
diagonal network is the reflection along line 0, a sign on one line, so the
ramp from it is a ramp of unit vectors. A size move lands over seconds by
design: half a frame per chunk, about 6 s for the longest line going 0 to 1,
to keep a sweep free of pitch artefacts; a line's length moves at most half a
frame a chunk (a pitch bend of 27 cents), the pre-delay two. The chunk counter
runs from the last reset, not from a block's edge, so blocks of 1, 17, 64 and
512 render bit for bit alike. The first `retarget` after `prepare` (and a
`reset` or `fresh`) lands at once, so a kernel built at a network is at it.

**The kernel contract on the FDN, no vacuous hook.** 19 cases run on
`FdnDesign` (freezable, 8 lines): 1b's twelve, `line_lengths` and six new; `FdnDesign` runs the design
contract twice (freezable and not). New cases, each with the defect it must fail
(`a_defective_fdn_fails_its_case`): `line_lengths` (every line's first echo at
`pre_delay + length`; the last line 5% long, inside the RT60 tolerance),
`pre_delay` (the first sound moves by exactly its frames, `latency()` unchanged;
pre-delay dropped), `size` (a bigger room sounds later; all lines one length),
`diffusion` (more diffusion fills at least 0.15 more of the frames of the
0.15 to 0.3 s window; no mixing), `freeze` (neither growth nor loss beyond 0.25
dB in 8 s, and no input reaches it; a loop gain of 0.998, an open input),
`width` (natural: correlation under 0.3; none: over 0.999; width ignored),
`wet_gain` (the level ratio to 2%, an exact zero for -inf; wet gain ignored).
`no_hook_of_the_fdn_is_vacuous` fails if a hook returns nothing. Freeverb still
leaves those hooks empty (it has none of those controls).

What changed in the harness, and why:

- **Tolerance 15%.** The band decays measure within 0.3% (low), 3 to 6% over
  (high) and 4 to 12% under (mid), the mid because the first-order skirts of the crossovers leak
  the neighbouring bands' gains into a tone at 1.1 kHz. A second-order split
  would halve it.
- **The fit is deep and the windows long.** A comb bank decays in steps (a line
  at a time), and a 1024-frame window measures where the steps fall: parallel
  combs (`diffusion` 0) measured 40% slow. Windows are twice the longest line,
  stepping by 1024, fitted from 20 to 100 dB down (`KernelCase::fit_depth`;
  Freeverb keeps 40 to 65, where its flush of values under 1e-6 starts).
- **`arrival`, `wet_only`, `latency` and `line_lengths` run on `static_probes`**
  (no modulation): a modulated line sounds off its centre figure. 1b's `arrival`
  summed `pre_delay` and `min_delay`, which already includes it (the pre-delay
  was 0 until now).
- **`click_free` plays a 150 Hz tone and allows 3 times the steady jump.** Noise
  hid a jump in a delay line (its steady jumps are as large as any click); the
  tone made a gain taken whole and a length taken whole fail (checked by hand:
  `ramp_chunks = 1`, `LENGTH_STEP = 1e6`, `PRE_STEP = 1e6`). The factor went from
  1.5 to 3 because Freeverb's tone reaches 2 times the steady jump at a step of
  its loop gain; the 4-times gain defect still fails it by a wide margin, and so does a
  modest unslewed 1.5 times (both kernels' tests run it). Measured steady-jump
  multiples of the clean kernels: Freeverb 1.98, the FDN 1.51, so 3 leaves each
  clean kernel a margin of 1.5 (Freeverb) and 2 (the FDN) over what it measures.
- **The stability case's bound** is `|B| m / (1 - g)` times `|C|`, the width and
  the wet gain (loose, but a runaway gain is infinite); the grid peak scan in
  `retarget` is what keeps a non-monotone set of band gains from exceeding 1
  (checked: removing the scaling fails `kernel_contract_stability`).

**What the FDN found in the modulation design (§10 against the code).**

- **Linear interpolation takes the top band when a line is modulated.** The
  decay at Nyquist, measured with the modulation at a depth of 0.5, was 0.46 s
  for a requested 1.2 s: a fractional delay of one half cancels a tone at
  Nyquist, and the modulation visits every fraction. §3.1's "a fraction of a
  percent" is true of the low and mid bands and false of the highest. The band
  cases measure unmodulated; `tail_covers` keeps the modulation (a shorter ring is
  covered). A better read (all-pass or Hermite) is the remedy.
- **Design cost is small; `retarget` can be large.** In release, `design_into`
  288 ns, `Network::tail` 423 ns, `retarget` 71 ns. The first `retarget` measured
  15.6 us, because it scanned the response of every line for a peak above 1; it
  now scans only when the triangle bound `|g_high| + |g_mid - g_high| +
  |g_low - g_mid|` exceeds 1 (a non-monotone set of gains). A modulated FDN
  redesigns once a block, about 0.8 us against 2.8 us to render 64 frames.
- **The lane's grid and the kernel's chunk are two grids, and one is enough.**
  The lane sends a point every 32 frames and the node reads the last frame of the
  block; the kernel ramps on its own 32-frame chunks from there. Nothing needed
  `retarget` more often than a block.

**What the FDN says about the trait shape (for doc 024).**

- **Ramping lives in the kernel, between `retarget`s.** `retarget` samples
  `theta` once a block and the kernel interpolates to it over its own chunks, as
  doc 024 section 4.5 says. The chunk (32 frames) is an internal grid, not a
  retarget cadence: a `Cadence::Every(32)` would design (288 ns) and `tail` (423
  ns) every chunk for nothing the kernel needs. `Cadence::Block` covers the FDN.
- **The ramp's clock must be the stream's, not the block's.** Chunks counted
  from a block's edge make the output depend on the block length;
  `block_independent` (blocks of 1, 17, 64, 512) is what found it, and any kernel
  that ramps needs the same counter.
- **"Landed" is part of the contract.** A kernel built and retargeted once must
  equal one that settled (`reset`, `fresh`), and a ramp must arrive exactly (a
  freeze is lossless only if the gains are exactly 1): the first `retarget` after
  `prepare` lands, later ones ramp. The trait says neither today.
- **Some of the harness is the model's `response`.** `fit_depth`, `tone`,
  `static_probes` and the 15% tolerance are the contract working around the
  difference between the model's response and the kernel's (the skirts, the
  interpolation's droop). A `response` query on the model, compared with the
  kernel's frozen response, would find each directly and make the tolerance a
  figure of merit.
- **The node's tail logic is not the reverb's.** The silence scan, the cut at
  `tail + latency`, the reset-and-park and the high-water rule are generic over
  any kernel with a ring-out; the FDN's freeze (unbounded, then thawed) used the
  same code as Freeverb's once the thaw was handled.
- **Vacuity is checkable.** A hook that returns nothing makes its case pass
  without running; `no_hook_of_the_fdn_is_vacuous` asserts non-vacuity per
  design, which a shared contract could ask of each design that declares a
  control.

Step 1 is three PRs. The old plan's two were ~2,200 lines together; the model
adds the `Network` and its queries (~1,000 lines with their tests), which is
reviewable on its own and needs no audio, so it comes first and can start in
parallel with 0 and 0b. 1a ships no kernel change, so the GM render cannot
move. 1b is the first PR that touches the graph; 1c is where the choice becomes
real (two algorithms, the enum). After 1b, 1c, 2 and 3b are independent files;
3 waits on 1c only for shared blocks.

## 10. What the reverb asks of modulation

The owner asked whether tutti's modulation design survives a real, demanding
node. Each item is answered against the code on `main`; `tutti-modulation` is renamed
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

### 10.1 A tail that depends on a modulated param

- **Is `Shape`'s tail fixed at prepare?** Yes. `Node::shape` "must not change
  after `prepare` without the node being re-inserted — a shape change is a
  recompile", and the compiler reads shapes at `prepare`/insert
  (`Editor`'s re-prepare path). A node cannot update its tail at runtime.
- **What does the node declare?** The worst case over the range it can be
  driven to. The node cannot read the `ModRange` (a `bevy-tutti` declaration
  that becomes the port's `ParamRange` in the spec; the node never sees it), so
  it must not trust it: the node declares from its own ceiling (`bounds().tail`,
  from `max_decay`, a build parameter) and clamps the value it reads to that ceiling, whatever the
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
  `Network::tail` at the ceiling, it calls `Kernel::reset()` (the state is below the
  floor, now exactly zeroed) and returns `Idle`; the next block with input
  wakes it by the executor's rule. The counter lives in the node, not the trait,
  so the trait stays a processing contract. Smallest change: none in
  `tutti-graph`. The one test is that an `Idle` reverb equals a fresh one after
  it wakes.
- **If a modulated decay could be known at build** (a lane that is a plain
  automation curve), a tighter tail would be possible by re-inserting the node.
  Not worth it; the ceiling is one number.

### 10.2 Costly-to-apply params

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
  it becomes the documented convention for `Kernel::retarget` (§11), plus a
  `lerp` on the absorption design.
- **Non-finite writes.** A raw cell accepts NaN; every per-block read goes
  through the nodes' `LastGood` (`ramp.rs`). The reverb's `Controls` builder (`ReverbParams::controls`) does
  the same, which is also why the contract's stability case includes non-finite
  controls.

### 10.3 Structural params (size)

- **Bounded maximum at prepare?** The node needs the maximum size when it
  allocates the lines, in `prepare`. The param system does not hand it one:
  `Node::prepare(&Prepare)` carries rate and block size, and a `ParamRange` is a
  per-port clamp in the spec that the node never reads. So the maximum is the
  node's own build parameter, as `DelayLineNode` already does (`max_delay`, then
  `delay_secs` clamped to it, in `delay.rs`). A reverb takes it from `Design::bounds` the same
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

### 10.4 New `UnitParam` kinds

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

### 10.5 Bevy and the session

Yes, with no reverb-specific code. `ParamRef::unit_at(entity, UnitParam::Decay, 1)`
names the low-band RT60; `Mods`, `ModRoute` and `ModRange` take any `ParamRef`;
the node declares only the params it has (`ParamSet` builder), and
`Shape::with_param_addrs` lists the modulatable ones as it does for the
multiband compressor's per-band thresholds and the crossover's splits (the
tests `tutti-graph/tests/indexed_params.rs`, `bevy-tutti/tests/indexed_param.rs`
and `tutti-session/tests/common/bands.rs` show the indexed path end to end). The
number of modulatable ports is capped at `MAX_PARAM_PORTS` (32), well above the
FDN's dozen params, so the cap does not bind.

### 10.6 What the exercise found

| Finding | Smallest change |
|---|---|
| The tail is fixed at prepare, the node cannot see a `ModRange` | A ceiling in `Shape::tail`, plus a per-block report (§12, step 0b) |
| `Idle` is the node's call and `Modified` is never parked | A quiet-input counter in the node, reset and `Idle` after the tail |
| Per-block last-frame reads plus the node's own ramp is the cadence | Document it as `retarget`'s convention (§11); add `lerp` to the absorption design |
| A node cannot learn a `ParamRange` at prepare, and should not | `Design::bounds` (§3.2): the design's own ceilings, from its build config |
| New kinds are append-only ids with one pinned table | Append `Decay`, `Diffusion` (step 0) and `Freeze` (with the FDN, step 1c) |
| `MAX_PARAM_PORTS` is 32 | Does not bind; an FDN has about twelve params |

## 11. Where `retarget` fits the modulation design

The modulation design (doc 014) is "the API compiles, a node evaluates", and the
reverb follows it with the same split. A lane writes the param value; the node
reads its `Param` cells once per block into `Controls`; `Design::design` is the
pure mapping from controls to a `Network` (the "compile"); `Kernel::retarget`
is where the node evaluates it, per block, after the lane's frames are in. So:

- **Cadence.** One `design` per block whose `Controls` changed (a bitwise
  `PartialEq` against the last; an unmodulated, unchanged reverb designs once).
  `retarget` runs then, and not otherwise.
- **Smoothing is `retarget`'s.** It ramps gains from where the last block ended,
  interpolates coefficient sets (`lerp`), and slews lengths at a bounded rate
  (§10.2, §10.3). The lane supplies a shape; the kernel decides what is smooth.
- **Ranges.** A kernel's maximum sizes come from `Design::bounds` (§3.2), which
  is the node's own ceiling and not the `ModRange`; the node clamps the value it
  reads to it.
- **The tail.** `Network::tail` is the reported tail (§12); the ceiling is
  `bounds().tail`.
- **Rows 10.1 to 10.5 are unchanged.** What changed is that the node's tail,
  stability and ceiling now come from the `Network`, not from three separate
  hand-written functions.

## 12. The graph's dynamic tail (step 0b)

Owner decision, 2026-11-08: the graph re-reads the tail. The worst-case
workaround is rejected. This is the smallest change to `tutti-graph` that lets
a node's tail follow its params at runtime, with the plan immutable.

### 12.1 What the graph does with a tail today

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

### 12.2 Options

| | A. `Status` carries the tail | B. Ceiling in `Shape`, node reports each block (recommended) | C. `Tail::Dynamic { max }` + report | D. Tail published as a control cell |
|---|---|---|---|---|
| Change | `Status` is returned by every node; add a field | `Shape::tail` keeps its meaning as an upper bound; `Io::report_tail(Tail)` | A new `Tail` variant in `tutti-types`; same report | A shared atomic in the shape, written by the node, read elsewhere |
| Churn | Every node and `Status` match in the workspace | Additive: nodes that do not report keep today's behaviour | `then`, `beside`, `from_*`, every `match` on `Tail`, serde | The shape holds shared mutable state; `Eq`/`Hash` on shapes |
| Plan | untouched | untouched | `Dynamic` flows into `park_after`, `verify` | untouched, but a cell in each rec |
| Audio thread | free | one store into the unit record | same as B | an atomic load per block per reader, cross-thread cache line |
| Pooled = serial | yes | yes (below) | yes | yes |
| Control-side read | none | via an executor view (12.4) | via the same | direct, but of a value the audio thread wrote and the control side cannot tie to a block |

**Recommend B.** A is a workspace-wide signature change for a figure only a few
nodes vary. C adds a variant that the ceiling already expresses (`Finite(max)`
is "at most this long"). D puts shared mutable state in a value that is
compared and hashed and violates "nodes communicate only through ports", for a
number the executor needs privately.

### 12.3 The mechanism

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
- The reverb node reports `network.tail(floor)` (§3.1) after `design`, in
  `process`, every block; `Ring::Unbounded` (a lossless freeze loop, §2.5) is
  reported as `Tail::Unbounded`, so freeze needs no special case.

### 12.4 Export: the tail at the render's end

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

**Settled by step 0b: the fallback is built, the second phase is not.**
`RenderConfig::tail` is a frame count fixed before the render, and
`RenderPlan::new`, the sink, the progress total, the resampler and the
normalized two-pass render all read it, so a tail known only at the
programme's end needs a new config mode across `tutti-export`,
`tutti-session` and `bevy-tutti`, not a change to the pull loop. Export spends
the ceiling against `cap`, as today; `tutti-export`'s
`export_spends_the_ceiling_of_a_node_that_narrows_its_tail` pins it. The
executor-side view and `reported_tail_now()` wait for the reverb to need them.

### 12.5 Doc 013 change note

Step 0b adds a note to doc 013's contract section ("A tail excludes latency"):
`Shape::tail` is an upper bound; a node may narrow it per block with
`Io::report_tail`; the executor and export read the narrowed value, the plan
and spec the bound; the reverb is the first user. It is recorded there with the
step, not before.

### 12.6 Tests for step 0b

A node that reports a shorter then a longer tail than its ceiling; the skip
waits the reported figure (`tests/executor.rs`, beside
`the_skip_waits_for_latency_plus_tail`); an excess report is clamped; `Unbounded`
is never skipped; serial and pooled renders are bit-identical with reporting
nodes (`tests/parallel.rs`); export of a reverb-like node renders the tail at
the end, not the ceiling. Mutations: ignore the report (fails the shorter
case); skip the clamp (fails the excess case).

## 13. Decisions and open questions

Decided by the owner on 2026-11-08, in this doc's terms:

| Question | Decision |
|---|---|
| Tail with a live decay | The graph re-reads the tail: §12, step 0b. The ceiling is `bounds().tail` |
| Fork: fresh or cloned state | `fresh()`; the graph's preroll warms it. The export-fork test that pins it ships in 1b |
| Enum convenience | `ReverbKind` ships in 1c with the FDN beside Freeverb; it only spawns the matching node. No engine-level "auto" |
| Second algorithm | The FDN, in step 1, because it exercises the model and the runtime tail |
| Freeze | With the FDN (1c): `UnitParam::Freeze`, and `Tail::Unbounded` through §12 |
| Model | A state-space network (`design -> Network`), pure and analytic, with a kernel per algorithm and no shared engine (§2 to §4) |
| Generic or boxed node | Generic `ReverbNode<D>` (§5.1) |

Open, with a recommendation:

| # | Question | Recommendation |
|---|---|---|
| 13.1 | Is a shared generic engine worth offering? | Yes, as an opt-in `Generic` kernel, later (step 4), never the default or the oracle (§4.2) |
| 13.2 | `Network` fixed capacity, or a heap `Vec`? | Fixed (`MAX_LINES` 64): `design` runs on the audio thread. A reverb needing more lines is out of scope |
| 13.3 | How far does the `Network` go for the plate's nested all-pass-in-loop and time-varying lengths? | Describe both (`Line::diffuser`, `Line::modulation`) and analyse at the *centre* lengths, with the nested all-pass's low-frequency group delay added to the line's effective length. The queries are then exact for the static design and approximate within the modulation excursion over the length; the kernel contract measures the modulated case with modulation at its maximum, so a drift is a failure, not a claim. Do not model modulation analytically beyond that |
| 13.4 | Where does `design` allocate if it needs a big `Dense` matrix? | `Dense` is capped (`MAX_DENSE`) and a control-side tool; shipped designs use structured kinds |
| 13.5 | Which algorithms after step 1? | Convolution, plate, early reflections (§6) |
| 13.6 | Chorus and the other modulation effects? | Later, once reverb has two or three users; the `Design`/`Kernel` split may well generalise, but extract after the second use, not before |
| 13.7 | `Diffusion` and `Size` as `Mix`? | Yes, with a `Mix` doc line (§2.2) |
| 13.8 | Does `ConvolverNode` stay? | Yes in step 2; thin it onto the same kernel |
| 13.9 | Does `Param<U>` smooth? | No: the kernel ramps from where the last block ended (§10.2) |
