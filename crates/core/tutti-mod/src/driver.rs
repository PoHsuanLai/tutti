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
//!   contains — the "continuous-value tax". That set is double-buffered and
//!   sorted, so the sweep is a binary search over a reused allocation rather
//!   than a linear scan over a fresh one: a mod matrix grows as sources ×
//!   targets, which is exactly the shape that punishes a quadratic sweep.

use std::sync::Arc;

use tutti_types::RtPublish;
use tutti_types::{Beat, Hz, Param, Phase, PhaseIncrement, Seconds};

use crate::id::{LayerKey, ModTargetId};
use crate::router::ModRouter;
use crate::routing::ModRoutingSnapshot;
use crate::shape::shape;
use crate::Modulator;

/// Where a source's frequency comes from: a constant, or a live cell another
/// modulator writes.
///
/// The [`Modulated`](Rate::Modulated) arm is what makes modulation *cascade* —
/// an LFO whose rate is itself modulated. It holds the same [`Param<Hz>`] an
/// [`AtomicTarget`](crate::AtomicTarget) mirrors into (via
/// [`Param::as_atomic`]), so the driver writes it as a target on one source and
/// reads it as a rate on another, with no extra wiring.
///
/// The two must be *the same* cell. A `Param<Hz>` minted separately from the one
/// handed to the target compiles, runs, and modulates nothing — so build one and
/// clone the handle, never construct twice.
///
/// A cascaded rate lags its driver by at most one frame: the driver samples
/// every source in a single pass, so a source read this frame may see the value
/// its modulator wrote last frame. That is what makes a cycle (A's rate driven
/// by B, B's by A) terminate rather than recurse — a one-frame-delayed feedback
/// loop, which is a legitimate modulation technique, not an error to reject.
#[derive(Debug, Clone)]
pub enum Rate {
    /// A constant frequency, fixed when the source is built.
    Fixed(Hz),
    /// A frequency read fresh each frame from a shared cell.
    Modulated(Param<Hz>),
}

impl Rate {
    /// This frame's frequency. One `Acquire` load in the modulated arm — read
    /// once per frame by `tick_phase`, never per sample.
    #[inline]
    pub fn hz(&self) -> Hz {
        match self {
            Rate::Fixed(hz) => *hz,
            Rate::Modulated(param) => param.load(),
        }
    }
}

impl From<Hz> for Rate {
    fn from(hz: Hz) -> Self {
        Rate::Fixed(hz)
    }
}

impl From<Param<Hz>> for Rate {
    fn from(param: Param<Hz>) -> Self {
        Rate::Modulated(param)
    }
}

/// A source's own rate — how its phase is generated from the transport, so each
/// LFO runs at its own frequency (unlike one shared phase for all sources).
///
/// The values are the transport's own vocabulary (`Beat`/`Seconds` reach the
/// driver via [`ModPreFrame::run`]); the driver does not depend on the transport
/// itself, only on the two scalars a caller reads off it.
///
/// Not `Copy`: [`Rate::Modulated`] holds a shared cell, and a `Copy` rate would
/// invite building one per frame instead of cloning the handle to the one the
/// target already writes.
#[derive(Debug, Clone)]
pub struct SourceRate {
    /// `beat_synced`: cycles per beat. Free-running: cycles per second (`Hz`).
    pub frequency: Rate,
    /// Constant shift applied after phase generation.
    ///
    /// A [`PhaseIncrement`] rather than a [`Phase`] despite the name: it is a
    /// *displacement* added to a generated position, not a position itself, and
    /// it is meaningfully negative — which a `Phase` cannot be.
    ///
    /// Deliberately not a [`Rate`]: modulating the offset of a source whose
    /// phase already advances is a second, independent capability, and rate
    /// covers the motivating case. Add it when something needs it.
    pub phase_offset: PhaseIncrement,
    /// `true`: phase is derived from `beat / frequency` (locks to transport);

    /// `false`: integrate `frequency * dt` into a free-running accumulator.
    pub beat_synced: bool,
}

impl SourceRate {
    /// A source locked to the transport at `frequency` cycles per beat.
    ///
    /// Takes anything that becomes a [`Rate`] — an [`Hz`] for a constant, a
    /// [`Param<Hz>`] for a modulated one.
    pub fn beat_synced(
        frequency: impl Into<Rate>,
        phase_offset: impl Into<PhaseIncrement>,
    ) -> Self {
        Self {
            frequency: frequency.into(),
            phase_offset: phase_offset.into(),
            beat_synced: true,
        }
    }

    /// A free-running source at `frequency` Hz (cycles per second).
    ///
    /// Takes anything that becomes a [`Rate`] — see
    /// [`beat_synced`](Self::beat_synced).
    pub fn free_running(
        frequency: impl Into<Rate>,
        phase_offset: impl Into<PhaseIncrement>,
    ) -> Self {
        Self {
            frequency: frequency.into(),
            phase_offset: phase_offset.into(),
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
    /// Derive this source's [`Phase`] from `(beat, dt)` via its own rate,
    /// advance state + phase, return the raw `[-1, 1]` value.
    fn sample(&mut self, beat: Beat, dt: Seconds) -> f32;
}

/// Pairs a [`Modulator`] with its threaded state, its [`SourceRate`], and its
/// free-running phase accumulator, erasing the associated type.
pub struct Sourced<M: Modulator> {
    modulator: M,
    state: M::State,
    rate: SourceRate,
    /// Free-running accumulated phase (unused when `rate.beat_synced`).
    phase: Phase,
}

impl<M: Modulator> Sourced<M> {
    /// A source with an explicit [`SourceRate`].
    pub fn new(modulator: M, rate: SourceRate) -> Self {
        Self {
            modulator,
            state: M::State::default(),
            rate,
            phase: Phase::START,
        }
    }

    /// This frame's [`Phase`], advancing the free-running accumulator.
    #[inline]
    fn tick_phase(&mut self, beat: Beat, dt: Seconds) -> Phase {
        // Read once per frame. A modulated rate is an `Acquire` load behind
        // this call; per-sample reads are what the once-per-frame driver exists
        // to avoid.
        let freq = self.rate.frequency.hz().get();
        let base = if self.rate.beat_synced {
            if freq.abs() < f32::EPSILON {
                Phase::START
            } else {
                // A beat-synced source reads its position off the transport, so
                // it re-derives rather than accumulating — seeking the transport
                // lands the modulator where the new beat says, not where a
                // running sum would have carried it.
                Phase::wrapped((beat.get() as f32) / freq)
            }
        } else {
            self.phase = self.phase.advance(PhaseIncrement(freq * dt.get()));
            self.phase
        };
        base.offset_by(self.rate.phase_offset)
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
    ///
    /// Double-buffered with [`active`](Self::active) and swapped each frame, so
    /// steady-state running reuses both allocations instead of building a fresh
    /// `Vec` per frame. Kept sorted, which is what lets the stale-layer sweep
    /// binary-search rather than scan.
    prev_active: Vec<(ModTargetId, LayerKey)>,
    /// This frame's layers. Swapped into `prev_active` at the end of `run`; held
    /// as a field purely to keep its capacity across frames.
    active: Vec<(ModTargetId, LayerKey)>,
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
            active: Vec::new(),
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
        // `active` is a reused buffer, not a fresh allocation: after the first
        // few frames its capacity already covers the edge count, so steady-state
        // running does not touch the allocator.
        let mut active = std::mem::take(&mut self.active);
        active.clear();
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
        //
        // Sorted so the membership test below is a binary search. A linear
        // `contains` here is quadratic in the edge count, which a mod matrix is
        // exactly the shape to grow: every source × every target it drives.
        active.sort_unstable();
        for (target, key) in self.prev_active.iter().copied() {
            if active.binary_search(&(target, key)).is_err() {
                router.clear(target, key);
            }
        }

        // Swap rather than assign: `prev_active`'s allocation becomes next
        // frame's `active` buffer instead of being dropped.
        std::mem::swap(&mut self.prev_active, &mut active);
        self.active = active;
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

    /// The stale-layer sweep binary-searches `active`, which is only correct if
    /// `active` is sorted. Two edges cannot tell a working comparison from a
    /// broken one — with `ModTargetId::next()` handing out ascending ids, a
    /// small set is already in order and an unsorted search would accidentally
    /// agree.
    ///
    /// So this drives enough targets that insertion order and sorted order
    /// genuinely differ: edges are declared with their target ids interleaved,
    /// then a hot-swap retires every *odd*-indexed one. Each survivor must keep
    /// its modulation and each retiree must fall back to base — a mis-ordered
    /// search would clear live layers, strand dead ones, or both.
    #[test]
    fn stale_sweep_is_correct_when_many_layers_retire_at_once() {
        const N: usize = 16;

        let bus = Arc::new(ModBus::new());
        let targets: Vec<_> = (0..N)
            .map(|_| {
                let t = Arc::new(AtomicTarget::new(1000.0, 0.0, 2000.0));
                let id = ModTargetId::next();
                bus.insert(id, t.clone());
                (id, t)
            })
            .collect();

        // `active` is built by iterating SOURCES in index order, so shuffling the
        // edge list would achieve nothing — `edges_for_source` regroups it. The
        // ordering has to be broken where it is actually observed: source `i`
        // drives target `N-1-i`, so ascending source index yields *descending*
        // target ids and `active` comes out reverse-sorted.
        let target_of = |i: usize| targets[N - 1 - i].0;

        let all_edges: Vec<_> = (0..N)
            .map(|i| ModEdge::linear(i, target_of(i), LayerKey(1), 1.0, 0.0, 2000.0))
            .collect();

        let mut table = ModRoutingTable::new();
        table.set_edges(all_edges, N);
        table.commit();

        let mut driver = ModPreFrame::new(table.snapshot_arc());
        driver.set_router(bus.clone());
        driver.set_sources((0..N).map(|_| source(LfoShape::Sine)).collect());

        frame(&mut driver, 0.25);
        for (i, (_, t)) in targets.iter().enumerate() {
            assert!(
                (t.final_value() - 1000.0).abs() > 1e-6,
                "target {i} should be modulated on the first frame"
            );
        }

        // Retire every odd SOURCE's edge, keep every even one.
        let survivors: Vec<_> = (0..N)
            .filter(|i| i % 2 == 0)
            .map(|i| ModEdge::linear(i, target_of(i), LayerKey(1), 1.0, 0.0, 2000.0))
            .collect();
        table.set_edges(survivors, N);
        table.commit();
        frame(&mut driver, 0.5);

        for i in 0..N {
            // Target `N-1-i` is the one source `i` drives.
            let t = &targets[N - 1 - i].1;
            if i % 2 == 0 {
                assert!(
                    (t.final_value() - 1000.0).abs() > 1e-6,
                    "source {i}'s target kept its edge and must still be modulated"
                );
            } else {
                assert!(
                    (t.final_value() - 1000.0).abs() < 1e-6,
                    "source {i}'s target lost its edge and must fall back to base"
                );
            }
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

    /// A `Rate::Modulated` cell is read every frame, not captured at build
    /// time. Without this the whole cascade is inert: `Sourced` would hold the
    /// frequency the cell happened to contain when the source was constructed.
    ///
    /// Driven by hand rather than through a second LFO, so a failure means the
    /// *read* is broken and not the routing that would feed it.
    #[test]
    fn a_modulated_rate_is_reread_every_frame() {
        let rate: Param<Hz> = Param::new(Hz(1.0));
        let mut fast = Sourced::new(
            Lfo::new(LfoShape::Sine),
            SourceRate::free_running(rate.clone(), 0.0),
        );

        // At 1 Hz with a 0.25 s step, phase advances a quarter cycle per frame.
        let slow_step = fast.tick_phase(Beat(0.0), Seconds(0.25));
        assert!(
            (slow_step - 0.25).abs() < 1e-5,
            "1 Hz over 0.25 s is a quarter cycle, got {slow_step}"
        );

        // Quadruple the rate through the shared cell; the same dt must now
        // advance a full cycle, landing back where it started.
        rate.store(Hz(4.0));
        let fast_step = fast.tick_phase(Beat(0.0), Seconds(0.25));
        assert!(
            (fast_step - 0.25).abs() < 1e-5,
            "4 Hz over 0.25 s is a full cycle back to 0.25, got {fast_step}"
        );

        // And a fixed rate must be unaffected by any of this.
        let mut fixed = Sourced::new(
            Lfo::new(LfoShape::Sine),
            SourceRate::free_running(Hz(1.0), 0.0),
        );
        let a = fixed.tick_phase(Beat(0.0), Seconds(0.25));
        rate.store(Hz(64.0));
        let b = fixed.tick_phase(Beat(0.0), Seconds(0.25));
        assert!(
            (a - 0.25).abs() < 1e-5 && (b - 0.5).abs() < 1e-5,
            "a fixed rate ignores the cell entirely, got {a} then {b}"
        );
    }

    /// Modulation cascades: one LFO drives the *rate* of another, through the
    /// ordinary target/edge machinery and no special case.
    ///
    /// The cascade is wired by sharing one cell — the `AtomicTarget` mirrors
    /// into the same `Param<Hz>` the second source reads as its rate. That
    /// sharing is the load-bearing part and the thing that fails silently if
    /// got wrong, so the test asserts the *carrier* moves and then that the
    /// downstream source's phase advance actually responds to it.
    #[test]
    fn one_source_can_modulate_anothers_rate() {
        // The shared cell: a rate, and a modulation target writing into it.
        let rate: Param<Hz> = Param::new(Hz(2.0));
        let rate_target = Arc::new(AtomicTarget::with_mirror(
            2.0,
            2.0,
            10.0,
            rate.as_atomic(), // <- the same cell the source below reads
        ));

        let id_rate = ModTargetId::next();
        let bus = Arc::new(ModBus::new());
        bus.insert(id_rate, rate_target.clone());

        // Source 0 is a plain LFO; its only job is to drive source 1's rate.
        let mut table = ModRoutingTable::new();
        table.set_edges([ModEdge::linear(0, id_rate, LayerKey(1), 1.0, 2.0, 10.0)], 2);
        table.commit();

        let mut driver = ModPreFrame::new(table.snapshot_arc());
        driver.set_router(bus.clone());
        driver.set_sources(vec![
            source(LfoShape::Sine),
            // Source 1's rate IS the cell source 0 writes.
            Box::new(Sourced::new(
                Lfo::new(LfoShape::Sine),
                SourceRate::free_running(rate.clone(), 0.0),
            )),
        ]);

        // Drive to the sine's positive peak: the carrier must leave its base.
        driver.run(Beat(0.25), Seconds(0.0));
        let driven = rate.load();
        assert!(
            driven.get() > 2.0,
            "the modulated rate should have been driven above its base, got {driven:?}"
        );

        // And the downstream source must actually run at the driven rate. Two
        // otherwise identical free-running sources, one at the base rate and
        // one reading the cell, must diverge — which can only happen if the
        // cascade reached the phase advance.
        let mut cascaded = Sourced::new(
            Lfo::new(LfoShape::Sine),
            SourceRate::free_running(rate.clone(), 0.0),
        );
        let mut baseline = Sourced::new(
            Lfo::new(LfoShape::Sine),
            SourceRate::free_running(Hz(2.0), 0.0),
        );
        let cascaded_phase = cascaded.tick_phase(Beat(0.0), Seconds(0.1));
        let baseline_phase = baseline.tick_phase(Beat(0.0), Seconds(0.1));
        assert!(
            (cascaded_phase - baseline_phase).abs() > 1e-4,
            "a driven rate must advance phase differently than the base rate: \
             {cascaded_phase} vs {baseline_phase}"
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
