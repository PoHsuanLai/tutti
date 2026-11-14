//! A preset's bank and program are typed, and a preset whose values are not
//! MIDI's is kept but cannot be played (SF2 2.04 section 7.2).

use std::io::Cursor;

use tutti_fixtures::sf2::{Inst, Preset, bytes};
use tutti_sf2::{Bank, InstrumentGenerator, PresetGenerator, SoundFont};
use tutti_ump::U7;

fn preset(name: &'static str, bank: u16, program: u16) -> Preset {
    Preset {
        name,
        bank,
        program,
        zones: vec![(vec![], 0)],
    }
}

/// 7.2: a preset whose bank or program field is outside MIDI's 0 to 127 (128
/// for wBank) is kept but cannot be played: it loads, with no bank or no
/// program.
/// Spec: sf2.pdta.phdr.bank-program.range
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
    let font = SoundFont::from_reader(&mut Cursor::new(file)).expect("the font loads");
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

/// 8.1.2: a generator number the spec leaves unused or reserved (14, 18 to 20,
/// 42, 49, 55, 59), `endOper` (60) and a number past it are ignored, at both
/// levels: the records before and after them still apply, and the values they
/// carry reach no named generator.
/// Spec: sf2.genrule.unused.ignored, sf2.pdta.pgen.unknown-generator.ignored, sf2.pdta.igen.unknown-generator.ignored, sf2.gen.unused1, sf2.gen.unused2, sf2.gen.unused3, sf2.gen.unused4, sf2.gen.reserved1, sf2.gen.reserved2, sf2.gen.reserved3, sf2.gen.unused5, sf2.gen.endOper
#[test]
fn an_unused_or_reserved_generator_number_is_ignored() {
    const UNUSED: [u16; 11] = [14, 18, 19, 20, 42, 49, 55, 59, 60, 61, u16::MAX];
    let mut preset_zone = vec![(17, 9)];
    let mut instrument_zone = vec![(17, 4)];
    for id in UNUSED {
        preset_zone.push((id, 7));
        instrument_zone.push((id, 7));
    }
    preset_zone.push((51, 2));
    let file = bytes(
        &[Preset {
            name: "p",
            bank: 0,
            program: 0,
            zones: vec![(preset_zone, 0)],
        }],
        &[Inst {
            name: "i",
            zones: vec![instrument_zone],
        }],
    );
    let font = SoundFont::from_reader(&mut Cursor::new(file)).expect("the font loads");
    let region = &font.presets()[0].regions()[0];
    let instrument = &font.instruments()[region.instrument_id()].regions()[0];
    assert_eq!(region.generator(PresetGenerator::Pan), 9);
    assert_eq!(region.generator(PresetGenerator::CoarseTune), 2);
    assert_eq!(instrument.generator(InstrumentGenerator::Pan), 4);
    for named in [
        PresetGenerator::ModulationLfoToVolume,
        PresetGenerator::ChorusEffectsSend,
        PresetGenerator::ReverbEffectsSend,
        PresetGenerator::KeyRange,
    ] {
        let expected = if named == PresetGenerator::KeyRange {
            0x7F00
        } else {
            0
        };
        assert_eq!(region.generator(named), expected, "{named:?}");
    }
    assert_eq!(region.key_range(), 0..=127);
}

/// 8.1.2: `instrument` (41) is a preset-level generator; in an instrument zone it
/// is ignored. The zone keeps its other generators, still ends at its `sampleID`,
/// and is not taken for a global zone nor dropped.
/// Spec: sf2.pdta.igen.instrument.illegal
#[test]
fn an_instrument_record_in_an_instrument_zone_is_ignored() {
    let file = bytes(
        &[Preset {
            name: "p",
            bank: 0,
            program: 0,
            zones: vec![(vec![], 0)],
        }],
        &[Inst {
            name: "i",
            zones: vec![vec![(41, 1), (17, 4), (41, 1)]],
        }],
    );
    let font = SoundFont::from_reader(&mut Cursor::new(file)).expect("the font loads");
    let regions = font.instruments()[0].regions();
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0].generator(InstrumentGenerator::Pan), 4);
    assert_eq!(regions[0].sample_id(), 0);
}
