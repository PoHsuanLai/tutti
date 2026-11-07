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

/// Instrument 0 of the fixture ("Flute TB"), decoded by hand from the file's
/// `inst`, `ibag` and `igen` chunks with a separate script: ten regions, each
/// a key range and a sample id, none with a velocity range.
const FLUTE_TB: [(u8, u8, usize); 10] = [
    (0, 60, 5),
    (61, 65, 6),
    (66, 68, 8),
    (69, 71, 9),
    (72, 76, 3),
    (77, 80, 0),
    (81, 84, 1),
    (85, 89, 4),
    (90, 94, 2),
    (95, 108, 7),
];

// Mutation (run): `low..=high` -> `low..=low` in `generator_range` makes every
// range a single key and fails the range comparison on the first region.
#[test]
fn the_first_instrument_has_the_regions_decoded_from_the_file() {
    let soundfont = timgm6mb();
    let flute = &soundfont.instruments()[0];
    assert_eq!(flute.name(), "Flute TB");
    let regions: Vec<_> = flute
        .regions()
        .iter()
        .map(|r| (*r.key_range().start(), *r.key_range().end(), r.sample_id()))
        .collect();
    assert_eq!(regions, FLUTE_TB);
    for region in flute.regions() {
        assert_eq!(region.velocity_range(), 0..=127, "no velocity range: all");
    }
}

// Mutation (run): `contains` testing the key against `start..end` (exclusive)
// fails at the last key of a range. The fixture has no velocity range, so the
// velocity half of `contains` is covered by `tutti-rustysynth`'s
// `a_region_answers_only_the_velocities_of_its_range`.
#[test]
fn a_region_contains_the_keys_and_velocities_of_the_decoded_ranges() {
    let soundfont = timgm6mb();
    let regions = soundfont.instruments()[0].regions();
    let contains = |region: usize, key: u8, velocity: u8| {
        regions[region].contains(NoteNumber::new(key).unwrap(), U7::new(velocity).unwrap())
    };
    // Region 1 is keys 61 to 65.
    assert!(contains(1, 61, 1) && contains(1, 65, 127) && contains(1, 63, 64));
    assert!(!contains(1, 60, 64) && !contains(1, 66, 64));
    // Region 0 is keys 0 to 60, region 9 is 95 to 108: the ends of the file.
    assert!(contains(0, 0, 64) && contains(0, 60, 64) && !contains(0, 61, 64));
    assert!(contains(9, 95, 64) && contains(9, 108, 64) && !contains(9, 109, 64));
    // Every key from 0 to 108 is in exactly one of the ten regions.
    for key in 0..=108 {
        let hits = (0..regions.len()).filter(|&r| contains(r, key, 64)).count();
        assert_eq!(hits, 1, "key {key}");
    }
    assert!(!(0..regions.len()).any(|r| contains(r, 109, 64)));
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
