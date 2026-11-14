use crate::UnisonVoices;
use crate::{PolySynth, SynthConfig};

/// **The ports and their bases.** A synth declares its volume, and with
/// a unison engine its detune and spread too ,
/// each port's base its cell.
#[test]
fn the_shape_declares_volume_and_the_unison_params() {
    use tutti_graph::Node;
    use tutti_types::UnitParam;

    let mut plain = PolySynth::new(SynthConfig::default());
    plain.set_volume(tutti_types::Amplitude(0.5));
    assert_eq!(plain.shape().params.as_slice(), &[UnitParam::Volume][..]);
    assert_eq!(plain.param_base(0), Some(0.5));
    assert_eq!(plain.param_base(1), None);

    let unison = PolySynth::new(SynthConfig {
        unison: Some(crate::UnisonConfig {
            voice_count: UnisonVoices::of(3),
            detune_cents: tutti_types::CentsSpan::of(12.0),
            stereo_spread: tutti_types::Spread::new_clamped(0.25),
            phase_randomize: false,
        }),
        ..SynthConfig::default()
    });
    assert_eq!(
        unison.shape().params.as_slice(),
        &[
            UnitParam::Volume,
            UnitParam::Detune,
            UnitParam::StereoSpread
        ][..]
    );
    assert_eq!(unison.param_base(1), Some(12.0));
    assert_eq!(unison.param_base(2), Some(0.25));
}

/// **The tail follows the rate the graph prepares the synth at**, not a
/// rate the config named: a 0.5 s release is 24 000 frames at 48 kHz and
/// 48 000 at 96 kHz.
#[test]
fn the_release_tail_is_in_frames_at_the_prepared_rate() {
    use tutti_graph::{Node, Prepare};
    use tutti_types::{MaxBlock, SampleRate, Samples, Tail};

    let mut synth = PolySynth::new(SynthConfig {
        envelope: tutti_types::Adsr {
            release: tutti_types::Seconds(0.5),
            ..Default::default()
        },
        ..Default::default()
    });
    for (rate, frames) in [(48_000.0, 24_000), (96_000.0, 48_000)] {
        synth.prepare(&Prepare::new(
            SampleRate::of(rate),
            MaxBlock::of(Samples(64)),
        ));
        assert_eq!(
            synth.shape().tail,
            Tail::Finite(Samples(frames)),
            "at {rate} Hz"
        );
    }
}
