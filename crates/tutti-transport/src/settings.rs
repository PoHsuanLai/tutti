//! Plain shared transport state — the values, not the decisions.
//!
//! Nothing here goes through the command queue and nothing can reject a
//! write: setting the tempo is a store, not a request. That is the whole
//! distinction from [`MotionFsm`](super::MotionFsm), where the state machine
//! decides whether a transition happens at all.
//!
//! Fields are public because there is nothing to encapsulate — each one is a
//! lock-free cell shared between the UI thread and the audio thread, and a
//! forwarding method would only rename it.

use super::TempoReader;
use super::state::AtomicBpm;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tutti_rt::ArcLend;
use tutti_tempo::TempoMap;

use atomic_float::AtomicF64;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
use tutti_types::{Beat, Bpm};

/// Transport values shared between threads.
///
/// Clone shares every field — this is a handle, not a snapshot.
#[derive(Clone, Debug)]
pub struct TransportSettings {
    /// The project's tempo map, published by the control side
    /// ([`set_tempo_map`](Self::set_tempo_map), [`set_tempo`](Self::set_tempo))
    /// and lent to the engine's clock once per block through the one `Reader`
    /// its [`TransportAudio`](super::TransportAudio) registers. The session's
    /// `Reclaim` phase sweeps it ([`reclaim_tempo`](Self::reclaim_tempo)).
    pub(crate) tempo_map: Arc<ArcLend<TempoMap>>,
    /// Moves on after every publish to [`tempo_map`](Self::tempo_map), which is
    /// stored first: a clock that sees a new count lends the new map or a later
    /// one, never an older one.
    pub(crate) map_generation: Arc<AtomicU64>,
    /// The playhead, in beats. Written by `TransportClock` via its position
    /// writeback (and by a locate, which moves the readout at once); read by
    /// the UI and by pull-based sources through [`beat`](Self::beat).
    ///
    /// Crate-private, as is [`segment_generation`](Self::segment_generation):
    /// only the transport's one audio side
    /// ([`Transport::audio`](super::Transport::audio)) stores them. A public
    /// cell was a second writer anyone could be:
    ///
    /// ```compile_fail,E0616
    /// let t = tutti_transport::Transport::new(48_000.0);
    /// t.settings().beat.store(8.0, std::sync::atomic::Ordering::Release);
    /// ```
    ///
    /// A host that drives the playhead by hand takes the audio side and
    /// stores through it ([`TransportAudio::set_playhead`](super::TransportAudio::set_playhead)).
    pub(crate) beat: Arc<AtomicF64>,
    /// The segment the playhead is on
    /// ([`Timeline::segment_generation`](super::Timeline::segment_generation)):
    /// moves on at every seek, tempo or rate change, loop wrap and play
    /// start. Written by `TransportClock` beside [`beat`](Self::beat), and
    /// before it. Crate-private, like [`beat`](Self::beat):
    ///
    /// ```compile_fail,E0616
    /// let t = tutti_transport::Transport::new(48_000.0);
    /// t.settings().segment_generation.store(0, std::sync::atomic::Ordering::Release);
    /// ```
    pub(crate) segment_generation: Arc<AtomicU64>,
    /// Whether the transport is armed and capturing. Read by the metronome's
    /// `RecordingOnly` mode and by hosted plugins' transport snapshot.
    pub(crate) recording: Arc<AtomicBool>,
    /// Whether a count-in is running. Read independently of `recording` — the
    /// metronome's `RecordingOnly` mode plays only when recording *and not* in
    /// preroll, while `PrerollOnly` plays on exactly the opposite condition.
    ///
    /// No engine-side writer sets it; a host drives it around its own
    /// count-in.
    pub in_preroll: Arc<AtomicBool>,
    /// Derived from motion by the engine's motion machine — the clock
    /// reads it to decide whether to advance. Crate-private: nothing outside
    /// can set it.
    ///
    /// ```compile_fail,E0616
    /// let t = tutti_transport::Transport::new(48_000.0);
    /// t.settings().paused.store(false, std::sync::atomic::Ordering::Release);
    /// ```
    pub(crate) paused: Arc<AtomicBool>,
    /// Free-running sample count since the stream started, written by
    /// `TransportClock` every buffer.
    ///
    /// Unlike [`beat`](Self::beat) this never jumps: it ignores loops, seeks and
    /// stops. Hosted plugins receive it as VST3's `continousTimeSamples` /
    /// CLAP's `steady_time`, which free-running effects key off precisely
    /// because the playhead is discontinuous.
    pub(crate) steady_time: Arc<AtomicI64>,
    /// The tempo the clock is actually running at: the map's, as the clock
    /// last took it, which for a constant map ignores wiggles smaller than its
    /// hysteresis. Written by the clock; what a beat must be resolved with
    /// to agree with it.
    pub(crate) tempo_in_force: Arc<AtomicBpm>,
}

impl TransportSettings {
    /// Fresh settings: 120 BPM, beat 0, paused, not recording.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tempo_map: Arc::new(ArcLend::new(Self::default_map())),
            map_generation: Arc::new(AtomicU64::new(0)),
            beat: Arc::new(AtomicF64::new(0.0)),
            segment_generation: Arc::new(AtomicU64::new(0)),
            recording: Arc::new(AtomicBool::new(false)),
            in_preroll: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(true)),
            steady_time: Arc::new(AtomicI64::new(0)),
            tempo_in_force: Arc::new(AtomicBpm::new(Bpm::default())),
        }
    }

    /// The tempo the clock is actually running at (see the field).
    #[must_use]
    pub fn tempo_in_force(&self) -> Bpm {
        self.tempo_in_force.load(Ordering::Acquire)
    }

    /// Samples elapsed since the stream started. Free-running: never reset by a
    /// loop, seek, or stop.
    pub fn steady_time(&self) -> i64 {
        self.steady_time.load(Ordering::Relaxed)
    }

    /// Sets a constant tempo from the next block: sugar for publishing
    /// `TempoMap::constant(bpm)` ([`set_tempo_map`](Self::set_tempo_map)), so it
    /// **replaces** any map. A [`Bpm`] is finite and above zero, so a caller
    /// holding a raw number checks it with [`Bpm::new`] first.
    ///
    /// A tempo so small that a beat's rate underflows (a denormal) is no map
    /// ([`TempoMap::constant`] refuses it): nothing is published and the
    /// project keeps the map it had.
    pub fn set_tempo(&self, bpm: Bpm) {
        if let Ok(map) = TempoMap::constant(bpm) {
            self.tempo_map.publish(map);
        }
        self.map_generation.fetch_add(1, Ordering::Release);
    }

    /// Publishes `map` as the project's tempo: the engine takes it at the next
    /// block start.
    ///
    /// Any map is followed: the clock takes each step on the frame it falls
    /// on (doc 020, stage 3) and each ramp cell by cell, a constant tempo of
    /// 128 frames at the chord of the curve (stage 4).
    pub fn set_tempo_map(&self, map: impl Into<Arc<TempoMap>>) {
        self.tempo_map.publish(map.into());
        self.map_generation.fetch_add(1, Ordering::Release);
    }

    /// The map as published now. Control thread: it takes the cell's lock and
    /// an owning handle, as [`ArcLend::current`] does.
    #[must_use]
    pub fn tempo_map(&self) -> Arc<TempoMap> {
        self.tempo_map.current()
    }

    /// Frees the maps no block holds any more, and returns how many are still
    /// held. Control thread, on a tick it already has: the session's `Reclaim`
    /// phase.
    pub fn reclaim_tempo(&self) -> usize {
        self.tempo_map.reclaim()
    }

    fn default_map() -> TempoMap {
        TempoMap::constant(Bpm::default()).expect("120 BPM is a map")
    }

    /// A reader of the map, registered here on the control side: for the one
    /// engine that lends it once per block, or a hosted node that reports the
    /// project's tempo ([`TempoReader`]). Each call registers a reader of its
    /// own.
    #[must_use]
    pub fn tempo_reader(&self) -> TempoReader {
        TempoReader::new(&self.tempo_map)
    }

    /// The playhead as last published by the clock.
    pub fn beat(&self) -> Beat {
        Beat(self.beat.load(Ordering::Acquire))
    }

    /// The playhead's segment generation as last published by the clock.
    pub fn segment_generation(&self) -> u64 {
        self.segment_generation.load(Ordering::Acquire)
    }

    /// Overwrite the published playhead: this crate's tests, which stand in
    /// for the clock. Outside it, the writer is
    /// [`TransportAudio::set_playhead`](super::TransportAudio::set_playhead).
    #[cfg(test)]
    pub(crate) fn set_beat(&self, beat: impl Into<Beat>) {
        self.beat.store(beat.into().get(), Ordering::Release);
    }

    /// Whether the transport is armed and capturing.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }

    /// Arm or disarm recording.
    pub fn set_recording(&self, recording: bool) {
        self.recording.store(recording, Ordering::Release);
    }

    /// Whether a count-in is running.
    #[must_use]
    pub fn is_in_preroll(&self) -> bool {
        self.in_preroll.load(Ordering::Acquire)
    }

    /// Mark the count-in as running or finished.
    pub fn set_in_preroll(&self, in_preroll: bool) {
        self.in_preroll.store(in_preroll, Ordering::Release);
    }

    /// Whether the clock is holding position.
    ///
    /// Read-only here: the flag is derived from motion by
    /// the engine's motion machine, so there is no setter to race it with.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
}

impl Default for TransportSettings {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_shares_state() {
        let a = TransportSettings::new();
        let b = a.clone();

        a.set_tempo(Bpm::of(140.0));
        assert_eq!(
            b.tempo_map().constant_tempo(),
            Some(Bpm::of(140.0)),
            "clones must share, not copy"
        );
    }

    /// `set_tempo` is the constant map of that tempo, to the bit, and the
    /// beats fall where the definition puts them: a beat is `60 / bpm`
    /// seconds, so beat 4 at 48 kHz is `4 x 60 x 48 000 / bpm` frames.
    #[test]
    fn set_tempo_publishes_the_constant_map_of_that_tempo() {
        let rate = tutti_types::SampleRate(48_000.0);
        let s = TransportSettings::new();
        for bpm in [120.0, 90.0, 97.123_456_789_012_35, 120.587_389_506_707_47] {
            s.set_tempo(Bpm::of(bpm));
            let map = s.tempo_map();
            assert_eq!(map.constant_tempo(), Some(Bpm::of(bpm)), "{bpm} to the bit");
            let frames = 4.0 * 60.0 * 48_000.0 / bpm;
            assert_eq!(
                map.frame_at_or_after(Beat(4.0), rate).get(),
                tutti_types::first_frame_at_or_after(frames) as u64,
                "beat 4 at {bpm}"
            );
        }
    }

    /// A constant map, a map of steps and a map with a ramp are each the
    /// taken whole.
    #[test]
    fn a_constant_a_step_and_a_ramp_map_are_each_taken() {
        use tutti_tempo::{Shape, TempoPoint};
        let s = TransportSettings::new();
        let constant = TempoMap::constant(Bpm::of(75.5)).unwrap();
        s.set_tempo_map(Arc::new(constant));
        assert_eq!(s.tempo_map().constant_tempo(), Some(Bpm::of(75.5)));

        let two = |shape| {
            TempoMap::new([
                TempoPoint {
                    beat: Beat(0.0),
                    tempo: Bpm::of(100.0),
                    shape,
                },
                TempoPoint {
                    beat: Beat(4.0),
                    tempo: Bpm::of(200.0),
                    shape: Shape::Step,
                },
            ])
            .unwrap()
        };
        for shape in [Shape::RampTime, Shape::RampBeats] {
            s.set_tempo_map(Arc::new(constant_of(75.5)));
            s.set_tempo_map(Arc::new(two(shape)));
            assert_eq!(s.tempo_map().constant_tempo(), None);
            assert_eq!(s.tempo_map().points()[0].shape, shape);
        }

        s.set_tempo_map(Arc::new(two(Shape::Step)));
        assert_eq!(s.tempo_map().constant_tempo(), None);
    }

    fn constant_of(bpm: f64) -> TempoMap {
        TempoMap::constant(Bpm::of(bpm)).unwrap()
    }

    /// A map a block holds when it is replaced stays retired until the sweep
    /// after the block, then is freed: `reclaim_tempo` is that sweep.
    #[test]
    fn the_tempo_map_is_swept_by_reclaim_tempo() {
        let s = TransportSettings::new();
        let mut reader = s.tempo_reader();
        let held = reader.lend();
        let old = Arc::downgrade(&s.tempo_map());
        s.set_tempo(Bpm::of(90.0));
        assert!(old.upgrade().is_some(), "a block still reads it");
        assert_eq!(s.reclaim_tempo(), 1, "pinned by the loan");
        drop(held);
        assert_eq!(s.reclaim_tempo(), 0);
        assert!(old.upgrade().is_none(), "freed on the control side");
    }
}
