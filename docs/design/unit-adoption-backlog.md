# Unit-type adoption — remaining backlog

Status: **§1, §2 (partly), §4 (partly) and §5 landed on `fix/unit-tier4`**;
the rest still open. Auditor: Claude, 2026-07-31, against `cb3d8185`.

Landed so far — see the commits for the measurements behind each:

- §1a/§1b — the two zero-tick-unit guards (`ZeroDctpq`, SMF `parse()`).
- §1c — pitch bend unified on the multiplicative derivation. The direction was
  the opposite of what this document first assumed: three of the four sites
  bend an interpolated frequency mid-glide and have no note number to look up,
  so the table path could not generalize.
- §2 — `tick_mtc`, `FilterModConfig`, `ModulatedDelayConfig`, `DelayLineNode`'s
  `fb`, `Portamento`'s three endpoints, `Resampler::new`.
- §4 — `shape.rs`'s Sine arm, `note.rs`'s pitch-ratio log and `A4_HZ`,
  portamento's glide-interval log.
- §5 — both missing inverses.

Still open: the rest of §3 (the plugin `SampleRate` chain, `PluginRequest`,
`DiskVoiceConfig`, `PitchResult`), the remaining §4 items (`click.rs`,
`handle.rs`, the SMF BPM↔µs pair), and the `SmfNote`/`ClipNote` beat fields —
those last are blocked, see §9.

The engine-wide sweep after tiers 1–3 (PRs #104, #107). Every claim below was
verified by reading the code at the cited line; the two bugs were reproduced by
measurement, not inference. Findings the audit *rejected* are recorded in §6 —
they cost real time to disprove and shouldn't be re-derived.

Scope: the tutti engine workspace only. The app workspace (`dawai-*`) does not
build on `main`, so its call sites can neither validate nor invalidate this
work; see `fix/app-side-ecs-migration`.

---

## 1. Two latent bugs — fix first, independent of typing

### 1a. A DCTPQ=0 clip file parses, and every event lands at infinity

`crates/tutti/crates/midi/tutti-midi-types/src/clip_file.rs:402` reads the
tick-unit declaration as `ticks_per_quarter = Some((word0 & 0xFFFF) as u16)`,
and `:446` validates only its *presence*:

```rust
let ticks_per_quarter = ticks_per_quarter.ok_or(ClipFileError::MissingDctpq)?;
```

Both `timed()` (`:206`) and `duration_beats()` (`:239`) then divide by it.

**Reproduced** with the crate's own writer (`write_clip_file(0, ..)`, which does
not validate either):

```
PARSED OK. tpq=0
timed() beats = [inf]
duration_beats() = inf
```

The SMF path rejects the identical condition —
`tutti-midi-io/src/smf.rs:172-174` returns `Error::MidiFileParse("zero ticks-per-beat")`.
This is its MIDI-2 twin, unguarded.

Fix: a `ClipFileError::ZeroDctpq` (or fold into `MissingDctpq` — a zero tick
unit *is* a missing declaration) rejected at `read_clip_file`.

### 1b. The same SMF header field is guarded on one path and not the other

Within `tutti-midi-io/src/smf.rs`, two functions read `smf.header.timing`:

- `tracks()` at `:168` — guards zero, returns `MidiUnsupportedTiming` / `MidiFileParse`
- `ParsedMidiFile::parse()` at `:46` — no zero check; `parse_track` at `:105`
  then computes `tick as f64 / f64::from(ticks_per_beat)`

Same file, same field, diverged guards. This is the `cycles_of` / `offset_of`
pattern from #103 and #107 for the third time.

### 1c. Pitch bend bends a held note differently from a new one

`crates/tutti/crates/dsp/tutti-synth/src/polysynth.rs`. Two incompatible
derivations of one wheel position:

| Path | Line | Derivation |
|---|---|---|
| `handle_note_on` | 406 | `base_freq * (range * bend).to_pitch_ratio()` |
| glide / retrigger | 699, 801 | same multiplicative form |
| `apply_pitch_bend` (held notes) | 584–591 | `tuning.fractional_note_to_freq(note + bend_semitones)` |

The multiplicative path applies `2^(semitones/12)` to the base frequency. The
additive path goes through `Tuning::fractional_note_to_freq`
(`tuning.rs:219-234`), which clamps at note ≤ 0 / ≥ 127 and interpolates in log
space **between adjacent table entries**. Under an unevenly-spaced table these
are not the same number.

**Measured in the synth** on `Tuning::just_intonation`, note 60, full up-bend:

```
held Hz(297.00003) vs struck Hz(296.33002)   — 3.9 cents apart
```

(An earlier estimate in this document said 11.7 cents. That came from a
standalone model using a different just-intonation table than the one
`Tuning::just_intonation` actually ships; 3.9 cents is the real figure, taken
from the synth. The defect was real either way, the magnitude was not.)

Audible, and invisible under equal temperament — which is why it survived:
every default-tuning test agrees with both derivations.

Note the typing tell: `:584` hand-rolls the multiply as bare floats
(`self.pitch_bend * range.get()`) while the other three use the typed
`Semitones * f32 -> Semitones` operator. The divergence is visible in the types.

Fix is a design decision, not a rename: pick one derivation. The table path is
the correct one under a custom tuning, so the note-on sites should likely move
to it — but that changes equal-temperament output by 0 and custom-tuning output
audibly, so it wants a test pinning both.

---

## 2. Transposable adjacent parameters — the highest-severity typing class

Ordered by blast radius. Each is two-or-more same-typed neighbours of
*different* units, where a swap compiles clean.

| Site | Signature / fields | Units |
|---|---|---|
| `tutti-midi-runtime/src/clock_master.rs:236` | `tick_mtc(block_size: usize, beats_per_sample: f64, beat: f64, max_offset: u32)` | `BeatDuration`, `Beat` |
| `tutti-export/src/process/resample.rs:140` | `Resampler::new(channels, source_rate: u32, target_rate: u32, chunk)` | `SampleRate` ×2 |
| `tutti-synth/src/synth.rs:138-150` | `FilterModConfig { mod_wheel_depth, velocity_depth, lfo_rate, lfo_depth }` — four `pub f32` | `Depth`, `Depth`, `Hz`, `Depth` |
| `tutti-units/src/modulation/modulated_delay.rs:16-36` | `ModulatedDelayConfig { base_delay_secs, max_delay_secs, lr_phase_offset }` | `Seconds` ×2, `PhaseIncrement` |
| `tutti-midi-io/src/smf.rs:147,149` | `SmfNote { start_beats, duration_beats }` | `Beat`, `BeatDuration` |
| `tutti-midi-types/src/clip_file.rs:314,316` | `ClipNote { start_beats, duration_beats }` | `Beat`, `BeatDuration` |
| `tutti-midi-io/src/smf.rs:88` | `get_events_in_range(start_beats: f64, end_beats: f64)` | `Beat` ×2 |
| `tutti-synth/src/portamento.rs:49-58` | `start_freq`, `target_freq`, `current_freq` — three private `f32` | `Hz` ×3 |
| `tutti-units/src/spatial/hrtf_panner.rs:353` | `direction_from_degrees(azimuth_deg: f32, elevation_deg: f32)` | `Azimuth`, `Elevation` |
| `tutti-units/src/delay.rs:175` | `process_sample(input: f32, delay_samples: f32, fb: f32, mix: Mix)` | `Feedback` — note `mix` is *already* typed in the same signature |

`Resampler::new` looked sharpest — **both call sites already hold `SampleRate`**
(`encode/mod.rs:183`, `resample.rs:392`) and narrowed at the call — but typing
it does **not** close the transposition, and the fix's own comment now says so.
Both parameters become `SampleRate`, i.e. the *same* type, so a swap still
compiles (verified). Only a `SourceRate`/`TargetRate` split would catch it, and
two names for one behaviour is not a type. What it buys is one derivation of
the ratio from values that never lost precision.

`tick_mtc` is the direct analogue of the `BeatWindow` fix in #107 — and both
types are in scope one line earlier (`:190` calls the canonical
`beats_per_sample`, which *returns* `BeatDuration`, then `.get()`s it away).

---

## 3. Typed at the boundary, flattened in the middle

The dominant pattern. A typed value arrives, gets `.get()`-stripped immediately,
travels bare, and is re-wrapped at the far end. Same shape as #108's AU latency.

- **`SampleRate` through the plugin load chain** — `audio_unit.rs:24-27` unwraps
  at the first host-side line, then bare through ~10 hops:
  `host/builder.rs:34,40,46,55,68,108,125` → `host/plugins.rs:314,332,341,427` →
  `host/node/mod.rs:93,254,268` → `harmony_source.rs:61`,
  `transport_source.rs:53,64`, `subprocess/launch.rs:52,113`. Only the two ends
  (fundsp's `AudioUnit`, the IPC wire) are real boundaries. Fix is
  `impl Into<SampleRate>` on the constructors — source-compatible, since
  `unit_newtype!` generates `From<f64>`.
- **`NoteExpressionSource::sample_rate: f64`**
  (`host/node/note_expression_source.rs:36,40`) — crosses nothing at all. Its
  sibling `ParamAutomationSource` (`:336,349`) already stores `SampleRate` and
  takes `impl Into<SampleRate>`. Two of four per-block sources are typed.
- **`PluginRequest.sample_rate: f64`** (`bevy-tutti/src/plugin_host/load.rs:96`)
  — a Bevy `Component` whose **own doc comment** says to read it off
  `AudioConfig`, where the field *is* `SampleRate`
  (`bevy-tutti/src/graph/resources.rs:25`). The `Default` at `:108` uses
  `48_000.0` where `SampleRate::SR_48K` exists.
- **`ClockMaster`'s internals** — stores `sample_rate: SampleRate` and reads
  `Beat`/`Bpm` through the `Timeline` trait, then `.get()`s all three at
  `:156,183,190` and threads bare f64s through five functions.
- **`DiskVoiceConfig.file_sample_rate: f64`**
  (`tutti-sampler/src/voice/disk_voice.rs:576,600,703`) — produced from typed
  values at `tutti-sampler/src/ports.rs:219`, re-wrapped at `disk_voice.rs:738`. Bare only across
  the config hop, in a file that spends 15 lines of prose explaining the value
  must not have `src_ratio` applied twice.
- **`Portamento`** — entire public surface typed (`set_target(impl Into<Hz>)`,
  `tick() -> Hz`); the three private fields are bare.
- **`PitchResult { frequency: f32, confidence: f32 }`**
  (`tutti-analysis/src/pitch.rs:27-32`) — `Hz` and `Confidence` are imported *in
  the same file* (`:40`) and used by the detector; only the return struct is
  bare. The sibling `yin.rs:255-264` exists purely to re-wrap this crate's own
  output.

---

## 4. Hand-rolled conversions duplicating a named converter

- `tutti-core/src/transport/handle.rs:69-75` — `beats_per_second()` /
  `samples_per_beat()`. A third spelling of the `/60.0` relation alongside
  `transport::beats_per_sample` and `BeatDuration::to_seconds`, both returning
  bare `f64`, neither guarded. **No production callers** — the only uses are
  `samples_per_beat`'s own call to `beats_per_second` and two tests in the same
  file (`:160,163`). Dead public API, so this is a deletion candidate as much as
  a typing one. That also caps the severity of the missing zero-guard: nothing
  reaches it today.
- `tutti-mod/src/shape.rs:61` — `(phase * TAU).sin()` where
  `Phase::to_radians()` exists and is documented "for the `sin`/`cos` call".
  The other match arms legitimately use the raw phase; only the Sine arm
  duplicates.
- `tutti-types/src/value/note.rs:230` — `12.0 * (freq / A4_HZ).log2()`. This is
  the inverse of `frequency()` (`:219`), which correctly uses
  `Semitones::to_pitch_ratio()`. **See §5** — the named inverse does not exist.
- `tutti-types/src/value/note.rs:233` — `(steps - nearest) * 100.0` where
  `Semitones::to_cents()` exists; `:247` `const A4_HZ: f32 = 440.0` is an `Hz`.
- `tutti-core/src/transport/click.rs:217-238` — `generate_click` carries five
  untyped roster quantities: `click_duration = 0.03` (`Seconds`, and
  `sample_rate * duration as usize` re-implements `to_samples_ceil` — the
  truncation under-allocates a frame), `freq` (`Hz`), `accent_volume`
  (`Amplitude`), and `2.0 * PI * freq * t` (the `PhaseIncrement`/`Radians`
  path). Self-contained, so medium severity despite the count.
- `tutti-midi-io/src/smf.rs:117` and `:328` — BPM ↔ µs-per-quarter, both
  directions, both unguarded. The MIDI-2 twin
  (`ump/flex_data.rs:86-98`) guards both directions and uses a named constant.
  Three copies of one relation; one is unprotected.
- `tutti-export/src/process/resample.rs:125,157` — `ratio: f64` derived by hand
  as `target/source`. `SrcRatio` is defined as *source ÷ dest* — the reciprocal
  — and its doc says `for_rates` is "the one place the derivation lives". Not a
  drop-in (`SrcRatio` is f32, this is a frame budget), but the inverted
  same-named ratio is its own hazard.
- `bevy-tutti/src/modulation/driver.rs:269-278` — samples→seconds by hand;
  both endpoints typed, only the middle bare. **Blocked on §5.**

---

## 5. Missing inverses — the omission rule applied to our own work

CLAUDE.md: *"An omission must ship with its replacement."* Two converters have
one direction only, and in both cases a call site has already escaped to raw
float — the exact `Degrees` failure the rule was written for.

- **`Semitones::from_pitch_ratio`** does not exist. `Cents::from_pitch_ratio`
  was added in #107 (`units.rs:1393`) and `Semitones::to_pitch_ratio` exists
  (`:1415`), but the semitone inverse was never written — which is why
  `note.rs:230` hand-rolls `12.0 * log2(..)`. This gap is ours, from #107.
- **`Samples::to_seconds(SampleRate)`** does not exist, though
  `Seconds::to_samples` does. `bevy-tutti/src/modulation/driver.rs:273` is the
  escaped call site. (`Timeline::steady_time()` returns `i64`, not `Samples`,
  so this one needs the trait looked at too.)

Write these before the call sites that need them, not after.

---

## 6. Verified NOT findings — do not re-derive

Each of these looked like a finding and is not. Recorded because disproving
them cost real time.

- **`transport_source.rs:90`'s `if tempo > 0.0` is not a diverged NaN guard.**
  All three format hosts gate independently before setting the valid flag —
  `vst3/types/transport.rs:121`, `clap/instance/audio.rs:542`,
  `vst2/time_info.rs:68`, each `is_usable(t) && t > 0.0`. A NaN never reaches a
  plugin. This is the shared guard working as designed. The duplicated
  `beats * 60.0 / tempo` is still a §4 nit, but it is not a bug.
- **`clock_master.rs:219`'s unguarded division is safe.** `:185` early-returns
  on `tempo_bpm <= 0.0`, covering the whole function. The second guard at `:252`
  is belt-and-braces, not evidence of divergence.
- **`Transport::samples_per_beat`'s divide-by-zero is unreachable in practice**
  — the function has no callers anywhere in the repo (every `samples_per_beat`
  grep hit is a local test variable). Listed in §4 as dead API, not as a bug.
- **MTC's `secs_per_beat` must NOT be routed through `BeatDuration::to_seconds`.**
  That method's own doc (`units.rs:1837-1846`) warns the
  `(tempo/60)/sample_rate` association is load-bearing and that re-associating
  it silently breaks the offline/live sample-for-sample pinning. An f32
  precision objection was also considered and **measured at ~0.001 frames** —
  it does not hold; the association argument is the one that does.
- **`ModParamRange::with(param, base, min, max)`** and mirrors
  (`bevy-tutti/src/modulation/components.rs:291`, `host/node/mod.rs:550`,
  `param_automation_source.rs:81,242`) are three adjacent same-typed `f32` —
  and are correct. The values are in *the target param's own units* (Hz for a
  cutoff, linear gain for a fader), documented at `components.rs:277-279`. No
  single newtype can cover them. The transposability risk is real and
  unmitigated, but it is a documented decision.
- **MIDI wire values are correctly bare.** `velocity: u16`, channel nibbles,
  UMP fields, ticks-per-quarter, 7-bit/14-bit CC — protocol integers, not
  measurements. The MIDI author was disciplined about this; the gap is
  musical-time types only.
- **`M3DB`** (`tutti-types/src/downmix.rs:34`) is an `Amplitude` held bare, but
  every use is inside a vectorizing sample loop where the project's own
  inner-loop rule says index raw. Cosmetic at most.

---

## 7. Verified clean

`tutti-mod` (all 15 files — the strongest crate audited), `tutti-cpal`,
`tutti-io`, `tutti-core/metering`, `tutti-analysis` except `pitch.rs`,
`tutti-sampler/stretch` (`intake_rate` vs `input_rate` explicitly documented as
two quantities that must not share a type), `bevy-tutti/graph/param.rs` (the
model: the unit is a type parameter), `bevy-tutti/export`, `bevy-tutti/latency`,
all four plugin format hosts (correctly bare — ABI), the IPC protocol
(settled in #105/#108), and every MIDI file outside the four named above —
`registry`, `endpoint`, `mpe_ingest`, `pre_block`, `port`, `snapshot_reader`,
`routing_table`, both sysex reassemblers, `capability_inquiry`, and
`jr_timestamp` (fully typed; the model for the rest of MIDI).

---

## 8. Why MIDI is the outlier

Worth recording, because the obvious explanation is wrong.

`tutti-midi-*` landed **2026-07-20**; `units.rs` has existed since
**2026-06-05**. MIDI did not predate the unit vocabulary — it was written
alongside it without adopting it. Only `tutti-midi-types` even depends on
`tutti-types`, and before #107 the runtime's only uses were `RtPublish`/`RtRef`.

The discipline is real where the author applied it: wire values are correctly
left as integers, `ClockMaster` holds a `SampleRate`, `jr_timestamp` is fully
typed, and the `Timeline` trait it reads returns `Beat`/`Bpm`. What is missing
is specifically the musical-time vocabulary in the middle of functions.

---

## 9. Blocked on the app workspace

`SmfNote { start_beats, duration_beats }` (`tutti-midi-io/src/smf.rs:151,153`)
and `ClipNote`'s identical pair (`tutti-midi-types/src/clip_file.rs:314,316`)
are the same position-and-span-as-two-`f64`s hazard as everything in §2, and
they are **not** fixed.

`dawai-frontend/src/project/import.rs:205` reads those fields directly, across
the workspace boundary, and that crate does not build on `main` (`dawai-model`
alone has ~98 errors from a `tutti_core::ecs` that no longer exists). Typing
the fields means shipping a break into a crate that can neither confirm nor
deny it. The `SmfNote` doc comment's "engine-neutral `u8`/`f64`" rationale is
*not* one of CLAUDE.md's carve-outs — this is a plain in-tree Rust caller — so
this is a scheduling constraint, not a design decision.

Do these with, or after, `fix/app-side-ecs-migration`.

Also still open and unblocked, but not attempted here:

- The §3 plugin `SampleRate` chain (~10 hops) — one coherent commit, largest
  mechanical diff in the backlog.
- `PluginRequest.sample_rate`, `DiskVoiceConfig.file_sample_rate`,
  `PitchResult`, `set_session_sample_rate`, `read_stereo_frame`.
- `click.rs`'s five untyped quantities; `handle.rs`'s dead `samples_per_beat`
  (a deletion candidate as much as a typing one); the SMF BPM↔µs pair, whose
  MIDI-2 twin is already guarded.

## Suggested order

1. **§1 bugs** — DCTPQ guard, SMF `parse()` guard, pitch-bend unification.
   Independent of typing; each wants a test that fails against current `main`.
2. **§5 missing inverses** — they unblock §4 sites and are ours to fix.
3. **§2 transposable pairs** — highest typing value; `tick_mtc` and
   `Resampler::new` first.
4. **§3 boundary flattening** — largest diff, most mechanical. The plugin
   `SampleRate` chain is one coherent commit.
5. **§4 remaining hand-rolled conversions.**

Tiers 1–3 shipped as one PR each with per-commit verification; same shape here.
