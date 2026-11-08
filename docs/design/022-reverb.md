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
`tutti-mod` fell into, in a new place. It would block: fixed-N matrix kernels and
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

Each step is a PR that passes `just ci`. #354 merges first.

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
- **`Ceiling` is `{ rate, max_decay, floor }`** (checked: `new` refuses),
  because lengths in `Bounds` need the rate and the tail needs the floor.
  A design with no decay control (Freeverb) ignores `max_decay`.
- **`design` is total**: a control out of range is clamped and a non-finite
  one takes the algorithm's default, so the node's `LastGood` is a second line,
  not the only one.
- **The series chain cannot express a parallel branch.** Early reflections
  added to a late tail (the usual shape) are not `Taps` then `Fdn` in series,
  which would convolve them. Step 3b needs a parallel `Stage` or a second
  list; 1a leaves it.
- **No direct path is a path query**, not a field: `Network::min_delay` (a
  series all-pass counts 0, as its `-g` term is direct) and `has_direct_path`.
- **`Network` is about 55 KB** (`MAX_STAGES` times the largest stage), not
  "a few kilobytes"; a test holds it under 64 KiB.
- **A one-pole's three bands** are its response at DC, a quarter of the rate
  and Nyquist. `tail` counts an all-pass by its geometric train (a delay times
  the trips to the floor), more than `Freeverb::ring_out`'s once-count, and
  takes the transit from the longest effective line.
- **`Tap` has a `channel`, not a pan** (`Pan` is a reading, not a control).
- **`echo_density(t)` is echoes per second** from the loops alone: a diagonal
  sums `1/tau` over the lines that have come round; a mixing matrix gives
  `N^k / tau_mean` after `k` trips, capped at the rate.
- **The no-allocation half of "pure"** lives in `tests/network_alloc.rs`
  (a counting allocator needs `unsafe`, which the crate's unit tests cannot
  have). The other cases are `network::contract` (crate-private, `cfg(test)`),
  run over Freeverb and a test-only `ToyFdn` that has the controls Freeverb
  lacks; each case also has a deliberately defective design it must fail.

Step 1 is three PRs. The old plan's two were ~2,200 lines together; the model
adds the `Network` and its queries (~1,000 lines with their tests), which is
reviewable on its own and needs no audio, so it comes first and can start in
parallel with 0 and 0b. 1a ships no kernel change, so the GM render cannot
move. 1b is the first PR that touches the graph; 1c is where the choice becomes
real (two algorithms, the enum). After 1b, 1c, 2 and 3b are independent files;
3 waits on 1c only for shared blocks.

## 10. What the reverb asks of modulation

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

Unresolved by reading: how `RenderPlan`'s pull loop is structured around
`total`; if the second phase is awkward the fallback is the ceiling against
`cap`, today's behaviour. Step 0b settles it with a test.

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
