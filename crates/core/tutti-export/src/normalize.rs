//! Measure-then-apply normalization, composed over the streaming primitives.
//!
//! Normalization needs the whole signal measured before a gain can be chosen,
//! which is two passes. [`render_to_file`](crate::render_to_file) will not do
//! that — it streams, and holding the signal to pick a gain is exactly what
//! forced the old exporter to buffer everything. So the two-pass path is a
//! *separately named* function: the cost is in the name, not hidden behind a
//! config field a caller sets without noticing.
//!
//! The composition itself is public API elsewhere — a caller who wants to log
//! the reading, gate on it, or apply a gain of its own writes the four steps
//! directly ([`render_to_buffers`](crate::render_to_buffers),
//! [`measure_loudness`](tutti_analysis::measure_loudness),
//! [`Loudness::gain_to`](tutti_analysis::Loudness::gain_to),
//! [`Rendered::apply_gain`](crate::Rendered::apply_gain),
//! [`write_buffers`](crate::write_buffers)). This function exists because every
//! host was otherwise writing the same five lines.

use std::path::Path;

use tutti_analysis::{measure_loudness, LoudnessConfig};
use tutti_core::transport::RenderClock;
use tutti_types::Db;
use tutti_types::Interleaved;

use crate::config::ExportConfig;
use crate::{render_to_buffers, write_buffers, Result, Written};

/// A gain chosen by measuring the rendered signal.
///
/// Both variants measure with the same EBU R128 meter, so both cost the same
/// second pass; they differ only in what they aim at. In particular `Peak`
/// targets **true** peak (4× oversampled, per BS.1770), not sample peak — an
/// inter-sample peak a sample-peak reading misses is exactly what clips on a
/// consumer DAC after resampling.
///
/// # Above six channels
///
/// [`Lufs`](Self::Lufs) is measured through R128's standard channel map, which
/// weights the first six channels (L, R, C, LFE, Ls, Rs) and leaves any beyond
/// them unweighted. A 7.1 or 7.1.4 export is therefore normalized on its first
/// six channels: content that lives only in the rear surrounds or the height
/// layer does not raise the reading, so the gain lands higher than that mix's
/// true loudness warrants — the ceiling is what stops it.
///
/// [`Peak`](Self::Peak) has no such limit; true peak folds over every channel.
/// Prefer it when normalizing wide layouts whose energy is not concentrated in
/// the front six.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Normalize {
    /// Bring the loudest true peak to `target` dBTP.
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
    /// [`Loudness::gain_to`](tutti_analysis::Loudness::gain_to) already
    /// applies).
    fn gain_for(&self, measured: &tutti_analysis::Loudness) -> Db {
        match *self {
            // `Db - Db` is a gain difference, which is why `Db` implements
            // `Sub` — see the `unit_additive!(Db)` note in `units.rs`.
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
    /// two-pass render uses internally, so the two can never disagree.
    ///
    /// # Errors
    ///
    /// [`Error::Unmeasurable`](crate::Error::Unmeasurable) when the meter will
    /// not read this signal:
    /// EBU R128 accepts 1–64 channels at 16 Hz–2.8 MHz, and a `Rendered`
    /// outside that cannot produce a reading. This is an error rather than a
    /// `None` the caller might discard, because the alternative — writing the
    /// file un-normalized and reporting success — is silent: nothing in
    /// [`Written`](crate::Written) records that the gain the caller asked for
    /// was never applied.
    pub fn gain_for_rendered(&self, rendered: &crate::Rendered) -> Result<Db> {
        let meter = LoudnessConfig::new(rendered.sample_rate, rendered.layout());
        let samples = rendered.interleaved();
        measure_loudness(&meter, Interleaved::new(&samples, rendered.layout()))
            .map(|m| self.gain_for(&m))
            .ok_or_else(|| {
                crate::Error::Unmeasurable(format!(
                    "loudness meter rejected {} channels at {} Hz",
                    rendered.channels(),
                    rendered.sample_rate.get()
                ))
            })
    }
}

/// Render `graph`, measure it, apply the resulting gain, and write it to `path`.
///
/// **Two passes.** The whole render is held in memory so a gain can be chosen
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
/// The trailing [`write_buffers`] is then given a config with `resample`
/// cleared: the conversion already happened, and running it twice would resample
/// from a rate the samples are no longer at.
///
/// # Width
///
/// The measurement runs at the rendered width. EBU R128 meters 1–64 channels,
/// so a surround export is normalized against its own loudness rather than
/// falling back to a different metric — but note that beyond 6 channels the
/// standard channel map marks the extra channels unweighted, so a 7.1 or 7.1.4
/// mix is measured on its first six channels. [`Normalize::Peak`] is unaffected;
/// true peak folds over every channel.
pub fn render_normalized_to_file(
    graph: crate::RenderGraph,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    normalize: Normalize,
    path: &Path,
) -> Result<Written> {
    let rendered = render_to_buffers(graph, config, clock)?;

    // Convert first, so what is measured is what is written.
    let (mut rendered, config) = match config.resample {
        Some(r) if r.target_rate.get().round() != rendered.sample_rate.get().round() => {
            let converted = crate::process::resample_rendered(&rendered, r)?;
            // The conversion has happened, so the config must stop asking for
            // it — but `render.sample_rate` has to move with it. The header rate
            // is `resample.target_rate` *or* `render.sample_rate`, so clearing
            // one without setting the other writes converted samples under the
            // pre-conversion rate: right audio, wrong speed.
            let mut cfg = *config;
            cfg.resample = None;
            cfg.render.sample_rate = converted.sample_rate;
            (converted, std::borrow::Cow::Owned(cfg))
        }
        _ => (rendered, std::borrow::Cow::Borrowed(config)),
    };

    rendered.apply_gain(normalize.gain_for_rendered(&rendered)?);

    write_buffers(&rendered, &config, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_analysis::Loudness;

    fn reading(lufs: f32, true_peak: f32) -> Loudness {
        Loudness {
            lufs: Db(lufs),
            true_peak: Db(true_peak),
            range: Db(0.0),
        }
    }

    /// Peak mode targets TRUE peak, not sample peak — the whole reason it goes
    /// through the R128 meter rather than a `fold(max, abs)` over the planes.
    #[test]
    fn peak_moves_the_true_peak_to_the_target() {
        let m = reading(-20.0, -6.0);
        let gain = Normalize::peak(Db(-1.0)).gain_for(&m);
        assert!(
            (gain.get() - 5.0).abs() < 1e-5,
            "−6 dBTP to a −1 dBTP target is +5 dB, got {gain:?}"
        );
    }

    /// Loudness mode defers to `gain_to`, ceiling included.
    #[test]
    fn lufs_respects_the_true_peak_ceiling() {
        // +6 dB would put a −2 dBTP peak at +4, which is 5 dB over a −1 ceiling.
        let m = reading(-20.0, -2.0);
        let gain = Normalize::lufs(Db(-14.0)).gain_for(&m);
        assert!(
            (gain.get() - 1.0).abs() < 1e-5,
            "the ceiling must pull the loudness gain back, got {gain:?}"
        );
    }

    /// `Normalize::lufs` carries the −1.0 dBTP ceiling, so callers do not have
    /// to know the number.
    #[test]
    fn lufs_defaults_to_a_minus_one_dbtp_ceiling() {
        assert_eq!(
            Normalize::lufs(Db(-14.0)),
            Normalize::Lufs {
                target: Db(-14.0),
                ceiling: Db(-1.0)
            }
        );
    }
}
