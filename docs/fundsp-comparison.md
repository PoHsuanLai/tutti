# Tutti custom DSP vs FunDSP opcodes — implementation comparison

**Purpose.** Document, with code-level evidence, why Tutti hand-rolls `impl AudioUnit` DSP in `tutti-units`
instead of using the equivalent FunDSP opcodes. This is the reference that justifies *keeping* the custom
units (rather than "migrating" to FunDSP) and identifies the few genuine gaps worth *adding* from FunDSP.

**Method.** Code + spec analysis only — both implementations read side by side; no audio rendering. Every
verdict cites `file:line` on both sides. Paths are relative to `crates/tutti/crates/`.

**TL;DR.** The param *transport* is already FunDSP (`bevy-tutti/src/graph/reconcile.rs` routes every scalar
through `Net::set` + `Setting`). What is custom is the *DSP*. Of the custom units, **none should be replaced**:
they are either equal-algorithm-with-better-integration (SVF), strictly more capable (ladder, delay, limiter,
modulation), or have no FunDSP counterpart at all (compressor, gate, spatial, convolution). FunDSP's *unique*
offerings that Tutti lacks — reverb (already adopted) and **waveshaping/distortion** — are the additive wins.

| Unit | Verdict |
|---|---|
| SVF filter | **EQUAL algorithm, Tutti better integration** — keep |
| Ladder / Moog | **CUSTOM BETTER** — keep |
| Delay | **CUSTOM BETTER** — keep |
| Limiter | **CUSTOM BETTER** — keep |
| Compressor / Gate | **NO FUNDSP EQUIVALENT** — keep |
| Chorus / Flanger / Phaser | **CUSTOM BETTER** — keep |
| Spatial (VBAP / binaural) | **NO FUNDSP EQUIVALENT** — keep |
| Convolution | **NO usable FUNDSP EQUIVALENT** for the DAW path — keep |
| Reverb | **FUNDSP ONLY** — already adopted (`reverb_stereo`); `reverb2/3/4_stereo` are gaps to add |
| Distortion / waveshaping | **FUNDSP ONLY** — real gap, top add candidate |

---

## 1. SVF filter — EQUAL algorithm, Tutti better integration

| | FunDSP (`fundsp-tutti/src/svf.rs`) | Tutti (`tutti-units/src/filter/svf.rs`) |
|---|---|---|
| Topology | Simper/Cytomic SVF, `SvfCoefs<F: Real>` (`svf.rs:17`) | Same Cytomic SVF, `compute_svf_coeffs` (`svf.rs:34`); identical `g = tan(π·fc/sr)`, `k = 1/Q` |
| State precision | Generic `F: Real` (f32/f64) | Generic `F: Real`, defaults to **f64** (`SvfFilterNode<F = f64>`, `svf.rs:177`) for low-cutoff accuracy |
| Filter mode | **Type parameter** via `SvfMode` trait (`svf.rs:227`); `LowpassMode`/`HighpassMode`/`BandpassMode`/`NotchMode`/`PeakMode`/`AllpassMode` are distinct types (`svf.rs:281,340,389,438,487,537`) | **Runtime enum** `SvfType` (`svf.rs:11`), switchable in place via `set_filter_type` (`svf.rs:236`) |
| `set()` | `FixedSvf::set` honors `Center`/`CenterQ`/`CenterQGain` | `set` decodes `UnitParam::Cutoff`/`Q`/`GainDb` (`svf.rs:298`, stereo `:593`) |
| Audio-rate mod | Via input ports on the non-fixed `Svf` | **Optional** cutoff/Q param-input ports via `with_param_inputs` (`svf.rs:410`), integrated with dawai's `param_ports.rs` rebuild; bit-identical fast path when absent |
| Latency | 0 | 0; block-rate coeff guard (`svf.rs:251`: 0.01 Hz / 0.0001 Q / 0.01 dB thresholds) |

**Verdict: EQUAL algorithm, keep Tutti.** The DSP is the same Cytomic SVF. Everything that differs favors the
DAW: a runtime mode enum (vs a type parameter that would force node replacement to change shape), f64 state
by default, atomic `Arc<AtomicF32>` UI handles, and the bespoke audio-rate param-port path wired into dawai's
modulation rebuild. Migrating to FunDSP's `Svf` would lose all of that and gain nothing.

## 2. Ladder / Moog — CUSTOM BETTER

| | FunDSP `Moog` (`fundsp-tutti/src/moog.rs`) | Tutti `LadderFilterNode` (`tutti-units/src/filter/ladder.rs`) |
|---|---|---|
| Modes | **LP only** (`Moog<F, N>`, `moog.rs:17`) | **LP12 / LP24 / HP12 / HP24** (`LadderType`, `ladder.rs:11`) |
| Saturation | tanh on the **final stage only** (`moog.rs:93`) | zero-delay-feedback ladder with explicit **`drive` pre-saturation** (`ladder.rs:71`, atomic handle `:104`) |
| `set()` | honors `Center`/`CenterQ` (`moog.rs:103`) | `UnitParam::Cutoff`/`Q`/`Drive` |

**Verdict: CUSTOM BETTER, keep.** FunDSP's `moog()` is a single LP mode with no drive control. Tutti offers
four slopes/modes and a dedicated drive stage — features dawai's `LadderFilter { cutoff, q, drive }` exposes.

## 3. Delay — CUSTOM BETTER

| | FunDSP (`fundsp-tutti/src/delay.rs`) | Tutti (`tutti-units/src/delay.rs`) |
|---|---|---|
| Feedback | **None** — `Delay` (`delay.rs:73`) is a plain line; `Tap`/`TapLinear` (`:150`/`:390`) are read-only multitaps | **Feedback** + wet/dry (`DelayLineNode`, `delay.rs:105`; `feedback` atomic `:108/:144`) |
| Interpolation | fixed per type (`Tap` cubic, `TapLinear` linear) | selectable None / Linear / **CubicHermite** (`InterpolationMode`, `delay.rs:10`) |
| Stereo | — | **cross-feedback ping-pong** (`StereoDelayLineNode`) |
| Time control | audio-rate input port | atomic `delay_time` |

**Verdict: CUSTOM BETTER, keep.** FunDSP has no feedback delay at all — its building blocks are read-only
delay lines. Tutti is a complete delay effect (feedback, wet/dry, cubic interp, stereo cross-feedback).

## 4. Limiter — CUSTOM BETTER

| | FunDSP `Limiter` (`fundsp-tutti/src/dynamics.rs`) | Tutti `LimiterNode` (`tutti-units/src/dynamics/limiter.rs`) |
|---|---|---|
| Lookahead | yes — hierarchic `ReduceBuffer<f32, Maximum>` on **amplitude** (`dynamics.rs:59,125,133`) | yes — `MonotonicMinDeque` on **gain** (`limiter.rs:7,16`); returns window-min gain (`:66`) |
| Stereo | per-construction | **stereo-linked** gain reduction (`LookaheadRing`, `limiter.rs:14,82`) |
| `set()` | **none** — attack/release/lookahead baked at construction | `UnitParam::Threshold`/`Ceiling`/`Release`, all settable |
| Extra | — | zero-latency `BrickwallLimiter` companion |

**Verdict: CUSTOM BETTER, keep.** Both do lookahead, but FunDSP's is unparameterizable (no `set()`) and
amplitude-domain; Tutti's is gain-domain, stereo-linked, fully settable, and ships a zero-latency brickwall
variant alongside.

## 5. Compressor / Gate — NO FUNDSP EQUIVALENT

FunDSP has **no compressor and no gate** anywhere (`dynamics.rs` contains only `Limiter`, `ReduceBuffer`,
`Declick`, metering; no `compressor()`/`gate()` opcode in the prelude). Tutti provides both:

- `Compressor` (`tutti-units/src/dynamics/compressor.rs`) — soft knee, makeup, **external sidechain**,
  runtime-configurable channel count with a single linked gain from max-abs of the sidechain (`compressor.rs:79–116`).
- `Gate` (`tutti-units/src/dynamics/gate.rs`) — threshold + **hold** (`gate.rs:15`) + range, sidechain.

**Verdict: NO FUNDSP EQUIVALENT, keep.**

## 6. Chorus / Flanger / Phaser — CUSTOM BETTER

FunDSP's `chorus()`/`flanger()`/`phaser()` (`prelude.rs:2717/2767/2791`) bake their LFO/feedback into a
closure **at construction** — no live parameters. Tutti's `ChorusNode`/`FlangerNode`/`PhaserNode`
(`tutti-units/src/modulation/*`) expose live atomic rate/depth/feedback/mix, and the phaser adds a
configurable stage count (`phaser.rs:56,66`).

**Verdict: CUSTOM BETTER, keep** — automatable params are mandatory for a DAW; FunDSP's are fixed at build.

## 7. Spatial — NO FUNDSP EQUIVALENT

FunDSP offers only basic stereo `pan`/`panner`. Tutti's `spatial/` provides VBAP multichannel panning
(stereo→Atmos 7.1.4) and ITD/ILD binaural panning. **Verdict: NO FUNDSP EQUIVALENT, keep.**

## 8. Convolution — NO usable FUNDSP EQUIVALENT

FunDSP has `convolve.rs` (`Convolver`), but it is used only internally by its reverb and is not an
IR-loading convolution-reverb effect. Tutti's `convolution/` is a partitioned IR convolver with dawai's
deferred-IR-load integration. **Verdict: keep.**

## 9. Reverb — FUNDSP ONLY (already adopted)

Tutti has **no custom reverb**; it already uses FunDSP's `reverb_stereo` (`dawai-model/.../effect/audio_graph/time.rs`),
driven by **crossfade node-replacement** because `reverb_stereo` has no `set()` (handled in
`bevy-tutti/src/graph/reconcile.rs` `reconcile_reverb_params`). FunDSP also ships `reverb2/3/4_stereo`, which
are **unused** — these are the higher-quality variants worth adding.

## 10. Distortion / waveshaping — FUNDSP ONLY (real gap)

Tutti has **no distortion effect** (verified: every `shape(`/`clip(` reference in the dawai workspace is
unrelated own code — LFO shaping in `dawai-model/src/modulation/evaluate.rs` and `channel/param_mod.rs`, and
Bevy UI `Overflow::clip()`). FunDSP's `shape.rs` provides a full set of stateless shapers behind the `Shape`
trait (`shape.rs:11`): `Clip` (`:48`), `ClipTo` (`:63`), `Tanh` (`:81`), `Atan` (`:95`), `Softsign` (`:111`),
`Crush` (`:126`), plus an RMS-normalizing `Adaptive` wrapper (`:164`), and an `oversample()` wrapper for
anti-aliased nonlinear processing. None expose `set()` (drive is a constructor field), so the dawai path is
crossfade-rebuild (Pattern A) or a thin pre-gain wrapper.

**Verdict: FUNDSP ONLY — top add candidate.**

---

## Consequence

The only items where adopting FunDSP *adds capability* are **reverb2/3/4_stereo** (better reverbs alongside
the already-used `reverb_stereo`), **distortion/waveshaping** (a real missing effect), and a **real allpass**
(`allpass_hz` = `FixedSvf<AllpassMode>` at `prelude.rs:2412`, which would replace dawai's current Notch
approximation). Everything else stays custom — see `fundsp-fork-audit.md` for which fork modules that leaves
unused.
