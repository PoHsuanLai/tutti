//! In-memory sample playback with optional loop crossfade.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tutti_core::{
    AtomicSamplePosition, AudioUnit, BeatDuration, Beat, BufferMut, BufferRef, Linear,
    Ratio, SamplePosition, SampleRate, Timeline, Wave,
};

use super::loop_crossfade::LoopCrossfade;

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
    pub speed: Ratio,
    /// Loop intent. `Off` plays once; `On { .. }` loops over the range and
    /// [`SamplerUnit::with_config`] primes the crossfade internally.
    pub loop_setting: LoopSetting,
    /// Optional transport binding for beat-synced playback.
    pub placement: Option<TransportPlacement>,
}

impl Default for SamplerUnitConfig {
    fn default() -> Self {
        Self {
            gain: Linear::new(1.0),
            speed: Ratio::new(1.0),
            loop_setting: LoopSetting::Off,
            placement: None,
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

    speed: Ratio,

    sample_rate: SampleRate,

    /// SRC ratio: file_sample_rate / session_sample_rate. 1.0 = no conversion.
    src_ratio: Ratio,

    /// Loop configuration. `OneShot` plays through once; `Looping` guarantees a
    /// range and carries the optional crossfade.
    loop_mode: LoopMode,

    /// Optional transport binding for beat-synced playback.
    placement: Option<TransportPlacement>,
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
        }
    }
}

impl SamplerUnit {
    pub fn new(wave: Arc<Wave>) -> Self {
        let sample_rate = SampleRate::new(wave.sample_rate());
        Self {
            wave,
            position: AtomicSamplePosition::new(SamplePosition::new(0.0)),
            playing: AtomicBool::new(true),
            gain: Linear::new(1.0),
            speed: Ratio::new(1.0),
            sample_rate,
            src_ratio: Ratio::new(1.0),
            loop_mode: LoopMode::OneShot,
            placement: None,
        }
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

    pub fn set_placement(
        &mut self,
        start_beat: Beat,
        duration_beats: Option<BeatDuration>,
    ) {
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

    pub fn set_speed(&mut self, speed: Ratio) {
        self.speed = speed;
    }

    pub fn speed(&self) -> Ratio {
        self.speed
    }

    pub fn src_ratio(&self) -> Ratio {
        self.src_ratio
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

    /// Computes SRC ratio from file vs session sample rate.
    pub fn set_session_sample_rate(&mut self, session_rate: f64) {
        let file_rate = self.wave.sample_rate();
        self.src_ratio = Ratio::new(if (file_rate - session_rate).abs() < 0.01 {
            1.0
        } else {
            (file_rate / session_rate) as f32
        });
    }

    pub fn set_loop_range(
        &mut self,
        loop_start: SamplePosition,
        loop_end: SamplePosition,
        crossfade_samples: usize,
    ) {
        let crossfade = if crossfade_samples > 0 {
            let mut xfade = LoopCrossfade::new(crossfade_samples);

            let preloop_samples: Vec<_> = (0..crossfade_samples)
                .map(|i| self.get_sample_raw(loop_start.get() + i as f64))
                .collect();
            xfade.fill_preloop(&preloop_samples);

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
        self.loop_mode = LoopMode::OneShot;
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

    #[inline]
    pub fn get_sample_raw(&self, position: f64) -> (f32, f32) {
        let len = self.wave.len() as f64;
        if position >= len {
            return (0.0, 0.0);
        }

        // 4-tap cubic Hermite via the shared kernel (idx-1, idx, idx+1, idx+2,
        // bound-clamped), unifying this path with `StreamingSamplerUnit`.
        super::interp::read_stereo_frame(&self.wave, position)
    }

    #[inline]
    pub fn get_sample(&self, position: f64) -> (f32, f32) {
        let (l, r) = self.get_sample_raw(position);
        let gain = self.gain.get();
        (l * gain, r * gain)
    }

    #[inline]
    pub fn transport_sample_position(&self) -> Option<f64> {
        let placement = self.placement.as_ref()?;
        super::interp::transport_sample_offset(
            placement.transport.as_ref(),
            placement.start_beat,
            placement.duration_beats,
            self.wave.sample_rate(),
        )
    }
}

impl super::clip_reader::ClipReader for SamplerUnit {
    fn set_gain(&mut self, gain: Linear) {
        SamplerUnit::set_gain(self, gain);
    }

    fn set_placement(&mut self, start_beat: Beat, duration: Option<BeatDuration>) {
        SamplerUnit::set_placement(self, start_beat, duration);
    }

    fn set_speed(&mut self, speed: Ratio) {
        SamplerUnit::set_speed(self, speed);
    }

    fn set_direction(&mut self, _direction: super::track_clip_reader::Direction) {
        // Deliberate no-op. For the in-RAM backend, direction is carried on the
        // reader's per-slot `direction` (consumed by the reversed index in the hot
        // read), not inside the `SamplerUnit`. The `UpdateReverse` drain arm sets
        // that slot field directly; there is no source-side direction state to
        // mutate here.
    }

    fn set_loop(&mut self, setting: LoopSetting) {
        // Same behavior as the `ClipCommand::UpdateLoop` / `ClearLoop` drain
        // arms: `On` primes the loop range + crossfade; `Off` clears the range
        // and disables looping.
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

    fn seek(&mut self, to: SamplePosition) {
        // In-RAM seek is the instant position store; `trigger_at` also arms
        // playback, matching the existing manual-seek semantics.
        self.trigger_at(to);
    }

    fn set_wave(&mut self, wave: Arc<Wave>) {
        SamplerUnit::set_wave(self, wave);
    }

    fn play(&self) {
        SamplerUnit::play(self);
    }

    fn stop(&self) {
        SamplerUnit::stop(self);
    }

    fn is_playing(&self) -> bool {
        SamplerUnit::is_playing(self)
    }
}

impl AudioUnit for SamplerUnit {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
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
        if self.placement.is_some() {
            if output.len() >= 2 {
                match self.transport_sample_position() {
                    None => {
                        output[0] = 0.0;
                        output[1] = 0.0;
                    }
                    Some(pos) => {
                        let (left, right) = self.get_sample(pos);
                        output[0] = left;
                        output[1] = right;
                    }
                }
            }
            return;
        }

        if !self.playing.load(Ordering::Relaxed) {
            if output.len() >= 2 {
                output[0] = 0.0;
                output[1] = 0.0;
            }
            return;
        }

        let pos = self.position.load(Ordering::Relaxed).get();

        let (mut left, mut right) = self.get_sample(pos);

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
                        let sample = xfade.process((left, right));
                        left = sample.0;
                        right = sample.1;
                    }
                }
                (true, loop_start, loop_end)
            }
        };

        if output.len() >= 2 {
            output[0] = left;
            output[1] = right;
        }

        let new_pos = pos + (self.speed.get() * self.src_ratio.get()) as f64;

        if new_pos >= loop_end {
            if looping {
                let overshoot = new_pos - loop_end;
                let wrapped = loop_start + overshoot;
                self.position
                    .store(SamplePosition::new(wrapped), Ordering::Relaxed);

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

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        if self.placement.is_some() {
            match self.transport_sample_position() {
                None => {
                    for i in 0..size {
                        output.set_f32(0, i, 0.0);
                        output.set_f32(1, i, 0.0);
                    }
                }
                Some(start_pos) => {
                    let advance = (self.speed.get() * self.src_ratio.get()) as f64;
                    for i in 0..size {
                        let pos = start_pos + i as f64 * advance;
                        let (left, right) = self.get_sample(pos);
                        output.set_f32(0, i, left);
                        output.set_f32(1, i, right);
                    }
                }
            }
            return;
        }

        if !self.playing.load(Ordering::Relaxed) {
            for i in 0..size {
                output.set_f32(0, i, 0.0);
                output.set_f32(1, i, 0.0);
            }
            return;
        }

        let mut pos = self.position.load(Ordering::Relaxed).get();
        let wave_len = self.wave.len() as f64;

        let (looping, loop_start, loop_end) = match &self.loop_mode {
            LoopMode::OneShot => (false, 0.0, wave_len),
            LoopMode::Looping { range, .. } => (true, range.0.get(), range.1.get()),
        };

        let crossfade_start = match &self.loop_mode {
            LoopMode::Looping {
                crossfade: Some(xf),
                ..
            } => loop_end - xf.len() as f64,
            _ => loop_end,
        };

        for i in 0..size {
            if pos >= loop_end {
                if looping {
                    let overshoot = pos - loop_end;
                    pos = loop_start + overshoot;

                    if let LoopMode::Looping {
                        crossfade: Some(xfade),
                        ..
                    } = &mut self.loop_mode
                    {
                        xfade.reset();
                    }
                } else {
                    self.playing.store(false, Ordering::Relaxed);
                    for j in i..size {
                        output.set_f32(0, j, 0.0);
                        output.set_f32(1, j, 0.0);
                    }
                    break;
                }
            }

            let current_pos = pos;
            let (mut left, mut right) = self.get_sample(current_pos);

            if let LoopMode::Looping {
                crossfade: Some(xfade),
                ..
            } = &mut self.loop_mode
            {
                if current_pos >= crossfade_start && current_pos < loop_end && !xfade.is_active() {
                    xfade.start();
                }
                if xfade.is_active() {
                    let sample = xfade.process((left, right));
                    left = sample.0;
                    right = sample.1;
                }
            }

            output.set_f32(0, i, left);
            output.set_f32(1, i, right);

            pos = current_pos + (self.speed.get() * self.src_ratio.get()) as f64;
        }

        self.position
            .store(SamplePosition::new(pos), Ordering::Relaxed);
    }

    audio_unit_boilerplate!(id = crate::node_id::SAMPLER_NODE_ID, outputs = 2);
}

#[cfg(test)]
mod tests {
    use super::*;
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

    struct MockTransport {
        playing: bool,
        beat: f64,
        tempo: f64,
    }

    impl MockTransport {
        fn new(beat: f64, tempo: f64) -> Arc<Self> {
            Arc::new(Self {
                playing: true,
                beat,
                tempo,
            })
        }

        fn stopped() -> Arc<Self> {
            Arc::new(Self {
                playing: false,
                beat: 0.0,
                tempo: 120.0,
            })
        }
    }

    impl Timeline for MockTransport {
        fn beat(&self) -> tutti_core::Beat {
            tutti_core::Beat(self.beat)
        }
        fn loop_range(&self) -> Option<tutti_core::LoopRange> {
            None
        }
        fn is_rolling(&self) -> bool {
            self.playing
        }
        fn tempo(&self) -> tutti_core::params::Bpm {
            tutti_core::params::Bpm::new(self.tempo)
        }
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
                speed: Ratio::new(2.0),
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
        assert_eq!(sampler.speed(), Ratio::new(2.0));
    }

    #[test]
    fn config_default_matches_new() {
        let wave = ramp_wave(100, 44100.0);
        let sampler = SamplerUnit::with_config(wave, SamplerUnitConfig::default());

        // Default config must reproduce `new`'s audible baseline: unity gain,
        // normal speed, one-shot — NOT the newtypes' zero default.
        assert_eq!(sampler.gain(), Linear::new(1.0));
        assert_eq!(sampler.speed(), Ratio::new(1.0));
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
        fast.set_speed(Ratio::new(2.0));

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
        sampler.set_speed(Ratio::new(2.0));

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
        let mut sampler =
            SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);

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
        let mut sampler =
            SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn transport_before_start_beat_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::new(1.0, 120.0);
        let mut sampler =
            SamplerUnit::with_transport(wave, transport, Beat::new(4.0), None);

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
        let mut sampler =
            SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);

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
        let mut sampler =
            SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);

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
                speed: Ratio::new(1.5),
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
        assert_eq!(cloned.speed(), Ratio::new(1.5));
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
}
