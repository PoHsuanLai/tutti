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
#[test]
fn a_macro_is_its_value() {
    assert_eq!(Source::from(Macro(-0.4)), Source::Macro { value: -0.4 });
}

/// A follower defaults to 5 ms / 100 ms, `Default` agrees with `new`, and
/// each builder sets only its own field.
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
