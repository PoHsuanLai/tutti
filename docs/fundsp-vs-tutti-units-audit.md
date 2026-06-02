# `tutti-units` vs. fundsp — duplication audit

**Context.** tutti vendors a fork of fundsp at `crates/tutti/crates/fundsp-tutti/`
(repo `github.com/PoHsuanLai/fundsp`). Several `tutti-units` DSP nodes are
hand-rolled reimplementations of primitives the fork already provides — in
some cases with *more* capability than our versions (notably audio-rate
parameter inputs). This audit catalogs each hand-rolled `*Node` in
`tutti-units`, whether fundsp already provides an equivalent, the fundsp
variant's audio-rate status, and a migration recommendation.

The motivating finding: the hand-rolled `StereoSvfFilterNode` reads cutoff/Q
from atomics once per block (control-rate only). fundsp's `Svf<F, M>`
(`fundsp-tutti/src/svf.rs:807`) takes cutoff/Q as **per-sample audio-rate
input ports** (`type Inputs = U3`/`U4`; `tick()` calls
`mode.update_inputs(input, …)` reading ports 1/2 every sample). So the
audio-rate-effect-ports work we were about to hand-build already exists in
fundsp for free.

## Why the hand-rolled versions exist (the ergonomics fundsp lacks)

The tutti `*Node` wrappers add three things over a raw fundsp `An<...>`:
1. **`Param<U>` atomic handles** — UI/automation thread writes
   `set_frequency`/`set_q` via a cloned `Arc<AtomicF32>`; the audio thread
   reads it. fundsp's `Svf` (varying) takes params only as *audio inputs*;
   fundsp's `FixedSvf` takes them only via `set_*`/`Setting` (no shared
   handle). Neither gives the "UI holds an atomic, audio reads it" pattern
   directly.
2. **A block-rate `maybe_update()` skip** — recompute coefficients only on a
   threshold crossing, so an unmodulated filter pays ~nothing per block.
3. **A uniform `AudioUnit` (dyn) object** with stable `get_id()` for the
   node registry / serialization, plus stereo wrappers with shared coeffs +
   per-channel integrator state.

Any fundsp migration must re-provide (1) and (2) — typically by wrapping the
fundsp `An<...>` in a thin tutti node that owns the `Param` atomics, feeds
them into the fundsp node (either via `set_*` for the block-rate path or via
a `Constant`/input-port for the audio-rate path), and keeps the
`maybe_update` skip.

## Catalog

| tutti-units node | fundsp equivalent | fundsp audio-rate? | recommendation |
|---|---|---|---|
| `SvfFilterNode` / `StereoSvfFilterNode` (`filter/svf.rs`) | `Svf<F,M>` (varying, `U3`/`U4` inputs) + `FixedSvf<F,M>` (fixed, `U1`) for every mode: lowpass/highpass/bandpass/notch/peak/allpass/bell/lowshelf/highshelf | **YES** — cutoff/Q (and gain for bell/shelf) are per-sample input ports | **MIGRATE.** This is the whole point. Wrap `Svf`/`FixedSvf`; the fork even shares our exact coeff math (`SvfCoefs::lowpass` etc. is identical to our `compute_svf_coeffs`). The hand-rolled SVF is a near-verbatim copy. |
| `LadderFilterNode` / `StereoLadderFilterNode` (`filter/ladder.rs`) | `moog()` → `Moog<F,U3>` (cutoff+Q inputs), `moog_hz()` → `Moog<F,U1>` (fixed) | **YES** — `Moog<F,U3>` takes cutoff+Q as audio inputs | **MIGRATE** (with a caveat). fundsp `Moog` is a Moog ladder; verify it matches our LP12/LP24/HP12/HP24 + `drive` behavior. fundsp `moog` is LP-only resonant — our HP modes + `drive` (tanh saturation) may have **no** direct fundsp equivalent and would need a wrapper or stay custom. Check before committing. |
| `EqBandNode` (`filter/eq_band.rs`) | `bell`/`lowshelf`/`highshelf` `Svf` modes | YES (same `Svf`) | **MIGRATE** alongside SVF — it's the same `Svf` family with gain. |
| `DelayLineNode` / `StereoDelayLineNode` (`delay.rs`) | `delay(t)` → `An<Delay>` (fixed), `tap`/`tap_linear` (varying delay-time input) | `tap`/`tap_linear` YES (delay time is an input); plain `delay` no | **EVALUATE.** If our delay needs feedback + wet/dry + tempo-sync, that's composition (`delay >> ...` + `feedback`), not a single fundsp node. Likely keep a thin wrapper but build it *from* fundsp `delay`/`tap` instead of a hand-rolled ring buffer. |
| `LimiterNode` (`dynamics/limiter.rs`) | `limiter(attack, release)` → `Limiter<U1>`, `limiter_stereo` → `Limiter<U2>` | lookahead limiter; params are construction-time | **EVALUATE.** fundsp `Limiter` is a real lookahead limiter. If ours matches the semantics, migrate; if ours has ceiling/threshold knobs fundsp's lacks, wrap. |
| `dynamics/compressor.rs`, `gate.rs` | **none** — fundsp has no compressor or gate | n/a | **KEEP.** Genuinely custom; fundsp doesn't provide these. (fundsp has `limiter` only.) |
| `ChorusNode` (`modulation/chorus.rs`) | `chorus(seed, separation, variation, mod_freq)` | construction-time params | **EVALUATE.** fundsp `chorus` exists; compare param surface. |
| `FlangerNode` (`modulation/flanger.rs`) | `flanger(fb, min_delay, max_delay, f)` (closure-driven) | the `f` closure drives delay per-sample | **EVALUATE.** fundsp `flanger` takes a closure for the LFO; ours likely has explicit rate/depth knobs. Wrap or keep. |
| `PhaserNode` / `StereoPhaserNode` (`modulation/phaser.rs`) | `phaser(fb, f)` (closure-driven allpass phaser) | closure-driven | **EVALUATE.** Same shape as flanger. |
| `modulation/modulated_delay.rs` | `tap`/`tap_linear` (varying delay) | YES | **EVALUATE** — could be `tap_linear` under the hood. |
| `ConvolverNode` / `StereoConvolverNode` (`convolution/`) | fundsp has FFT (`fft.rs`) + `resynth` but **no partitioned convolver node** | n/a | **KEEP.** Partitioned/streaming IR convolution is custom; fundsp doesn't ship a ready convolver node. |
| `LfoNode` (`lfo.rs`) | `lfo`/`lfo2`/`envelope` (closure-driven), `sine`/`saw`/etc. | the LFO is a closure of time | **KEEP / EVALUATE.** Ours is `TransportReader`-driven (beat-synced, tempo-aware) — fundsp's `lfo` is a free-running closure of seconds. The transport integration is the custom value; keep, but its *shape* generators could call fundsp oscillators. |
| `SpatialPannerNode` / `BinauralPannerNode` / VBAP (`spatial/`) | `pan(p)` / `panner()` (stereo constant-power pan only) | pan position is an input for `panner` | **KEEP.** Binaural (HRTF) + VBAP are far beyond fundsp's stereo `pan`. Custom. |

## Genuinely-custom (no fundsp equivalent — keep)
- `compressor`, `gate` (dynamics)
- `Convolver` (partitioned IR)
- Binaural / VBAP spatial panners
- `LfoNode`'s transport-synced driving (the generator shapes could reuse fundsp)

## Clear wins (migrate)
- **SVF family** (`svf.rs` + `eq_band.rs`) → fundsp `Svf`/`FixedSvf`. Audio-rate
  cutoff/Q/gain for free; the coeff math is already identical in the fork.
- **Ladder/Moog** → fundsp `Moog` — *if* HP modes + drive can be matched or
  are acceptable to drop/wrap.

## Recommended migration shape (for the SVF win)
A `tutti-units` node that:
- owns `frequency`/`q`/`gain` as `Param<...>` (keeps the UI handle API + the
  `node_mut` setter path other code relies on),
- holds **two** fundsp sub-nodes or one switchable: a `FixedSvf` for the
  block-rate fast path (no mod) driven by `set_cutoff_q`, OR — when a mod
  port is requested — the varying `Svf` with cutoff/Q fed from input ports,
- exposes the same `with_param_inputs(..., mod_cutoff, mod_q)` + `*_port()`
  API the strip established, so the router materializer wiring is identical
  to the strip VCA splice,
- preserves `get_id()` stability for serialization.

This gets audio-rate filter modulation "for real" via fundsp's proven DSP
instead of hand-maintained coeff code, while keeping tutti's atomic-handle
ergonomics.

## Status
- Agents' hand-rolled SVF/ladder audio-rate ports: **discarded** (reverted in
  the tutti submodule) in favor of this fundsp pivot.
- This audit is the decision input; no migration committed yet.
