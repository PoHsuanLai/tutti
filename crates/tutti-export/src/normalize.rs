//! Normalization: choosing a gain from a measurement of the whole signal.
//!
//! Normalization needs the whole signal measured before a gain can be chosen,
//! which is two passes, so an output that asks for it
//! ([`FileTarget::with_normalize`](crate::FileTarget::with_normalize)) is not
//! streamed to its encoder while the graph renders: [`render`](crate::render())
//! keeps what the graph rendered for it (`spill`: memory up to a budget, then a
//! temporary file beside the target), measures it with a [`Measurer`] at the
//! file's rate, and writes it again through the same chain a streamed output
//! uses, with the gain. The graph is rendered once.
//!
//! A caller who holds planes and wants the reading for itself uses
//! [`Normalize::gain_for_rendered`] and [`Rendered::apply_gain`](crate::Rendered::apply_gain).

use tutti_dsp::DitherShape;
use tutti_measure::{Loudness, LoudnessConfig, LoudnessState, finish_loudness, step_loudness};
use tutti_types::{Amplitude, ChannelLayout, Db, Interleaved, InterleavedMut, SampleRate, Samples};

use crate::Result;
use crate::Unmeasured;
#[cfg(doc)]
use crate::Written;
use crate::control::{Control, Outcome, Stage, Weights};
use crate::output::FileTarget;

/// A gain chosen by measuring the rendered signal.
///
/// Both variants measure with the same EBU R128 meter, so both cost the same
/// second pass; they differ only in what they aim at. In particular `Peak`
/// targets true peak, not sample peak — an
/// inter-sample peak a sample-peak reading misses is exactly what clips on a
/// consumer DAC after resampling. The meter oversamples 4× below 96 kHz and 2×
/// from 96 kHz to below 192 kHz; at 192 kHz and above it reads the sample peak.
///
/// # Above six channels
///
/// [`Lufs`](Self::Lufs) is measured through R128's standard channel map. For
/// six or more channels it weights L, R, C, Ls and Rs (slots 0, 1, 2, 4, 5),
/// ignores the LFE (slot 3) and leaves any channel beyond the sixth unweighted;
/// four channels read as L, R, Ls, Rs and five as L, R, C, Ls, Rs. A 5.1 mix
/// with only the LFE active reads -70 LUFS. A 7.1 or 7.1.4 export is therefore
/// normalized on five of its first six channels: content that lives only in
/// the rear surrounds or the height layer does not raise the reading, so the
/// gain lands higher than that mix's
/// true loudness warrants — the ceiling is what stops it.
///
/// [`Peak`](Self::Peak) has no such limit; true peak folds over every channel.
/// Prefer it when normalizing wide layouts whose energy is not concentrated in
/// the front six.
///
/// # Measured at the output rate
///
/// When the file is written at another rate, the conversion happens *before*
/// the measurement. Sample-rate conversion moves the true peak (its
/// interpolation overshoots between the original samples), so a gain chosen at
/// the render rate and applied to resampled audio misses its target, and a
/// ceiling chosen to prevent clipping does not prevent it. Measuring the
/// converted signal is what makes the dBTP figure a promise about the file
/// rather than about an intermediate nobody hears. (The render is converted
/// twice, once to be measured and once to be written, so as to hold no
/// converted copy; the converter is deterministic and both see the same
/// samples.)
///
/// # Headroom
///
/// For an integer format (`Pcm::Int16`, `Pcm::Int24`) the peak target of
/// [`Peak`](Self::Peak), and the ceiling of [`Lufs`](Self::Lufs), are pulled
/// down to leave one LSB plus the dither amplitude of headroom below full
/// scale (an LSB for no dither, 1.5 for rectangular, 2 for triangular): a peak
/// normalized to 0 dBTP, plus dither, would be clamped by the quantizer. A
/// target already under the limit is unchanged. [`Written::clipped_samples`]
/// counts what is clamped all the same, and [`Written::gain`] is the gain to
/// the lowered target. An output that takes another's gain
/// ([`GainSource::FromOutput`](crate::GainSource::FromOutput)) keeps that
/// output's headroom, not its own.
///
/// # Quiet audio
///
/// [`Peak`](Self::Peak) refuses only exact silence. Near-silence (-100 dBTP,
/// say) is audio with a reading and gets its gain (about +99 dB); read
/// [`Written::gain`] to see what was applied.
///
/// [`Written::gain`] reports the gain applied and [`Written::measured`] the
/// reading it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Normalize {
    /// Bring the loudest true peak to `target` dBTP.
    ///
    /// Any audio with a reading is measurable, however quiet: a render whose
    /// true peak is -100 dBTP gets about +99 dB, and the gain is applied
    /// (it is reported in [`Written::gain`]). Only exact silence, a true
    /// peak at the floor ([`Db::FLOOR`]), is refused
    /// ([`Error::Unmeasurable`](crate::Error::Unmeasurable)); there is no
    /// invented threshold between the two.
    Peak {
        /// True-peak level to land on, in dBTP. Typically negative — 0 leaves no
        /// headroom for a consumer DAC's reconstruction filter.
        target: Db,
    },
    /// Bring integrated loudness to `target` LUFS, pulled back so the true peak
    /// does not exceed `ceiling` dBTP.
    Lufs {
        /// Integrated loudness to land on, in LUFS.
        target: Db,
        /// True-peak limit in dBTP. When the loudness gain would push the peak
        /// past this, the ceiling wins and the file lands quieter than
        /// `target`. [`Normalize::lufs`] sets it to −1.0.
        ceiling: Db,
    },
}

impl Normalize {
    /// Loudness normalization with a −1.0 dBTP ceiling — the value streaming
    /// platforms ask for, carried here so callers need not know the number.
    pub const fn lufs(target: Db) -> Self {
        Self::Lufs {
            target,
            ceiling: Db(-1.0),
        }
    }

    /// Peak normalization to `target` dBTP.
    pub const fn peak(target: Db) -> Self {
        Self::Peak { target }
    }

    /// The gain this mode asks for, given a reading.
    ///
    /// Split out so it is testable without rendering, and so the two arms sit
    /// next to each other: both are "move a measured dB figure to a target",
    /// and only the loudness arm has a ceiling to respect (which
    /// [`Loudness::gain_to`](tutti_measure::Loudness::gain_to) already
    /// applies).
    pub(crate) fn gain_for(&self, measured: &tutti_measure::Loudness) -> Db {
        match *self {
            // `Db - Db` is a gain difference, which is why `Db` implements
            // `Sub` — see the `Additive` note on `Db` in `units/level.rs`.
            Self::Peak { target } => target - measured.true_peak,
            Self::Lufs { target, ceiling } => measured.gain_to(target, ceiling),
        }
    }

    /// The gain that normalizes `rendered`.
    ///
    /// For a caller that already holds a [`Rendered`](crate::Rendered) and is
    /// not writing it with a normalizing output — handing the PCM to
    /// another encoder, say. Returning the gain rather than applying it keeps
    /// the reading available to log or gate on, and is the same value a
    /// normalizing output uses, so the two can never disagree. (A normalizing
    /// output also pulls its target and ceiling down to leave an
    /// integer format its headroom, which this cannot: it does not know the
    /// format. See "Headroom" on [`Normalize`].)
    ///
    /// # Errors
    ///
    /// [`Error::Unmeasurable`](crate::Error::Unmeasurable) when the audio has
    /// no usable reading, and `reason` says which:
    ///
    /// - [`Unmeasured::Format`]: the meter will not
    ///   read this signal (EBU R128 accepts 1–64 channels at 16 Hz–2.8 MHz);
    /// - [`Unmeasured::BelowGate`], for
    ///   [`Lufs`](Self::Lufs): nothing passed the integrated loudness gate
    ///   (silence, under -70 LUFS, or under 400 ms), so the reading is the
    ///   gate's floor and not a measurement;
    /// - [`Unmeasured::Silent`], for
    ///   [`Peak`](Self::Peak): the true peak is at the floor.
    ///
    /// An error rather than a `None` the caller might discard, or a gain made
    /// up from the floor (+47 dB for a LUFS target of -23, up to +99 dB for a
    /// peak one): writing a hiss at full scale, or the file un-normalized, and
    /// reporting success is silent — nothing in [`Written`] records it.
    pub fn gain_for_rendered(&self, rendered: &crate::Rendered) -> Result<Db> {
        let mut control = Control::none();
        control.weigh(Weights::Normalized);
        let total = rendered.frames();
        let outcome = self.measure(rendered, Samples(0), total, &mut control)?;
        Ok(self.gain_for(&finished(outcome)))
    }

    /// The reading [`gain_for_rendered`](Self::gain_for_rendered) takes its
    /// gain from, refused when it is not a measurement of the programme.
    ///
    /// Measured by a [`Measurer`] a chunk at a time, so `control` is checked
    /// between chunks and each chunk reads as [`Stage::Measuring`]: `done`
    /// frames of `total` were already spent in that stage (the conversion
    /// before it), and this reading counts `rendered.frames()` more.
    ///
    /// # Errors
    ///
    /// As [`gain_for_rendered`](Self::gain_for_rendered).
    fn measure(
        &self,
        rendered: &crate::Rendered,
        done: Samples,
        total: Samples,
        control: &mut Control<'_>,
    ) -> Result<Outcome<Loudness>> {
        let unmeasurable = |reason| crate::Error::Unmeasurable {
            output: None,
            channels: rendered.layout(),
            sample_rate: rendered.sample_rate(),
            reason,
        };
        let mut measurer = Measurer::new(rendered.sample_rate(), rendered.layout())
            .ok_or_else(|| unmeasurable(Unmeasured::Format))?;
        let planes = rendered.planes();
        let frames = rendered.frames().get();
        let chunk = chunk_frames(rendered.sample_rate());
        let mut at = 0;
        while at < frames {
            if control.is_cancelled() {
                return Ok(Outcome::Cancelled);
            }
            let end = (at + chunk).min(frames);
            measurer.feed(planes, at..end);
            at = end;
            control.advance(Stage::Measuring, Samples(done.get() + at), total);
        }
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let reading = measurer.finish();
        self.check(&reading).map_err(unmeasurable)?;
        Ok(Outcome::Finished(reading))
    }

    /// Whether `reading` measures the programme, for what this mode aims at.
    ///
    /// The loudness meter reports its absolute gate (-70 LUFS) when nothing
    /// passed it, and the true-peak floor for silence: those are the readings
    /// of "no signal", not of a quiet one, and a gain from either is invented.
    pub(crate) fn check(
        &self,
        reading: &tutti_measure::Loudness,
    ) -> std::result::Result<(), Unmeasured> {
        match self {
            // An integrated loudness is always above the gate when it was
            // measured, so the gate itself is the "passed nothing" sentinel.
            Self::Lufs { .. } if reading.lufs.get() <= ABSOLUTE_GATE_LUFS => {
                Err(Unmeasured::BelowGate)
            }
            Self::Peak { .. } if reading.true_peak <= Db::FLOOR => Err(Unmeasured::Silent),
            _ => Ok(()),
        }
    }

    /// This mode with its peak target (`Peak`) or ceiling (`Lufs`) pulled down
    /// to what the file's format can hold.
    ///
    /// An integer format clamps at full scale, and dither adds noise to every
    /// sample before it does, so a peak at 0 dBTP clips about half its crests.
    /// The limit is one LSB plus the dither's amplitude (0 for none, half an
    /// LSB for rectangular, one for triangular) below full scale; a target or
    /// ceiling already under it is unchanged, and a format that does not
    /// quantize ([`Pcm::Float32`](tutti_codec::Pcm::Float32), Ogg Vorbis) has no
    /// limit.
    pub(crate) fn within(self, target: &FileTarget) -> Self {
        let Some(limit) = headroom_ceiling(target) else {
            return self;
        };
        let cap = |db: Db| Db(db.get().min(limit.get()));
        match self {
            Self::Peak { target } => Self::Peak {
                target: cap(target),
            },
            Self::Lufs { target, ceiling } => Self::Lufs {
                target,
                ceiling: cap(ceiling),
            },
        }
    }
}

/// The value of an outcome no token could have cancelled.
fn finished<T>(outcome: Outcome<T>) -> T {
    outcome
        .finished()
        .expect("a control with no token is never cancelled")
}

/// The loudest peak, in dBTP, an integer format can take with its dither
/// ([`Normalize::within`]), or `None` for a format that does not quantize.
fn headroom_ceiling(target: &FileTarget) -> Option<Db> {
    let depth = target.format().int_depth()?;
    // One LSB in the `[-1, 1]` float domain, as the dither scales it.
    let lsb = depth.lsb().get();
    let dither_peak = match target.dither() {
        None => 0.0,
        Some(DitherShape::Rectangular) => 0.5,
        Some(DitherShape::Triangular) => 1.0,
    };
    Some(Db::from_amplitude(Amplitude(
        1.0 - (1.0 + dither_peak) * lsb,
    )))
}

/// The most frames a measurement takes at once: one second of audio at `rate`,
/// and at least one. A cancel waits for at most one chunk (doc 019 section
/// 6.2), and the chunk is the unit the conversion before it is cut in too.
pub(crate) fn chunk_frames(rate: SampleRate) -> usize {
    (rate.get().round() as usize).max(1)
}

/// A loudness reading taken a chunk at a time, over planes.
///
/// The smallest thing normalize needs to be stoppable: `tutti-measure`'s
/// `LoudnessState` already accumulates (and is what `measure_loudness` folds),
/// so this only adds the planar-to-interleaved copy of one chunk, so that no
/// whole interleaved copy of the render is made (the one-shot reading made
/// one), and the chunking the caller checks `Control` between. Chunks end on
/// frame boundaries, where the reading does not depend on where the stream is
/// cut.
pub(crate) struct Measurer {
    cfg: LoudnessConfig,
    state: LoudnessState,
    /// One chunk, interleaved: reused, so a measurement allocates once.
    scratch: Vec<f32>,
}

impl Measurer {
    /// A meter for frames at `rate` in `layout`; `None` for a width or rate
    /// EBU R128 does not take.
    pub(crate) fn new(rate: SampleRate, layout: ChannelLayout) -> Option<Self> {
        let cfg = LoudnessConfig::new(rate, layout);
        Some(Self {
            state: LoudnessState::new(&cfg)?,
            cfg,
            scratch: Vec::new(),
        })
    }

    /// Feeds the frames `span` of `planes`.
    fn feed(&mut self, planes: &[Vec<f32>], span: std::ops::Range<usize>) {
        let layout = self.cfg.layout;
        let frames = span.len();
        self.scratch.clear();
        self.scratch
            .resize(Samples(frames).interleaved_len(layout), 0.0);
        InterleavedMut::new(&mut self.scratch, layout).interleave_from(planes, span.start);
        step_loudness(
            &self.cfg,
            &mut self.state,
            Interleaved::new(&self.scratch, layout),
        );
    }

    /// Feeds `frames`, already interleaved in this meter's layout.
    pub(crate) fn feed_interleaved(&mut self, frames: &[f32]) {
        step_loudness(
            &self.cfg,
            &mut self.state,
            Interleaved::new(frames, self.cfg.layout),
        );
    }

    /// The reading of everything fed.
    pub(crate) fn finish(self) -> Loudness {
        finish_loudness(self.state)
    }
}

/// EBU R128's absolute gate, in LUFS: what the meter's integrated loudness
/// reads when no block passed it.
const ABSOLUTE_GATE_LUFS: f32 = -70.0;

#[cfg(test)]
mod tests;
