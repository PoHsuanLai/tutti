# 024: design, model, realization

[Doc 022](022-reverb.md) gave the reverb a three-part shape: a **design** (a pure
function from typed controls to a model value), a **model** (a value with
analytic queries: ring time, stability, latency) and a **realization** (the
hand-written runtime that realizes the model), with a contract suite that holds
each design to the model's invariants and each realization to the model. PR #367
(merged) added the generic node around it, `ReverbNode<D>`: redesign in place
on the audio thread, the tail report, `longest()`, parking and fork.

Nothing in that shape is about reverb, and the reason is mathematical, not
engineering. Each of those three parts is a standard object:

- a **design** is a *parameter map*, `theta = D(controls, rate)`;
- a **model** is a *system* `S_theta`, taken up to equivalence: the class of all
  realizations that have the same input-output map, queried by that map's
  invariants (its transfer function, its static curve, its spectrum) and never by
  its internal form;
- a **realization** is *one realization* of that class: a state-space structure
  (transposed direct form II, a trapezoidal state-variable filter, a lattice, a
  64-line feedback delay network) that happens to compute the map.

So the queries belong to the **mathematical class** of the system, not to the
algorithm. Every linear time-invariant (LTI) system has a response, a spectral
radius, a slowest decay and a latency, whether it is a reverb, a biquad or a
convolver. Every memoryless nonlinearity has a curve, a DC value and harmonics.
Every autonomous generator has a period or a duration and a spectrum. This design
sorts tutti's algorithms by that class, derives which of them the shape fits from
the class, and makes the class the unit the traits and the contract are written
for. It is also the producer side of two gaps in
[doc 023](023-plugin-capabilities.md): G8 (a curve response) and G9 (a readout),
with G5 (quality) as an engineering input.

**The owner's constraint, which shapes everything below** (2026-11-09): the
abstraction must not force a runtime mechanism that makes algorithms hard to
optimise or gives them a suboptimal fixed shape to write; tutti hit that once in
older versions of the modulation crate (doc 022 section 4.1, doc 014 section 1).
So **a class is a trait over structured representations, never one forced data
type.** A 64-line FDN answers as an LTI system *through its own structure* (its
delay lines, its matrix, its gains), not by being expanded into poles and zeros; a
biquad answers through its five coefficients. There is no universal model, no
generic interpreter, and no per-sample dispatch.

How to read this against the code: everything is on `main`, which now includes
PRs 365, 366 and 367 (the reverb realization and node), and is cited by path. Types
this document proposes are marked **(proposal)**. The traits in section 4 were
type-checked in a scratch crate outside the repo (two realizations of one biquad
run the same class case; a case bounded on `Lti` refuses a model without it; a
`Series` and a `Parallel` of `Lti` models are `Lti`; a `Wiener` is a `SteadyState` and the `Memoryless` case refuses it);
nothing else here has been built, and no figure is measured except those doc 022
already measured.

## Summary

- **Six processor classes and two non-processor ones** (section 2.2), by four
  axes (linear or not, memoryless or stateful, time-invariant / slowly varying /
  multirate, driven or autonomous): (1) LTI, with order 0 a gain matrix; (2)
  time-varying, a linear parameter-varying system along a known trajectory `theta(t)`; (3) memoryless nonlinear, a
  static map; (4) stateful nonlinear, a composition of (3) with (1)/(2); (5)
  autonomous generators; (6) multirate or time-scale. Plus measurements
  (functionals) and event or discrete logic.
- **Inventory (section 3): 54 rows, sorted by class. 24 fit, 14 fit in part, 16
  do not.** The verdicts follow from the class: a row fits when its whole
  behaviour is one class (or a composition of them) whose queries are exact for
  it; in part when a piece (a hold timer, a windowed minimum, a `tanh`, a
  variable-rate realization) is outside the class; not at all when it is a measurement,
  event logic or a primitive.
- **Three levels, each a different mathematical object** (section 2.1):
  `Design` is a parameter map; the **model** is a system up to equivalence, held
  by a *class trait* (`Lti`, `Memoryless`, `Autonomous`, ...); a `Realization<M>` is one
  realization, and "realization realizes model" is the realization check. TDF-II, the
  state-variable filter and a lattice are three realizations of one `Lti` model.
- **Queries hang off the class** (section 4.2): `System` (latency, tail; every
  processor), `Lti` (response, spectral radius, `l1_norm`, `hinf_norm`, dc gain, stability),
  `Memoryless` (curve, DC, harmonics, `lipschitz`, `max_gain`; memoryless only), `SteadyState`
  (class 4: steady-state characteristic, time constants, peak gain), `Autonomous` with `Periodic`,
  `Piecewise` and `Noise` (frequency, spectrum, durations). `Readout` is on the
  realization (it observes state, `y = C s`).
- **A composition algebra** (section 4.9): `Series`, `Parallel`, `SeriesN`,
  `Wiener`, `Hammerstein`, `GainControl`, each a type-level combinator over models that
  answers its class's queries from its parts with a stated bound; no runtime
  graph. It resolves doc 022's step 3b blocker (early reflections in parallel
  with the FDN tail) and the old Q4 (a series EQ chain's curve).
- **The engineering layer is labelled and kept apart** (section 4.4): `Bounds`,
  `Ceiling`, `Effort`, `Cadence` and `Quality` size and schedule a realization;
  they are not properties of the math object. `Cadence` is the time grid on which
  `theta(t)` is sampled, with its anchor (section 4.5); the *law* by which `theta` moves
  between grid points is not engineering but part of the model (section 4.5a).
- **Amendments of 2026-11-11, from the IAMF stress test (section 4.10).** IAMF's
  linear pipeline was tried against this design and showed three places where it was
  weaker than it needed to be; each is fixed by generalising an item that exists, and
  the special cases it replaces are deleted, not kept beside it. (A1) `Cadence` becomes
  a period and an anchor (section 4.5). (A2) `Controlled` becomes a model whose controls
  are a trajectory with a law (section 4.5a); the planned `Ltv`, `Variation` and
  `Scheduled` are not built. (A3) `Fold` and `ChannelTopology` carry a basis and a
  metric (section 4.2, order 0); the planned `GainMatrix` trait and the separate
  `Ambisonic` topology are not built. (A4) the periodic view of class 6, theory only
  (section 4.2). (A5) a cap that bounds a design is a `Bounds` field (section 4.4).
  Steps 2b0 and 7a of section 10 carry them.
- **The contract is written once per class** (section 6): every LTI realization
  inherits the impulse-response-against-response, ring-time-against-decay,
  linearity and shift-invariance checks; every static map inherits the level
  sweep. Composites inherit their class's cases for free.
- **`ReverbNode<D>` becomes `DesignNode<D>`** (section 5), with `Blend`,
  `Readouts` and `CADENCE` as associated types, not runtime flags. The first
  finding against the reverb-only node is the cadence: filters solve every 16
  frames inside a block, and the dynamics nodes take per-frame lanes.
- **Plan (section 10): a step 0b (the analysis split and the first consolidations, section 9) and seven steps after the merged #367, each a PR.**
  Generalise on the reverb (answering as `Lti`); filters (several realizations of
  one H(z); G8); composition (the parametric EQ, then reverb 3b); dynamics (G9);
  delay, modulation effects and the waveshaper; convolver and HRTF; the rest. The
  amendments above enter as step 2b0 (A1, A2: before or with 2b) and step 7a (A3: before the
  panners).

## 1. Current state

What 022 built, and which of it is reverb in name only. Paths on `main` unless
marked "on `main` (#367)".

| Piece | Where | Reverb-specific? |
|---|---|---|
| `Network`, `Stage`, `Inline`, the queries (`rt60`, `rt60_bound`, `tail`, `is_stable`, `is_lossless`, densities, `min_delay`, `latency`) | `tutti-dsp/src/network.rs`, `network/queries.rs` | The model: yes. `tail`, `is_stable` and `latency` are what every family has |
| `Ring { Finite(Seconds), Unbounded }` | `network/queries.rs` | No: every family's tail |
| `Design { Controls, design_into, design, bounds, channels }`; also `type Realization`, `fn realization` (PR 1a renamed them from `Kernel`, `kernel`) | `network/design.rs` | Names `Network` and `Bounds` (the reverb's) in its signatures |
| `Ceiling { rate, max_decay, floor }` | `network/design.rs` | `max_decay` is; `rate` and `floor` are not |
| `Bounds { lines, stages, taps, outputs, line, delay, ir, pre_delay, tail }`, `Bounds::of`, `dominates` | `network/design.rs` | Every field but `tail` |
| On `main`: `Realization` (non-generic; PR 1a renamed it from `Kernel`, and step 1 / PR 1b makes it `Realization<M>`), `Setup { rate, max_block, input, output }`, `Effort` | `network/realization.rs` | `Realization` names `Network` and `Bounds`; `Setup` and `Effort` are not reverb at all |
| `design_contract!`, `DesignCase`, `Mutant`; `realization_contract!`, `RealizationCase`, `Defect` (PR 1a renamed them from `kernel_contract!`, `KernelCase`) | `contract/design.rs`, `contract/realization.rs` (a crate-root `#[cfg(test)] mod contract` since PR 1a) | Crate-private and `cfg(test)`; the cases mix generic ones (pure, bounds dominate, reset, fresh, block independence, click-free, latency, no allocation) with reverb ones (`rt60`, `band_order`, `decorrelated`, `line_lengths`, freeze) |
| On `main`: `DesignNode<D>`, `NodeDesign`, `ControlParams` (PR 1a renamed them from `ReverbNode`, `ReverbDesign`, `ReverbParams`), `DryAlign` | `tutti-nodes/src/reverb.rs`, `dry_align.rs` | The control path, tail report, `longest`, parking, cut-to-frame, fork and dry alignment are generic; the names are not |
| On `main`: node tests | `tutti-nodes/tests/reverb_node.rs`, `reverb_node_generic.rs` | Generic cases through a lying design |

Two things the code already does that this design builds on rather than adds.
`SvfCoeffs::new` (`tutti-dsp/src/filter/svf.rs`) and
`compute_compressor_gain_reduction` (`dynamics/curves.rs`) are **already pure
functions from typed controls to a description**; the filter and the compressor
have a design today and no model type around it. And `MultibandControls::gain_reduction_db(band)`
(`tutti-nodes/src/dynamics/multiband.rs`) is **already a readout**; it is
reachable only through that one node's own handle.

## 2. The pattern, and what must not be forced

```
controls (typed units) --design--> model (a system, up to equivalence) <--realized by-- realization (runs)
  a parameter map: pure, total        invariants of its class: H(z), f, spectrum      one state-space structure
```

### 2.1 What each level is, mathematically

| Tutti term | Mathematical object | What it means here |
|---|---|---|
| `Design` | a **parameter map** `theta = D(c, fs)` | A pure, total function from typed controls and the rate to the parameters that fix one system. It has no signal and no state |
| the **model** (`System`, `Lti`, ...) | a **system** `S_theta` up to equivalence: the class of realizations with one input-output map | Queried through that map's invariants. Two models are the same model if no query tells them apart |
| a **class trait** | the **kind** of system, with the invariants that kind defines | `Lti` has `H(z)`, a spectral radius, a slowest decay; `Memoryless` has `f`; `Autonomous` has a period or a duration |
| `Realization<M>` | **one realization**: a state-space structure `(A, B, C, D)` with a particular form | TDF-II, a trapezoidal SVF and a lattice are three realizations of one `H(z)`. A realization has state, a model does not |
| "realization realizes model" (the contract) | the **realization check**: the realization's input-output map equals `S_theta` within a stated tolerance | Measured through the class's invariants (impulse response against `H`, level sweep against `f`), never against another render |
| `retarget` and `Cadence` | **sampling `theta(t)`** | The model is a family `S_theta` and `theta` moves slowly; `Cadence` states how slowly as a number (solve at least every `n` frames) and `retarget` takes one sample of it. The realization interpolates between samples |
| `Readout` | an **observation of the state**, `y = C s` | What a running realization reports about itself (the deepest gain reduction since the last read). State, so it cannot be a query of the model |
| `Bounds`, `Ceiling`, `Effort`, `Cadence`, `Quality` | **engineering**: the size and the schedule of a realization | Not properties of `S_theta`. Section 4.4 keeps them apart from the class traits |
| `Fidelity` | the **distance from an ideal system** | The error floor of a realizable model against the ideal it approximates (aliasing of a shaper, the stopband of a resampler). A property of the model relative to its ideal |

### 2.2 The classes

Four axes sort every algorithm in tutti: **linear or not**, **memoryless or
stateful**, **time-invariant, slowly varying or multirate**, and **driven or
autonomous**. They give six processor classes and two that are not processors.

| Class | Axes | Representation | What the class answers (exactly) | In tutti |
|---|---|---|---|---|
| **1. LTI** | linear, stateful (order 0: memoryless), time-invariant, driven | rational `H(z)` (IIR) or `h[n]` (FIR); in state space `(A, B, C, D)`. Order 0 is a **gain matrix** | the response `H(e^jw)` per input/output pair, the spectral radius of `A` (stable iff below 1), the slowest decay (ring time), `l1_norm` (`||h||_1`) and `hinf_norm` (`||H||_inf`), DC gain, latency, group delay | filters at a frozen setting, delay, comb, convolver, the reverb `Network`, HRTF; at order 0 `Fold`, VBAP, pan, the bus strip, the channel sum |
| **2. Time-varying (linear parameter-varying)** | linear, stateful (order 0: memoryless), **time-varying through a known trajectory `theta(t)`**, driven | a family `S_theta` with `x[n+1] = A(theta[n]) x[n] + B(theta[n]) u[n]`, `y = C x + D u`, and the trajectory with its law (section 4.5a) | the class-1 queries at each frozen `theta` (`Controlled::at`); at order 0 every norm over the trajectory's range, exactly; for a stateful system the frozen queries hold only under the slowness the trajectory declares (section 4.2) | every filter under modulation; chorus, flanger, phaser; the FDN with modulated lines; at order 0 a pan, a rotation or a demix under a moving control |
| **3. Memoryless nonlinear** | nonlinear, memoryless, time-invariant, driven | a static map `y = f(x)` | the curve, DC `f(0)`, odd symmetry, the harmonics of a sine through it, `lipschitz` and `max_gain`, the aliasing bound | the waveshaper, the brickwall clip, a dynamics gain curve on its own |
| **4. Stateful nonlinear** | nonlinear, stateful, time-invariant, driven | a **composition of 3 with 1 or 2**: Wiener (LTI then map), Hammerstein (map then LTI), or a detector path controlling a gain | the steady-state characteristic (the map at the LTI part's DC gain), the time constants and latency of the LTI part, a peak gain; **no frequency response** | compressor, gate, limiter, followers; the ladder (class 1 only once linearised for small signals) |
| **5. Autonomous generators** | any, driven by a phase or an event, then autonomous | `f(phi(t))`, a segment list, or a stochastic process | the period or frequency, the harmonic spectrum, the segment durations, the range, a noise variance or spectrum | oscillators, LFOs, envelopes, noise, dither's noise, the click's waveform |
| **6. Multirate or time-scale** | linear (resampler) or not (stretch), stateful, **periodically time-varying** with an LTI prototype (LTI under blocking by the period, section 4.2) | a ratio, an LTI prototype (the resampler's windowed-sinc low-pass), a time-varying read | the ratio, the prototype's response, the latency, the duration ratio | the FFT resampler, the stretch unit, the sampler's playback read |
| **M. Measurements** | a functional of a signal, not a processor | a config and an analyser | a resolution (`fs/N`), a latency | `analysis/` |
| **E. Event or discrete logic** | not a signal map | a state machine over events | transitions | voice allocation, the gate's hold timer, the click's trigger, lanes as data |

Two consequences used throughout.

1. **Modulation is not a class; it is a trajectory `theta(t)`.** Every class can be
   driven by a control that moves. Class 1 at a frozen setting is class 2 as soon
   as a lane moves a cutoff; the table says "1 (2 under modulation)" for the
   filters. The class-2 queries are the class-1 queries at `theta(t)`, `Cadence`
   (section 4.5) is the grid on which it is sampled, and the law between samples is part of the
   model (section 4.5a).
2. **A class is a trait, and a model may implement several.** A resampler is
   class 6 and its prototype is class 1. A compressor is class 4 and its detector
   is class 1 and its gain map class 3. The composition algebra (section 4.9) is
   how a model states that.

### 2.3 The engineering layer is not the math

`Bounds` (the sizes a realization allocates), `Ceiling` (the limits a node builds to:
rate, tail floor, a control's maximum), `Effort` (what the realization costs),
`Cadence` (the grid on which `theta` is sampled) and `Quality` (which realization tier) are
properties of a **realization and of the node that schedules it**. Two realizations of
one `H(z)` have different `Bounds` and `Effort` and the same model. They sit on
`Design` and `Realization` as associated items, never on a class trait, so a class
trait states only what the mathematical object is. `Cadence` is the one that
touches the math: its anchor decides whether a render depends on the block length
(section 4.5), and a contract case checks that a trajectory is slow against its
period. The *law* by which `theta` moves between grid points is not on this list:
it is part of the model (section 4.5a).

### 2.4 What must not be forced

The constraints, each of which is a test the design must pass in section 4.8:

1. **No universal model.** `Network`, a biquad, a compressor curve and a gain
   matrix have nothing in common but the class traits they implement. A
   shared `enum AnyModel` or a bag of optional fields is the settings bag doc 022
   section 5.1 refused, in a new place.
2. **No generic interpreter.** No realization runs "any model". Doc 022 section 4.2's
   opt-in `Generic` realization stays a reverb-only idea and is not generalised here.
3. **No per-sample virtual call.** Every trait is used statically. `Design` is not
   object-safe (`type Controls`, `type Model`, `type Realization`), and none of the
   class traits is called from the audio thread through `dyn`. The node
   calls `Realization::process` once per block (or per segment, section 4.5), the same
   dispatch frequency as `Node::process`.
4. **No fixed buffer shape.** `Realization<M>` has no `process`: the block shape is a
   second trait (`Effect<M>`: planar slices in, planar slices out, equal length).
   A generator, a resampler that consumes and produces different counts, or an
   instrument that takes events has its own shape trait and its own node, and
   still uses `Design`, a class trait, `Realization<M>` and the contract.
5. **The realization reads what it wants of the model.** A model may carry more than a
   realization uses (Freeverb ignores `width` and `wet`, doc 022 "Step 1b as built"),
   and two realizations may realize one model (a plain reference and a SIMD one).
6. **A hand-fused realization stays hand-fused.** Freeverb's exact float order, the
   convolver's partition schedule, the polysynth's eight-lane SIMD bank
   (`tutti-polysynth/src/kernel.rs`) are the realizations. Each step's bit-identity
   test is the proof that moving to the shape did not touch them (section 10).
7. **A class is a trait over structure.** An FDN answers `Lti` through its lines and its matrix, never by expanding to poles and zeros; a biquad through its five coefficients. No class trait names a data layout.
8. **A family's own cases are its own.** `rt60` and `decorrelated` are reverb
   cases and stay in the reverb's module; the generic contract does not grow a
   union of every family's checks.


### 2.5 Glossary

One row per term, with its mathematical definition and the name it replaces. The
renames follow the conventions of the field: a *realization* is one way to compute
a given `H(z)`; a *kernel* is an impulse response or a convolution kernel.

| Term | Definition | Was |
|---|---|---|
| **`Design`** | a parameter map `theta = D(c, fs)`: a pure, total function | `Design` |
| **model** | a system `S_theta` up to equivalence: the class of realizations with one input-output map | `Network` and its kin |
| **`Realization<M>`** | one realization of `M`: a state-space structure computing its input-output map | `Kernel` |
| **`Effect<M>`** | the planar block shape a node drives (engineering; not a math term) | `Effect` (kept) |
| **`System`** | a causal system; answers `latency` and `tail` | `Causal` |
| **`latency`** | the processing part of the onset of `h`; `onset(h) = latency + musical delay` | `Latent` |
| **`tail(floor)`** | the effective support of `h` past `latency`: least `T` with `abs(h[n]) <= eps * max abs(h)` for `n > latency + T`, `eps = 10^(floor/20)` | `Ringing` |
| **`Lti`** | linear time-invariant: `H(z)` rational or `h[n]` | `Lti` |
| **`frequency_response`** | `H(e^jw)` from each input to an output | `response` |
| **`dc_gain`** | `abs(H(1))` | `dc_gain` |
| **`spectral_radius`** | an upper bound on the spectral radius of the state matrix `A`; stable iff below 1 | `spectral_radius` |
| **`l1_norm`** | an upper bound on `norm(h, 1)`: the BIBO gain, the guarantee against overs | `gain_bound` |
| **`hinf_norm`** | an upper bound on `norm(H, inf)`: the peak of the magnitude response, what an EQ display scales to | `gain_bound` |
| **`Trajectory`, `Controlled`** | `theta(t)` as a signal with a law (pieces of a start, a length, endpoints and a shape), and a model driven by such a source: a linear parameter-varying system; `at(t)` is the frozen-time model | `Ltv`, `Varying` (not built) |
| **`Cadence`** | the grid on which `theta(t)` is sampled: a period and an anchor (engineering) | `Cadence` |
| **basis, metric** | the coordinates a channel bundle is in, and the Gram matrix `G` that makes energy `c^T G c`; isometry is `M^T G_dst M = G_src` | new |
| **`Memoryless`** | a static map `y = f(x)`, no state | `StaticMap` |
| **`lipschitz`** | the constant `L` with `abs(f(a) - f(b)) <= L abs(a - b)` | new |
| **`max_gain`** | `sup abs(f(x)) / abs(x)`: the largest secant gain from the origin | `max_gain` |
| **`SteadyState`** | the steady-state characteristic of a stateful nonlinear composite (Wiener and Hammerstein literature): `steady_state(x)`, `time_constants`, `peak_gain` | `SettledCurve` |
| **`Wiener`, `Hammerstein`** | LTI then static map; static map then LTI | kept |
| **`GainControl`** | `y = x g(W x)`: a gain computed from a detector `W` multiplies the input | `Gained` |
| **`Series`, `Parallel`, `SeriesN`** | the composition of `Lti` systems: product, aligned sum, a runtime list | kept |
| **`Autonomous`** | a system with no input: `f(phi(t))`, a segment list or a stochastic process | `Generator` |
| **`Periodic`, `Piecewise`, `Noise`** | an autonomous system that is periodic, a list of segments, or stochastic | `Periodic`, `Segmented`, `Noise` |
| **`Readout`** | an observation of the state, `y = C s` | `Readout` |
| **`Fidelity`** | the distance of a model from the ideal it approximates | `Fidelity` |
| **`Bounds`, `Ceiling`, `Effort`, `Quality`** | engineering: size, limits, cost and tier of a realization | kept |

## 3. The inventory

One row per algorithm, sorted by **class** (section 2.2); the **Class** column is
the primary class of the algorithm's own behaviour at a frozen setting, with "(2)"
where a moving control makes it class 2, "(1 lin.)" for class 1 only once
linearised, "(comp.)" for a composite, "P" for a primitive or infrastructure, "M"
for a measurement and "E" for event or discrete logic. **Realization today** says what
runs now and whether its controls are already typed units (`Param<Hz>` and the
like, per CLAUDE.md "Unit types").

**The verdict is derived from the class.**

- **Fits** when the algorithm's whole behaviour is one class (or a composition of
  classes, section 4.9) and the class's queries are exact for it. The row names the
  design, the model and those queries.
- **Partly** when the class fits but something in the algorithm is outside it: an
  operator that is not in the class (a hold timer, a windowed minimum, an
  attack/release switch, a `tanh` in a feedback loop), a realization shape that is not
  `Effect<M>`, a model that is data, or a layering that blocks the trait. The row
  says which piece.
- **No** when the algorithm is not a processor of a signal by a class: a
  measurement (M), event or discrete logic (E), or a primitive below the shape
  (P).

### 3.1 Tutti-dsp: filters, dynamics, time-based

| # | Class | Algorithm (where) | Realization today and controls | Verdict | Design, model, queries, or why not |
|---|---|---|---|---|---|
| 1 | 1 (2) | **State-variable filter**, eight responses (`tutti-dsp/src/filter/svf.rs`; nodes `SvfFilterNode`, `EqBandNode` in `tutti-nodes/src/filter/`) | `svf_step` over `SvfCoefficients<F>`; coefficients solved every 16 frames and `lerp`ed (`control.rs`, `svf.rs` `segments`). Typed: `Param<Hz>`, `Param<Q>`, `Param<Db>` | **Fits** | Design: `(SvfType, Hz, Q, Db, rate)` to the model, which is `SvfCoeffs` (`a1..a3`, `m0..m2`, already pure) plus the rate. Queries: `frequency_response` exactly (the trapezoidal SVF is the bilinear map of the analogue prototype with `g = tan(pi fc / fs)`, `k = 1/Q`; the contract proves the claim against the realization's impulse response), poles and stability (`k > 0` gives radius below 1), `tail` from the pole radius (the slowest pole of `s^2 + k s + 1`), `latency` zero. Bell and shelf gains are in the response. A series stack of bands is `SeriesN` of this model (section 4.9) |
| 2 | 4 (1 lin.) | **Moog ladder**, LP12/LP24/HP12/HP24 (`filter/ladder.rs`, `LadderFilterNode`) | `ladder_step`, four stages with a `tanh` at the input (`ladder_step`), `LadderCoefficients`. Typed: `Hz`, `Resonance`, `Drive` | **Partly** | The small-signal model fits: design `(Hz, Resonance, rate)` to `(g1, k)`; `frequency_response` of the linearised ladder (`(s/(1+s))^n` forms are in the type docs), stability (resonance below the self-oscillation gain), `tail` (`Unbounded` at maximum resonance: it self-oscillates). The `tanh` makes the large-signal response drive-dependent, which `frequency_response` does not claim: it is stated "small signal" and the contract measures it at an input level far below the `tanh` knee |
| 3 | 1 (2) | **Linkwitz-Riley crossover**, 2 to 6 bands (`filter/crossover.rs`, `CrossoverNode`) | `Crossover::split_block`; design and state fused in one struct (`set_frequency` re-solves). Typed: `Hz` | **Fits** | Design: `(frequencies, rate)` to per-split `Split` coefficients (already a private type). Multi-output `frequency_response` (`outputs() = bands`): LP/HP at `fc` are -6.02 dB and in phase, the bands sum to unity magnitude (the file's own header states it), and the sum is the Butterworth all-pass. Stability, `tail`, `latency` zero. Needs design and state separated, which is the one real refactor |
| 4 | P | `Real` float trait (`filter/real.rs`) | `f32`/`f64` | **No** | A numeric abstraction below the shape. A realization generic over `F: Real` is still one realization |
| 5 | 4 | **Compressor** (`dynamics/compressor.rs`, `curves.rs`; `CompressorNode`) | `CompressorKernel` (envelope + `EnvelopeFollower`), `CompressorBlock { threshold, knee, ratio, makeup_from, makeup_to }`. Typed: `Db`, `CompressionRatio`, `Seconds` | **Fits** | Design: `(threshold, knee, ratio, makeup, attack, release, rate)` to `GainControl<Wiener<Follower, GainMap>>` (section 4.9): a follower (`Lti`, one pole per direction) then the gain map (`Memoryless`, memoryless) multiplying the input. Queries: `SteadyState` (the gain map seen through the follower's DC gain; the map is `compute_compressor_gain_reduction`, `In = Db`, `Out = Db`: zero below `threshold - knee/2`, the quadratic over the knee, slope `1 - 1/ratio` above), time constants (step response reaches 63% in `attack`), `tail` zero (silent in, silent out; the node declares no tail, `compressor.rs` line 443), `latency` zero. `Readout`: `CompressorKernel::gain_reduction_db` exists |
| 6 | 4 + E | **Gate** (`dynamics/gate.rs`; `GateNode`) | `GateKernel`, `GateEnvelopeFollower`, `GateBlock`. Typed: `Db`, `Seconds` | **Partly** | Class 4 plus event logic. Fits: the settled curve is a step (`gate_gain_at`: range below the threshold, unity above), so `Memoryless` is degenerate; attack and release are the smoother's time constants; `Readout`: `is_open`/`gate_level` exist. Does not: the **hold** is a timer (`GateEnvelopeFollower::step(gate_open: bool)`), a discrete state machine (class E) that no `Lti` or `Memoryless` query describes; it is an inherent query |
| 7 | 4 | **Lookahead limiter** (`dynamics/limiter.rs`, `curves.rs`; `LimiterNode`) | `LookaheadRing`, `MonotonicMinDeque` for the window minimum, `LimiterCurve`. The per-frame recursion (linked peak, curve gain, instant attack, `smooth_envelope` release, `push_gain`) is in the node, `tutti-nodes/src/dynamics/limiter.rs` `process` (~362-440), where the compressor and gate already have dsp kernels (`CompressorKernel`, `GateKernel`). Typed: `Db`, `Seconds` | **Partly** | Class 4 with a delay and a windowed minimum. Fits: `Memoryless` (`LimiterCurve::gain`: gain `ceiling - threshold` up to the threshold, `ceiling/peak` past it, so output `<= ceiling`), `latency` = the lookahead (the node declares it, `limiter.rs` line 340), `tail` zero, `Readout` gain reduction. Does not: the detector is a sliding-window minimum (`MonotonicMinDeque`), a nonlinear operator that is not an LTI smoother, so the Wiener decomposition holds for steady input only and the attack is the lookahead by construction The recursion is the one part of a dynamics family not yet a pure algorithm over slices, which CLAUDE.md puts in `tutti-dsp`: a `LimiterKernel` takes it there (section 9, consolidation 9), and the node keeps the `Param` and lane reads, as `CompressorNode` does |
| 8 | 3 | **Brickwall clip** (`limiter.rs` `brickwall_clip`; `BrickwallLimiterNode`) | one `clamp` | **Fits** | A memoryless `Memoryless` over amplitude (`In = Out = f32`), `latency` and `tail` zero. Trivial, and the same trait as the waveshaper (row 17) |
| 9 | 4 (comp.) | **Multiband compressor** (`MultibandCompressorNode`) | crossover plus a `CompressorKernel` per band | **Partly** | A composite: `Series<Crossover, bands>` of the crossover model (row 3) and one compressor model per band (section 4.9). Per-band `Memoryless` and `Readout` (`gain_reduction_db(band)` exists). The summed `frequency_response` is not a single model query (it is signal dependent). Fits as a model that holds sub-models |
| 10 | 4 | **Envelope followers** (`dynamics/envelope.rs`; `EnvelopeFollowerNode` in `signal/follower.rs`) | `EnvelopeFollower`, `GateEnvelopeFollower`; the node outputs a control signal | **Partly** | A building block of rows 5 to 7, whose attack and release are their time-constant queries. The node is a measurement with a control output: its model is the two coefficients and a step response, with no response or curve to query |
| 11 | 1 (2) | **Delay line** (`delay.rs`, `buffer.rs` `CircularBuffer`; `DelayLineNode`) | `DelayLine::read_sample` with `InterpolationMode`, a per-channel `Ramp` of the time. Typed: `Seconds`, `Feedback`, `Mix` | **Fits** | Design: `(time, feedback, mix, interpolation, rate)` to a model of delay in fractional frames and loop gain. Queries: `Bounds` (the maximum delay, a build parameter, `delay.rs`), `tail` (`time x trips to the floor`, as the all-pass term of doc 022 section 3.1), stability (`|feedback| < 1`, `Feedback::MAX_STABLE`), `frequency_response` (the comb `1/(1 - g z^-D)` for the frozen setting), `latency` zero (musical delay is not latency). The interpolation's droop is a term of `Fidelity` |
| 12 | 2 | **Chorus and flanger** (`swept_delay.rs` `SweptLines`; `ModDelayNode` in `modulation/mod_delay.rs`) | `SweptLines::run`, LFO solved at control points and interpolated. Typed: `Seconds`, `Hz`, `Feedback`, `Mix`, `PhaseIncrement` | **Fits** | Class 2. Design: `(base delay, depth, rate, feedback, mix, rate)` to a delay-line `Lti` with an internal LFO (a `Controlled` model whose source of `theta` is the LFO). Queries: the frozen comb `frequency_response`; `tail` (`feedback_max` makes it exactly `max_delay` at none, per the type docs); `spectral_radius` = `|feedback|`; the source's `slew_bound` = the LFO's rate and excursion. Family-only: the sidebands of a sinusoidal delay modulation (Bessel `J_n(beta)`) |
| 13 | 2 | **Phaser** (`allpass.rs` `allpass_coeff`, `allpass_chain`; `PhaserNode`) | the swept first-order all-pass chain. Typed: `Hz` range, `Depth`, `Feedback`, `Mix` | **Fits** | Class 2. The all-pass chain at a sweep position is an exact `Lti` (first-order all-pass phase), so the notch frequencies and depths for a mix are its `frequency_response`; the LFO is the trajectory's source and declares the sweep; `spectral_radius` from the feedback; `tail` from it. The sweep mapping (`min + (max - min) * lfo * depth` into `allpass_coeff`) is in the node, `tutti-nodes/src/modulation/phaser.rs` `coeff_at` (~318-322), not in dsp (section 9, consolidation 10) |
| 14 | P | `interp.rs` `cubic_hermite`, `InterpolationMode` | four-point interpolation | **No** | A primitive below the shape. Its response is a term in rows 11 and 12 |
| 15 | 1 (2) | **Reverb**, Freeverb and the `Network` (`reverb.rs`, `reverb_design.rs`, `network/`) | on `main`: `Freeverb` is the realization; `FreeverbControls { room, damp }` | **Fits** | The first family: this is doc 022 |
| 16 | 1 | **Partitioned convolver** (`convolver.rs`; `ConvolverNode`) | `Convolver`, `IrSpectra` behind an `Arc`, non-uniform levels from a cost model. Typed: `Mix`, `Amplitude` (`convolution/params.rs`) | **Fits** | Design: `(IR, block size B, quality)` to a model that carries what analysis needs: `frequency_response` is the IR's FFT magnitude, `tail` the IR length, `latency` = `B` (the file header: output at frame `n` is the convolution at `n - B`), `Bounds` the IR length and level plan. The IR is data, so the control path is a **published** model, not param cells (`ArcLend`, section 5.3). Doc 022's `FirSummary` is this model's reverb face |

### 3.2 Tutti-dsp: nonlinear, rate and format, infrastructure, analysis

| # | Class | Algorithm (where) | Realization today and controls | Verdict | Design, model, queries, or why not |
|---|---|---|---|---|---|
| 17 | 3 | **Waveshaper** (`waveshape.rs` `ShapeKind`, `Shaper`; `DistortionNode`) | `Shaper::shape`, `shape_block*`; memoryless. Typed: `Drive` | **Fits** | Design: `(ShapeKind, drive)` to `Shaper` (already a value). `Memoryless` with `In = Out = f32`. Exact queries from the curve: DC offset `eval(0)`, odd symmetry (`eval(-x) = -eval(x)`, hence no even harmonics for a sine), the harmonic amplitudes of a sine of amplitude `A` (the DFT of `eval(A sin t)` over one cycle, computed on the control side), and `Fidelity` as the aliasing bound (the highest harmonic above the floor against Nyquist; `Crush` is a quantiser and has none that decays). `latency` and `tail` zero |
| 18 | 6 | **FFT resampler** (`resample/fft.rs`, `streaming.rs`; `tutti-export` `process/resample.rs`) | `FftResampler::process_into_buffer`, `StreamingResampler`, `ChunkSize`. Plain `SampleRate` pair | **Partly** | Design, model and queries fit: `(source, target, chunk size)` to the ratio (exact, from `gcd`), `latency` (`output_delay()` frames, a figure the file header already computes), `frequency_response` of the windowed-sinc low-pass (passband ripple, stopband attenuation, cutoff), `Fidelity`. The realization shape does not: input and output counts differ (`input_frames_next`, `output_frames_max`), so it is not an `Effect<M>`; it gets its own shape trait and no `DesignNode` |
| 19 | 5 | **Dither** (`dither.rs`; `tutti-export` `process/dither.rs`) | `DitherNoise::apply` in place, xorshift32. Typed: `Amplitude` (the LSB) | **Partly** | The model is the noise's density: `Rectangular` has variance `lsb^2/12`, `Triangular` `lsb^2/6`, and the error power of a quantiser with it; these are the exact queries. It adds noise to silence, so `tail` is `Unbounded` and it never parks. In place, not planar in/out. One query (`noise power`) is not in the vocabulary; it is an inherent method, not a trait |
| 20 | P | `control.rs` (`Ramp`, `segments`, `COEFF_INTERVAL`, `lane_group`) | helpers | **No** | The cadence machinery the node uses (section 4.5), not an algorithm |
| 21 | P | `buffer.rs` (`CircularBuffer`, `MonotonicMinDeque`) | containers | **No** | Containers below the shape |
| 22 | P | `fft/` (`RealFft`, `StagedRealFft`, `FftScratch`) | transforms | **No** | A primitive below the shape; the contract uses `RealFft` to measure responses |
| 23 | M | `analysis/`, which, less its window and STFT geometry, moves to `tutti-measure` (section 9) (spectrum, loudness, pitch, tempo, onset, chroma, chord, key, notes, peaks, silence, stereo, vectorscope, align, beat, window, xcorr, yin, ...) | streaming config/state/step | **No** | Measurements, not processors. They have a design (a window length fixes the frequency resolution `fs/N` and the latency) but no sound to realize; the pattern that fits them is a config, an analyser, and a `Readout`-like result, which this design does not cover |
| 24 | P | `tutti-export` `pump_blocks` pipeline | driver | **No** | A pipeline that uses rows 18 and 19, not an algorithm |

### 3.3 Tutti-nodes: the rest

| # | Class | Node (where) | Realization today and controls | Verdict | Design, model, queries, or why not |
|---|---|---|---|---|---|
| 25 | 5 | **LFO and modulators** (`lfo.rs`, `modulator.rs`; `ModulatorNode<Wave>`) | `Wave` (in `tutti-modulation`) is already a pure `cycles -> value` function. Typed: `Hz`, `Depth`, `PhaseIncrement` | **Partly** | The model is the realization: there is no second thing to hold it to. The exact queries (period, range, Fourier series of a saw, triangle, square) are inherent methods on `Wave`. `Wave` is in `tutti-modulation`, which depends on `tutti-types` alone, so it cannot implement `tutti-dsp`'s traits (layering, section 9) |
| 26 | 5 (+E) | **Note envelope** (`signal/note_envelope.rs`; `NoteEnvelopeNode`), `Adsr` in `tutti-types/src/value/adsr.rs` | segments sent to control lanes | **Fits** | A generator. Design: `(Adsr, curve, rate)` to the segment list; queries: segment durations, total time to a level, `tail` = release time to the floor |
| 27 | 5 + E | **Click** (`click.rs`; `ClickNode`) | transport-driven source reading `Env` per frame | **Partly** | Class 5 triggered by class E. Fits: the click's waveform is a table, a generator with a known spectrum and duration. Does not: *when* it sounds is a function of the transport (`Env::for_each_beat`), event logic outside any signal class |
| 28 | P | `ramp.rs` (`LastGood`, `block_driven`, `clamp_finite`) | helpers | **No** | The control reads every node shares; the generic node uses them (section 5) |
| 29 | E | **Automation and control lanes** (`automation/lane.rs`, `control_lane.rs`, `recording.rs`) | a curve evaluated at the beat | **No** | The model is user data (a curve) and already a pure function of position |
| 30 | 1, order 0 | **Channel sum** (`mix_bus.rs`; `ChannelSumNode`) | summing | **Partly** | Class 1 at order 0: a gain matrix of ones, so `frequency_response` is flat and `spectral_radius` is 0. It fits trivially and has nothing to design: no controls, so no `Design` and nothing for `Describe` |
| 31 | 1, order 0 (2) | **Bus strip** (`strip.rs`; `BusStripNode`) | gain, balance, mute: `balance_gains` | **Fits** | Design: `(Amplitude, Pan, mute)` to a 2x2 gain matrix. Queries: the matrix, unity at centre, flat `frequency_response`. Small, and the same kind of model as `Fold` (row 32): a gain matrix |
| 32 | 1, order 0 | **Downmix and `Fold`** (`downmix_unit.rs`; `tutti-types/src/fold.rs`) | `Fold` is built on the control side and applied without allocating | **Fits** | Already the split: `Fold` is the model (a gain matrix), `Fold::frame` the realization. Queries: the matrix, power preservation, `FoldError`. `Fold` is in `tutti-types` and cannot implement `tutti-dsp` traits, so a one-field wrapper in `tutti-dsp` does (step 7a: `Fold` is the one order-0 map and carries a basis; there is no `GainMatrix` trait) |
| 33 | E | `reconcile.rs` | graph editing | **No** | Modulation reconciliation (doc 014), not an algorithm |

### 3.4 Instruments and the other crates

| # | Class | Algorithm (where) | Realization today and controls | Verdict | Design, model, queries, or why not |
|---|---|---|---|---|---|
| 34 | 5 | **Polysynth oscillators** (`tutti-polysynth/src/kernel.rs`: `sine`, `saw` and `pulse` with PolyBLEP/PolyBLAMP, `triangle`, `white`, `pink`) | `f32x8` lanes, no state, called from `bank.rs` | **Fits** | A generator. Design: `(OscillatorType, frequency, pulse width, rate)` to a model; queries: harmonic amplitudes (saw `1/n`, square odd `1/n`), the aliasing bound after PolyBLEP (`Fidelity`). The SIMD realization is exactly the thing that must not be reshaped (section 2, constraint 6) |
| 35 | 5 | **Polysynth envelope** (`bank.rs`, `Adsr`) | straight-line stages converted to per-sample slopes | **Fits** | The ADSR model of row 26, with `tail` = release |
| 36 | 1 / table | **Unison and portamento** (`unison.rs`, `portamento.rs`) | `UnisonEngine`, `Portamento` | **Fits** | Small pure designs: unison to a table of detune, pan and gain per voice (`UnisonVoiceParams`); portamento to a glide curve and time |
| 37 | E | **Voice allocation** (`voice.rs`) | stateful allocator | **No** | Already `tutti-voicing`'s `Voicing::step`, a pure step over state; a different pattern with its own contract |
| 38 | 5 | **SoundFont envelopes** (`tutti-soundfont/src/volume_envelope.rs`, `modulation_envelope.rs`) | dB-linear stages from SF2 timecents | **Fits** | Segment durations are exact (the file's tests assert a stage falls 100 dB in its time); `tail` = release to the floor |
| 39 | 5 | **SoundFont LFO** (`lfo.rs`) | delay then a triangle at a frequency | **Fits** | Class 5, `Periodic`: a delay, then a triangle at a frequency; period and delay are exact. (Row 25's layering problem does not apply: `tutti-soundfont` owns its LFO and may take a `tutti-dsp` edge, as row 40 notes) |
| 40 | 1 (2) | **SoundFont low-pass biquad** (`bi_quad_filter.rs`) | `set_low_pass_filter(cutoff, resonance)` then `process` | **Fits** | A biquad: the same `frequency_response`, stability and `tail` as row 1, with its model a coefficient set. `tutti-soundfont` depends on `tutti-dsp` (consolidation 4, landed) |
| 41 | 6 | **SoundFont voice render** (`render.rs`, `voice.rs`, `region_ex.rs`; the generator sum of SF2 9.4) | sample playback with interpolation, oscillator | **Partly** | The design half is pure and fits: SF2 generators (cents, timecents, centibels) to voice parameters through the 9.3 conversions. The sample-playback oscillator is a resampling read whose model is a pitch ratio and a loop; no response to speak of |
| 42 | 6 | **Stretch unit** (`tutti-sampler/src/stretch/unit.rs`; phase vocoder) | `Unit::tick`/`process`, source-pulled; latency depends on bypass | **Partly** | Fits: `(stretch factor, cents, FFT size, enabled)` to a model whose `latency` is `latency_samples()` (a function of the FFT size and whether it is processing, per the method's docs), the duration ratio, `Graded` (FFT size is the quality). Does not fit: nonlinear, so no `frequency_response`; the realization consumes at a variable rate (`input_rate`), so not an `Effect<M>`. Doc 026 gives it the shape: a `StretchDesign` with a `Retime<M>` realization (position-driven, `splice`, `reach`) |
| 43 | E | **Sampler butler and voices** (`tutti-sampler/src/butler/`, `voice/`) | streaming I/O | **No** | I/O and scheduling, not an algorithm |
| 44 | 1, order 0 | **VBAP** (the layout and solve in `tutti-dsp/src/spatial/`, `SpeakerLayout`; the node and the smoothing in `tutti-spatial/src/vbap/`) | `VbapPanner::solve_block` with angle smoothing over `SpeakerLayout::gains_into` | **Fits** | Design: `(azimuth, elevation, spread, width, speaker layout)` to the gain matrix. Queries: the matrix; power normalisation (`sum g^2 = 1`); at most a few non-zero speakers; continuity in azimuth. The smoothing is the realization's |
| 45 | 1 (2) | **HRTF binaural** (`tutti-spatial/src/hrtf/`) | `HrtfBinaural`, HRIR convolution with a de-zipper | **Fits** | Design: `(azimuth, elevation, rate)` to an interpolated HRIR pair (from the sphere: data). `frequency_response` is the HRIR's FFT, `tail` is `ir_len - 1` (`ring_out`), `latency` the delay compensation. Published like row 16 |

Whether the instruments' controls are typed units was not audited row by row.
What the code shows: `Adsr` is `Seconds` and `Amplitude`
(`tutti-types/src/value/adsr.rs`); the VBAP panner takes `Azimuth`, `Elevation`,
`Spread` and `StereoWidth`, and the solve it calls (`vbap` 0.1 until doc 028 section 4.3's PR 1
moved it into `tutti-dsp`) takes bare `f64` degrees
(the code's own comment: "The types stop at the trigonometry"); the polysynth's
`SynthConfig` is a plain config struct whose fields a step-6 PR would type or
justify (CLAUDE.md "Unit types").

### 3.5 Found by the survey of `main`

A survey of `main` (eeabe3edc) found DSP that sections 3.1 to 3.4 miss, duplicate
or leave in the wrong crate. The rows continue the numbering; paths and lines were
read against the code. Section 9 turns the duplicates and misplacements into the
consolidation list.

| # | Class | Algorithm (where) | Realization today and controls | Verdict | Design, model, queries, or why not |
|---|---|---|---|---|---|
| 46 | 1 | **One-pole smoothers**: `ExponentialSmoother` (`tutti-spatial/src/smoothing.rs`), `smooth_envelope` (`tutti-dsp/src/dynamics/curves.rs` ~110) | `y += c * (x - y)`, the same recurrence in both. `ExponentialSmoother` steps once per `stride` samples, has a closed form for `n` steps (`advance`, `gap_closed`) and an angle variant (`process_angle`, `advance_angle`: along the short arc). `smooth_envelope` takes `c` as an argument. The follower node (`tutti-nodes/src/signal/follower.rs`, f64 envelope, attack and release coefficients) and the polysynth's `gain_glide` (`bank.rs`) are the same recurrence with a second coefficient or a fixed time. Typed: `Seconds` | **Fits** | Class 1, order 1: a pole at `1 - c`. Design: `(time, rate, stride)` to `c`. The solve is `Seconds::one_pole_coeff` in `tutti-types` and stays there, because `Param`, dsp and the graph all need it. Queries: DC gain 1, `spectral_radius` `1 - c`, the step response reaches 63% in `time`, `tail` from the pole. The attack/release switch of a follower is row 10 (class 4); the angle variant is not LTI (the wrap is a nonlinearity on the difference) and stays in spatial as an inherent piece. The polysynth portamento (`portamento.rs`) is not a variant: it is a finite ramp shaped by `PortamentoCurve` (row 51) |
| 47 | 6 | **HRIR resampler** (`tutti-spatial/src/hrtf/sphere.rs` `resample_into`, `resample_batched`, ~203-262) | Blackman-windowed sinc, half-width 32 zero crossings, cutoff 0.95 of the lower Nyquist, weights evaluated once per output tap and applied to a batch of responses; runs when the sphere is built, never on the audio thread | **Partly** | Class 6 with an LTI prototype (the low-pass), the same shape as row 18. It duplicates the kernel of `tutti-dsp/src/resample/fft.rs` `sinc_filter`, and its window formula (`0.42 + 0.5 cos u + 0.08 cos 2u`) duplicates `CosineWindow::BLACKMAN` (`analysis/window.rs`). The two differ in what they compute: `sinc_filter` is a fixed FIR applied in the FFT domain at a ratio of integers, this one evaluates the kernel at a fractional offset per output tap. The shared piece is the windowed-sinc kernel, not the resampler. Partly for the reason of row 18: not an `Effect<M>` |
| 48 | 1, order 0 | **Fades and crossfades**: `CrossfadeCurve` (`tutti-graph/src/fade.rs` `gains`, ~84-96: `EqualPower`, `EqualAmplitude`), the sampler's loop-seam crossfade (`tutti-sampler/src/voice/loop_span.rs` `fade_at`, ~189-199), the wrap fade (`voice/memory_source.rs`, `clock.rs` `Wrap`, 5 ms) and a linear ramp in `voice/live_read.rs` | A gain pair per frame, `g_in(k) * a + g_out(k) * b`. `CrossfadeCurve::gains(k, len)` takes `x = (k + 1) / (len + 1)`. The wrap fade uses `EqualPower`; the loop seam and `live_read.rs` write `(k + 1) / (n + 1)` out by hand | **Fits** | Class 1, order 0, **time-varying by a prescribed schedule**: at each frame `k` a 1x2 gain row `[g_out g_in]`, so a gain matrix whose entries follow a curve. It is not class 2: `theta` is not slow, it sweeps its whole range inside the fade by design. The model is the curve pair, a pure function of `x`; the exact queries are the sum law (`g_in + g_out = 1` for `EqualAmplitude`, `g_in^2 + g_out^2 = 1` for `EqualPower`), monotonicity and the endpoints. The seam's linear weight is `CrossfadeCurve::gains` at `x` with `g_in = x`, a third variant, `Linear` (consolidation 5) |
| 49 | E (1, order 0, time-varying) | **The engine's declick `Fader`** (`tutti-engine/src/engine.rs` ~878-940) | A gain carried across blocks, shaped per frame by a `GainPlan` of declicked commands over transport frames: a linear fall to zero on the command's frame, then a rise of `1/fade` a frame; one gain for every channel | **No** | Order-0 and time-varying like row 48, but what defines it is the plan (when a command lands, `At::NextBlock`, a late command, a jump), which is event logic over transport frames, and the only invariant is the slope bound `1/fade`. It is not a graph node (it shapes the engine's output buffer after the plan) and holds no model to query. It stays in `tutti-engine`, which owns `GainPlan` and the transport clock; the law it runs (a fall that multiplies the gain by `left / (left + 1)`, a rise that adds `1/fade`) is not a `CrossfadeCurve` |
| 50 | M | **Level metering**: `block_stats` (`tutti-graph/src/meter.rs` ~104: peak and sum of squares, in lane order), `Ballistics`, `BallisticsState` and `step_ballistics` (`tutti-dsp/src/analysis/stereo.rs` ~159) | `block_stats` runs as the meter op in the plan (`Editor::open_meter`); `Ballistics` is the reader's | **No** | A measurement (row 23's kind): a functional of a signal, with no sound to realize. `block_stats` stays in `tutti-graph`: the meter is a plan op, and graph and dsp are both layer 2 with no edge between them, so graph cannot take it from dsp (nor from `tutti-measure`, layer 3). `Ballistics` moves with the analysis split to `tutti-measure` (section 9) |
| 51 | 5 | **Curve shapes**: `SegmentShape` (`tutti-graph/src/event.rs` ~189-288, `eval(t)` on a segment), `LfoShape::evaluate_periodic` (`tutti-modulation/src/shape.rs` ~89, on a phase), `PortamentoCurve` (`tutti-polysynth/src/portamento.rs` ~38: `Linear`, `Exponential`, `Logarithmic` of the progress) | Three enums, each a pure function of a position in `[0, 1]` or of a phase, each in the crate that needs it. `SegmentShape` and `PortamentoCurve` share the linear, square and square-root laws (`SegmentShape::PowerIn(2.0)` is `PortamentoCurve::Exponential`, `PowerIn(0.5)` is `Logarithmic`); `LfoShape` is periodic | **Partly** | Class 5: a curve is a `Periodic` (the LFO) or a `Piecewise` (a segment) generator's own shape, with exact queries (range, endpoints, monotonicity, the Fourier series of a periodic one). Partly because of the layer table: `tutti-graph`, `tutti-modulation` and `tutti-dsp` are all layer 2 and none may depend on another, so no one of them can be the home. Under A2 (section 4.5a) the shape is the *law* of a trajectory, so the model needs it and it cannot stay private to the graph; `tutti-modulation`'s `Bezier` (`shape.rs`) is a fourth copy of the family, and the home is Q25 (under study, 2026-11-11). Consolidation 6 |
| 52 | 1, order 0 | **Strip balance law** (`tutti-nodes/src/strip.rs` `balance_gains`, `balance_side`, ~249-262) | `left = min(1, 1 - p)`, `right = min(1, 1 + p)` for pan `p` in `[-1, 1]`: unity at the centre, the far side attenuated linearly, nothing boosted | **Fits** | Row 31's design, as a law: `Pan` to a 2x2 diagonal gain matrix. Queries: the matrix, unity at `p = 0`, monotone in `p`, flat `frequency_response`. It is a pure function in a node file, which CLAUDE.md puts in `tutti-dsp`. It is not VBAP's law (row 44, power-normalised) and is not a constant-power pan |
| 53 | 1, order 0 | **Width-only folds** (`tutti-types/src/downmix.rs`: `fold_frame_to_mono`, `fold_frame_to_stereo`; the buffer forms `fold_buffer_to_mono`, `fold_planar_to_mono`) | Fixed BS.775 matrices by source width (1, 2, 4, 6, 8, 12), applied to one frame. Callers: `tutti-nodes` `convolution/node.rs` and `downmix_unit.rs` (tests), `tutti-spatial` `hrtf/panner.rs` and `vbap/node.rs`, `tutti-types` `interleaved.rs` | **No** | The matrices `Fold` builds (row 32), for the widths that have an order, to the bit (`fold.rs` documents this for smpte and quad). A duplicate with no model of its own, scheduled for deletion; callers move to a `Fold` built on the control side. The buffer forms stay until their callers (`interleaved.rs`, `tutti-dsp`'s re-export) have moved |
| 54 | P (5, noise) | **Bit generators**: the polysynth unison's xorshift32 (`tutti-polysynth/src/unison.rs` ~211, `seed_rng`), `hash_bipolar` (`tutti-modulation/src/program.rs` ~805), the LCG of the room IR synthesiser (`tutti-nodes/src/convolution/ir.rs` ~80); and, already in dsp, `DitherNoise`'s xorshift32 (`dither.rs`) | Four generators. The polysynth noise oscillator runs eight xorshift32 lanes in `tutti-polysynth/src/kernel.rs` ~138 and is a SIMD realization (row 34), not a candidate | **No** | A primitive below the shape; its distribution (uniform, period `2^32 - 1` for xorshift32) is the model of the generator that uses it (rows 19, 25, 34). Targets: the unison's and the IR's generators go to dsp's xorshift (the LCG's sequence changes, so the generated IR does; nothing pins it against a reference). `hash_bipolar` is **not** a candidate for it: it is addressable, a pure function of the index, so a random wave replays after a seek and in a fork (its own docs), which a stream generator cannot do, and `tutti-modulation` may not depend on dsp |

### 3.6 Counts

| Verdict | Rows | Count |
|---|---|---|
| Fits | 1, 3, 5, 8, 11, 12, 13, 15, 16, 17, 26, 31, 32, 34, 35, 36, 38, 39, 40, 44, 45, 46, 48, 52 | **24** |
| Partly | 2, 6, 7, 9, 10, 18, 19, 25, 27, 30, 41, 42, 47, 51 | **14** |
| No | 4, 14, 20, 21, 22, 23, 24, 28, 29, 33, 37, 43, 49, 50, 53, 54 | **16** |

Changes against the algorithm-first reading, each forced by the class: the gate
(6) and limiter (7) move from Fits to Partly, because a hold timer and a
sliding-window minimum are not in class 4's LTI-plus-map; chorus and flanger (12)
and the phaser (13) move to Fits, because slowly varying LTI is a class with exact
frozen queries, not a leftover; the SoundFont LFO (39) moves to Fits as a
`Periodic` generator; the click (27) and the channel sum (30) move from No to
Partly, because the click's waveform is a generator and the sum is an order-0 LTI
with nothing to design. The survey rows (46 to 54, section 3.5) add three Fits (46, 48, 52), two Partly (47, 51) and four No (49, 50, 53, 54); row 7 changes no verdict, only where its recursion lives.

### 3.7 Counts by class, and what each needs

| Class | Rows | Count | Traits it needs | Notes |
|---|---|---|---|---|
| 1. LTI, order 1 or more | 1, 3, 11, 15, 16, 40, 45, 36 (portamento), 46 | 9 | `System`, `Lti` | the filters, delay, reverb, convolver, HRTF; all become class 2 when a lane moves a control |
| 1. LTI, order 0 (gain matrix) | 30, 31, 32, 44, 48, 52, 53 | 7 | `System`, `Lti` | `Fold`, VBAP, strip, sum; the crossfade is order 0 with a prescribed schedule (row 48) |
| 2. Time-varying (LPV) | 12, 13 | 2 | `Lti`, `Controlled` | chorus, flanger, phaser; the FDN's modulated lines are the reverb's (row 15) |
| 3. Memoryless nonlinear | 8, 17 | 2 | `System`, `Memoryless` | waveshaper, brickwall |
| 4. Stateful nonlinear | 2, 5, 6, 7, 9, 10 | 6 | `Wiener`, `Hammerstein`, `GainControl`, `SteadyState` (parts: `Lti`, `Memoryless`), `Readout` | the ladder only linearises to class 1 |
| 5. Autonomous generators | 19, 25, 26, 27, 34, 35, 38, 39, 51 | 9 | `Autonomous`, `Periodic`, `Piecewise`, `Noise` | oscillators, LFOs, envelopes, noise, click waveform |
| 6. Multirate, time-scale | 18, 41, 42, 47 | 4 | per-family; the prototype is an `Lti` | own realization shape |
| M. Measurements | 23, 50 | 2 | none | `Readout`-like results |
| E. Event or discrete logic | 29, 33, 37, 43, 49 | 5 | none | the gate's hold and the click's trigger are class E inside rows 6 and 27; row 49 is the engine's gain plan |
| P. Primitives and infrastructure | 4, 14, 20, 21, 22, 24, 28, 54 | 8 | none | below or beside the shape |

(Row 36 is counted in class 1 for its portamento, which the survey shows is a
finite curve-shaped ramp and not a first-order slew (row 51); the row stays where it
is until step 7 revisits it, and its unison half is a parameter table with no
signal. Rows total 54.)

The 16 "no" rows are of four kinds, and the kind says what to do with them. Eight
are **primitives or infrastructure** (4, 14, 20, 21, 22, 24, 28, 54): they sit below
or beside the shape and need nothing. Two are **measurements** (23, 50). Five are
**event or discrete logic** (29, 33, 37, 43, 49): a state machine, a plan or user
data, with a pure step of its own where one exists (`tutti-voicing`). One is a
**duplicate scheduled for deletion** (53). None is "no" because the shape bends to
it.


## 4. The traits

All in `tutti-dsp`, in a new neutral module (section 9). Sketches are Rust against
the real code (`Setup`, `Effort`, `Channels`, `Ring`, `Design`, `Realization`, `Ceiling`
and `Bounds` are on `main` since #367 and move as described). They were
type-checked in a scratch crate outside the repo.

### 4.1 `Design`: the parameter map

```rust
/// The pure function from a family's controls to the parameters of a system.
pub trait Design: Clone + Send {
    type Controls: Copy + PartialEq + Send;               // as today
    type Model: System;                                   // new: a class trait, section 4.2
    type Realization: Realization<Self::Model>;                     // generalised, section 4.3
    // engineering (section 4.4), not math:
    type Bounds: Bounds;  type Range: Copy;
    const CADENCE: Cadence = Cadence::BLOCK;

    fn channels(&self) -> Channels;
    fn blank(&self, rate: SampleRate) -> Self::Model;     // Network::new(rate)
    fn design_into(&self, c: &Self::Controls, rate: SampleRate, out: &mut Self::Model);
    fn design(&self, c: &Self::Controls, rate: SampleRate) -> Self::Model { /* blank + design_into */ }
    fn bounds(&self, ceiling: &Ceiling<Self::Range>) -> Self::Bounds;   // worst case over the controls
    fn measure(&self, model: &Self::Model, floor: Db) -> Self::Bounds;  // today's Bounds::of
    fn realization(&self) -> Self::Realization;
}
```

Differences from `main`: `type Model` is any class trait, not `Network`; `Bounds`
is the design's (engineering) and no longer part of the model, so the model trait
states only mathematics; `blank` and `measure` are the two things the old
`Network::new` and `Bounds::of` did, now on the design that owns the sizes.
`design_into` stays the required method: a `Network` is 56 KB and cannot be built
by value on an audio thread (doc 022 "Step 1a as built"); a biquad is a few dozen
bytes and in place is merely harmless.

### 4.2 The model: one trait per class

A class trait states the invariants of its mathematical kind. Each is a trait over
**structured representations**: it names the question and leaves the answer to the
model's own structure.

```rust
/// A causal system from input signals to output signals. The base of every processor class:
/// it answers where the response to a unit impulse starts and where it effectively ends.
pub trait System {
    /// The processing part of the onset of `h` (the first frame at which the response to a
    /// unit impulse is non-zero), in frames. `onset(h) = latency + musical delay`, where the
    /// musical delay (a pre-delay, a delay time, a tap spacing) is declared by the model and is
    /// never latency (CLAUDE.md). A lookahead and a convolver's partition are latency.
    fn latency(&self) -> Samples;
    /// The effective support of `h` past `latency`: the least `T` such that
    /// `|h[n]| <= eps * max|h|` for every `n > latency + T`, with `eps = 10^(floor/20)`.
    /// Musical delay counts toward it, as a reverb's pre-delay does. `Finite(0)` for a
    /// memoryless system, `Unbounded` if no such `T` exists. For a nonlinear system `h` is the
    /// zero-state response to a unit impulse at the declared operating level.
    fn tail(&self, floor: Db) -> Ring;
}

/// Class 1. A linear time-invariant system (order 0: a gain matrix).
pub trait Lti: System {
    fn inputs(&self) -> usize { 1 }
    fn outputs(&self) -> usize { 1 }
    /// H(e^jw) from every input to `output` at each frequency. Control side; may allocate.
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]);
    /// An upper bound on the spectral radius of the state map; stable iff below 1.
    fn spectral_radius(&self) -> f64;
    /// An upper bound on `||h||_1`, the l1 norm of the impulse response: the BIBO gain, the
    /// guarantee that `|x| <= 1` gives `|y| <= l1_norm` (the guarantee against overs).
    fn l1_norm(&self) -> f64;
    /// An upper bound on `||H||_inf`, the peak of `|H(e^jw)|`: what a response display scales
    /// to. Always at most `l1_norm`, so a model with nothing tighter returns that.
    fn hinf_norm(&self) -> f64 { self.l1_norm() }
    fn is_stable(&self) -> bool { self.spectral_radius() < 1.0 }
    fn dc_gain(&self) -> f64 { /* |response at 0 Hz| */ }
    // provided: magnitude, phase, group_delay, from `frequency_response`
}

/// Class 3. A memoryless nonlinearity.
pub trait Memoryless: System {
    type In: Copy;  type Out: Copy;
    fn eval(&self, x: Self::In) -> Self::Out;
    /// The Lipschitz constant: `|f(a) - f(b)| <= lipschitz |a - b|` (the guarantee a click-free
    /// retarget and a bounded slope need).
    fn lipschitz(&self) -> f64;
    /// `sup |f(x)| / |x|`, the largest secant gain from the origin (the guarantee against
    /// overs for a map with `f(0) = 0`). At most `lipschitz`.
    fn max_gain(&self) -> f64;
    // provided for f32 -> f32: dc(), is_odd(), harmonics(amplitude, &mut [Complex64])
}

/// Class 4. The steady-state characteristic of a stateful nonlinear composite (the term of
/// the Wiener and Hammerstein literature). NOT `Memoryless`: it has
/// memory, and the memoryless check of class 3 fails it by construction.
pub trait SteadyState: System {
    type In: Copy;  type Out: Copy;
    /// The steady-state output for a steady input level (the envelope settled).
    fn steady_state(&self, x: Self::In) -> Self::Out;
    /// The time constants of the stateful part (attack, release, a window), in seconds.
    fn time_constants(&self, out: &mut [Seconds]) -> usize;
    /// An upper bound on the peak output for `|x| <= 1` (for a Wiener system
    /// `l1_norm(L) * max_gain(S)`).
    fn peak_gain(&self) -> f64;
}

/// Class 5. An autonomous generator. Not `System`: it has no input to end.
pub trait Autonomous { fn range(&self) -> (f32, f32); }
pub trait Periodic:  Autonomous { fn frequency(&self) -> Hz; fn harmonic(&self, k: usize) -> Complex64; }
pub trait Piecewise: Autonomous { fn durations(&self, out: &mut [Seconds]) -> usize; }   // an envelope
pub trait Noise:     Autonomous { fn variance(&self) -> f64; fn psd(&self, f: Hz) -> f64; }

/// The worst-case departure of a model from the ideal it approximates (the aliasing floor of a
/// shaper, the stopband of a resampler, an interpolator's droop). Lower is better. A property of
/// the model relative to an ideal, so it hangs off the model, not the realization.
pub trait Fidelity { fn error_floor(&self) -> Db; }
```

How each class answers through its own structure, and what it is **not**:

- **`Lti` through structure, not poles.** A biquad answers `frequency_response` from its five
  coefficients and `spectral_radius` from its two poles. A reverb `Network` answers
  from its stages: a diagonal comb bank sums the comb responses; an FDN evaluates
  `c^T (I - A D(w) G)^-1 b` over its own lines and orthogonal matrix, never forming
  a pole-zero list of order `sum of lengths`; its `spectral_radius` is the bound it
  already has (`rt60_bound`'s slowest per-frame decay times `max(||A||, 1)`,
  doc 022 "Step 1a as built") and its `l1_norm` is the L1 bound its realization
  contract already computes (`RealizationCase::peak_bound`); its `hinf_norm` is the same
  figure unless the structure gives a tighter one (an orthogonal mixing matrix does not
  raise the peak). Both are *sufficient*
  bounds, stated as such, and the contract measures that the truth is under them.
  A convolver answers from its IR. `tail` defaults, for a model that gives only its
  spectral radius, to the transit plus `ln(10^(floor/20)) / ln r` frames; a model
  with a tighter structured answer overrides it (the reverb does).
- **Latency is declared, not derived.** `System::latency` is the processing part
  of the onset of `h` (a lookahead, a partition); `group_delay` is a
  response-derived query and a different thing (an IIR has group delay and no
  latency). The contract measures the first non-zero output against
  `latency + musical delay`.
- **`Memoryless` is memoryless only, and its answers are exact and cheap.** DC, odd
  symmetry and the harmonics of a sine of amplitude `A` come from `eval` alone (the
  DFT of `eval(A sin t)` over one cycle, on the control side).
- **`Autonomous` is not `System`.** An oscillator has no input whose end starts a
  tail. An envelope driven by a note has a release, which is a `Piecewise` duration
  (the release segment), and a tail is reported by the node only when it has an audio input (a
  `System` wrapper, not the generator).
- **A known gap between the model and the realization's interpolation (class 2).**
  Linear interpolation in a modulated delay cancels near Nyquist: the FDN's decay
  at Nyquist measured 0.46 s for 1.2 s requested at a modulation depth of 0.5, since
  a fractional delay of one half cancels a tone there and the modulation visits
  every fraction (doc 022 "Step 1c as built"). The frozen-time query
  `Controlled::at(t).frequency_response` is that of the **unmodulated** realization and
  is accurate only in the low and mid bands; at the top band the model overstates
  the decay time. The contract therefore holds the frozen queries on the static
  probes only (section 6.1) and states the band limit; a model that wants the top
  band under modulation needs an interpolation-aware `Fidelity` (the droop of the
  interpolator is already a term of `Fidelity` for the delay line, row 11).
- **Class 2 is a linear parameter-varying (LPV) system with a known trajectory**
  (amendment A2, 2026-11-11; it replaces the proposed `Ltv` trait and its `Variation`, which
  were never built). The object is `x[n+1] = A(theta[n]) x[n] + B(theta[n]) u[n]`,
  `y[n] = C(theta[n]) x[n] + D(theta[n]) u[n]`. With the trajectory `theta(t)` given it is a
  linear time-varying system; as a family over the admissible trajectories (a rate and a range)
  it is LPV. `theta` is exogenous: a detector-driven `theta(level)` is a different *source* of
  `theta` (a quasi-LPV system if the loop is closed, which composition does not offer), not a
  different class, and a settled detector is the constant trajectory (doc 028 §3.4).
  Three statements make it precise.
  1. **The interpolation law between samples of `theta` is part of the model, not of the
     realization.** A model that leaves the law to the realization cannot define the system it is
     run as, and the contract can then hold the frozen queries on static probes only. The evidence
     that this is semantic is Q21's measurement: the same filter under a block-end ramp of its
     *coefficients* is 15 to 17 times further from the per-sample solve than the node's
     *parameter*-space ramp solved every 16 frames. The reference that measurement uses is the
     model at `theta(t)`, which this class names.
  2. **The trajectory lives in parameter space, and `design_into` maps it.** Linear
     interpolation of two rotation matrices `R(0)` and `R(90 deg)` is `(I + R)/2` at the midpoint,
     whose columns have norm `sqrt(1/2)`: it dips by 3 dB and is not a rotation, whereas
     interpolating the angle and then designing keeps an isometry. Conversely a spec may
     interpolate in decibels or entrywise. So the domain of the law is a stated choice of the
     design (`Design::Controls` is the space), and the same piece type serves both. Row 46's angle
     smoother (`process_angle`, along the short arc) is already a per-case choice of domain.
  3. **Order 0 is exact; stateful is a regime.** With no state, `y[n] = M(theta[n]) x[n]` holds
     with equality, latency is 0, tail is `Finite(0)`, and the norms over a trajectory's range are
     suprema: `hinf_norm = sup sigma_max(M(theta))`, `l1_norm = sup (largest row sum)`. For a
     stateful `D` the frozen-time model `at(t)` is an approximation valid when `theta` is slow
     against the dynamics (Q23's band limit still applies), and a frozen radius below 1 does
     **not** imply stability of the time-varying system (section 4.9).

  The API is section 4.5a's. An `Lti` model under a moving *control* is this picture with
  `theta` written by a lane; an LFO inside a model (a chorus) is the same with the LFO as the
  source, its rate and excursion the source's `slew_bound`.
- **Order 0 has a basis and a metric** (amendment A3, 2026-11-11; it replaces the proposed
  `GainMatrix` trait and the Euclid-only `is_isometry` of doc 028 §3.5). A bundle of channels is a
  vector of *coordinates* in a basis, and a gain matrix is a linear map between coordinate
  spaces. Energy is `c^T G c` for the basis's Gram matrix `G`. Speaker feeds, ear signals and
  object streams have `G = I`. Spherical harmonics in ACN order are diagonal and depend on
  the normalisation: N3D coordinates have `G = I`, SN3D coordinates have `G = diag(2n + 1)` per
  degree `n` (the same field has N3D coordinates `sqrt(2n + 1)` times the SN3D ones, and the
  encode's column power is `order + 1` in SN3D against `(order + 1)^2` in N3D, doc 028 §3.5). **An
  isometry is therefore relative: `M` is an energy-preserving change of basis iff
  `M^T G_dst M = G_src`.** The N3D to SN3D conversion is the identity on the physical field and
  an isometry in this sense, and not in the Euclid one; a decode that is an isometry in N3D
  is not one in SN3D. The carriers are the ones that exist: `Speaker`, the per-channel meaning of
  `ChannelTopology`, gains the variants a bundle needs (a harmonic by ACN index, an ear, an
  object), `ChannelTopology` gains the metric (`gram`, diagonal, enough for every basis named
  here) and a constructor for an ambisonic bundle (doc 023 G11), and `Fold` becomes the one
  order-0 map: a validated constructor from a matrix between two topologies, `matrix`,
  `column_power`, and `is_isometry` measured in the topologies' metrics. `Fold::between`
  stays the default *policy* (the ITU coefficients); a decode, a pan law, VBAP, a rotation and a
  basis conversion are `Design`s whose model is a `Fold`, differing in how they build the
  matrix. `Series` over folds checks that one's destination topology is the next one's source,
  so an SH bus wired to a speaker bed is refused where a width-only check passes it. `Fold`
  moves to `f64` storage and narrows once at the apply (decided 2026-11-11, Q26). Where
  `Fold` stops: it is an order-0 sparse map. A binaural decode from SH to ears is a
  multi-input FIR bank (an `Lti` with 16 inputs and 2 outputs), `Series<Fold, FirBank>` or one
  model with typed ear outputs, and its topologies still check the wiring. A graph port stays
  per-channel and carries no meaning; the topology rides the declaration, as `MasterTopology`
  does, and is checked on the control side. Type-level `Fold<From, To>` is refused (CLAUDE.md:
  width is runtime).
- **Class 4 is `SteadyState` over a composition** (section 4.9). **Class 6's trait is
  `TimeScale`** (a duration ratio and a `reach` query), with the position-driven shape
  `Retime<M>`, written in [doc 026](026-stretch.md) §3 for the stretcher; the resampler
  adopts it in step 7 (its prototype is an `Lti`), because the realization shape is not
  `Effect<M>` and its queries are small. **Block-periodic systems are LTI under blocking, so
  class 6 adds no class** (amendment A4, optional, 2026-11-11): stacking `P` consecutive samples
  turns a period-`P` system into an LTI MIMO system with `P` inputs and `P` outputs at `1/P` of
  the rate (polyphase blocking; a rate changer is the same lift with unequal widths). Its
  `hinf_norm` is the induced gain of the periodic system; the phase-averaged part is the LTI
  prototype and the aliasing terms are the other components, whose norm is the stopband of a
  resampler and the wrap error that doc 028 §3.8 calls `Fidelity` for `StftEffect`. A
  critically sampled filter bank reconstructs perfectly iff its polyphase matrix is
  paraunitary, which is the isometry query again, on the lifted system. No query is added ahead of a
  consumer: the aliasing norm lands with its first consumer, the STFT effect's `Fidelity`
  (doc 028 §3.8), and a periodic model then states its period as `Cadence::stream(P)`. A block FIR whose block grid is a realization detail
  (the partitioned convolver) is plain LTI, not periodic. Realizations are untouched.

**Why a trait per class, with queries hanging from it.** A fat `Queries` trait
needs a default for every query a family lacks, so a contract cannot tell "no
answer" from "wrong answer", and a UI cannot tell a flat response from an absent
one. With a trait per class the question "does this model have a response" is a
bound, answered at compile time (the scratch crate refuses `lti_realizes` for a
model that is only `System`). The class also makes the contract **once per class**
(section 6): a case needs a model and a realization, and the class says what to
measure. The cost is one `impl` per class and no run-time discovery of "what can
this model do", which a UI that holds a `dyn` would want; the node builds its
`Describe` surface at construction, where the type is known (section 7).

A query that only one family answers stays an inherent method of its model
(`Network::rt60`, `echo_density`, `is_lossless`; a chorus's sidebands). It becomes
a trait when a second family answers it and one contract case covers both.

### 4.3 `Realization<M>`: a realization

```rust
/// One realization of a model: a state-space structure that computes its input-output map.
pub trait Realization<M>: Send + Sized {
    fn prepare(&mut self, setup: &Setup);                 // control side; allocates
    fn retarget(&mut self, model: &M);                    // audio thread; samples theta; no allocation
    fn reset(&mut self);
    fn latency(&self) -> Samples { Samples::ZERO }
    fn effort(&self) -> Effort { Effort::Heavy }          // engineering
    #[must_use] fn fresh(&self) -> Self;
}
/// The planar block shape a graph node drives.
pub trait Effect<M>: Realization<M> { fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]); }
/// What a running realization reports about itself, the deepest since last taken: y = C s.
pub trait Readout { type Value: Copy; fn take(&mut self) -> Self::Value; }
```

`Realization<M>` no longer takes `bounds` in `prepare`: the sizes are the design's
(`Design::bounds`), and a realization that needs them takes them in its constructor
(`Design::realization` has the design), so the realization trait names no engineering type.
(On `main` `prepare(&Bounds, &Setup)` does; moving it is a one-line change per
realization, and step 1 may instead keep an associated `type Bounds` on `Realization`.
Q16.)

**The model is the equivalence class and the realization is one realization.** The
trait says so by being generic over the model rather than an associated type of
the design: *one model, several realizations*. The SVF, a transposed-direct-form-II
biquad and a lattice all implement `Realization<Biquad>`; the scratch crate holds the
three with one `realize!` line each, and the class case runs over all of them. A
second reverb design with Freeverb's stages shares Freeverb's realization. A realization is
tied to the model it realizes, not to who produced it. Which realization is best
is the realization's concern and an engineering one: the SVF stays stable under fast
modulation, a lattice under coefficient quantisation, TDF-II is cheapest at fixed
coefficients, and none of that is visible in `H(z)`.

`process` is not in `Realization<M>`: constraint 4. A resampler, a stretcher and an
oscillator bank implement `Realization<M>` for the lifecycle and the contract and a
different shape trait for their block. The contract's measuring helpers render
through a small harness trait (`Render`, **(proposal)**) that each shape
implements, so the same measurement (an impulse, a tone burst, a noise run) works
for any of them.

### 4.4 The engineering layer

Everything in this subsection sizes or schedules a realization. None of it is a
property of the mathematical object, and none is on a class trait.

| Item | What it is | Where |
|---|---|---|
| `Bounds` | the sizes a realization allocates for the worst case of a design (lines, taps, IR length, max delay); `dominates` | `Design::Bounds`, `Design::bounds`, `Design::measure` |
| `Ceiling<R>` | the limits a node builds to: the rate, the level a tail is measured to, the family's range `R` of a control (`MaxDecay` for the reverb, `MaxDelay` for a delay, `()` for a filter) | argument of `Design::bounds` |
| `Effort` | `Light`, `Moderate` or `Heavy`: what scheduling class the realization merits | `Realization::effort` |
| `Cadence` | the grid on which `theta` is sampled: a period and an anchor (section 4.5) | `Design::CADENCE` |
| `Quality` | which realization tier is built (section 4.7) | `Graded`, a field of the design |

**Ramping is inside the realization.** Doc 022 step 1c (the FDN, PR #372) found
that the smoothing between two retargets belongs to the realization, not the node:
`retarget` only sets targets, and the realization moves to them (a slew of whole
chunks that lands exactly), so `Cadence::BLOCK` suffices for the design of the FDN and a
32-frame `Cadence::every(32)` would redesign and query the tail per chunk for
nothing. The chunk grid itself is a stream-anchored one (section 4.5). Two consequences are in the contract (section 6.1): **the ramp's clock is
the stream's, not the block's** (it runs from the last reset, so blocks of 1, 17,
64 and 512 render bit for bit alike; `block_independent` found this), and **the
first retarget after `prepare`, `reset` or `fresh` lands at once** while later ones
ramp. Whether a design needs `Every` at all, or an internal ramp, is revisited in
section 4.5.

`Ceiling<R>` keeps today's validation (a positive finite rate, a decay in
`(0, 60]` s, a floor below 0 dB); `R`'s own constructor validates `R`. A bound
that a design cannot make dominate its own designs fails `bounds_dominate` in the
contract, as it does for the reverb today. The reverb's `Bounds` struct is
unchanged and becomes `FreeverbDesign::Bounds`.

**Caps are sizes, not theory** (amendment A5, 2026-11-11). A cap that bounds a *design* is a
`Bounds` field sized at `prepare`, and a cap that is a host capability stays an executor
capability. `MAX_SPEAKERS` (12, `tutti-dsp/src/spatial/layout.rs`) and `Network`'s
`MAX_CHANNELS` (8, `tutti-dsp/src/network.rs`) exist for inline audio-thread storage and are
bounds to lift, not design: where the audio thread can use a buffer sized at `prepare`, the
constant is replaced by a `Bounds` value. `MAX_ROOT_CHANNELS` (8, `tutti-graph/src/editor.rs`,
what the host that runs the executor can take, enforced at commit) and `MAX_PORTS` (64 per node)
are the second kind, limits of the current executor and likewise to lift where a consumer needs
more than they allow; neither is a property of any model.


### 4.5 Cadence: the time grid of `theta(t)`

A model is a system `S_theta`; in operation `theta(t)` moves with the controls, so
the node runs a *family*. `Cadence` is the **grid on which `theta` is sampled**: a period
and an anchor. It is engineering in the sense of section 4.4 (it schedules a realization) and
mathematics in two respects: the family is a good model of the running system only if `theta`
is slow against the period (the source's `slew_bound`, section 4.5a), and the **anchor**
decides whether the render depends on the block length.

```rust
/// When theta is sampled. Replaces `enum Cadence { Block }` (amendment A1, 2026-11-11);
/// `Design::CADENCE` keeps its name and type.
pub struct Cadence { period: Period, anchor: Anchor }
pub enum Period { Block, Frames(NonZeroU32) }
pub enum Anchor { Block, Stream { offset: Samples } }   // grid at offset + k * period on the stream clock
impl Cadence {
    pub const BLOCK: Self;                              // Period::Block, Anchor::Block: one design per block
    pub const fn every(n: NonZeroU32) -> Self;          // Frames(n) from the block's start: the filters
    pub const fn stream(n: NonZeroU32) -> Self;         // Frames(n) on the stream clock: the FDN's chunks, an STFT hop
    pub fn block_independent(self) -> bool;             // Period::Frames with Anchor::Stream
}
```

`Cadence::Block` and the planned `Cadence::Every(n)` of this section's earlier draft are the
instances `BLOCK` and `every(n)`; the FDN's private chunk clock is the instance `stream(32)`;
and the "normative cadence" some formats have is not a cadence at all but a trajectory whose
anchor is data (section 4.5a). Only `Block` is in the code today (`model/cadence.rs`, a
`non_exhaustive` enum), and nothing reads `Design::CADENCE` yet.

**Finding (2026-11-11): two anchors already coexist, and neither is named.**
`tutti_dsp::segments(size, solve_first)` (`control.rs:93`) steps from the *block's start*, so a
filter's render depends on the block length (`every(16)`, `Anchor::Block`); the FDN's 32-frame
slew is counted from the last reset, on the *stream clock*, so blocks of 1, 17, 64 and 512 are
bit-identical (`stream(32)`; doc 022 "Step 1c as built"). Block independence is a property of the
anchor. The contract's block-independence case (section 6.1) is therefore required iff
`block_independent()`, and a block-anchored cadence is stated as not block independent; the filters'
digests (`tutti-nodes/tests/filter_digests.rs`) pin `every(16)` at the block anchor, unchanged.
Whether `every(16)` should move to a stream anchor, which would make the filters block
independent and change their render and digests, is Q27 and is not decided here. A frame-based
model (an STFT effect, a decimator) states its period as `Cadence::stream(hop)`; its
`latency = size - 1` (doc 028 §3.8) is then the wait for the stream-anchored frame, derived and
not declared.

**Every against an internal ramp (decided, Q21).** The FDN settled it one way: design once per
block and let the realization ramp. Filters solve every 16 frames *inside* a block because a
lane may move the cutoff within it (the node reads the port at the solve points), which an
internal ramp between block-end targets cannot see. Step 2a measured it: the filters keep
`every(16)` ("Step 2 as built", Q21) and the reverbs stay at `BLOCK`. Step 2b0 adds the struct
with the realization's first consumer.

`DesignNode` splits a block with `tutti_dsp::segments(size, unprimed)` (it exists
and is what `SvfFilterNode` calls) and, per segment, reads the controls at the
segment's last frame, designs, calls `retarget` and `process` on the segment's
slices. For `BLOCK` that is one segment and the reverb is unchanged. For `every(16)`
it reproduces what the SVF does: the coefficients are solved at the solve points
and the realization `lerp`s from the previous solve to the new one across the segment
(`SvfCoefficients::lerp`), arriving exactly on the segment's last frame, as `Ramp`
does (`control.rs`). The realization owns the interpolation; the node owns where the
segments are. The first block (`unprimed`) solves at its first sample, as the SVF
does.

**Where cadence stops.** The compressor, gate and limiter nodes take a threshold
(and the limiter a ceiling) as a **per-frame lane** (`io.param(0).frames()`,
`compressor.rs` line 469, `limiter.rs` line 365). `every(1)` per-frame would pay
the node's per-segment overhead on every frame, which is exactly the fixed shape
the owner warned against. So per-frame lanes are not a `Cadence` (the limit of `every`, one frame, is a lane); they are a
second, optional realization shape (**(proposal)** `Driven<M>`: `process_driven(input,
output, lane)`, chosen by the node on a block whose port is `Frames`), or the node
keeps its own `process` and adopts the model and the contract only. Section 11 Q3
recommends deciding that against the code in step 4, and states the fallback,
which loses nothing: the dynamics get their curves, readouts and contract either way.


### 4.5a Trajectory: `theta(t)` as a signal with a law

Amendment A2 (2026-11-11). Section 4.2 gives the theory (class 2: the law and its domain are part
of the model, order 0 is exact, stateful is a regime); this is what it is made of. Every
mechanism below exists in some form today: a trajectory piece is `tutti-graph`'s
`ParamSegment` (`event.rs`) and its `SegmentShape`; `Controlled` is doc 028 §3.4's combinator;
the filters' "applied controls" are fields of `ControlParams`. A2 gives them one set of
names and deletes the proposals that were beside them.

```rust
/// A source of controls on the model's clock. (proposal)
pub trait ControlSource {
    type Theta: Copy;
    /// `theta` at absolute time `t` (control side; may allocate).
    fn at(&self, t: Samples) -> Self::Theta;
    /// A bound on `|d theta / dt|` per frame, the declared slowness; `None` if it jumps.
    fn slew_bound(&self) -> Option<f64>;
}
/// A detector in steady state: `theta` constant in `t` (doc 028 §3.4's `Controlled::at(level)`).
pub struct Settled<D: SteadyState>(D);
/// Sorted pieces `(start, length, from, to, law)`, in `Design::Controls`' space.
pub struct Trajectory<T> { /* pieces */ }
/// A model of design `D` driven by `S`. `System` always; `Lti` iff `D::Model: Lti` and `S` is `Settled`.
pub struct Controlled<S: ControlSource, D: Design<Controls = S::Theta>> { /* source, design, rate */ }
impl<S, D> Controlled<S, D> {
    pub fn at(&self, t: Samples) -> D::Model;       // frozen at t; exact at order 0
    pub fn norm_over_theta(&self) -> Norms;         // sup of hinf, l1 over the source's range; exact at order 0, a bound otherwise
}
```

- **The law and its home.** A piece's law is a pure function of the progress in `[0, 1]`:
  step, linear, the power and exponential shapes, Bezier, slerp for a direction. These are the
  curve shapes of row 51 (`SegmentShape`, `LfoShape::evaluate_periodic`, `PortamentoCurve`) and
  `tutti-modulation`'s `Bezier` (`shape.rs`), to be one set of laws (consolidation 6). The model
  needs the law, so it cannot live in `tutti-graph`; **which crate holds it is Q25, under study
  (2026-11-11)**, with the layer question that graph, dsp and modulation are peers at layer 2.
  The split of `ParamSegment` is independent of the answer: a *piece* `{ from, to, length,
  shape }` is the pure part, and the event is a piece plus an address and a position, so events
  keep their 32 bytes and **no new `EventKind`** is needed. A vector `theta` is `N` pieces at one
  offset; that they apply in one frame with one law is a contract case. A large `theta` (a table,
  an IR) already has a route, the published model (section 5.3).
- **The anchor of a trajectory is data.** A piece starts where the data says, on the stream
  clock, so a grid derived from a parameter block's start is a trajectory whose pieces begin at
  that frame; it needs no `Cadence`. (`Anchor::Stream { offset }` is the same thing for a
  periodic grid.)
- **What it subsumes, and deletes from the plan.** `Ltv` and `Variation` (never built; section
  4.2's frozen query is `Controlled::at`, the variation the source's `slew_bound`); the proposed
  `Scheduled<M, S>` (a schedule of models is a trajectory of controls); doc 026's `Warp` as a
  separate trait (a monotone scalar trajectory, `Trajectory<f64>`; doc 026 and doc 028 note
  it); the idea of sending a matrix as an event (`theta` travels and `design_into` builds the
  matrix on the audio thread). **Not subsumed on purpose:** `Smoother` and `ExponentialSmoother`
  (row 46) are a causal *filter* on `theta`, a class-1 system on the control signal and not a
  law with fixed endpoints; it is an alternative realization of "applied follows target" whose
  step response is the exponential law, and it keeps its place.
- **The sampler.** The structure common to step 2b's blockers 1, 2, 3 and 6 is
  `theta_applied = sampler(theta_target; grid, law, hold-state)`, whose output feeds
  `design_into`. Today that sampler is an unnamed part of `DesignNode` and `ControlParams`;
  here the grid is `Cadence`, the law is the piece's, and the hold-state is the source's.
- **Queries and cases.** `at(t)` equals the realization's frozen response at grid points;
  `norm_over_theta` is the over guarantee for a limiter ahead of a time-varying mixer; if every
  `at(t)` is an isometry (section 4.2, order 0) the time-varying system preserves energy
  **iff the law is applied in `theta`-space** (entrywise interpolation of an isometry is not
  one). The contract group `Controlled` is in section 6.2.
- **Remaining engineering limits, stated.** `ParamSegment.position` and `length` are `f32`,
  exact to 2^24 frames (about 5.8 minutes at 48 kHz); a longer hold rounds to a few frames. An
  exact Bezier needs a second control point the 32-byte event has no room for. Both are Q28.

### 4.6 The control side of a node

`ReverbParams` (on `main`) is the `Param` cells of an algorithm and how a block
reads them. It is generic already and gets a neutral name (`ControlParams`) and two
changes: `read` takes the frame to read at (the segment's last frame, for
`every(n)`; the block's last frame, as today, for `BLOCK`), and the `longest` hook
becomes declarative where it can (section 8.2). `tutti-dsp` still holds no `Param`.


### 4.7 Quality (doc 023 G5) as a design input

AU's `RenderQuality` is graded (0 to 127); VST2's `noRealTime` and the others carry
`RenderMode` only (doc 023 rows A18, S20, C38). Native algorithms have real
quality choices: the convolver's partition (a latency/cost trade), the export
resampler's realization length and chunk size (`ChunkSize::PRESETS`), the stretcher's
FFT size, a waveshaper's oversampling, a reverb's line count.

```rust
// tutti-types, beside RenderMode (doc 023 step 10). (proposal)
#[non_exhaustive] #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Quality { Draft, Standard, High, Best }
impl Quality { pub fn from_au(render_quality: u32) -> Self { /* 4 bands of 0..=127 */ } }
```

```rust
/// A design with a quality tier (proposal). Engineering: it selects a realization tier.
pub trait Graded: Design { fn quality(&self) -> Quality; #[must_use] fn with_quality(self, q: Quality) -> Self; }
```

`Quality` is engineering (section 4.4): it chooses which realization is built. Its
*consequences* are the model's queries (`System::latency`, `tail`, `Fidelity`), so
the math object a tier describes is still honest about what the tier does.

It enters **as part of the design value (`&self`), not `Controls`**, because it
changes `Bounds` (what is allocated) and often `System::latency`. A control is retargeted
per block; a quality change is a rebuild through the graph's `crossfade`, as
doc 022 section 5.1 already says of any structural change. `Graded` is the
trait a design implements to say so. The model records the consequence in its
queries, so nothing about quality is invisible: latency changes, the tail changes,
`Fidelity::error_floor` improves.

The contract's `quality` case (section 6) runs over `tiers()` in order and asserts
the model's `Fidelity` is non-increasing in dB, `Realization::effort` is non-decreasing,
and `Bounds` of a lower tier is dominated by a higher one. Doc 023 says G5 is read
only from AU on the plugin side; a native node reads it as a field.

### 4.8 Families against the traits

The test the owner's caution sets: the sketch must not bend to one family. Four
families were written out and compiled in the scratch crate; none needed another's
field, and none carries a method it does not answer.

**Filter (SVF), class 1 (2 under modulation).** The model is a coefficient set;
the realization is today's `svf_step`.

```rust
#[derive(Clone, Copy)] pub struct SvfModel { /* SvfCoeffs + rate + type */ }
impl System for SvfModel { /* latency 0; tail from the pole radius */ }
impl Lti    for SvfModel { /* H(z) from g, k, m0..m2; spectral_radius from the poles; l1_norm, hinf_norm */ }
impl Design for SvfDesign {
    type Controls = SvfControls;          // { ty: SvfType, freq: Hz, q: Q, gain: Db }
    type Model = SvfModel; type Realization = SvfRealization; type Bounds = (); type Range = ();
    const CADENCE: Cadence = Cadence::every(COEFF_INTERVAL);       // 16
}
impl Realization<SvfModel> for SvfRealization { /* retarget stores `next`; process lerps across the segment */ }
impl Effect<SvfModel> for SvfRealization { /* svf_step over lane groups (control.rs LANES) */ }
// More realizations of the same model, none changing the model:
impl Realization<SvfModel> for Tdf2 { .. }   impl Realization<SvfModel> for Lattice { .. }
```

**Dynamics (compressor), class 4 as a composition.** No frequency response: it is not
linear. The steady-state characteristic is the gain map seen through the detector's DC gain.

```rust
type CompressorModel = GainControl<Wiener<Follower, GainMap>>;      // section 4.9
impl Memoryless    for GainMap { type In = Db; type Out = Db; fn eval(&self, x: Db) -> Db { /* compute_compressor_gain_reduction */ } }
impl Lti       for Follower { /* one pole per direction: attack, release; LTI only while the direction is fixed (the switch is the nonlinearity, row 10) */ }
impl SteadyState for CompressorModel { /* steady_state(level) = gain map at the follower's DC gain; time_constants = [attack, release] */ }
impl Readout   for CompressorKernel { type Value = Db; fn take(&mut self) -> Db { /* deepest since last take */ } }
```

**Reverb, class 1 (2 with modulated lines), structured.** No dense pole list.

```rust
impl System for Network {}                // latency, tail (rt60_bound), as today
impl Lti    for Network {}                // response: comb sums / FDN solve over its own structure
impl Design for FreeverbDesign { type Model = Network; type Bounds = reverb::Bounds; type Range = MaxDecay; type Realization = Freeverb; }
```

**A gain matrix, class 1 at order 0.** `Fold`, VBAP, the strip: `Lti` with
`spectral_radius() == 0.0`, `latency() == 0`, `tail() == Finite(0)`, `frequency_response`
the matrix at every frequency. The same trait answers the smallest family and the
largest, which is the check that the class is the right unit.

### 4.9 Composition

The classes compose, and the queries compose with them, so a model can say what it
is made of. Composition is a **type-level combinator over models**, each
answering its class's queries from its parts with a stated bound. It is not a
runtime graph: `Series<A, B>` is a type with two fields and no interpreter, and a
realization for it is either two realizations run in order or one hand-fused realization that
realizes the same model (the model does not care).

| Combinator | Class of the result | Response | Latency | Tail | Stability | `l1_norm`, `hinf_norm` |
|---|---|---|---|---|---|---|
| `Series<A, B>` (A then B) | `Lti` if both | `H_B H_A` (a matrix product when multi-channel; exact) | `l_A + l_B` (exact) | `tail_A(f - g_B) + tail_B(f - g_A)`, where `g_X = 20 log10 max(1, l1_norm_X)`: a bound, so that the residue of each part passed through the other stays under the floor (the proof is a step-3 obligation, with the contract's measured tail as the safety net) | preserved for LTI parts: radius `max(r_A, r_B)` | both multiply (the norms are sub-multiplicative) |
| `Parallel<A, B>` (A + B, aligned) | `Lti` if both | `H_A z^-(L - l_A) + H_B z^-(L - l_B)`, `L = max(l_A, l_B)` | `max(l_A, l_B)` (the shorter branch is delayed to align) | `max(t_A, t_B)` (tail excludes latency, so the alignment delays cancel) | preserved: `max(r_A, r_B)` | both add (triangle inequality) |
| `SeriesN<M, N>` (N of one model, a runtime count up to `N`) | `Lti` if `M` is | product over the bands | sum | as `Series` | preserved | both multiply |
| `Wiener<L, S>` (LTI then static map) | class 4: `SteadyState` (not `Memoryless`: it has memory) | no response (nonlinear); the steady-state characteristic is `S(L.dc_gain * x)` | `L`'s | `L`'s | bounded if `L` is stable and `S` has a finite `max_gain`: `peak_gain = l1_norm(L) * max_gain(S)` | |
| `Hammerstein<S, L>` (map then LTI) | class 4: `SteadyState` | no response; steady-state characteristic `L.dc_gain * S(x)` | `L`'s | `L`'s | as `Wiener` | |
| `GainControl<W>` (a gain computed from a detector path multiplies the input) | class 4: `SteadyState` | none; `y = x g(W x)`, the steady-state gain is `W`'s characteristic | `W`'s | zero (silence in, silence out) | bounded by the largest steady-state gain | |

```rust
pub struct Series<A, B>(pub A, pub B);       // sketched and compiled in the scratch crate
pub struct Parallel<A, B>(pub A, pub B);
impl<A: Lti, B: Lti> Lti for Series<A, B>   { /* response multiplies; radius max; `l1_norm` multiplies */ }
impl<A: Lti, B: Lti> Lti for Parallel<A, B> { /* response adds (with alignment); radius max; `l1_norm` adds */ }
impl<L: Lti, S: Memoryless<In = f32, Out = f32>> SteadyState for Wiener<L, S> { /* steady_state(x) = S(L.dc_gain * x) */ }
```

**Open obligation (step 3): the `Series` tail bound has no proof yet.** The
bound `tail_A(f - g_B) + tail_B(f - g_A)` is argued, not proved: the argument is
that the residue of each part after its own tail, passed through the other part,
stays under the floor, and the exact floor shift (the other part's l1 gain, plus a
margin for the two residues adding) is what step 3 must derive, as doc 022 derived
`rt60_bound` ("Step 1a as built"). Until it is written, the table's tail entry for
`Series` is a conjecture; `Parallel`'s (the maximum) and `Series`'s latency and
gain bound are exact or standard. The contract's measured-tail case on random
pairs is the safety net, not the proof.

What composition does **not** include, deliberately: **feedback.** Closing a loop
around a composite does not preserve stability (it needs a small-gain argument and
a bound that depends on the loop), so the FDN's recirculation stays inside its
model as structure, and a user-level feedback composition is not offered (Q17).
Series and parallel of LTI models preserve stability exactly; that is the property
the algebra is built on.

**Time variation does not carry over (2026-11-11).** The stability column above is a statement
about LTI parts. A `Series` or `Parallel` of stateful time-varying parts (class 2, a `Controlled`
over a stateful design) does not inherit it: each part's frozen radius below 1 does not imply
stability under time variation (a switched linear system needs a common Lyapunov function or a
dwell time), and a composite's frozen radius is not enough either. This is an obligation, not a
theorem: a time-varying composite claims stability only with its source's `slew_bound` and a
dwell-time or common-Lyapunov argument for the design, and until a design carries one the
contract's measured-stability case (a long run of noise along the extreme trajectories) is the
safety net, as the measured tail is for the `Series` bound. **Order 0 is exact.** With no state,
latency is 0, the tail is `Finite(0)` and the radius is 0, norms over a trajectory are suprema
and multiply along a `Series`, and the open tail obligation above is vacuous for a series of
order-0 parts. That is why a matrix pipeline (section 4.10) composes without a new argument.

**Realizations compose by the same rule, optionally.** A `Series` realization runs its parts'
realizations in order over a scratch plane and a `Parallel` realization runs both and sums,
each part keeping its own hand-optimised loop. Nothing forces it: a fused realization
may realize `Parallel<Taps, Network>` as one loop, and it is held to the same
`Lti` contract as the composition of the parts. A composite's contract is the
class's contract (section 6): the composite inherits the impulse-response,
ring-time and stability cases without a new line.

**The reverb `Network` relates to this as a structured model.** `Network` is
class 1 (class 2 when its lines are modulated) whose internal stage chain
(`Stage::{Taps, AllPass, Fdn, Fir}` in `network.rs`) is *already a series
composition*, held as a runtime list because a design chooses how many stages
there are. Its `frequency_response` is the product of its stages' responses, its `latency`
the sum, its `tail` the series bound, in the same algebra: the rules above are
written once as free functions over `Lti` parts and used both by the type-level
`Series` and by `Network`'s fold over its list. Two carriers, one algebra. The
runtime list stays for the stage chain and the type-level combinators are for
compositions a design fixes in its type.

**It resolves two open problems.**

1. **Reverb step 3b** (early reflections in parallel with the late tail), which
   doc 022 "Step 1a as built" marks a BLOCKER because the series chain "cannot
   express a parallel branch". The model for a reverb with an early stage is
   `Parallel<Taps, Network>`: the early reflections are a FIR `Lti` (latency 0),
   the late tail is the FDN `Network`; the sum aligns their latencies and takes
   the longer tail. `Network` is not changed (no `Stage::Parallel`, and its series
   queries `tail`, `min_delay` and the densities stay valid inside each branch);
   `min_delay` and `has_direct_path` of the sum are the minimum over the
   branches. The realization is `Parallel<TapsRealization, Freeverb or FdnRealization>` or a
   fused one.
2. **Doc 024's old Q4** (the curve of a series EQ chain). A parametric EQ is one
   design whose model is `SeriesN<SvfModel, N>`: its `frequency_response` is the product of
   the bands, exactly, its stability is the radius of the worst band, its ring time
   the slowest. A chain of *separate graph nodes* is the application's product of
   each node's `Describe::curve` (they are different nodes; no model spans them).
   There is no `Chain` model to invent.


### 4.10 A stress test: IAMF

IAMF (Immersive Audio Model and Formats, with its renderer specification OAR) was read as a test of
this design on 2026-11-11, **not as a build commitment**. The test found the three gaps that
amendments A1 to A3 close, and its result is that after them every linear stage of IAMF is a
composition of models of this document, with no concept added. The spec's own observation is
the same: every normative operation except the codecs is an order-0 matrix, a scalar gain
schedule or an FIR bank. Facts below are from the IAMF 2.0 text and OAR as read in
`iamf-spec-pipeline.md`; in version 2.0 the default renderer (OAR) is normative, and the
BS.2127 renderer's HOA decoder is AllRAD.

| Stage | Model | Why it fits |
|---|---|---|
| Codec packets (Opus, AAC, FLAC, PCM) | not a model | a codec is nonlinear, lossy and bitstream-driven; outside |
| Demix | `Controlled<Trajectory<Index>, DemixDesign>`, model a `Fold` | one value per frame, applied without interpolation: a piecewise-constant schedule with state (the weight index is a saturating counter, nonlinear in the *parameter* and linear in the signal). Order 0, so exact |
| Recon gain | `Controlled<Trajectory<Gains>, GainDesign>`, a diagonal `Fold` | one value per frame with a fixed crossfade at the frame head (a window length the spec recommends); the window is a piece anchored at the frame boundary, data and not a cadence |
| Mix gain | `Controlled` over a `Trajectory` of gains | step, linear, slerp and Bezier, interpolated per sample on `floor(t * fs / parameter_rate)` from the parameter block's start: an **anchored grid** (A1), with the law in decibels, i.e. **in parameter space** (A2, `design_into` converts to amplitude); the reference renderer interpolates in dB |
| Channel render, scene decode, object pan | `Fold`s typed by topologies; AllRAD as `Series<ShDecode, VbapColumns>` | the basis and metric of A3; the SH bus into a bed is refused by the topology check |
| Mix | `Parallel` of the above; the sum is the graph's fan-in `Op::Sum` | |
| Binaural | `Series<Controlled<Quat, ShRotation>, SH-to-ear FIR bank>` | the rotation is order 0 and exact; the bank is an `Lti` with typed ear outputs and needs a multi-input FIR realization |
| Limiter | class 4 `GainControl` | no frequency response by design |

**Placement, as planned and not built.** The bitstream is data at layer 2, as `tutti-sf2` is the
SoundFont file as data (a crate named `tutti-iamf` in the plan); `tutti-codec` grows a packet
API and a multi-track ISOBMFF layer; demix and recon gain are `Fold`s with a basis; at runtime
the parameter blocks ride the existing `ControlProgram` and `ControlLaneNode`; loudness stays in
`tutti-measure`. There is no player crate.

**Correctly outside the abstraction.** The codecs; the bitstream and the container; loudness, a
measurement (class M). **Remaining gaps, neither a design gap:** the packet-level decode API in
`tutti-codec` (decode there is file-shaped today), and the binaural data and conformance
question: conformance is PSNR-based against the spec's vectors (above 80 dB for lossless), not
bit-exact, and the spec permits a clean-room binaural renderer (OAR §9), checked perceptually.
AOM publishes a reference renderer in Rust (`AOMediaCodec/roar`); it exists as a reference to
read, and tests here come from the spec and its conformance vectors, never from matching it.
Whether a non-exact law (a Bezier as linear pieces, the `f32` position of section 4.5a) is within
that tolerance is Q28.


## 5. The generic node

`ReverbNode<D>` becomes **`DesignNode<D>`** in `tutti-nodes`. It is the node of
doc 022 section 5 and "Step 1b as built" with the reverb removed from its names.

```rust
pub trait NodeDesign: Design + 'static
where Self::Realization: Effect<Self::Model> {
    type Params: ControlParams<Controls = Self::Controls>;   // was ReverbDesign::Params
    type Blend: Blend;                                       // NoBlend | DryWet
    type Readouts: Readouts<Self::Realization>;                   // () | a realization's readout, published
}
pub struct DesignNode<D: NodeDesign> { /* design, realization, params, mode, blend, readouts,
    model: Box<D::Model>, longest: Box<D::Model>, applied, current, ceiling, rate,
    channels, ring, since, silent */ }
```

### 5.1 What it carries, and what is optional

| Part | Source today (on `main`) | Per family |
|---|---|---|
| The control path: cells, modulated ports read per block, audio-thread redesign in place when the controls changed | `ReverbNode::process`, `ReverbParams::read`, `apply` | **Always.** Doc 022's finding holds for every family: a lane writes the node's *port*, never its cell, so only the audio thread sees a modulated value |
| `Fixed` vs `Live` mode (a send with no cells declares the exact tail of its one model) | `Mode::{Fixed, Live}`, `fixed()` | Always; a family with no `Fixed` use never calls `fixed` |
| The tail report and the `ring` high-water rule | `io.report_tail(tail_of(self.ring))` | **Always**, from `Model: System::tail` |
| `longest()` for the tail over a block | `ReverbParams::longest` | Only for a family whose model has a controllable tail: `Params::longest` returns `None` by default; section 8.2 makes it a declared direction |
| Parking: scan for the last non-zero input frame, owe `ring + latency` frames, cut to the frame, `reset`, `Status::Idle` | the body of `process` | **Always**, and uniformly from `System::tail`: a memoryless model owes zero frames; dither's `Unbounded` never parks; a realization that lies about its tail is caught by `assert_honest_tail` (doc 013 step 0b) and the node contract |
| Dry/wet | `mix: Param<Mix>`, `DryAlign`, the blend loop | **Optional through `D::Blend`**: a reverb, delay or chorus has a wet; a filter, a compressor or a panner has none |
| A readout | not on `main` | **Optional through `D::Readouts`** |
| Fork | `fork_fresh`: design clone, `Realization::fresh`, cells detached, `ParamNode` | Always; export forks with preroll (doc 013) |
| Shape | `Shape::audio(..).with_cost(effort).with_latency(realization.latency()).with_tail(ceiling).with_param_addrs(..)` | Always; `Cost` is a `match` over `Effort` |

### 5.2 Optional parts are associated types, not flags

```rust
pub trait Blend: Send + 'static {
    /// The addresses it adds to the node's ports, ahead of the algorithm's.
    const ADDRS: &'static [UnitAddr];
    fn prepare(&mut self, latency: Samples, width: usize, max_block: MaxBlock);   // DryAlign rings
    fn finish(&mut self, io: &mut Io<'_>, cut: usize, size: usize);               // the blend loop
}
pub struct NoBlend;   // ADDRS = [], every method empty: the node's ports start at 0
pub struct DryWet { /* mix: Param<Mix>, good_mix: LastGood, dry: Vec<DryAlign>, dry_buf, last_mix */ }
```

`NoBlend::finish` is an empty function the optimiser removes; a filter pays no
branch and no `Param<Mix>`. `Readouts<K>` is the same with `fn publish(&mut self,
realization: &mut K)` called once at the end of `process`; `()` implements it for any
`K`, and a family with a readout names a type that implements it for its realization
(section 7). Nothing is a runtime flag in the audio path: the choice is a type
parameter fixed when the node is built.

One consequence for ports: `ReverbNode` puts `Wet` at port 0 and the algorithm's
addresses after it (`first = 1`). With `NoBlend` the algorithm's start at 0, so
`first` is `D::Blend::ADDRS.len()`. A test pins the port order of the reverb
unchanged (it is persisted in saved graphs by `UnitAddr`).

### 5.3 A published model

The IR families (rows 16 and 45) cannot design on the audio thread: the IR is data.
Doc 022 step 2 puts the convolver's IR behind `ArcLend`. `DesignNode` does this
with a **second control path**, not a flag: `ControlParams` has an associated
`Published` mode where `Controls` is a handle (`Arc<Ir>`) read through a `Reader`
once per block, and `design_into` is replaced by a control-side build that
`ArcLend::publish`es a model the realization `retarget`s from a `Loan` (lend once per
block, never park a `Loan`; CLAUDE.md "Publishing to the audio thread"). The cells
and the lane-driven ports of the other families do not exist for it. This is the
largest piece of the convolution step (section 10) and is not needed by steps 1 to 4.

### 5.4 Descriptions and the Bevy side

`param_graph_node!(DesignNode<D>)` is one line per `D`, as for the reverb today;
`bevy-tutti` composes nothing new (`scripts/check-adapter.sh` holds). The old node
names stay as type aliases where they are public API: `FreeverbNode =
DesignNode<FreeverbDesign>` already is one on `main`, and `SvfFilterNode<F>`,
`CompressorNode` and the others would become aliases of `DesignNode` over their
designs (Q9 covers the inherent methods those nodes expose today, such as
`SvfFilterNode::frequency()` returning an `Arc<AtomicF32>`).


## 6. The contract machinery

Written **once per class**, in `tutti-dsp::contract` (section 9). A realization of
a model inherits every case of the model's class; a composite inherits the cases of
its result class; a family adds only the cases that are its own. Doc 022's
`design_contract!` and `realization_contract!` become a **core** every family runs and
**class groups** a family opts into by naming its class. Every figure is analytic
or from the spec, never another implementation's render (CLAUDE.md, "Do not
compare a render against another reference render in a new test"). The cases are
the *realization check* of section 2.1: they measure the realization's input-output map
and compare it to the model's invariant.

### 6.1 Core cases (every class)

The generic cases already in the realization contract (on `main`), with the reverb's
figure replaced where it was one:

| Case | Figure | Mutant |
|---|---|---|
| Pure | same controls give the same model; non-finite controls leave the last good ones; `design_into` under `assert_no_alloc` | read a clock |
| Bounds dominate | `bounds()` holds `measure(design())` for every reachable control | undersize by one |
| Latency | `Realization::latency()` equals `System::latency()` equals the measured shift of the first output sample | return 0 |
| Reset, fresh | bit-identical to a freshly built realization at the same controls | `reset` does nothing; `fresh` shares state |
| Block independence | blocks of 1, 17, 64, 512 bit-identical (time-domain realizations; a convolver within a declared round-off); **required iff `Cadence::block_independent()`**, and a block-anchored cadence is stated as not block independent (section 4.5) | a block-dependent ramp |
| Click-free retarget | a step of the controls mid-signal: the largest jump within a small factor of the steady-state bound | apply a coefficient unslewed |
| No allocation | `retarget` and `process` under `assert_no_alloc` with changing models | n/a (it is the gate) |

Three further cases came out of the FDN (doc 022 "Step 1c as built") and are core:

| Case | Figure | Mutant |
|---|---|---|
| Landed | the first `retarget` after `prepare`, `reset` or `fresh` lands at once (a realization built at a model *is* at it); later retargets ramp; `reset` and `fresh` equal a settled realization | ramp the first retarget; `reset` that keeps a half-ramped state |
| Stream clock | a ramp's clock runs from the last reset, not from a block's edge: blocks of 1, 17, 64, 512 are bit-identical **with the controls changing** (found by `block_independent`) | restart the ramp at each block |
| Not vacuous | each hook a design gives the contract (anchors, slower pairs, extremes, steps) returns something, or the design says why it cannot (a hook that returns nothing makes its case pass for no reason); checked per design, as `no_hook_of_the_fdn_is_vacuous` does | return an empty list from a hook |

The realization contract's present workarounds (a fit depth, tone probes, static
probes for the unmodulated cases, a 15% RT60 tolerance) exist because the reverb
model has no `frequency_response`; with the `Lti` group's response check the tolerance
is the response's, and the workarounds are retired rather than generalised.

The no-allocation case needs a counting allocator, which needs `unsafe` that a
`forbid(unsafe_code)` library cannot hold (today `tests/network_alloc.rs`). The
machinery ships it as a macro (`no_alloc_contract!`) that a family expands in its
own `tests/` file.

### 6.2 Class groups

| Class | Design case (no realization) | Realization case (realization against model) | Mutant pattern |
|---|---|---|---|
| **`Lti`** (1; also 2 at frozen `theta`) | `spectral_radius` below 1 on the whole reachable grid and the radius is a true bound on the poles where they are cheap; `frequency_response` at the family's **anchors** equals the spec (LP at `fc` is -3.01 dB at Butterworth Q; a bell's centre is its gain; a shelf is half its gain at `fc`; LR4 is -6.02 dB at `fc` and the bands sum flat); `tail` non-decreasing along the family's slower pairs; order 0: `spectral_radius == 0`, `tail == 0`; `hinf_norm <= l1_norm` | **Membership:** the realization is linear (`f(a+b) = f(a)+f(b)`, `f(ka) = k f(a)`) and shift-invariant (a delayed input gives the delayed output) at the frozen setting, within rounding. **Response:** the `RealFft` of the impulse response against `frequency_response`, magnitude (and phase where declared) on a log grid. **Ring time:** the measured decay rate against `spectral_radius`, the measured time to -40, -60, -90 dB within `tail(floor)` (covers) and `tail` not above a declared slack over it (tight). **Stability and `l1_norm`:** 60 s of noise at the extreme controls finite and under `l1_norm` (the guarantee against overs). **`hinf_norm`:** the measured peak of the response grid is under `hinf_norm` and `hinf_norm` is not above a declared slack over it (what an EQ display scales to); a model that returns the default (`l1_norm`) fails the tightness case if it claims otherwise. **DC:** the measured DC gain against `dc_gain` | add a `tanh` (membership fails); detune the cutoff 2% (response fails); halve `tail` (covers); double it (tight); a pole outside the unit circle (stability) |
| **`Controlled`** (2) | the source's `slew_bound` is slow against the period of `CADENCE`; order 0: `norm_over_theta` is the supremum over the range | (i) at the grid points the realization's frozen response **is** `design(theta(t))`'s (a response check after the segment); (ii) a step trajectory lands exactly on its frame; (iii) a trajectory crossing a block edge equals the same trajectory inside one block; (iv) a law applied in `theta`-space keeps an isometry at every frame (an energy check along a rotation); (v) a vector `theta` of `N` pieces at one offset changes in one frame | a realization that never reaches the target; a step one frame early; a ramp that restarts per block; a ramp in matrix space for a rotation (an energy dip at the midpoint); one lane offset by a frame |
| **`Memoryless`** (3) | monotone where the family says; `max_gain` bounds `eval` (the over guarantee) and `lipschitz` bounds the slope between sweep points; the limiter's output never above the ceiling; a shaper's symmetry and bound | **Memoryless:** shuffling the input samples shuffles the output (no state). **Level sweep:** a steady tone or DC at each level against `eval(level)`, exact to the ulp for a memoryless realization. **Harmonics:** the DFT of a sine through the realization against `harmonics`; DC against `dc` | threshold +1 dB; ignore the knee; make it stateful |
| **`SteadyState`** (4: `Wiener`, `Hammerstein`, `GainControl`; **not** the memoryless check, which a stateful composite fails by construction) | the steady-state characteristic of the composite equals the map at the LTI part's DC gain; `peak_gain` bounds it (the over guarantee: the sweep's peak output at `|x| <= 1`); `time_constants` are non-negative | **Settled-level sweep:** a steady tone or DC at each level, run for at least five of the slowest time constant, against `steady_state(level)`; **step response** reaches 63% in the LTI part's time constant; `Readout::take` equals the curve at the level and equals the gain measured on the output (two realization facts that must agree); `take` clears what it took | threshold +1 dB; flip the readout's sign; report the gain before makeup |
| **`Periodic`** (5) | `harmonic(k)` decays as the family says (saw `1/k`) | the measured fundamental (autocorrelation) against `frequency`; the DFT lines against `harmonic(k)`; the aliasing floor against `Fidelity` | detune the phase increment; drop the band-limiting |
| **`Piecewise`** (5) | `durations` non-negative and sum to the total | the measured time to each threshold crossing against the segment durations (the SoundFont tests already assert that a stage falls 100 dB in its time) | a decay stage one constant short |
| **`Noise`** (5) | `variance` is the closed form of the shape | the measured variance and spectrum over a long run | triangular treated as rectangular |
| **Multirate** (6) | ratio exact; latency a whole number of frames | output frames equal `ratio x input`; a swept tone's stopband against the prototype's `Lti` response (the resampler). The stretchers' `TimeScale` and `PitchPreserving` groups are in doc 026 §7 | a realization one frame short |
| **`Graded`** (any) | `Fidelity::error_floor` non-increasing across tiers; `Bounds` of a lower tier dominated by a higher; `Effort` non-decreasing | the tier's `System::latency` equals its measured latency | a tier that changes nothing |

Because the cases are bounded on the class, a realization for a *different* realization
of the same model costs one line: the scratch crate's `Tdf2`, `SvfTpt` and
`Lattice` all pass the same `Lti` group, and a defective one (a `tanh` in the
loop, a detuned coefficient) fails it. That is the realization check doing its
job: it does not know or care which structure it was handed.

### 6.3 Opting in

```rust
// In the family's test module (or tutti-dsp tests for the families in tutti-dsp):
design_contract!(SvfDesign: Lti);
realization_contract!(SvfDesign: Lti, tolerance = ..);        // runs the Lti group over SvfDesign::Realization
realization_contract!(Tdf2Design: Lti, tolerance = ..);       // another realization of the same model
```

1. The family names its **class**. The class is a trait bound on `D::Model`, so
   naming a class the model is not is a compile error (the case is bounded on it).
2. The family implements `DesignCase` and the class's probe trait (`LtiCase`:
   the anchors, the slower pairs, the extreme controls; `MemorylessCase`: the
   sweep).
3. A class implemented but **not listed** is not a compile error (a macro cannot
   see every `impl`). `scripts/check-contract-coverage.sh` **(proposal)** greps
   every `impl Lti|Memoryless|SteadyState|Periodic|Piecewise|Noise for` in the workspace and
   fails unless the type appears in a `*_contract!` list, as
   `check-range-types` does for its rule. The plugin contract's `gaps = N`
   ratchet is the precedent for a count a reviewer must raise.
4. Each case has a `Defect`/`Flawed` mutant run by a self-test in `tutti-dsp` (the
   `a_defective_realization_fails_its_case` pattern of #367), so a case that cannot fail
   cannot ship.
5. A family's own cases (`rt60`, `band_order`, `line_lengths`, freeze for the
   reverb; `sum_flat` for the crossover; the knee's quadratic for the compressor)
   are listed beside the class, not inside it.

### 6.4 The node contract

The node's silence scan, cut to the frame, park and wake, and high-water tail logic
are generic over **any realization that rings out** (a finite or unbounded `tail`),
and the contract treats them so. The cases that need a node and a graph (not just a realization) are today
`tutti-nodes/tests/reverb_node.rs` and `reverb_node_generic.rs`. They generalise to
`node_contract!(D)` in `tutti-nodes`'s `testing` module (feature `testing`,
`Cargo.toml` line 29), over `tutti_graph::contract::Driven`: the declared tail
is honest after the cut (`assert_honest_tail`), park and wake equal a fresh node,
a port-driven block equals a cell-driven one, the dry path is aligned to the
realization's latency and not its delay (`Blend` families only), a fork renders from
silence, and a serial/pooled render is bit-identical (`tests/parallel.rs`). The
"lying design" fixture (an echo that rings past its claim, with a latency) becomes
a shared test double.

The measuring helpers (Schroeder integration, band energy, tone bursts, first
non-zero sample, level sweep, impulse-response magnitude by `RealFft`,
superposition and shift tests) go in `tutti_dsp::contract`, public under the
feature, so the plugin probe of doc 023 section 6 can use the same ones on a
hosted plugin (Q13).


## 7. Doc 023's gaps, and one read surface

### 7.1 G8 (curve response)

Doc 023 sketches `trait CurveResponse { fn magnitude(&self, freqs: &[Hz], out: &mut [Db]); }`
and says "a native node can compute the answer exactly". Here it is exact because
`Lti::frequency_response` is a query of the model (and `Memoryless::eval` of a static one):

```rust
/// What a curve display asks. (proposal; doc 023 section 5.8 has the magnitude half)
pub enum Curve<'a> {
    Magnitude(&'a dyn CurveResponse),     // dB over Hz: an EQ, a crossover, a reverb's none
    Transfer(&'a dyn TransferCurve),      // dB over dB: a compressor, a gate, a limiter
}
```

A `DesignNode<D>` where `D::Model: Lti` returns a `Curve::Magnitude` built
on the control side from the cells' base values (`ControlParams::snapshot`,
**(proposal)**, the cells without a port) by `design` then `Lti::magnitude`.
Where `D::Model: Memoryless` (or a class-4 composite's `SteadyState`, section 4.9) it returns `Curve::Transfer` the same way. It is
**available only where the class exists**, by an inherent `impl` with a
`where D::Model: Lti` bound (`DesignNode::curve()`); no run-time "does this
node have a curve" flag is stored. The design is cheap (0.2 us for the reverb; one
`tan` for an SVF), so a drag handle can call it per UI frame.

What a native curve can do that a plugin's cannot: ask a **hypothetical**. A
`CurveOf<D>::at(&controls)` designs at any controls, so a UI drags a handle and
draws the curve for the value under the cursor before it commits. It shows the
**base** value; a live curve at the modulated value needs the applied `Controls`
published back. They are `Copy` and `PartialEq`, so a sequence lock (`tutti-rt`'s
`seq_cell`) carries them, written once per changed block (Q10).

### 7.2 G9 (readout)

Doc 023: three formats declare a gain-reduction value (CLAP gain-adjustment, VST3
`Comp:GainReduction`, AU meter-read-only), and the dynamics nodes are the native
producer. The readout is a realization fact. The node's `Readouts` associated type
publishes it; the cell is the one meters already use: `tutti_rt::level_cell`
(`LevelWriter::fold(peak, sum_sq, frames)`, `LevelReader::take`), whose
"a block lands whole in one take, peak and energy together" is what a gain
reduction needs (the peak is the deepest reduction since the last take, the energy
its mean). It has a loom model already, so no new lock-free structure
(CLAUDE.md "New lock-free ... goes in `tutti-rt`"), and a quiet cell is swept by
the same `Reclaimer` rule. The reader is one consumer (`&mut self`, as meters).
`CompressorKernel::gain_reduction_db` and `MultibandControls::gain_reduction_db`
are the two producers that exist today.

### 7.3 One surface for native and hosted

Doc 023 section 5.1 puts `Display { pages, readouts: Vec<Readout>, curve: bool }`
in the hosted plugin's `Capabilities`, and Q1 recommends an optional `Describe`
trait implemented by a node and read by `tutti-session` beside
`SessionNode::params`. The same trait is the surface for both:

```rust
/// What a node tells an application about itself. Not on the audio path. (proposal; doc 023 Q1)
pub trait Describe {
    fn display(&self) -> Display;                          // doc 023 section 5.1
    fn curve(&self) -> Option<Curve<'_>>;                  // control side
    fn readout(&self, kind: ReadoutKind) -> Option<ReadoutReader>;
}
```

| Question | Native `DesignNode<D>` | Hosted plugin (doc 023) |
|---|---|---|
| Curve | `Lti::frequency_response` or the `Memoryless`/`SteadyState` of `design(controls)`: exact, at any controls | CLAP mini-curve-display, AU `FrequencyResponse`: a main-thread call through the `Controller` (016), only at the plugin's current parameters |
| Readout | the node's `level_cell` reader | the plugin's meter parameter or function name, polled and folded into the same cell shape |
| Latency and tail | `Is(model.latency())`, `Is(model.tail())`: analytic, and held by the contract | `Is(plugin claim)`, with `measured: Some(..)` from the probe (doc 023 section 6) |
| Quality | `Graded::quality()` | `Reported<QualityRange>` from AU, `Unasked` elsewhere |

An application reads `Describe` and never asks which it holds. The one asymmetry is
named in the type: `Processing::measured` is `None` for a native node because its
claims are contract-checked, not probed. The same measurement code runs in both
places (Q13): the probe measures a plugin's latency and tail as the realization contract
measures a realization's. Doc 023's section 5.8 sketch gains the `Curve` enum with the `Transfer` variant
(a one-line change, made in this document's PR). Doc 023 consumes the producers
here; this document owns the native producer and `Describe`.


## 8. Modulation: what the reverb found, applied generally

Doc 022 sections 10 and 11 and "what the reverb found" on `main` (#367). Each finding, for
every family.

### 8.1 Cost of a design per block, per family

Doc 022 measured the reverb in release: `design_into` 0.2 us, `Network::tail`
1.1 us (a libm `exp10` per band per line), `retarget` 5 ns; a lane that changes
every block costs about 1.3 us against 2.3 us to render 64 frames. **Nothing below
is measured except that row**; each step measures its own and records it here.

| Family | A design solves | Cadence | Smoothing policy | Tail query |
|---|---|---|---|---|
| Reverb | a few hundred float ops per line; `tail` dominates | `Block` | Freeverb steps; the FDN ramps lengths and `lerp`s absorption (022 sections 10.2, 10.3) | libm per band: skip in a block that cannot lengthen it |
| SVF, biquad, ladder | one `tan` (and a `powf` for gain) per solve (`svf.rs`) | `every(16)` | coefficient `lerp`, arriving exactly at the segment end | pole radius: one `sqrt`, one `ln` |
| Crossover | a `tan` per split | `Block` today (`block_driven`) | re-solve at the block's target | as the filters |
| Compressor, gate, limiter | trivial arithmetic | `Block`; a per-frame threshold lane is a second shape (4.5) | the follower smooths; makeup ramps across the block (`CompressorBlock::makeup_from/to`) | zero |
| Delay, chorus | trivial | `Block` | a per-channel `Ramp` of the time, fractional reads, so a moving length is a pitch bend (022 section 10.3) | `time x trips` |
| Waveshaper | trivial | `Block` | `Drive` stepped; memoryless | zero |
| Convolver, HRTF | not a design: a published model | on publish | crossfade or a de-zipper (`HrtfBinaural`) | IR length |
| Resampler, stretch | structural: rebuild | none | n/a | `output_delay` |

### 8.2 Extremum hooks: `longest()` generalised

On #367 each `ReverbParams` writes `longest`: the controls that ring longest among
the values a modulated port takes anywhere in the block, because the port gives
every frame but the **direction** in which a param lengthens a tail is the
algorithm's knowledge. That is correct, and it is repeated once per family.
Many families' directions are monotone per parameter (a filter's tail grows with Q
and falls with cutoff; a delay's with feedback and time), so the direction can be
a table the node reads:

```rust
// in ControlParams (proposal)
const LENGTHENS: &'static [(UnitAddr, Dir)];     // Dir::{Up, Down}: more of this, longer tail
fn longest(&self, io: &Io<'_>, first: usize) -> Option<Self::Controls> { /* default: pick the
    frame extreme of each LENGTHENS port, the base for the rest */ }
```

and the design contract's tail case verifies the table: for each entry a
`tail_pairs` check that the model's `tail` is non-decreasing in the declared
direction (a wrong direction fails, which is the mutant). A family whose
parameters interact (the tail is not separable) overrides `longest`, as the reverb
does today. The node's `since`/`ring` logic is unchanged.

### 8.3 A family whose redesign must ramp

The filters. Two choices exist: design per sample, or design at solve points and
interpolate the coefficients. The SVF already measured it (`control.rs`,
`COEFF_INTERVAL`): per-sample solving is the dominant cost of a modulated SVF (a
`tan` per sample); solving every 16 frames and interpolating costs a quarter of a
64-frame block's `tan`s, with 3e-5 worst error on a 200 Hz to 8 kHz sweep in 85 ms
(4.6e-4 at 64 frames, exactly 0 at 1). So **interpolate**, at `every(16)`. The
guarantee the contract holds is the one that matters for correctness: at a
segment's end the realization's frozen response **is** the model's (an `Lti::frequency_response` check
after the segment), so the interpolation can never leave the filter at a setting
the design did not ask for. Coefficient `lerp` does not by itself guarantee
stability between two stable sets; the existing SVF relies on the segment being
short and the error being the measured one, and the `Lti` stability case runs the filter
under a worst-case sweep so that this is a measured fact, not an assumption.

The other policies, for the record: a step (Freeverb: the output is read from the
past, so a step in the gain is a step in what the next trip carries, which the
click-free case holds to a bound), a slew at a bounded rate (the FDN's lengths),
a block ramp of a gain (the compressor's makeup, the strip's volume), and a
crossfade through the graph (any structural change).

### 8.4 The rest of 022's findings, carried over

- **The audio thread must be able to design.** Held: `design_into` is in place and
  total. A family whose model is data (rows 16, 45) is the published exception (5.3).
- **A `ModRange` above the ceiling is harmless**, because the design clamps and
  `Ceiling<R>` bounds the tail. Held unchanged.
- **`report_tail` alone does not park** a node: the cut, the reset and `Idle` are
  the node's, now once for every family.
- **New `UnitParam` kinds are append-only.** A family that needs one (the filters
  need none: `Cutoff`, `Q`, `Gain` exist; the compressor's exist) appends it with
  its pinned id (doc 022 section 10.4).


## 9. Where things live

Layering per CLAUDE.md and `scripts/check-layout.sh` (lines 381 to 383:
`tutti-dsp` and `tutti-modulation` may depend on `tutti-types` alone).

| Item | Crate | Layer | Check |
|---|---|---|---|
| `Design`, `Realization<M>`, `Effect<M>`, `Setup`, `Channels`, `Ring`; the class traits (`System`, `Lti`, `Memoryless`, `SteadyState`, `Autonomous`, `Periodic`, `Piecewise`, `Noise`), `Fidelity`, `Readout`, `Graded`; the combinators (`Series`, `Parallel`, `SeriesN`, `Wiener`, `Hammerstein`, `GainControl`); the engineering items (`Bounds`, `Ceiling<R>`, `Effort`, `Cadence` with its anchor); `ControlSource`, `Settled`, `Controlled`; `Complex64` use | `tutti-dsp`, a neutral module (`model`), exported at the crate root | 2 | no new tutti edge; `forbid(unsafe_code)`; no atomics or `Arc`-published state; every control arrives per call |
| The families' models and designs | `tutti-dsp` (`filter/`, `dynamics/`, `delay`, `waveshape`, `convolver`, `reverb`, `resample/`, `dither`, and by the consolidation below the smoother, the biquad, `LimiterKernel`, the vocoder parts) | 2 | pure algorithm over slices; a node file holds only the adapter (CLAUDE.md) |
| `contract` module: core cases, class groups, `design_contract!`, `realization_contract!`, `no_alloc_contract!`, the measuring helpers, `Defect` and `Flawed` | `tutti-dsp`, behind a `contract` feature (off by default; `rand` becomes an optional dependency of it) | 2 | the precedent is `tutti-graph`'s `contract` feature (`lib.rs` lines 201 to 222; `tutti-nodes` turns it on in its dev-dependencies) |
| `Quality` | `tutti-types`, beside `RenderMode` | 1 | vocabulary only |
| The trajectory and its laws (`Piece`, `Trajectory`, the curve shapes of row 51 and `tutti-modulation`'s `Bezier`) | **Q25, under study (2026-11-11)**: `tutti-modulation`, `tutti-types`, a crate made from `audio-automation`, or `audio-automation` kept external | 1 or 2 | the layer question of Q25; the model needs the law, so not `tutti-graph` |
| The basis and metric (`Speaker` variants, `ChannelTopology::gram`, the ambisonic constructor) and `Fold` as the one order-0 map, in `f64` | `tutti-types`, where `Fold` and `ChannelTopology` are | 1 | vocabulary plus `Fold`'s apply, which CLAUDE.md already names; the dsp-side `System`/`Lti` impls are on a one-field wrapper in `tutti-dsp` |
| `DesignNode<D>`, `NodeDesign`, `ControlParams`, `Blend`, `DryWet`, `NoBlend`, `Readouts`, `DryAlign`, `node_contract!` | `tutti-nodes` (`node_contract!` behind `testing`) | 4 | the node holds the adapter only |
| `Describe`, `Curve`, `ReadoutReader`, the session read | `tutti-session` (with doc 023's `Display`) | 7 | Bevy-free, re-exports no crate whole |
| The analysers (`analysis/` less `window`, `geometry`, `grid`, `error`) | `tutti-measure` (landed, #376), depending on `tutti-dsp` | 3 | the analysis split below; `ebur128` moves with it |
| Instruments' models | their crates' call to `tutti-dsp` (`tutti-polysynth` and `tutti-spatial` already depend on it; `tutti-soundfont` would add the edge) | 5 | layer 5 over layer 2 |
| `param_graph_node!(DesignNode<D>)` | `bevy-tutti` | 8 | a line per `D`; `check-adapter.sh` holds |
| `scripts/check-contract-coverage.sh` | scripts | | proposal (section 6.3) |

**Why the contract is a feature of `tutti-dsp` and not a `testing/` crate.** A
`testing/` crate that depended on `tutti-dsp` while `tutti-dsp`'s own unit tests
depended on it would give those tests two copies of `tutti-dsp` (the library and
the `cfg(test)` build), and their `Design` types would not be the same type. A
feature on the one crate avoids it, and is what `tutti-graph` does.

**Layering consequences found in the inventory.** `Wave` (`tutti-modulation`) and
`Fold` and `Adsr` (`tutti-types`) are below `tutti-dsp` and cannot implement its
traits; rows 25, 26 and 32 keep their queries as inherent methods or take a wrapper
in `tutti-nodes`. Moving the traits down to `tutti-types` would put an interface
into the vocabulary crate (CLAUDE.md: "vocabulary only"), and is not recommended.

### The analysis split (approved by the owner; landed in #376)

Built as described below: `tutti-measure` at layer 3 holds the analysers, and
`window`, `geometry`, `grid` and `error` stayed in `tutti-dsp`, which is Q24's
answer. The text is kept as the reasoning behind the cut.

`tutti-dsp/src/analysis/` is 15,976 lines and carries the `ebur128` dependency.
It is class M (row 23): functionals of a signal, with no sound to realize. It moves
to a **new crate, `tutti-measure`, at layer 3**, depending on `tutti-dsp` (layer 2),
which is what the doc's split between a processor and a measurement already says.
After it, **`tutti-dsp` holds realizations (processors) and `tutti-measure` holds
measurements**. The FFT stays in `tutti-dsp`: a processor (the resampler, the
convolver, the vocoder) needs it as much as an analyser does.

The split is not a directory move. Four files of `analysis/` are used by
processors and stay in `tutti-dsp`: `window.rs` (`CosineWindow`, `Window`: used by
`fft/scratch.rs`, and by the resampler after consolidation 1) and `geometry.rs`
(`StftGeometry`: used by the vocoder after consolidation 8), with the two they
name, `grid.rs` and `error.rs`. The analysers that build on them (`transform.rs`,
spectrum, loudness, pitch, tempo, onset, chroma, chord, key, notes, peaks, silence,
stereo with `Ballistics`, vectorscope, align, beat, xcorr, yin) move. Consumers
that name an analyser change one dependency edge: `tutti-export` (`Loudness`,
`measure_loudness`) at layer 5, and `tutti`, which re-exports `tutti_dsp` whole as
`dsp` and would add a `measure` module behind its own feature.
`tutti-sampler` names only `StftGeometry` and does not change. `tutti-measure` keeps every module
private, is `#![forbid(unsafe_code)]`, `publish = false` and has no `bevy`
feature, like its neighbours; `scripts/check-layout.sh` places it. CLAUDE.md and
`check-layout.sh` change in the split PR, not here. Open question Q24 is the
exact boundary.

### Consolidation

What the survey (section 3.5) found twice, or in the wrong crate, ranked by value
against risk. Each item has its target and what blocks it. None changes a
class trait; they are what step 0b and the later steps (section 10) move.

| # | Move | Target | Blocker |
|---|---|---|---|
| 1 | The HRIR resampler's windowed sinc (row 47) | `tutti-dsp` `resample`, with `CosineWindow::BLACKMAN` replacing its inline window formula; the batching and the fractional-offset evaluation stay in `tutti-spatial` | **Landed (#448).** None. The shared piece is the kernel, not `FftResampler` (the two compute different things) |
| 2 | `ExponentialSmoother` and `smooth_envelope` (row 46) | A `Smoother` in `tutti-dsp`: one recurrence, a coefficient in and the closed form for `n` steps (`gap_closed`); `smooth_envelope` becomes its step. The angle-wrapping variant (`process_angle`, `advance_angle`) stays in `tutti-spatial` over it. `Seconds::one_pole_coeff` stays in `tutti-types` | **Landed (#451).** None. A follower's attack/release switch is row 10 and stays a dynamics type |
| 3 | The width-only folds in `downmix.rs` (row 53) | Deleted; callers (`convolution/node.rs`, `hrtf/panner.rs`, `vbap/node.rs`, `interleaved.rs`, the `downmix_unit.rs` tests) build a `Fold` on the control side | **Landed (#446).** The buffer forms (`fold_buffer_to_mono`, `fold_planar_to_mono`) have the same callers to move; `Fold` reproduces the frame forms to the bit for smpte and quad (`fold.rs`) |
| 4 | **Landed.** The SoundFont RBJ low-pass biquad (row 40, `tutti-soundfont/src/bi_quad_filter.rs`) | A `tutti-dsp` biquad (a realization of row 1's model); `tutti-soundfont` gains the `tutti-dsp` edge row 40 already names | The output may change in the last bits (a different summation order or form), so the soundfont's digests move and are re-recorded for an argued reason, not pinned against the old render. The rustysynth MIT header (`LICENSE-RUSTYSYNTH.txt`) moves with the code if any of that file does |
| 5 | The sampler's loop-seam crossfade (`loop_span.rs` `fade_at`) and the linear ramp in `live_read.rs` (row 48) | `CrossfadeCurve::Linear`, a third variant of `tutti-graph`'s `CrossfadeCurve` (`gains` at `x`: `g_in = x`, `g_out = 1 - x`); the wrap fade already uses `EqualPower` | None. Layer 5 over layer 2. The seam's weights are `(k + 1) / (n + 1)`, which is `x` of `gains` to the bit |
| 6 | One home for the curve shapes (row 51: `SegmentShape`, `LfoShape::evaluate_periodic`, `PortamentoCurve`, and `tutti-modulation/src/shape.rs`'s `Bezier`), which under A2 are the laws of a trajectory (section 4.5a) | One module, one set of laws; it lands in step 2b0 | **Q25, reopened and under study (2026-11-11).** The layer table (graph, modulation and dsp are all layer 2 with no edge between them; `tutti-types` is vocabulary only) and the owner's lean toward `tutti-modulation`, possibly with `audio-automation` copied in |
| 7 | The PRNGs (row 54): the polysynth unison's xorshift and the IR synthesiser's LCG | `tutti-dsp`'s xorshift (`DitherNoise`'s generator, factored out) | Low value. The LCG's sequence changes, so the generated IR does. `hash_bipolar` is excluded: it is addressable and `tutti-modulation` may not depend on dsp |
| 8 | The stretch vocoder, `OverlapAdd` and `SampleFifo` (`tutti-sampler/src/stretch/vocoder.rs`, `buffers.rs`) | `tutti-dsp`, next to `StftGeometry`; the source-pulled `Unit` stays in the sampler | Lands with step 6 (the STFT processors). **Struck (owner, 2026-11-10).** Doc 026 stage 4 deletes the old vocoder and `crates/tutti-sampler/src/stretch/` whole, so moving it first would move code only to delete it. The new vocoder is written in `tutti-dsp` by doc 026 stage 2 |
| 9 | The limiter's recursion (row 7) | A `LimiterKernel` in `tutti-dsp/src/dynamics/`, as `CompressorKernel` and `GateKernel` are; the node keeps the `Param` and lane reads | **Landed (#443).** None. Pin it by the node's digest before and after |
| 10 | The phaser's sweep mapping (row 13) and the strip's balance law (row 52) | `tutti-dsp`: a function from `(range, lfo, depth)` to the sweep frequency, and `Pan` to a gain pair | **Landed (#444).** None. Both are pure functions in a node file, which CLAUDE.md puts in dsp |

**What stays where it is, and why.**

- **`Fold`, the graph meter (`block_stats`) and `latency::plan`**: layering.
  `tutti-codec` is layer 2 and needs `Fold`, so `Fold` is `tutti-types`'; the meter
  is a plan op, and graph and dsp are peers; `latency::plan` is the compiler's own
  solve and lives with `Topology`.
- **The engine `Fader`** (row 49): a plan over transport frames, in the crate that
  owns the plan.
- **The codec quantizer** (`tutti-codec/src/pcm.rs` `quantize`): the encoder's one
  quantization, at the file boundary. The dither that precedes it is dsp's.
- **The click waveform** (`tutti-nodes/src/click.rs`): one consumer, a windowed sine
  rendered at prepare.
- **The SoundFont's fixed-point oscillator** (`tutti-soundfont/src/oscillator.rs`,
  an `i64` with 24 fraction bits): a deliberate match with the SF2 reference render
  it derives from. Row 41's design half moves; this does not.

**What does not get a crate.** *Synthesis*: the polysynth and the soundfont share no
code (the polysynth is a SIMD oscillator bank, the soundfont a sample player), so a
shared crate would hold nothing. *Spatial*: already a crate. *Stretch*: one
consumer; its generic parts go to dsp (consolidation 8) and the unit stays.

### What gets renamed

Step 1a (PR 1a, renames only) did the rows it marks **done**; step 1b (PR 1b, the model) did the rest.

| Today (on `main`) | Becomes |
|---|---|
| `ReverbNode<D>` | `DesignNode<D>` (`FreeverbNode` stays, an alias): **done** |
| `ReverbDesign` | `NodeDesign`: **done** |
| `ReverbParams` | `ControlParams`: **done** |
| `network::contract::{DesignCase, cases}` | `contract::design` (**done**: the crate-root `#[cfg(test)] mod contract`); PR 1b splits out `reverb::cases` for the reverb-only ones |
| `network::kernel_contract::{KernelCase, Defect, Flawed}`, `kernel_contract!`, `Kernel`, `Design::kernel` | `contract::realization::{RealizationCase, Defect, Flawed}`, `realization_contract!`, `Realization` (non-generic), `Design::realization`: **done** (`network/kernel.rs` is `network/realization.rs`) |
| `Network` (the model) | unchanged; `Network: System + Lti`: **done** |
| `Ceiling` | `Ceiling<R>`; the reverb's is `Ceiling<MaxDecay>`: **done** |
| `Bounds` (the struct) | stays the reverb's, as `FreeverbDesign::Bounds`; no longer part of the model (`network/bounds.rs`, `impl Sizes`): **done** |
| `Realization` (non-generic) | `Realization<M>` (with an associated `type Bounds`) and `Effect<M>`: **done** |
| `Network::{tail, latency, is_stable}` | `impl System for Network` (`latency`, `tail`) and `impl Lti` (`spectral_radius`, `l1_norm`, `hinf_norm`, `frequency_response`); `is_stable` is `Lti::is_stable`, **overridden** with the network's own logic (the provided `spectral_radius() < 1` would refuse a frozen loop, whose radius is 1 and which is marginally stable, and would not see the hostile figures `lines_hold` refuses by name); `tail` and `latency` names unchanged: **done** |
| CLAUDE.md "the `tutti-dsp` paragraph names `Design`, `Realization` and `Network`" | names the traits of this module (doc 022 already promised it for 1a): **done** in 1b |


## 10. Staged plan

Each step is one PR that passes `cargo xtask ci`. Bit-identity rule: where a realization is
unchanged, record a digest of the node's render of seeded input at the commit
before the step, and assert it after (the method of doc 022 "Bit-identity of the
General MIDI send"); a render that calls libm (`tan`, `powf`, `exp10`) uses
`tutti_nodes::testing::assert_golden`, on the target it was recorded on. No new
test compares a render against another reference render.

| Step | Content | Moves | Proves it | After |
|---|---|---|---|---|
| 0 | This document | | | |
| 0b | **Split and consolidate the code the survey found (section 9), before step 1.** **The split has landed (#376: `tutti-measure`, layer 3, `check-layout.sh` places it), and so have consolidations 1 (#448), 2 (#451), 3 (#446), 9 (#443) and 10 (#444); the `tutti-dsp` `Error` split (#452) followed as a PR of its own, not a numbered consolidation.** The `tutti-measure` split (layer 3, over `tutti-dsp`; `window`, `geometry`, `grid` and `error` stay), then consolidations 1 (HRIR sinc onto dsp's resampler and `CosineWindow`), 2 (`Smoother`), 3 (delete the width-only folds, callers onto `Fold`), 9 (`LimiterKernel`) and 10 (the phaser sweep mapping and the strip balance law). Each is its own PR, the split first. The split PR updates CLAUDE.md and `scripts/check-layout.sh`; the rest are code moves under existing rules | `tutti-dsp/src/analysis/` to `tutti-measure`; `tutti-spatial` `hrtf/sphere.rs` and `smoothing.rs`; `tutti-types/src/downmix.rs` and its callers; `tutti-nodes` `dynamics/limiter.rs`, `modulation/phaser.rs`, `strip.rs` | The split: the workspace builds and tests with the same tests, unmoved; `check-layout.sh` places the crate. Each consolidation: the moved code's own tests, unchanged (never edited to pass); a digest of the node's render before and after where the output must not change (limiter, strip, phaser; golden-guarded where it calls libm); the HRIR resampler against its analytic anchors (unity DC gain, the response of the windowed sinc), not against the old output | #367 (merged) |
| 1 | **Landed: 1a (renames) and 1b (the model; see "Step 1b as built" below).** **Generalise on the reverb, which answers as `Lti`. Two PRs: 1a (the renames below marked "renamed in this step", landed as a renames-only PR with the port order pinned first) and 1b (the model).** The neutral `model` module: `Design`'s `type Model`, `blank`, `measure`, engineering `Bounds`/`Range`; the class traits `System` and `Lti` (`Memoryless` and `Autonomous` are not added until they have an implementer); `Realization<M>` and `Effect<M>` (**the code's `Kernel`, on `main` since #367 and reverb only, is renamed `Realization` in this step**, with `Design::kernel` to `Design::realization`, `kernel_contract!` to `realization_contract!` and `KernelCase` to `RealizationCase`; doc 022 and CLAUDE.md are updated in the same PR. `Effect<M>` **keeps its name**: it names the planar block shape (an audio effect), which is engineering, not a term the math conventions use for something else; recommend keeping it), `Ceiling<R>`, `Cadence` (only `Block` used); `Network: System + Lti` (`frequency_response` from the comb sums and the FDN solve over its own lines, `spectral_radius` from `rt60_bound`, `gain_bound` from the L1 bound `peak_bound` already computes, and `hinf_norm` the same figure until a tighter one exists); the `contract` feature with core and `Lti` groups and the reverb-only cases split out, including the three cases the FDN (PR #372) found (section 6.1): **landed**, the **stream clock** for ramps, and the **vacuity** hook; `ReverbNode` to `DesignNode` with `DryWet`, `NoBlend` and `()` as `Readouts`; the renames of section 9. The polysynth's `kernel.rs` is not renamed here: it holds oscillator functions, implements no `Kernel`, and its name is the step 7 PR's to settle | `network/{design,kernel,contract,kernel_contract,queries}.rs` (the files keep their names or are renamed with their types, the PR's choice), `tutti-nodes/src/reverb.rs`, `dry_align.rs` | The GM digest `the_general_midi_send_renders_what_it_always_did` and `retargeting_to_the_general_midi_network_changes_no_coefficient` pass unchanged; every 022 case passes under its new name and still fails its `Defect`; the new `Lti` group passes on Freeverb (response at the comb frequencies, measured decay against `spectral_radius`, linearity and shift invariance) and fails a defective realization; `tests/reverb_node*.rs` pass; port order pinned. The FDN's contract workarounds (fit depth, tone probes, static probes, the 15% RT60 tolerance) exist only because the model has no `frequency_response`; the solve below retires them. Risk named: `Network::frequency_response` for a mixing FDN is a control-side solve per frequency (64x64 at most). If it proves too large, `frequency_response` splits into a sub-trait `Spectral: Lti` and the reverb implements it in 1c | #367 (merged); ideally 1c (PR #372, the FDN) so **two reverb realizations** run the traits; if 1c is not in, the test-only `ToyFdn` is the second |
| 2 | **2a landed (the models and `Biquad`, and Q21 by measurement; see "Step 2 as built"); 2b open: the `Design`, the realization, `Cadence::every(16)` and the node migration, together, after step 2b0 (or in its PR) and once `DesignNode` has the three pieces 2b0 does not supply (tail and park, the public API, `DryWet`; see "What 2b has to solve").** **Filters (SVF, ladder, LR crossover), with several realizations of one H(z).** `SvfModel`, `LadderModel` (small signal, class 1 by `Linearised`), `CrossoverModel`; `Cadence::every(16)` in the node, **or the same ramp inside the realization at `Cadence::BLOCK`** (decided by measurement in this step, Q21); separate the crossover's design from its state; **a transposed-direct-form-II biquad realization and a lattice realization as second and third realizations of `SvfModel`'s response** (test-support realizations in `tutti-dsp`), so the `Lti` group is run on three realizations of one H(z) and the model is shown to be the equivalence class; `Describe::curve` and `Curve::Magnitude`; `ControlParams::snapshot`. **Consolidation 4 landed with this step**: the SoundFont's RBJ biquad is deleted; the voice takes `SvfModel::biquad()` and runs it in `DirectForm1`, both `tutti-dsp`'s. **Delivers G8** | `filter/*.rs` in `tutti-dsp`; `SvfFilterNode`, `LadderFilterNode`, `CrossoverNode`, `EqBandNode` become `DesignNode` aliases | The `Lti` group on all three realizations (mutants: a 2% cutoff error, a `tanh` in the loop); analytic anchors; ring time from the slowest pole against the measured tail; node digests (golden-guarded, `tan`) before and after, including a swept-cutoff block, which pins that `every(16)` is the old schedule; the segment-end case | 1 |
| 2b0 | **The time grid and the trajectory (amendments A1 and A2 of 2026-11-11), before 2b or in its PR.** (A1) `Cadence { period, anchor }` replaces the enum (`BLOCK`, `every`, `stream`, `block_independent`), `DesignNode` reads `Design::CADENCE` for the first time, and the block-independence case becomes required iff `block_independent()`. (A2) The trajectory and its laws (one set: `SegmentShape`, `LfoShape`, `PortamentoCurve` and `tutti-modulation`'s `Bezier`; consolidation 6, home per Q25), `ParamSegment` split into a pure piece plus an address and a position (32 bytes kept, no new `EventKind`), `ControlSource`, `Trajectory` and `Controlled<S, D>` in `tutti-dsp`'s `model` (doc 028 §3.4's `Controlled` is this type, built once: its `Settled` source lands with the dynamic EQ, doc 028 C1). `ControlParams` reads the *applied* controls through the source. `Ltv`, `Variation` and `Scheduled` are not built | `tutti-dsp/src/model/cadence.rs` and a new source module; the law's home (Q25); `tutti-graph/src/event.rs`, `tutti-modulation/src/shape.rs`, `tutti-polysynth/src/portamento.rs` | The `Controlled` group of section 6.2 with its mutants; the existing tests of each law moved unchanged, and digests of segment, LFO and portamento renders before and after consolidation 6; `filter_digests.rs` unchanged (`every(16)` at the block anchor); a trajectory crossing a block edge against the same inside one block | 2a; consolidation 6 once Q25 is answered |
| 3 | **Composition, first consumer the parametric EQ.** `Series`, `Parallel`, `SeriesN` and their realizations (two realizations in order; two summed), with the algebra as free functions over `Lti` parts that `Network`'s stage fold also uses; a parametric EQ design whose model is `SeriesN<SvfModel, N>`; the `Series` tail bound proof. Reverb step 3b (early reflections as `Parallel<Taps, Network>`) is then unblocked and is doc 022's own PR | `tutti-dsp` `model` module, a new EQ design in `filter/` | The algebra laws as unit tests (response of `Series` is the product of the parts' responses at random points; `Parallel` aligns latency; stability preserved); the composite passes the unchanged `Lti` group; the tail bound against the measured tail on random pairs; mutants: sum the latencies in `Parallel`, drop the alignment | 2 (and the reverb's 1b for the 3b consumer) |
| 4 | **Dynamics: compressor, gate, limiter, brickwall (class 3 and 4).** `SteadyState`, `Wiener`, `Hammerstein`, `GainControl` (over the memoryless `Memoryless` for the brickwall and the gain maps), `Readout`, `Readouts` publishing through `level_cell`, `Describe::readout`, `Curve::Transfer`. Decide `Driven<M>` against the code (Q3). Gate hold and the limiter's windowed minimum stay inherent (they are class E and a nonlinear operator). **Delivers G9** | `dynamics/*.rs`; the dynamics nodes | The class-4 settled-level sweep and step response (mutants: threshold +1 dB, ratio off, knee ignored); the readout equals the measured gain; node digests; the `multiband` readout test unchanged | 3 (`Wiener`), 2 (the `Describe` plumbing) |
| 5 | **Delay, chorus, flanger, phaser (class 1 and 2); the waveshaper (class 3).** `Controlled` over an LFO source; the frozen comb and all-pass `frequency_response`; the waveshaper's `Memoryless` with its harmonics and `Fidelity`; `Graded` for oversampling if the realization gains it | `delay.rs`, `swept_delay.rs`, `allpass.rs`, `waveshape.rs` and their nodes | Comb response against the FFT; notch frequencies; the slow-variation case; DC and odd-symmetry cases; the aliasing case; digests | 2 |
| 6 | **Convolver, HRTF (class 1, published model).** The published control path (5.3), `FirSummary` unified with the convolver's model, `Graded` for the partition; `ReverbKind` gains the convolution variant (doc 022 step 2). Consolidation 8 is struck (owner, 2026-11-10): doc 026 stage 4 deletes the old stretch vocoder, so it is not moved | `convolver.rs`, `ConvolverNode`, `tutti-spatial` hrtf | `frequency_response` against the IR FFT; `latency` = `B`; a `Loan` is never parked; the export-fork test | 1 and doc 022 step 2 |
| 7a | **Basis and metric (amendment A3, 2026-11-11), before the panners and before doc 028's ambisonics (F1a).** `Speaker` gains `Harmonic { acn }`, `Ear` and `Object` (renaming it `Channel` is a mechanical edit); `ChannelTopology` gains `gram` (the metric, diagonal) and the ambisonic constructor (doc 023 G11); `Fold` gains `from_matrix(src, dst, m)` (validates width **and** topology agreement), `matrix`, `column_power` and `is_isometry(tol)` relative to the metrics, and **stores `f64`, narrowing once at the apply** (decided 2026-11-11, Q26: it changes a shipped type). `Fold::between` stays the default policy. The `System`/`Lti` impls for order 0 are on a one-field wrapper in `tutti-dsp` (`Fold` is below it, row 32). The planned `GainMatrix` trait, the separate `Ambisonic` topology and the Euclid-only `is_isometry` are not built | `tutti-types/src/{topology,fold}.rs` (and `fold/tests.rs`), the wrapper in `tutti-dsp` | The existing `Fold` tests unedited, and the ITU downmix figures to the bit (a computed gain that moves in the last bit under `f64` is argued, not pinned); a metric isometry case (mutant: the Euclid test, which rejects the N3D/SN3D conversion); a wiring refusal (SH order 3 into a 16-speaker bed; mutant: a width-only check); a rotation is an isometry in both norms | 1 |
| 7 | **The rest, each in the PR that needs it, in this order:** resampler (class 6, own shape trait; `Fidelity`, `Graded`, latency; used by export); panners and fold (`Design`s whose model is a `Fold`, class 1 at order 0; step 7a first); envelopes, oscillators, LFOs and noise (class 5: `Autonomous`, `Periodic`, `Piecewise`, `Noise`, in `tutti-polysynth` and `tutti-soundfont`, the latter gated on a `tutti-dsp` edge); stretch; dither | per family | per family, with a mutation record | the step 1 shape |

### Step 1b as built

What PR 1b did, and where it differs from the sketches above.

**The `model` module** (`tutti-dsp/src/model/{system,lti,design,realization,ceiling,cadence}.rs`,
private, exported at the root, added to the path gate). `System`, `Lti` (with `Complex64`
re-exported), `Design` with `type Model: System + Clone + Send`, `type Bounds: Sizes`, `type Range: Range`,
`blank`, `measure` and `const CADENCE`; `Realization<M>` with an associated `type Bounds` (Q16: the
sizes stay the design's, the realization names the type it is prepared from), `Effect<M>` holding
`process`; `Ceiling<R>` over `Range` with `MaxDecay` (range-checked, `new -> Option`, `TryFrom`, no serde in
the library: the range-contract test carries its own serde impl); `#[non_exhaustive] Cadence::Block`
only. `Bounds` (the reverb's struct) is `network/bounds.rs` and `impl Sizes`. `Ring` moved to `model/system.rs`.

**`Network: System + Lti`** (`network/response.rs`). `frequency_response` composes the stages as a vector transfer
(pre-delay, taps, all-passes per channel, an FDN solved over its own lines, then the kernel's
mid and side with width and wet): a diagonal `A` in O(N), a Householder or reflection by a
rank-one update (Sherman and Morrison) in O(N), a circulant, Hadamard or dense one by Gaussian elimination.
A stage that takes one signal is handed the mean of its input channels (the kernel's mono sum).
It is checked against the realization, not against another render: the `RealFft` of the
kernel's impulse response against the model on the FFT bins is within 4e-4 to 2e-3 relative (L2)
for Freeverb and for the mixing FDN, so the 1e-2 tolerance of the `response` case has a 5 times margin,
and a 5% loop gain or a 10% output gain fails it. **No `Spectral` split was needed.**
`spectral_radius` is `10^(log rho + log sigma)` of the slowest band (`rt60_bound`'s own figure), an
all-pass's `|g|^(1/D)`, 1 for a lossless loop and infinity for what the analysis refuses; measured decay
rates of both reverbs are 11 to 28% faster than it says (a bound). `is_stable` keeps its own logic (§9
corrected: it is not the provided default).

`l1_norm` and `hinf_norm` are sufficient bounds. The all-pass l1 norms are exact (`1 + 2|g|` for the
lattice, `1 + 1/(1 - |g|)` for Freeverb's, from the impulse responses), taps are `sum |gain|`, the width fold
`max(1, w)`, a FIR infinity. **A diagonal FDN** is exact: `sum_i |c_ri b_i| / (1 - a_i)`, `a_i` the l1 norm
of line `i`'s absorption. **A mixing FDN**: the plan's `||B|| m / (1 - q) ||C||` does not follow from
`||S_(n+m)|| <= q ||S_n|| + m ||B||` (a sup over time of a norm over lines does not survive lines
of different delay: a factor `sqrt(N)` is lost), so the shipped bound is a different one with a proof in
`Fdn::l1_gain`: for `0 < r < 1`, Cauchy and Schwarz give `||h||_1 <= (1 - r^2)^(-1/2) sup_(|z|=r) |H|`, and
`|H|` on that circle is at most `||C|| r^(-m) ||B|| / (1 - q)` with `q = ||A|| max_i |G_i| r^(-m_i)` (a delay of
`m` has magnitude `r^(-m)` there, an absorption at most its majorant `sum |g_k| r^(-k)`); the smallest over a
log grid of `1 - r` is returned. It is loose (a Householder FDN of 8 s decay gets some `1e4` against a
true response of a few; infinity where `q >= 1` for every `r`, as with bands that rise with frequency).
`hinf_norm` is the resolvent bound at `r = 1`, with the absorption's peak a grid maximum plus a
Lipschitz margin (the sum of the magnitudes of the three band weights overshoots when the bands rise), and
never above `l1_norm`: it is 2 to 4 times the measured peak of Freeverb and of the FDN where the grid resolves
the poles, so the contract's slack is 50.

**The contract** (`contract` feature, off by default, `rand` optional; the one public module):
`DesignCase`, `RealizationCase`, `LtiCase`, `ReverbCase` (the reverb hooks, which `Defective` forwards),
`Hook`, `Anchor`, `cases::{design, realization}`, and `design_contract!(d, Lti, reverb)` and
`realization_contract!(d, Lti, reverb)` (the groups follow the design after commas: a `macro_rules` expression
cannot be followed by a colon). Files `contract/{basic,design_case,harness,lti,macros,reverb}.rs`
(`basic`, not `core`, which would shadow the `core` crate) and a `cfg(test)` `flaws.rs` with `Defect`, `Flawed`,
`Defective`, `Mutant`, `ToyFdn` and the doubles `Scaled`, `Lying` and `Lie` (a model that lies about its `tail`,
`hinf_norm` or `l1_norm`). `tests/contract_public.rs` runs the macros on a gain outside the crate. New cases, each
with its mutation as a test: `landed` and `stream_clock` (the ramp cases; they need a realization that ramps, so
their defects are on the FDN, with two test-only flaws in `FdnKernel`), `vacuity` (`DesignCase::vacant`, a
list of `(Hook, why)`), and the `Lti` cases (`radius`, `anchors`, `norms`, `ports`, `linear`, `response`,
`decay`, `tail_tight`). `peak_bound` is retired (`stability` holds the render to `l1_norm`). The FDN's
workarounds (fit depth, tone probes, static probes, the 15% RT60 tolerance) are kept: the response case holds
the realization's whole response, but the RT60 cases read the model's `rt60` and do not use it.

**Findings, for the record.**

- `FdnKernel` realized `Matrix::Diagonal` as the reflection along line 0 (`I - 2 e0 e0^T`, a sign on line 0's
  loop), which changes no decay and moved line 0's comb teeth half a spacing (relative response error 0.8 at
  diffusion 0). Fixed after step 1b (owner-approved, 2026-11-11): the kernel's `Diagonal` is now the identity, as
  the model and doc 022 say, so the `response` case runs every probe and the renders at diffusion 0 changed.
  The identity and a reflection have determinants +1 and -1, so no ramp between them is orthogonal throughout; the
  kernel switches `m` of `I - 2 m v v^T` between 0 and 1 at the start or end of the ramp instead (see `fdn.rs`).
- The kernel rescales a set of band gains whose response exceeds 1 anywhere (to peak at 0.9999), and
  **reachable controls do ask for it** (a mid band that rings far longer than both neighbours: peak 1.015
  at corners of the grid, so the kernel lowered the loop gains by 1.5%, a large share of the decay at 8 s).
  The scaling now lives in the design: `FdnDesign::design_into` writes the scaled gains into the network
  (`fdn::rescale_bands`, the one function the kernel also calls), so the kernel finds nothing to rescale
  and the network is what runs. The renders are bit-identical (the GM digest, the node digests and
  `retargeting_to_the_general_midi_network_changes_no_coefficient` pass unchanged). The cost is one
  existing case: `rt60_matches` and `band_order` read the decay the controls ask for, which for such
  controls is not the decay the network has, so `requested_rt60` returns `None` there (the controls ask
  for something unrealizable). `no_rescale` holds every reachable design to a peak of at most 1.01 (the
  kernel's own 97-point grid reads up to 0.12% under the true peak).
- The slack of `hinf_norm` is checked only where a grid resolves the sharpest pole (its bandwidth is at
  least twice the bin spacing): a system that rings for seconds has poles narrower than any affordable
  grid, so its measured peak is under the true one, and the slack would blame the bound.

**Not done, by decision.** `Readouts` (step 4: dynamics is its first consumer); the
the `Memoryless` trait (no implementer yet; `Ltv` is no longer planned, section 4.2); `Describe` and `Curve` (step 2b).

### Step 2 as built

Step 2 was split as step 1 was: **2a** (the models and `Biquad`; landed) and **2b** (the `Design`,
the realization, `Cadence::every(16)` and the nodes as `DesignNode` aliases, together, with
`ControlParams::snapshot`, `Describe::curve` and G8; open, with the reasons below). A first cut of 2a also
carried `SvfDesign` and `SvfRealization`; it was trimmed, because without the nodes migrated it was a second
block loop beside the nodes' own. Consolidation 4 (the SoundFont's biquad) landed after 2a as its own PR: `bi_quad_filter.rs` is deleted,
`tutti-soundfont` gains the `tutti-dsp` edge, and the voice's `low_pass` (the SF2 Q fit and the near-Nyquist
bypass) designs a `SvfModel` low-pass and runs its `biquad()` through `DirectForm1` (a public f64 direct form I
history with `process`, `hold` and `reset`). The voice's output moved in the last bits (f64 coefficients and
state, the model's `tan` prewarp), with the owner's approval; its golden digest was re-recorded. The rustysynth
notice stays on the voice (the `Q` fit is the one line of that file that remains), and nothing derived from it
went to `tutti-dsp`.

**Models** (`tutti-dsp/src/filter/{biquad,svf_model,ladder_model,crossover_model}.rs`, exported at
the root). `SvfControls { ty, freq, q, gain }` (`Copy`, `PartialEq`, with `MAX_Q = 1000`, the one clamp
added to `SvfCoeffs::new`'s: a larger `Q` has a radius that rounds to 1); `SvfModel` (`System + Lti`:
`SvfCoeffs` now carries `g` and `k`, so the response reads the analog prototype `m0 + (m1 s + m2) /
(s^2 + k s + 1)` under `s = tan(w/2) / g`, accurate to DC; the poles come from the prototype's roots
through `z = (1 + g s) / (1 - g s)` in a cancellation-free form, so `spectral_radius` is exact and `1 - r` is
resolved to a part in 10^16; `l1_norm` is `(|b0| + |b1| + |b2|) min((1 - r)^-2, K / (1 - r))` with
`K = 2r / |p1 - p2|`; `hinf_norm` is the peak on a grid and a log-spaced run around the resonance,
refined by golden section and raised by 1e-6; `tail` is the envelope bound against the largest `|h|`
found in closed form, within 3 times the measured ring); `SvfModel::biquad()` gives the same `H(z)` as a
`Biquad` (a plain value of `b0..b2, a1, a2`, public).
`LadderModel` is the **small-signal** linearisation (`tanh x = x`), class 1 by that definition: the
loop `u = x / (1 + k F L^3)` with one-pole stages `L = g1 (1 + z^-1) / (1 - p z^-1)` and the feedback
from the last stage's state, four poles by Durand and Kerner, `l1_norm` infinite (no bound), `hinf_norm`
numerical, `is_stable` false at a resonance near 1 (the saturator holds the real filter there). It has no
`Design` and no realization: a nonlinear filter does not pass the `Lti` group by construction, so its
test is the small-signal impulse through `ladder_step`. `CrossoverModel` separates the crossover's design
from its state: `Crossover` now holds a `CrossoverModel` (the rate, the solved frequencies and the splits'
`Split` coefficients) and only the integrators and the ramp are its own (`Crossover::model()`); the model is a
multi-output `Lti` (`outputs() = bands`, each band `HP^2` of the earlier splits, `LP^2` of its own and the
all-pass of the later), with radius from the lowest split, `hinf_norm` 1 and `tail` with a double pole.
The crossover's private `Biquad` is now the shared `Biquad`.

**Tests of the models.** The models are not run under the `Lti` contract group, which takes a `Design` and
its realization; their own unit tests hold the response (against the RBJ cookbook's biquad, an independent
derivation, for all eight types at 43 frequencies each, to 1e-9, and the prototype's closed forms), the radius
against the direct form's own denominator roots, the tail against the measured ring and a hostile control
still giving a stable model. 2b runs the `Lti` group on the realization.

**Q21, by measurement.** A lane sweeping the cutoff 200 Hz to 8 kHz in 85 ms, a 40 Hz LFO on the cutoff
(150 to 2850 Hz) and a block-rate cutoff step 800 Hz to 3 kHz, 64-frame blocks, `Q` 1.2, seeded noise,
against the per-sample solve along the same parameter path (the method of `COEFF_INTERVAL`'s doc comment):

| Case | Node, `every(16)` | `Block` with a coefficient ramp to the block-end target |
|---|---|---|
| lane sweep 200 Hz to 8 kHz in 85 ms | 4.8e-5 | 8.2e-4 (17 times) |
| 40 Hz LFO lane | 1.5e-3 | 2.4e-2 (15 times) |
| block-rate step 800 Hz to 3 kHz | 1.5e-3 | 2.7e-2 (17 times), not bit-identical to the node |

(max error over the peak output.) The internal ramp is an order of magnitude from the truth even for a
block-rate control, because the node ramps the **parameter** and solves `tan` at every 16th frame
while a coefficient-space ramp across the block is a straight line through a curved path; and it cannot
see a lane at all. It cannot preserve the old schedule, which the nodes' digests pin, so **the filters keep
`every(16)`**, which 2b0 adds to `Cadence` with the realization, one `process` call ramping across its segment.
Recorded as the decision of Q21; the reverbs stay at `Block`.

**Digests** (`tutti-nodes/tests/filter_digests.rs`, recorded **before** any change, on the unmodified nodes; every
solve reaches libm, so each is `assert_golden`; the committed values are **placeholders (0) until recorded on
glibc 2.39**, with the local glibc 2.43 figures, which are identical before and after the
crossover's refactor: SVF `0xf13aae01ca0d27c9`, bell `0xc913d3e2bb80c6df`, EQ band `0x93db583971fce46b`,
ladder `0xfacbee3adce13507`, crossover `0xe487a6a17b136f8d`). They cover a cell moved between blocks
(the block-rate ramp), a cutoff lane, a Q lane and both, at 44.1 and 48 kHz, `f32` and `f64`, mono and stereo.

**What 2b has to solve** (why the nodes are not aliases yet; the digests are the gate). `DesignNode` is a
`Cadence::BLOCK` node today and the four filter nodes are not hosted by it for these reasons, each a change
to `DesignNode` or `ControlParams`, not to a filter:

1. **Segments.** `DesignNode::process` designs once per block; `every(16)` needs `segments(size, unprimed)`,
   a design, a `retarget` and a `process` per segment, the controls read at the segment's last frame
   (`ControlParams::read` takes a frame), and the first block's first sample solved alone.
2. **The block-rate glide.** A moved cell glides: the node ramps the **parameter** from the value the last solve
   used to the cell's over the block (`Ramp::new(last, base, size)`), then solves at each segment end. The
   previous solve's parameters live in the node, so `ControlParams` needs the applied controls to ramp from.
3. **The hysteresis.** The coefficients are held (not re-solved) while a control stays within `FREQ_EPS`,
   `Q_EPS` and `GAIN_EPS` of the last solve; a model re-designed at a drifted control is a different bit pattern.
   The read must return the applied controls when nothing moved past the epsilon, so `next == prev` and the held path runs.
4. **The tail.** The filter nodes report `Tail::Unknown`; `DesignNode` reports a finite ceiling and **parks**
   (cuts to silence) once the input has been silent for the tail, which changes the render of a ringing
   filter at -60 dB. A filter needs `Tail::Unknown` or a never-park mode. The ceiling of an SVF design is `Unbounded`.
5. **The public API** (Q9): `frequency()`, `q()`, `gain_db()` return the shared `Arc<AtomicF32>` cells,
   `set_filter_type(&mut self)` re-types a node in place (a design with the type in its controls, changed on the
   control side), `F: Real` is a type parameter of the node and `with_gain_db` a builder; `EqBandNode` is an `SvfFilterNode`
   plus a `BandState` bypass, a wrapper that is not a design.
6. **The crossover** is neither `every(16)` nor `Block` as `DesignNode` has them: `split_block` ramps the frequencies
   linearly over the **block** and re-solves every 16 samples from the interpolated frequency, with `bands * channels`
   outputs and a `Crossover` per channel.
7. **No `DryWet`.** None of the four filter nodes has a mix port, so each would be `NoBlend` (doc 013's M4 makes the
   mix a graph primitive); nothing here adds a `DryWet` user.

**After the amendments (2026-11-11).** Blockers 1, 2, 3 and 6 share one structure, `theta_applied =
sampler(theta_target; grid, law, hold-state)`, and step 2b0 gives it the names section 4.5 and 4.5a
state. 1 is `Cadence::every(16)` (`segments` is its grid iterator, and "unprimed" is the grid's
first point). 2 is a trajectory piece from the *applied* controls, now the source's state and not a
node field, to the cell's value over the block. 3 is a dead-band on the source (it returns the held
controls unless they moved by more than the tolerance), so far only partly: `Controls: PartialEq`
becomes "moved by more than a tolerance", a small extension of an existing trait. 6 is blocker 2's
mechanism with two laws on two grids: a block-long piece in frequency (in `theta`-space) with the
solve on `every(16)`, on a `Controlled` whose model is the multi-output `CrossoverModel`. **Left for
2b itself:** 4 (the tail and park rule: park only on a finite ceiling, and `Unbounded` never
parks, is the proposed rule, since the SVF design's ceiling is `Unbounded` and `DesignNode` parks),
5 (the public API, Q9) and 7 (no `DryWet`; each node is `NoBlend`).

**G8 is not delivered in 2a, on purpose.** `Describe` and `Curve` need a home that nothing has: the trait is
implemented by a node (`tutti-nodes`, layer 4) and read by `tutti-session` (layer 7), so it must sit below both
(`tutti-graph` beside `Node`, or `tutti-types` for the plain query values doc 023 row 11 names) and doc 023's Q1
does not say which; `DesignNode::curve()` and `ControlParams::snapshot` have no filter node to serve until 2b. The
model side is in place (`SvfModel::frequency_response`, exact), so the curve is the node's one `design` and
`Lti::magnitude`.

**Why this order.** Step 0b goes before step 1 because it moves code the later steps build on: the analysers leave `tutti-dsp` before the class traits arrive in it, so the new `model` module lands in a crate that holds processors only, and the smoother, `LimiterKernel` and the balance law are in dsp before steps 4 and 5 write their models. Step 1 is the only step that changes a shipped API, so it goes
first and on the family whose behaviour is pinned by a digest; the reverb answers
as `Lti` so the first class trait is proved on the structured, largest model, the
one most likely to bend a trait made for biquads. Step 2 is next because filters
disagree with the reverb's cadence and are the textbook case for the realization
split: three realizations of one H(z) under one contract is the clearest evidence the
model is the equivalence class, and it delivers G8. Composition (step 3) lands
where it is first *needed*: the parametric EQ is its smallest consumer and it
unblocks reverb 3b, which doc 022 had marked a blocker. Dynamics (step 4) is the
family that most tests "don't force a runtime mechanism" (per-frame lanes), so it
comes once the node and contract are stable, and `Wiener` lands with it. Steps 5 to 7 are independent; step 6 waits on doc 022's own step 2; the
instruments are last because they are the families where the generic node is
**not** the destination, only the model and the contract.

Doc 013's change note is updated when step 1 lands (the tail and park logic is now
the node's, not the reverb's); doc 022 gains a pointer here, and its section 4/5
text is not rewritten.

[Doc 028](028-dsp-roadmap.md) places the DSP coverage roadmap (oversampling, ADAA, true peak, EQ, ambisonics and the rest) in these steps, with the abstraction each family needs.

## 11. Open questions

| # | Question | Recommendation |
|---|---|---|
| Q1 | Is `System` (latency and tail) the base of every processor class, with `Autonomous` outside it? | Yes. Every node declares both in `Shape`; the park logic needs only these. A generator has no input to end, so it is not `System` and its node reports a tail only when it has an audio input |
| Q2 | Name the shared concept `Model`? | Keep the word in prose (doc 022's and the owner's); in code the class traits are the names (`Lti`, `Memoryless`), and `Design::Model` is the associated type |
| Q3 | Per-frame control lanes (the dynamics' threshold and ceiling): `Cadence::Frame`, a second realization shape `Driven<M>`, or leave those nodes hand-written? | Decide in step 4 against the code. Order of preference: `Driven<M>` (the node picks per block, `Frames` vs `Base`); else the three nodes keep their `process` and adopt `Design`, the class traits and the contract only. Not `Cadence::Frame`: it is the fixed shape the owner warned against |
| Q4 | **Resolved by the composition algebra.** The curve of a series EQ chain | A parametric EQ is one design whose model is `SeriesN<SvfModel, N>`, with an exact product `frequency_response`. A chain of separate graph nodes is the application's product of each node's `Describe::curve`. No `Chain` model (section 4.9) |
| Q5 | Class-coverage gate: a script or a macro? | A script (`check-contract-coverage.sh`), as `check-range-types`. A macro cannot see every `impl` |
| Q6 | Contract as a `tutti-dsp` feature or a `testing/` crate? | The feature (the owner's direction, and the dev-cycle argument of section 9) |
| Q7 | `Quality`: four tiers or AU's 0 to 127? | Four tiers with `from_au`. A native algorithm has few real tiers, and 128 steps would be false precision |
| Q8 | May an export render at a different `Quality` from the live graph? | Not yet. It changes `System::latency`, so PDC and the fork's plan differ from the live one. Revisit with `RenderMode::Offline` once a family has a tier where it pays |
| Q9 | The nodes' public inherent API (`SvfFilterNode::frequency()` returning an `Arc<AtomicF32>`, `ModDelayNode::set_depth`, ...) | Keep it by having each family's `ControlParams` expose the same handles; make the old node names aliases; fix callers in the same PR (the project's rule against shims) |
| Q10 | Publish the applied `Controls` back for a live curve at the modulated value? | Yes, as a `seq_cell` written once per changed block, in step 2, if the application wants a live curve; the base curve works without it |
| Q11 | Filters generic over `F: Real` (f64 by default): is the design generic too? | No: the model is always `f64` (analysis) and only the realization is generic over `F` |
| Q12 | Instruments: a node of their own? | A separate design. They share `Design`, the class traits (`Autonomous`) and the contract; the event-driven shape and `tutti-voicing` are their node's concern |
| Q13 | Share the contract's measuring helpers with doc 023's plugin probe? | Yes: `tutti-plugin` (layer 6) may depend on `tutti-dsp` with the `contract` feature, so a hosted plugin's latency and tail are measured by the code that measures a native realization's |
| Q14 | Is `Lti::frequency_response` single-input? A sidechained compressor has two inputs | `frequency_response` takes `output` and is defined from every input (`inputs()`); a sidechain's curve is a second `Memoryless` only if a family wants it |
| Q15 | Does an analyser (row 23, class M) deserve its own pattern? | Not here. `Readout` is the likely surface; revisit when a result is wanted in the application |
| Q16 | `Realization::prepare` takes `&Bounds` on `main`; with `Bounds` moved off the model, does `Realization<M>` keep it? | Drop it from the trait and give the realization its sizes at `Design::realization()` (the design has the bounds), so `Realization<M>` names no engineering type. If a realization must be sized after construction, an associated `type Bounds` on the realization is the fallback; decide in step 1 |
| Q17 | Offer feedback composition (`Feedback<A, B>`)? | No. Series and parallel of LTI models preserve stability exactly; closing a loop needs a small-gain bound that depends on the loop. The FDN's recirculation stays structure inside its model |
| Q18 | `spectral_radius`, `l1_norm` and `hinf_norm` as upper bounds, or exact? | Upper bounds, documented as sufficient (the reverb's `rt60_bound` and `peak_bound` already are). A model with an exact value says so in its docs, and the contract measures that the true radius and norm are under the stated ones |
| Q19 | `Parallel`'s latency alignment: in the model or in the realization? | In the model: it states the aligned latency (`max`) and its `frequency_response` includes the compensating delays, so a fused or composed realization are both held to one answer |
| Q20 | A moving `theta` inside a model (an LFO in a chorus): `Ltv`, or just a note in the docs? | **Revised 2026-11-11: neither a trait nor a note.** It is the class-2 model of section 4.2 (a linear parameter-varying system along a trajectory, section 4.5a): `Controlled::at` is the frozen-time query and the source's `slew_bound` is the declared variation, a checkable number against the `Cadence` period. `Ltv` and `Variation` are not built |
| Q21 | Does a design need `Cadence::Every`, or can the realization ramp internally at `Cadence::Block` (the FDN's finding)? Are the filters the same case? | **Resolved in step 2 by measurement: the filters keep `Every(16)`.** An internal coefficient ramp between block-end targets is not the old schedule even for a block-rate control (it is 17 times further from the per-sample solve, and not bit-identical), and it cannot see a lane inside a block ("Step 2 as built"). `Block` stays right where the control is block-rate and the realization owns the ramp (the reverbs). **Re-read 2026-11-11:** the filters' model is "a parameter-space path solved on a 16-frame grid", which is `Cadence::every(16)` with its law, and the coefficient-ramp alternative defines a different system; the grid is anchored at the block (Q27) |
| Q22 | Keep the name `Effect<M>` for the planar block shape? | Yes. It is the engineering shape of an audio effect, not a term the field uses for something else; `Realization<M>` is the renamed math term |
| Q23 | The frozen-time query (`Controlled::at`) overstates decay near Nyquist under linear-interpolated modulation (0.46 s measured for 1.2 s requested). Fix the model or document the gap? | Document the gap and keep the frozen queries to the low and mid bands under modulation (section 4.2); an interpolation-aware `Fidelity` is the model-side fix if a family needs the top band |
| Q24 | Where exactly does the analysis split cut? `window.rs` and `geometry.rs` are used by processors (`fft/scratch.rs`, the resampler and vocoder after consolidations 1 and 8), and they name `grid.rs` and `error.rs`; `transform.rs` (the STFT) builds on both | Cut at what a processor needs: `window`, `geometry`, `grid` and `error` stay in `tutti-dsp` with the FFT, everything that reads a signal and reports a figure moves. Settle the file list in the split PR by what fails to compile, not by this table |
| Q25 | Where do the curve shapes (row 51, with `tutti-modulation`'s `Bezier`) live, now that under A2 they are the laws of a trajectory and the model needs them, given that graph, modulation and dsp are peers at layer 2 and `tutti-types` is vocabulary? | **Open, under study (2026-11-11).** Candidates: `tutti-modulation` (the owner's lean; it needs the layer question answered, because `tutti-graph` and `tutti-dsp` would both have to depend on it and the three are peers at layer 2); `tutti-types`, beside `Fold` (the lead's earlier recommendation: the trajectory is the value type of a control signal and its evaluation law is its definition, but it puts functions in the vocabulary crate that CLAUDE.md refuses beyond `Fold`); a crate made from `audio-automation` (copied in), which already holds curves and automation; or `audio-automation` kept external, which keeps the `expected Curve, found Curve` pin of CLAUDE.md and cannot sit below the engine. Consolidation 6 and step 2b0 wait on it. Whatever the answer, a piece is pure and the event is a piece plus an address |
| Q26 | Does `Fold` store `f32` or `f64`? | **Decided (owner, 2026-11-11): `f64` storage, narrowing once at the apply.** Doc 028 §3.5 wants matrices at their own precision for the identities (isometry, commutation), which `f32` gains cannot hold; the narrowing happens where the audio is, as doc 028 states. It changes a shipped, tested type (`Hit.gain`, `fold/tests.rs`), so it is step 7a's and its tests are not edited. A dense 16 by 16 decode through a sparse row list may need a dense mode |
| Q27 | Should `every(16)` move to a stream anchor, making the filters block independent? And does `Design::CADENCE` name the grid the design is sampled on (the FDN: per block, its slew grid private) or the grid the render is a function of (the FDN: `stream(32)`)? | Open. The first changes the filters' render and their digests (re-recorded only for an argued reason, CLAUDE.md); the amendment makes the difference visible and does not decide it. On the second, `DesignNode` splitting at a finer stream grid would redesign the FDN per chunk, which doc 022 measured as wasted, so the render's grid and the design's may be two declarations |
| Q28 | How exact must a trajectory's laws be? The 32-byte event has no room for a second Bezier control point, and `ParamSegment.position` and `length` are `f32` (exact to 2^24 frames) | Open, an owner question. Either a piece is approximated (a tolerance the spec for the consumer sets: IAMF's Bezier has one control point per quadratic; conformance there is PSNR-based, section 4.10) or the event carries a tension in `shape_param`. A contract case for the vector atomicity of `N` pieces at one offset is not yet written |
