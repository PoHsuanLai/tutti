//! The reference SoundFont (`assets/soundfonts/TimGM6mb.sf2`) read as data.

use std::fs::File;

use tutti_fixtures::asset;
use tutti_sf2::{GeneratorType, SoundFont};
use tutti_ump::{NoteNumber, U7};

fn timgm6mb() -> SoundFont {
    SoundFont::new(&mut File::open(asset("soundfonts/TimGM6mb.sf2")).unwrap()).unwrap()
}

#[test]
fn the_file_has_presets_instruments_and_samples() {
    let soundfont = timgm6mb();
    assert!(soundfont.presets().len() > 100, "a General MIDI set");
    assert!(!soundfont.instruments().is_empty());
    assert!(!soundfont.sample_headers().is_empty());
    assert_eq!(soundfont.bits_per_sample(), 16);
    assert!(!soundfont.wave_data().is_empty());
    assert!(
        soundfont
            .presets()
            .iter()
            .any(|p| (p.bank_number(), p.patch_number()) == (0, 0)),
        "the acoustic grand piano"
    );
}

// Mutation (run): returning `low..=low` from `generator_range` makes the
// region's key range a single key, and fails the first two assertions.
#[test]
fn a_region_contains_exactly_the_keys_and_velocities_of_its_ranges() {
    let soundfont = timgm6mb();
    let mut checked = 0;
    let mut widest = 0;
    for instrument in soundfont.instruments() {
        for region in instrument.regions() {
            let (keys, velocities) = (region.key_range(), region.velocity_range());
            assert!(keys.start() <= keys.end() && *keys.end() < 128);
            widest = widest.max(keys.end() - keys.start());
            for key in (0..=127).filter_map(NoteNumber::new) {
                for velocity in [1, 64, 127] {
                    let velocity = U7::new(velocity).unwrap();
                    assert_eq!(
                        region.contains(key, velocity),
                        keys.contains(&key.get()) && velocities.contains(&velocity.get()),
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 1000);
    assert!(widest >= 12, "a sample spans an octave of keys, not one");
}

#[test]
fn every_region_names_a_real_sample_inside_the_wave_data() {
    let soundfont = timgm6mb();
    for instrument in soundfont.instruments() {
        for region in instrument.regions() {
            assert!(region.sample_id() < soundfont.sample_headers().len());
            assert!(0 <= region.sample_start());
            assert!(region.sample_start() < region.sample_end());
            assert!((region.sample_end() as usize) < soundfont.wave_data().len());
        }
    }
}

#[test]
fn the_generator_count_is_the_last_generator_plus_one() {
    assert_eq!(GeneratorType::COUNT, GeneratorType::UnusedEnd as usize + 1);
}
