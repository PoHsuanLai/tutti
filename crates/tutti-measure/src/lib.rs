#![doc = include_str!("../README.md")]
//! Audio measurement for the Tutti audio engine over `&[f32]`: waveform blocks,
//! onsets, pitch, EBU R128 loudness, stereo correlation, spectra, spectral shape,
//! silence regions, vectorscope points, tempo, beats, key, chords and the short-time
//! Fourier transform. These are measurements, not processors: they read audio and
//! return numbers. The processors (filters, dynamics, convolution, delay, reverb) and
//! the CPU-independent FFT these run on are in `tutti-dsp`.
//!
//! Use it for offline or background analysis of audio you already hold as
//! samples: drawing a waveform, finding transients, tracking pitch, measuring
//! loudness or checking a stereo image. The algorithms have no opinion about
//! where the results go:
//!
//! - `summarize` — min/max/RMS waveform blocks for a timeline, **per channel**.
//!   Folding to one series is `PeakBlocks::to_mono`, a caller's choice rather than
//!   this crate's default.
//! - `detect_onsets` — onset detection over four selectable detection functions.
//! - `yin` — monophonic pitch estimation (de Cheveigné & Kawahara, 2002).
//! - `correlate` — inter-channel phase correlation and stereo image.
//! - `measure_loudness` — EBU R128 integrated loudness, loudness range and true
//!   peak.
//! - `step_windowed_loudness` — momentary (400 ms) and short-term (3 s) loudness and
//!   true peak of a running stream, after every chunk.
//! - `step_spectrum` — a streaming spectrum: power in dB per bin, or per
//!   third-octave or log band, with smoothing, peak hold and a rolling
//!   spectrogram.
//! - `spectral_features` — centroid, flatness and rolloff of every STFT frame.
//! - `strip_silence` — the sample ranges that hold signal, with a minimum silence
//!   length and padding.
//! - `vectorscope_points` (`step_vectorscope` for blocks) — a stereo pair as
//!   (side, mid) points for a goniometer.
//! - `cross_correlate` — FFT cross-correlation of two buffers at every lag.
//! - `align` — GCC-PHAT delay between two takes: lag, sub-sample offset, confidence.
//! - `estimate_tempo` / `track_beats` — tempo (BPM, confidence, octave alternatives) by
//!   autocorrelation under a log-Gaussian prior, and beat positions by dynamic
//!   programming (Ellis 2007); `_from_novelty` forms take your own detection curve.
//! - `transcribe` — monophonic audio to notes: a pitch (`Note` plus a cents bend),
//!   a span in samples and a level, split on onsets and pitch changes;
//!   `notes_from_track` takes your own pitch track.
//! - `chroma` / `step_chroma` — harmonic pitch class profile (Gomez 2006) of spectral
//!   peaks, with an estimated tuning in cents; `estimate_key` — Krumhansl-Schmuckler
//!   key with a confidence and a runner-up; `detect_chords` — major and minor
//!   triads smoothed by Viterbi, as spans in samples.
//! - `stft` / `istft_transform` — the short-time Fourier transform, in three result
//!   types so invertibility is a compile-time question.
//!
//! ## Scope
//!
//! - **No graph, no framework, no ECS.** Nothing here is a graph node or knows
//!   about one.
//! - **No threading and no scheduling.** "Live" is a property of a call site, never
//!   of an algorithm, so nothing here is named for it. A host that wants these
//!   results on a background thread owns that plumbing itself.
//! - **No display.** These return numbers, not pixels.
//!
//! ## Configs, and carries where they are needed
//!
//! Every algorithm takes a **config**: immutable, validated once, so an invalid
//! combination fails at construction rather than silently producing nothing.
//!
//! Six of them additionally need a **carry** between frames — onset detection
//! diffs against the previous spectrum, waveform blocking holds a partial block,
//! strip silence holds the region it is growing, chroma a running mean, windowed
//! loudness its meter, and the spectrum its ring of samples and its readings.
//! Those expose the carry as an explicit value and a `step` function, and their
//! batch entry points (`detect_onsets`, `summarize`, `strip_silence`) *fold that
//! same step*, so the two paths cannot drift. Tests pin the equality across chunk
//! sizes and channel layouts.
//!
//! The rest are stateless: `yin`, `correlate`, `cross_correlate`,
//! `spectral_features` are pure functions of their input (the vectorscope's
//! `step_vectorscope` carries only its stride phase between blocks),
//! and `stft` is batch-only — there is no incremental transform. Meter ballistics
//! (`step_ballistics`) carries a smoothed reading, but that is a filter over results
//! rather than a step of the correlation itself.
//!
//! ## Quick start
//!
//! ```rust
//! use tutti_measure::{
//!     correlate, detect_onsets, summarize, yin, DetectionFunction,
//!     OnsetConfig, PeakConfig, YinConfig,
//! };
//! use tutti_dsp::{FftScratch, StftGeometry};
//! use tutti_types::{ChannelLayout, Interleaved, SampleRate, Samples, StereoPlanes};
//!
//! let sample_rate = SampleRate::SR_44K1;
//! let samples: Vec<f32> = vec![0.0; 44100];
//! let mut fft = FftScratch::new();
//!
//! // Waveform blocks for display — one series per channel.
//! let blocks = summarize(
//!     &PeakConfig::new(Samples(512), ChannelLayout::STEREO),
//!     Interleaved::new(&samples, ChannelLayout::STEREO),
//! );
//! let left = blocks.channel(0).expect("stereo has a channel 0");
//! // A meter wants one number per block; a waveform draws both channels.
//! let merged = blocks.to_mono();
//!
//! // Onsets, via spectral flux.
//! let geometry = StftGeometry::new(sample_rate, Samples(2048), Samples(512))?;
//! let onsets = detect_onsets(
//!     &OnsetConfig::new(geometry, DetectionFunction::SpectralFlux),
//!     &samples,
//!     &mut fft,
//! )?;
//!
//! // Pitch. An inverted range is refused here, not silently unvoiced later.
//! let pitch = yin(&YinConfig::standard(sample_rate)?, &samples)?;
//!
//! // Stereo correlation. The planes are paired once — a length mismatch is
//! // refused here rather than silently truncated inside the measurement.
//! let planes = StereoPlanes::new(&samples, &samples).expect("equal lengths");
//! let reading = correlate(planes);
//! # Ok::<(), tutti_measure::Error>(())
//! ```
//!
//! ## Reading from a running graph
//!
//! Nothing here knows about the graph, so the seam is a tap (`tutti-graph`'s
//! `Editor::open_tap`, or a session's `Graph::taps().open`): the audio thread copies
//! the sources into a ring a lane and a control thread drains them through the
//! `TapReader`. What arrives on this side is an ordinary `&[f32]`, which is the
//! whole reason these algorithms need no engine vocabulary.
//!
//! ```rust
//! use tutti_types::MaxBlock;
//! use tutti_measure::correlate;
//! use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, TransportPosition, Unforkable};
//! use tutti_graph::{OutPort, Source};
//! use tutti_types::{Beat, Bpm, ChannelLayout, SampleRate, Samples, StereoPlanes};
//! use tutti_graph::NodeKey;
//! use tutti_graph::{TapAlign, TapId};
//!
//! /// A stereo source: 0.5 on the left, -0.5 on the right.
//! struct Tone;
//! impl Node for Tone {
//!     fn shape(&self) -> Shape {
//!         Shape::source_unassigned(ChannelLayout::STEREO)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         io.output(0).fill(0.5);
//!         io.output(1).fill(-0.5);
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! // A tap on the graph's two outputs.
//! let (mut editor, mut executor) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))));
//! editor.insert(NodeKey(1), "tone", Unforkable(Tone));
//! let out = |port| Source::Node(OutPort { node: NodeKey(1), port });
//! editor.spec_mut().topology.set_outputs([out(0), out(1)]);
//! let mut tap = editor.open_tap(TapId(0), vec![out(0), out(1)], TapAlign::ToOutput, Samples(256)).expect("the ports exist");
//! editor.commit().expect("compiles");
//! executor.apply_pending();
//! editor.collect();
//!
//! // The audio-callback side: a block of 2 frames.
//! let (mut l, mut r) = ([0.0f32; 2], [0.0f32; 2]);
//! let transport = TransportPosition::new(true, Bpm::of(120.0), Beat(0.0), None);
//! executor.process(2, &transport, &[], &mut [&mut l[..], &mut r[..]]);
//!
//! // The analysis side: two interleaved frames, deinterleaved.
//! let mut drained = [0.0f32; 4];
//! assert_eq!(tap.pop_into(&mut drained), Samples(2));
//! let (left, right) = ([drained[0], drained[2]], [drained[1], drained[3]]);
//! let planes = StereoPlanes::new(&left, &right).expect("drained in lockstep");
//!
//! // `Correlation` is a MEASUREMENT type, deliberately distinct from the control
//! // types (`Mix`, `Depth`) despite the coinciding range.
//! let reading = correlate(planes);
//! ```
//!
//! ## Constraint: an invalid combination is refused at construction, never mid-measurement
//!
//! The config constructors are where `tutti_measure::Error` comes from — a hop that exceeds or does
//! not COLA-divide its window, an inverted or above-Nyquist frequency range, a
//! non-positive sample rate. Refusing there is the point: a detector that *accepted*
//! an inverted range would report "unvoiced" forever instead, which is a silent
//! wrong answer rather than an error.
//!
//! A setting with one range of its own is a range-checked type (`Harmonics`,
//! `Tightness`, `SpectrumFloor`, `NoteSpan`, ...): its `new` refuses a value
//! outside the range, so a config holds none, and its `with_*` setter cannot
//! fail. The constructors that still return `Error` refuse only what spans
//! fields or the input: a hop against its window, a band against Nyquist, a
//! tempo against the curve's length.
//!
//! The measurement entry points are correspondingly thin on failure. `correlate` and
//! `summarize` are infallible — the pairing and layout checks already happened, in
//! `StereoPlanes::new` and `PeakConfig`. `measure_loudness` returns `Option`,
//! because too little audio to gate is an absence rather than a fault. Only `yin`
//! and `detect_onsets` return `Result`, and both do so before touching a sample.
//!
//! `Result`, not `assert!`, throughout: a caller feeding a short buffer gets an
//! error rather than a panic.
//!
//! ## Allocation
//!
//! The batch entry points allocate their results, so call them off the audio
//! thread. The `step_*` functions and `FftScratch` let a caller reuse buffers
//! across calls. `step_spectrum` allocates nothing once its state is built, and
//! `step_windowed_loudness` allocates nothing either, but reads three-second windows
//! on every call, so it belongs on the consumer side of a tap's `TapReader`, not in an
//! audio callback.
//!
//! ## Features
//!
//! None: `default = []`.
//!
//! The `tutti` crate re-exports this one as `tutti::measure` behind its `dsp`
//! feature, which also turns on `tutti-dsp`. `tutti-export` uses it for loudness
//! measurement.
#![forbid(unsafe_code)]
//!
//! ## Items
//!
//! - Transform: [`stft`], [`stft_magnitude`], [`stft_polar`] and
//!   [`istft_transform`], over a validated [`StftGeometry`](tutti_dsp::StftGeometry) with a
//!   [`CosineWindow`](tutti_dsp::CosineWindow), producing [`Stft`], [`StftMagnitude`] or [`StftPolar`].
//! - Pitch: [`yin()`] and [`yin_track`] with a [`YinConfig`], returning
//!   [`PitchEstimate`] and [`Pitch`]; [`median_filter`] and [`penalize_jumps`]
//!   clean up a track.
//! - Onsets: [`detect_onsets`] with an [`OnsetConfig`] and a
//!   [`DetectionFunction`], or incrementally with [`step_onset`] and
//!   [`finish_onset`]; [`OnsetState::novelty`] reads the raw detection curve.
//! - Waveform blocks: [`summarize`] with a [`PeakConfig`] into [`PeakBlocks`],
//!   or incrementally with [`step_peaks`] and [`finish_peaks`].
//! - Loudness: [`measure_loudness`] with a [`LoudnessConfig`] into
//!   [`Loudness`], or incrementally with [`step_loudness`] and
//!   [`finish_loudness`].
//! - Windowed loudness: [`step_windowed_loudness`] with a [`WindowedLoudnessConfig`] and a
//!   [`WindowedLoudnessState`] reads momentary (400 ms), short-term (3 s) and true
//!   peak as a [`WindowedReading`] after every chunk.
//! - Spectrum: [`step_spectrum`] with a [`SpectrumConfig`] and a
//!   [`SpectrumState`] streams power in dB per bin or [`Banding`] band, with
//!   [`Smoothing`], held peaks and a rolling spectrogram.
//! - Stereo: [`correlate`] into a [`StereoReading`], smoothed for a meter by
//!   [`step_ballistics`].
//! - Spectral shape: [`spectral_centroid`], [`spectral_flatness`] and
//!   [`spectral_rolloff`] over one magnitude spectrum, or [`spectral_features`]
//!   over every frame of a [`StftMagnitude`], into [`SpectralFeatures`].
//! - Strip silence: [`strip_silence`] with a [`SilenceConfig`] into
//!   [`Region`]s, or incrementally with [`step_silence`] and
//!   [`finish_silence`], which reads the config the scan used.
//! - Vectorscope: [`vectorscope_points`] turns a stereo pair into
//!   [`ScopePoint`]s (side, mid), or block by block with [`step_vectorscope`]
//!   and a [`VectorscopeState`].
//! - Cross-correlation: [`cross_correlate`] into a [`CrossCorrelation`], read
//!   at a lag or for its [`CorrelationPeak`].
//! - Take alignment: [`align()`] with an [`AlignConfig`] finds an [`Alignment`]
//!   (whole-sample lag, sub-sample offset, confidence) by GCC-PHAT.
//! - Tempo: [`estimate_tempo()`] with a [`TempoConfig`] finds a [`Tempo`] (BPM,
//!   confidence, runner-up octaves); [`track_beats()`] places a [`BeatGrid`] by
//!   dynamic programming; the `_from_novelty` forms take your own curve.
//! - Notes: [`transcribe`] with a [`NoteConfig`] turns a mono buffer into
//!   [`NoteEvent`]s (note, cents bend, span, level); [`notes_from_track`]
//!   takes your own pitch track, [`frame_levels`] and onsets;
//!   [`VelocityRange`] maps a level to a MIDI velocity.
//! - Harmony: [`chroma()`] with a [`ChromaConfig`] folds spectral peaks into
//!   twelve pitch classes and estimates the [`Tuning`], or incrementally with
//!   [`step_chroma`] and a [`ChromaState`]; [`estimate_key`] finds a [`Key`] by
//!   Krumhansl-Schmuckler over a [`KeyProfile`]; [`detect_chords`] with a
//!   [`ChordConfig`] smooths triads into [`ChordSpan`]s by Viterbi.

mod align;
mod beat;
mod chord;
mod chroma;
mod error;
mod features;
mod key;
mod loudness;
mod notes;
mod onset;
mod peaks;
// No `///` here: a doc comment on a `mod` line shadows the module's own `//!`.
// The module header carries the description.
mod pitch;
#[cfg(test)]
mod range_contracts;
mod ranged;
mod silence;
mod spectrum;
mod stereo;
mod tempo;
mod transform;
mod vectorscope;
mod xcorr;
mod yin;

pub use align::{AlignConfig, Alignment, align};
pub use chord::{
    ChordConfig, ChordQuality, ChordSpan, SelfTransition, SelfTransitionError, Sharpness,
    SharpnessError, detect_chords,
};
pub use chroma::{
    Chroma, ChromaConfig, ChromaState, HarmonicDecay, HarmonicDecayError, Harmonics,
    HarmonicsError, PeakFloor, PeakFloorError, Tuning, TuningOffset, TuningOffsetError, chroma,
    step_chroma,
};
pub use error::{Error, Result};
pub use features::{
    SpectralFeatures, spectral_centroid, spectral_features, spectral_flatness, spectral_rolloff,
};
pub use key::{Key, KeyEstimate, KeyProfile, Mode, estimate_key, estimate_key_from_mean};
pub use loudness::{
    Loudness, LoudnessConfig, LoudnessState, WindowedLayout, WindowedLayoutError,
    WindowedLoudnessConfig, WindowedLoudnessState, WindowedRate, WindowedRateError,
    WindowedReading, finish as finish_loudness, measure_loudness, step_loudness,
    step_windowed_loudness,
};
// `finish` is renamed on the way out, matching `finish_peaks` /
// `finish_loudness` above: three modules each have a `finish`, and the bare name
// says nothing about which accumulator it drains.
pub use notes::{
    NoteConfig, NoteEvent, NoteSpan, NoteSpanError, PitchSplit, PitchSplitError, VelocityRange,
    frame_levels, notes_from_track, transcribe,
};
pub use onset::{
    DetectionFunction, Onset, OnsetConfig, OnsetState, complex_domain_deviation, detect_onsets,
    finish as finish_onset, high_frequency_content, spectral_energy, spectral_flux, step_onset,
    suppress_close_onsets,
};
pub use peaks::{
    PeakAccum, PeakBlock, PeakBlocks, PeakConfig, PeakState, finish as finish_peaks, step_peaks,
    summarize, summarize_block,
};
pub use silence::{
    Region, SilenceConfig, SilenceState, finish as finish_silence, step_silence, strip_silence,
};
pub use spectrum::{
    BandCount, BandCountError, Banding, BandsPerOctave, BandsPerOctaveError, PeakDecay,
    PeakDecayError, Smoothing, SpectrumConfig, SpectrumFloor, SpectrumFloorError, SpectrumState,
    SpectrumTime, SpectrumTimeError, step_spectrum,
};
pub use stereo::{
    Ballistics, BallisticsState, StereoLevels, StereoReading, correlate, step_ballistics,
};
pub use tempo::{
    BeatGrid, PriorSpread, PriorSpreadError, Tempo, TempoCandidate, TempoConfig, Tightness,
    TightnessError, estimate_tempo, estimate_tempo_from_novelty, track_beats,
    track_beats_from_novelty,
};
pub use transform::{
    HopPolicy, NormalizedMagnitudes, RawMagnitudes, SampleRange, Stft, StftMagnitude, StftPolar,
    StftRequest, istft as istft_transform, stft, stft_magnitude, stft_polar,
};
pub use vectorscope::{ScopePoint, VectorscopeState, step_vectorscope, vectorscope_points};
pub use xcorr::{CorrelationPeak, CrossCorrelation, cross_correlate};
pub use yin::{Pitch, PitchEstimate, YinConfig, median_filter, penalize_jumps, yin, yin_track};

/// Notes and pitch classes, re-exported from the engine vocabulary: a
/// [`Pitch`] names one, and callers should not need a second import to read it.
pub use tutti_types::{Note, PitchClass};
