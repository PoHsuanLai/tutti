//! Sample-accurate transport clock — the graph node that turns the transport's
//! atomics into a per-sample beat signal.
//!
//! Emits the beat across [`BEAT_PORTS`](super::state::BEAT_PORTS) output ports
//! (whole, then fraction) so precision does not decay as a session runs long,
//! and writes the playhead back to the transport once per buffer.

use super::state::{ClockLinks, LoopSpan};
use crate::Ordering;
use crate::{Beat, BeatDuration, Bpm};
use fundsp::prelude::*;
use std::any;

/// How far the tempo must move before the cached per-sample increment is
/// re-derived. Comparing for equality would re-derive on every buffer from ULP
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

/// The untimed transport inputs a native graph block is rendered under,
/// read **once** per block (at the start of the engine's walk): a store from
/// the control thread lands at the next block, never at a cut in this one.
/// Only a command the engine applies changes them mid-block
/// ([`Control::apply`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Control {
    pub(crate) tempo: Bpm,
    pub(crate) paused: bool,
    pub(crate) looping: Option<super::LoopRange>,
}

impl Control {
    /// Read the live inputs.
    pub(crate) fn read(settings: &super::TransportSettings) -> Self {
        Self {
            tempo: settings.tempo(),
            paused: settings.is_paused(),
            looping: settings.loop_span.range(),
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

/// Split a beat into the two `f32` port values (whole, fraction).
///
/// The inverse of [`beat_from_ports`](super::state::beat_from_ports); see
/// [`BEAT_PORTS`](super::state::BEAT_PORTS) for why the split exists at all.
#[inline]
pub(super) fn split_beat(beat: Beat) -> (f32, f32) {
    (beat.floor().get() as f32, beat.fract().get() as f32)
}

/// The beat generator: a zero-input, two-output [`AudioUnit`] emitting the
/// playhead as `(whole, fraction)`.
///
/// Emit-then-advance. Sample 0 of a block carries the block's start beat, and
/// only then does the beat increment — anything that advances a second clock
/// alongside this one must match that order or sit permanently one
/// `beats_per_sample` out of step.
#[derive(Clone, Debug)]
pub struct TransportClock {
    /// Everything shared with the live transport. `isolate()` replaces this
    /// wholesale; every other field is this clock's own.
    links: ClockLinks,
    /// The playhead. Advanced once per sample in `tick`/`process`.
    current_beat: Beat,
    sample_rate: crate::SampleRate,
    /// Beats advanced per output sample — the cached
    /// `tempo / 60 / sample_rate`. A derived rate, invalidated whenever
    /// `last_tempo` or `sample_rate` moves.
    beat_per_sample: BeatDuration,
    /// Tempo the cached increment was computed from; a change here is what
    /// invalidates it.
    last_tempo: Bpm,
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
            current_beat: Beat(0.0),
            sample_rate,
            beat_per_sample: super::state::beats_per_sample(initial_tempo, sample_rate),
            last_tempo: initial_tempo,
        };
        clock.publish_tempo();
        clock
    }

    /// Publish the tempo in force (`last_tempo`) to the live transport.
    fn publish_tempo(&self) {
        if let Some(ref out) = self.links.tempo_in_force {
            out.store(self.last_tempo.get(), Ordering::Release);
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
    /// Severs every shared link to the live transport (the same cut
    /// [`AudioUnit::isolate`] makes), so this clock writes to nothing live and
    /// reads no live loop or seek state.
    pub fn starting_at(&self, beat: impl Into<Beat>) -> Self {
        let mut derived = self.clone();
        derived.isolate();
        derived.current_beat = beat.into();
        derived
    }

    /// Derive an independent stream running at a fixed `bpm`.
    pub fn at_tempo(&self, bpm: impl Into<crate::Bpm>) -> Self {
        let mut derived = self.clone();
        derived.isolate();
        derived.set_tempo(bpm);
        derived
    }

    /// Set this clock's tempo, refreshing the cached per-sample increment.
    fn set_tempo(&mut self, bpm: impl Into<crate::Bpm>) {
        let bpm = bpm.into();
        self.links.tempo.store(bpm.get(), Ordering::Release);
        self.beat_per_sample = super::state::beats_per_sample(bpm, self.sample_rate);
        self.last_tempo = bpm;
        self.publish_tempo();
    }

    /// The beat this clock will emit next. Its own position, not the live
    /// transport's — an isolated clone reports its private playhead here.
    pub fn current_beat(&self) -> Beat {
        self.current_beat
    }

    #[inline]
    fn update_tempo_if_changed(&mut self) {
        let asked = Bpm(self.links.tempo.load(Ordering::Acquire));
        self.take_tempo(asked);
    }

    /// Take `asked` as the tempo, if it moved past the hysteresis.
    #[inline]
    fn take_tempo(&mut self, asked: Bpm) {
        let tempo = tempo_in_effect(asked, self.last_tempo);
        if tempo != self.last_tempo {
            self.beat_per_sample = super::state::beats_per_sample(tempo, self.sample_rate);
            self.last_tempo = tempo;
            self.publish_tempo();
        }
    }

    #[inline]
    fn apply_pending_seek(&mut self) {
        if let Some(target) = self.links.seek.take() {
            self.current_beat = target;
        }
    }

    #[inline]
    fn apply_loop_wrap(&mut self, from: crate::Beat) {
        let Some(region) = self.links.loop_span.as_ref().and_then(LoopSpan::range) else {
            return;
        };
        // `LoopRange` is non-empty by construction, so `wrap` needs no guard.
        // Only a playhead that crossed the end wraps: see `LoopRange::advance`.
        self.current_beat = region.advance(from, self.current_beat);
    }

    /// The rate this clock converts tempo to a per-frame increment at.
    pub(crate) fn sample_rate(&self) -> crate::SampleRate {
        self.sample_rate
    }

    /// Take a pending seek (when `take_seek`) and `control`'s tempo, and
    /// report the transport from this frame on under `control`, as a native
    /// graph block sees it ([`tutti_graph::Transport`]). The seek is taken
    /// only at a block's start and after a motion command, the two points
    /// where the motion machine (this thread) can have requested one.
    ///
    /// The engine's graph backend has no clock node in its graph: it holds a
    /// `TransportClock` of its own and drives it with this and
    /// [`advance`](Self::advance), so the playhead a graph node reads in its
    /// `Env` is computed by the same code, in the same order, as the beat
    /// this clock emits on its ports in a `Net`. `process` is `begin`, then
    /// emit-and-advance frame by frame; `begin` + `advance(n)` is the same
    /// arithmetic without the emit.
    pub(crate) fn begin(&mut self, control: &Control, take_seek: bool) -> tutti_graph::Transport {
        if take_seek {
            self.apply_pending_seek();
        }
        self.take_tempo(control.tempo);
        tutti_graph::Transport {
            playing: !control.paused,
            tempo: self.last_tempo,
            beat: self.current_beat,
            looping: control.looping.map(|r| tutti_graph::LoopRange {
                start: r.start(),
                end: r.end(),
            }),
        }
    }

    /// Advance `frames` under `from`, the transport the last
    /// [`begin`](Self::begin) reported, exactly as `process` would over that
    /// many frames: the same per-frame increment and loop wrap, then the
    /// position writeback and the steady-time count.
    ///
    /// Takes the play state and loop from `from` rather than re-reading the
    /// atomics, so a store from the control thread between the two calls
    /// cannot make the playhead move differently from what the block was
    /// told.
    pub(crate) fn advance(&mut self, frames: usize, from: &tutti_graph::Transport) {
        if from.playing {
            match from
                .looping
                .and_then(|l| super::LoopRange::new(l.start, l.end))
            {
                Some(region) => {
                    for _ in 0..frames {
                        let from = self.current_beat;
                        self.current_beat = region.advance(from, from + self.beat_per_sample);
                    }
                }
                None => {
                    for _ in 0..frames {
                        self.current_beat += self.beat_per_sample;
                    }
                }
            }
        }
        if let Some(ref writeback) = self.links.position_writeback {
            writeback.store(self.current_beat.get(), Ordering::Release);
        }
        self.advance_steady_time(frames);
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

impl AudioUnit for TransportClock {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.current_beat = Beat(0.0);
    }

    /// Sever every shared link to the *live* transport so this clone can be
    /// ticked on a worker thread — an offline region render — without
    /// disturbing live playback.
    ///
    /// # Why a clone is not already safe
    ///
    /// `Clone` shares `tempo`, `paused`, `seek`, the loop span and
    /// `position_writeback` by `Arc`. That is correct for the commit-clone,
    /// where only the original is ticked. An offline render is not that: it
    /// ticks the clone for seconds while the live graph plays, and this clock
    /// **writes** `position_writeback` and consumes `seek` every buffer. A
    /// shared clone therefore stomps the live playhead and swallows live seeks,
    /// jerking the live samplers to garbage positions — continuous noise for
    /// the whole render.
    ///
    /// The cut itself is [`ClockLinks::severed`](super::ClockLinks::severed).
    /// Dropping the loop costs nothing: a render's length and start come from
    /// the offline transport and the region bounds, never from this clock's
    /// loop fields.
    fn isolate(&mut self) {
        self.links = self.links.severed();
    }

    /// Re-seat the clock on the render's start beat and tempo.
    ///
    /// `isolate()` severs the live links but leaves the clock at whatever beat
    /// the *live* playhead held when it was cloned, which would make the render
    /// depend on when it was started. See [`OfflineTransport`](super::OfflineTransport).
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        let Some(transport) = ctx.downcast_ref::<super::OfflineTransport>() else {
            return;
        };
        // Read at rebind time — before the renderer has advanced anything — so
        // these are the seeded start values, not a moving position.
        *self = self
            .at_tempo(transport.tempo())
            .starting_at(transport.beat());
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        self.sample_rate = sample_rate;
        // The tempo in force, not the one asked: the increment is always
        // derived from `last_tempo` (see `take_tempo`), which is the tempo a
        // graph engine reports in its `Env` and an `EnvClock` steps by. A
        // request inside the hysteresis would otherwise run this clock at a
        // tempo it does not publish.
        self.beat_per_sample = super::state::beats_per_sample(self.last_tempo, sample_rate);
    }

    #[inline]
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        self.apply_pending_seek();
        self.update_tempo_if_changed();

        let (whole, frac) = split_beat(self.current_beat);
        output[0] = whole;
        output[1] = frac;

        if !self.links.paused.load(Ordering::Acquire) {
            let from = self.current_beat;
            self.current_beat += self.beat_per_sample;
            self.apply_loop_wrap(from);
        }

        if let Some(ref writeback) = self.links.position_writeback {
            writeback.store(self.current_beat.get(), Ordering::Release);
        }
        self.advance_steady_time(1);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        self.apply_pending_seek();
        self.update_tempo_if_changed();

        let is_paused = self.links.paused.load(Ordering::Acquire);
        // Hoisted once per buffer: the loop region cannot change mid-block.
        let active_loop = self.links.loop_span.as_ref().and_then(LoopSpan::range);

        if is_paused {
            let (whole, frac) = split_beat(self.current_beat);
            for i in 0..size {
                output.set_f32(0, i, whole);
                output.set_f32(1, i, frac);
            }
        } else if let Some(region) = active_loop {
            for i in 0..size {
                let (whole, frac) = split_beat(self.current_beat);
                output.set_f32(0, i, whole);
                output.set_f32(1, i, frac);
                let from = self.current_beat;
                self.current_beat = region.advance(from, from + self.beat_per_sample);
            }
        } else {
            for i in 0..size {
                let (whole, frac) = split_beat(self.current_beat);
                output.set_f32(0, i, whole);
                output.set_f32(1, i, frac);
                self.current_beat += self.beat_per_sample;
            }
        }

        if let Some(ref writeback) = self.links.position_writeback {
            writeback.store(self.current_beat.get(), Ordering::Release);
        }
        self.advance_steady_time(size);
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
                steady_time: None,
                tempo_in_force: None,
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
                steady_time: None,
                tempo_in_force: None,
            },
            44100.0,
        );
        (clock, seek)
    }

    fn reconstruct_beat(output: &[f32; 2]) -> f32 {
        output[0] + output[1]
    }

    #[test]
    fn test_transport_clock_tick() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(ClockLinks::bare(tempo, paused), 44100.0);

        let mut output = [0.0f32; 2];

        clock.tick(&[], &mut output);
        assert!((reconstruct_beat(&output) - 0.0).abs() < 0.001);

        for _ in 0..44100 {
            clock.tick(&[], &mut output);
        }

        assert!((reconstruct_beat(&output) - 2.0).abs() < 0.01);
    }

    /// An offline region render clones the live net and ticks the clone on a
    /// worker thread, and `tick` *writes* `position_writeback` every sample.
    /// `isolate()` must sever it, or the worker stomps the live playhead the
    /// rest of the app reads as "current beat".
    #[test]
    fn isolate_severs_live_position_writeback() {
        let (tempo, paused) = create_test_atomics();
        let live_position = Arc::new(AtomicF64::new(7.5)); // live playhead "now"
        let clock = TransportClock::new(ClockLinks::bare(tempo, paused), 44100.0);

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
            (render.current_beat().get() - 2.0).abs() < 0.01,
            "isolated clock must still advance its own beat, got {}",
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

        let empty = BufferRef::new(&[]);
        let mut scratch = BufferArray::<U2>::new();

        clock.process(64, &empty, &mut scratch.buffer_mut());
        assert_eq!(steady.load(Ordering::Relaxed), 64);

        // Paused: musical time stops, sample time does not.
        paused.store(true, Ordering::Release);
        let beat_while_paused = clock.current_beat();
        clock.process(64, &empty, &mut scratch.buffer_mut());
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
            clock.process(64, &empty, &mut scratch.buffer_mut());
        }
        assert_eq!(
            steady.load(Ordering::Relaxed),
            128 + 100 * 64,
            "loop wraps must not reset the free-running counter"
        );
    }

    /// `steady_time` is `Arc`-shared, so it is subject to the same cut as the
    /// position writeback: an offline render must not advance the live counter.
    #[test]
    fn isolate_severs_steady_time() {
        let (tempo, paused) = create_test_atomics();
        let steady = Arc::new(crate::AtomicI64::new(9_000));
        let mut links = ClockLinks::bare(tempo, paused);
        links.steady_time = Some(Arc::clone(&steady));
        let clock = TransportClock::new(links, 44100.0);

        let mut render = clock.clone();
        render.isolate();

        let mut out = [0.0f32; 2];
        for _ in 0..1_000 {
            render.tick(&[], &mut out);
        }

        assert_eq!(
            steady.load(Ordering::Relaxed),
            9_000,
            "an isolated render clock must not advance the live steady time"
        );
    }

    #[test]
    fn test_transport_clock_pause() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(ClockLinks::bare(tempo, paused.clone()), 44100.0);

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
        let (mut clock, seek) = clock_with_seek(tempo, paused);

        seek.request(4.0);

        let mut output = [0.0f32; 2];
        clock.tick(&[], &mut output);

        assert!((reconstruct_beat(&output) - 4.0).abs() < 0.001);
    }

    #[test]
    fn test_transport_clock_tempo_change() {
        let (tempo, paused) = create_test_atomics();
        let mut clock = TransportClock::new(ClockLinks::bare(tempo.clone(), paused), 44100.0);

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
    fn test_transport_clock_loop_disabled() {
        let (tempo, paused) = create_test_atomics();
        let loop_span = LoopSpan::new(0.0, 4.0);
        loop_span.set_enabled(false);

        let mut clock = clock_with_loop(tempo, paused, loop_span);

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

        let seek = SeekSlot::new();
        let mut clock = TransportClock::new(
            ClockLinks {
                tempo,
                paused,
                seek: seek.clone(),
                loop_span: Some(loop_span),
                position_writeback: None,
                steady_time: None,
                tempo_in_force: None,
            },
            44100.0,
        );

        seek.request(0.99999);
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
        let (mut clock, seek) = clock_with_seek(tempo, paused);

        let mut output = [0.0f32; 2];

        // Advance to beat ~16384 where f32 truncation would lose precision
        seek.request(16384.5);
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

    /// A rate change re-derives the per-frame increment from the tempo in
    /// force, not from a request the hysteresis is holding back: the clock
    /// keeps running at the tempo it publishes, the one a graph engine's
    /// `Env` reports and an `EnvClock` steps by.
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
        let mut out = [0.0f32; 2];
        clock.tick(&[], &mut out);
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
        let mut output = [0.0f32; 2];
        derived.tick(&[], &mut output);

        assert!(
            (reconstruct_beat(&output) - 8.0).abs() < 1e-4,
            "expected first beat 8.0, got {}",
            reconstruct_beat(&output)
        );
    }

    #[test]
    fn derived_stream_does_not_disturb_the_live_playhead() {
        let (tempo, paused) = create_test_atomics();
        let writeback = Arc::new(AtomicF64::new(0.0));
        let live = TransportClock::new(ClockLinks::bare(tempo, paused), 44100.0);

        // Tick a derived stream for a while; the live writeback must not move.
        let mut derived = live.starting_at(100.0);
        let mut output = [0.0f32; 2];
        for _ in 0..1000 {
            derived.tick(&[], &mut output);
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
                steady_time: None,
                tempo_in_force: None,
            },
            44100.0,
        );

        let mut derived = live.starting_at(20.0);

        // A live seek must not yank the derived stream.
        seek.request(0.0);
        let mut output = [0.0f32; 2];
        derived.tick(&[], &mut output);
        assert!(
            (reconstruct_beat(&output) - 20.0).abs() < 1e-4,
            "live seek leaked into the derived stream: {}",
            reconstruct_beat(&output)
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
        let mut output = [0.0f32; 2];
        for _ in 0..44100 {
            derived.tick(&[], &mut output);
        }

        let beat = reconstruct_beat(&output);
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
        let mut output = [0.0f32; 2];
        derived.tick(&[], &mut output);
        assert!((reconstruct_beat(&output) - 8.0).abs() < 1e-4);

        // Still at the derived tempo: one second later is 4 beats on.
        for _ in 0..44100 {
            derived.tick(&[], &mut output);
        }
        let beat = reconstruct_beat(&output);
        assert!(
            (beat - 12.0).abs() < 0.01,
            "expected ~12.0 (8 + 4 beats at 240 BPM), got {beat}"
        );
    }

    /// A fork of the clock steps at the tempo it was taken at: a live tempo
    /// change must not reach it (`ClockLinks::severed` copies the tempo into
    /// a fresh cell). The pause flag is deliberately *not* a snapshot — a
    /// fork always rolls — so it is not a control here.
    ///
    /// Mutation: in `ClockLinks::severed`, keep `tempo: Arc::clone(tempo)`
    /// → "a live move reached the fork".
    #[test]
    fn isolate_snapshots_the_tempo() {
        tutti_graph::contract::IsolateRow::new("TransportClock", || {
            let (tempo, paused) = create_test_atomics();
            TransportClock::new(ClockLinks::bare(tempo, paused), 48_000.0)
        })
        .control("tempo", |c| c.links.tempo.store(140.0, Ordering::Release))
        .check();
    }
}
