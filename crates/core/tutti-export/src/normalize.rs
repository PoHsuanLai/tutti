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

use crate::config::ExportConfig;
use crate::{render_to_buffers, write_buffers, Result, Written};

/// A gain chosen by measuring the rendered signal.
///
/// Both variants measure with the same EBU R128 meter, so both cost the same
/// second pass; they differ only in what they aim at. In particular `Peak`
/// targets **true** peak (4× oversampled, per BS.1770), not sample peak — an
/// inter-sample peak a sample-peak reading misses is exactly what clips on a
/// consumer DAC after resampling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Normalize {
    /// Bring the loudest true peak to `target` dBTP.
    Peak { target: Db },
    /// Bring integrated loudness to `target` LUFS, pulled back so the true peak
    /// does not exceed `ceiling` dBTP.
    Lufs { target: Db, ceiling: Db },
}

impl Normalize {
    /// Loudness normalization with a −1.0 dBTP ceiling — the value streaming
    /// platforms ask for, and the one the old builder's `lufs()` hardcoded.
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
            Self::Peak { target } => Db(target.get() - measured.true_peak.get()),
            Self::Lufs { target, ceiling } => measured.gain_to(target, ceiling),
        }
    }
}

/// Render `net`, measure it, apply the resulting gain, and write it to `path`.
///
/// **Two passes.** The whole render is held in memory so a gain can be chosen
/// from it. Use [`render_to_file`](crate::render_to_file) when no gain is
/// needed — it streams and holds nothing.
///
/// The measurement runs at the rendered width: R128 meters any channel count,
/// so a surround export is normalized against its own loudness rather than
/// silently falling back to a different metric.
///
/// A signal the meter cannot read (no channels) is written through unchanged
/// rather than failing the export — the render succeeded, and refusing to write
/// it would lose it.
pub fn render_normalized_to_file(
    net: tutti_core::dsp::Net,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    normalize: Normalize,
    path: &Path,
) -> Result<Written> {
    let mut rendered = render_to_buffers(net, config, clock)?;

    let meter = LoudnessConfig::new(rendered.sample_rate, config.encode.channels);
    if let Some(measured) = measure_loudness(&meter, &rendered.interleaved()) {
        rendered.apply_gain(normalize.gain_for(&measured));
    }

    write_buffers(&rendered, config, path)
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

    /// `Normalize::lufs` carries the −1.0 dBTP ceiling the old builder
    /// hardcoded, so callers do not have to know the number.
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
