//! The transport: a motion state machine plus the values it runs against.

use std::sync::Arc;

use super::motion::MotionFsm;
use super::settings::TransportSettings;
use super::state::ClockLinks;
use super::state::LoopRange;
use crate::params::{Beat, Bpm, SampleRate};

/// The two halves of a transport, held together.
///
/// There is no facade here — the fields are public and carry their own APIs:
///
/// ```ignore
/// transport.motion.send(MotionEvent::Play);      // a request; may be refused
/// transport.settings.set_tempo(140.0);           // a value; cannot fail
/// transport.settings.loop_span.set_range(0.0, 4.0);
/// ```
///
/// The split is by *who decides*. A motion change goes through a state
/// machine on the audio thread, which may reject or defer it. A setting is
/// just a store. Fusing them is what made the old `TransportManager` carry 19
/// fields and 11 atomic getters.
///
/// This type exists because [`Timeline`](super::Timeline)
/// spans both halves — a reader wants the beat (settings) *and* whether we are
/// rolling (motion) — and needs one `Clone + Send + Sync + 'static` type to be
/// erased behind `Arc<dyn …>`.
#[derive(Clone, Debug)]
pub struct Transport {
    pub motion: MotionFsm,
    pub settings: TransportSettings,
    sample_rate: SampleRate,
}

impl Transport {
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        let settings = TransportSettings::new();
        Self {
            motion: MotionFsm::new(settings.clone()),
            settings,
            sample_rate: sample_rate.into(),
        }
    }

    /// Everything a `TransportClock` shares with this transport — the inputs it
    /// reads *and* the playhead it writes.
    ///
    /// Supplying both halves here is the point: the writeback used to be a
    /// separate builder call, so every construction site had to remember to
    /// chain it. `settings.beat` is documented as "written by `TransportClock`
    /// via its position writeback"; this is what closes that loop.
    pub fn clock_links(&self) -> ClockLinks {
        ClockLinks {
            tempo: Arc::clone(&self.settings.tempo),
            paused: Arc::clone(&self.settings.paused),
            seek: self.motion.seek.clone(),
            loop_span: Some(self.settings.loop_span.clone()),
            position_writeback: Some(Arc::clone(&self.settings.beat)),
            steady_time: Some(Arc::clone(&self.settings.steady_time)),
        }
    }

    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    pub fn beats_per_second(&self) -> f64 {
        self.settings.tempo().get() / 60.0
    }

    pub fn samples_per_beat(&self) -> f64 {
        self.sample_rate.get() / self.beats_per_second()
    }
}

impl super::Timeline for Transport {
    fn beat(&self) -> Beat {
        self.settings.beat()
    }

    fn tempo(&self) -> Bpm {
        self.settings.tempo()
    }

    fn is_rolling(&self) -> bool {
        self.motion.is_playing()
    }
}

impl super::TransportState for Transport {
    fn is_recording(&self) -> bool {
        self.settings.is_recording()
    }

    fn loop_range(&self) -> Option<LoopRange> {
        self.settings.loop_span.range()
    }

    fn steady_time(&self) -> i64 {
        self.settings.steady_time()
    }
}

#[cfg(test)]
mod tests {
    use super::super::motion::MotionEvent;
    use super::super::Timeline;
    use super::*;

    #[test]
    fn clone_shares_both_halves() {
        let a = Transport::new(48000.0);
        let b = a.clone();

        a.settings.set_tempo(140.0);
        assert_eq!(b.settings.tempo().get(), 140.0);

        let _ = a.motion.try_send(MotionEvent::Play);
        b.motion.drain();
        assert!(a.motion.is_playing(), "the FSM is shared, not copied");
    }

    #[test]
    fn read_view_spans_both_halves() {
        let t = Transport::new(48000.0);
        t.settings.set_beat(8.0);
        t.settings.set_tempo(90.0);
        let _ = t.motion.try_send(MotionEvent::Play);
        t.motion.drain();

        // beat/tempo come from settings, is_playing from the FSM — the reason
        // this type exists.
        assert_eq!(t.beat(), Beat(8.0));
        assert_eq!(t.tempo().get(), 90.0);
        assert!(t.is_rolling());
    }

    #[test]
    fn clock_inputs_track_live_edits() {
        let t = Transport::new(48000.0);
        let inputs = t.clock_links();

        t.settings.set_tempo(160.0);
        assert_eq!(
            inputs.tempo.load(std::sync::atomic::Ordering::Acquire),
            160.0,
            "the clock must see later tempo changes"
        );

        let _ = t.motion.try_send(MotionEvent::locate(4.0));
        t.motion.drain();
        assert_eq!(inputs.seek.take(), Some(Beat(4.0)));
    }

    #[test]
    fn samples_per_beat_follows_tempo() {
        let t = Transport::new(44100.0);
        assert!((t.samples_per_beat() - 22050.0).abs() < 1e-6, "120 BPM");

        t.settings.set_tempo(60.0);
        assert!((t.samples_per_beat() - 44100.0).abs() < 1e-6, "60 BPM");
    }
}
