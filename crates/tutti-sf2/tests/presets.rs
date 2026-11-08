//! A preset's bank and program are typed, and a preset whose values are not
//! MIDI's is kept but cannot be played (SF2 2.04 section 7.2).

use std::io::Cursor;

use tutti_fixtures::sf2::{Inst, Preset, bytes};
use tutti_sf2::{Bank, SoundFont};
use tutti_ump::U7;

fn preset(name: &'static str, bank: u16, program: u16) -> Preset {
    Preset {
        name,
        bank,
        program,
        zones: vec![(vec![], 0)],
    }
}

/// 7.2: "if the value in either field is not a valid MIDI value of zero
/// through 127, or 128 for wBank, the preset cannot be played but should be
/// maintained". The preset is kept, with no bank or no program.
///
/// Mutation (run): `Bank::new` accepting up to 255 gives bank 200 a `Some` and
/// fails the second row; `Preset::patch` without the `U7` check fails the third.
#[test]
fn a_preset_outside_midis_values_is_kept_and_says_it_cannot_be_played() {
    let file = bytes(
        &[
            preset("piano", 0, 0),
            preset("kit", 128, 0),
            preset("far bank", 200, 5),
            preset("far program", 3, 200),
        ],
        &[Inst {
            name: "i",
            zones: vec![vec![]],
        }],
    );
    let font = SoundFont::new(&mut Cursor::new(file)).expect("the font loads");
    let seen: Vec<_> = font
        .presets()
        .iter()
        .map(|p| (p.name(), p.bank(), p.patch()))
        .collect();
    assert_eq!(
        seen,
        [
            (
                "piano",
                Some(Bank::Melodic(U7::new(0).unwrap())),
                U7::new(0)
            ),
            ("kit", Some(Bank::Percussion), U7::new(0)),
            ("far bank", None, U7::new(5)),
            (
                "far program",
                Some(Bank::Melodic(U7::new(3).unwrap())),
                None
            ),
        ]
    );
}
