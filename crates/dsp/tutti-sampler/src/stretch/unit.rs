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
    AtomicF32, AudioUnit, BufferMut, BufferRef, Cents, ChannelLayout, Ordering, ReadRate,
    RtScratch, SampleRate, Samples, SignalFrame, StretchFactor, Tail,
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
/// The vocoders and the block scratch are this unit's, by value. They used
/// to sit in an `Arc<Bank>` that every clone shared, with a `ticker` claim
/// token and `AudioThreadCell`s to catch two handles ticking one bank: `Net`
/// cloned every node on every graph commit, and a deep copy was 201.8 MB per
/// commit over 640 stereo nodes (`examples/profile_stretch_clone.rs`). The
/// native graph does not clone a node to commit it (doc 013 item 7), so the
/// sharing, the claim and the cells went, and so did the `allocate` hook that
/// sized the scratch a sharing clone left empty.
///
/// A clone is now what the two remaining callers of `Clone` need, and no
/// more: a **fresh** filter with the same width, window and parameters — its
/// own vocoders built on the same grid (sharing the immutable window and phase
/// tables, `Vocoder::clone_fresh`), none of the running state. Those callers
/// are `tutti_graph::Legacy::controlled`'s shadow (taken once, at insert) and
/// a fork cloned from it; both reset what they clone, so copying a running
/// stream's rings would buy nothing. It allocates about 100 KB per channel,
/// on the control thread, once per insert and once per fork.
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
    /// [`AudioUnit::tick`] hands over exactly one and takes exactly one back. The
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
    /// Stereo, at the default FFT size.
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        Self::with_fft_size_and_channels(sample_rate, FftSize::default(), ChannelLayout::STEREO)
    }

    /// Stereo, at a custom FFT size.
    pub fn with_fft_size(sample_rate: impl Into<SampleRate>, fft_size: FftSize) -> Self {
        Self::with_fft_size_and_channels(sample_rate, fft_size, ChannelLayout::STEREO)
    }

    /// `channels` wide, at the default FFT size.
    pub fn with_channels(
        sample_rate: impl Into<SampleRate>,
        channels: impl Into<ChannelLayout>,
    ) -> Self {
        Self::with_fft_size_and_channels(sample_rate, FftSize::default(), channels)
    }

    /// Full constructor: custom FFT size at a custom width.
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
        // that rule lives now for every node in the crate.
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

    /// Set the duration scaling, clamped into
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

    /// Set the transposition in [`Cents`], clamped to ±2400 (two octaves each
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

    /// Engage or bypass the vocoder outright.
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
    /// The bypass case is what `route` reports to fundsp's PDC, and reporting a
    /// window there while the audio passes straight through makes every other
    /// branch of the graph get delayed to compensate for a delay that does not
    /// exist — 46 ms at the default 2048 window. It is reachable through ordinary
    /// use, not only at construction: `PlaybackSlot::set_stretch` keeps the resident
    /// filter and writes its atomics, so returning a stretched voice to 1.0 leaves
    /// a filter sitting at unity.
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
    /// what one [`tick`](AudioUnit::tick) per frame, each handed `n` input
    /// samples and an `n`-wide output frame cleared to zero, would write.
    ///
    /// This is the slot's block read through the filter
    /// (`PlaybackSlot::process_into`). Bit-identical to the ticks it
    /// replaced, because it does the same arithmetic on the same values in
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

impl AudioUnit for Unit {
    fn inputs(&self) -> usize {
        // A filter: it consumes the frame the caller feeds in (already tick'd
        // from the real audio source), one channel per vocoder. Boundary:
        // `AudioUnit::inputs` is a fixed fundsp trait signature.
        self.stride()
    }

    fn outputs(&self) -> usize {
        self.stride()
    }

    fn reset(&mut self) {
        for v in &mut self.ch.vocoders {
            v.reset();
        }
        self.intake_debt = 0.0;
    }

    /// Retunes the geometry and **preserves** all running state — the phase
    /// history and the intake debt. Allocation-free, so a device
    /// change mid-stream costs a per-channel geometry rebuild and nothing else.
    ///
    /// The fundsp contract allows either answer (`AudioUnit::set_sample_rate`:
    /// "the unit is allowed to reset itself here... if the sample rate stays
    /// unchanged, the goal is to maintain current state"), and tutti's two
    /// implementors sit at opposite ends of that latitude. The other is
    /// `tutti_spatial`'s `HrtfBinaural::set_sample_rate`, which resamples and
    /// rebuilds its whole HRIR sphere and zeroes the streaming buffers —
    /// allocating, and far from free. A caller that treats the two as
    /// interchangeable is the thing that breaks.
    fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        // The grid's window and hop are sample counts and its phase table is
        // their ratio, so none of the vocoder state depends on the rate. Only
        // the rate the geometry reports back does — rebuild it, and leave the
        // running phase history alone.
        for v in &mut self.ch.vocoders {
            v.geometry = Self::geometry(sample_rate, FftSize::default());
        }
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
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

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
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
        let in_ch = input.channels();

        for (c, s) in self.ch.scratch_in.iter_mut().enumerate() {
            let buf = s.active(size);
            // Fewer input channels than vocoders: mirror `tick`'s
            // fan-from-channel-0 rather than emitting silence.
            let src_ch = if c < in_ch { c } else { 0 };
            for (i, b) in buf.iter_mut().enumerate().take(size) {
                *b = input.at_f32(src_ch, i);
            }
        }

        let out_ch = output.channels().min(channels);

        if !self.is_processing() {
            for c in 0..out_ch {
                let buf = self.ch.scratch_in[c].active_ref(size);
                for (i, &s) in buf.iter().enumerate().take(size) {
                    output.set_f32(c, i, s);
                }
            }
            return;
        }

        let (analysis_hop, synthesis_hop) = self.hops();
        let rate = self.intake_rate();

        // Same intake pacing as `tick`, applied per sample of the block so the
        // two entry points consume the source identically. Walking the block
        // rather than pushing it whole is what keeps `process` and `tick`
        // producing the same audio — they drifted apart once before by writing
        // the two paths separately.
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
            if c >= out_ch {
                continue;
            }
            for (i, &s) in out.iter().enumerate().take(size) {
                output.set_f32(c, i, if i < count { s } else { 0.0 });
            }
        }
    }

    audio_unit_boilerplate!(id = crate::node_id::TIME_STRETCH_ID);

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // As a filter, the incoming `input` frame IS the source signal. Width
        // must track `outputs()` or fundsp mis-plans this node's latency.
        // Stride derived once, above the loop. Boundary: `SignalFrame::new`
        // is a fundsp signature.
        let channels = self.stride();
        let mut out = SignalFrame::new(channels);
        let latency = self.latency_samples() as f64;
        let first = input.at(0).delay(latency);
        for c in 0..channels {
            let sig = if c < input.len() {
                input.at(c).delay(latency)
            } else {
                first
            };
            out.set(c, sig);
        }
        out
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
    fn tail(&mut self) -> Tail {
        match self.latency_samples() {
            0 => Tail::None,
            n => Tail::Finite(Samples(n)),
        }
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }

    // No `isolate` and no `allocate`: a unit shares nothing with its clones
    // (the vocoders, scratch and atomics are each clone's own), so there is
    // nothing to sever, and its scratch is sized wherever it is built. Both
    // hooks existed only for the `Arc<Bank>` a `Net` commit's clone shared.
}
