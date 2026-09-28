//! [`Unit`] — the public filter: frame in, frame out, one `Vocoder` per channel.
//!
//! Owns no source. The caller ticks its own source and feeds each frame in,
//! which is why stretch and pitch compose here rather than fight: the hop
//! geometry expresses both, and [`Unit::input_rate`] tells the caller how fast
//! to feed it.

use std::sync::Arc;

use tutti_analysis::StftGeometry;

use super::vocoder::Vocoder;
use super::{FftSize, MAX_BUFFER_SIZE};
use crate::lanes::Lane;
use tutti_core::{
    AtomicF32, Cents, ChannelLayout, Ordering, ReadRate, RtScratch, SampleRate, Samples,
    StretchFactor, Tail,
};

/// Real-time time-stretching and pitch-shifting unit.
///
/// A pure frame-in → frame-out **filter**: it owns NO source. The caller ticks
/// the real audio source itself and feeds the resulting frame in as this unit's
/// `input`; what lives here is the latent phase-vocoder state (one vocoder per
/// channel, the scratch buffers, the atomics).
///
/// # Stretch, pitch and read rate are three different quantities
///
/// - [`set_stretch_factor`](Self::set_stretch_factor) takes a
///   [`StretchFactor`] — duration scaling that leaves pitch alone, which is the
///   operation `PlaybackRate` cannot express because resampling couples the two.
/// - [`set_pitch_cents`](Self::set_pitch_cents) takes [`Cents`] — transposition
///   that leaves duration alone.
/// - [`input_rate`](Self::input_rate) returns a [`ReadRate`] — how fast a
///   *placed* caller must advance its own source cursor. Derived from the
///   stretch factor; never a user intent in itself.
///
/// # Real-time safety
///
/// Every buffer is allocated in
/// [`with_fft_size_and_channels`](Self::with_fft_size_and_channels), or by
/// `clone`. Neither `tick` nor `process` allocates or blocks; both are safe
/// on the audio thread.
///
/// # Owned, not shared
///
/// The vocoders and the block scratch are this unit's, by value; no two
/// units share running state.
///
/// A clone is a **fresh** filter with the same width, window and parameters:
/// its own vocoders built on the same grid (sharing only the immutable window
/// and phase tables), none of the running state. That is what a voice's fork
/// needs, since it resets what it clones anyway. A clone allocates about
/// 100 KB per channel: clone on the control thread.
///
/// # Channels
///
/// One vocoder per channel, plus one in/out
/// [`RtScratch`](tutti_core::RtScratch) pair each. The vocoders are
/// **independent** — there is no phase locking between them, so a correlated
/// source can drift channel-to-channel. Fixing that is a separate question from
/// width.
pub struct Unit {
    /// The per-channel state, owned. Boxed so a `Unit` moves as a pointer:
    /// the pool's drain moves filters in and out of `VoiceCommand`s, which
    /// stay small (and a move out of a field frees nothing, where a move out
    /// of a `Box<Unit>` would free the box on the audio thread).
    pub(super) ch: Box<Channels>,
    /// The unit's declared width — one vocoder per channel.
    pub(super) width: ChannelLayout,
    pub(super) stretch_factor: Arc<AtomicF32>,
    pub(super) pitch_cents: Arc<AtomicF32>,
    pub(super) enabled: bool,
    /// Fractional debt in the source-intake resampler: how much of the next
    /// source sample the unit still owes itself before it may consume one.
    ///
    /// A time-stretcher emits `stretch` samples per source sample, but
    /// [`tick`](Self::tick) hands over exactly one and takes exactly one back. The
    /// only way to satisfy both is for the unit to consume the source at
    /// `1 / stretch` internally — dropping input above unity, repeating it below —
    /// which is what this accumulator paces. See [`Unit::hops`].
    ///
    /// One accumulator for all channels: they share a stretch factor, so
    /// per-channel debts would always be equal and could only drift through a
    /// bug that skewed the channels against each other.
    pub(super) intake_debt: f64,
}

/// A [`Unit`]'s per-channel state: one vocoder per channel, and `process`'s
/// block scratch.
pub(super) struct Channels {
    /// One per channel; its length **is** the unit's width.
    pub(super) vocoders: Vec<Vocoder>,
    /// `process`'s per-block working buffers, one pair per channel. Carry
    /// nothing between blocks: `process` overwrites `scratch_in` from its
    /// input and clears `scratch_out` before draining into it.
    pub(super) scratch_in: Vec<RtScratch<f32>>,
    pub(super) scratch_out: Vec<RtScratch<f32>>,
}

// Hand-rolled: holds non-`Debug` vocoders and `RtScratch` buffers. Print the
// live stretch/pitch atomics and the enabled flag.
impl std::fmt::Debug for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unit")
            .field("channels", &self.width)
            .field("enabled", &self.enabled)
            .field("stretch_factor", &self.stretch_factor())
            .field("pitch_cents", &self.pitch_cents())
            .finish_non_exhaustive()
    }
}

impl Unit {
    /// Creates a stereo unit at `sample_rate`, with the default FFT size
    /// ([`FftSize::N2048`]), unity stretch and no transposition (bypassing).
    /// Allocates: control thread.
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        Self::with_fft_size_and_channels(sample_rate, FftSize::default(), ChannelLayout::STEREO)
    }

    /// Creates a stereo unit with a custom FFT size.
    pub fn with_fft_size(sample_rate: impl Into<SampleRate>, fft_size: FftSize) -> Self {
        Self::with_fft_size_and_channels(sample_rate, fft_size, ChannelLayout::STEREO)
    }

    /// Creates a `channels`-wide unit at the default FFT size.
    pub fn with_channels(
        sample_rate: impl Into<SampleRate>,
        channels: impl Into<ChannelLayout>,
    ) -> Self {
        Self::with_fft_size_and_channels(sample_rate, FftSize::default(), channels)
    }

    /// Creates a `channels`-wide unit with a custom FFT size.
    ///
    /// A zero-wide layout is treated as mono.
    ///
    /// Width is fixed here because it sizes one vocoder and two scratch buffers
    /// per channel, all allocated on this path so the RT path never does.
    /// Callers must pass the width of the source they will feed in: a narrower
    /// stretcher silently truncates the frames handed to `tick`.
    pub fn with_fft_size_and_channels(
        sample_rate: impl Into<SampleRate>,
        fft_size: FftSize,
        channels: impl Into<ChannelLayout>,
    ) -> Self {
        let geometry = Self::geometry(sample_rate, fft_size);
        // A zero-wide filter has nothing to process and would make `inputs()` /
        // `outputs()` lie to the graph — see [`crate::nonempty`], which is where
        // that rule lives for every node in the crate.
        let width = crate::nonempty(channels.into());
        // Stride derived once, here on the construction path.
        let n = width.count() as usize;
        Self::from_vocoders(
            (0..n).map(|_| Vocoder::new(geometry)).collect(),
            width,
            StretchFactor::UNITY.get(),
            0.0,
            true,
        )
    }

    /// A unit over `vocoders` (one per channel of `width`), its block scratch
    /// sized here: a unit must be usable without a later hook, and this is the
    /// control thread.
    fn from_vocoders(
        vocoders: Vec<Vocoder>,
        width: ChannelLayout,
        stretch_factor: f32,
        pitch_cents: f32,
        enabled: bool,
    ) -> Self {
        let n = vocoders.len();
        let scratch = || {
            (0..n)
                .map(|_| RtScratch::new(MAX_BUFFER_SIZE))
                .collect::<Vec<_>>()
        };
        Self {
            ch: Box::new(Channels {
                vocoders,
                scratch_in: scratch(),
                scratch_out: scratch(),
            }),
            width,
            stretch_factor: Arc::new(AtomicF32::new(stretch_factor)),
            pitch_cents: Arc::new(AtomicF32::new(pitch_cents)),
            enabled,
            intake_debt: 0.0,
        }
    }

    /// The analysis grid, which is COLA-valid for every [`FftSize`].
    ///
    /// `cola` is fallible in general — a hop that does not divide its window,
    /// or overlaps under 75%, cannot reconstruct. Neither is reachable here:
    /// `FftSize::hop` is exactly `size / 4`. The rate is clamped because a
    /// non-positive one is the remaining rejectable input, and a stretcher that
    /// panicked on a device reporting 0 Hz would be worse than one that runs at
    /// a nominal rate.
    pub(super) fn geometry(sample_rate: impl Into<SampleRate>, fft_size: FftSize) -> StftGeometry {
        let rate = sample_rate.into().get().max(1.0);
        StftGeometry::cola(rate, fft_size.size(), fft_size.hop())
            .expect("BUG: FftSize hop is size/4, which is COLA-valid at a positive rate")
    }

    /// Channel width — the number of vocoders, and this unit's in/out arity.
    pub fn channels(&self) -> ChannelLayout {
        self.width
    }

    /// The same width as a stride, for indexing. Derive it **once** per call,
    /// above any loop.
    #[inline]
    fn stride(&self) -> usize {
        self.width.count() as usize
    }

    /// Sets the duration scaling, clamped into
    /// [`StretchFactor::MIN`]..=[`StretchFactor::MAX`].
    ///
    /// Above unity the material gets longer, below it shorter, and pitch is
    /// untouched either way. `&self` and a single atomic store, so a control
    /// thread may call it while the audio thread ticks.
    pub fn set_stretch_factor(&self, factor: StretchFactor) {
        self.stretch_factor.store(
            StretchFactor::new_clamped(factor.get()).get(),
            Ordering::Release,
        );
    }

    /// The duration scaling currently in force, as last clamped and stored.
    pub fn stretch_factor(&self) -> StretchFactor {
        StretchFactor::new(self.stretch_factor.load(Ordering::Acquire))
    }

    /// The stretch cell itself, for a caller driving it lock-free from
    /// elsewhere — automation, or a pool holding the filter across voices.
    ///
    /// Writes through this handle bypass the clamp that
    /// [`set_stretch_factor`](Self::set_stretch_factor) applies, so a caller
    /// using it owns keeping the value inside
    /// [`StretchFactor::MIN`]..=[`StretchFactor::MAX`].
    ///
    /// A block read (`filter_lanes`, the voice pool's path) reads the cell
    /// once per block, so a write that lands mid-block takes effect at the
    /// next block, not at the next frame.
    pub fn stretch_factor_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.stretch_factor)
    }

    /// Sets the transposition in [`Cents`], clamped to ±2400 (two octaves each
    /// way).
    ///
    /// Duration is untouched: the resample that transposes is undone by the
    /// hops. `&self` and a single atomic store, so it is safe alongside a
    /// running audio thread.
    pub fn set_pitch_cents(&self, cents: Cents) {
        self.pitch_cents.store(
            cents.get().clamp(MIN_PITCH_CENTS, MAX_PITCH_CENTS),
            Ordering::Release,
        );
    }

    /// The transposition currently in force, as last clamped and stored.
    pub fn pitch_cents(&self) -> Cents {
        Cents::new(self.pitch_cents.load(Ordering::Acquire))
    }

    /// The pitch cell itself, for a caller driving it lock-free from elsewhere.
    ///
    /// As with [`stretch_factor_arc`](Self::stretch_factor_arc), writes here
    /// skip the ±2400-cent clamp and the caller owns the range.
    pub fn pitch_cents_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.pitch_cents)
    }

    /// Engages or bypasses the vocoder outright.
    ///
    /// Bypassing is not the same as setting unity stretch and zero pitch even
    /// though both take the pass-through branch: this flag survives any later
    /// parameter write, so a disabled unit stays silent-cost regardless of what
    /// automation does to the atomics. Reported latency falls to zero either way
    /// — see [`latency_samples`](Self::latency_samples).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Whether the vocoder is engaged at all, ignoring the current parameters.
    ///
    /// An enabled unit sitting at unity stretch and zero pitch still reports
    /// `true` here while [`is_processing`](Self::is_processing) reports `false`.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the unit is doing anything but passing audio through.
    ///
    /// `false` when disabled, or when stretch is within `0.001` of unity *and*
    /// pitch within half a cent of zero — both beneath audibility, so the unit
    /// bypasses rather than paying for an FFT. This is the condition every other
    /// branch keys off: latency, `tail`, and the fast paths in `tick` and
    /// `process`.
    pub fn is_processing(&self) -> bool {
        self.enabled
            && ((self.stretch_factor().get() - StretchFactor::UNITY.get()).abs() > STRETCH_EPSILON
                || self.pitch_cents().get().abs() > PITCH_EPSILON_CENTS)
    }

    /// Processing latency, in samples — **zero while bypassing**.
    ///
    /// One whole window must arrive before the first frame can be analysed, so a
    /// processing unit delays by exactly that. A bypassing one does not: `tick`
    /// and `process` copy input to output directly at unity stretch and pitch, or
    /// when disabled.
    ///
    /// Reporting a window while the audio passes straight through would make
    /// delay compensation hold every other branch of the graph back for a
    /// delay that does not exist (46 ms at the default 2048 window). Bypass is
    /// reachable through ordinary use, not only at construction: returning a
    /// stretched voice to 1.0 keeps its filter resident, sitting at unity.
    ///
    /// Every channel reports the same value (a function of the shared FFT size),
    /// so channel 0 speaks for all.
    pub fn latency_samples(&self) -> usize {
        if !self.is_processing() {
            return 0;
        }
        self.ch
            .vocoders
            .first()
            .map_or(0, |v| v.geometry.window().get())
    }

    /// Output frames a flushed filter must be fed before its output is
    /// steady: the [`latency`](Self::latency_samples) (one window of the
    /// vocoder's own input) over the [`intake_rate`](Self::intake_rate) it
    /// takes that input at, so a window of input times the effective
    /// stretch — 4 096 frames at 2x on the default window. Zero when
    /// bypassing.
    ///
    /// Not a PDC figure: it counts output frames of *feed*, the refill a
    /// jump costs (`PlaybackSlot::prime_stretch`).
    pub(crate) fn refill_frames(&self) -> usize {
        (self.latency_samples() as f64 / self.intake_rate()).ceil() as usize
    }

    /// Time-scaling the vocoder actually performs: `stretch × pitch_ratio`.
    ///
    /// **Not** [`stretch_factor`](Self::stretch_factor), and deliberately not
    /// clamped to [`StretchFactor::MIN`]..=[`StretchFactor::MAX`]. Those bounds
    /// describe *user intent* — how much longer the material should get. This is
    /// an internal quantity, and pitch shifting drives it outside them by
    /// construction: 4× stretch up two octaves is an effective 16.0, and 0.25×
    /// down two octaves is 0.0625.
    ///
    /// Pitch shift is resample-then-restore. Reading the source `pitch_ratio`
    /// faster transposes it *and* shortens it (that much is plain varispeed);
    /// stretching by the same ratio restores the duration and leaves the
    /// transposition behind. Neither half is a pitch shift alone — the caller
    /// supplies the read rate via [`input_rate`](Self::input_rate) and the
    /// vocoder supplies the restore, which is why the two must be derived from
    /// one place.
    ///
    /// Verified at the corners: pitch lands within 1 Hz of target at effective
    /// factors from 0.0625 to 16.0, i.e. across the whole clamp-free range.
    #[inline]
    pub(super) fn effective_stretch(&self) -> f32 {
        self.stretch_factor().get() * self.pitch_cents().to_pitch_ratio()
    }

    /// Fed samples this unit consumes per output sample:
    /// `1 / effective_stretch`.
    ///
    /// **The unit paces its own intake**, and the caller feeds one source sample
    /// per output sample. That is the crate's existing contract — every voice
    /// path hands `tick` a single frame and expects one back — and pitch shifting
    /// rides on it rather than changing it.
    ///
    /// Consuming fed frames faster than one-per-output *is* a resample, and
    /// resampling is the only thing that transposes. So this rate carries both
    /// halves, for opposite reasons:
    ///
    /// - **`1 / stretch`** — consume slower so the material lasts longer;
    ///   the hops restore the pitch that slow read would otherwise drop.
    /// - **`/ pitch_ratio`** — consume faster to transpose up; the hops, which
    ///   scale by the same [`effective_stretch`](Self::effective_stretch),
    ///   restore the duration that fast read costs.
    ///
    /// Both must be derived from the one effective factor the hops use, or the
    /// resample and the restore disagree and the result is neither the requested
    /// pitch nor the requested length.
    ///
    /// Deliberately **not** a [`ReadRate`]: that type names what a caller
    /// advances a source cursor by (see [`input_rate`](Self::input_rate)), and
    /// this is the unit's own consumption of frames already produced. Two
    /// quantities on two sides of a boundary; sharing one type is what would let
    /// an edit swap them.
    ///
    /// `f64` to match `intake_debt`: the debt accumulates once per output sample
    /// and is never reset, so a long voice sums millions of terms. Widening here
    /// keeps that from drifting — the same reason [`ReadRate`] is `f64`-backed.
    #[inline]
    pub(super) fn intake_rate(&self) -> f64 {
        1.0 / self.effective_stretch() as f64
    }

    /// The (analysis, synthesis) hop pair for the current effective stretch.
    ///
    /// The **synthesis** hop is pinned to the grid's natural `size / 4`, and the
    /// **analysis** hop is `synthesis / effective`. Their ratio is exactly
    /// [`effective_stretch`](Self::effective_stretch) — the time-scaling that,
    /// combined with the caller reading at [`input_rate`](Self::input_rate),
    /// yields the requested stretch and pitch independently.
    ///
    /// Pinning synthesis is a COLA requirement, not a preference. Overlap-add
    /// reconstruction needs the synthesis frames to overlap by at least 75% for a
    /// Hann-squared pair to sum to a constant; the synthesis hop is what sets that
    /// overlap, and the window is fixed. Scaling synthesis *up* with the stretch
    /// factor — the textbook offline formulation — walks the overlap down as the
    /// factor rises: 62% at 1.5x, 50% at 2x, and at 4x the hop equals the whole
    /// window, so consecutive frames abut with NO overlap at all. The Hann²
    /// envelopes then ripple instead of summing flat, and the output amplitude
    /// modulates at the frame rate. Measured as 16 of 256 blocks dipping under a
    /// tenth of full level at 4x, on a perfectly steady input.
    ///
    /// Scaling analysis down instead keeps every factor at the same 75% overlap
    /// the grid was built for, and `Unit::geometry` asserts that grid is COLA-valid.
    ///
    /// Both hops are clamped to at least 1: a zero analysis hop would re-read the
    /// same frame forever, and a zero synthesis hop would advance the output ring
    /// nowhere and spin `process` in an infinite loop.
    #[inline]
    pub(super) fn hops(&self) -> (usize, usize) {
        let synthesis = self
            .ch
            .vocoders
            .first()
            .map_or(1, |v| v.geometry.hop().get());
        let analysis = ((synthesis as f32 / self.effective_stretch()).round() as usize).max(1);
        (analysis, synthesis.max(1))
    }

    /// Source samples a **placed** caller advances per output sample:
    /// `1 / stretch`.
    ///
    /// For a voice whose position is *derived from the playhead* rather than fed
    /// sample-by-sample. Such a caller cannot let the unit pace its own intake —
    /// it computes where in the wave to read from the transport, so it must scale
    /// that position itself or the stretch never reaches the source.
    ///
    /// **Pitch is deliberately absent**, and this is the subtle half of the
    /// design. Pitch shifting is a resample, and the unit performs that resample
    /// internally by consuming fed frames at its own intake rate. Folding the
    /// pitch ratio in here as well would resample *twice* — once at the caller's
    /// cursor, once at the intake — and the second cancels the first exactly.
    /// Measured: with pitch folded in here the output holds 440 Hz at every
    /// requested interval, a silent no-op rather than a transposition.
    ///
    /// The internal intake rate carries both halves because it *is* the
    /// resample. The two are not interchangeable despite both being "samples
    /// consumed per output sample": one scales a cursor into a wave, the other
    /// scales consumption of an already-produced stream.
    ///
    /// `1.0` when the unit is bypassing, so a caller can apply it
    /// unconditionally.
    #[inline]
    pub fn input_rate(&self) -> ReadRate {
        if !self.is_processing() {
            return ReadRate::UNITY;
        }
        ReadRate(1.0 / self.stretch_factor().get() as f64)
    }
}

/// Below this, a stretch factor is indistinguishable from unity and the unit
/// bypasses rather than paying for an FFT.
const STRETCH_EPSILON: f32 = 0.001;
/// Half a cent — beneath the threshold of hearing.
const PITCH_EPSILON_CENTS: f32 = 0.5;
/// Two octaves down.
pub(super) const MIN_PITCH_CENTS: f32 = -2400.0;
/// Two octaves up.
pub(super) const MAX_PITCH_CENTS: f32 = 2400.0;

impl Unit {
    /// Filter frames `frames` of the lanes `input` into the same frames of
    /// `output`, `n` lanes each (`n` ≥ 1: the caller's width): **exactly**
    /// what one [`tick`](Self::tick) per frame, each handed `n` input
    /// samples and an `n`-wide output frame cleared to zero, would write.
    ///
    /// This is the slot's block read through the filter
    /// (`PlaybackSlot::process_into`). Bit-identical to calling `tick` once
    /// per frame, because it does the same arithmetic on the same values in
    /// the same order per channel:
    ///
    /// - The parameters (stretch, pitch, enabled, and the hops and intake
    ///   rate derived from them) are read **once per block**, where `tick`
    ///   read its atomics every frame; a control write lands on the next
    ///   block rather than mid-block.
    /// - The channels share nothing but the intake debt, whose sequence of
    ///   values does not depend on the samples. So each channel replays that
    ///   sequence from the block's starting debt, channel-outer (one vocoder's
    ///   rings stay hot for the whole block), and pushes and processes exactly
    ///   the frames `tick` would have pushed it.
    /// - A lane `c` past the input's width reads lane 0 (`tick`'s fan from
    ///   channel 0); an output lane past the unit's width is written zero, and
    ///   a vocoder past the output's width is fed and never drained, as
    ///   `tick` leaves them.
    ///
    /// Allocation-free.
    pub(crate) fn filter_lanes(
        &mut self,
        input: &[Lane],
        output: &mut [Lane],
        n: usize,
        frames: std::ops::Range<usize>,
    ) {
        let n = n.min(input.len()).min(output.len());
        if n == 0 {
            return;
        }
        let stride = self.stride();
        let n_out = stride.min(n);
        for lane in &mut output[n_out..n] {
            lane[frames.clone()].fill(0.0);
        }
        if !self.is_processing() {
            for (c, lane) in output[..n_out].iter_mut().enumerate() {
                let src = &input[if c < n { c } else { 0 }];
                lane[frames.clone()].copy_from_slice(&src[frames.clone()]);
            }
            return;
        }
        let (analysis_hop, synthesis_hop) = self.hops();
        let rate = self.intake_rate();
        let start = self.intake_debt;
        let mut end = start;
        for (c, v) in self.ch.vocoders.iter_mut().enumerate() {
            let src = &input[if c < n { c } else { 0 }];
            let mut debt = start;
            let mut one = [0.0f32];
            for i in frames.clone() {
                debt += rate;
                while debt >= 1.0 {
                    debt -= 1.0;
                    v.input.push(&[src[i]]);
                    v.process(analysis_hop, synthesis_hop);
                }
                if c < n_out {
                    one[0] = 0.0;
                    v.output.drain(&mut one);
                    output[c][i] = one[0];
                }
            }
            end = debt;
        }
        self.intake_debt = end;
    }
}

impl Clone for Unit {
    /// A **fresh** filter: the same width, window and parameters, its own
    /// vocoders with none of this one's running state, and its own block
    /// scratch. See "Owned, not shared" on [`Unit`] for who clones, and why
    /// fresh rather than a copy of the stream.
    ///
    /// Allocates (about 100 KB per channel): control thread only.
    fn clone(&self) -> Self {
        Self::from_vocoders(
            self.ch.vocoders.iter().map(Vocoder::clone_fresh).collect(),
            self.width,
            self.stretch_factor.load(Ordering::Acquire),
            self.pitch_cents.load(Ordering::Acquire),
            self.enabled,
        )
    }
}

// The filter's frame and block entry points. A slot's filter is never a graph
// node, so these are plain methods rather than a `tutti_graph::Node` impl.
impl Unit {
    /// Clears the running state: every vocoder's rings and phase history, and
    /// the intake debt. Allocation-free.
    pub fn reset(&mut self) {
        for v in &mut self.ch.vocoders {
            v.reset();
        }
        self.intake_debt = 0.0;
    }

    /// Retunes the geometry and **preserves** all running state — the phase
    /// history and the intake debt. Allocation-free, so a device
    /// change mid-stream costs a per-channel geometry rebuild and nothing else.
    ///
    /// Keeping the state is a choice, and not every processor makes it
    /// (`tutti_spatial`'s HRTF panner, preparing at a new rate, rebuilds its
    /// HRIR sphere and zeroes its streaming buffers), so a caller should not
    /// assume the two behave alike.
    pub fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        // The grid's window and hop are sample counts and its phase table is
        // their ratio, so none of the vocoder state depends on the rate. Only
        // the rate the geometry reports back does — rebuild it, and leave the
        // running phase history alone.
        for v in &mut self.ch.vocoders {
            v.geometry = Self::geometry(sample_rate, FftSize::default());
        }
    }

    /// Processes one frame: `input` in (one sample per channel; a short frame
    /// fans channel 0 to the rest), `output` out. Allocation-free; safe on the
    /// audio thread.
    pub fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // `input` is the source frame the caller already produced (in-memory index
        // or streaming ring pop). This unit does not own or pull a source.
        //
        // A short `input` fans channel 0 to the rest: a mono feed into a wider
        // stretcher stays audible on every channel rather than going silent
        // past the first.
        // Stride derived once, above the loops.
        let n = self.stride().min(output.len());
        let src0 = input.first().copied().unwrap_or(0.0);
        let src = |c: usize| input.get(c).copied().unwrap_or(src0);

        if !self.is_processing() {
            for (c, o) in output.iter_mut().enumerate().take(n) {
                *o = src(c);
            }
            return;
        }

        let (analysis_hop, synthesis_hop) = self.hops();

        // Pace the intake at the PITCH half — see `intake_debt` and
        // `input_rate`. Above unity this drops fed samples (transposing up);
        // below it, feeds the same one twice (transposing down). The stretch
        // half is the caller's job, already applied to the frames arriving here.
        self.intake_debt += self.intake_rate();
        while self.intake_debt >= 1.0 {
            self.intake_debt -= 1.0;
            for (c, v) in self.ch.vocoders.iter_mut().enumerate() {
                v.input.push(&[src(c)]);
                v.process(analysis_hop, synthesis_hop);
            }
        }

        let mut one = [0.0f32];
        for (c, o) in output.iter_mut().enumerate().take(n) {
            one[0] = 0.0;
            self.ch.vocoders[c].output.drain(&mut one);
            *o = one[0];
        }
    }

    /// `size` frames of planar `input` (one slice per channel; fewer
    /// channels than the unit fan channel 0 to the rest) into planar
    /// `output`, as `size` calls of [`tick`](Self::tick) would. Each slice
    /// holds at least `size` frames. Allocation-free.
    ///
    /// # Panics
    ///
    /// If a slice of `input` or `output` holds fewer than `size` frames, or if
    /// `size` exceeds the unit's scratch capacity of 8192 frames.
    pub fn process(&mut self, size: usize, input: &[&[f32]], output: &mut [&mut [f32]]) {
        // `size` past MAX_BUFFER_SIZE is clamped by `RtScratch::active`; the
        // fixed capacity makes a per-block reallocation impossible.
        //
        // One scratch pair per channel (not one flat strided buffer): the
        // vocoder API is per-channel `push`/`pop` over a contiguous run of
        // samples, so channel-major is what it wants. A frame-major buffer
        // would need a de-interleave here and a re-interleave after, for no
        // gain.
        // Stride derived once per block, above the loops.
        let channels = self.stride();
        let in_ch = input.len();

        for (c, s) in self.ch.scratch_in.iter_mut().enumerate() {
            let buf = s.active(size);
            // Fewer input channels than vocoders: mirror `tick`'s
            // fan-from-channel-0 rather than emitting silence.
            let src_ch = if c < in_ch { c } else { 0 };
            for (i, b) in buf.iter_mut().enumerate().take(size) {
                *b = input[src_ch][i];
            }
        }

        let out_ch = output.len().min(channels);

        if !self.is_processing() {
            for (lane, scratch) in output[..out_ch].iter_mut().zip(&self.ch.scratch_in) {
                let buf = scratch.active_ref(size);
                let n = buf.len().min(size);
                lane[..n].copy_from_slice(&buf[..n]);
            }
            return;
        }

        let (analysis_hop, synthesis_hop) = self.hops();
        let rate = self.intake_rate();

        // Same intake pacing as `tick`, applied per sample of the block so the
        // two entry points consume the source identically. Walking the block
        // rather than pushing it whole is what keeps `process` and `tick`
        // producing the same audio; two separately written paths would drift.
        for i in 0..size {
            self.intake_debt += rate;
            while self.intake_debt >= 1.0 {
                self.intake_debt -= 1.0;
                for (c, v) in self.ch.vocoders.iter_mut().enumerate() {
                    let sample = self.ch.scratch_in[c].active_ref(size)[i];
                    v.input.push(&[sample]);
                    v.process(analysis_hop, synthesis_hop);
                }
            }
        }

        for c in 0..channels {
            let out = self.ch.scratch_out[c].active(size);
            out.fill(0.0);
            let count = self.ch.vocoders[c].output.drain(out);
            let Some(lane) = output[..out_ch].get_mut(c) else {
                continue;
            };
            for (i, &s) in out.iter().enumerate().take(size) {
                lane[i] = if i < count { s } else { 0.0 };
            }
        }
    }

    /// The overlap-add accumulator's contents — one FFT window.
    ///
    /// The same number [`latency_samples`](Self::latency_samples) reports, and
    /// for the same reason: a window's worth of audio has been written into the
    /// accumulator but not yet advanced past. It must be drained after the input
    /// stops or the stretched material ends a window early.
    ///
    /// The bypass branch is mirrored deliberately. A unit sitting at unity does
    /// no overlap-add and holds nothing, so reporting a window there would
    /// append ~46 ms of silence to every unstretched voice.
    pub fn tail(&self) -> Tail {
        match self.latency_samples() {
            0 => Tail::None,
            n => Tail::Finite(Samples(n)),
        }
    }
}
