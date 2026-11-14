use super::*;
use tutti_types::Semitones;

const BASE: Hz = Hz(1000.0);

fn voice(lfo_rate: Option<Hz>) -> SynthVoice {
    let config = SynthConfig {
        filter: FilterType::Moog {
            cutoff: BASE,
            resonance: Resonance::NONE,
        },
        filter_mod: FilterModConfig {
            lfo_depth: Depth::FULL,
            lfo_rate,
            ..FilterModConfig::default()
        },
        ..SynthConfig::default()
    };
    SynthVoice::from_config(&config, SampleRate::SR_48K, UnisonVoices::ONE)
}

/// **A filter LFO with no rate does not move the cutoff**, whatever its depth:
/// the rate is what makes the sweep exist.
#[test]
fn an_lfo_with_no_rate_leaves_the_cutoff_at_its_base() {
    let mut voice = voice(None);
    let channel = ChannelState::new(Semitones(2.0));
    for _ in 0..4 {
        assert_eq!(
            voice.modulated_cutoff(2400, SampleRate::SR_48K, &channel),
            BASE
        );
    }
}

/// **The LFO sweeps the cutoff by `1 + sin(phase) * depth / 2`**: after a
/// quarter period of a 5 Hz LFO (2400 frames at 48 kHz) the sine is at its
/// crest, so a full depth lifts the cutoff by 50%.
#[test]
fn an_lfo_sweeps_the_cutoff_by_half_its_depth_at_the_crest() {
    let mut voice = voice(Some(Hz(5.0)));
    let channel = ChannelState::new(Semitones(2.0));
    let cutoff = voice.modulated_cutoff(2400, SampleRate::SR_48K, &channel);
    assert!(
        (cutoff.get() - 1.5 * BASE.get()).abs() < 0.5,
        "the crest cutoff is {cutoff:?}, expected 1500 Hz"
    );
}
