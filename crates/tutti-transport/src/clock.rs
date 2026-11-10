//! Sample-accurate transport clock — the engine's playhead: it turns the
//! transport's atomics into the transport each graph block is rendered under,
//! and writes the playhead back to the transport once per block.
//!
//! It is not a graph node. The engine holds one and drives it (its
//! crate-private `begin`, `advance` and `publish_position`); a node reads the
//! transport from its block's `Env`.

use super::loop_region::LoopRegion;
use super::state::ClockLinks;
use super::walk::{TempoAhead, TempoWalk};
use std::sync::atomic::Ordering;
use tutti_tempo::TempoMap;
use tutti_types::FrameClock;
use tutti_types::{Beat, Bpm, Samples};

/// How far the tempo must move before the clock takes it (and starts a new
/// segment). Comparing for equality would restart on every buffer from ULP
/// noise.
const TEMPO_EPSILON: f64 = 0.001;

/// The tempo a clock running at `in_force` takes when asked for `asked`:
/// `asked`, unless it is within the hysteresis of `in_force`. The one
/// spelling of the rule, for the clock and for whoever resolves a beat
/// against it.
#[inline]
pub(crate) fn tempo_in_effect(asked: Bpm, in_force: Bpm) -> Bpm {
    if asked.differs_from(in_force, TEMPO_EPSILON) {
        asked
    } else {
        in_force
    }
}

/// The loop of a block's transport, as the clock takes it.
fn loop_of(from: &tutti_graph::Transport) -> Option<tutti_types::LoopRange> {
    from.looping
        .and_then(|l| tutti_types::LoopRange::new(l.start, l.end))
}

/// The untimed transport inputs a graph block is rendered under,
/// read **once** per block (at the start of the engine's walk): a store from
/// the control thread lands at the next block, never at a cut in this one.
/// Only a command the engine applies changes them mid-block
/// ([`Control::apply`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Control {
    /// The tempo the block renders at.
    pub tempo: Bpm,
    /// Whether the transport is paused.
    pub paused: bool,
    /// The engine's loop region. Not read off the settings: the engine owns
    /// it, and an edit reaches it as a command.
    pub loop_region: LoopRegion,
    /// Whether the session is recording, for the graph's
    /// [`Transport::recording`](tutti_graph::Transport::recording). Read once
    /// per block, like the rest; no command moves it mid-block.
    pub recording: bool,
}

impl Control {
    /// Read the live inputs, and take the loop region from the engine and the
    /// tempo from the caller, which follows the published map
    /// ([`TransportAudio`](super::TransportAudio)).
    pub fn read(settings: &super::TransportSettings, loop_region: LoopRegion, tempo: Bpm) -> Self {
        Self {
            tempo,
            paused: settings.is_paused(),
            loop_region,
            recording: settings.is_recording(),
        }
    }

    /// Carry `command`'s effect, and nothing else. A motion change's effect
    /// is pausedness, which only the motion machine writes, on this thread. A
    /// loop edit is replayed on this copy of the region.
    pub fn apply(
        &mut self,
        command: &super::TransportCommand,
        settings: &super::TransportSettings,
    ) {
        match *command {
            super::TransportCommand::Tempo(bpm) => self.tempo = bpm,
            super::TransportCommand::Loop(edit) => self.loop_region = self.loop_region.with(edit),
            super::TransportCommand::Motion(_) => self.paused = settings.is_paused(),
        }
    }
}

/// The engine's playhead: a frame count on a segment, stepped a block at a
/// time by the engine (its crate-private `begin`, then `advance`).
///
/// Emit-then-advance. A block's first frame carries the block's start beat,
/// and only then does the beat move — anything that advances a second clock
/// alongside this one ([`Env::for_each_beat`](tutti_graph::Env::for_each_beat),
/// [`Env::transport_at`](tutti_graph::Env::transport_at), an
/// `OfflineTimeline`) matches that order, or sits permanently one frame out
/// of step.
///
/// **The frame is the source of truth.** The playhead is a
/// frame count on a segment (`FrameClock`), and the beat is derived from it
/// in closed form, never accumulated: at 90 BPM and 48 kHz, frame 96 000 is
/// beat 3 to the bit, where adding `beats_per_sample` 96 000 times drifts off
/// it. A seek, a tempo or rate change and a loop wrap start a new segment on
/// their frame.
#[derive(Clone, Debug)]
pub(crate) struct TransportClock {
    /// Everything shared with the live transport. A derived stream
    /// (`starting_at`, `at_tempo`)
    /// severs it wholesale; every other field is this clock's own.
    links: ClockLinks,
    /// The playhead, and the tempo in force (its hysteresis applied) and rate
    /// it rolls at. Advanced a block at a time by [`advance`](Self::advance):
    /// the beat of any frame is in closed form from the segment, so the same
    /// beats as a frame-by-frame walk.
    clock: FrameClock,
    /// Whether the last block (or frame) this clock ran was rolling, so a
    /// play start moves the segment generation on
    /// ([`FrameClock::mark_play_start`]).
    rolling: bool,
    /// Where the playhead is on the published map, for a map that changes
    /// tempo (doc 020 stage 3). Used only when a block hands the clock the map
    /// (`Some`): a constant map never does, and the clock then runs its own
    /// closed form, untouched.
    walk: TempoWalk,
}

impl TransportClock {
    /// Build a clock over `links`, starting at beat 0.
    ///
    /// The one constructor: `links` carries both halves of the position
    /// handshake, so a clock cannot be assembled with its inputs wired and its
    /// writeback forgotten.
    pub fn new(links: ClockLinks, sample_rate: impl Into<tutti_types::SampleRate>) -> Self {
        let sample_rate = sample_rate.into();
        let initial_tempo = links.tempo.load(Ordering::Acquire);

        let clock = Self {
            links,
            clock: FrameClock::new(Beat(0.0), initial_tempo, sample_rate),
            rolling: false,
            walk: TempoWalk::default(),
        };
        clock.publish_tempo();
        clock
    }

    /// Publish the tempo in force to the live transport.
    fn publish_tempo(&self) {
        if let Some(ref out) = self.links.tempo_in_force {
            out.store(self.clock.tempo(), Ordering::Release);
        }
    }

    // ---- Derived beat streams -------------------------------------------
    //
    // A clock IS a stream of beats. These combinators derive an independent
    // stream from it, for material that must be timed separately from live
    // playback (offline renders, previews).
    //
    // They are deliberately **static**: a derived stream snapshots its
    // configuration and shares nothing with the live transport, so ticking it
    // on a worker thread cannot disturb playback. The live transport stays
    // authoritative for the timeline, because its loop/tempo atomics are the
    // audio-thread mirror of persisted document state that the host reconciles
    // every frame — not incidental state a stream transform could absorb.

    /// Derive an independent stream starting at `beat`.
    ///
    /// Absolute, not an offset: the returned clock's first emitted beat is
    /// exactly `beat`, matching the one-shot absolute semantics of seek.
    ///
    /// Severs every shared link to the live transport
    /// ([`ClockLinks::severed`](super::ClockLinks::severed)), so this clock
    /// writes to nothing live and reads no live loop or seek state.
    ///
    /// # Why a clone is not already safe
    ///
    /// `Clone` shares `tempo`, `paused`, `seek`, the loop span and
    /// `position_writeback` by `Arc`. A clock stepped on a worker thread for
    /// seconds while the live engine plays **writes** `position_writeback`
    /// and consumes `seek` every block: a shared clone stomps the live
    /// playhead and swallows live seeks.
    #[cfg(test)]
    #[must_use]
    pub fn starting_at(&self, beat: impl Into<Beat>) -> Self {
        let mut derived = self.severed();
        derived.clock.seat(beat.into());
        derived
    }

    /// Derive an independent stream running at a fixed `bpm` (severed, as
    /// [`starting_at`](Self::starting_at) is).
    #[cfg(test)]
    #[must_use]
    pub fn at_tempo(&self, bpm: Bpm) -> Self {
        let mut derived = self.severed();
        derived.set_tempo(bpm);
        derived
    }

    /// A clone sharing nothing with the live transport. The tempo is copied
    /// into a cell of its own; the loop is dropped.
    #[cfg(test)]
    fn severed(&self) -> Self {
        let mut derived = self.clone();
        derived.links = derived.links.severed();
        derived
    }

    /// Set this clock's tempo, from its current frame on.
    #[cfg(test)]
    fn set_tempo(&mut self, bpm: Bpm) {
        self.links.tempo.store(bpm, Ordering::Release);
        self.clock.set_tempo(bpm);
        self.publish_tempo();
    }

    /// Publishes `beat` as the live playhead without moving this clock (see
    /// [`ClockLinks::set_playhead`](super::state::ClockLinks::set_playhead)).
    pub(crate) fn set_playhead(&self, beat: Beat) {
        self.links.set_playhead(beat);
    }

    /// The beat this clock will emit next. Its own position, not the live
    /// transport's — a derived stream reports its private playhead here.
    #[cfg(test)]
    pub fn current_beat(&self) -> Beat {
        self.clock.beat()
    }

    /// Take `asked` as the tempo, if it moved past the hysteresis: a new
    /// segment from this frame.
    #[inline]
    fn take_tempo(&mut self, asked: Bpm) {
        let tempo = tempo_in_effect(asked, self.clock.tempo());
        if tempo != self.clock.tempo() {
            self.clock.set_tempo(tempo);
            self.publish_tempo();
        }
    }

    #[inline]
    fn apply_pending_seek(&mut self, map: Option<&TempoMap>) {
        if let Some(target) = self.links.seek.take() {
            match map {
                // Seats at the exact beat; the map frame is its
                // `frame_at_or_after` (doc 020, section 4).
                Some(map) => {
                    self.walk.seat(map, &mut self.clock, target);
                    self.publish_tempo();
                }
                None => self.clock.seat(target),
            }
        }
    }

    /// Start following a map that changes tempo: a segment at the current
    /// beat (the playhead does not jump; the generation moves on, as for any
    /// segment restart), the map frame found from it, and the tempo in force
    /// there.
    pub fn follow(&mut self, map: &TempoMap) {
        self.clock.seat(self.clock.beat());
        self.walk.place(map, &mut self.clock);
        self.publish_tempo();
    }

    /// The rate this clock counts frames at.
    pub fn sample_rate(&self) -> tutti_types::SampleRate {
        self.clock.sample_rate()
    }

    /// Take a pending seek (when `take_seek`) and `control`'s tempo, and
    /// report the transport from this frame on under `control`, as a
    /// graph block sees it ([`tutti_graph::Transport`]). The seek is taken
    /// only at a block's start and after a motion command, the two points
    /// where the motion machine (this thread) can have requested one.
    ///
    /// The engine has no clock node in its graph: it holds a
    /// `TransportClock` of its own and drives it with this and
    /// [`advance`](Self::advance), so the playhead a graph node reads in its
    /// `Env` is computed by the same code, in the same order, as every other
    /// reader of this clock.
    ///
    /// `map` is the published map when it changes tempo, `None` for a constant
    /// one: with a map the walk owns the tempo (no hysteresis, and
    /// `control.tempo` is not read); without, `control.tempo` is the tempo.
    pub fn begin(
        &mut self,
        control: &Control,
        take_seek: bool,
        map: Option<&TempoMap>,
    ) -> tutti_graph::Transport {
        if take_seek {
            self.apply_pending_seek(map);
        }
        if map.is_none() {
            self.take_tempo(control.tempo);
        }
        self.note_rolling(!control.paused);
        // Counted: the frame count the beat is derived from, so the graph's
        // `Env::for_each_beat` and `Env::transport_at` continue this clock
        // with its own code.
        tutti_graph::Transport::counted(
            !control.paused,
            self.clock.tempo(),
            self.clock.origin(),
            control
                .loop_region
                .active()
                .map(|r| tutti_graph::LoopRange {
                    start: r.start(),
                    end: r.end(),
                }),
        )
        .with_recording(control.recording)
    }

    /// Advance `frames` under `from`, the transport the last
    /// [`begin`](Self::begin) reported, exactly as `process` would over that
    /// many frames: the same frames counted and the same loop wraps (so the
    /// same beats, to the bit), then the steady-time count. **Not** the position writeback: the engine walks
    /// a block before rendering it, and publishes with
    /// [`publish_position`](Self::publish_position) once the block is
    /// rendered, so the live playhead reads the block's first frame while it
    /// renders (as `process` publishes only at the end of its call).
    ///
    /// Takes the play state and loop from `from` rather than re-reading the
    /// atomics, so a store from the control thread between the two calls
    /// cannot make the playhead move differently from what the block was
    /// told.
    ///
    /// With a `map` that changes tempo the clock rolls cell by cell and takes
    /// the tempo at each boundary, on the frame it falls on: the same frames
    /// [`ahead`](Self::ahead) reported.
    pub fn advance(
        &mut self,
        frames: usize,
        from: &tutti_graph::Transport,
        map: Option<&TempoMap>,
    ) {
        if from.playing {
            let region = loop_of(from);
            match map {
                Some(map) => {
                    self.walk.roll(map, &mut self.clock, frames, region, false);
                    self.publish_tempo();
                }
                None => self.clock.advance(Samples(frames), region),
            }
        }
        self.advance_steady_time(frames);
    }

    /// The tempo changes inside the next `frames` of a block that began under
    /// `from`, at most `cap` of them, found without moving the clock
    /// (the dry run of [`advance`](Self::advance)). Nothing for a constant map
    /// and for a transport that is not rolling.
    pub fn ahead(
        &self,
        frames: usize,
        from: &tutti_graph::Transport,
        map: Option<&TempoMap>,
        cap: usize,
    ) -> TempoAhead {
        let (Some(map), true) = (map, from.playing) else {
            return TempoAhead::NONE;
        };
        self.walk
            .ahead(map, &self.clock, frames, loop_of(from), cap, |clock| {
                tutti_graph::Transport::counted(true, clock.tempo(), clock.origin(), from.looping)
                    .with_recording(from.recording())
            })
    }

    /// Publish this clock's position as the live playhead — its segment's
    /// generation, then its beat: the figures a [`Timeline`]
    /// (`Transport::segment_generation`, `Transport::beat`) reads — without
    /// moving it: the graph engine's writeback, after each block it renders
    /// (see [`advance`](Self::advance)), and `process`'s. A no-op for a
    /// clock with no writeback.
    ///
    /// The generation first, so a reader that reads the beat and then the
    /// generation never pairs this beat with an older generation
    /// ([`Timeline::segment_generation`]).
    ///
    /// [`Timeline`]: super::Timeline
    /// [`Timeline::segment_generation`]: super::Timeline::segment_generation
    pub fn publish_position(&self) {
        if let Some(ref generation) = self.links.segment_generation {
            generation.store(self.clock.generation(), Ordering::Release);
        }
        if let Some(ref writeback) = self.links.position_writeback {
            writeback.store(self.clock.beat().get(), Ordering::Release);
        }
    }

    /// Whether this block (or frame) rolls: a play start after a stop moves
    /// the segment generation on, so a reader that cached a position before
    /// the stop re-seats even when the playhead did not move.
    #[inline]
    fn note_rolling(&mut self, rolling: bool) {
        if rolling && !self.rolling {
            self.clock.mark_play_start();
        }
        self.rolling = rolling;
    }

    /// Advance the free-running sample counter.
    ///
    /// Deliberately **not** gated on `paused`: this counts samples the device
    /// has pulled, not musical time. A delay or LFO keyed to it must keep
    /// running while the transport is stopped — that is the entire reason the
    /// plugin ABIs carry it separately from the playhead.
    ///
    /// `Relaxed` because nothing else is published alongside it; a reader wants
    /// the latest value, not ordering against other stores.
    #[inline]
    fn advance_steady_time(&self, samples: usize) {
        if let Some(ref steady) = self.links.steady_time {
            steady.fetch_add(samples as i64, Ordering::Relaxed);
        }
    }
}

impl TransportClock {
    /// A new segment at `sample_rate`, at the tempo in force, not the one
    /// asked: the clock takes a tempo only through `take_tempo`'s
    /// hysteresis, and the tempo in force is the one a graph engine reports
    /// in its `Env` and `Env::for_each_beat` steps by. A request inside the
    /// hysteresis would otherwise run this clock at a tempo it does not
    /// publish.
    ///
    /// Under a `map` that changes tempo the playhead is re-seated at the
    /// current beat on the map's timeline at the new rate (doc 020, section
    /// 10), and the tempo in force there is taken.
    pub fn set_sample_rate(
        &mut self,
        sample_rate: tutti_types::SampleRate,
        map: Option<&TempoMap>,
    ) {
        self.clock.set_sample_rate(sample_rate);
        if let Some(map) = map {
            self.walk.place(map, &mut self.clock);
            self.publish_tempo();
        }
    }

    /// Step `frames` as the engine steps a block, under the transport's live
    /// inputs read off this clock's links (what `Control::read` reads off the
    /// settings) and a disarmed loop: take a pending seek and the tempo,
    /// advance, publish. The beat of the block's first frame. For tests that
    /// drive a bare clock.
    #[cfg(test)]
    pub(crate) fn step(&mut self, frames: usize) -> Beat {
        self.step_in(frames, LoopRegion::default())
    }

    /// [`step`](Self::step) under `loop_region`.
    #[cfg(test)]
    pub(crate) fn step_in(&mut self, frames: usize, loop_region: LoopRegion) -> Beat {
        let control = Control {
            tempo: self.links.tempo.load(Ordering::Acquire),
            paused: self.links.paused.load(Ordering::Acquire),
            loop_region,
            recording: false,
        };
        let t = self.begin(&control, true, None);
        let first = self.clock.beat();
        self.advance(frames, &t, None);
        self.publish_position();
        first
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::{AtomicBpm, SeekSlot};
    use super::*;
    use atomic_float::AtomicF64;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use tutti_types::LoopRange;
    use tutti_types::SampleRate;

    fn create_test_atomics() -> (Arc<AtomicBpm>, Arc<AtomicBool>) {
        (
            Arc::new(AtomicBpm::new(Bpm::default())), // 120 BPM
            Arc::new(AtomicBool::new(false)),         // Not paused
        )
    }

    /// A clock plus the seek slot that drives it.
    fn clock_with_seek(
        tempo: Arc<AtomicBpm>,
        paused: Arc<AtomicBool>,
    ) -> (TransportClock, SeekSlot) {
        let seek = SeekSlot::new();
        let clock = TransportClock::new(
            ClockLinks {
                tempo,
                paused,
                seek: seek.clone(),
                position_writeback: None,
                segment_generation: None,
                steady_time: None,
                tempo_in_force: None,
            },
            SampleRate::new(44100.0),
        );
        (clock, seek)
    }

    /// One frame, as the engine steps a one-frame block: the beat the frame
    /// carries (what `tick` emitted on its ports).
    fn tick(clock: &mut TransportClock) -> f64 {
        clock.step(1).get()
    }

    /// `tick` under an armed loop over `start..end`.
    fn tick_looping(clock: &mut TransportClock, start: f64, end: f64) -> f64 {
        let region = LoopRegion::new(
            LoopRange::new(Beat::new(start), Beat::new(end)).expect("ordered"),
            true,
        );
        clock.step_in(1, region).get()
    }

    #[test]
    fn test_transport_clock_tick() {
        let (tempo, paused) = create_test_atomics();
        let mut clock =
            TransportClock::new(ClockLinks::bare(tempo, paused), SampleRate::new(44100.0));

        assert!((tick(&mut clock) - 0.0).abs() < 0.001);

        let mut beat = 0.0;
        for _ in 0..44100 {
            beat = tick(&mut clock);
        }

        assert!((beat - 2.0).abs() < 0.01);
    }

    /// A derived stream (an offline render's clock) is stepped on a worker
    /// thread, and stepping *writes* `position_writeback` every block. The
    /// derivation must sever it, or the worker stomps the live playhead the
    /// rest of the app reads as "current beat".
    ///
    /// Mutation (run): `severed` keeping the live links (drop its
    /// `links.severed()`) → the live playhead reads the stream's beat → fails.
    #[test]
    fn a_derived_stream_severs_live_position_writeback() {
        let (tempo, paused) = create_test_atomics();
        let live_position = Arc::new(AtomicF64::new(7.5)); // live playhead "now"
        let mut links = ClockLinks::bare(tempo, paused);
        links.position_writeback = Some(Arc::clone(&live_position));
        let clock = TransportClock::new(links, SampleRate::new(44100.0));

        let mut render = clock.starting_at(Beat::new(0.0));

        // Step the stream a full second — if it still shared the writeback,
        // it would overwrite `live_position` with its own advancing beat
        // (starting from 0.0), wrecking the live playhead.
        for _ in 0..44100 {
            tick(&mut render);
        }

        assert_eq!(
            live_position.load(Ordering::Acquire),
            7.5,
            "live playhead must be untouched by the render clock"
        );
        // The stream still advances its own private beat (~2 beats at 120
        // BPM), so the render actually produces audio over its window.
        assert!(
            (render.current_beat().get() - 2.0).abs() < 0.01,
            "the stream must still advance its own beat, got {}",
            render.current_beat().get()
        );
    }

    /// The counter's defining property: it counts device samples, so it keeps
    /// running while the transport is paused and does not jump on a loop wrap.
    /// A free-running delay or LFO keyed to it depends on exactly that.
    #[test]
    fn steady_time_ignores_pause_and_loop() {
        let (tempo, paused) = create_test_atomics();
        let steady = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let mut links = ClockLinks::bare(Arc::clone(&tempo), Arc::clone(&paused));
        links.steady_time = Some(Arc::clone(&steady));
        // A two-beat loop, so the playhead wraps repeatedly over the run.
        let looping = LoopRegion::new(
            LoopRange::new(Beat::new(0.0), Beat::new(2.0)).expect("ordered"),
            true,
        );
        let mut clock = TransportClock::new(links, SampleRate::new(44100.0));

        clock.step_in(64, looping);
        assert_eq!(steady.load(Ordering::Relaxed), 64);

        // Paused: musical time stops, sample time does not.
        paused.store(true, Ordering::Release);
        let beat_while_paused = clock.current_beat();
        clock.step_in(64, looping);
        assert_eq!(
            steady.load(Ordering::Relaxed),
            128,
            "steady time must advance while paused"
        );
        assert_eq!(
            clock.current_beat(),
            beat_while_paused,
            "the playhead must not move while paused"
        );

        // Rolling again across many loop wraps: still strictly monotonic.
        paused.store(false, Ordering::Release);
        for _ in 0..100 {
            clock.step_in(64, looping);
        }
        assert_eq!(
            steady.load(Ordering::Relaxed),
            128 + 100 * 64,
            "loop wraps must not reset the free-running counter"
        );
    }

    /// `steady_time` is `Arc`-shared, so it is subject to the same cut as the
    /// position writeback: an offline render must not advance the live counter.
    ///
    /// Mutation (run): `ClockLinks::severed` keeping `steady_time` → the live
    /// counter reads 10 000 → fails.
    #[test]
    fn a_derived_stream_severs_steady_time() {
        let (tempo, paused) = create_test_atomics();
        let steady = Arc::new(std::sync::atomic::AtomicI64::new(9_000));
        let mut links = ClockLinks::bare(tempo, paused);
        links.steady_time = Some(Arc::clone(&steady));
        let clock = TransportClock::new(links, SampleRate::new(44100.0));

        let mut render = clock.starting_at(Beat::new(0.0));
        for _ in 0..1_000 {
            tick(&mut render);
        }

        assert_eq!(
            steady.load(Ordering::Relaxed),
            9_000,
            "a render clock must not advance the live steady time"
        );
    }

    #[test]
    fn test_transport_clock_pause() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(
            ClockLinks::bare(tempo, paused.clone()),
            SampleRate::new(44100.0),
        );

        let mut beat = 0.0;
        for _ in 0..1000 {
            beat = tick(&mut clock);
        }
        let beat_before_pause = beat;

        paused.store(true, Ordering::Release);

        for _ in 0..1000 {
            beat = tick(&mut clock);
        }

        // The first paused frame carries the beat the last rolling one
        // advanced to.
        assert!((beat - beat_before_pause).abs() < 0.001);
    }

    #[test]
    fn test_transport_clock_seek() {
        let (tempo, paused) = create_test_atomics();
        let (mut clock, seek) = clock_with_seek(tempo, paused);

        seek.request(Beat::new(4.0));

        assert!((tick(&mut clock) - 4.0).abs() < 0.001);
    }

    #[test]
    fn test_transport_clock_tempo_change() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(
            ClockLinks::bare(tempo.clone(), paused),
            SampleRate::new(44100.0),
        );

        for _ in 0..44100 {
            tick(&mut clock);
        }

        tempo.store(Bpm::of(240.0), Ordering::Release);

        let mut beat = 0.0;
        for _ in 0..44100 {
            beat = tick(&mut clock);
        }

        assert!((beat - 6.0).abs() < 0.1);
    }

    #[test]
    fn test_transport_clock_loop_disabled() {
        let (tempo, paused) = create_test_atomics();
        let mut clock =
            TransportClock::new(ClockLinks::bare(tempo, paused), SampleRate::new(44100.0));
        let disarmed = LoopRegion::new(
            LoopRange::new(Beat::new(0.0), Beat::new(4.0)).expect("ordered"),
            false,
        );

        let mut beat = 0.0;
        for _ in 0..90000 {
            beat = clock.step_in(1, disarmed).get();
        }

        assert!(
            beat > 4.0,
            "Expected beat > 4.0 with loop disabled, got {}",
            beat
        );
    }

    #[test]
    fn test_transport_clock_loop_overshoot_precision() {
        let (tempo, paused) = create_test_atomics();
        let seek = SeekSlot::new();
        let mut links = ClockLinks::bare(tempo, paused);
        links.seek = seek.clone();
        let mut clock = TransportClock::new(links, SampleRate::new(44100.0));

        seek.request(Beat::new(0.99999));
        tick_looping(&mut clock, 0.0, 1.0);

        let beat = tick_looping(&mut clock, 0.0, 1.0);
        assert!(beat >= 0.0, "Beat should be >= 0 after wrap");
        assert!(
            beat < 0.01,
            "Beat should be near start after wrap: {}",
            beat
        );
    }

    /// Far into a session a seek still lands with its fraction: the beat is
    /// `f64` throughout.
    #[test]
    fn test_transport_clock_precision_far_into_a_session() {
        let (tempo, paused) = create_test_atomics();
        let (mut clock, seek) = clock_with_seek(tempo, paused);

        // Advance to beat ~16384 where f32 truncation would lose precision
        seek.request(Beat::new(16384.5));
        let beat = clock.step(1);
        let (whole, frac) = (beat.floor().get(), beat.fract().get());

        // Channel 0 should be the floor (16384.0)
        assert_eq!(whole, 16384.0);
        // Channel 1 should be the fractional part (~0.5) with full f32 precision
        assert!(
            (frac - 0.5).abs() < 0.001,
            "Fractional part should be ~0.5, got {frac}"
        );
    }

    /// A rate change re-derives the per-frame increment from the tempo in
    /// force, not from a request the hysteresis is holding back: the clock
    /// keeps running at the tempo it publishes, the one a graph engine's
    /// `Env` reports and `Env::for_each_beat` steps by.
    ///
    /// Mutation (run): derive it from the asked tempo in `set_sample_rate`
    /// → the first step is 120.0005 BPM's → fails.
    #[test]
    fn a_rate_change_keeps_the_tempo_in_force() {
        let (tempo, paused) = create_test_atomics();
        let in_force = Arc::new(AtomicBpm::new(Bpm::of(1.0)));
        let mut links = ClockLinks::bare(Arc::clone(&tempo), paused);
        links.tempo_in_force = Some(Arc::clone(&in_force));
        let mut clock = TransportClock::new(links, SampleRate::new(44100.0));
        // Inside the hysteresis: asked, but not taken.
        tempo.store(Bpm::of(120.0005), Ordering::Release);
        clock.set_sample_rate(tutti_types::SampleRate(48_000.0), None);
        tick(&mut clock);
        assert_eq!(in_force.load(Ordering::Acquire), Bpm::of(120.0));
        assert_eq!(
            clock.current_beat(),
            Beat(0.0) + tutti_types::beats_per_sample(Bpm::of(120.0), SampleRate::new(48_000.0)),
        );
    }

    // ---- derived beat streams ------------------------------------------

    #[test]
    fn starting_at_emits_that_beat_first() {
        let (tempo, paused) = create_test_atomics();
        let live = TransportClock::new(ClockLinks::bare(tempo, paused), SampleRate::new(44100.0));

        let mut derived = live.starting_at(Beat::new(8.0));
        let first = tick(&mut derived);

        assert!(
            (first - 8.0).abs() < 1e-4,
            "expected first beat 8.0, got {first}"
        );
    }

    #[test]
    fn derived_stream_does_not_disturb_the_live_playhead() {
        let (tempo, paused) = create_test_atomics();
        let writeback = Arc::new(AtomicF64::new(0.0));
        let mut links = ClockLinks::bare(tempo, paused);
        links.position_writeback = Some(Arc::clone(&writeback));
        let live = TransportClock::new(links, SampleRate::new(44100.0));

        // Step a derived stream for a while; the live writeback must not move.
        let mut derived = live.starting_at(Beat::new(100.0));
        for _ in 0..1000 {
            tick(&mut derived);
        }

        assert_eq!(
            writeback.load(Ordering::Acquire),
            0.0,
            "derived stream wrote into the live playhead"
        );
    }

    #[test]
    fn derived_stream_ignores_live_seek() {
        let (tempo, paused) = create_test_atomics();
        let seek = SeekSlot::new();
        let mut links = ClockLinks::bare(tempo, paused);
        links.seek = seek.clone();
        let live = TransportClock::new(links, SampleRate::new(44100.0));

        let mut derived = live.starting_at(Beat::new(20.0));

        // A live seek must not yank the derived stream.
        seek.request(Beat::new(0.0));
        let first = tick(&mut derived);
        assert!(
            (first - 20.0).abs() < 1e-4,
            "live seek leaked into the derived stream: {first}"
        );
        // ...and the live seek is still pending for the live clock.
        assert!(seek.is_pending(), "derived stream consumed the live seek");
    }

    #[test]
    fn at_tempo_changes_the_beat_rate() {
        let (tempo, paused) = create_test_atomics();
        let live = TransportClock::new(ClockLinks::bare(tempo, paused), SampleRate::new(44100.0));

        // One second of samples at 240 BPM = 4 beats.
        let mut derived = live.at_tempo(Bpm::of(240.0));
        let mut beat = 0.0;
        for _ in 0..44100 {
            beat = tick(&mut derived);
        }

        assert!(
            (beat - 4.0).abs() < 0.01,
            "expected ~4 beats at 240 BPM, got {beat}"
        );
    }

    #[test]
    fn combinators_compose() {
        let (tempo, paused) = create_test_atomics();
        let live = TransportClock::new(ClockLinks::bare(tempo, paused), SampleRate::new(44100.0));

        let mut derived = live.at_tempo(Bpm::of(240.0)).starting_at(Beat::new(8.0));
        assert!((tick(&mut derived) - 8.0).abs() < 1e-4);

        // Still at the derived tempo: one second later is 4 beats on.
        let mut beat = 0.0;
        for _ in 0..44100 {
            beat = tick(&mut derived);
        }
        assert!(
            (beat - 12.0).abs() < 0.01,
            "expected ~12.0 (8 + 4 beats at 240 BPM), got {beat}"
        );
    }

    /// A derived stream steps at the tempo it was derived at: a live tempo
    /// change must not reach it (`ClockLinks::severed` copies the tempo into
    /// a fresh cell). The pause flag is deliberately *not* a snapshot — a
    /// derived stream always rolls — so it is not checked here.
    ///
    /// Mutation (run): in `ClockLinks::severed`, keep `tempo: Arc::clone(tempo)`
    /// → the stream takes 140 BPM → fails.
    #[test]
    fn a_derived_stream_keeps_its_tempo() {
        let (tempo, paused) = create_test_atomics();
        let live = TransportClock::new(
            ClockLinks::bare(Arc::clone(&tempo), paused),
            SampleRate::new(48_000.0),
        );
        let mut derived = live.starting_at(Beat::new(0.0));
        tempo.store(Bpm::of(140.0), Ordering::Release);
        derived.step(48_000);
        assert_eq!(derived.current_beat(), Beat(2.0), "one second at 120 BPM");
    }
}
