//! In-memory sample playback with optional loop crossfade.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame, TransportReader, Wave};

use super::loop_crossfade::LoopCrossfade;

/// In-memory sample playback with optional loop crossfade.
///
/// By default, plays immediately when added to the graph (suitable for timeline clips
/// and offline export). Use `stop()` and `trigger()` for manual control if needed
/// (e.g., MIDI-triggered one-shots).
pub struct SamplerUnit {
    wave: Arc<Wave>,
    position: AtomicU64,

    /// Defaults to true (auto-play).
    playing: AtomicBool,

    looping: AtomicBool,

    gain: f32,

    speed: f32,

    sample_rate: f32,

    /// SRC ratio: file_sample_rate / session_sample_rate. 1.0 = no conversion.
    src_ratio: f32,

    /// Loop range (start, end) in samples. If None, loops entire sample.
    loop_range: Option<(u64, u64)>,

    /// Crossfade for smooth loop transitions.
    crossfade: Option<LoopCrossfade>,

    /// Optional transport for beat-synced playback.
    /// When set, sampler only plays when transport is rolling
    /// and uses beat position to compute sample offset.
    transport: Option<Arc<dyn TransportReader>>,

    /// Start position in beats on the timeline.
    start_beat: f64,

    /// Duration in beats, or None to play the entire sample.
    duration_beats: Option<f64>,
}

impl Clone for SamplerUnit {
    fn clone(&self) -> Self {
        Self {
            wave: Arc::clone(&self.wave),
            position: AtomicU64::new(self.position.load(Ordering::Relaxed)),
            playing: AtomicBool::new(self.playing.load(Ordering::Relaxed)),
            looping: AtomicBool::new(self.looping.load(Ordering::Relaxed)),
            gain: self.gain,
            speed: self.speed,
            sample_rate: self.sample_rate,
            src_ratio: self.src_ratio,
            loop_range: self.loop_range,
            crossfade: self.crossfade.clone(),
            transport: self.transport.clone(),
            start_beat: self.start_beat,
            duration_beats: self.duration_beats,
        }
    }
}

impl SamplerUnit {
    pub fn new(wave: Arc<Wave>) -> Self {
        let sample_rate = wave.sample_rate() as f32;
        Self {
            wave,
            position: AtomicU64::new(0),
            playing: AtomicBool::new(true),
            looping: AtomicBool::new(false),
            gain: 1.0,
            speed: 1.0,
            sample_rate,
            src_ratio: 1.0,
            loop_range: None,
            crossfade: None,
            transport: None,
            start_beat: 0.0,
            duration_beats: None,
        }
    }

    pub fn with_settings(wave: Arc<Wave>, gain: f32, speed: f32, looping: bool) -> Self {
        let sample_rate = wave.sample_rate() as f32;
        Self {
            wave,
            position: AtomicU64::new(0),
            playing: AtomicBool::new(true),
            looping: AtomicBool::new(looping),
            gain,
            speed,
            sample_rate,
            src_ratio: 1.0,
            loop_range: None,
            crossfade: None,
            transport: None,
            start_beat: 0.0,
            duration_beats: None,
        }
    }

    pub fn with_transport(
        wave: Arc<Wave>,
        transport: Arc<dyn TransportReader>,
        start_beat: f64,
        duration_beats: Option<f64>,
    ) -> Self {
        let sample_rate = wave.sample_rate() as f32;
        Self {
            wave,
            position: AtomicU64::new(0),
            playing: AtomicBool::new(true),
            looping: AtomicBool::new(false),
            gain: 1.0,
            speed: 1.0,
            sample_rate,
            src_ratio: 1.0,
            loop_range: None,
            crossfade: None,
            transport: Some(transport),
            start_beat,
            duration_beats,
        }
    }

    pub fn set_transport(
        &mut self,
        transport: Arc<dyn TransportReader>,
        start_beat: f64,
        duration_beats: Option<f64>,
    ) {
        self.transport = Some(transport);
        self.start_beat = start_beat;
        self.duration_beats = duration_beats;
    }

    pub fn set_placement(&mut self, start_beat: f64, duration_beats: Option<f64>) {
        self.start_beat = start_beat;
        self.duration_beats = duration_beats;
    }

    /// Used by export to inject export timeline.
    pub fn replace_transport(&mut self, transport: Arc<dyn TransportReader>) {
        self.transport = Some(transport);
    }

    pub fn has_transport(&self) -> bool {
        self.transport.is_some()
    }

    pub fn trigger(&self) {
        self.position.store(0, Ordering::Relaxed);
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn trigger_at(&self, position: u64) {
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

    pub fn set_looping(&self, looping: bool) {
        self.looping.store(looping, Ordering::Relaxed);
    }

    pub fn is_looping(&self) -> bool {
        self.looping.load(Ordering::Relaxed)
    }

    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    pub fn start_beat(&self) -> f64 {
        self.start_beat
    }

    /// None means play entire sample.
    pub fn duration_beats(&self) -> Option<f64> {
        self.duration_beats
    }

    pub fn duration_samples(&self) -> usize {
        self.wave.len()
    }

    pub fn duration_seconds(&self) -> f64 {
        self.wave.duration()
    }

    pub fn set_gain(&mut self, gain: f32) {
        self.gain = gain;
    }

    pub fn gain(&self) -> f32 {
        self.gain
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.speed = speed;
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    pub fn src_ratio(&self) -> f32 {
        self.src_ratio
    }

    pub fn wave(&self) -> &Arc<Wave> {
        &self.wave
    }

    /// Replace the wave data. Resets playback position to the start.
    ///
    /// Call from `graph_mut` — not safe to call from the audio thread directly.
    pub fn set_wave(&mut self, wave: Arc<Wave>) {
        self.sample_rate = wave.sample_rate() as f32;
        self.wave = wave;
        self.position.store(0, Ordering::Release);
    }

    /// Computes SRC ratio from file vs session sample rate.
    pub fn set_session_sample_rate(&mut self, session_rate: f64) {
        let file_rate = self.wave.sample_rate();
        self.src_ratio = if (file_rate - session_rate).abs() < 0.01 {
            1.0
        } else {
            (file_rate / session_rate) as f32
        };
    }

    pub fn set_loop_range(&mut self, loop_start: u64, loop_end: u64, crossfade_samples: usize) {
        self.loop_range = Some((loop_start, loop_end));
        self.looping.store(true, Ordering::Relaxed);

        if crossfade_samples > 0 {
            let mut xfade = LoopCrossfade::new(crossfade_samples);

            let preloop_samples: Vec<_> = (0..crossfade_samples)
                .map(|i| self.get_sample_raw(loop_start as f64 + i as f64))
                .collect();
            xfade.fill_preloop(&preloop_samples);

            self.crossfade = Some(xfade);
        } else {
            self.crossfade = None;
        }
    }

    pub fn clear_loop_range(&mut self) {
        self.loop_range = None;
        self.crossfade = None;
    }

    pub fn loop_range(&self) -> Option<(u64, u64)> {
        self.loop_range
    }

    #[inline]
    pub fn get_sample_raw(&self, position: f64) -> (f32, f32) {
        let len = self.wave.len() as f64;
        if position >= len {
            return (0.0, 0.0);
        }

        let idx = position.floor() as usize;
        let frac = position.fract() as f32;

        let (l0, r0) = if self.wave.channels() >= 2 {
            (self.wave.at(0, idx), self.wave.at(1, idx))
        } else {
            let mono = self.wave.at(0, idx);
            (mono, mono)
        };

        let next_idx = (idx + 1).min(self.wave.len().saturating_sub(1));
        let (l1, r1) = if self.wave.channels() >= 2 {
            (self.wave.at(0, next_idx), self.wave.at(1, next_idx))
        } else {
            let mono = self.wave.at(0, next_idx);
            (mono, mono)
        };

        let left = l0 + (l1 - l0) * frac;
        let right = r0 + (r1 - r0) * frac;

        (left, right)
    }

    #[inline]
    pub fn get_sample(&self, position: f64) -> (f32, f32) {
        let (l, r) = self.get_sample_raw(position);
        (l * self.gain, r * self.gain)
    }

    #[inline]
    pub fn transport_sample_position(&self) -> Option<f64> {
        let transport = self.transport.as_ref()?;
        if !transport.is_playing() {
            return None;
        }
        let current_beat = transport.current_beat();
        let beat_offset = current_beat - self.start_beat;
        if beat_offset < 0.0 {
            return None;
        }
        if let Some(dur) = self.duration_beats {
            if beat_offset >= dur {
                return None;
            }
        }
        let tempo = transport.tempo().get();
        if tempo <= 0.0 {
            return None;
        }
        let seconds_offset = beat_offset * 60.0 / tempo;
        Some(seconds_offset * self.wave.sample_rate())
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
        self.position.store(0, Ordering::Relaxed);
        self.playing.store(false, Ordering::Relaxed);
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate as f32;
        self.set_session_sample_rate(sample_rate);
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        if self.transport.is_some() {
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

        let pos_bits = self.position.load(Ordering::Relaxed);
        let pos = f64::from_bits(pos_bits);

        let (mut left, mut right) = self.get_sample(pos);

        let (loop_start, loop_end) = self
            .loop_range
            .map_or((0.0, self.wave.len() as f64), |(s, e)| (s as f64, e as f64));

        if let Some(ref mut xfade) = self.crossfade {
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

        if output.len() >= 2 {
            output[0] = left;
            output[1] = right;
        }

        let new_pos = pos + (self.speed * self.src_ratio) as f64;

        if new_pos >= loop_end {
            if self.looping.load(Ordering::Relaxed) {
                let overshoot = new_pos - loop_end;
                let wrapped = loop_start + overshoot;
                self.position.store(wrapped.to_bits(), Ordering::Relaxed);

                if let Some(ref mut xfade) = self.crossfade {
                    xfade.reset();
                }
            } else {
                self.playing.store(false, Ordering::Relaxed);
                self.position.store(loop_end.to_bits(), Ordering::Relaxed);
            }
        } else {
            self.position.store(new_pos.to_bits(), Ordering::Relaxed);
        }
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        if self.transport.is_some() {
            match self.transport_sample_position() {
                None => {
                    for i in 0..size {
                        output.set_f32(0, i, 0.0);
                        output.set_f32(1, i, 0.0);
                    }
                }
                Some(start_pos) => {
                    let advance = (self.speed * self.src_ratio) as f64;
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

        let mut pos_bits = self.position.load(Ordering::Relaxed);
        let looping = self.looping.load(Ordering::Relaxed);

        let (loop_start, loop_end) = self
            .loop_range
            .map_or((0.0, self.wave.len() as f64), |(s, e)| (s as f64, e as f64));

        let crossfade_start = self
            .crossfade
            .as_ref()
            .map_or(loop_end, |xf| loop_end - xf.len() as f64);

        for i in 0..size {
            let pos = f64::from_bits(pos_bits);

            if pos >= loop_end {
                if looping {
                    let overshoot = pos - loop_end;
                    let wrapped = loop_start + overshoot;
                    pos_bits = wrapped.to_bits();

                    if let Some(ref mut xfade) = self.crossfade {
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

            let current_pos = f64::from_bits(pos_bits);
            let (mut left, mut right) = self.get_sample(current_pos);

            if let Some(ref mut xfade) = self.crossfade {
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

            let new_pos = current_pos + (self.speed * self.src_ratio) as f64;
            pos_bits = new_pos.to_bits();
        }

        self.position.store(pos_bits, Ordering::Relaxed);
    }

    fn get_id(&self) -> u64 {
        tutti_core::node_id::SAMPLER_NODE_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(2)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::{BufferVec, SampleRate};

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

    impl TransportReader for MockTransport {
        fn current_beat(&self) -> f64 {
            self.beat
        }
        fn is_loop_enabled(&self) -> bool {
            false
        }
        fn get_loop_range(&self) -> Option<(f64, f64)> {
            None
        }
        fn is_playing(&self) -> bool {
            self.playing
        }
        fn is_recording(&self) -> bool {
            false
        }
        fn is_in_preroll(&self) -> bool {
            false
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
        assert_eq!(sampler.position(), 0);
    }

    #[test]
    fn test_sampler_trigger() {
        let wave = Wave::with_capacity(1, 44100.0, 100);
        let sampler = SamplerUnit::new(Arc::new(wave));

        sampler.trigger();
        assert!(sampler.is_playing());
        assert_eq!(sampler.position(), 0);

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

        sampler.set_loop_range(100, 500, 64);

        assert_eq!(sampler.loop_range(), Some((100, 500)));
        assert!(sampler.is_looping());

        sampler.clear_loop_range();
        assert!(sampler.loop_range().is_none());
    }

    #[test]
    fn test_loop_crossfade_integration() {
        let samples: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
        let wave = Wave::from_samples(44100.0, &samples);
        let mut sampler = SamplerUnit::new(Arc::new(wave));

        sampler.set_loop_range(10, 90, 10);
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
    fn with_settings_constructor() {
        let wave = ramp_wave(100, 44100.0);
        let sampler = SamplerUnit::with_settings(Arc::clone(&wave), 0.5, 2.0, true);

        assert!(sampler.is_playing());
        assert!(sampler.is_looping());
        assert_eq!(sampler.gain(), 0.5);
        assert_eq!(sampler.speed(), 2.0);
    }

    #[test]
    fn trigger_at_sets_position() {
        let wave = ramp_wave(100, 44100.0);
        let sampler = SamplerUnit::new(wave);

        sampler.stop();
        sampler.trigger_at(42);
        assert!(sampler.is_playing());
        assert_eq!(sampler.position(), 42);
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
        assert!(sampler.position() > 0);
        assert!(sampler.is_playing());

        sampler.reset();
        assert_eq!(sampler.position(), 0);
        assert!(!sampler.is_playing());
    }

    #[test]
    fn gain_scales_output() {
        let wave = ramp_wave(100, 44100.0);

        let mut sampler_full = SamplerUnit::new(Arc::clone(&wave));
        let mut sampler_half = SamplerUnit::new(wave);
        sampler_half.set_gain(0.5);

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
        fast.set_speed(2.0);

        let mut out = [0.0f32; 2];
        for _ in 0..10 {
            normal.tick(&[], &mut out);
            fast.tick(&[], &mut out);
        }

        let normal_pos = f64::from_bits(normal.position());
        let fast_pos = f64::from_bits(fast.position());
        assert!((fast_pos - normal_pos * 2.0).abs() < 1e-6);
    }

    #[test]
    fn src_ratio_adjusts_for_sample_rate_mismatch() {
        let wave = ramp_wave(100, 48000.0);
        let mut sampler = SamplerUnit::new(wave);
        sampler.set_session_sample_rate(24000.0);

        let mut out = [0.0f32; 2];
        sampler.tick(&[], &mut out);

        let pos = f64::from_bits(sampler.position());
        assert!((pos - 2.0).abs() < 1e-6, "48k/24k = 2x advance per tick");
    }

    #[test]
    fn src_ratio_unity_when_rates_match() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = SamplerUnit::new(wave);
        sampler.set_session_sample_rate(44100.0);

        let mut out = [0.0f32; 2];
        sampler.tick(&[], &mut out);

        let pos = f64::from_bits(sampler.position());
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
        let pos = f64::from_bits(sampler.position());
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
        let pos_after = f64::from_bits(sampler.position());
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
        sampler.set_speed(2.0);

        let mut out = [0.0f32; 2];
        // 5 ticks at speed=2 → position advances 0,2,4,6,8 → after tick 5
        // position = 10.0, wraps to 0.0
        for _ in 0..5 {
            sampler.tick(&[], &mut out);
        }
        assert!(sampler.is_playing());
        let pos = f64::from_bits(sampler.position());
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
        let mut sampler = SamplerUnit::with_transport(wave, transport, 0.0, None);

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
        let mut sampler = SamplerUnit::with_transport(wave, transport, 0.0, None);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn transport_before_start_beat_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::new(1.0, 120.0);
        let mut sampler = SamplerUnit::with_transport(wave, transport, 4.0, None);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0, "beat 1.0 < start_beat 4.0 → silence");
    }

    #[test]
    fn transport_past_duration_beats_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::new(10.0, 120.0);
        let mut sampler = SamplerUnit::with_transport(wave, transport, 0.0, Some(4.0));

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0, "beat 10.0 past duration 4.0 → silence");
    }

    #[test]
    fn transport_process_block() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::new(0.0, 120.0);
        let mut sampler = SamplerUnit::with_transport(wave, transport, 0.0, None);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(4, &input, &mut output);

        assert!((output.at_f32(0, 0) - 1.0).abs() < 1e-6, "beat 0 → sample 0");
    }

    #[test]
    fn transport_process_block_silence_when_stopped() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::stopped();
        let mut sampler = SamplerUnit::with_transport(wave, transport, 0.0, None);

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
        let sampler = SamplerUnit::with_settings(wave, 0.75, 1.5, true);
        sampler.trigger_at(42);

        let cloned = sampler.clone();
        assert_eq!(cloned.gain(), 0.75);
        assert_eq!(cloned.speed(), 1.5);
        assert!(cloned.is_looping());
        assert!(cloned.is_playing());
        assert_eq!(cloned.position(), 42);
    }

    // --- set_wave ---

    #[test]
    fn set_wave_resets_position() {
        let wave1 = ramp_wave(100, 44100.0);
        let wave2 = ramp_wave(50, 48000.0);
        let mut sampler = SamplerUnit::new(wave1);

        sampler.trigger_at(42);
        sampler.set_wave(wave2);

        assert_eq!(sampler.position(), 0);
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
        let pos = f64::from_bits(sampler.position());
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
