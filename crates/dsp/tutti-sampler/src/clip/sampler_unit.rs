//! In-memory sample playback with optional loop crossfade.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tutti_core::{
    AtomicSamplePosition, AudioUnit, Beat, BeatDuration, BufferMut, BufferRef, Linear,
    PlaybackRate, SamplePosition, SampleRate, SignalFrame, SrcRatio, Timeline, Wave,
};

use super::loop_crossfade::LoopCrossfade;
use crate::MAX_SAMPLER_CHANNELS;

/// Live loop state on a `SamplerUnit`. Internal: `Looping` carries the running
/// [`LoopCrossfade`] DSP object, which callers can neither build nor observe —
/// the public loop *intent* is [`LoopSetting`].
///
/// Modeled on the butler's `Link.loop_config`: making loop state a single enum
/// means `OneShot` renders `range`/`crossfade` unreachable, and `Looping`
/// guarantees a range — the three fields can no longer disagree.
#[derive(Default)]
pub(crate) enum LoopMode {
    /// Play through once, then stop.
    #[default]
    OneShot,
    /// Loop over `range` (start, end) in samples, with optional crossfade.
    Looping {
        range: (SamplePosition, SamplePosition),
        crossfade: Option<LoopCrossfade>,
    },
}

/// Public loop *intent* for a [`SamplerUnitConfig`] — a plain, buildable value
/// that says whether and how to loop, without exposing the live
/// [`LoopCrossfade`] runtime state. [`SamplerUnit::with_config`] converts it
/// into the internal [`LoopMode`], priming the crossfade privately.
///
/// This mirrors how the streaming/timeline loop already speaks in
/// `(start, end, crossfade_samples)` via `ClipCommand::UpdateLoop`.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum LoopSetting {
    /// Play through once, then stop.
    #[default]
    Off,
    /// Loop over `[start, end)` in samples, with `crossfade_samples` of loop
    /// crossfade (0 = hard loop, no crossfade).
    On {
        start: SamplePosition,
        end: SamplePosition,
        crossfade_samples: usize,
    },
}

/// Transport binding for beat-synced playback. Present as a whole or absent as
/// a whole: no loose `transport`/`start_beat`/`duration_beats` that can drift
/// out of sync.
pub struct TransportPlacement {
    /// Transport clock. The sampler only plays when it is rolling, and uses its
    /// beat position to compute the sample offset.
    pub transport: Arc<dyn Timeline>,
    /// Start position in beats on the timeline.
    pub start_beat: Beat,
    /// Duration in beats, or None to play the entire sample.
    pub duration_beats: Option<BeatDuration>,
}

impl Clone for TransportPlacement {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            start_beat: self.start_beat,
            duration_beats: self.duration_beats,
        }
    }
}

impl std::fmt::Debug for TransportPlacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportPlacement")
            .field("start_beat", &self.start_beat)
            .field("duration_beats", &self.duration_beats)
            .finish_non_exhaustive()
    }
}

/// Configuration for building a [`SamplerUnit`], passed to
/// [`SamplerUnit::with_config`]. Matches tutti's config-struct constructor
/// convention (`PolySynth::new(SynthConfig)`, `OfflineTimeline::new(..)`).
///
/// `Default` yields the same audible baseline as [`SamplerUnit::new`]: unity
/// gain, normal speed, one-shot, no transport binding. It is hand-written (not
/// derived) because the newtypes default to zero — a derived default would ship
/// silent (`gain = 0`) and frozen (`speed = 0`).
#[derive(Clone, Debug)]
pub struct SamplerUnitConfig {
    pub gain: Linear,
    pub speed: PlaybackRate,
    /// Loop intent. `Off` plays once; `On { .. }` loops over the range and
    /// [`SamplerUnit::with_config`] primes the crossfade internally.
    pub loop_setting: LoopSetting,
    /// Optional transport binding for beat-synced playback.
    pub placement: Option<TransportPlacement>,
    /// Output width. Defaults to stereo — see [`SamplerUnit::channels`] for why
    /// this is declared rather than taken from the wave.
    pub channels: usize,
}

impl Default for SamplerUnitConfig {
    fn default() -> Self {
        Self {
            gain: Linear::new(1.0),
            speed: PlaybackRate::UNITY,
            loop_setting: LoopSetting::Off,
            placement: None,
            channels: 2,
        }
    }
}

impl Clone for LoopMode {
    fn clone(&self) -> Self {
        match self {
            Self::OneShot => Self::OneShot,
            Self::Looping { range, crossfade } => Self::Looping {
                range: *range,
                crossfade: crossfade.clone(),
            },
        }
    }
}

impl std::fmt::Debug for LoopMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OneShot => f.write_str("OneShot"),
            Self::Looping { range, .. } => f
                .debug_struct("Looping")
                .field("range", range)
                .finish_non_exhaustive(),
        }
    }
}

/// In-memory sample playback with optional loop crossfade.
///
/// By default, plays immediately when added to the graph (suitable for timeline clips
/// and offline export). Use `stop()` and `trigger()` for manual control if needed
/// (e.g., MIDI-triggered one-shots).
pub struct SamplerUnit {
    wave: Arc<Wave>,
    position: AtomicSamplePosition,

    /// Defaults to true (auto-play).
    playing: AtomicBool,

    gain: Linear,

    /// Varispeed — user intent, bounded by the type. Composes with
    /// [`src_ratio`](Self::src_ratio) through
    /// [`PlaybackRate::read_rate`](tutti_core::PlaybackRate::read_rate); never
    /// multiplied by a bare scalar.
    speed: PlaybackRate,

    sample_rate: SampleRate,

    /// Sample-rate conversion — derived from the file and session rates, never
    /// user intent. Distinct from [`speed`](Self::speed) for that reason.
    src_ratio: SrcRatio,

    /// Loop configuration. `OneShot` plays through once; `Looping` guarantees a
    /// range and carries the optional crossfade.
    loop_mode: LoopMode,

    /// Optional transport binding for beat-synced playback.
    placement: Option<TransportPlacement>,

    /// A crossfade buffer kept resident so a loop change never allocates.
    ///
    /// `set_loop_range` runs on the audio thread (via the reader's command
    /// drain), so it takes this, re-points it with `retune`, and puts it back
    /// inside `loop_mode`. `clear_loop_range` returns it here. Built once at
    /// construction with `MAX_CROSSFADE_FRAMES` reserved.
    loop_crossfade: Option<LoopCrossfade>,

    /// Output width — this unit's `outputs()`, fixed at construction.
    ///
    /// Deliberately **not** derived from `wave.channels()`: a node whose arity
    /// followed its content would re-arity itself in the graph the moment a
    /// wider file was loaded, and `Net` edges are built against `outputs()`.
    /// The wave's own width is reconciled against this one by
    /// [`read_frame`](super::interp::read_frame)'s channel policy.
    channels: usize,
}

// Hand-rolled: `wave` is a non-`Debug` `Arc<Wave>` and `placement` holds an
// `Arc<dyn Timeline>`. Print the wave length + scalar params; never
// borrow the `Wave` samples.
impl std::fmt::Debug for SamplerUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplerUnit")
            .field("wave_frames", &self.wave.len())
            .field("position", &self.position.load(Ordering::Relaxed))
            .field("playing", &self.playing.load(Ordering::Relaxed))
            .field("gain", &self.gain)
            .field("speed", &self.speed)
            .field("sample_rate", &self.sample_rate)
            .field("src_ratio", &self.src_ratio)
            .field("loop_mode", &self.loop_mode)
            .field("has_placement", &self.placement.is_some())
            .finish_non_exhaustive()
    }
}

impl Clone for SamplerUnit {
    fn clone(&self) -> Self {
        Self {
            wave: Arc::clone(&self.wave),
            position: AtomicSamplePosition::new(self.position.load(Ordering::Relaxed)),
            playing: AtomicBool::new(self.playing.load(Ordering::Relaxed)),
            gain: self.gain,
            speed: self.speed,
            sample_rate: self.sample_rate,
            src_ratio: self.src_ratio,
            loop_mode: self.loop_mode.clone(),
            placement: self.placement.clone(),
            loop_crossfade: self.loop_crossfade.clone(),
            channels: self.channels,
        }
    }
}

impl SamplerUnit {
    /// A **stereo** sampler over `wave`.
    ///
    /// Stays stereo even for a wider wave — see [`channels`](Self::channels).
    /// Use [`with_channels`](Self::with_channels) to declare a different width.
    pub fn new(wave: Arc<Wave>) -> Self {
        let sample_rate = SampleRate::new(wave.sample_rate());
        Self {
            wave,
            position: AtomicSamplePosition::new(SamplePosition::new(0.0)),
            playing: AtomicBool::new(true),
            gain: Linear::new(1.0),
            speed: PlaybackRate::UNITY,
            sample_rate,
            src_ratio: SrcRatio::UNITY,
            loop_mode: LoopMode::OneShot,
            placement: None,
            loop_crossfade: Some(LoopCrossfade::with_channels(0, 2)),
            channels: 2,
        }
    }

    /// A `channels`-wide sampler over `wave`.
    ///
    /// The wave's own channel count is independent of this; the two are
    /// reconciled per read by [`read_frame`](super::interp::read_frame)'s
    /// channel policy (mono fans, anything else folds).
    pub fn with_channels(wave: Arc<Wave>, channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            channels,
            // Reserve at THIS width: the default from `new` is stereo-sized.
            loop_crossfade: Some(LoopCrossfade::with_channels(0, channels)),
            ..Self::new(wave)
        }
    }

    /// Output width — this unit's `outputs()`.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Build from an explicit [`SamplerUnitConfig`] — the canonical
    /// configurable constructor, matching tutti's `X::new(XConfig)` convention.
    ///
    /// A `LoopSetting::On { crossfade_samples, .. }` primes the loop crossfade
    /// from the wave (via the same path as [`set_loop_range`](Self::set_loop_range));
    /// `crossfade_samples == 0` loops with no crossfade.
    pub fn with_config(wave: Arc<Wave>, config: SamplerUnitConfig) -> Self {
        let mut unit = Self {
            gain: config.gain,
            speed: config.speed,
            placement: config.placement,
            loop_crossfade: Some(LoopCrossfade::with_channels(0, config.channels.max(1))),
            channels: config.channels.max(1),
            ..Self::new(wave)
        };
        if let LoopSetting::On {
            start,
            end,
            crossfade_samples,
        } = config.loop_setting
        {
            unit.set_loop_range(start, end, crossfade_samples);
        }
        unit
    }

    /// Convenience constructor for the common transport-bound clip case: bind a
    /// transport at `start_beat` for `duration_beats`, everything else default.
    /// Equivalent to `with_config(wave, SamplerUnitConfig { placement: Some(..),
    /// ..Default::default() })`; kept because it reads better at the three
    /// timeline call sites (tutti-synth likewise keeps convenience ctors
    /// alongside its config one).
    pub fn with_transport(
        wave: Arc<Wave>,
        transport: Arc<dyn Timeline>,
        start_beat: Beat,
        duration_beats: Option<BeatDuration>,
    ) -> Self {
        Self::with_config(
            wave,
            SamplerUnitConfig {
                placement: Some(TransportPlacement {
                    transport,
                    start_beat,
                    duration_beats,
                }),
                ..Default::default()
            },
        )
    }

    pub fn set_transport(
        &mut self,
        transport: Arc<dyn Timeline>,
        start_beat: Beat,
        duration_beats: Option<BeatDuration>,
    ) {
        self.placement = Some(TransportPlacement {
            transport,
            start_beat,
            duration_beats,
        });
    }

    pub fn set_placement(&mut self, start_beat: Beat, duration_beats: Option<BeatDuration>) {
        if let Some(placement) = &mut self.placement {
            placement.start_beat = start_beat;
            placement.duration_beats = duration_beats;
        }
    }

    /// Used by export to inject export timeline. Preserves the existing
    /// start-beat / duration when a placement is already present; otherwise
    /// binds the transport at beat 0 for the whole sample.
    pub fn replace_transport(&mut self, transport: Arc<dyn Timeline>) {
        match &mut self.placement {
            Some(placement) => placement.transport = transport,
            None => {
                self.placement = Some(TransportPlacement {
                    transport,
                    start_beat: Beat::new(0.0),
                    duration_beats: None,
                });
            }
        }
    }

    pub fn has_transport(&self) -> bool {
        self.placement.is_some()
    }

    pub fn trigger(&self) {
        self.position
            .store(SamplePosition::new(0.0), Ordering::Relaxed);
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn trigger_at(&self, position: SamplePosition) {
        self.position.store(position, Ordering::Relaxed);
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Toggle looping. Enabling loops over the whole sample (unless a range was
    /// already set); disabling drops any range and crossfade.
    pub fn set_looping(&mut self, looping: bool) {
        match (looping, &self.loop_mode) {
            // Already looping — keep the existing range/crossfade.
            (true, LoopMode::Looping { .. }) => {}
            (true, LoopMode::OneShot) => {
                self.loop_mode = LoopMode::Looping {
                    range: (
                        SamplePosition::new(0.0),
                        SamplePosition::new(self.wave.len() as f64),
                    ),
                    crossfade: None,
                };
            }
            (false, _) => {
                self.loop_mode = LoopMode::OneShot;
            }
        }
    }

    pub fn is_looping(&self) -> bool {
        matches!(self.loop_mode, LoopMode::Looping { .. })
    }

    pub fn position(&self) -> SamplePosition {
        self.position.load(Ordering::Relaxed)
    }

    pub fn start_beat(&self) -> Beat {
        self.placement
            .as_ref()
            .map_or(Beat::new(0.0), |p| p.start_beat)
    }

    /// None means play entire sample.
    pub fn duration_beats(&self) -> Option<BeatDuration> {
        self.placement.as_ref().and_then(|p| p.duration_beats)
    }

    pub fn duration_samples(&self) -> usize {
        self.wave.len()
    }

    pub fn duration_seconds(&self) -> f64 {
        self.wave.duration()
    }

    pub fn set_gain(&mut self, gain: Linear) {
        self.gain = gain;
    }

    pub fn gain(&self) -> Linear {
        self.gain
    }

    /// Set varispeed. Out-of-range and non-finite values are handled by
    /// [`PlaybackRate`]'s bounded constructor, not here — that is the point of
    /// the type: both playback tiers get the same range without either having
    /// to remember to clamp.
    pub fn set_speed(&mut self, speed: PlaybackRate) {
        self.speed = speed;
    }

    pub fn speed(&self) -> PlaybackRate {
        self.speed
    }

    pub fn src_ratio(&self) -> SrcRatio {
        self.src_ratio
    }

    /// Source samples consumed per output sample: varispeed × conversion.
    #[inline]
    fn read_rate(&self) -> f64 {
        self.speed.read_rate(self.src_ratio)
    }

    pub fn wave(&self) -> &Arc<Wave> {
        &self.wave
    }

    /// Replace the wave data. Resets playback position to the start.
    ///
    /// Call from `graph_mut` — not safe to call from the audio thread directly.
    pub fn set_wave(&mut self, wave: Arc<Wave>) {
        self.sample_rate = SampleRate::new(wave.sample_rate());
        self.wave = wave;
        self.position
            .store(SamplePosition::new(0.0), Ordering::Release);
    }

    /// Re-derive the sample-rate conversion ratio for a new session rate.
    ///
    /// The derivation itself lives in [`SrcRatio::for_rates`] — the butler's
    /// streaming path calls the same function, so the two tiers cannot drift
    /// apart on matched-rate detection or the divide-by-zero guard.
    pub fn set_session_sample_rate(&mut self, session_rate: f64) {
        self.src_ratio = SrcRatio::for_rates(self.wave.sample_rate(), session_rate);
    }

    /// Apply a [`LoopSetting`]: `On` primes the range + crossfade, `Off`
    /// clears the range and disables looping.
    ///
    /// In-RAM only. The streaming tier's loop is butler-owned (a
    /// `Command::Loop` from the reader's drain), which is why this is inherent
    /// rather than a shared trait method — there is no honest way for one call
    /// to mean both.
    pub fn set_loop_setting(&mut self, setting: LoopSetting) {
        match setting {
            LoopSetting::On {
                start,
                end,
                crossfade_samples,
            } => self.set_loop_range(start, end, crossfade_samples),
            LoopSetting::Off => {
                self.clear_loop_range();
                self.set_looping(false);
            }
        }
    }

    pub fn set_loop_range(
        &mut self,
        loop_start: SamplePosition,
        loop_end: SamplePosition,
        crossfade_samples: usize,
    ) {
        // Reuse the resident crossfade rather than building one.
        //
        // This runs on the audio thread: `ClipCommand::UpdateLoop` is drained by
        // `TrackClipReaderUnit::drain_commands`, which `tick`/`process` call. So
        // changing a loop mid-playback used to allocate `crossfade_samples *
        // channels` floats in the callback, plus a temporary buffer to read the
        // wave into. Both are gone: the crossfade's buffer is reserved once at
        // `MAX_CROSSFADE_FRAMES`, `retune` re-points it without growing, and
        // `fill_preloop_with` writes the tail in place.
        let crossfade = if crossfade_samples > 0 {
            let mut xfade = self
                .loop_crossfade
                .take()
                .unwrap_or_else(|| LoopCrossfade::with_channels(crossfade_samples, self.channels));
            xfade.retune(crossfade_samples);

            let start = loop_start.get();
            // Split the borrow: `fill_preloop_with` holds `&mut xfade` while the
            // closure reads `&self`, so the read cannot go through `self.
            // get_sample_raw_into` directly.
            let wave = Arc::clone(&self.wave);
            let gain_free_read = |i: usize, frame: &mut [f32]| {
                let pos = start + i as f64;
                if pos >= wave.len() as f64 {
                    frame.fill(0.0);
                } else {
                    super::interp::read_frame(&wave, pos, frame);
                }
            };
            xfade.fill_preloop_with(gain_free_read);

            Some(xfade)
        } else {
            None
        };

        self.loop_mode = LoopMode::Looping {
            range: (loop_start, loop_end),
            crossfade,
        };
    }

    pub fn clear_loop_range(&mut self) {
        // Reclaim the crossfade rather than dropping it, so re-enabling a loop
        // later still finds a resident buffer and stays allocation-free.
        if let LoopMode::Looping {
            crossfade: Some(xfade),
            ..
        } = std::mem::replace(&mut self.loop_mode, LoopMode::OneShot)
        {
            self.loop_crossfade = Some(xfade);
        }
    }

    pub fn loop_range(&self) -> Option<(SamplePosition, SamplePosition)> {
        match &self.loop_mode {
            LoopMode::Looping { range, .. } => Some(*range),
            LoopMode::OneShot => None,
        }
    }

    /// The current loop as a public [`LoopSetting`] intent (crossfade length
    /// recovered from the live [`LoopCrossfade`]). Lets a caller that built this
    /// unit imperatively read its loop back as a value — used by the
    /// `TrackClipReader` add shim to fold a pre-configured `SamplerUnit`'s loop
    /// into a `Playback` record.
    pub fn loop_setting(&self) -> LoopSetting {
        match &self.loop_mode {
            LoopMode::Looping { range, crossfade } => LoopSetting::On {
                start: range.0,
                end: range.1,
                crossfade_samples: crossfade.as_ref().map_or(0, |x| x.len()),
            },
            LoopMode::OneShot => LoopSetting::Off,
        }
    }

    /// Read one un-gained frame into `out` (width = `out.len()`).
    ///
    /// Writes every element, so the caller never pre-zeros. The wave's own
    /// channel count need not match `out.len()` —
    /// [`read_frame`](super::interp::read_frame) owns that policy.
    #[inline]
    pub fn get_sample_raw_into(&self, position: f64, out: &mut [f32]) {
        let len = self.wave.len() as f64;
        if position >= len {
            out.fill(0.0);
            return;
        }

        // 4-tap cubic Hermite via the shared kernel (idx-1, idx, idx+1, idx+2,
        // bound-clamped), unifying this path with `StreamingSamplerUnit`.
        super::interp::read_frame(&self.wave, position, out);
    }

    /// [`get_sample_raw_into`](Self::get_sample_raw_into) with this unit's gain
    /// applied. One scalar gain across every channel — per-channel level is the
    /// mixer strip's job, not the reader's.
    #[inline]
    pub fn get_sample_into(&self, position: f64, out: &mut [f32]) {
        self.get_sample_raw_into(position, out);
        let gain = self.gain.get();
        for s in out.iter_mut() {
            *s *= gain;
        }
    }

    /// Stereo shim over [`get_sample_raw_into`](Self::get_sample_raw_into).
    #[inline]
    pub fn get_sample_raw(&self, position: f64) -> (f32, f32) {
        let mut out = [0.0f32; 2];
        self.get_sample_raw_into(position, &mut out);
        (out[0], out[1])
    }

    /// Stereo shim over [`get_sample_into`](Self::get_sample_into).
    #[inline]
    pub fn get_sample(&self, position: f64) -> (f32, f32) {
        let mut out = [0.0f32; 2];
        self.get_sample_into(position, &mut out);
        (out[0], out[1])
    }

    #[inline]
    pub fn transport_sample_position(&self) -> Option<f64> {
        let placement = self.placement.as_ref()?;
        super::interp::transport_sample_offset(
            placement.transport.as_ref(),
            placement.start_beat,
            placement.duration_beats,
            self.wave.sample_rate(),
            self.read_rate(),
        )
    }

    /// Produce one output frame and advance whatever state that entails.
    ///
    /// The single playback algorithm. `tick` calls it once, `process` calls it
    /// per sample — the same relationship `TransportClock::tick`/`process` have
    /// in `tutti-core`. Previously the two entry points were written out
    /// separately and had drifted apart: `tick` ignored `speed` entirely for a
    /// placed clip while `process` applied it, so the same unit produced
    /// different audio depending on which the graph happened to call.
    ///
    /// Two position models live here, and the split is deliberate:
    ///
    /// - **Placed** (a timeline clip) — position is *derived* from the playhead,
    ///   so the clip cannot drift from the transport. Varispeed is folded into
    ///   the beat→sample mapping, not accumulated here.
    /// - **Free-running** (no transport) — nothing else owns this clip's time,
    ///   so it advances its own cursor by `read_rate`.
    ///
    /// `offset_in_block` is the sample's index within the current `process`
    /// call (always 0 from `tick`). The placed branch NEEDS it: a transport
    /// advances once per block, not per sample — the offline driver calls
    /// `advance(block_size)` after `process` returns
    /// (`tutti-export/src/render/driver.rs`), and `TransportClock` is
    /// emit-then-advance. Re-reading `beat()` for every sample would therefore
    /// return the same value all block long and emit a constant frame instead
    /// of the material under the playhead.
    /// Writes every element of `out` on every path, so a caller never pre-zeros
    /// and a partial write can never leave a stale channel from the previous
    /// block in a trailing slot.
    #[inline]
    fn next_frame_into(&mut self, offset_in_block: usize, out: &mut [f32]) {
        if self.placement.is_some() {
            // Derived: the transport owns the position. Step within the block by
            // `read_rate` from the block's start beat — the transport itself
            // only moves between blocks.
            match self.transport_sample_position() {
                None => out.fill(0.0),
                Some(start_pos) => {
                    let pos = start_pos + offset_in_block as f64 * self.read_rate();
                    self.get_sample_into(pos, out);
                }
            }
            return;
        }

        if !self.playing.load(Ordering::Relaxed) {
            out.fill(0.0);
            return;
        }

        let pos = self.position.load(Ordering::Relaxed).get();
        self.get_sample_into(pos, out);

        let wave_len = self.wave.len() as f64;
        let (looping, loop_start, loop_end) = match &mut self.loop_mode {
            LoopMode::OneShot => (false, 0.0, wave_len),
            LoopMode::Looping { range, crossfade } => {
                let (loop_start, loop_end) = (range.0.get(), range.1.get());
                if let Some(xfade) = crossfade {
                    let crossfade_start = loop_end - xfade.len() as f64;
                    if pos >= crossfade_start && pos < loop_end && !xfade.is_active() {
                        xfade.start();
                    }
                    if xfade.is_active() {
                        xfade.process_in_place(out);
                    }
                }
                (true, loop_start, loop_end)
            }
        };

        let new_pos = pos + self.read_rate();

        if new_pos >= loop_end {
            if looping {
                // Modulo, not `loop_start + (new_pos - loop_end)`: at high
                // varispeed one advance can overshoot a short loop by more than
                // its own length, and the subtraction form would land past the
                // loop end and never recover.
                self.position.store(
                    SamplePosition::new(wrap_into_loop(new_pos, loop_start, loop_end)),
                    Ordering::Relaxed,
                );

                if let LoopMode::Looping {
                    crossfade: Some(xfade),
                    ..
                } = &mut self.loop_mode
                {
                    xfade.reset();
                }
            } else {
                self.playing.store(false, Ordering::Relaxed);
                self.position
                    .store(SamplePosition::new(loop_end), Ordering::Relaxed);
            }
        } else {
            self.position
                .store(SamplePosition::new(new_pos), Ordering::Relaxed);
        }
    }
}

/// Wrap a position that ran past `loop_end` back into `[loop_start, loop_end)`.
///
/// Modulo rather than a single subtraction so an overshoot larger than the loop
/// itself still lands inside the region — reachable at high varispeed over a
/// short loop. A zero-or-negative-length region has nothing to wrap into, so the
/// position pins to `loop_start` rather than producing NaN.
///
/// Deliberately NOT shared with the butler's `wave_io::wrap_into`: that one is
/// integer-domain over a range the caller has already validated, while this
/// reads a fractional position and must survive a degenerate range. Unifying
/// them would put a lossy cast on the per-sample read path.
#[inline]
fn wrap_into_loop(pos: f64, loop_start: f64, loop_end: f64) -> f64 {
    let len = loop_end - loop_start;
    if len <= 0.0 {
        return loop_start;
    }
    loop_start + (pos - loop_start).rem_euclid(len)
}

impl AudioUnit for SamplerUnit {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    fn reset(&mut self) {
        self.position
            .store(SamplePosition::new(0.0), Ordering::Relaxed);
        self.playing.store(false, Ordering::Relaxed);
    }

    fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.sample_rate = sample_rate;
        self.set_session_sample_rate(sample_rate.get());
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        // The caller's slice IS the frame — no intermediate storage needed.
        let n = self.channels.min(output.len());
        self.next_frame_into(0, &mut output[..n]);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        // `BufferMut` is planar `(channel, index)` with no frame-shaped
        // accessor, so unlike `tick` this one genuinely needs a frame to
        // scatter from. Stack-allocated at the fixed ceiling and used as a
        // prefix — the house pattern (see `tutti-export`'s `fold_net_frame` and
        // the plugin hosts), and the only way to stay alloc-free at a runtime
        // width.
        let n = self
            .channels
            .min(output.channels())
            .min(MAX_SAMPLER_CHANNELS);
        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
        for i in 0..size {
            self.next_frame_into(i, &mut frame[..n]);
            for (c, &s) in frame.iter().enumerate().take(n) {
                output.set_f32(c, i, s);
            }
        }
    }

    audio_unit_boilerplate!(id = crate::node_id::SAMPLER_NODE_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        SignalFrame::new(self.channels)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use tutti_core::BufferVec;

    fn ramp_wave(len: usize, sample_rate: f64) -> Arc<Wave> {
        let samples: Vec<f32> = (0..len).map(|i| (i + 1) as f32).collect();
        Arc::new(Wave::from_samples(sample_rate, &samples))
    }

    fn stereo_ramp_wave(len: usize, sample_rate: f64) -> Arc<Wave> {
        let mut wave = Wave::zero(2, sample_rate, len as f64 / sample_rate);
        for i in 0..len {
            wave.set(0, i, (i + 1) as f32);
            wave.set(1, i, -((i + 1) as f32));
        }
        Arc::new(wave)
    }

    // --- Mock transport for beat-synced tests ---

    /// Interior-mutable so a test can advance the playhead BETWEEN blocks, the
    /// way a real transport moves. A plain-`f64` mock cannot: the beat never
    /// changes, every frame derives the same position, and an equivalence test
    /// over it passes no matter what the code does.
    struct MockTransport {
        playing: AtomicBool,
        beat: AtomicU64,
        tempo: AtomicU64,
    }

    impl MockTransport {
        fn new(beat: f64, tempo: f64) -> Arc<Self> {
            Arc::new(Self {
                playing: AtomicBool::new(true),
                beat: AtomicU64::new(beat.to_bits()),
                tempo: AtomicU64::new(tempo.to_bits()),
            })
        }

        fn stopped() -> Arc<Self> {
            let t = Self::new(0.0, 120.0);
            t.playing.store(false, Ordering::Relaxed);
            t
        }

        /// Rewind by `samples`, so a test can replay the same span twice.
        fn rewind(&self, samples: usize, sample_rate: f64) {
            let tempo = f64::from_bits(self.tempo.load(Ordering::Relaxed));
            let beats = samples as f64 * tempo / 60.0 / sample_rate;
            let now = f64::from_bits(self.beat.load(Ordering::Relaxed));
            self.beat.store((now - beats).to_bits(), Ordering::Relaxed);
        }

        /// Advance by `samples` at `sample_rate`, as a block-driven transport
        /// does after `process` returns.
        fn advance(&self, samples: usize, sample_rate: f64) {
            let tempo = f64::from_bits(self.tempo.load(Ordering::Relaxed));
            let beats = samples as f64 * tempo / 60.0 / sample_rate;
            let now = f64::from_bits(self.beat.load(Ordering::Relaxed));
            self.beat.store((now + beats).to_bits(), Ordering::Relaxed);
        }
    }

    impl Timeline for MockTransport {
        fn beat(&self) -> tutti_core::Beat {
            tutti_core::Beat(f64::from_bits(self.beat.load(Ordering::Relaxed)))
        }
        fn is_rolling(&self) -> bool {
            self.playing.load(Ordering::Relaxed)
        }
        fn tempo(&self) -> tutti_core::params::Bpm {
            tutti_core::params::Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
        }
    }

    // --- tick/process equivalence ---
    //
    // `tick` and `process` are two entry points into one algorithm, so N ticks
    // must equal one process(N) sample-for-sample. Modelled on tutti-core's
    // `advance_wraps_once_per_block_not_once_per_sample`.
    //
    // KNOWN LIMIT: these are CONSISTENCY checks, not correctness ones. Both
    // paths now call `next_frame`, so a change moves them together — an
    // injected off-by-one in the placed branch still passes here, because
    // advancing the mock one sample per tick compensates it exactly. What
    // catches that class of bug is `placed_clip_reads_across_a_block_not_dc`
    // and `placed_clip_block_step_follows_playback_rate`, which assert the
    // shape of the output *within* one block. Keep these as regression guards
    // against the two paths being rewritten apart again; do not read a pass
    // here as proof the placed branch is right.

    fn collect_ticks(unit: &mut SamplerUnit, n: usize) -> Vec<(f32, f32)> {
        (0..n)
            .map(|_| {
                let mut out = [0.0f32; 2];
                unit.tick(&[], &mut out);
                (out[0], out[1])
            })
            .collect()
    }

    fn collect_process(unit: &mut SamplerUnit, n: usize) -> Vec<(f32, f32)> {
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        output.resize(n);
        unit.process(n, &input.buffer_ref(), &mut output.buffer_mut());
        (0..n)
            .map(|i| (output.at_f32(0, i), output.at_f32(1, i)))
            .collect()
    }

    /// `tick` is one sample per call and a transport advances once per *block*,
    /// so the equivalent of `process(n)` is n single-sample blocks with the
    /// playhead moving a sample's worth between each. `transport` must be the
    /// clock both units are bound to; pass `None` for free-running units.
    ///
    /// Advancing matters: with a frozen playhead every placed frame derives the
    /// same position, both paths emit the same constant, and the assertion holds
    /// no matter what the code does.
    fn assert_tick_matches_process(
        mut a: SamplerUnit,
        mut b: SamplerUnit,
        n: usize,
        transport: Option<&Arc<MockTransport>>,
        case: &str,
    ) {
        let ticked: Vec<(f32, f32)> = (0..n)
            .map(|_| {
                let mut out = [0.0f32; 2];
                a.tick(&[], &mut out);
                if let Some(t) = transport {
                    t.advance(1, 44100.0);
                }
                (out[0], out[1])
            })
            .collect();

        // Rewind so `process` sees the same span the ticks just walked.
        if let Some(t) = transport {
            t.rewind(n, 44100.0);
        }
        let processed = collect_process(&mut b, n);

        assert_eq!(
            ticked, processed,
            "{case}: tick x{n} diverged from process({n})"
        );
    }

    #[test]
    fn tick_matches_process_free_running() {
        let wave = ramp_wave(64, 44100.0);
        assert_tick_matches_process(
            SamplerUnit::new(Arc::clone(&wave)),
            SamplerUnit::new(wave),
            16,
            None,
            "free-running",
        );
    }

    #[test]
    fn tick_matches_process_at_non_unity_speed() {
        let wave = ramp_wave(256, 44100.0);
        let build = || {
            let mut u = SamplerUnit::new(Arc::clone(&wave));
            u.set_speed(PlaybackRate::new(1.5));
            u
        };
        assert_tick_matches_process(build(), build(), 32, None, "free-running @1.5x");
    }

    #[test]
    fn tick_matches_process_when_placed() {
        // The case that was actually broken: a placed clip at non-unity speed.
        let wave = ramp_wave(4096, 44100.0);
        let transport = MockTransport::new(1.0, 120.0);
        let build = || {
            let mut u = SamplerUnit::with_config(
                Arc::clone(&wave),
                SamplerUnitConfig {
                    speed: PlaybackRate::new(0.5),
                    placement: Some(TransportPlacement {
                        transport: transport.clone(),
                        start_beat: Beat::new(0.0),
                        duration_beats: None,
                    }),
                    ..Default::default()
                },
            );
            u.set_speed(PlaybackRate::new(0.5));
            u
        };
        assert_tick_matches_process(build(), build(), 32, Some(&transport), "placed @0.5x");
    }

    #[test]
    fn tick_matches_process_across_a_loop_wrap() {
        // Span the loop boundary so the wrap arithmetic runs inside the block.
        let wave = ramp_wave(64, 44100.0);
        let build = || {
            SamplerUnit::with_config(
                Arc::clone(&wave),
                SamplerUnitConfig {
                    loop_setting: LoopSetting::On {
                        start: SamplePosition::new(0.0),
                        end: SamplePosition::new(8.0),
                        crossfade_samples: 0,
                    },
                    ..Default::default()
                },
            )
        };
        assert_tick_matches_process(build(), build(), 32, None, "loop wrap");
    }

    // --- varispeed actually reaches a placed clip ---
    //
    // The equivalence tests above cannot catch the original defect on their own:
    // both entry points now call `next_frame`, so any change affects them
    // identically. These pin the *behaviour* instead — that speed reaches the
    // placed path at all. Previously `tick` returned a position derived without
    // the rate, so a placed clip played at 1x no matter what speed was set.

    fn placed_unit(wave: &Arc<Wave>, transport: &Arc<MockTransport>, rate: f32) -> SamplerUnit {
        SamplerUnit::with_config(
            Arc::clone(wave),
            SamplerUnitConfig {
                speed: PlaybackRate::new(rate),
                placement: Some(TransportPlacement {
                    transport: transport.clone(),
                    start_beat: Beat::new(0.0),
                    duration_beats: None,
                }),
                ..Default::default()
            },
        )
    }

    #[test]
    fn placed_clip_honours_speed_in_tick() {
        // A ramp wave encodes position in its amplitude, so the sample value at
        // a fixed playhead tells us which source frame was read.
        // Beat 0.25 @ 120 BPM / 44.1 kHz = 5512.5 samples in, comfortably
        // inside a 16k wave at both 1x and 0.5x.
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::new(0.25, 120.0);

        let mut unity = placed_unit(&wave, &transport, 1.0);
        let mut half = placed_unit(&wave, &transport, 0.5);

        let a = collect_ticks(&mut unity, 1)[0].0;
        let b = collect_ticks(&mut half, 1)[0].0;

        assert!(a > 0.0 && b > 0.0, "both should be sounding: {a}, {b}");
        assert!(
            (b - a / 2.0).abs() < 2.0,
            "at 0.5x the clip should be half as far in ({a} -> expected ~{}, got {b})",
            a / 2.0
        );
    }

    #[test]
    fn placed_clip_honours_speed_in_process() {
        // The same assertion through the block path — this one always held, and
        // is here so the pair documents that the two agree for the right reason.
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::new(0.25, 120.0);

        let mut unity = placed_unit(&wave, &transport, 1.0);
        let mut half = placed_unit(&wave, &transport, 0.5);

        let a = collect_process(&mut unity, 4)[0].0;
        let b = collect_process(&mut half, 4)[0].0;

        assert!((b - a / 2.0).abs() < 2.0, "expected ~{}, got {b}", a / 2.0);
    }

    /// A placed clip must read ACROSS a block, not emit one frozen frame.
    ///
    /// A transport advances once per block — the offline driver calls
    /// `advance(block_size)` after `process` returns — so deriving position from
    /// `beat()` alone gives every sample in the block the same value. The result
    /// is constant DC where the material should be moving. Caught only by
    /// asserting *within* one `process` call: the tick/process equivalence tests
    /// cannot see it, because a frozen transport freezes both paths identically.
    #[test]
    fn placed_clip_reads_across_a_block_not_dc() {
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::new(0.25, 120.0);
        let mut u = placed_unit(&wave, &transport, 1.0);

        let block = collect_process(&mut u, 8);
        let first = block[0].0;
        let last = block[7].0;

        assert!(first > 0.0, "clip should be sounding, got {first}");
        assert!(
            last > first,
            "a ramp wave must rise across the block; got constant DC \
             (first={first}, last={last}) — the transport only moves between \
             blocks, so position must step by read_rate within one"
        );
        // Unity rate over a ramp: one source sample per output sample.
        let step = (last - first) / 7.0;
        assert!(
            (step - 1.0).abs() < 0.01,
            "at 1x the ramp should advance ~1 sample per output frame, got {step}"
        );
    }

    /// Same, at half speed: the block must still rise, but half as fast.
    #[test]
    fn placed_clip_block_step_follows_playback_rate() {
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::new(0.25, 120.0);
        let mut u = placed_unit(&wave, &transport, 0.5);

        let block = collect_process(&mut u, 8);
        let step = (block[7].0 - block[0].0) / 7.0;
        assert!(
            (step - 0.5).abs() < 0.01,
            "at 0.5x the in-block step should be ~0.5 samples/frame, got {step}"
        );
    }

    // --- loop wrap arithmetic ---

    #[test]
    fn loop_wrap_handles_overshoot_longer_than_the_loop() {
        // At high varispeed one advance can jump past the loop end by more than
        // the loop's own length. The old `loop_start + (pos - loop_end)` form
        // landed *outside* the region and never recovered; modulo lands inside.
        let wrapped = wrap_into_loop(105.0, 10.0, 20.0);
        assert!(
            (10.0..20.0).contains(&wrapped),
            "overshoot of 8.5 loop lengths must land inside [10, 20), got {wrapped}"
        );
        assert_eq!(wrapped, 15.0);
    }

    #[test]
    fn loop_wrap_is_stable_for_a_single_overshoot() {
        // The common case still behaves exactly as the subtraction form did.
        assert_eq!(wrap_into_loop(22.0, 10.0, 20.0), 12.0);
    }

    #[test]
    fn loop_wrap_survives_a_degenerate_region() {
        // A zero-length region has nothing to wrap into; pin rather than NaN.
        assert_eq!(wrap_into_loop(50.0, 10.0, 10.0), 10.0);
    }

    // --- Existing tests ---

    #[test]
    fn test_sampler_unit_creation() {
        let wave = Wave::with_capacity(1, 44100.0, 100);
        let sampler = SamplerUnit::new(Arc::new(wave));

        assert!(sampler.is_playing());
        assert!(!sampler.is_looping());
        assert_eq!(sampler.position(), SamplePosition::new(0.0));
    }

    #[test]
    fn test_sampler_trigger() {
        let wave = Wave::with_capacity(1, 44100.0, 100);
        let sampler = SamplerUnit::new(Arc::new(wave));

        sampler.trigger();
        assert!(sampler.is_playing());
        assert_eq!(sampler.position(), SamplePosition::new(0.0));

        sampler.stop();
        assert!(!sampler.is_playing());
    }

    #[test]
    fn test_sampler_outputs_silence_when_stopped() {
        let wave = Wave::with_capacity(1, 44100.0, 100);
        let mut sampler = SamplerUnit::new(Arc::new(wave));

        sampler.stop();

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn test_loop_range_api() {
        let samples = vec![0.0f32; 1000];
        let wave = Wave::from_samples(44100.0, &samples);
        let mut sampler = SamplerUnit::new(Arc::new(wave));

        assert!(sampler.loop_range().is_none());

        sampler.set_loop_range(SamplePosition::new(100.0), SamplePosition::new(500.0), 64);

        assert_eq!(
            sampler.loop_range(),
            Some((SamplePosition::new(100.0), SamplePosition::new(500.0)))
        );
        assert!(sampler.is_looping());

        sampler.clear_loop_range();
        assert!(sampler.loop_range().is_none());
    }

    #[test]
    fn test_loop_crossfade_integration() {
        let samples: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
        let wave = Wave::from_samples(44100.0, &samples);
        let mut sampler = SamplerUnit::new(Arc::new(wave));

        sampler.set_loop_range(SamplePosition::new(10.0), SamplePosition::new(90.0), 10);
        sampler.trigger();

        for _ in 0..75 {
            let mut output = [0.0f32; 2];
            sampler.tick(&[], &mut output);
        }

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert!(sampler.is_playing());
    }

    // --- New coverage tests ---

    #[test]
    fn with_config_constructor() {
        let wave = ramp_wave(100, 44100.0);
        let sampler = SamplerUnit::with_config(
            Arc::clone(&wave),
            SamplerUnitConfig {
                gain: Linear::new(0.5),
                speed: PlaybackRate::new(2.0),
                loop_setting: LoopSetting::On {
                    start: SamplePosition::new(0.0),
                    end: SamplePosition::new(100.0),
                    crossfade_samples: 0,
                },
                ..Default::default()
            },
        );

        assert!(sampler.is_playing());
        assert!(sampler.is_looping());
        assert_eq!(sampler.gain(), Linear::new(0.5));
        assert_eq!(sampler.speed(), PlaybackRate::new(2.0));
    }

    #[test]
    fn config_default_matches_new() {
        let wave = ramp_wave(100, 44100.0);
        let sampler = SamplerUnit::with_config(wave, SamplerUnitConfig::default());

        // Default config must reproduce `new`'s audible baseline: unity gain,
        // normal speed, one-shot — NOT the newtypes' zero default.
        assert_eq!(sampler.gain(), Linear::new(1.0));
        assert_eq!(sampler.speed(), PlaybackRate::new(1.0));
        assert!(!sampler.is_looping());
    }

    #[test]
    fn trigger_at_sets_position() {
        let wave = ramp_wave(100, 44100.0);
        let sampler = SamplerUnit::new(wave);

        sampler.stop();
        sampler.trigger_at(SamplePosition::new(42.0));
        assert!(sampler.is_playing());
        assert_eq!(sampler.position(), SamplePosition::new(42.0));
    }

    #[test]
    fn reset_clears_position_and_stops() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = SamplerUnit::new(wave);

        // Advance position
        let mut output = [0.0f32; 2];
        for _ in 0..10 {
            sampler.tick(&[], &mut output);
        }
        assert!(sampler.position().get() > 0.0);
        assert!(sampler.is_playing());

        sampler.reset();
        assert_eq!(sampler.position(), SamplePosition::new(0.0));
        assert!(!sampler.is_playing());
    }

    #[test]
    fn gain_scales_output() {
        let wave = ramp_wave(100, 44100.0);

        let mut sampler_full = SamplerUnit::new(Arc::clone(&wave));
        let mut sampler_half = SamplerUnit::new(wave);
        sampler_half.set_gain(Linear::new(0.5));

        let mut out_full = [0.0f32; 2];
        let mut out_half = [0.0f32; 2];

        sampler_full.tick(&[], &mut out_full);
        sampler_half.tick(&[], &mut out_half);

        assert!((out_half[0] - out_full[0] * 0.5).abs() < 1e-6);
        assert!((out_half[1] - out_full[1] * 0.5).abs() < 1e-6);
    }

    #[test]
    fn mono_wave_duplicates_to_stereo() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = SamplerUnit::new(wave);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], output[1]);
        assert!(output[0] > 0.0);
    }

    #[test]
    fn stereo_wave_preserves_channels() {
        let wave = stereo_ramp_wave(100, 44100.0);
        let mut sampler = SamplerUnit::new(wave);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert!(output[0] > 0.0);
        assert!(output[1] < 0.0);
        assert!((output[0] + output[1]).abs() < 1e-6);
    }

    #[test]
    fn speed_2x_advances_twice_as_fast() {
        let wave = ramp_wave(100, 44100.0);

        let mut normal = SamplerUnit::new(Arc::clone(&wave));
        let mut fast = SamplerUnit::new(wave);
        fast.set_speed(PlaybackRate::new(2.0));

        let mut out = [0.0f32; 2];
        for _ in 0..10 {
            normal.tick(&[], &mut out);
            fast.tick(&[], &mut out);
        }

        let normal_pos = normal.position().get();
        let fast_pos = fast.position().get();
        assert!((fast_pos - normal_pos * 2.0).abs() < 1e-6);
    }

    #[test]
    fn src_ratio_adjusts_for_sample_rate_mismatch() {
        let wave = ramp_wave(100, 48000.0);
        let mut sampler = SamplerUnit::new(wave);
        sampler.set_session_sample_rate(24000.0);

        let mut out = [0.0f32; 2];
        sampler.tick(&[], &mut out);

        let pos = sampler.position().get();
        assert!((pos - 2.0).abs() < 1e-6, "48k/24k = 2x advance per tick");
    }

    #[test]
    fn src_ratio_unity_when_rates_match() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = SamplerUnit::new(wave);
        sampler.set_session_sample_rate(44100.0);

        let mut out = [0.0f32; 2];
        sampler.tick(&[], &mut out);

        let pos = sampler.position().get();
        assert!((pos - 1.0).abs() < 1e-6);
    }

    #[test]
    fn stops_at_end_when_not_looping() {
        let wave = ramp_wave(10, 44100.0);
        let mut sampler = SamplerUnit::new(wave);

        let mut out = [0.0f32; 2];
        for _ in 0..20 {
            sampler.tick(&[], &mut out);
        }

        assert!(!sampler.is_playing());
    }

    #[test]
    fn loops_back_when_looping() {
        let wave = ramp_wave(10, 44100.0);
        let mut sampler = SamplerUnit::new(wave);
        sampler.set_looping(true);

        let mut out = [0.0f32; 2];
        // Tick exactly 10 times → position reaches 10.0, wraps to 0.0
        for _ in 0..10 {
            sampler.tick(&[], &mut out);
        }
        assert!(sampler.is_playing());
        let pos = sampler.position().get();
        assert!(
            (pos - 0.0).abs() < 1e-6,
            "10 ticks at speed=1 on len=10 should wrap to 0.0, got {pos}"
        );

        // One more tick reads sample[0] (pos=0.0 after wrap) = 1.0,
        // then advances position to 1.0.
        sampler.tick(&[], &mut out);
        assert!(
            (out[0] - 1.0).abs() < 1e-6,
            "after wrap to 0.0, should read sample[0] = 1.0, got {}",
            out[0]
        );
        let pos_after = sampler.position().get();
        assert!(
            (pos_after - 1.0).abs() < 1e-6,
            "position should advance to 1.0, got {pos_after}"
        );
    }

    #[test]
    fn looping_overshoot_at_double_speed() {
        let wave = ramp_wave(10, 44100.0);
        let mut sampler = SamplerUnit::new(wave);
        sampler.set_looping(true);
        sampler.set_speed(PlaybackRate::new(2.0));

        let mut out = [0.0f32; 2];
        // 5 ticks at speed=2 → position advances 0,2,4,6,8 → after tick 5
        // position = 10.0, wraps to 0.0
        for _ in 0..5 {
            sampler.tick(&[], &mut out);
        }
        assert!(sampler.is_playing());
        let pos = sampler.position().get();
        assert!(
            (pos - 0.0).abs() < 1e-6,
            "5 ticks at speed=2 on len=10 should wrap to 0.0, got {pos}"
        );

        // 6th tick reads sample[0] = 1.0, advances to 2.0
        sampler.tick(&[], &mut out);
        assert!(
            (out[0] - 1.0).abs() < 1e-6,
            "after wrap, sample[0] should be 1.0, got {}",
            out[0]
        );
    }

    #[test]
    fn process_block_produces_correct_samples() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = SamplerUnit::new(wave);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(4, &input, &mut output);

        assert!((output.at_f32(0, 0) - 1.0).abs() < 1e-6);
        assert!((output.at_f32(0, 1) - 2.0).abs() < 1e-6);
        assert!((output.at_f32(0, 2) - 3.0).abs() < 1e-6);
        assert!((output.at_f32(0, 3) - 4.0).abs() < 1e-6);
    }

    #[test]
    fn process_block_silence_when_stopped() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = SamplerUnit::new(wave);
        sampler.stop();

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(4, &input, &mut output);

        for i in 0..4 {
            assert_eq!(output.at_f32(0, i), 0.0);
            assert_eq!(output.at_f32(1, i), 0.0);
        }
    }

    #[test]
    fn process_stops_mid_block_when_sample_ends() {
        let wave = ramp_wave(3, 44100.0);
        let mut sampler = SamplerUnit::new(wave);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(8, &input, &mut output);

        assert!(!sampler.is_playing());
        assert!((output.at_f32(0, 0) - 1.0).abs() < 1e-6);
        assert!((output.at_f32(0, 1) - 2.0).abs() < 1e-6);
        assert!((output.at_f32(0, 2) - 3.0).abs() < 1e-6);
        assert_eq!(output.at_f32(0, 4), 0.0);
    }

    // --- Transport-driven playback ---

    #[test]
    fn transport_driven_produces_samples_at_beat_position() {
        // At 120 BPM, beat 1.0 = 0.5 seconds = 22050 samples at 44100 Hz.
        // ramp_wave has sample[i] = i+1, so sample[22050] = 22051.0.
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::new(1.0, 120.0);
        let mut sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        let expected_sample_idx = 22050.0; // 1 beat * 60/120 * 44100
        let expected_value = expected_sample_idx + 1.0; // ramp offset
        assert!(
            (output[0] - expected_value).abs() < 1.0,
            "beat 1.0 @ 120BPM/44.1k should read near sample 22050, got {}",
            output[0]
        );
    }

    #[test]
    fn transport_stopped_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::stopped();
        let mut sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn transport_before_start_beat_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::new(1.0, 120.0);
        let mut sampler = SamplerUnit::with_transport(wave, transport, Beat::new(4.0), None);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0, "beat 1.0 < start_beat 4.0 → silence");
    }

    #[test]
    fn transport_past_duration_beats_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::new(10.0, 120.0);
        let mut sampler = SamplerUnit::with_transport(
            wave,
            transport,
            Beat::new(0.0),
            Some(BeatDuration::new(4.0)),
        );

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0, "beat 10.0 past duration 4.0 → silence");
    }

    #[test]
    fn transport_process_block() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::new(0.0, 120.0);
        let mut sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(4, &input, &mut output);

        assert!(
            (output.at_f32(0, 0) - 1.0).abs() < 1e-6,
            "beat 0 → sample 0"
        );
    }

    #[test]
    fn transport_process_block_silence_when_stopped() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::stopped();
        let mut sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(4, &input, &mut output);

        for i in 0..4 {
            assert_eq!(output.at_f32(0, i), 0.0);
        }
    }

    // --- Clone ---

    #[test]
    fn clone_preserves_state() {
        let wave = ramp_wave(100, 44100.0);
        let sampler = SamplerUnit::with_config(
            wave,
            SamplerUnitConfig {
                gain: Linear::new(0.75),
                speed: PlaybackRate::new(1.5),
                loop_setting: LoopSetting::On {
                    start: SamplePosition::new(0.0),
                    end: SamplePosition::new(100.0),
                    crossfade_samples: 0,
                },
                ..Default::default()
            },
        );
        sampler.trigger_at(SamplePosition::new(42.0));

        let cloned = sampler.clone();
        assert_eq!(cloned.gain(), Linear::new(0.75));
        assert_eq!(cloned.speed(), PlaybackRate::new(1.5));
        assert!(cloned.is_looping());
        assert!(cloned.is_playing());
        assert_eq!(cloned.position(), SamplePosition::new(42.0));
    }

    // --- set_wave ---

    #[test]
    fn set_wave_resets_position() {
        let wave1 = ramp_wave(100, 44100.0);
        let wave2 = ramp_wave(50, 48000.0);
        let mut sampler = SamplerUnit::new(wave1);

        sampler.trigger_at(SamplePosition::new(42.0));
        sampler.set_wave(wave2);

        assert_eq!(sampler.position(), SamplePosition::new(0.0));
        assert_eq!(sampler.duration_samples(), 50);
    }

    // --- set_sample_rate (AudioUnit trait) ---

    #[test]
    fn set_sample_rate_updates_src_ratio() {
        let wave = ramp_wave(100, 48000.0);
        let mut sampler = SamplerUnit::new(wave);

        sampler.set_sample_rate(SampleRate(24000.0));

        let mut out = [0.0f32; 2];
        sampler.tick(&[], &mut out);
        let pos = sampler.position().get();
        assert!((pos - 2.0).abs() < 1e-6, "48k/24k = 2x advance");
    }

    // --- Interpolation at end of sample ---

    #[test]
    fn interpolation_at_last_sample_clamps() {
        let wave = ramp_wave(3, 44100.0);
        let mut sampler = SamplerUnit::new(wave);

        let mut out = [0.0f32; 2];
        sampler.tick(&[], &mut out);
        assert!((out[0] - 1.0).abs() < 1e-6);

        sampler.tick(&[], &mut out);
        assert!((out[0] - 2.0).abs() < 1e-6);

        sampler.tick(&[], &mut out);
        assert!((out[0] - 3.0).abs() < 1e-6);
    }

    #[test]
    fn looping_wraps_and_continues_producing_audio() {
        // Verify that looping a 2-sample wave keeps producing the same
        // values cyclically (not silence, not garbage).
        let wave = ramp_wave(2, 44100.0);
        let mut sampler = SamplerUnit::new(wave);
        sampler.set_looping(true);

        let mut outputs = Vec::new();
        let mut out = [0.0f32; 2];
        for _ in 0..6 {
            sampler.tick(&[], &mut out);
            outputs.push(out[0]);
        }

        assert!(sampler.is_playing());
        // ramp_wave(2) = [1.0, 2.0]. Looping: 1, 2, 1, 2, 1, 2
        assert!((outputs[0] - 1.0).abs() < 1e-6, "cycle 0 sample 0");
        assert!((outputs[2] - 1.0).abs() < 1e-6, "cycle 1 sample 0");
        assert!((outputs[4] - 1.0).abs() < 1e-6, "cycle 2 sample 0");
    }

    /// Channel `c` carries the constant `c + 1`, so a wrong-channel read is a
    /// wrong value rather than a plausible one.
    fn indexed_wave(channels: usize, len: usize) -> Arc<Wave> {
        let mut w = Wave::zero(channels, 44_100.0, len as f64 / 44_100.0);
        for i in 0..w.len() {
            for c in 0..channels {
                w.set(c, i, (c + 1) as f32);
            }
        }
        Arc::new(w)
    }

    /// Declared width, not inferred: a 6-channel wave through `new` still yields
    /// a stereo node. A node that re-arity'd itself from its content would break
    /// `Net` edges already wired against `outputs()`.
    #[test]
    fn new_stays_stereo_even_for_a_wide_wave() {
        let u = SamplerUnit::new(indexed_wave(6, 32));
        assert_eq!(u.channels(), 2);
        assert_eq!(u.outputs(), 2);
    }

    #[test]
    fn with_channels_declares_the_width() {
        let u = SamplerUnit::with_channels(indexed_wave(6, 32), 6);
        assert_eq!(u.channels(), 6);
        assert_eq!(u.outputs(), 6);
        assert_eq!(
            SamplerUnit::with_channels(indexed_wave(2, 32), 0).channels(),
            1
        );
    }

    /// `route`'s width must track `outputs()` or fundsp mis-plans this node's
    /// latency — silent except as PDC drift.
    #[test]
    fn route_width_tracks_outputs() {
        for w in [1usize, 2, 6, 8] {
            let mut u = SamplerUnit::with_channels(indexed_wave(2, 32), w);
            let out = u.route(&SignalFrame::new(0), 44_100.0);
            assert_eq!(
                out.len(),
                u.outputs(),
                "route/outputs disagree at width {w}"
            );
        }
    }

    /// All six channels must reach all six outputs, through BOTH entry points.
    /// `tick` writes the caller's slice directly while `process` scatters from a
    /// stack frame into a planar buffer — different code, so both are checked.
    #[test]
    fn six_channel_wave_reaches_all_six_outputs() {
        let mut u = SamplerUnit::with_channels(indexed_wave(6, 64), 6);

        let mut out = [0.0f32; 6];
        u.tick(&[], &mut out);
        for (c, &got) in out.iter().enumerate() {
            assert!(
                (got - (c + 1) as f32).abs() < 1e-4,
                "tick: channel {c} should carry {}, got {got} ({out:?})",
                c + 1
            );
        }

        let mut u = SamplerUnit::with_channels(indexed_wave(6, 64), 6);
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(6);
        u.process(8, &input.buffer_ref(), &mut output.buffer_mut());
        let buf = output.buffer_ref();
        for c in 0..6 {
            let got = buf.at_f32(c, 0);
            assert!(
                (got - (c + 1) as f32).abs() < 1e-4,
                "process: channel {c} should carry {}, got {got}",
                c + 1
            );
        }
    }

    /// Gain is one scalar across every channel — per-channel level is the mixer
    /// strip's job, not the reader's.
    #[test]
    fn gain_applies_uniformly_across_all_channels() {
        let mut u = SamplerUnit::with_channels(indexed_wave(6, 64), 6);
        u.set_gain(Linear::new(0.5));
        let mut out = [0.0f32; 6];
        u.tick(&[], &mut out);
        for (c, &got) in out.iter().enumerate() {
            let want = (c + 1) as f32 * 0.5;
            assert!(
                (got - want).abs() < 1e-4,
                "channel {c}: expected {want}, got {got}"
            );
        }
    }

    /// A looping 6-channel clip must keep every channel through the crossfade.
    /// The crossfade blends in place across the whole frame, so a stereo-shaped
    /// blend would leave channels 2..6 un-faded (or worse, untouched).
    #[test]
    fn six_channel_loop_crossfade_covers_every_channel() {
        let mut u = SamplerUnit::with_channels(indexed_wave(6, 64), 6);
        u.set_loop_range(SamplePosition::new(0.0), SamplePosition::new(16.0), 4);
        let mut out = [0.0f32; 6];
        // Drive past the loop point so the crossfade engages at least once.
        for _ in 0..40 {
            u.tick(&[], &mut out);
            for (c, &s) in out.iter().enumerate() {
                assert!(
                    s.is_finite(),
                    "channel {c} produced a non-finite sample during loop crossfade"
                );
            }
        }
    }
}
