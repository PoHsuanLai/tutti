//! The transport: a motion state machine plus the values it runs against.

use std::sync::Arc;

use super::motion::MotionFsm;
use super::settings::TransportSettings;
use super::state::ClockLinks;
use super::state::LoopRange;
use crate::{Beat, BeatDuration, Bpm, SampleRate, Samples};

/// The two halves of a transport, held together.
///
/// There is no facade here — the fields are public and carry their own APIs:
///
/// ```
/// # use tutti_core::{Bpm, MotionEvent, Transport};
/// # let transport = Transport::new(48_000.0);
/// transport.motion.try_send(MotionEvent::Play)?; // a request; may be refused
/// transport.settings.set_tempo(Bpm(140.0));      // a value; cannot fail
/// transport.settings.loop_span.set_range(0.0, 4.0);
/// # Ok::<_, tutti_core::QueueFull>(())
/// ```
///
/// The split is by *who decides*. A motion change goes through a state
/// machine on the audio thread, which may reject or defer it. A setting is
/// just a store. Fusing the two is what grows a transport into a wall of
/// atomic getters with no rule for which of them can fail.
///
/// This type exists because [`Timeline`](super::Timeline) spans both halves — a
/// reader needs the beat (settings) *and* whether it is rolling (motion) — and
/// needs one `Clone + Send + Sync + 'static` type to be erased behind
/// `Arc<dyn …>`.
#[derive(Clone, Debug)]
pub struct Transport {
    /// The state machine: play, stop, locate, scrub. Requests may be refused.
    pub motion: MotionFsm,
    /// The plain shared values: tempo, playhead, loop region, record arm.
    pub settings: TransportSettings,
    sample_rate: SampleRate,
}

impl Transport {
    /// Build a stopped transport at 120 BPM, running at `sample_rate`.
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        let settings = TransportSettings::new();
        Self {
            motion: MotionFsm::new(settings.clone()),
            settings,
            sample_rate: sample_rate.into(),
        }
    }

    /// Everything a [`TransportClock`](super::TransportClock) shares with this
    /// transport — the inputs it reads *and* the playhead it writes.
    ///
    /// Both halves come from this one call on purpose. `settings.beat` is only
    /// ever filled by the clock's position writeback, so a construction path
    /// that supplies the inputs without the writeback yields a transport whose
    /// playhead never moves.
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

    /// The device rate this transport converts musical time against. Fixed at
    /// construction.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Frames one beat spans at the current tempo, rounded to the nearest
    /// frame.
    ///
    /// Derived from [`beats_per_sample`](super::beats_per_sample) rather than
    /// from a second `/ 60.0`: the grouping of that division is load-bearing,
    /// and a second spelling of it would round differently.
    ///
    /// A non-positive tempo gives [`Samples::ZERO`] rather than an infinity.
    /// `set_tempo` does not clamp, so `Bpm(0.0)` is reachable from the control
    /// thread, and an infinite frame count is an unbounded allocation waiting
    /// for its first caller.
    pub fn samples_per_beat(&self) -> Samples {
        let bps = super::beats_per_sample(self.settings.tempo(), self.sample_rate);
        if bps <= BeatDuration(0.0) {
            return Samples::ZERO;
        }
        Samples((1.0 / bps.get()).round() as usize)
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

        let _ = t.motion.try_send(MotionEvent::locate(Beat(4.0)));
        t.motion.drain();
        assert_eq!(inputs.seek.take(), Some(Beat(4.0)));
    }

    #[test]
    fn samples_per_beat_follows_tempo() {
        let t = Transport::new(44100.0);
        assert_eq!(t.samples_per_beat(), Samples(22050), "120 BPM");

        t.settings.set_tempo(60.0);
        assert_eq!(t.samples_per_beat(), Samples(44100), "60 BPM");

        // `set_tempo` does not clamp, so this is reachable from the control
        // thread. An `inf` frame count here is an unbounded allocation.
        t.settings.set_tempo(0.0);
        assert_eq!(t.samples_per_beat(), Samples::ZERO, "zero tempo");
        t.settings.set_tempo(-120.0);
        assert_eq!(t.samples_per_beat(), Samples::ZERO, "negative tempo");
    }
}
