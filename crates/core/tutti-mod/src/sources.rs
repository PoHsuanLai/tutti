//! Builders for the built-in [`Source`]s: an [`Lfo`], a [`Macro`], a
//! [`Follower`] and a [`NoteEnvelope`], each converting into the `Source` the
//! compiler reads (`Source::from`).
//!
//! These are the source half only. A follower's and a note envelope's input
//! (the audio or the MIDI that feeds their node) is the host's to wire, keyed
//! by whatever the host keys its nodes by, so it is not here.

use tutti_types::{BeatDuration, Hz, PhaseIncrement, Seconds};

use crate::{Adsr, Clock, LfoShape, Source, Wave};

/// How an [`Lfo`] keeps time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LfoRate {
    /// One cycle per this many beats: bar-locked, follows seeks, holds while
    /// the transport is stopped.
    Beats(BeatDuration),
    /// Cycles per second on the graph's play clock, running while stopped.
    Hz(Hz),
}

/// A low-frequency oscillator: a closed-form wave on a clock, no node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lfo {
    /// The wave.
    pub shape: LfoShape,
    /// The random shapes' seed; ignored by the others.
    pub seed: u64,
    /// Its clock.
    pub rate: LfoRate,
    /// Added to the cycle position.
    pub phase: PhaseIncrement,
}

impl Lfo {
    /// `shape` at one cycle per second.
    pub const fn new(shape: LfoShape) -> Self {
        Self {
            shape,
            seed: 0,
            rate: LfoRate::Hz(Hz(1.0)),
            phase: PhaseIncrement(0.0),
        }
    }

    /// A sine.
    pub const fn sine() -> Self {
        Self::new(LfoShape::Sine)
    }

    /// A triangle.
    pub const fn triangle() -> Self {
        Self::new(LfoShape::Triangle)
    }

    /// A square.
    pub const fn square() -> Self {
        Self::new(LfoShape::Square)
    }

    /// A rising saw.
    pub const fn saw() -> Self {
        Self::new(LfoShape::Sawtooth)
    }

    /// One cycle per `per_cycle` beats.
    pub const fn rate(mut self, per_cycle: BeatDuration) -> Self {
        self.rate = LfoRate::Beats(per_cycle);
        self
    }

    /// `rate` cycles per second.
    pub const fn hz(mut self, rate: Hz) -> Self {
        self.rate = LfoRate::Hz(rate);
        self
    }

    /// Starting `phase` cycles in.
    pub const fn phase(mut self, phase: PhaseIncrement) -> Self {
        self.phase = phase;
        self
    }

    /// The random shapes' seed.
    pub const fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }
}

impl From<Lfo> for Source {
    /// A [`Source::Lfo`]: the shape and seed as the wave, the rate and phase
    /// as the clock, with no anchor (the compiler's to set).
    fn from(lfo: Lfo) -> Self {
        let clock = match lfo.rate {
            LfoRate::Beats(per_cycle) => Clock::Beats {
                per_cycle,
                phase: lfo.phase,
            },
            LfoRate::Hz(rate) => Clock::Seconds {
                rate,
                phase: lfo.phase,
                anchor: None,
            },
        };
        Source::Lfo {
            wave: Wave::Shape {
                shape: lfo.shape,
                seed: lfo.seed,
            },
            clock,
        }
    }
}

/// A constant the user moves: a raw value, saturating past ±1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Macro(pub f32);

impl From<Macro> for Source {
    /// A [`Source::Macro`] at the value.
    fn from(m: Macro) -> Self {
        Source::Macro { value: m.0 }
    }
}

/// An envelope follower's times: the source half of a follower, whose input
/// the host wires.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Follower {
    /// Rise time constant.
    pub attack: Seconds,
    /// Fall time constant.
    pub release: Seconds,
}

impl Follower {
    /// A 5 ms attack and a 100 ms release.
    pub const fn new() -> Self {
        Self {
            attack: Seconds(0.005),
            release: Seconds(0.1),
        }
    }

    /// Rises with `attack`.
    pub const fn attack(mut self, attack: Seconds) -> Self {
        self.attack = attack;
        self
    }

    /// Falls with `release`.
    pub const fn release(mut self, release: Seconds) -> Self {
        self.release = release;
        self
    }
}

impl Default for Follower {
    /// [`Follower::new`]: a 5 ms attack and a 100 ms release.
    fn default() -> Self {
        Self::new()
    }
}

impl From<Follower> for Source {
    /// A [`Source::Follower`] with the times.
    fn from(f: Follower) -> Self {
        Source::Follower {
            attack: f.attack,
            release: f.release,
        }
    }
}

/// An ADSR per note: the source half of a note envelope, whose MIDI the host
/// wires.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteEnvelope {
    /// The stages.
    pub adsr: Adsr,
}

impl NoteEnvelope {
    /// An envelope of `adsr`.
    pub const fn new(adsr: Adsr) -> Self {
        Self { adsr }
    }
}

impl From<NoteEnvelope> for Source {
    /// A [`Source::NoteEnvelope`] of the stages.
    fn from(e: NoteEnvelope) -> Self {
        Source::NoteEnvelope { adsr: e.adsr }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RateAnchor;
    use tutti_types::Amplitude;

    /// The fields of an [`Source::Lfo`]: `(shape, seed, clock)`.
    fn lfo_parts(source: Source) -> (LfoShape, u64, Clock) {
        match source {
            Source::Lfo {
                wave: Wave::Shape { shape, seed },
                clock,
            } => (shape, seed, clock),
            other => panic!("not a shaped LFO: {other:?}"),
        }
    }

    /// Each named constructor's shape, at the defaults: seed 0, one cycle a
    /// second, phase 0, no anchor.
    ///
    /// Mutations (each run, each failed): `triangle` building a sine (its row);
    /// `new` defaulting to 2 Hz (the clock assert).
    #[test]
    fn each_lfo_constructor_is_its_shape_at_one_hertz() {
        let rows = [
            (Lfo::sine(), LfoShape::Sine),
            (Lfo::triangle(), LfoShape::Triangle),
            (Lfo::square(), LfoShape::Square),
            (Lfo::saw(), LfoShape::Sawtooth),
            (Lfo::new(LfoShape::RandomSmooth), LfoShape::RandomSmooth),
        ];
        for (lfo, want) in rows {
            let (shape, seed, clock) = lfo_parts(lfo.into());
            assert_eq!(shape, want);
            assert_eq!(seed, 0);
            assert_eq!(
                clock,
                Clock::Seconds {
                    rate: Hz(1.0),
                    phase: PhaseIncrement(0.0),
                    anchor: None,
                },
                "{want:?}"
            );
        }
    }

    /// `rate` is a beat clock with the phase; `hz` is a seconds clock with
    /// the phase and no anchor; `seed` reaches the wave. The last of `rate`
    /// and `hz` wins.
    ///
    /// Mutations (each run, each failed): the beat clock dropping the phase
    /// (the beats assert); `hz` setting a beat rate (the seconds assert);
    /// `seed` ignored (the seed assert).
    #[test]
    fn an_lfos_clock_and_seed_are_pinned_field_by_field() {
        let beats = Lfo::saw()
            .hz(Hz(4.0))
            .rate(BeatDuration(0.5))
            .phase(PhaseIncrement(0.25))
            .seed(7);
        let (shape, seed, clock) = lfo_parts(beats.into());
        assert_eq!(shape, LfoShape::Sawtooth);
        assert_eq!(seed, 7);
        assert_eq!(
            clock,
            Clock::Beats {
                per_cycle: BeatDuration(0.5),
                phase: PhaseIncrement(0.25),
            }
        );

        let seconds = Lfo::sine()
            .rate(BeatDuration(2.0))
            .hz(Hz(3.0))
            .phase(PhaseIncrement(0.5));
        let (_, _, clock) = lfo_parts(seconds.into());
        assert_eq!(
            clock,
            Clock::Seconds {
                rate: Hz(3.0),
                phase: PhaseIncrement(0.5),
                anchor: None::<RateAnchor>,
            }
        );
    }

    /// A macro is its value.
    ///
    /// Mutation: `From<Macro>` negating the value (failed).
    #[test]
    fn a_macro_is_its_value() {
        assert_eq!(Source::from(Macro(-0.4)), Source::Macro { value: -0.4 });
    }

    /// A follower defaults to 5 ms / 100 ms, `Default` agrees with `new`, and
    /// each builder sets only its own field.
    ///
    /// Mutations (each run, each failed): `attack` writing the release (the
    /// built assert); a 10 ms default attack (the default assert).
    #[test]
    fn a_followers_times_are_pinned_field_by_field() {
        assert_eq!(
            Source::from(Follower::default()),
            Source::Follower {
                attack: Seconds(0.005),
                release: Seconds(0.1),
            }
        );
        assert_eq!(Follower::default(), Follower::new());
        assert_eq!(
            Source::from(Follower::new().attack(Seconds(0.02)).release(Seconds(0.3))),
            Source::Follower {
                attack: Seconds(0.02),
                release: Seconds(0.3),
            }
        );
    }

    /// A note envelope carries its stages unchanged.
    ///
    /// Mutation: `From<NoteEnvelope>` swapping attack and release (failed).
    #[test]
    fn a_note_envelope_carries_its_stages() {
        let adsr = Adsr {
            attack: Seconds(0.01),
            decay: Seconds(0.2),
            sustain: Amplitude(0.6),
            release: Seconds(0.5),
        };
        assert_eq!(
            Source::from(NoteEnvelope::new(adsr)),
            Source::NoteEnvelope { adsr }
        );
    }
}
