//! Sample-accurate transport clock.

use super::state::{ClockInputs, LoopSpan, SeekSlot};
use crate::{AtomicBool, AtomicF64, Ordering};
use fundsp::prelude::*;
use std::any;
use std::sync::Arc;

/// Wrap a beat position back into the loop range, preserving overshoot.
#[inline]
fn wrap_beat(beat: f64, loop_start: f64, loop_end: f64) -> f64 {
    let overshoot = beat - loop_end;
    let loop_length = loop_end - loop_start;
    loop_start + (overshoot % loop_length)
}

/// Split a beat into floor (whole beats) and fractional part.
/// Returns (floor as f32, fraction as f32) for dual-channel output.
#[inline]
fn split_beat(beat: f64) -> (f32, f32) {
    let floor = beat.floor();
    (floor as f32, (beat - floor) as f32)
}

pub struct TransportClock {
    current_beat: f64,
    tempo: Arc<AtomicF64>,
    paused: Arc<AtomicBool>,
    seek: SeekSlot,
    position_writeback: Option<Arc<AtomicF64>>,
    /// `None` = this clock ignores looping entirely (offline renders).
    loop_span: Option<LoopSpan>,
    sample_rate: f64,
    beat_per_sample: f64,
    last_tempo: f64,
}

impl TransportClock {
    #[inline]
    fn calculate_beat_per_sample(
        tempo: impl Into<crate::Bpm>,
        sample_rate: impl Into<crate::SampleRate>,
    ) -> f64 {
        let tempo = tempo.into().get();
        let sample_rate = sample_rate.into().get();
        (tempo / 60.0) / sample_rate
    }

    pub fn new(
        tempo: Arc<AtomicF64>,
        paused: Arc<AtomicBool>,
        sample_rate: impl Into<crate::SampleRate>,
    ) -> Self {
        let sample_rate = sample_rate.into().get();
        let initial_tempo = tempo.load(Ordering::Acquire);

        Self {
            current_beat: 0.0,
            tempo,
            paused,
            seek: SeekSlot::new(),
            position_writeback: None,
            loop_span: None,
            sample_rate,
            beat_per_sample: Self::calculate_beat_per_sample(initial_tempo, sample_rate),
            last_tempo: initial_tempo,
        }
    }

    /// Build a clock from the transport's whole input surface.
    ///
    /// Replaces the `new(..).with_seek(..).with_loop(..)` chain and the eight
    /// loose `Arc` clones it required at each construction site.
    pub fn from_inputs(inputs: ClockInputs, sample_rate: impl Into<crate::SampleRate>) -> Self {
        let ClockInputs {
            tempo,
            paused,
            seek,
            loop_span,
        } = inputs;
        let mut clock = Self::new(tempo, paused, sample_rate);
        clock.seek = seek;
        clock.loop_span = Some(loop_span);
        clock
    }

    pub fn with_seek(mut self, seek: SeekSlot) -> Self {
        self.seek = seek;
        self
    }

    pub fn with_loop(mut self, loop_span: LoopSpan) -> Self {
        self.loop_span = Some(loop_span);
        self
    }

    pub fn with_position_writeback(mut self, writeback: Arc<AtomicF64>) -> Self {
        self.position_writeback = Some(writeback);
        self
    }

    pub fn seek_slot(&self) -> SeekSlot {
        self.seek.clone()
    }

    pub fn seek(&self, beat: f64) {
        self.seek.request(beat);
    }

    pub fn current_beat(&self) -> f64 {
        self.current_beat
    }

    #[inline]
    fn update_tempo_if_changed(&mut self) {
        let current_tempo = self.tempo.load(Ordering::Acquire);
        if (current_tempo - self.last_tempo).abs() > 0.001 {
            self.beat_per_sample = Self::calculate_beat_per_sample(current_tempo, self.sample_rate);
            self.last_tempo = current_tempo;
        }
    }

    #[inline]
    fn apply_pending_seek(&mut self) {
        if let Some(target) = self.seek.take() {
            self.current_beat = target;
        }
    }

    #[inline]
    fn apply_loop_wrap(&mut self) {
        let Some((loop_start, loop_end)) = self.loop_span.as_ref().and_then(LoopSpan::range) else {
            return;
        };

        if loop_end > loop_start && self.current_beat >= loop_end {
            self.current_beat = wrap_beat(self.current_beat, loop_start, loop_end);
        }
    }
}

impl AudioUnit for TransportClock {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.current_beat = 0.0;
    }

    /// Sever every shared link to the *live* transport so this clone can be
    /// ticked on a worker thread (an offline region render) without disturbing
    /// live playback.
    ///
    /// `Clone` shares all of `tempo`/`paused`/`seek_*`/loop/`position_writeback`
    /// by `Arc` (correct for the commit-clone, where only the original is
    /// ticked). But an offline render ticks this clone for seconds while the
    /// live graph plays, and the clock **writes** `position_writeback` and reads
    /// `paused`/`seek_*` every buffer — so the worker would stomp the live
    /// playhead (the writeback the live playback reads as "current beat") and
    /// consume live seeks, jerking the live samplers to garbage positions →
    /// continuous noise for the whole render.
    ///
    /// Snapshot the live tempo into a fresh private atomic, force unpaused with
    /// no pending seek (the render advances its own linear window), and — most
    /// importantly — drop `position_writeback` and the loop atomics so this
    /// clock writes to nothing live and reads no live loop/seek state. The
    /// render's actual length/start is governed by the offline transport and
    /// region bounds, not by this clock's loop fields.
    fn isolate(&mut self) {
        let tempo_now = self.tempo.load(Ordering::Acquire);
        self.tempo = Arc::new(AtomicF64::new(tempo_now));
        self.paused = Arc::new(AtomicBool::new(false));
        self.seek = SeekSlot::new();
        self.position_writeback = None;
        self.loop_span = None;
    }

    fn set_sample_rate(&mut self, sample_rate: crate::params::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.beat_per_sample =
            Self::calculate_beat_per_sample(self.tempo.load(Ordering::Acquire), sample_rate);
    }

    #[inline]
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        self.apply_pending_seek();
        self.update_tempo_if_changed();

        let (whole, frac) = split_beat(self.current_beat);
        output[0] = whole;
        output[1] = frac;

        if !self.paused.load(Ordering::Acquire) {
            self.current_beat += self.beat_per_sample;
            self.apply_loop_wrap();
        }

        if let Some(ref writeback) = self.position_writeback {
            writeback.store(self.current_beat, Ordering::Release);
        }
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        self.apply_pending_seek();
        self.update_tempo_if_changed();

        let is_paused = self.paused.load(Ordering::Acquire);
        // Hoisted once per buffer: the loop region cannot change mid-block.
        let active_loop = self.loop_span.as_ref().and_then(LoopSpan::range);

        if is_paused {
            let (whole, frac) = split_beat(self.current_beat);
            for i in 0..size {
                output.set_f32(0, i, whole);
                output.set_f32(1, i, frac);
            }
        } else if let Some((loop_start, loop_end)) = active_loop.filter(|(start, end)| end > start)
        {
            for i in 0..size {
                let (whole, frac) = split_beat(self.current_beat);
                output.set_f32(0, i, whole);
                output.set_f32(1, i, frac);
                self.current_beat += self.beat_per_sample;

                if self.current_beat >= loop_end {
                    self.current_beat = wrap_beat(self.current_beat, loop_start, loop_end);
                }
            }
        } else {
            for i in 0..size {
                let (whole, frac) = split_beat(self.current_beat);
                output.set_f32(0, i, whole);
                output.set_f32(1, i, frac);
                self.current_beat += self.beat_per_sample;
            }
        }

        if let Some(ref writeback) = self.position_writeback {
            writeback.store(self.current_beat, Ordering::Release);
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::TRANSPORT_CLOCK_ID
    }

    fn as_any(&self) -> &dyn any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn any::Any {
        self
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let (whole, frac) = split_beat(self.current_beat);
        let mut output = SignalFrame::new(2);
        output.set(0, Signal::Value(f64::from(whole)));
        output.set(1, Signal::Value(f64::from(frac)));
        output
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl Clone for TransportClock {
    fn clone(&self) -> Self {
        Self {
            current_beat: self.current_beat,
            tempo: Arc::clone(&self.tempo),
            paused: Arc::clone(&self.paused),
            seek: self.seek.clone(),
            position_writeback: self.position_writeback.as_ref().map(Arc::clone),
            loop_span: self.loop_span.clone(),
            sample_rate: self.sample_rate,
            beat_per_sample: self.beat_per_sample,
            last_tempo: self.last_tempo,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_atomics() -> (Arc<AtomicF64>, Arc<AtomicBool>) {
        (
            Arc::new(AtomicF64::new(120.0)),  // 120 BPM
            Arc::new(AtomicBool::new(false)), // Not paused
        )
    }

    fn reconstruct_beat(output: &[f32; 2]) -> f32 {
        output[0] + output[1]
    }

    #[test]
    fn test_transport_clock_creation() {
        let (tempo, paused) = create_test_atomics();
        let clock = TransportClock::new(tempo, paused, 44100.0);

        assert_eq!(clock.inputs(), 0);
        assert_eq!(clock.outputs(), 2);
        assert!((clock.current_beat() - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_transport_clock_tick() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(tempo, paused, 44100.0);

        let mut output = [0.0f32; 2];

        clock.tick(&[], &mut output);
        assert!((reconstruct_beat(&output) - 0.0).abs() < 0.001);

        for _ in 0..44100 {
            clock.tick(&[], &mut output);
        }

        assert!((reconstruct_beat(&output) - 2.0).abs() < 0.01);
    }

    /// Regression: an offline region render clones the live net and ticks the
    /// clone on a worker thread. `Clone` shares the live `position_writeback`
    /// (and `paused`/`seek_*`) by `Arc`, and `tick` *writes* the writeback every
    /// sample — so the worker would stomp the live playhead the rest of the app
    /// reads as "current beat", jerking live samplers to garbage positions →
    /// continuous noise for the whole render. `isolate()` must sever these.
    #[test]
    fn isolate_severs_live_position_writeback() {
        let (tempo, paused) = create_test_atomics();
        let live_position = Arc::new(AtomicF64::new(7.5)); // live playhead "now"
        let clock = TransportClock::new(tempo, paused, 44100.0)
            .with_position_writeback(Arc::clone(&live_position));

        // The render's clone, isolated as the render's rebind pass does.
        let mut render = clock.clone();
        render.isolate();

        // Tick the isolated clone a full second — if it still shared the
        // writeback, it would overwrite `live_position` with its own advancing
        // beat (starting from 0.0), wrecking the live playhead.
        let mut out = [0.0f32; 2];
        for _ in 0..44100 {
            render.tick(&[], &mut out);
        }

        assert_eq!(
            live_position.load(Ordering::Acquire),
            7.5,
            "live playhead must be untouched by the isolated render clock"
        );
        // The clone still advances its own private beat (~2 beats at 120 BPM),
        // so the render actually produces audio over its window.
        assert!(
            (render.current_beat() - 2.0).abs() < 0.01,
            "isolated clock must still advance its own beat, got {}",
            render.current_beat()
        );
    }

    #[test]
    fn test_transport_clock_pause() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(tempo, paused.clone(), 44100.0);

        let mut output = [0.0f32; 2];

        for _ in 0..1000 {
            clock.tick(&[], &mut output);
        }
        let beat_before_pause = reconstruct_beat(&output);

        paused.store(true, Ordering::Release);

        for _ in 0..1000 {
            clock.tick(&[], &mut output);
        }

        assert!((reconstruct_beat(&output) - beat_before_pause).abs() < 0.001);
    }

    #[test]
    fn test_transport_clock_seek() {
        let (tempo, paused) = create_test_atomics();
        let clock = TransportClock::new(tempo, paused, 44100.0);

        clock.seek(4.0);

        let mut clock = clock;
        let mut output = [0.0f32; 2];
        clock.tick(&[], &mut output);

        assert!((reconstruct_beat(&output) - 4.0).abs() < 0.001);
    }

    #[test]
    fn test_transport_clock_tempo_change() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(tempo.clone(), paused, 44100.0);

        let mut output = [0.0f32; 2];

        for _ in 0..44100 {
            clock.tick(&[], &mut output);
        }

        tempo.store(240.0, Ordering::Release);

        for _ in 0..44100 {
            clock.tick(&[], &mut output);
        }

        assert!((reconstruct_beat(&output) - 6.0).abs() < 0.1);
    }

    #[test]
    fn test_transport_clock_loop_wrapping() {
        let (tempo, paused) = create_test_atomics();
        let loop_span = LoopSpan::new(0.0, 4.0);
        loop_span.set_enabled(true);

        let mut clock = TransportClock::new(tempo, paused, 44100.0).with_loop(loop_span);

        let mut output = [0.0f32; 2];

        for _ in 0..90000 {
            clock.tick(&[], &mut output);
        }

        let beat = reconstruct_beat(&output);
        assert!(beat < 0.5, "Expected beat near 0 after loop, got {}", beat);
        assert!(beat >= 0.0, "Beat should be >= 0 after wrap");
    }

    #[test]
    fn test_transport_clock_loop_disabled() {
        let (tempo, paused) = create_test_atomics();
        let loop_span = LoopSpan::new(0.0, 4.0);
        loop_span.set_enabled(false);

        let mut clock = TransportClock::new(tempo, paused, 44100.0).with_loop(loop_span);

        let mut output = [0.0f32; 2];

        for _ in 0..90000 {
            clock.tick(&[], &mut output);
        }

        let beat = reconstruct_beat(&output);
        assert!(
            beat > 4.0,
            "Expected beat > 4.0 with loop disabled, got {}",
            beat
        );
    }

    #[test]
    fn test_transport_clock_loop_overshoot_precision() {
        let (tempo, paused) = create_test_atomics();
        let loop_span = LoopSpan::new(0.0, 1.0);
        loop_span.set_enabled(true);

        let mut clock = TransportClock::new(tempo, paused, 44100.0).with_loop(loop_span);

        clock.seek(0.99999);
        let mut output = [0.0f32; 2];
        clock.tick(&[], &mut output);

        clock.tick(&[], &mut output);

        let beat = reconstruct_beat(&output);
        assert!(beat >= 0.0, "Beat should be >= 0 after wrap");
        assert!(
            beat < 0.01,
            "Beat should be near start after wrap: {}",
            beat
        );
    }

    #[test]
    fn test_transport_clock_dual_channel_precision() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(tempo, paused, 44100.0);

        let mut output = [0.0f32; 2];

        // Advance to beat ~16384 where f32 truncation would lose precision
        clock.seek(16384.5);
        clock.tick(&[], &mut output);

        // Channel 0 should be the floor (16384.0)
        assert_eq!(output[0], 16384.0);
        // Channel 1 should be the fractional part (~0.5) with full f32 precision
        assert!(
            (output[1] - 0.5).abs() < 0.001,
            "Fractional part should be ~0.5, got {}",
            output[1]
        );
    }
}
