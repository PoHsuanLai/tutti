//! EBU R128 loudness — integrated LUFS, loudness range, and true peak.
//!
//! The measurement half of normalization. Applying a gain is not analysis: it
//! is `target - measured` and a multiply, which the caller composes (see
//! [`Loudness::gain_to`]). Keeping the two apart is what lets an exporter
//! stream — measure while rendering, apply on a second pass — instead of
//! holding the whole signal to do both at once.
//!
//! `ebur128` is an online meter, so this follows the crate's streaming shape
//! ([`LoudnessConfig`] + [`LoudnessState`] + [`step_loudness`] + [`finish`]),
//! with [`measure_loudness`] folding the same path for a whole buffer.
//!
//! # Interleaved in, unit-typed out
//!
//! `step_loudness` takes **interleaved** frames at `cfg.layout`'s width, the
//! shape a render loop already has. Results come back as [`Db`]: LUFS and dBTP
//! are both decibel readings, and the f32 that `Db` carries is ~3× finer than a
//! 24-bit LSB at any level these reach, so nothing measurable is lost by not
//! keeping them `f64`.

use ebur128::{EbuR128, Mode};
// `SampleRate` is fundsp's, reached through the engine root like `yin.rs` does;
// the rest of the vocabulary comes straight from `tutti-types`.
use tutti_core::SampleRate;
use tutti_types::{ChannelLayout, Db};

/// What the meter is measuring: the rate and channel layout of the frames fed
/// to [`step_loudness`].
///
/// The rate is **not** optional and **not** defaulted. Both true-peak sites in
/// the code this replaced hardcoded 48 kHz while their sibling threaded the
/// real rate, so a 44.1 kHz render measured its peaks through a filter built
/// for the wrong rate and normalized to a biased target. Carrying the rate in
/// the config makes that unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessConfig {
    pub rate: SampleRate,
    pub layout: ChannelLayout,
}

impl LoudnessConfig {
    pub fn new(rate: impl Into<SampleRate>, layout: ChannelLayout) -> Self {
        Self {
            rate: rate.into(),
            layout,
        }
    }
}

/// An EBU R128 reading.
///
/// All three are decibel quantities, so all three are [`Db`] — LUFS and LU are
/// not given their own types. That is a deliberate call: an affine `Lufs`/`Lu`
/// pair would be two new types for three fields, and the one operation anyone
/// performs on them (`target - measured`, giving a gain) is exactly what
/// [`gain_to`](Self::gain_to) already names. Revisit if surround R128 lands and
/// these grow real algebra.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    /// Integrated (gated) loudness, in LUFS.
    pub lufs: Db,
    /// Maximum true peak, in dBTP — 4× oversampled, so it catches
    /// inter-sample peaks a plain sample peak misses.
    pub true_peak: Db,
    /// Loudness range (LRA), in LU.
    pub range: Db,
}

impl Loudness {
    /// The gain that moves this reading to `target`, limited so the result's
    /// true peak does not exceed `ceiling`.
    ///
    /// This is the whole of "normalize", and it is deliberately a *value*
    /// rather than an in-place mutation: the caller multiplies, so the same
    /// reading can gate a decision, be logged, or be applied on a second pass
    /// over a signal that was never held in memory.
    pub fn gain_to(&self, target: Db, ceiling: Db) -> Db {
        let gain = target.get() - self.lufs.get();
        let projected = self.true_peak.get() + gain;
        // Pull back by exactly the overshoot, never push up: a signal already
        // under the ceiling keeps the loudness-derived gain.
        Db(gain - (projected - ceiling.get()).max(0.0))
    }
}

/// The meter's carry between chunks.
///
/// Wraps `EbuR128`, which accumulates internally — there is no partial-frame
/// carry to keep here because [`step_loudness`] only ever feeds it whole
/// frames (it truncates a ragged tail rather than splitting one across calls).
pub struct LoudnessState {
    meter: EbuR128,
}

// `EbuR128` is not `Debug`; report what the meter was built for instead.
impl std::fmt::Debug for LoudnessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoudnessState").finish_non_exhaustive()
    }
}

impl LoudnessState {
    /// Build a meter for `cfg`.
    ///
    /// Returns `None` for a layout `ebur128` cannot meter (zero channels), so
    /// the failure is a value rather than a panic inside a render.
    pub fn new(cfg: &LoudnessConfig) -> Option<Self> {
        let channels = u32::from(cfg.layout.count());
        if channels == 0 {
            return None;
        }
        EbuR128::new(
            channels,
            cfg.rate.get().round() as u32,
            Mode::I | Mode::LRA | Mode::TRUE_PEAK,
        )
        .ok()
        .map(|meter| Self { meter })
    }
}

/// Feed one chunk of **interleaved** frames at `cfg.layout`'s width.
///
/// A ragged tail (a chunk that ends mid-frame) is ignored rather than split:
/// `ebur128` takes whole frames, and silently metering a half frame would skew
/// the reading. Chunks need not align to any block size.
pub fn step_loudness(cfg: &LoudnessConfig, state: &mut LoudnessState, interleaved: &[f32]) {
    let channels = cfg.layout.count() as usize;
    if channels == 0 {
        return;
    }
    let whole = interleaved.len() - interleaved.len() % channels;
    if whole == 0 {
        return;
    }
    // Errors here mean a layout/meter mismatch the constructor already ruled
    // out; a metering miss must not fail a render.
    let _ = state.meter.add_frames_f32(&interleaved[..whole]);
}

/// Read the meter.
///
/// Consumes the state: R128's integrated loudness is a whole-signal answer, and
/// letting a caller read it mid-stream and keep feeding invites treating a
/// partial reading as final.
pub fn finish(state: LoudnessState) -> Loudness {
    let meter = state.meter;
    // `-70` LUFS is R128's absolute gate: below it, the meter reports no
    // integrated loudness because nothing passed the gate. Report the gate
    // rather than an error — silence is a legitimate input.
    let lufs = meter.loudness_global().unwrap_or(-70.0);
    let range = meter.loudness_range().unwrap_or(0.0);

    // True peak is per-channel; the file's peak is the loudest of them.
    let channels = meter.channels();
    let peak_linear = (0..channels)
        .filter_map(|c| meter.true_peak(c).ok())
        .fold(0.0f64, f64::max);

    Loudness {
        lufs: Db(lufs as f32),
        true_peak: linear_to_dbtp(peak_linear),
        range: Db(range as f32),
    }
}

/// Measure a whole interleaved buffer.
///
/// Folds [`step_loudness`] — the same implementation the streaming path uses,
/// so the two can never disagree. `None` when the layout has no channels.
pub fn measure_loudness(cfg: &LoudnessConfig, interleaved: &[f32]) -> Option<Loudness> {
    let mut state = LoudnessState::new(cfg)?;
    step_loudness(cfg, &mut state, interleaved);
    Some(finish(state))
}

/// True-peak amplitude as dBTP, pinned to [`Db::FLOOR`] at silence.
///
/// The floor is `Db::FLOOR` itself rather than a hand-copied `-144.0`: the
/// constant exists precisely to stop the engine from carrying three different
/// silence floors, which it did before.
#[inline]
fn linear_to_dbtp(linear: f64) -> Db {
    if linear > 0.0 {
        Db((20.0 * linear.log10()) as f32)
    } else {
        Db::FLOOR
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// Interleaved stereo sine at `amp`, `secs` long.
    fn sine(rate: f64, secs: f64, freq: f32, amp: f32) -> Vec<f32> {
        let n = (rate * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let s = amp * (TAU * freq * i as f32 / rate as f32).sin();
                [s, s]
            })
            .collect()
    }

    fn cfg(rate: f64) -> LoudnessConfig {
        LoudnessConfig::new(SampleRate(rate), ChannelLayout::Stereo)
    }

    /// The one-shot form must fold the streaming form exactly — the property
    /// `peaks.rs` holds, and the reason a batch/stream drift cannot appear.
    #[test]
    fn streaming_matches_one_shot() {
        let c = cfg(48_000.0);
        let buf = sine(48_000.0, 1.0, 1_000.0, 0.5);

        let one_shot = measure_loudness(&c, &buf).unwrap();

        let mut state = LoudnessState::new(&c).unwrap();
        // Deliberately ragged: 777 is not a multiple of the frame width, so
        // this also exercises the partial-frame guard.
        for chunk in buf.chunks(777) {
            step_loudness(&c, &mut state, chunk);
        }
        let streamed = finish(state);

        assert!(
            (one_shot.lufs.get() - streamed.lufs.get()).abs() < 0.01,
            "one-shot {:?} vs streamed {:?}",
            one_shot.lufs,
            streamed.lufs
        );
    }

    /// The rate is honoured, not assumed. A meter built for the wrong rate
    /// reads a different loudness for the same musical signal — which is the
    /// bug this module's config exists to prevent.
    #[test]
    fn the_configured_rate_is_used() {
        // Same *sample* data interpreted at two rates is a different signal
        // (different frequency, different duration), so the readings differ.
        let buf = sine(44_100.0, 2.0, 1_000.0, 0.5);
        let at_44 = measure_loudness(&cfg(44_100.0), &buf).unwrap();
        let at_48 = measure_loudness(&cfg(48_000.0), &buf).unwrap();
        assert!(
            (at_44.lufs.get() - at_48.lufs.get()).abs() > 1e-4,
            "a meter that ignored its rate would report the same LUFS twice: \
             {at_44:?} vs {at_48:?}"
        );
    }

    /// A −6 dBFS 1 kHz sine reads about −6.7 LUFS. Derived from BS.1770's
    /// definition, not from this implementation:
    ///
    /// ```text
    /// LUFS = -0.691 + 10*log10( Σ_ch G_ch * mean_square_ch )
    /// mean square of a sine of amplitude a = a²/2 = 0.125
    /// stereo, both G = 1.0                  → Σ = 0.25
    /// -0.691 + 10*log10(0.25)               = -6.71 LUFS
    /// ```
    ///
    /// Note the stereo **sum**: two identical channels are ~3 dB louder than
    /// one, which is why this is not the −9.7 a single channel would read.
    /// The tolerance covers K-weighting's small lift at 1 kHz (the shelf is
    /// flat near 0 dB there, but not exactly).
    #[test]
    fn a_known_sine_reads_its_expected_loudness() {
        let buf = sine(48_000.0, 3.0, 1_000.0, 0.5); // −6 dBFS peak
        let m = measure_loudness(&cfg(48_000.0), &buf).unwrap();
        assert!(
            (m.lufs.get() - (-6.71)).abs() < 1.0,
            "expected about −6.7 LUFS, got {:?}",
            m.lufs
        );
        assert!(
            (m.true_peak.get() - (-6.0)).abs() < 0.5,
            "expected about −6 dBTP, got {:?}",
            m.true_peak
        );
    }

    #[test]
    fn silence_floors_rather_than_erroring() {
        let m = measure_loudness(&cfg(48_000.0), &vec![0.0; 4800]).unwrap();
        assert_eq!(m.true_peak, Db::FLOOR);
        assert!(m.lufs.get() <= -70.0, "gated silence, got {:?}", m.lufs);
    }

    /// The gain is the plain difference when the ceiling is not in play.
    #[test]
    fn gain_to_targets_the_requested_loudness() {
        let m = Loudness {
            lufs: Db(-20.0),
            true_peak: Db(-12.0),
            range: Db(0.0),
        };
        // −20 → −14 is +6 dB; the peak lands at −6, under a −1 ceiling.
        assert!((m.gain_to(Db(-14.0), Db(-1.0)).get() - 6.0).abs() < 1e-5);
    }

    /// …and is pulled back by exactly the overshoot when it is.
    #[test]
    fn gain_to_respects_the_true_peak_ceiling() {
        let m = Loudness {
            lufs: Db(-20.0),
            true_peak: Db(-2.0),
            range: Db(0.0),
        };
        // +6 dB would put the peak at +4, which is 5 dB over a −1 ceiling.
        assert!((m.gain_to(Db(-14.0), Db(-1.0)).get() - 1.0).abs() < 1e-5);
    }

    /// A signal already under the ceiling must not be pushed *up* to meet it.
    #[test]
    fn gain_to_never_amplifies_to_reach_the_ceiling() {
        let m = Loudness {
            lufs: Db(-20.0),
            true_peak: Db(-40.0),
            range: Db(0.0),
        };
        assert!((m.gain_to(Db(-20.0), Db(-1.0)).get()).abs() < 1e-5);
    }
}
