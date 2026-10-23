//! The pure modulators the audio-rate nodes run: the [`Modulator`] trait,
//! implemented here for `tutti-mod`'s [`Wave`].
//!
//! A modulator is a pure function of its clock position, in unwrapped cycles:
//! the cycle index addresses a stepped shape's step, the fraction is the
//! phase. That makes a random wave seekable and fork-safe (the value at a
//! position is the same however the playhead got there), and it is why the
//! LFO is [`Wave`] itself rather than a copy here: the node and the control
//! lane evaluate one function ([`Wave::raw_at`]), so a random LFO is one
//! sequence whichever tier runs it.

use tutti_core::PhaseIncrement;
use tutti_mod::Wave;

/// A pure `cycles -> value` function: the position, in unwrapped cycles, is
/// all it reads. It holds no state, so it is `Sync`, and a node that runs it
/// replays the same values after a seek or in a fork.
pub trait Modulator {
    /// The value at `cycles`, an unwrapped clock position: the integer part
    /// is the cycle index, the fraction the phase. Typically in `[-1, 1]`.
    ///
    /// A negative position is valid: the phase is the position's
    /// `floor`-based fraction, so a modulator running backwards reads the end
    /// of the cycle, not off the front of it.
    fn value(&self, cycles: f64) -> f32;

    /// Fills `out` starting at `cycles`, advancing by `dphase` per sample, and
    /// returns the position after the last sample. Does not allocate. The
    /// default loops [`Modulator::value`].
    fn fill(&self, cycles: f64, dphase: PhaseIncrement, out: &mut [f32]) -> f64 {
        let step = f64::from(dphase.get());
        let mut c = cycles;
        for s in out.iter_mut() {
            *s = self.value(c);
            c += step;
        }
        c
    }
}

/// The wave the control lane evaluates, at audio rate: a shape (the random
/// ones addressed by cycle index and seed), a drawn period, or a constant.
impl Modulator for Wave {
    #[inline]
    fn value(&self, cycles: f64) -> f32 {
        self.raw_at(cycles, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::Phase;
    use tutti_mod::LfoShape;

    fn shape(shape: LfoShape) -> Wave {
        Wave::Shape { shape, seed: 0 }
    }

    /// The deterministic shapes reach `Modulator::value` unaltered, at the
    /// position's phase.
    ///
    /// The waveform maths itself is `tutti-mod`'s (`LfoShape::evaluate_periodic`,
    /// asserted there over every shape). What this pins is that the position's
    /// fraction is the phase, in any cycle.
    #[test]
    fn deterministic_shapes_read_the_positions_phase() {
        for (s, phase, expected) in [
            (LfoShape::Sine, 0.25, 1.0),
            (LfoShape::Square, 0.1, 1.0),
            (LfoShape::Square, 0.9, -1.0),
            (LfoShape::Sawtooth, 0.5, 0.0),
        ] {
            for cycle in [0.0, 3.0, 1_000.0] {
                let got = shape(s).value(cycle + phase);
                assert!(
                    (got - expected).abs() < 0.01,
                    "{s:?} at {}: expected {expected}, got {got}",
                    cycle + phase
                );
            }
        }
    }

    /// A negative `dphase` runs the modulator backwards: each sample reads
    /// the shape at its wrapped phase, the end of the cycle once the position
    /// passes zero (a `fract` wrap keeps the sign and reads off the front).
    ///
    /// Mutation (run): `Wave::raw_at`'s phase taken as `cycles.fract()`
    /// instead of `cycles - cycles.floor()` → the second sample reads phase
    /// -0.15 → fails.
    #[test]
    fn fill_runs_backwards_through_the_cycles_end() {
        let saw = shape(LfoShape::Sawtooth);
        let mut out = [0.0_f32; 16];
        let end = saw.fill(0.1, PhaseIncrement(-0.25), &mut out);
        for (i, v) in out.iter().enumerate() {
            let phase = Phase::wrapped(0.1 - 0.25 * i as f32);
            let want = LfoShape::Sawtooth.evaluate_periodic(phase);
            assert!((v - want).abs() < 1e-5, "sample {i}: {v} vs {want}");
        }
        assert!((end - (0.1 - 0.25 * 16.0)).abs() < 1e-9);
    }

    #[test]
    fn fill_still_wraps_forwards() {
        let saw = shape(LfoShape::Sawtooth);
        let mut out = [0.0_f32; 8];
        saw.fill(0.9, PhaseIncrement(0.25), &mut out);
        assert!((out[0] - LfoShape::Sawtooth.evaluate_periodic(Phase(0.9))).abs() < 1e-5);
        assert!((out[1] - LfoShape::Sawtooth.evaluate_periodic(Phase(0.15))).abs() < 1e-5);
        assert!(out.iter().all(|v| (-1.0..=1.0).contains(v)));
    }
}
