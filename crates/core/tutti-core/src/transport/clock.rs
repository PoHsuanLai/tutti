//! Sample-accurate transport clock — the engine's playhead: it turns the
//! transport's atomics into the transport each graph block is rendered under,
//! and writes the playhead back to the transport once per block.
//!
//! It is not a graph node. The engine holds one and drives it (its
//! crate-private `begin`, `advance` and `publish_position`); a node reads the
//! transport from its block's `Env`.

use super::state::ClockLinks;
#[cfg(test)]
use super::state::LoopSpan;
use crate::Ordering;
use crate::{Beat, Bpm, Samples};
use tutti_types::FrameClock;

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

/// The untimed transport inputs a graph block is rendered under,
/// read **once** per block (at the start of the engine's walk): a store from
/// the control thread lands at the next block, never at a cut in this one.
/// Only a command the engine applies changes them mid-block
/// ([`Control::apply`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Control {
    pub(crate) tempo: Bpm,
    pub(crate) paused: bool,
    pub(crate) looping: Option<super::LoopRange>,
    /// Whether the session is recording, for the graph's
    /// [`Transport::recording`](tutti_graph::Transport::recording). Read once
    /// per block, like the rest; no command moves it mid-block.
    pub(crate) recording: bool,
}

impl Control {
    /// Read the live inputs.
    pub(crate) fn read(settings: &super::TransportSettings) -> Self {
        Self {
            tempo: settings.tempo(),
            paused: settings.is_paused(),
            looping: settings.loop_span.range(),
            recording: settings.is_recording(),
        }
    }

    /// Carry `command`'s effect, and nothing else. A motion change's effect
    /// is pausedness, which only the motion machine writes, on this thread.
    pub(crate) fn apply(
        &mut self,
        command: &super::TransportCommand,
        settings: &super::TransportSettings,
    ) {
        match *command {
            super::TransportCommand::Tempo(bpm) => self.tempo = bpm,
            super::TransportCommand::Loop(range) => self.looping = range,
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
/// **The frame is the source of truth** (doc 013 §6). The playhead is a
/// frame count on a segment (`FrameClock`), and the beat is derived from it
/// in closed form, never accumulated: at 90 BPM and 48 kHz, frame 96 000 is
/// beat 3 to the bit, where adding `beats_per_sample` 96 000 times drifts off
/// it. A seek, a tempo or rate change and a loop wrap start a new segment on
/// their frame.
#[derive(Clone, Debug)]
pub struct TransportClock {
    /// Everything shared with the live transport. A derived stream
    /// ([`starting_at`](Self::starting_at), [`at_tempo`](Self::at_tempo))
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
}

impl TransportClock {
    /// Build a clock over `links`, starting at beat 0.
    ///
    /// The one constructor: `links` carries both halves of the position
    /// handshake, so a clock cannot be assembled with its inputs wired and its
    /// writeback forgotten.
    pub fn new(links: ClockLinks, sample_rate: impl Into<crate::SampleRate>) -> Self {
        let sample_rate = sample_rate.into();
        let initial_tempo = Bpm(links.tempo.load(Ordering::Acquire));

        let clock = Self {
            links,
            clock: FrameClock::new(Beat(0.0), initial_tempo, sample_rate),
            rolling: false,
        };
        clock.publish_tempo();
        clock
    }

    /// Publish the tempo in force to the live transport.
    fn publish_tempo(&self) {
        if let Some(ref out) = self.links.tempo_in_force {
            out.store(self.clock.tempo().get(), Ordering::Release);
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
    pub fn starting_at(&self, beat: impl Into<Beat>) -> Self {
        let mut derived = self.severed();
        derived.clock.seat(beat.into());
        derived
    }

    /// Derive an independent stream running at a fixed `bpm` (severed, as
    /// [`starting_at`](Self::starting_at) is).
    pub fn at_tempo(&self, bpm: impl Into<crate::Bpm>) -> Self {
        let mut derived = self.severed();
        derived.set_tempo(bpm);
        derived
    }

    /// A clone sharing nothing with the live transport. The tempo is copied
    /// into a cell of its own; the loop is dropped.
    fn severed(&self) -> Self {
        let mut derived = self.clone();
        derived.links = derived.links.severed();
        derived
    }

    /// Set this clock's tempo, from its current frame on.
    fn set_tempo(&mut self, bpm: impl Into<crate::Bpm>) {
        let bpm = bpm.into();
        self.links.tempo.store(bpm.get(), Ordering::Release);
        self.clock.set_tempo(bpm);
        self.publish_tempo();
    }

    /// The beat this clock will emit next. Its own position, not the live
    /// transport's — a derived stream reports its private playhead here.
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
    fn apply_pending_seek(&mut self) {
        if let Some(target) = self.links.seek.take() {
            self.clock.seat(target);
        }
    }

    /// The rate this clock counts frames at.
    pub(crate) fn sample_rate(&self) -> crate::SampleRate {
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
    pub(crate) fn begin(&mut self, control: &Control, take_seek: bool) -> tutti_graph::Transport {
        if take_seek {
            self.apply_pending_seek();
        }
        self.take_tempo(control.tempo);
        self.note_rolling(!control.paused);
        // Counted: the frame count the beat is derived from, so the graph's
        // `Env::for_each_beat` and `Env::transport_at` continue this clock
        // with its own code.
        tutti_graph::Transport::counted(
            !control.paused,
            self.clock.tempo(),
            self.clock.origin(),
            control.looping.map(|r| tutti_graph::LoopRange {
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
    pub(crate) fn advance(&mut self, frames: usize, from: &tutti_graph::Transport) {
        if from.playing {
            let region = from
                .looping
                .and_then(|l| super::LoopRange::new(l.start, l.end));
            self.clock.advance(Samples(frames), region);
        }
        self.advance_steady_time(frames);
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
    pub(crate) fn publish_position(&self) {
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
    pub(crate) fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        self.clock.set_sample_rate(sample_rate);
    }

    /// Step `frames` as the engine steps a block, under the transport's live
    /// inputs read off this clock's links (what `Control::read` reads off the
    /// settings): take a pending seek and the tempo, advance, publish. The
    /// beat of the block's first frame. For tests that drive a bare clock.
    #[cfg(test)]
    pub(crate) fn step(&mut self, frames: usize) -> Beat {
        let control = Control {
            tempo: Bpm(self.links.tempo.load(Ordering::Acquire)),
            paused: self.links.paused.load(Ordering::Acquire),
            looping: self.links.loop_span.as_ref().and_then(LoopSpan::range),
            recording: false,
        };
        let t = self.begin(&control, true);
        let first = self.clock.beat();
        self.advance(frames, &t);
        self.publish_position();
        first
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::SeekSlot;
    use super::*;
    use crate::{AtomicBool, AtomicF64};
    use std::sync::Arc;

    fn create_test_atomics() -> (Arc<AtomicF64>, Arc<AtomicBool>) {
        (
            Arc::new(AtomicF64::new(120.0)),  // 120 BPM
            Arc::new(AtomicBool::new(false)), // Not paused
        )
    }

    /// A clock wired to a loop span, via the one real constructor.
    fn clock_with_loop(
        tempo: Arc<AtomicF64>,
        paused: Arc<AtomicBool>,
        loop_span: LoopSpan,
    ) -> TransportClock {
        TransportClock::new(
            ClockLinks {
                tempo,
                paused,
                seek: SeekSlot::new(),
                loop_span: Some(loop_span),
                position_writeback: None,
                segment_generation: None,
                steady_time: None,
                tempo_in_force: None,
                claim: None,
            },
            44100.0,
        )
    }

    /// A clock plus the seek slot that drives it.
    fn clock_with_seek(
        tempo: Arc<AtomicF64>,
        paused: Arc<AtomicBool>,
    ) -> (TransportClock, SeekSlot) {
        let seek = SeekSlot::new();
        let clock = TransportClock::new(
            ClockLinks {
                tempo,
                paused,
                seek: seek.clone(),
                loop_span: Some(LoopSpan::default()),
                position_writeback: None,
                segment_generation: None,
                steady_time: None,
                tempo_in_force: None,
                claim: None,
            },
            44100.0,
        );
        (clock, seek)
    }

    /// One frame, as the engine steps a one-frame block: the beat the frame
    /// carries (what `tick` emitted on its ports).
    fn tick(clock: &mut TransportClock) -> f64 {
        clock.step(1).get()
    }

    #[test]
    fn test_transport_clock_tick() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(ClockLinks::bare(tempo, paused), 44100.0);

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
        let clock = TransportClock::new(links, 44100.0);

        let mut render = clock.starting_at(0.0);

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
        let steady = Arc::new(crate::AtomicI64::new(0));
        let mut links = ClockLinks::bare(Arc::clone(&tempo), Arc::clone(&paused));
        links.steady_time = Some(Arc::clone(&steady));
        // A two-beat loop, so the playhead wraps repeatedly over the run.
        links.loop_span = Some({
            let s = LoopSpan::new(0.0, 2.0);
            s.set_enabled(true);
            s
        });
        let mut clock = TransportClock::new(links, 44100.0);

        clock.step(64);
        assert_eq!(steady.load(Ordering::Relaxed), 64);

        // Paused: musical time stops, sample time does not.
        paused.store(true, Ordering::Release);
        let beat_while_paused = clock.current_beat();
        clock.step(64);
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
            clock.step(64);
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
        let steady = Arc::new(crate::AtomicI64::new(9_000));
        let mut links = ClockLinks::bare(tempo, paused);
        links.steady_time = Some(Arc::clone(&steady));
        let clock = TransportClock::new(links, 44100.0);

        let mut render = clock.starting_at(0.0);
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
        let mut clock = TransportClock::new(ClockLinks::bare(tempo, paused.clone()), 44100.0);

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

        seek.request(4.0);

        assert!((tick(&mut clock) - 4.0).abs() < 0.001);
    }

    #[test]
    fn test_transport_clock_tempo_change() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(ClockLinks::bare(tempo.clone(), paused), 44100.0);

        for _ in 0..44100 {
            tick(&mut clock);
        }

        tempo.store(240.0, Ordering::Release);

        let mut beat = 0.0;
        for _ in 0..44100 {
            beat = tick(&mut clock);
        }

        assert!((beat - 6.0).abs() < 0.1);
    }

    #[test]
    fn test_transport_clock_loop_disabled() {
        let (tempo, paused) = create_test_atomics();
        let loop_span = LoopSpan::new(0.0, 4.0);
        loop_span.set_enabled(false);

        let mut clock = clock_with_loop(tempo, paused, loop_span);

        let mut beat = 0.0;
        for _ in 0..90000 {
            beat = tick(&mut clock);
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
        let loop_span = LoopSpan::new(0.0, 1.0);
        loop_span.set_enabled(true);

        let seek = SeekSlot::new();
        let mut clock = TransportClock::new(
            ClockLinks {
                tempo,
                paused,
                seek: seek.clone(),
                loop_span: Some(loop_span),
                position_writeback: None,
                segment_generation: None,
                steady_time: None,
                tempo_in_force: None,
                claim: None,
            },
            44100.0,
        );

        seek.request(0.99999);
        tick(&mut clock);

        let beat = tick(&mut clock);
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
        seek.request(16384.5);
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
    /// (the old code) → the first step is 120.0005 BPM's → fails.
    #[test]
    fn a_rate_change_keeps_the_tempo_in_force() {
        let (tempo, paused) = create_test_atomics();
        let in_force = Arc::new(AtomicF64::new(0.0));
        let mut links = ClockLinks::bare(Arc::clone(&tempo), paused);
        links.tempo_in_force = Some(Arc::clone(&in_force));
        let mut clock = TransportClock::new(links, 44100.0);
        // Inside the hysteresis: asked, but not taken.
        tempo.store(120.0005, Ordering::Release);
        clock.set_sample_rate(crate::SampleRate(48_000.0));
        tick(&mut clock);
        assert_eq!(in_force.load(Ordering::Acquire), 120.0);
        assert_eq!(
            clock.current_beat(),
            Beat(0.0) + super::super::state::beats_per_sample(Bpm(120.0), 48_000.0),
        );
    }

    // ---- derived beat streams ------------------------------------------

    #[test]
    fn starting_at_emits_that_beat_first() {
        let (tempo, paused) = create_test_atomics();
        let live = TransportClock::new(ClockLinks::bare(tempo, paused), 44100.0);

        let mut derived = live.starting_at(8.0);
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
        let live = TransportClock::new(links, 44100.0);

        // Step a derived stream for a while; the live writeback must not move.
        let mut derived = live.starting_at(100.0);
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
    fn derived_stream_ignores_live_seek_and_loop() {
        let (tempo, paused) = create_test_atomics();
        let seek = SeekSlot::new();
        let loop_span = LoopSpan::new(0.0, 4.0);
        loop_span.set_enabled(true);
        let live = TransportClock::new(
            ClockLinks {
                tempo,
                paused,
                seek: seek.clone(),
                loop_span: Some(loop_span.clone()),
                position_writeback: None,
                segment_generation: None,
                steady_time: None,
                tempo_in_force: None,
                claim: None,
            },
            44100.0,
        );

        let mut derived = live.starting_at(20.0);

        // A live seek must not yank the derived stream.
        seek.request(0.0);
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
        let live = TransportClock::new(ClockLinks::bare(tempo, paused), 44100.0);

        // One second of samples at 240 BPM = 4 beats.
        let mut derived = live.at_tempo(240.0);
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
        let live = TransportClock::new(ClockLinks::bare(tempo, paused), 44100.0);

        let mut derived = live.at_tempo(240.0).starting_at(8.0);
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
        let live = TransportClock::new(ClockLinks::bare(Arc::clone(&tempo), paused), 48_000.0);
        let mut derived = live.starting_at(0.0);
        tempo.store(140.0, Ordering::Release);
        derived.step(48_000);
        assert_eq!(derived.current_beat(), Beat(2.0), "one second at 120 BPM");
    }
}
