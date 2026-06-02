# FunDSP fork — consumer / dead-code audit

**Purpose.** Map which modules of the vendored FunDSP fork (`crates/tutti/crates/fundsp-tutti/`, package
`fundsp-tutti`) are actually reachable from the dawai workspace, so we can later decide what to feature-gate
or move out. **Report-only — no deletions in this pass.**

**Method.** A "consumer" is any `.rs` file **outside the fork** (the fork includes its own
`tests/`/`examples/`/`benches/`, which are *not* consumers). Most crates reach FunDSP via
`tutti_core::dsp::*` (= `pub use fundsp::prelude::*`, `tutti-core/src/lib.rs:81`) plus the named re-exports
at `tutti-core/src/lib.rs:84-106`. Each "APPEARS-UNUSED" verdict below was confirmed by grepping the whole
workspace for the module's hallmark symbols and excluding (a) in-fork files and (b) unrelated name
collisions (dawai's own "DrumSequencer" UI, `dawai-spectral`'s `resynth` system, `Overflow::clip()`, the
"Resample Quality" UI string).

**Classification.** USED-DIRECTLY (a consumer names a symbol) · TRANSITIVE-ONLY (no direct consumer, but
pulled in by a USED opcode such as `reverb_stereo`, or by the graph runtime) · APPEARS-UNUSED (no consumer,
not reachable from a used path → archive candidate).

---

## Module table

| Module | Class | Evidence (consumer `file:line` / dependent / "none") |
|---|---|---|
| `audionode` | USED-DIRECTLY | `tutti-core/src/transport/click.rs` (`AudioNode`, `Frame`, typenum) |
| `audiounit` | USED-DIRECTLY | `tutti-core/src/processor.rs`; 27 `impl AudioUnit` in `tutti-units` |
| `net` | USED-DIRECTLY | `tutti-core/src/graph.rs` (`Net`, `NodeId`, `Source`); re-export `lib.rs:90` |
| `realnet` | USED-DIRECTLY | `tutti-core/src/graph.rs`/`processor.rs` (`NetBackend`); re-export `lib.rs:100` |
| `buffer` | USED-DIRECTLY | `BufferVec` re-export `lib.rs:87`; `BufferRef`/`BufferMut` in every unit |
| `setting` | USED-DIRECTLY | `Setting` re-export `lib.rs:102`; `unit_param.rs` (`Address`/`Parameter`) |
| `shared` | USED-DIRECTLY | `shared`/`Shared` re-export `lib.rs:92-93`; synth voice |
| `signal` | USED-DIRECTLY | `Signal`/`SignalFrame` re-export `lib.rs:103`; PDC `mono_delay.rs` |
| `params` | USED-DIRECTLY | `SampleRate` re-export `lib.rs:79` |
| `math` | USED-DIRECTLY | `Complex32` re-export `lib.rs` → sampler phase-vocoder; trait base for everything |
| `fft` | USED-DIRECTLY | `real_fft`/`inverse_fft` re-export `lib.rs:88` → `tutti-sampler/.../phase_vocoder.rs` |
| `prelude` | USED-DIRECTLY | `pub use fundsp::prelude::*` `lib.rs:81` (bulk opcode surface) |
| `wave` | USED-DIRECTLY | `Wave` re-export `lib.rs:104`; sampler/export |
| `read` | USED-DIRECTLY | `WaveAsset` re-export `lib.rs:99` → `dawai-model/src/clip/audio.rs`, `bevy-tutti` (heavily) |
| `resample` | USED-DIRECTLY | `wave.resample_fir(.., Quality::High)` in `dawai-extension-runtime/.../project.rs:586`; `tutti-export/src/graph.rs:153` |
| `moog` | USED-DIRECTLY | `moog()` in dawai filter builder |
| `reverb` | USED-DIRECTLY | `reverb_stereo()` in `dawai-model/.../effect/audio_graph/time.rs`, `bevy-tutti` reconcile |
| `biquad` | TRANSITIVE-ONLY | `LinkwitzRiley*` re-exported (`lib.rs:84`) but **no consumer** (see dead re-exports); biquad types pulled by prelude opcodes |
| `svf` | TRANSITIVE-ONLY | reachable via prelude (`lowpass` etc.) + `allpass_hz`; used internally by reverb/opcodes |
| `delay` | TRANSITIVE-ONLY | internal dep of `reverb_stereo`; prelude `delay`/`tap` |
| `filter` | TRANSITIVE-ONLY | one-pole filters pulled by reverb/opcodes |
| `feedback` | TRANSITIVE-ONLY | internal dep of `reverb_stereo`/`fdn` |
| `shape` | TRANSITIVE-ONLY | reachable via prelude; **no consumer yet** — the distortion add (see comparison doc) will make it USED-DIRECTLY |
| `combinator` | TRANSITIVE-ONLY | `An<X>` operators; used by synth voice graph |
| `oscillator` | TRANSITIVE-ONLY | `sine`/`saw`/`triangle`/`poly_pulse` used by synth voice (via prelude) |
| `noise` | TRANSITIVE-ONLY | prelude only |
| `pan` | TRANSITIVE-ONLY | prelude only |
| `envelope` | TRANSITIVE-ONLY | `adsr_live` used by synth voice (via prelude) |
| `dynamics` | TRANSITIVE-ONLY | `Limiter`/metering reachable via prelude; **no consumer** (Tutti uses its own limiter) |
| `convolve` | TRANSITIVE-ONLY | internal to reverb path; feature-gated (`fft`); no direct consumer |
| `slot`, `vertex`, `graph`, `ring`, `system`, `sound`, `denormal` | TRANSITIVE-ONLY | internal graph/runtime plumbing for `net`/`audiounit` |
| `adsr` | TRANSITIVE-ONLY | `adsr_live` via prelude (synth voice) |
| **`sequencer`** | **APPEARS-UNUSED** | only the dead re-export `lib.rs:101` + a fork example; **no production consumer** |
| **`realseq`** | **APPEARS-UNUSED** | backend of `sequencer`; none |
| **`granular`** | **APPEARS-UNUSED** | none (sampler's own `time_stretch/granular.rs` is unrelated) |
| **`resynth`** | **APPEARS-UNUSED** | none (`dawai-spectral`'s `resynth` system is unrelated) |
| **`wavetable`** | **APPEARS-UNUSED** | none |
| **`oversample`** | **APPEARS-UNUSED** | none (candidate dep if the anti-aliased Saturator is added) |
| **`generate`** | **APPEARS-UNUSED** | none |
| **`rez`** | **APPEARS-UNUSED** | none |
| **`fir`** | **APPEARS-UNUSED** | none |
| **`follow`** | **APPEARS-UNUSED** | none |
| **`biquad_bank`** | **APPEARS-UNUSED** | none |
| **`snoop`** | **APPEARS-UNUSED** | none |
| **`peak_builder`** | **APPEARS-UNUSED** | none |
| **`write`** | **APPEARS-UNUSED** | none (dawai's own `write_waveform_payload`/`write_wav_*` are unrelated) |
| **`prelude32` / `prelude64`** | **APPEARS-UNUSED** | consumers use the generic `prelude`; only fork tests/examples import these |

---

## Dead re-exports in `tutti-core/src/lib.rs` — **removed**

These re-export names had **zero downstream consumers** (verified across all paths: `tutti_core::X`, the
`tutti` facade, and the `tutti_core::dsp::*` prelude glob) and have been removed:

- `EventId`, `ReplayMode`, `Sequencer` from the `fundsp::sequencer` re-export.
- `LinkwitzRileyCrossover`, `LinkwitzRileyHighpass`, `LinkwitzRileyLowpass`, `LrOrder` (whole
  `fundsp::biquad` re-export line), plus the `lr_crossover*`/`lr_lowpass*`/`lr_highpass*` prelude names.

**Kept (corrects an earlier draft that listed all four sequencer names as dead):**
- `Fade` — **load-bearing.** `tutti::Fade` (facade) resolves to `tutti_core::Fade`, which comes *only*
  from this `fundsp::sequencer` re-export (the prelude glob does not export `Fade`). It is used by
  `TuttiGraph::crossfade_boxed` / `bevy-tutti reconcile.rs` for the reverb + distortion node-rebuild
  crossfade. Removing it would break that path.
- `WaveAsset` (`lib.rs`) — used by `dawai-model/src/clip/audio.rs`, `bevy-tutti`.

Note: the mono `SvfFilterNode` is **not** dead (an earlier note speculated it might be) — it backs
`EqBandNode` (`tutti-units/src/filter/eq_band.rs`) and has RT-no-alloc tests. Kept.

---

## Largest self-contained archive candidates

Confirmed-unused modules total **~9,900 LOC**. The biggest, most self-contained subsystems (those that would
shrink the fork most and have the fewest internal entanglements with the used graph runtime):

| Subsystem | Notes |
|---|---|
| `sequencer.rs` + `realseq.rs` | event scheduler; types re-exported but never consumed |
| `resynth.rs` | spectral phase-vocoder resynthesis |
| `granular.rs` + `generate.rs` | granular synthesis + procedural node generation (`generate` depends on `granular`) |
| `wavetable.rs` | band-limited wavetable synthesis |
| `prelude32.rs` / `prelude64.rs` | f32/f64-specialized prelude mirrors (consumers use generic `prelude`) |
| `oversample.rs` | **hold** — likely dep of a future anti-aliased Saturator distortion |
| `rez.rs`, `fir.rs`, `follow.rs`, `biquad_bank.rs`, `snoop.rs`, `peak_builder.rs`, `write.rs` | small leaf modules |

---

## Recommendation (deferred — not this pass)

1. **First, cheap & non-breaking:** delete the dead re-export lines above from `tutti-core/src/lib.rs`.
2. **Later:** feature-gate the APPEARS-UNUSED subsystems behind cargo features (off by default) so they
   compile out but stay available, **or** move them to an optional sibling crate. Re-run this audit's grep
   pass immediately before any removal — the distortion add will flip `shape` (and possibly `oversample`) to
   USED-DIRECTLY, and any new synth work could revive `wavetable`/oscillator modules.
3. Treat `resample` as **USED** (it backs `Wave::resample_fir`, called from the export/extension paths).
