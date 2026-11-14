//! Failures of the analysers.
//!
//! `Result`, not `Option`, and not `assert!`: a caller who passes a bad
//! setting deserves to know which invariant broke, and a caller feeding a
//! short buffer deserves better than a panic on the audio path.

use tutti_dsp::FrameCount;
use tutti_types::{Bpm, ChannelLayout, Db, Hz, SampleRate, Samples};

/// Everything that can go wrong constructing or feeding an analysis.
///
/// An analyser's own validation raises its own variant. [`Dsp`](Self::Dsp)
/// carries what the shared STFT infrastructure refused (a window and hop that
/// do not fit, a grid whose shape disagrees with its data), so `?` on
/// building a `StftGeometry` or a `Grid` keeps working.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The shared STFT infrastructure refused a geometry or a grid.
    #[error(transparent)]
    Dsp(#[from] tutti_dsp::Error),
    /// A zero hop: frames would never advance.
    #[error("hop size is zero: frames would not advance")]
    ZeroHop,
    /// A threshold in decibels that is NaN or infinite.
    #[error("threshold {threshold} is not finite")]
    InvalidThreshold {
        /// The offending threshold, in [`Db`].
        threshold: Db,
    },
    /// `min >= max`, or either bound at or below zero. Refused at construction
    /// because a detector that accepts it reports "unvoiced" forever instead.
    #[error("empty frequency range: min {min} is not below max {max}")]
    EmptyFrequencyRange {
        /// The lower bound in [`Hz`]; not below `max`.
        min: Hz,
        /// The upper bound in [`Hz`].
        max: Hz,
    },
    /// A frequency bound above Nyquist for the given rate.
    #[error("frequency {freq} is above Nyquist {nyquist}")]
    AboveNyquist {
        /// The offending bound, in [`Hz`].
        freq: Hz,
        /// Half the sample rate, in [`Hz`].
        nyquist: Hz,
    },
    /// A tempo range with `min` not below `max`, or a bound outside what the
    /// analysis searches, or one that leaves no whole-frame lag to search.
    #[error("empty tempo range: min {min} is not below max {max}, or no lag falls in it")]
    EmptyTempoRange {
        /// The slower bound, in [`Bpm`].
        min: Bpm,
        /// The faster bound, in [`Bpm`].
        max: Bpm,
    },
    /// A spectrum band that holds no bin: the window is too short for it.
    #[error(
        "band {band} ({lower}..{upper}) holds no bin: a window of {window} samples \
         holds every band"
    )]
    EmptyBand {
        /// The first band with no bin, counting from the lowest.
        band: usize,
        /// Its lower edge, in [`Hz`].
        lower: Hz,
        /// Its upper edge, in [`Hz`].
        upper: Hz,
        /// A window, in [`Samples`], at least as wide as the narrowest band
        /// needs, so that every band holds a bin.
        window: Samples,
    },
    /// A window outside what the analysis accepts: the FFT works on the
    /// window, so an absurd one would ask for gigabytes, and a tiny one
    /// resolves nothing.
    #[error("window of {window} is outside {min}..={max}")]
    WindowOutOfRange {
        /// The geometry's window, in [`Samples`].
        window: Samples,
        /// The smallest accepted window.
        min: Samples,
        /// The largest accepted window.
        max: Samples,
    },
    /// A hop wider than the window or frame: samples between frames would be
    /// read by none.
    #[error("hop {hop} is wider than the window {window}")]
    HopWiderThanWindow {
        /// The hop, in [`Samples`].
        hop: Samples,
        /// The window or frame, in [`Samples`].
        window: Samples,
    },
    /// A hop so small that more than `max` frames overlap in one window; each
    /// frame is a whole transform, so this bounds the cost of a tiny hop.
    #[error("a hop of {hop} makes more than {max} frames overlap in a window of {window}")]
    TooManyOverlaps {
        /// The window or frame, in [`Samples`].
        window: Samples,
        /// The hop, in [`Samples`].
        hop: Samples,
        /// The most overlapping frames accepted.
        max: usize,
    },
    /// A sample rate outside the range an analysis is built for.
    #[error("sample rate {rate} is outside {min}..={max}")]
    SampleRateOutOfRange {
        /// The rate refused.
        rate: SampleRate,
        /// The lowest accepted rate.
        min: SampleRate,
        /// The highest accepted rate.
        max: SampleRate,
    },
    /// More frames than one call analyses, which bounds its output.
    #[error("{frames} frames is more than the {max} one call analyses")]
    TooManyFrames {
        /// The frames the input would make.
        frames: FrameCount,
        /// The most accepted.
        max: FrameCount,
    },
    /// A frame that is not one window long.
    #[error("frame of {got} samples is not the window of {window}")]
    FrameLengthMismatch {
        /// What the caller supplied, in [`Samples`].
        got: Samples,
        /// The window, in [`Samples`].
        window: Samples,
    },
    /// A state built for another config than the one stepped with.
    #[error("state was built for another config")]
    StateMismatch,
    /// A grid of chroma that is not twelve columns wide.
    #[error("chroma is {columns} columns wide, not 12")]
    NotTwelveClasses {
        /// The grid's width.
        columns: usize,
    },
    /// A value that must not be negative is.
    #[error("input holds a negative value")]
    NegativeInput,
    /// A chord span whose position is beyond what a sample count holds.
    #[error("a span lies beyond a sample count")]
    SpanBeyondSampleCount,
    /// A YIN frame longer than a note analysis accepts (a `min_freq` that low
    /// is not a note).
    #[error("yin frame of {frame} is longer than {max}")]
    YinFrameTooLong {
        /// The YIN frame, in [`Samples`].
        frame: Samples,
        /// The longest accepted.
        max: Samples,
    },
    /// An onset detector that is not spectral flux, the only function whose
    /// peak has a known lag behind its attack.
    #[error("note onsets need spectral flux")]
    OnsetNotSpectralFlux,
    /// A pitch and an onset analysis at different sample rates.
    #[error("onset sample rate {onset} differs from yin's {yin}")]
    SampleRateMismatch {
        /// The onset geometry's rate.
        onset: SampleRate,
        /// YIN's rate.
        yin: SampleRate,
    },
    /// A velocity range whose floor is not below its ceiling.
    #[error("velocity floor {floor} is not below ceiling {ceiling}")]
    EmptyVelocityRange {
        /// The level read as velocity 1.
        floor: Db,
        /// The level read as velocity 127.
        ceiling: Db,
    },
    /// Levels and a pitch track of different lengths.
    #[error("{levels} levels for a pitch track of {track} frames")]
    LevelsLengthMismatch {
        /// Levels supplied.
        levels: usize,
        /// Frames in the track.
        track: usize,
    },
    /// A smoothing span of more frames than the median's cost allows.
    #[error("smoothing of {frames} frames is more than {max}")]
    SmoothingTooLong {
        /// The span, in frames at the hop in use.
        frames: FrameCount,
        /// The most accepted.
        max: FrameCount,
    },
    /// A voiced frequency of zero or below.
    #[error("a voiced frequency is not above zero")]
    NonPositiveFrequency,
    /// A tempo whose beat period is not between two frames and the novelty
    /// curve's length.
    #[error("tempo {bpm} has no beat period between two frames and the curve's length")]
    UntrackableTempo {
        /// The tempo refused.
        bpm: Bpm,
    },
    /// More spectrum bands than are accepted.
    #[error("{bands} bands is more than {max}")]
    TooManyBands {
        /// The bands the banding makes.
        bands: usize,
        /// The most accepted.
        max: usize,
    },
    /// A spectrogram of more cells than are accepted.
    #[error("a spectrogram of {cells} cells is more than {max}")]
    SpectrogramTooLarge {
        /// Columns times outputs.
        cells: usize,
        /// The most accepted.
        max: usize,
    },
    /// A peak hold of more frames than the counter holds.
    #[error("a peak hold of {frames} frames is more than {max}")]
    PeakHoldTooLong {
        /// The hold, in frames at the hop in use.
        frames: usize,
        /// The most accepted.
        max: usize,
    },
    /// A chunk whose layout is not the config's.
    #[error("chunk layout {chunk:?} differs from the config's {config:?}")]
    ChunkLayoutMismatch {
        /// The chunk's layout.
        chunk: ChannelLayout,
        /// The config's layout.
        config: ChannelLayout,
    },
    /// The loudness meter could not be built.
    #[error("the loudness meter could not be built")]
    MeterUnavailable,
    /// The loudness meter refused the frames fed to it.
    #[error("the loudness meter rejected the frames")]
    MeterRejectedFrames,
    /// No pitch to read a key from: no frames, silent, or a chroma so flat
    /// that every key correlates alike.
    #[error("no tonal content: silent or flat chroma")]
    NoTonalContent,
    /// Two grids that must share a shape do not.
    #[error("grids that must share a shape have different shapes")]
    GridShapeDisagreement,
    /// A sample that is NaN or infinite.
    #[error("input holds a NaN or infinite sample")]
    NonFiniteInput,
    /// Nothing to correlate: silent input, or a band holding no spectrum bin.
    #[error("no signal to correlate: silent input or an empty band")]
    NoSignal,
    /// Input shorter than the algorithm's minimum.
    #[error("need at least {needed} samples, got {got}")]
    InsufficientInput {
        /// The algorithm's minimum, in [`Samples`].
        needed: Samples,
        /// What the caller supplied, in [`Samples`].
        got: Samples,
    },
}

/// Result of an analysis call, defaulting the error to [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
