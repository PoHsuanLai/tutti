//! EBU R128 loudness — integrated LUFS, loudness range, and true peak, and
//! the momentary and short-term readings of a running meter.
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
//! # A running meter
//!
//! [`step_windowed_loudness`] answers a different question from [`finish`]: not
//! "how loud was the whole signal" but "how loud is it now", as a
//! [`WindowedReading`] after every chunk. Momentary loudness is the last 400 ms
//! and short-term the last 3 s, both K-weighted and ungated (EBU Tech 3341).
//! The true peak is the largest seen since the meter was built or
//! [reset](WindowedLoudnessState::reset).
//!
//! # Interleaved in, unit-typed out
//!
//! `step_loudness` takes an [`Interleaved`] — the shape a render loop already
//! has, now carrying its own width instead of borrowing `cfg.layout`'s by
//! convention. Results come back as [`Db`]: LUFS and dBTP
//! are both decibel readings, and the f32 that `Db` carries is ~3× finer than a
//! 24-bit LSB at any level these reach, so nothing measurable is lost by not
//! keeping them `f64`.

use ebur128::{EbuR128, Mode};
use tutti_types::{ChannelLayout, Db, Interleaved, SampleRate};

use crate::{Error, Result};

/// What the meter is measuring: the rate and channel layout of the frames fed
/// to [`step_loudness`].
///
/// The rate is **not** optional and **not** defaulted. Hardcode it to 48 kHz
/// and a 44.1 kHz render measures its peaks through a filter built for the
/// wrong rate and normalizes to a biased target. Carrying the rate in the
/// config makes that unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessConfig {
    /// Rate the fed frames are denominated at. Sets the true-peak
    /// oversampling filter, so a wrong value biases the reading silently.
    pub rate: SampleRate,
    /// Channel layout of the fed frames. Fixed at meter construction — a chunk
    /// of a different width is read as a different signal.
    pub layout: ChannelLayout,
}

impl LoudnessConfig {
    /// A config for frames at `rate` in `layout`.
    pub fn new(rate: impl Into<SampleRate>, layout: ChannelLayout) -> Self {
        Self {
            rate: rate.into(),
            layout,
        }
    }

    /// Whether `chunk` is the width this config's meter was built for.
    ///
    /// The meter's channel count is fixed at construction, so a chunk of a
    /// different width is split into the wrong number of frames and read as a
    /// different signal. Expressible only because the width travels with the
    /// buffer — a bare slice leaves the config's layout merely assumed.
    #[inline]
    pub fn chunk_matches(&self, chunk: Interleaved<'_>) -> bool {
        chunk.layout() == self.layout
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
    /// Maximum true peak, in dBTP. The meter oversamples 4× below 96 kHz and
    /// 2× from 96 kHz to below 192 kHz, so it catches inter-sample peaks a
    /// plain sample peak misses; at 192 kHz and above it is the sample peak.
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
/// Wraps `EbuR128`, which accumulates internally, plus the samples of a frame
/// a chunk ended in the middle of: [`step_loudness`] feeds the meter whole
/// frames only, and splices that carried part frame in front of the next
/// chunk, as [`step_peaks`](crate::step_peaks) does.
pub struct LoudnessState {
    meter: EbuR128,
    /// The leading samples of a frame the last chunk did not finish (fewer
    /// than the layout's channel count).
    partial_frame: Vec<f32>,
}

// `EbuR128` is not `Debug`; report what the meter was built for instead.
impl std::fmt::Debug for LoudnessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoudnessState").finish_non_exhaustive()
    }
}

impl LoudnessState {
    /// Builds a meter for `cfg`.
    ///
    /// Returns `None` for a layout `ebur128` cannot meter (zero channels), so
    /// the failure is a value rather than a panic inside a render.
    #[must_use]
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
        .map(|meter| Self {
            meter,
            partial_frame: Vec::new(),
        })
    }
}

/// Feeds one chunk of frames.
///
/// The chunk arrives as an [`Interleaved`], so its width travels with it rather
/// than beside it — as a separate `cfg.layout` and a bare buffer, the two have
/// to agree and nothing checks that they do. The meter is built from
/// `cfg.layout`, so a chunk at a different width is metered as the wrong number
/// of frames — [`chunk_matches`](LoudnessConfig::chunk_matches)
/// makes that a value the caller can act on. Here a mismatch is simply ignored,
/// because a metering miss must not fail a render.
///
/// A ragged tail (a chunk that ends mid-frame) is carried, not dropped: its
/// samples are spliced in front of the next chunk, so the meter only ever sees
/// whole frames and every later sample stays on its own channel. Dropping it
/// instead would shift each later sample onto the wrong channel. Chunks need
/// not align to frames or to any block size.
pub fn step_loudness(cfg: &LoudnessConfig, state: &mut LoudnessState, chunk: Interleaved<'_>) {
    if !cfg.chunk_matches(chunk) {
        return;
    }
    // Errors from the meter mean a layout/meter mismatch the guard above
    // already ruled out; a metering miss must not fail a render.
    if state.partial_frame.is_empty() {
        // The common case, a frame-aligned chunk, meters in place. `len()` is
        // frames; `window` turns the whole-frame prefix into samples.
        let whole = chunk.window(0..chunk.len());
        if !whole.is_empty() {
            let _ = state.meter.add_frames_f32(whole.samples());
        }
        state
            .partial_frame
            .extend_from_slice(&chunk.samples()[chunk.len() * chunk.stride()..]);
    } else {
        let mut carry = core::mem::take(&mut state.partial_frame);
        carry.extend_from_slice(chunk.samples());
        let spliced = Interleaved::new(&carry, chunk.layout());
        let consumed = spliced.len() * spliced.stride();
        if consumed > 0 {
            let _ = state.meter.add_frames_f32(&carry[..consumed]);
        }
        carry.drain(..consumed);
        state.partial_frame = carry;
    }
}

/// Reads the meter.
///
/// Consumes the state: R128's integrated loudness is a whole-signal answer, and
/// letting a caller read it mid-stream and keep feeding invites treating a
/// partial reading as final.
pub fn finish(state: LoudnessState) -> Loudness {
    let meter = state.meter;
    // `-70` LUFS is R128's absolute gate. The meter reports `-inf` (not an
    // error) when nothing passed it — silence, or simply an input shorter than
    // the 400 ms gating block, which a short render legitimately is. Clamping to
    // the gate keeps the reading finite, because an infinite loudness poisons
    // every gain derived from it into `NaN`.
    let lufs = meter.loudness_global().unwrap_or(f64::NEG_INFINITY);
    let lufs = if lufs.is_finite() { lufs } else { -70.0 };
    let range = meter.loudness_range().unwrap_or(0.0);
    let range = if range.is_finite() { range } else { 0.0 };

    // True peak is per-channel; the file's peak is the loudest of them.
    let channels = meter.channels();
    let peak_linear = (0..channels)
        .filter_map(|c| meter.true_peak(c).ok())
        .fold(0.0f64, f64::max);

    Loudness {
        lufs: Db(lufs as f32),
        true_peak: true_peak_db(peak_linear),
        range: Db(range as f32),
    }
}

/// An `f64` true-peak amplitude as dBTP, pinning silence at [`Db::FLOOR`].
///
/// [`Db::from_amplitude`] leaves silence at `-inf` because the floor belongs to
/// the consumer; this is that consumer. An infinite peak would reach
/// [`Loudness::gain_to`], which subtracts it into a `NaN` — the same hazard the
/// `-70` LUFS gate guards against.
///
/// Not routed through [`tutti_types::Amplitude`], which is `f32`: converting first narrows
/// the input before the `log10`, defeating the reason this path is `f64`. The
/// arithmetic is [`Db::from_amplitude`]'s, at the width this boundary holds.
#[inline]
fn true_peak_db(peak_linear: f64) -> Db {
    if peak_linear <= 0.0 {
        Db::FLOOR
    } else {
        Db((20.0 * peak_linear.log10()) as f32)
    }
}

/// The slowest and fastest rate a windowed meter is built for. The window the
/// meter keeps is three seconds of every channel, so the upper bound caps its
/// allocation.
const WINDOWED_RATES: core::ops::RangeInclusive<f64> = 8_000.0..=192_000.0;

/// The most channels a windowed meter is built for (the same cap, for the same
/// reason).
const WINDOWED_MAX_CHANNELS: u16 = 16;

/// What a [`WindowedLoudnessState`] meters: the rate and layout of the frames fed
/// to [`step_windowed_loudness`]. Validated on construction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowedLoudnessConfig {
    rate: SampleRate,
    layout: ChannelLayout,
}

impl WindowedLoudnessConfig {
    /// A config for frames at `rate` in `layout`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidLoudnessParameter`] unless `rate` is finite and within
    /// 8 kHz..=192 kHz, and `layout` has 1..=16 channels.
    pub fn new(rate: impl Into<SampleRate>, layout: ChannelLayout) -> Result<Self> {
        let rate = rate.into();
        if !WINDOWED_RATES.contains(&rate.get()) {
            return Err(Error::InvalidLoudnessParameter(
                "rate: outside 8000..=192000 Hz",
            ));
        }
        if !(1..=WINDOWED_MAX_CHANNELS).contains(&layout.count()) {
            return Err(Error::InvalidLoudnessParameter(
                "layout: outside 1..=16 channels",
            ));
        }
        Ok(Self { rate, layout })
    }

    /// The rate the fed frames are denominated at.
    #[inline]
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The channel layout of the fed frames.
    #[inline]
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Frames in `tenths` hundred-millisecond units, as the meter counts them.
    fn window_frames(&self, tenths: u64) -> u64 {
        ((self.rate.get().round() as u64 + 5) / 10) * tenths
    }
}

/// A running meter's reading.
///
/// All three are decibel readings, so all three are [`Db`] (see
/// [`Loudness`] for why LUFS has no type of its own).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowedReading {
    /// Loudness of the last 400 ms, in LUFS. [`LOUDNESS_FLOOR`](Self::LOUDNESS_FLOOR)
    /// until 400 ms have been fed.
    pub momentary: Db,
    /// Loudness of the last 3 s, in LUFS. [`LOUDNESS_FLOOR`](Self::LOUDNESS_FLOOR)
    /// until 3 s have been fed.
    pub short_term: Db,
    /// The largest true peak since the meter was built or reset, in dBTP.
    /// [`Db::FLOOR`] for silence.
    pub true_peak: Db,
}

impl WindowedReading {
    /// The lowest loudness a reading reports: a clamp at BS.1770's `-70` LUFS
    /// absolute gate level, the same one [`finish`] applies.
    ///
    /// Silence, a signal quieter than this and a window that is not yet full
    /// all read it, and only
    /// [`frames_seen`](WindowedLoudnessState::frames_seen) tells them apart.
    /// A finite floor keeps every gain derived from a reading finite.
    pub const LOUDNESS_FLOOR: Db = Db(-70.0);
}

/// The running meter's carry between chunks.
pub struct WindowedLoudnessState {
    cfg: WindowedLoudnessConfig,
    meter: EbuR128,
    /// The leading samples of a frame the last chunk did not finish. Its
    /// capacity is one frame, so it never grows.
    partial_frame: Vec<f32>,
    /// Whole frames fed since construction or the last reset.
    frames: u64,
}

// `EbuR128` is not `Debug`.
impl std::fmt::Debug for WindowedLoudnessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowedLoudnessState")
            .field("cfg", &self.cfg)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl WindowedLoudnessState {
    /// Builds a meter for `cfg`. Allocates its three-second window here: at
    /// most 3 s of `f64` samples of every channel, 192 kHz x 16 channels x 3 s x
    /// 8 bytes, about 74 MB, and far less at ordinary rates and widths.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidLoudnessParameter`] if the meter cannot be built.
    pub fn new(cfg: &WindowedLoudnessConfig) -> Result<Self> {
        let channels = cfg.layout.count();
        let meter = EbuR128::new(
            u32::from(channels),
            cfg.rate.get().round() as u32,
            Mode::M | Mode::S | Mode::TRUE_PEAK,
        )
        .map_err(|_| Error::InvalidLoudnessParameter("meter: could not be built"))?;
        Ok(Self {
            cfg: *cfg,
            meter,
            partial_frame: Vec::with_capacity(usize::from(channels)),
            frames: 0,
        })
    }

    /// Forgets everything fed: the windows read the floor again and the peak
    /// is cleared. Keeps the buffers.
    pub fn reset(&mut self) {
        self.meter.reset();
        self.partial_frame.clear();
        self.frames = 0;
    }

    /// Whole frames fed since construction or the last reset.
    #[inline]
    #[must_use]
    pub fn frames_seen(&self) -> u64 {
        self.frames
    }

    fn feed(&mut self, samples: &[f32]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        self.meter
            .add_frames_f32(samples)
            .map_err(|_| Error::InvalidLoudnessParameter("meter: rejected the frames"))?;
        self.frames += (samples.len() / usize::from(self.cfg.layout.count())) as u64;
        Ok(())
    }
}

/// A window's loudness: the floor until `needed` frames have been fed, and for
/// a reading the meter could not give or that is not finite (silence is `-inf`);
/// otherwise the reading, not under the floor.
fn windowed_db(frames: u64, needed: u64, lufs: Option<f64>) -> Db {
    match lufs {
        Some(lufs) if frames >= needed && lufs.is_finite() => {
            Db((lufs as f32).max(WindowedReading::LOUDNESS_FLOOR.get()))
        }
        _ => WindowedReading::LOUDNESS_FLOOR,
    }
}

/// Feeds one chunk and reads the meter.
///
/// A chunk may end mid-frame: the part frame is carried and completed by the
/// next chunk, so the reading does not depend on how the stream is cut. A
/// chunk may be empty.
///
/// A step allocates nothing once the state is built (the meter's `add_frames_f32`
/// and its momentary, short-term and true-peak getters, counted in
/// `tests/windowed_loudness_alloc.rs` for the locked `ebur128`), but it is not a realtime-safe call: each
/// reads both windows over their whole length, three seconds of every channel
/// for the short-term one, so its cost does not shrink with the chunk. Run it on
/// a consumer thread that drains a graph tap's `TapReader`, once per display tick, not in
/// an audio callback.
///
/// # Errors
///
/// [`Error::InvalidLoudnessParameter`] when `state` was built for another
/// config or `chunk` is not `cfg`'s width; [`Error::NonFiniteInput`] for a NaN
/// or infinite sample. The state is untouched on error.
pub fn step_windowed_loudness(
    cfg: &WindowedLoudnessConfig,
    state: &mut WindowedLoudnessState,
    chunk: Interleaved<'_>,
) -> Result<WindowedReading> {
    if state.cfg != *cfg {
        return Err(Error::InvalidLoudnessParameter(
            "state: built for another config",
        ));
    }
    if chunk.layout() != cfg.layout {
        return Err(Error::InvalidLoudnessParameter(
            "chunk: width differs from the config's layout",
        ));
    }
    let samples = chunk.samples();
    if samples.iter().any(|s| !s.is_finite()) {
        return Err(Error::NonFiniteInput);
    }

    let stride = chunk.stride();
    let mut rest = samples;
    if !state.partial_frame.is_empty() {
        let take = (stride - state.partial_frame.len()).min(rest.len());
        state.partial_frame.extend_from_slice(&rest[..take]);
        rest = &rest[take..];
        if state.partial_frame.len() == stride {
            let frame = core::mem::take(&mut state.partial_frame);
            let fed = state.feed(&frame);
            state.partial_frame = frame;
            state.partial_frame.clear();
            fed?;
        }
    }
    let whole = rest.len() / stride * stride;
    state.feed(&rest[..whole])?;
    state.partial_frame.extend_from_slice(&rest[whole..]);

    let peak = (0..u32::from(cfg.layout.count()))
        .filter_map(|c| state.meter.true_peak(c).ok())
        .fold(0.0f64, f64::max);
    Ok(WindowedReading {
        momentary: windowed_db(
            state.frames,
            cfg.window_frames(4),
            state.meter.loudness_momentary().ok(),
        ),
        short_term: windowed_db(
            state.frames,
            cfg.window_frames(30),
            state.meter.loudness_shortterm().ok(),
        ),
        true_peak: true_peak_db(peak),
    })
}

/// Measures a whole buffer.
///
/// Folds [`step_loudness`] — the same implementation the streaming path uses,
/// so the two can never disagree. `None` when the layout has no channels.
pub fn measure_loudness(cfg: &LoudnessConfig, buffer: Interleaved<'_>) -> Option<Loudness> {
    let mut state = LoudnessState::new(cfg)?;
    step_loudness(cfg, &mut state, buffer);
    Some(finish(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;
    use tutti_types::Amplitude;

    /// The true-peak floor lives at this boundary, not in `Db::from_amplitude`.
    #[test]
    fn true_peak_pins_silence_and_stays_wide() {
        // The shared conversion does not floor; this boundary does.
        assert!(Db::from_amplitude(Amplitude::SILENT).get().is_infinite());
        assert_eq!(true_peak_db(0.0), Db::FLOOR);
        assert!(true_peak_db(0.0).get().is_finite());

        // Agrees with the shared conversion everywhere above silence.
        let want = Db::from_amplitude(Amplitude(0.5));
        assert!((true_peak_db(0.5).get() - want.get()).abs() < 1e-5);
        assert!((true_peak_db(1.0).get() - 0.0).abs() < 1e-6);

        // Computed at f64 width: narrowing to `Amplitude` (f32) before the
        // log10 is the one thing this path exists to avoid.
        let tiny = 1.0e-30_f64;
        assert!(
            (true_peak_db(tiny).get() - -600.0).abs() < 1.0,
            "f64 input must survive the log10, got {:?}",
            true_peak_db(tiny)
        );
    }

    /// An infinite true peak poisons every gain derived from it — the same
    /// reason `lufs` is clamped to the `-70` gate.
    #[test]
    fn silent_render_yields_a_finite_normalization_gain() {
        let m = Loudness {
            lufs: Db(-70.0),
            true_peak: true_peak_db(0.0),
            range: Db(0.0),
        };
        assert!(m.gain_to(Db(-14.0), Db(-1.0)).get().is_finite());
    }

    /// Interleaved stereo sine at `amp`, `secs` long.
    fn sine(rate: f64, secs: f64, freq: f32, amp: f32) -> Vec<f32> {
        let n = (rate * secs) as usize;
        tutti_fixtures::signal::sine(
            tutti_types::Hz(freq),
            tutti_types::Amplitude(amp),
            SampleRate(rate),
            tutti_types::Samples(n),
        )
        .into_iter()
        .flat_map(|s| [s, s])
        .collect()
    }

    fn cfg(rate: f64) -> LoudnessConfig {
        LoudnessConfig::new(SampleRate(rate), ChannelLayout::STEREO)
    }

    /// A chunk that ends mid-frame carries its part frame into the next one.
    ///
    /// A 5.1 signal on the LFE channel alone, which R128 does not weight
    /// (`ebur128` maps channel 3 to `Unused`), reads as silence. Fed in
    /// ragged chunks (not a multiple of six), a meter that dropped each
    /// ragged tail would start the next chunk on the wrong channel, move the
    /// LFE onto a weighted one, and read it loud.
    ///
    /// Two chunk sizes: 777 leaves a 3-sample tail that the next chunk always
    /// completes exactly (the spliced carry ends empty), and 775 leaves
    /// tails cycling through 1..=5, so a carry that spans a chunk and keeps a
    /// remainder of its own is exercised too.
    #[test]
    fn a_ragged_chunk_carries_its_part_frame() {
        let layout = ChannelLayout::from(6u16);
        let c = LoudnessConfig::new(SampleRate(48_000.0), layout);
        let n = 48_000;
        let buf: Vec<f32> = (0..n)
            .flat_map(|i| {
                let s = 0.5 * (TAU * 1_000.0 * i as f32 / 48_000.0).sin();
                [0.0, 0.0, 0.0, s, 0.0, 0.0]
            })
            .collect();

        let one_shot = measure_loudness(&c, Interleaved::new(&buf, layout)).unwrap();
        assert_eq!(one_shot.lufs, Db(-70.0), "the LFE alone is unweighted");

        for size in [777, 775] {
            let mut state = LoudnessState::new(&c).unwrap();
            for chunk in buf.chunks(size) {
                step_loudness(&c, &mut state, Interleaved::new(chunk, layout));
            }
            let streamed = finish(state);
            assert_eq!(
                streamed.lufs, one_shot.lufs,
                "{size}-sample chunks read {:?}: a dropped part frame moved the LFE \
                 onto a weighted channel",
                streamed.lufs
            );
        }
    }

    /// The one-shot form must fold the streaming form exactly — the property
    /// `peaks.rs` holds, and the reason a batch/stream drift cannot appear.
    #[test]
    fn streaming_matches_one_shot() {
        let c = cfg(48_000.0);
        let buf = sine(48_000.0, 1.0, 1_000.0, 0.5);

        let one_shot = measure_loudness(&c, Interleaved::new(&buf, ChannelLayout::STEREO)).unwrap();

        let mut state = LoudnessState::new(&c).unwrap();
        // Deliberately ragged: 777 is not a multiple of the frame width, so
        // this also exercises the part-frame carry.
        for chunk in buf.chunks(777) {
            step_loudness(
                &c,
                &mut state,
                Interleaved::new(chunk, ChannelLayout::STEREO),
            );
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
        let stereo = Interleaved::new(&buf, ChannelLayout::STEREO);
        let at_44 = measure_loudness(&cfg(44_100.0), stereo).unwrap();
        let at_48 = measure_loudness(&cfg(48_000.0), stereo).unwrap();
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
    fn silence_floors_rather_than_erroring() {
        let silence = vec![0.0; 4800];
        let m = measure_loudness(
            &cfg(48_000.0),
            Interleaved::new(&silence, ChannelLayout::STEREO),
        )
        .unwrap();
        assert_eq!(m.true_peak, Db::FLOOR);
        assert!(m.lufs.get() <= -70.0, "gated silence, got {:?}", m.lufs);
    }

    /// Every reading must be finite, even when nothing passes R128's gate.
    ///
    /// `ebur128` returns `Ok(-inf)` rather than an error for an input shorter
    /// than the 400 ms gating block — which a short render legitimately is. An
    /// infinite LUFS silently poisons `gain_to` into `NaN`, and a `NaN` gain
    /// multiplies a whole render into `NaN`. Caught by an export test composing
    /// normalization over a 0.2 s buffer.
    #[test]
    fn a_reading_is_always_finite_even_below_the_gate() {
        // 100 ms — a quarter of the gating block.
        let short = sine(48_000.0, 0.1, 1_000.0, 0.5);
        let m = measure_loudness(
            &cfg(48_000.0),
            Interleaved::new(&short, ChannelLayout::STEREO),
        )
        .unwrap();
        assert!(
            m.lufs.get().is_finite(),
            "LUFS must be finite, got {:?}",
            m.lufs
        );
        assert!(m.range.get().is_finite());
        assert!(
            m.gain_to(Db(-14.0), Db(-1.0)).get().is_finite(),
            "a gain derived from a sub-gate reading must not be NaN"
        );
    }

    /// A chunk whose own width disagrees with the meter's is skipped, not fed.
    ///
    /// The buffer carries its width, so the disagreement is detectable. Fed
    /// anyway, a quad chunk handed to a stereo meter would be split into twice
    /// as many frames of the wrong signal and silently folded into the
    /// integrated reading.
    #[test]
    fn a_chunk_of_the_wrong_width_is_not_metered() {
        let c = cfg(48_000.0);
        let buf = sine(48_000.0, 1.0, 1_000.0, 0.5);

        let mut state = LoudnessState::new(&c).unwrap();
        step_loudness(&c, &mut state, Interleaved::new(&buf, ChannelLayout::QUAD));
        let m = finish(state);

        assert_eq!(
            m.true_peak,
            Db::FLOOR,
            "a mismatched chunk must not reach the meter, got {:?}",
            m.true_peak
        );
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

    /// Read-out tolerance for a steady tone, in LU: EBU Tech 3341's `±0.1 LU`
    /// for a conforming meter. The truncation of a 997 Hz tone to a window that
    /// is not a whole number of cycles adds at most
    /// `10 log10(1 + 1/(4 pi f T))` = 0.001 dB for the 400 ms window, so the
    /// published figure is the whole budget.
    const WINDOW_TOL: f32 = 0.1;

    /// Stereo (identical channels) 997 Hz sine, amplitude `amp(t)` at `t`
    /// seconds, with a continuous phase.
    fn windowed_sine(rate: f64, secs: f64, amp: impl Fn(f64) -> f64) -> Vec<f32> {
        let n = (rate * secs).round() as usize;
        (0..n)
            .flat_map(|i| {
                let t = i as f64 / rate;
                let s = (amp(t) * (std::f64::consts::TAU * 997.0 * t).sin()) as f32;
                [s, s]
            })
            .collect()
    }

    /// The reading for identical channels of amplitude `a`: BS.1770 gives
    /// `-0.691 + 10 log10(sum_ch G * a^2 / 2)`, and the K-weighting's gain at
    /// 997 Hz is the `+0.691` dB that offset cancels, so two channels read
    /// `10 log10(a^2) = 20 log10(a)`.
    fn stereo_lufs(a: f64) -> f32 {
        (20.0 * a.log10()) as f32
    }

    fn windowed_cfg(rate: f64) -> WindowedLoudnessConfig {
        WindowedLoudnessConfig::new(SampleRate(rate), ChannelLayout::STEREO).unwrap()
    }

    /// Feeds `buf` in `frames`-frame chunks, returning the reading after each.
    fn windowed_run(
        cfg: &WindowedLoudnessConfig,
        buf: &[f32],
        frames: usize,
    ) -> Vec<WindowedReading> {
        let mut state = WindowedLoudnessState::new(cfg).unwrap();
        buf.chunks(frames * 2)
            .map(|chunk| {
                step_windowed_loudness(
                    cfg,
                    &mut state,
                    Interleaved::new(chunk, ChannelLayout::STEREO),
                )
                .unwrap()
            })
            .collect()
    }

    /// A -20 dBFS stereo 997 Hz sine reads -20.0 LUFS in both windows, and
    /// each reads the floor until exactly its own window of frames is in.
    #[test]
    fn a_sine_reads_its_loudness_once_each_window_fills() {
        let want = stereo_lufs(0.1);
        assert!((want - -20.0).abs() < 1e-5);
        for rate in [44_100.0, 48_000.0] {
            let cfg = windowed_cfg(rate);
            let block = (rate / 100.0) as usize; // 10 ms
            let tone = windowed_sine(rate, 3.5, |_| 0.1);
            let r = windowed_run(&cfg, &tone, block);
            let at = |ms: usize| r[ms / 10 - 1];

            assert_eq!(at(390).momentary, WindowedReading::LOUDNESS_FLOOR, "{rate}");
            assert!(
                (at(400).momentary.get() - want).abs() < WINDOW_TOL,
                "{rate}: momentary {:?}",
                at(400).momentary
            );
            assert_eq!(
                at(2990).short_term,
                WindowedReading::LOUDNESS_FLOOR,
                "{rate}"
            );
            assert!(
                (at(3000).short_term.get() - want).abs() < WINDOW_TOL,
                "{rate}: short-term {:?}",
                at(3000).short_term
            );
            // The peak of an amplitude-0.1 tone is 20 log10(0.1) dBTP, a figure of
            // its own; the 4x oversampled meter adds under 0.3 dB, as in
            // `true_peak_is_the_signals_amplitude`.
            let peak_want = (20.0 * 0.1f64.log10()) as f32;
            assert!((at(3500).true_peak.get() - peak_want).abs() < 0.3);
        }
    }

    /// A 10 dB step (10x the power) moves momentary loudness along
    /// `10 log10(1 + 9 min(t/0.4, 1))` and short-term along the same with
    /// `t/3`: the windows are linear in power, so the share of the window past
    /// the step is `t / window`. Each is settled at its window and not before.
    #[test]
    fn a_ten_db_step_settles_after_each_window() {
        let rate = 48_000.0;
        let cfg = windowed_cfg(rate);
        let (before, step_at) = (0.05, 4.0);
        let after = before * 10f64.sqrt();
        let signal = windowed_sine(rate, step_at + 4.0, |t| {
            if t < step_at { before } else { after }
        });
        let r = windowed_run(&cfg, &signal, 480);
        let at = |t: f64| r[((step_at + t) * 100.0).round() as usize - 1];

        let (low, high) = (stereo_lufs(before), stereo_lufs(after));
        assert!((high - low - 10.0).abs() < 1e-4);
        assert!((at(0.0).momentary.get() - low).abs() < WINDOW_TOL);
        assert!((at(0.0).short_term.get() - low).abs() < WINDOW_TOL);

        let ramp = |t: f64, window: f64| {
            f64::from(low) + 10.0 * (1.0 + 9.0 * (t / window).min(1.0)).log10()
        };
        for t in [0.1, 0.2, 0.3, 0.4, 0.5, 1.0] {
            let got = at(t).momentary.get();
            assert!(
                (got - ramp(t, 0.4) as f32).abs() < WINDOW_TOL,
                "momentary +{t}s: {got}"
            );
        }
        for t in [0.5, 1.0, 2.0, 3.0, 3.5, 4.0] {
            let got = at(t).short_term.get();
            assert!(
                (got - ramp(t, 3.0) as f32).abs() < WINDOW_TOL,
                "short-term +{t}s: {got}"
            );
        }
        // Settled at the window, and visibly not before it.
        assert!((at(0.4).momentary.get() - high).abs() < WINDOW_TOL);
        assert!(at(0.3).momentary.get() < high - 1.0);
        assert!((at(3.0).short_term.get() - high).abs() < WINDOW_TOL);
        assert!(at(2.0).short_term.get() < high - 1.0);
    }

    /// Silence reads the floor in both windows and a floor peak; so does a
    /// window that signal has left, once the K-weighting's ring-out is under
    /// the floor.
    #[test]
    fn silence_reads_the_floor() {
        let cfg = windowed_cfg(48_000.0);
        let silence = vec![0.0f32; 48_000 * 2 * 4];
        let last = *windowed_run(&cfg, &silence, 4800).last().unwrap();
        assert_eq!(last.momentary, WindowedReading::LOUDNESS_FLOOR);
        assert_eq!(last.short_term, WindowedReading::LOUDNESS_FLOOR);
        assert_eq!(last.true_peak, Db::FLOOR);

        let mut left_behind = windowed_sine(48_000.0, 4.0, |_| 0.1);
        left_behind.extend(vec![0.0f32; 48_000 * 2]);
        let r = windowed_run(&cfg, &left_behind, 480);
        let last = r.last().unwrap();
        assert_eq!(last.momentary, WindowedReading::LOUDNESS_FLOOR);
        assert!(last.short_term.get() > -30.0, "{:?}", last.short_term);
        assert!(last.true_peak.get() > -21.0, "the peak is held");
    }

    /// The readings do not depend on how the stream is cut: 64-frame,
    /// 4096-frame and ragged (777-sample, so mid-frame) chunks agree bit for
    /// bit wherever they have fed the same frames.
    #[test]
    fn readings_do_not_depend_on_the_chunking() {
        let cfg = windowed_cfg(48_000.0);
        // A level change and a quiet stretch, so the windows are not trivial.
        let signal = windowed_sine(48_000.0, 5.0, |t| {
            if t < 2.0 {
                0.2
            } else if t < 3.0 {
                0.01
            } else {
                0.4
            }
        });
        let fine = windowed_run(&cfg, &signal, 64);
        let coarse = windowed_run(&cfg, &signal, 4096);
        let total = signal.len() / 2;
        for (i, c) in coarse.iter().enumerate() {
            let frames = ((i + 1) * 4096).min(total);
            if frames % 64 == 0 {
                assert_eq!(*c, fine[frames / 64 - 1], "after {frames} frames");
            }
        }
        assert_eq!(coarse.last(), fine.last());

        let mut state = WindowedLoudnessState::new(&cfg).unwrap();
        let mut ragged = None;
        for chunk in signal.chunks(777) {
            ragged = Some(
                step_windowed_loudness(
                    &cfg,
                    &mut state,
                    Interleaved::new(chunk, ChannelLayout::STEREO),
                )
                .unwrap(),
            );
        }
        assert_eq!(ragged.as_ref(), fine.last());
    }

    /// A part frame is carried onto the right channel: a 5.1 signal on the LFE
    /// alone, which R128 does not weight, reads the floor however it is cut.
    #[test]
    fn a_ragged_chunk_keeps_channels_in_place() {
        let layout = ChannelLayout::from(6u16);
        let cfg = WindowedLoudnessConfig::new(SampleRate(48_000.0), layout).unwrap();
        let buf: Vec<f32> = (0..48_000 * 4)
            .flat_map(|i| {
                let s = 0.5 * (TAU * 1_000.0 * i as f32 / 48_000.0).sin();
                [0.0, 0.0, 0.0, s, 0.0, 0.0]
            })
            .collect();
        for size in [775, 777] {
            let mut state = WindowedLoudnessState::new(&cfg).unwrap();
            let mut last = None;
            for chunk in buf.chunks(size) {
                last = Some(
                    step_windowed_loudness(&cfg, &mut state, Interleaved::new(chunk, layout))
                        .unwrap(),
                );
            }
            let last = last.unwrap();
            assert_eq!(last.momentary, WindowedReading::LOUDNESS_FLOOR, "{size}");
            assert_eq!(last.short_term, WindowedReading::LOUDNESS_FLOOR, "{size}");
        }
    }

    /// `reset` returns the meter to its first state.
    #[test]
    fn reset_reads_like_a_fresh_meter() {
        let cfg = windowed_cfg(48_000.0);
        let tone = windowed_sine(48_000.0, 4.0, |_| 0.1);
        let mut state = WindowedLoudnessState::new(&cfg).unwrap();
        step_windowed_loudness(
            &cfg,
            &mut state,
            Interleaved::new(&tone, ChannelLayout::STEREO),
        )
        .unwrap();
        assert!(state.frames_seen() > 0);
        state.reset();
        assert_eq!(state.frames_seen(), 0);
        let quiet = vec![0.0f32; 48_000 * 2];
        let r = step_windowed_loudness(
            &cfg,
            &mut state,
            Interleaved::new(&quiet, ChannelLayout::STEREO),
        )
        .unwrap();
        assert_eq!(r.momentary, WindowedReading::LOUDNESS_FLOOR);
        assert_eq!(r.short_term, WindowedReading::LOUDNESS_FLOOR);
        assert_eq!(r.true_peak, Db::FLOOR);
    }

    /// `reset` also drops a part frame carried from an odd-length chunk, so the
    /// next chunk starts on channel 0: a 5.1 signal on the LFE alone, which R128
    /// does not weight, would otherwise land on a weighted channel.
    #[test]
    fn reset_drops_a_carried_part_frame() {
        let layout = ChannelLayout::from(6u16);
        let cfg = WindowedLoudnessConfig::new(SampleRate(48_000.0), layout).unwrap();
        let lfe = |frames: usize| -> Vec<f32> {
            (0..frames)
                .flat_map(|i| {
                    let s = 0.5 * (TAU * 1_000.0 * i as f32 / 48_000.0).sin();
                    [0.0, 0.0, 0.0, s, 0.0, 0.0]
                })
                .collect()
        };
        let mut state = WindowedLoudnessState::new(&cfg).unwrap();
        // Three samples past a whole number of frames.
        let odd = lfe(101);
        step_windowed_loudness(&cfg, &mut state, Interleaved::new(&odd[..603], layout)).unwrap();
        state.reset();
        let whole = lfe(24_000);
        let r = step_windowed_loudness(&cfg, &mut state, Interleaved::new(&whole, layout)).unwrap();
        assert_eq!(r.momentary, WindowedReading::LOUDNESS_FLOOR);
    }

    /// The true peak is the loudest channel's, wherever it is: a tone of
    /// amplitude 0.5 on the right alone peaks at `20 log10(0.5)` dBTP (4x
    /// oversampling adds under 0.3 dB), and on the left alone just the same.
    #[test]
    fn the_true_peak_is_the_loudest_channels() {
        let cfg = windowed_cfg(48_000.0);
        let want = (20.0 * 0.5f64.log10()) as f32;
        for (left, right) in [(0.0f64, 0.5f64), (0.5, 0.0), (0.1, 0.5)] {
            let signal: Vec<f32> = (0..24_000)
                .flat_map(|i| {
                    let s = (TAU as f64 * 997.0 * i as f64 / 48_000.0).sin();
                    [(left * s) as f32, (right * s) as f32]
                })
                .collect();
            let mut state = WindowedLoudnessState::new(&cfg).unwrap();
            let r = step_windowed_loudness(
                &cfg,
                &mut state,
                Interleaved::new(&signal, ChannelLayout::STEREO),
            )
            .unwrap();
            assert!(
                (r.true_peak.get() - want).abs() < 0.3,
                "L {left} R {right}: {:?}",
                r.true_peak
            );
        }
    }

    /// Every setting is checked, NaN and infinity included, and a bad chunk is
    /// refused without touching the meter.
    #[test]
    fn bad_settings_and_input_are_errors() {
        for rate in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -48_000.0,
            7_999.0,
            192_001.0,
        ] {
            assert!(
                matches!(
                    WindowedLoudnessConfig::new(SampleRate(rate), ChannelLayout::STEREO),
                    Err(Error::InvalidLoudnessParameter(_))
                ),
                "rate {rate}"
            );
        }
        for channels in [0u16, 17] {
            assert!(matches!(
                WindowedLoudnessConfig::new(SampleRate(48_000.0), ChannelLayout::from(channels)),
                Err(Error::InvalidLoudnessParameter(_))
            ));
        }
        assert!(WindowedLoudnessConfig::new(SampleRate(8_000.0), ChannelLayout::MONO).is_ok());
        assert!(
            WindowedLoudnessConfig::new(SampleRate(192_000.0), ChannelLayout::from(16u16)).is_ok()
        );

        let cfg = windowed_cfg(48_000.0);
        let mut state = WindowedLoudnessState::new(&cfg).unwrap();
        let tone = windowed_sine(48_000.0, 0.5, |_| 0.1);
        let fed = |state: &mut WindowedLoudnessState, buf: &[f32]| {
            step_windowed_loudness(&cfg, state, Interleaved::new(buf, ChannelLayout::STEREO))
        };
        fed(&mut state, &tone).unwrap();
        let frames = state.frames_seen();

        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut chunk = tone.clone();
            chunk[7] = bad;
            assert_eq!(fed(&mut state, &chunk), Err(Error::NonFiniteInput));
            assert_eq!(state.frames_seen(), frames, "a refused chunk feeds nothing");
        }
        assert!(matches!(
            step_windowed_loudness(
                &cfg,
                &mut state,
                Interleaved::new(&tone, ChannelLayout::QUAD)
            ),
            Err(Error::InvalidLoudnessParameter(_))
        ));
        assert_eq!(state.frames_seen(), frames);

        let other = windowed_cfg(44_100.0);
        assert!(matches!(
            step_windowed_loudness(
                &other,
                &mut state,
                Interleaved::new(&tone, ChannelLayout::STEREO)
            ),
            Err(Error::InvalidLoudnessParameter(_))
        ));
    }
}
