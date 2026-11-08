//! Measure-then-apply normalization, composed over the streaming primitives.
//!
//! Normalization needs the whole signal measured before a gain can be chosen,
//! which is two passes. [`render_to_file`](crate::render_to_file) will not do
//! that — it streams, and choosing a gain would force it to hold the whole
//! signal. So the two-pass path is a
//! *separately named* function: the cost is in the name, not hidden behind a
//! config field a caller sets without noticing.
//!
//! The composition itself is public API elsewhere — a caller who wants to log
//! the reading, gate on it, or apply a gain of its own writes the four steps
//! directly ([`render_to_buffers`],
//! [`measure_loudness`],
//! [`Loudness::gain_to`](tutti_dsp::Loudness::gain_to),
//! [`Rendered::apply_gain`](crate::Rendered::apply_gain),
//! [`write_buffers`]). This function exists because every
//! host was otherwise writing the same five lines.

use std::path::Path;

use tutti_dsp::{LoudnessConfig, measure_loudness};
use tutti_transport::RenderClock;
use tutti_types::{Amplitude, Db, Interleaved};

use crate::Unmeasured;
use crate::options::Dither;

use crate::config::ExportConfig;
use crate::control::{Control, never_stop};
use crate::{Flow, Outcome, Pass, Progress, Result, Written};
#[cfg(doc)]
use crate::{render_to_buffers, render_to_file_with, write_buffers};

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
/// A peak target (or a loudness ceiling) within the headroom of full scale is
/// lowered on `Int16`/`Int24` (see `render_normalized_to_file`, "Headroom"), and
/// [`Written::normalized`] reports the effective figure and the gain applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Normalize {
    /// Bring the loudest true peak to `target` dBTP.
    ///
    /// Any audio with a reading is measurable, however quiet: a render whose
    /// true peak is -100 dBTP gets about +99 dB, and the gain is applied
    /// (it is reported in [`Written::normalized`]). Only exact silence, a true
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
    /// [`Loudness::gain_to`](tutti_dsp::Loudness::gain_to) already
    /// applies).
    fn gain_for(&self, measured: &tutti_dsp::Loudness) -> Db {
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
    /// not writing it with [`render_normalized_to_file`] — handing the PCM to
    /// another encoder, say. Returning the gain rather than applying it keeps
    /// the reading available to log or gate on, and is the same value the
    /// two-pass render uses internally, so the two can never disagree. (The
    /// two-pass render also pulls its target and ceiling down to leave an
    /// integer format its headroom, which this cannot: it does not know the
    /// format. See [`render_normalized_to_file`].)
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
        let unmeasurable = |reason| crate::Error::Unmeasurable {
            channels: rendered.layout(),
            sample_rate: rendered.sample_rate,
            reason,
        };
        let meter = LoudnessConfig::new(rendered.sample_rate, rendered.layout());
        let samples = rendered.interleaved();
        let reading = measure_loudness(&meter, Interleaved::new(&samples, rendered.layout()))
            .ok_or_else(|| unmeasurable(Unmeasured::Format))?;
        self.check(&reading).map_err(unmeasurable)?;
        Ok(self.gain_for(&reading))
    }

    /// Whether `reading` measures the programme, for what this mode aims at.
    ///
    /// The loudness meter reports its absolute gate (-70 LUFS) when nothing
    /// passed it, and the true-peak floor for silence: those are the readings
    /// of "no signal", not of a quiet one, and a gain from either is invented.
    fn check(&self, reading: &tutti_dsp::Loudness) -> std::result::Result<(), Unmeasured> {
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
    /// quantize ([`Pcm::Float32`](crate::Pcm::Float32), Ogg Vorbis) has no
    /// limit.
    pub(crate) fn within(self, config: &ExportConfig) -> Self {
        let Some(limit) = headroom_ceiling(config) else {
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

/// The loudest peak, in dBTP, an integer format can take with its dither
/// ([`Normalize::within`]), or `None` for a format that does not quantize.
fn headroom_ceiling(config: &ExportConfig) -> Option<Db> {
    let depth = config.encode.format.int_depth()?;
    // One LSB in the `[-1, 1]` float domain, as `DitherState` scales it.
    let lsb = 1.0 / (1u32 << (depth.bits() - 1)) as f32;
    let dither_peak = match config.dither {
        Dither::Off => 0.0,
        Dither::Rectangular => 0.5,
        Dither::Triangular => 1.0,
    };
    Some(Db::from_amplitude(Amplitude(
        1.0 - (1.0 + dither_peak) * lsb,
    )))
}

/// EBU R128's absolute gate, in LUFS: what the meter's integrated loudness
/// reads when no block passed it.
const ABSOLUTE_GATE_LUFS: f32 = -70.0;

/// Render `graph`, measure it, apply the resulting gain, and write it to `path`.
///
/// Two passes. The whole render is held in memory so a gain can be chosen
/// from it. Use [`render_to_file`](crate::render_to_file) when no gain is
/// needed — it streams and holds nothing.
///
/// # Measured at the output rate
///
/// When `config.resample` asks for a rate conversion, it happens *before* the
/// measurement, not after. Sample-rate conversion moves the true peak — its
/// interpolation overshoots between the original samples — so a gain chosen at
/// the render rate and applied to resampled audio misses its target, and a
/// ceiling chosen to prevent clipping does not prevent it. Measuring the
/// converted signal is what makes the dBTP figure a promise about the file
/// rather than about an intermediate nobody hears.
///
/// The trailing write (as [`write_buffers`]) is then given a config with `resample`
/// cleared: the conversion already happened, and running it twice would resample
/// from a rate the samples are not at.
///
/// # Width
///
/// The measurement runs at the rendered width. EBU R128 meters 1–64 channels,
/// so a surround export is normalized against its own loudness rather than
/// falling back to a different metric — but note that beyond 6 channels the
/// standard channel map marks the extra channels unweighted, so a 7.1 or 7.1.4
/// mix is measured on its first six channels. [`Normalize::Peak`] is unaffected;
/// true peak folds over every channel.
///
/// # Headroom
///
/// For an integer format (`Pcm::Int16`, `Pcm::Int24`) the peak target of
/// [`Normalize::Peak`], and the ceiling of [`Normalize::Lufs`], are pulled down
/// to leave one LSB plus the dither amplitude of headroom below full scale
/// (an LSB for no dither, 1.5 for rectangular, 2 for triangular): a peak
/// normalized to 0 dBTP, plus dither, would be clamped by the quantizer.
/// A target already under the limit is unchanged. [`Written::clipped_samples`]
/// counts what is clamped all the same, and [`Written::normalized`] reports the
/// effective target.
///
/// # Quiet audio
///
/// [`Normalize::Peak`] refuses only exact silence. Near-silence (-100 dBTP, say)
/// is audio with a reading and gets its gain (about +99 dB); read
/// [`Written::normalized`] to see what was applied.
///
/// # Errors
///
/// Everything [`render_to_buffers`] and
/// [`write_buffers`] return, plus [`Error::Unmeasurable`](crate::Error::Unmeasurable)
/// when the audio has no reading to take a gain from (silent, under 400 ms or
/// under the loudness gate for [`Normalize::Lufs`], at the true-peak floor for
/// [`Normalize::Peak`], or a width or rate the meter does not take): `path`
/// is left as it was. [`Error::Resample`](crate::Error::Resample) if the
/// sample-rate conversion fails.
pub fn render_normalized_to_file(
    graph: impl std::borrow::BorrowMut<crate::RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    normalize: Normalize,
    path: impl AsRef<Path>,
) -> Result<Written> {
    render_normalized_to_file_with(graph, config, clock, normalize, path, &mut never_stop)
        .map(crate::finished)
}

/// [`render_normalized_to_file`], reporting to `tick` after each block of
/// both passes: pass 0 renders the graph, pass 1 writes the gained audio
/// ([`Progress::pass`]). Stopping either returns [`Outcome::Cancelled`] with
/// `path` untouched, as [`render_to_file_with`] does; the measurement and
/// the resample between the passes do not tick.
///
/// # Errors
///
/// As [`render_normalized_to_file`].
pub fn render_normalized_to_file_with(
    graph: impl std::borrow::BorrowMut<crate::RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    normalize: Normalize,
    path: impl AsRef<Path>,
    tick: &mut dyn FnMut(Progress) -> Flow,
) -> Result<Outcome<Written>> {
    let path = path.as_ref();
    let mut control = Control::new(Pass::FIRST_OF_TWO, tick);
    let Outcome::Finished(rendered) = crate::render_pass(graph, config, clock, &mut control)?
    else {
        return Ok(Outcome::Cancelled);
    };

    // Convert first, so what is measured is what is written.
    let (mut rendered, config) = match config.resample {
        Some(r) if r.target_rate.get().round() != rendered.sample_rate.get().round() => {
            let converted = crate::process::resample_rendered(&rendered, r)?;
            // The conversion has happened, so the config must stop asking for
            // it — but `render.sample_rate` has to move with it. The header rate
            // is `resample.target_rate` *or* `render.sample_rate`, so clearing
            // one without setting the other writes converted samples under the
            // pre-conversion rate: right audio, wrong speed.
            let mut cfg = config.clone();
            cfg.resample = None;
            cfg.render.sample_rate = converted.sample_rate;
            (converted, std::borrow::Cow::Owned(cfg))
        }
        _ => (rendered, std::borrow::Cow::Borrowed(config)),
    };

    let normalize = normalize.within(&config);
    let gain = normalize.gain_for_rendered(&rendered)?;
    rendered.apply_gain(gain);
    let target = match normalize {
        Normalize::Peak { target } => target,
        Normalize::Lufs { ceiling, .. } => ceiling,
    };

    crate::encode::encode_planes(
        &rendered,
        &config,
        path,
        &mut control.pass(Pass::SECOND_OF_TWO),
    )
    .map(|outcome| {
        outcome.map(|mut written| {
            written.normalized = Some(Normalized { gain, target });
            written
        })
    })
}

/// What a normalizing export did ([`Written::normalized`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Normalized {
    /// The gain applied to the render.
    pub gain: Db,
    /// The effective limit the gain was chosen against, in dBTP: the peak
    /// target of [`Normalize::Peak`], or the ceiling of [`Normalize::Lufs`],
    /// after the headroom an integer format keeps was taken off it (so a 0 dBTP
    /// request on `Int16` reads a little under 0 here).
    pub target: Db,
}

#[cfg(test)]
mod tests;
