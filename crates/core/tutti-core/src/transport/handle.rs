//! The transport: a motion state machine plus the values it runs against.

use std::sync::Arc;

use super::motion::MotionFsm;
use super::settings::TransportSettings;
use super::state::LoopRange;
use super::state::{ClockLinks, PlayheadClaim, PlayheadClaimed};
use crate::{AtomicBool, AtomicF64, Ordering};
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
    /// Shared by every clone, so a device restart at a new rate
    /// ([`set_sample_rate`](Self::set_sample_rate)) reaches each holder —
    /// a clip reader's `Arc<dyn Timeline>`, the MIDI clock master — rather
    /// than only the handle it was called on.
    sample_rate: Arc<AtomicF64>,
    /// Whether a clock holds this transport's playhead-writer claim
    /// ([`clock_links`](Self::clock_links)). Shared by every clone, so the
    /// one writer is one per transport, not per handle.
    playhead_claimed: Arc<AtomicBool>,
}

impl Transport {
    /// Build a stopped transport at 120 BPM, running at `sample_rate`.
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        let settings = TransportSettings::new();
        Self {
            motion: MotionFsm::new(settings.clone()),
            settings,
            sample_rate: Arc::new(AtomicF64::new(sample_rate.into().get())),
            playhead_claimed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Everything a [`TransportClock`](super::TransportClock) shares with this
    /// transport — the inputs it reads *and* the playhead it writes — for
    /// the **one** clock that writes it.
    ///
    /// Both halves come from this one call on purpose. `settings.beat` is only
    /// ever filled by the clock's position writeback, so a construction path
    /// that supplies the inputs without the writeback yields a transport whose
    /// playhead never moves.
    ///
    /// # One writer
    ///
    /// Refused with [`PlayheadClaimed`] while links from an earlier call are
    /// alive (in a clock, or a clone of one), on this handle or any clone of
    /// it: two clocks would both consume every seek and both write the
    /// playhead. The claim is given back when the last holder of those links
    /// is dropped (the engine that drove them), or never taken by links
    /// [`severed`](ClockLinks::severed) from them.
    pub fn clock_links(&self) -> Result<ClockLinks, PlayheadClaimed> {
        let claim = PlayheadClaim::take(&self.playhead_claimed)?;
        Ok(ClockLinks {
            tempo: Arc::clone(&self.settings.tempo),
            paused: Arc::clone(&self.settings.paused),
            seek: self.motion.seek.clone(),
            loop_span: Some(self.settings.loop_span.clone()),
            position_writeback: Some(Arc::clone(&self.settings.beat)),
            segment_generation: Some(Arc::clone(&self.settings.segment_generation)),
            steady_time: Some(Arc::clone(&self.settings.steady_time)),
            tempo_in_force: Some(Arc::clone(&self.settings.tempo_in_force)),
            claim: Some(Arc::new(claim)),
        })
    }

    /// The device rate this transport converts musical time against: the
    /// one it was built at, until [`set_sample_rate`](Self::set_sample_rate).
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(self.sample_rate.load(Ordering::Acquire))
    }

    /// The device now runs at `sample_rate`: every clone of this transport
    /// converts against it from here on.
    ///
    /// Only the *conversion* rate. The playhead is in beats and does not
    /// move; the engine's clock follows the graph's own rate when the engine
    /// adopts it (`Engine::process`), which is where a frame count turns into
    /// beats. A host changing device rate calls this beside re-rating the
    /// graph — `bevy_tutti`'s device restart does both.
    ///
    /// A change also marks the boundary for the transport's frame-timed
    /// commands: one scheduled (`MotionFsm::schedule`) after this call is
    /// taken to be in the new rate's frames, and the engine leaves it alone
    /// when it adopts the rate; one scheduled before is rescaled to its
    /// wall-clock time. Call it before scheduling against the new rate.
    pub fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        let rate = sample_rate.into().get();
        if self.sample_rate.swap(rate, Ordering::AcqRel) != rate {
            self.motion.timed().mark_rate_change();
        }
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
        let bps = super::beats_per_sample(self.settings.tempo(), self.sample_rate());
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

    fn segment_generation(&self) -> u64 {
        self.settings.segment_generation()
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

    /// A rate change reaches every clone: a clip reader holds the transport
    /// as its own `Arc<dyn Timeline>`, and a device restart sets the rate on
    /// the host's copy.
    ///
    /// Mutation (run): `sample_rate` a plain `SampleRate` field again (and
    /// `set_sample_rate` taking `&mut self`, storing into it) → the clone
    /// keeps 44.1 kHz and its `samples_per_beat` stays 22 050 → fails.
    #[test]
    fn a_rate_change_reaches_every_clone() {
        let a = Transport::new(44_100.0);
        let b = a.clone();
        a.set_sample_rate(48_000.0);
        assert_eq!(b.sample_rate(), SampleRate(48_000.0));
        assert_eq!(b.samples_per_beat(), Samples(24_000), "120 BPM at 48 kHz");
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
        let inputs = t.clock_links().expect("the first clock");

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
