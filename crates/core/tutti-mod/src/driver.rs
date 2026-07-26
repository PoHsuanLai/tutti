//! The modulation **driver** — [`ModPreFrame`], the once-per-frame producer.
//!
//! It reads the routing snapshot, samples each source, and dispatches the
//! shaped offset into each target by id through a [`ModRouter`].
//!
//! Two things worth calling out:
//!
//! - **It owns the sources and threads their state.** Modulation sources are
//!   stateful scan samplers ([`Modulator`]), so the driver holds the source
//!   registry and threads each source's `Modulator::State` across frames. Each
//!   source is sampled **once per frame**, then its value is fanned across all
//!   its edges (so a source driving three targets advances its phase/RNG exactly
//!   once).
//! - **It clears stale layers on hot-swap.** Modulation values are continuous
//!   and never dropped, so when an edge disappears its last offset would linger
//!   in the target forever. The driver remembers the previously-active
//!   `(target, key)` set and clears any that the current snapshot no longer
//!   contains — the "continuous-value tax".

use std::sync::Arc;

use tutti_types::RtPublish;
use tutti_types::{Beat, Hz, Seconds};

use crate::id::{LayerKey, ModTargetId};
use crate::router::ModRouter;
use crate::routing::ModRoutingSnapshot;
use crate::shape::shape;
use crate::Modulator;

/// A source's own rate — how its phase is generated from the transport, so each
/// LFO runs at its own frequency (unlike one shared phase for all sources).
///
/// The values are the transport's own vocabulary (`Beat`/`Seconds` reach the
/// driver via [`ModPreFrame::run`]); the driver does not depend on the transport
/// itself, only on the two scalars a caller reads off it.
#[derive(Debug, Clone, Copy)]
pub struct SourceRate {
    /// `beat_synced`: cycles per beat. Free-running: cycles per second (`Hz`).
    pub frequency: Hz,
    /// Constant phase shift in `[0, 1)` applied after phase generation.
    pub phase_offset: f32,
    /// `true`: phase = `(beat / frequency + offset) % 1` (locks to transport);
    /// `false`: integrate `frequency * dt` into a free-running accumulator.
    pub beat_synced: bool,
}

impl SourceRate {
    /// A source locked to the transport at `frequency` cycles per beat.
    pub fn beat_synced(frequency: impl Into<Hz>, phase_offset: f32) -> Self {
        Self {
            frequency: frequency.into(),
            phase_offset,
            beat_synced: true,
        }
    }

    /// A free-running source at `frequency` Hz (cycles per second).
    pub fn free_running(frequency: impl Into<Hz>, phase_offset: f32) -> Self {
        Self {
            frequency: frequency.into(),
            phase_offset,
            beat_synced: false,
        }
    }
}

/// Object-safe erased source: samples once against the transport `(beat, dt)`,
/// advancing internal state, returning a raw `[-1, 1]` value.
///
/// [`Modulator`] stays the pure, state-threaded `&self` trait; this erasure is a
/// driver-side concern so the registry can be a heterogeneous
/// `Vec<Box<dyn ErasedModulator>>` (the associated `State` type differs per
/// modulator, so `dyn Modulator` isn't object-safe across them). [`Sourced`]
/// bridges any `Modulator` into it, owning the state **and** its per-source phase
/// — the driver is the mutable owner.
pub trait ErasedModulator: Send + Sync {
    /// Derive this source's phase from `(beat, dt)` via its own rate, advance
    /// state + phase, return the raw `[-1, 1]` value.
    fn sample(&mut self, beat: Beat, dt: Seconds) -> f32;
}

/// Pairs a [`Modulator`] with its threaded state, its [`SourceRate`], and its
/// free-running phase accumulator, erasing the associated type.
pub struct Sourced<M: Modulator> {
    modulator: M,
    state: M::State,
    rate: SourceRate,
    /// Free-running accumulated phase (unused when `rate.beat_synced`).
    phase: f32,
}

impl<M: Modulator> Sourced<M> {
    /// A source with an explicit [`SourceRate`].
    pub fn new(modulator: M, rate: SourceRate) -> Self {
        Self {
            modulator,
            state: M::State::default(),
            rate,
            phase: 0.0,
        }
    }

    /// This frame's phase in `[0, 1)`, advancing the free-running accumulator.
    #[inline]
    fn tick_phase(&mut self, beat: Beat, dt: Seconds) -> f32 {
        let freq = self.rate.frequency.get();
        let base = if self.rate.beat_synced {
            if freq.abs() < f32::EPSILON {
                0.0
            } else {
                (beat.get() as f32) / freq
            }
        } else {
            self.phase = (self.phase + freq * dt.get()).rem_euclid(1.0);
            self.phase
        };
        (base + self.rate.phase_offset).rem_euclid(1.0)
    }
}

impl<M: Modulator + Send + Sync> ErasedModulator for Sourced<M> {
    #[inline]
    fn sample(&mut self, beat: Beat, dt: Seconds) -> f32 {
        let phase = self.tick_phase(beat, dt);
        let (next, v) = self.modulator.value(self.state, phase);
        self.state = next;
        v
    }
}

/// Once-per-frame modulation producer. Holds the source registry, a routing
/// snapshot handle, and a router; [`run`](Self::run) samples + dispatches.
///
/// The source registry indices must line up with [`crate::ModEdge::source`].
pub struct ModPreFrame {
    sources: Vec<Box<dyn ErasedModulator>>,
    routing: Arc<RtPublish<ModRoutingSnapshot>>,
    router: Option<Arc<dyn ModRouter>>,
    /// The `(target, key)` layers written last frame — cleared next frame if the
    /// current snapshot no longer contains them.
    prev_active: Vec<(ModTargetId, LayerKey)>,
}

impl ModPreFrame {
    /// Build a driver reading `routing`. Install sources and a router before
    /// running.
    pub fn new(routing: Arc<RtPublish<ModRoutingSnapshot>>) -> Self {
        Self {
            sources: Vec::new(),
            routing,
            router: None,
            prev_active: Vec::new(),
        }
    }

    /// Install the dispatch router (the id→target map).
    pub fn set_router(&mut self, router: Arc<dyn ModRouter>) {
        self.router = Some(router);
    }

    /// Install the source registry. Index `i` is referenced by
    /// `ModEdge::source == i`.
    pub fn set_sources(&mut self, sources: Vec<Box<dyn ErasedModulator>>) {
        self.sources = sources;
    }

    /// Run one frame against the transport `(beat, dt)`.
    ///
    /// The caller reads `beat` (the same `Beat` the metronome reads) and the
    /// seconds `dt` since the last frame off the transport, and passes them in;
    /// the driver keeps no transport state of its own. Each source derives its
    /// own phase from `(beat, dt)` and its [`SourceRate`] (so LFOs run at
    /// independent frequencies), samples once (advancing its state), shapes +
    /// scales into target units, dispatches by id, and clears any layer that
    /// disappeared since the last frame.
    pub fn run(&mut self, beat: Beat, dt: Seconds) {
        let Some(router) = self.router.clone() else {
            return;
        };
        let snapshot = self.routing.read();

        // Sample each source ONCE (advancing its state), fan across its edges.
        let mut active: Vec<(ModTargetId, LayerKey)> = Vec::new();
        for (idx, source) in self.sources.iter_mut().enumerate() {
            // Skip sampling a source with no edges — but only if there truly are
            // none this frame; still advance nothing (a source with no routing
            // shouldn't burn phase/RNG state).
            let mut edges = snapshot.edges_for_source(idx).peekable();
            if edges.peek().is_none() {
                continue;
            }
            let raw = source.sample(beat, dt); // [-1, 1], state + phase advance once
            for edge in edges {
                // Offset in target units: shape applies the edge depth + curve +
                // polarity, then scale by the range span. Matches the
                // control-rate contract `raw * depth * (max - min)` (no 0.5).
                let offset =
                    shape(raw, edge.depth, edge.polarity, edge.curve) * (edge.max - edge.min);
                router.accumulate(edge.target, edge.key, offset);
                active.push((edge.target, edge.key));
            }
        }

        // Clear layers that were active last frame but aren't this frame — a
        // removed/disabled edge must not leave a stuck offset.
        for (target, key) in self.prev_active.iter().copied() {
            if !active.contains(&(target, key)) {
                router.clear(target, key);
            }
        }
        self.prev_active = active;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::AtomicTarget;
    use crate::router::ModBus;
    use crate::routing::{ModEdge, ModRoutingTable};
    use crate::target::ModTarget; // for `final_value()` on the target handles
    use crate::{Lfo, LfoShape};

    /// A beat-synced source at 1 cycle/beat: with `frame(p)` its phase is
    /// exactly `p`, so these tests drive a precise phase per frame as before.
    fn source(shape: LfoShape) -> Box<dyn ErasedModulator> {
        Box::new(Sourced::new(
            Lfo::new(shape),
            SourceRate::beat_synced(Hz(1.0), 0.0),
        ))
    }

    fn sine_source() -> Box<dyn ErasedModulator> {
        source(LfoShape::Sine)
    }

    /// Drive one frame at phase `p` (beat == p, since freq is 1 cycle/beat).
    fn frame(driver: &mut ModPreFrame, p: f32) {
        driver.run(Beat(p as f64), Seconds(0.0));
    }

    #[test]
    fn drives_a_target_within_range() {
        let cutoff = Arc::new(AtomicTarget::new(1000.0, 0.0, 2000.0));
        let id = ModTargetId::next();
        let bus = Arc::new(ModBus::new());
        bus.insert(id, cutoff.clone());

        let mut table = ModRoutingTable::new();
        table.set_edges([ModEdge::linear(0, id, LayerKey(1), 0.5, 0.0, 2000.0)], 1);
        table.commit();

        let mut driver = ModPreFrame::new(table.snapshot_arc());
        driver.set_router(bus.clone());
        driver.set_sources(vec![sine_source()]);

        for i in 0..8 {
            frame(&mut driver, i as f32 / 8.0);
            let v = cutoff.final_value();
            assert!((0.0..=2000.0).contains(&v), "out of range: {v}");
        }
    }

    #[test]
    fn one_source_fans_to_many_targets_sampled_once() {
        // A source with two edges must advance its state ONCE per frame — verify
        // by driving two Random-shaped targets from one source: both edges get
        // the SAME raw value each frame.
        let a = Arc::new(AtomicTarget::new(0.0, -1.0, 1.0));
        let b = Arc::new(AtomicTarget::new(0.0, -1.0, 1.0));
        let (ida, idb) = (ModTargetId::next(), ModTargetId::next());
        let bus = Arc::new(ModBus::new());
        bus.insert(ida, a.clone());
        bus.insert(idb, b.clone());

        let mut table = ModRoutingTable::new();
        // Same depth + range → same offset iff the source was sampled once.
        table.set_edges(
            [
                ModEdge::linear(0, ida, LayerKey(1), 1.0, -1.0, 1.0),
                ModEdge::linear(0, idb, LayerKey(1), 1.0, -1.0, 1.0),
            ],
            1,
        );
        table.commit();

        let mut driver = ModPreFrame::new(table.snapshot_arc());
        driver.set_router(bus.clone());
        driver.set_sources(vec![source(LfoShape::Random)]);

        for i in 0..16 {
            frame(&mut driver, i as f32 / 16.0);
            assert!(
                (a.final_value() - b.final_value()).abs() < 1e-6,
                "both edges must see the same single sample"
            );
        }
    }

    #[test]
    fn hot_swap_clears_the_removed_edges_stale_layer() {
        let cutoff = Arc::new(AtomicTarget::new(1000.0, 0.0, 2000.0));
        let gain = Arc::new(AtomicTarget::new(0.5, 0.0, 1.0));
        let (id_cut, id_gain) = (ModTargetId::next(), ModTargetId::next());
        let bus = Arc::new(ModBus::new());
        bus.insert(id_cut, cutoff.clone());
        bus.insert(id_gain, gain.clone());

        let mut table = ModRoutingTable::new();
        table.set_edges(
            [
                ModEdge::linear(0, id_cut, LayerKey(1), 1.0, 0.0, 2000.0),
                ModEdge::linear(1, id_gain, LayerKey(1), 0.5, 0.0, 1.0),
            ],
            2,
        );
        table.commit();

        let mut driver = ModPreFrame::new(table.snapshot_arc());
        driver.set_router(bus.clone());
        driver.set_sources(vec![
            source(LfoShape::Sine),     // source 0 → cutoff
            source(LfoShape::Triangle), // source 1 → gain
        ]);

        // Frame at sine peak (phase 0.25) so gain is moved off its base.
        frame(&mut driver, 0.25);
        // Triangle at 0.25 is at its peak too → gain moved.
        assert!(
            (gain.final_value() - 0.5).abs() > 1e-6,
            "gain should be modulated"
        );

        // Hot-swap: drop the gain edge, keep cutoff.
        table.set_edges(
            [ModEdge::linear(0, id_cut, LayerKey(1), 1.0, 0.0, 2000.0)],
            2,
        );
        table.commit();
        frame(&mut driver, 0.5);
        assert!(
            (gain.final_value() - 0.5).abs() < 1e-6,
            "gain must fall back to its base after its edge was removed"
        );
    }

    #[test]
    fn sources_run_at_independent_frequencies() {
        // The whole point of SourceRate: two free-running sources at different
        // Hz reach different phases from the SAME clock, so their values diverge.
        let fast = Arc::new(AtomicTarget::new(0.0, -1.0, 1.0));
        let slow = Arc::new(AtomicTarget::new(0.0, -1.0, 1.0));
        let (id_fast, id_slow) = (ModTargetId::next(), ModTargetId::next());
        let bus = Arc::new(ModBus::new());
        bus.insert(id_fast, fast.clone());
        bus.insert(id_slow, slow.clone());

        let mut table = ModRoutingTable::new();
        table.set_edges(
            [
                ModEdge::linear(0, id_fast, LayerKey(1), 1.0, -1.0, 1.0),
                ModEdge::linear(1, id_slow, LayerKey(1), 1.0, -1.0, 1.0),
            ],
            2,
        );
        table.commit();

        let mut driver = ModPreFrame::new(table.snapshot_arc());
        driver.set_router(bus.clone());
        // Same sine shape, DIFFERENT rates: 4 Hz vs 1 Hz, both free-running.
        driver.set_sources(vec![
            Box::new(Sourced::new(
                Lfo::new(LfoShape::Sine),
                SourceRate::free_running(Hz(4.0), 0.0),
            )),
            Box::new(Sourced::new(
                Lfo::new(LfoShape::Sine),
                SourceRate::free_running(Hz(1.0), 0.0),
            )),
        ]);

        // Advance a quarter second in 60fps-ish steps; the 4 Hz source has swept
        // a full cycle while the 1 Hz source has covered only a quarter — so at
        // least one frame must show clearly different values.
        let mut saw_divergence = false;
        for _ in 0..15 {
            driver.run(Beat(0.0), Seconds(1.0 / 60.0));
            if (fast.final_value() - slow.final_value()).abs() > 0.1 {
                saw_divergence = true;
            }
        }
        assert!(
            saw_divergence,
            "two sources at different Hz must reach different phases"
        );
    }

    #[test]
    fn beat_synced_source_locks_to_the_beat() {
        // A beat-synced source ignores dt and derives phase from beat/frequency:
        // at 1 cycle/beat, phase == beat fraction. Probe the sine's quarter
        // points: beat 0.25 (phase 0.25 → +peak) vs 0.75 (phase 0.75 → -peak)
        // are opposite; beat 0.25 and 1.25 (one full cycle apart) are equal.
        let t = Arc::new(AtomicTarget::new(0.0, -1.0, 1.0));
        let id = ModTargetId::next();
        let bus = Arc::new(ModBus::new());
        bus.insert(id, t.clone());

        let mut table = ModRoutingTable::new();
        table.set_edges([ModEdge::linear(0, id, LayerKey(1), 1.0, -1.0, 1.0)], 1);
        table.commit();

        let mut driver = ModPreFrame::new(table.snapshot_arc());
        driver.set_router(bus.clone());
        driver.set_sources(vec![Box::new(Sourced::new(
            Lfo::new(LfoShape::Sine),
            SourceRate::beat_synced(Hz(1.0), 0.0),
        ))]);

        driver.run(Beat(0.25), Seconds(0.0));
        let at_quarter = t.final_value();
        driver.run(Beat(0.75), Seconds(0.0));
        let at_three_quarter = t.final_value();
        driver.run(Beat(1.25), Seconds(0.0));
        let at_quarter_next_cycle = t.final_value();

        assert!(
            (at_quarter - at_quarter_next_cycle).abs() < 1e-4,
            "beat 0.25 and 1.25 are one cycle apart → same phase"
        );
        assert!(
            (at_quarter - at_three_quarter).abs() > 0.5,
            "beat 0.25 (+peak) and 0.75 (-peak) are opposite phases"
        );
    }
}
