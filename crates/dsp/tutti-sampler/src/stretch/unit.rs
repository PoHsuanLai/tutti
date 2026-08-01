//! [`Unit`] — the public filter: frame in, frame out, one [`Vocoder`] per channel.
//!
//! Owns no source. The caller ticks its own source and feeds each frame in,
//! which is why stretch and pitch compose here rather than fight: the hop
//! geometry expresses both, and [`Unit::input_rate`] tells the caller how fast
//! to feed it.

use std::sync::Arc;

use tutti_analysis::StftGeometry;

use super::vocoder::Vocoder;
use super::{next_handle_id, Bank, FftSize};
use tutti_core::{
    AtomicF32, AudioUnit, BufferMut, BufferRef, Cents, ChannelLayout, Ordering, ReadRate,
    SampleRate, Samples, SignalFrame, StretchFactor, Tail,
};

/// Real-time time-stretching and pitch-shifting unit.
///
/// A pure frame-in → frame-out **filter**: it owns NO source. The caller ticks
/// the real audio source itself and feeds the resulting frame in as this unit's
/// `input`; what lives here is the latent phase-vocoder state (one [`Vocoder`]
/// per channel, the scratch buffers, the atomics).
///
/// # Channels
///
/// One [`Vocoder`] per channel, plus one in/out [`RtScratch`] pair each. The
/// vocoders are **independent** — there is no phase locking between them, so a
/// correlated source can drift channel-to-channel. That was already true of the
/// original stereo pair; widening does not make it worse, and fixing it is a
/// separate question from width.
pub struct Unit {
    /// One per channel; the bank's length **is** the unit's width, so the
    /// scratch vectors are always the same length.
    ///
    /// Shared across graph generations — see [`Bank`]. `width` mirrors the
    /// length so `inputs()`/`outputs()` need no borrow: fundsp calls them during
    /// graph planning, where taking a borrow would collide with a live one.
    ///
    /// **Note the names.** `channels` here is the vocoder *bank*, not a count —
    /// the count is [`width`](Self::width). They were named this way before
    /// [`ChannelLayout`] existed; a blind rename would swap a `Vec<Vocoder>` for
    /// a channel count.
    pub(super) channels: Arc<Bank>,
    /// The unit's declared width — one vocoder per channel.
    pub(super) width: ChannelLayout,

    /// This handle's identity for [`Bank::claim`], unique among live handles.
    ///
    /// A counter, **not** `self as *const Self`. The address is not an identity:
    /// `Net::push(Box::new(unit))` moves the value, so a handle that claimed the
    /// bank before the move could never release its own claim afterwards — the
    /// guard would then fire on the legitimate successor. Found the hard way, by
    /// this exact bug in `a_successor_generation_continues_the_stream`.
    pub(super) id: usize,
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
        Self::with_fft_size_and_channels(sample_rate, FftSize::default(), ChannelLayout::Stereo)
    }

    /// Stereo, at a custom FFT size.
    pub fn with_fft_size(sample_rate: impl Into<SampleRate>, fft_size: FftSize) -> Self {
        Self::with_fft_size_and_channels(sample_rate, fft_size, ChannelLayout::Stereo)
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
        Self {
            channels: Bank::new((0..n).map(|_| Vocoder::new(geometry)).collect()),
            width,
            id: next_handle_id(),
            stretch_factor: Arc::new(AtomicF32::new(StretchFactor::UNITY.get())),
            pitch_cents: Arc::new(AtomicF32::new(0.0)),
            enabled: true,
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

    /// Clamped into [`StretchFactor::MIN`]..=[`StretchFactor::MAX`].
    pub fn set_stretch_factor(&self, factor: StretchFactor) {
        self.stretch_factor.store(
            StretchFactor::new_clamped(factor.get()).get(),
            Ordering::Release,
        );
    }

    pub fn stretch_factor(&self) -> StretchFactor {
        StretchFactor::new(self.stretch_factor.load(Ordering::Acquire))
    }

    /// Arc for lock-free external control.
    pub fn stretch_factor_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.stretch_factor)
    }

    /// Clamped to ±2 octaves.
    pub fn set_pitch_cents(&self, cents: Cents) {
        self.pitch_cents.store(
            cents.get().clamp(MIN_PITCH_CENTS, MAX_PITCH_CENTS),
            Ordering::Release,
        );
    }

    pub fn pitch_cents(&self) -> Cents {
        Cents::new(self.pitch_cents.load(Ordering::Acquire))
    }

    /// Arc for lock-free external control.
    pub fn pitch_cents_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.pitch_cents)
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the unit is doing anything but passing audio through.
    pub fn is_processing(&self) -> bool {
        self.enabled
            && ((self.stretch_factor().get() - StretchFactor::UNITY.get()).abs() > STRETCH_EPSILON
                || self.pitch_cents().get().abs() > PITCH_EPSILON_CENTS)
    }

    /// Whether these two units share one vocoder bank.
    ///
    /// Exposed so callers that must sever sharing before running a clone on
    /// another thread can *assert* they did — the alternative is trusting that
    /// [`AudioUnit::isolate`] was reached, which is exactly the assumption that
    /// shipped a data race in `VoiceNode`. Sharing is otherwise invisible from
    /// outside this module: it changes no output until two threads race, and by
    /// then nothing is observable in a test.
    pub fn shares_bank_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.channels, &other.channels)
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
    /// use, not only at construction: `VoiceSlot::set_stretch` keeps the resident
    /// filter and writes its atomics, so returning a stretched voice to 1.0 leaves
    /// a filter sitting at unity.
    ///
    /// Every channel reports the same value (a function of the shared FFT size),
    /// so channel 0 speaks for all.
    pub fn latency_samples(&self) -> usize {
        if !self.is_processing() {
            return 0;
        }
        self.channels
            .channels
            .borrow()
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
            .channels
            .channels
            .borrow()
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
    /// internally by consuming fed frames at
    /// [`intake_rate`](Self::intake_rate). Folding the pitch ratio in here as
    /// well would resample *twice* — once at the caller's cursor, once at the
    /// intake — and the second cancels the first exactly. Measured: with pitch
    /// folded in here, the output holds 440 Hz at every requested interval, which
    /// is the same silent no-op this fix removed.
    ///
    /// Contrast [`intake_rate`](Self::intake_rate), which carries both halves
    /// because it *is* the resample. The two are not interchangeable despite both
    /// being "samples consumed per output sample": one scales a cursor into a
    /// wave, the other scales consumption of an already-produced stream.
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

impl Clone for Unit {
    fn clone(&self) -> Self {
        // Fresh vocoder state rather than cloned: the phase accumulators and
        // overlap-add rings are mid-frame history, and a clone is a new voice
        // rather than a continuation of this one. Only the parameters carry.
        //
        // The block scratch is left **empty**, not cloned. It carries nothing
        // across blocks — `process` overwrites `scratch_in` from its input and
        // `fill(0.0)`s `scratch_out` before draining into it — so copying 64 KB
        // per channel only to overwrite it was 40% of a clone's bytes buying
        // nothing. `allocate` sizes it, which is exactly the hook fundsp
        // documents for "buffers for block processing" and which `Net::commit`
        // calls on the graph it is about to run.
        let mut cloned = Self {
            // The whole point: a refcount bump, not ~96 KB per channel.
            channels: Arc::clone(&self.channels),
            width: self.width,
            // A distinct identity: the clone is a different live handle, and the
            // guard exists precisely to tell it apart from its predecessor.
            id: next_handle_id(),
            stretch_factor: Arc::new(AtomicF32::new(self.stretch_factor.load(Ordering::Acquire))),
            pitch_cents: Arc::new(AtomicF32::new(self.pitch_cents.load(Ordering::Acquire))),
            enabled: self.enabled,
            intake_debt: 0.0,
        };
        cloned.enabled = self.enabled;
        cloned
    }
}

impl Drop for Unit {
    /// Release this handle's claim on the shared bank.
    ///
    /// A commit retires the previous generation, and its successor must be able
    /// to tick the bank it inherited. Without this the claim outlives the handle
    /// and every post-commit tick trips the guard.
    ///
    /// The body is release-only, but **dropping a `Unit` is not free**: after it
    /// runs, `channels: Arc<Bank>` is dropped too, and when that is the last
    /// reference the vocoders and block scratch (~192 KB at six channels) are
    /// deallocated right there. This comment used to claim the opposite, which
    /// was true of the body and false of the type.
    ///
    /// That matters because `VoiceCommand::Remove` retires a slot inside
    /// `drain_commands`, which runs from the audio callback. `VoicePool` now
    /// hands removed slots to a retirement channel so the free happens on the
    /// control thread — see `VoicePool::retired`. Any *other* caller dropping a
    /// `Unit` on the audio thread has the same hazard and needs the same
    /// treatment.
    fn drop(&mut self) {
        self.channels.release(self.id);
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
        // A reset restarts the stream, so it also transfers ticking rights: this
        // is the legitimate way a successor generation takes over a bank without
        // tripping the claim.
        self.channels.reclaim(self.id);
        for v in self.channels.channels.borrow_mut().iter_mut() {
            v.reset();
        }
        self.intake_debt = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        // The grid's window and hop are sample counts and its phase table is
        // their ratio, so none of the vocoder state depends on the rate. Only
        // the rate the geometry reports back does — rebuild it, and leave the
        // running phase history alone.
        for v in self.channels.channels.borrow_mut().iter_mut() {
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
        // One borrow for the whole call: the cell's contract is one borrow at a
        // time, and re-borrowing per sample would also cost a debug atomic each.
        self.channels.claim(self.id);
        let mut bank = self.channels.channels.borrow_mut();
        while self.intake_debt >= 1.0 {
            self.intake_debt -= 1.0;
            for (c, v) in bank.iter_mut().enumerate() {
                v.input.push(&[src(c)]);
                v.process(analysis_hop, synthesis_hop);
            }
        }

        let mut one = [0.0f32];
        for (c, o) in output.iter_mut().enumerate().take(n) {
            one[0] = 0.0;
            bank[c].output.drain(&mut one);
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

        // A clone shares the bank but leaves its scratch for `allocate` to size.
        // If that never ran, `RtScratch::active` clamps to a zero-length slice
        // and every loop below iterates zero times — the unit would emit silence
        // and look like a gain bug, the same failure shape that hid a 60 dB
        // error here before. Size it here instead: this is the control thread's
        // job, but a late allocation beats silent silence, and the debug assert
        // names the real fault. Every RT call on an allocated unit skips it.
        if !self.channels.scratch_is_ready(channels) {
            debug_assert!(
                false,
                "BUG: stretch::Unit::process before allocate(); the graph must \
                 call allocate() on a cloned unit before running it"
            );
            self.channels.allocate_scratch(channels);
        }

        // One claim and one borrow-set for the whole call. The scratch lives on
        // the bank now, so it is covered by the same claim that protects the
        // vocoders — a second live handle reaching this would be caught rather
        // than silently sharing working buffers.
        self.channels.claim(self.id);
        let mut scratch_in = self.channels.scratch_in.borrow_mut();

        for (c, s) in scratch_in.iter_mut().enumerate() {
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
                let buf = scratch_in[c].active_ref(size);
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
        let mut bank = self.channels.channels.borrow_mut();
        for i in 0..size {
            self.intake_debt += rate;
            while self.intake_debt >= 1.0 {
                self.intake_debt -= 1.0;
                for (c, v) in bank.iter_mut().enumerate() {
                    let sample = scratch_in[c].active_ref(size)[i];
                    v.input.push(&[sample]);
                    v.process(analysis_hop, synthesis_hop);
                }
            }
        }

        let mut scratch_out = self.channels.scratch_out.borrow_mut();
        for c in 0..channels {
            let out = scratch_out[c].active(size);
            out.fill(0.0);
            let count = bank[c].output.drain(out);
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

    /// Sever the shared vocoder bank, giving this unit private state.
    ///
    /// **This is what makes sharing sound.** `Unit::clone` hands out a refcount
    /// bump, which is safe only while generations are ticked one at a time. The
    /// offline region render breaks that: it `clone_isolated`s the live net and
    /// ticks it on a worker pool while the audio thread plays the original — two
    /// generations, two threads, concurrently. Sharing the FIFOs and phase
    /// accumulators there would corrupt both the render and playback.
    ///
    /// The render's isolation pass already calls this on every node of the clone
    /// before it reaches the worker, so the deep copy lands exactly where
    /// concurrency begins and nowhere else. Cost is the ~96 KB per channel that
    /// the commit path no longer pays, on a path that is already
    /// admission-capped for being expensive.
    ///
    /// Fresh state rather than a copy of the running one, matching what
    /// `Unit::clone` used to produce: an isolated render starts its filter clean
    /// rather than mid-frame on audio it will not emit.
    fn isolate(&mut self) {
        let geometry = self
            .channels
            .channels
            .borrow()
            .first()
            .map(|v| v.geometry)
            .unwrap_or_else(|| Self::geometry(SampleRate(44_100.0), FftSize::default()));
        let fresh: Vec<Vocoder> = self
            .channels
            .channels
            .borrow()
            .iter()
            .map(Vocoder::clone_fresh)
            .collect();
        let _ = geometry;
        self.channels = Bank::new(fresh);
        self.channels.reclaim(self.id);
        self.intake_debt = 0.0;
    }

    /// Size the per-block scratch. Idempotent, and never called from the audio
    /// thread.
    ///
    /// This is what makes [`Unit::clone`] cheap: the clone leaves the scratch
    /// empty, and the graph calls this before running the unit
    /// (`Net::commit_inner` → `Net::allocate` → `Vertex::allocate`, and
    /// `Net::set_unit` for a hot swap). Re-allocating an already-sized unit
    /// would be a needless 64 KB per channel, so a ready unit returns early.
    fn allocate(&mut self) {
        self.channels.allocate_scratch(self.stride());
    }
}
