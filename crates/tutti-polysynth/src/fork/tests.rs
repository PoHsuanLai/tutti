use tutti_fixtures::midi;
use tutti_graph::contract::{Direct, assert_param_fork};
use tutti_graph::{Event, Offset, ParamFork};
use tutti_types::{SampleRate, Seconds, UnitParam};
use tutti_ump::{MidiChannel, MidiGroup, Ump};

use crate::{Adsr, OscillatorType, PolySynth, SynthConfig, UnisonConfig};

const RATE: SampleRate = SampleRate(48_000.0);

/// A saw with an instant attack, so a note sounds from its own frame.
fn saw() -> PolySynth {
    PolySynth::new(SynthConfig {
        sample_rate: RATE,
        oscillator: OscillatorType::Saw,
        envelope: Adsr {
            attack: Seconds(0.0),
            ..Default::default()
        },
        unison: Some(UnisonConfig::default()),
        ..Default::default()
    })
    .expect("synth builds")
}

fn note_on() -> Ump {
    midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF)
}

/// The peak of channel 0 over `frames` of `synth` with a note on at
/// frame 0, in 64-frame blocks.
fn peak(synth: PolySynth, frames: usize) -> f32 {
    let mut hand = Direct::new(synth, RATE, 64);
    let on = Offset::new(0, tutti_types::Samples(64)).expect("inside");
    hand.events(0, &[Event::midi(on, note_on().0)]);
    let mut peak = 0.0f32;
    for _ in 0..frames / 64 {
        hand.block();
        peak = hand.output(0).iter().fold(peak, |a, s| a.max(s.abs()));
    }
    peak
}

/// The synth's params by address fork as a `ParamNode`'s must: every one
/// in its `ParamSet` (volume, and with unison the detune and spread), a
/// fork starting from the authored value, and no cell shared either way.
///
/// Mutations (run): drop `unison.detach()` from `fork_instance` →
/// "Detune: a live write reached the fork" → fails; `param_set` leaving
/// out `StereoSpread` → the list above is short (and
/// `a_fork_renders_the_volume_and_unison_it_was_taken_with`, which sets
/// it by address, is refused) → fails.
#[test]
fn the_synth_forks_as_a_param_node() {
    let synth = saw();
    let params: Vec<_> = tutti_graph::ParamNode::param_set(&synth).params().collect();
    assert_eq!(
        params,
        [
            UnitParam::Volume,
            UnitParam::Detune,
            UnitParam::StereoSpread
        ]
    );
    assert_param_fork(synth);
}

/// **A fork renders the volume last set through the synth's params**: a
/// volume set before the fork is the fork's, one set after is not, and
/// what a modulation driver left in the live cell is not either.
///
/// Mutation (run): `fork_instance` not detaching `master_volume` → the
/// move after the fork reaches it → fails. Mutation (run): the template
/// taken detached (`ParamFork::new(&synth.fork_instance())`) → the fork
/// renders the volume the synth was built with → fails.
#[test]
fn a_fork_takes_the_authored_volume_at_the_fork() {
    let at = |before: f32, after: f32| {
        let live = saw();
        let source = ParamFork::new(&live);
        let set = source.params().clone();
        set.set(UnitParam::Volume, before);
        // A modulation driver's composite in the live cell: not authored.
        live.set_volume(before * 3.0);
        let fork = source.fork_node();
        set.set(UnitParam::Volume, after);
        peak(fork, 4_096)
    };
    let half = at(0.5, 0.5);
    assert!(half > 0.0, "the note sounds");
    assert_eq!(at(0.5, 1.0), half, "a move after the fork reached it");
    let full = at(1.0, 1.0);
    assert!(
        (full - 2.0 * half).abs() < 1e-5,
        "the volume set before the fork is the fork's: {full} vs 2 × {half}"
    );
}
