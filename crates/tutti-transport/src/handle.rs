//! The transport: a motion state machine plus the values it runs against.

use std::sync::Arc;

use super::audio::{AudioTaken, TransportAudio};
use super::motion::MotionFsm;
use super::settings::TransportSettings;
use atomic_float::AtomicF64;
use std::sync::atomic::Ordering;
use tutti_tempo::TempoMap;
use tutti_types::{Beat, SampleRate};

/// The two halves of a transport, held together.
///
/// There is no facade here — the fields are public and carry their own APIs:
///
/// ```
/// # use tutti_types::{At, Bpm};
/// # use tutti_transport::{LoopEdit, MotionEvent, Transport};
/// # let transport = Transport::new(tutti_types::SampleRate(48_000.0));
/// transport.motion.try_send(MotionEvent::Play)?; // a request; may be refused
/// transport.settings.set_tempo(Bpm::of(140.0));      // a value; cannot fail
/// // Loop beats 0 to 4 from the next block; the engine owns the region.
/// let range = tutti_types::LoopRange::new(tutti_types::Beat(0.0), tutti_types::Beat(4.0)).expect("ordered");
/// transport.motion.schedule(At::NextBlock, LoopEdit::over(range))?;
/// # Ok::<_, Box<dyn std::error::Error>>(())
/// ```
///
/// The split is by *who decides*. A motion change goes through a state
/// machine on the audio thread, which may reject or defer it. A setting is
/// just a store. The loop region is neither: the engine owns it, an edit is a
/// scheduled command, and [`MotionFsm::loop_region`] reads what it holds.
/// Fusing the two is what grows a transport into a wall of atomic getters
/// with no rule for which of them can fail.
///
/// This type exists because [`Timeline`](super::Timeline) spans both halves — a
/// reader needs the beat (settings) *and* whether it is rolling (motion) — and
/// needs one `Clone + Send + Sync + 'static` type to be erased behind
/// `Arc<dyn …>`.
#[derive(Clone, Debug)]
pub struct Transport {
    /// The state machine: play, stop, locate, scrub. Requests may be refused.
    pub motion: MotionFsm,
    /// The plain shared values: tempo, playhead, record arm.
    pub settings: TransportSettings,
    /// Shared by every clone, so a device restart at a new rate
    /// ([`set_sample_rate`](Self::set_sample_rate)) reaches each holder —
    /// a clip reader's `Arc<dyn Timeline>`, the MIDI clock master — rather
    /// than only the handle it was called on.
    sample_rate: Arc<AtomicF64>,
}

impl Transport {
    /// Creates a stopped transport at 120 BPM, running at `sample_rate`.
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        let settings = TransportSettings::new();
        Self {
            motion: MotionFsm::new(settings.clone()),
            settings,
            sample_rate: Arc::new(AtomicF64::new(sample_rate.into().get())),
        }
    }

    /// The audio-thread side of this transport, for the **one** holder that
    /// renders under it: the motion machine, the playhead clock and the
    /// timed commands, taken together and handed back together when the
    /// returned value drops.
    ///
    /// # Errors
    ///
    /// [`AudioTaken`] while an earlier value from this transport, or from a
    /// clone of it, is alive: two would both consume every seek and both
    /// write the playhead. There is one claim and one place it is held (the
    /// motion machine's home), so no other half can be taken around it.
    pub fn audio(&self, sample_rate: impl Into<SampleRate>) -> Result<TransportAudio, AudioTaken> {
        TransportAudio::take(self, sample_rate.into())
    }

    /// The links a clock over this transport shares, writer half included.
    /// Crate-private: [`audio`](Self::audio) is the only caller, and it only
    /// calls with the motion machine in hand.
    pub(crate) fn links(&self) -> super::state::ClockLinks {
        super::state::ClockLinks {
            paused: Arc::clone(&self.settings.paused),
            seek: self.motion.seek.clone(),
            position_writeback: Some(Arc::clone(&self.settings.beat)),
            segment_generation: Some(Arc::clone(&self.settings.segment_generation)),
            steady_time: Some(Arc::clone(&self.settings.steady_time)),
            tempo_in_force: Some(Arc::clone(&self.settings.tempo_in_force)),
        }
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
}

impl super::Timeline for Transport {
    fn beat(&self) -> Beat {
        self.settings.beat()
    }

    fn tempo_map(&self) -> Arc<TempoMap> {
        self.settings.tempo_map()
    }

    fn is_rolling(&self) -> bool {
        self.motion.is_playing()
    }

    fn segment_generation(&self) -> u64 {
        self.settings.segment_generation()
    }
}

#[cfg(test)]
mod tests {
    use super::super::Timeline;
    use super::super::motion::MotionEvent;
    use super::super::motion::harness::Audio;
    use super::*;
    use tutti_types::Bpm;

    #[test]
    fn clone_shares_both_halves() {
        let a = Transport::new(SampleRate::new(48000.0));
        let b = a.clone();

        a.settings.set_tempo(Bpm::of(140.0));
        assert_eq!(
            b.settings.tempo_map().constant_tempo(),
            Some(Bpm::of(140.0))
        );

        let _ = a.motion.try_send(MotionEvent::Play);
        Audio::of(&b.motion).drain();
        assert!(a.motion.is_playing(), "the FSM is shared, not copied");
    }

    /// A rate change reaches every clone: a clip reader holds the transport
    /// as its own `Arc<dyn Timeline>`, and a device restart sets the rate on
    /// the host's copy.
    ///
    /// Mutation (run): `sample_rate` a plain `SampleRate` field again (and
    /// `set_sample_rate` taking `&mut self`, storing into it) → the clone
    /// keeps 44.1 kHz and its beat is 22 050 frames → fails.
    #[test]
    fn a_rate_change_reaches_every_clone() {
        let a = Transport::new(SampleRate::new(44_100.0));
        let b = a.clone();
        a.set_sample_rate(SampleRate::new(48_000.0));
        assert_eq!(b.sample_rate(), SampleRate(48_000.0));
        let beat = b
            .settings
            .tempo_map()
            .frame_at_or_after(Beat(1.0), b.sample_rate());
        assert_eq!(beat.get(), 24_000, "one beat at 120 BPM and 48 kHz");
    }

    #[test]
    fn read_view_spans_both_halves() {
        let t = Transport::new(SampleRate::new(48000.0));
        t.settings.set_beat(Beat::new(8.0));
        t.settings.set_tempo(Bpm::of(90.0));
        let _ = t.motion.try_send(MotionEvent::Play);
        Audio::of(&t.motion).drain();

        // beat/tempo come from settings, is_playing from the FSM — the reason
        // this type exists.
        assert_eq!(t.beat(), Beat(8.0));
        assert_eq!(t.tempo_map().constant_tempo(), Some(Bpm::of(90.0)));
        assert!(t.is_rolling());
    }

    #[test]
    fn clock_inputs_track_live_edits() {
        let t = Transport::new(SampleRate::new(48000.0));
        let inputs = t.links();

        let _ = t.motion.try_send(MotionEvent::locate(Beat(4.0)));
        Audio::of(&t.motion).drain();
        assert_eq!(inputs.seek.take(), Some(Beat(4.0)));
    }

    /// **A tempo a caller got wrong stops at the type**: a zero, negative or
    /// non-finite number is refused before `set_tempo`, so the map the clock
    /// follows stays where it was.
    ///
    /// Mutation (run): `Bpm::new` tests `raw >= 0.0` instead of `raw > 0.0`;
    /// the zero is let through to `set_tempo` and the tempo assertion fails.
    #[test]
    fn a_refused_tempo_never_reaches_the_clock() {
        let t = Transport::new(SampleRate::new(44100.0));
        t.settings.set_tempo(Bpm::of(90.0));
        for raw in [0.0, -120.0, f64::NAN, f64::INFINITY] {
            let asked = Bpm::try_from(raw);
            assert!(asked.is_err(), "{raw} is not a tempo");
            if let Ok(bpm) = asked {
                t.settings.set_tempo(bpm);
            }
            assert_eq!(
                t.settings.tempo_map().constant_tempo(),
                Some(Bpm::of(90.0)),
                "after {raw}"
            );
        }
    }
}
