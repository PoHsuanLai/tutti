//! A structurally defective file is a clean load error, never a panic; a
//! preset with no zone is ignored; and the DWORDs of a header are unsigned.
//! SF2 2.04 sections 7.2, 7.6 and 7.10.

use std::io::Cursor;

use tutti_fixtures::sf2::{Inst, Preset, bytes};
use tutti_sf2::{Error, SoundFont};

fn preset(name: &'static str, program: u16, zones: usize) -> Preset {
    Preset {
        name,
        bank: 0,
        program,
        zones: (0..zones).map(|_| (vec![], 0)).collect(),
    }
}

fn inst(zones: usize) -> Inst {
    Inst {
        name: "i",
        zones: (0..zones).map(|_| vec![]).collect(),
    }
}

/// Where the data of the sub-chunk `id` starts in `file`.
fn chunk(file: &[u8], id: &[u8; 4]) -> usize {
    file.windows(4)
        .position(|w| w == id)
        .map(|at| at + 8)
        .unwrap_or_else(|| panic!("no {id:?} chunk"))
}

fn load(file: Vec<u8>) -> Result<SoundFont, Error> {
    SoundFont::new(&mut Cursor::new(file))
}

/// Writes a little-endian word or double word at `at`.
fn put(file: &mut [u8], at: usize, value: u32, width: usize) {
    file[at..at + width].copy_from_slice(&value.to_le_bytes()[..width]);
}

const PHDR_RECORD: usize = 38;
const INST_RECORD: usize = 22;
const SHDR_RECORD: usize = 46;

/// 7.2: the preset bag indices only grow, and the terminal one is the number
/// of zones. A file where they do not is structurally defective: an `Err`,
/// where reading it would slice past the zone list.
///
/// Mutation (run): dropping the monotonic test in `PresetInfo::read_from_chunk`
/// fails the second row; dropping the terminal test in `Preset::create` fails
/// the first and third.
#[test]
fn a_preset_bag_index_that_is_wrong_is_a_clean_error() {
    let file = bytes(&[preset("a", 0, 1), preset("b", 1, 1)], &[inst(1)]);
    let phdr = chunk(&file, b"phdr");
    // The terminal index past the zone list.
    let mut past = file.clone();
    put(&mut past, phdr + 2 * PHDR_RECORD + 24, 9, 2);
    // The first preset's index above the second's.
    let mut backwards = file.clone();
    put(&mut backwards, phdr + 24, 2, 2);
    // The terminal index short of the zone list.
    let mut short = file.clone();
    put(&mut short, phdr + 2 * PHDR_RECORD + 24, 1, 2);
    for (what, file) in [("past", past), ("backwards", backwards), ("short", short)] {
        let error = load(file).err().unwrap_or_else(|| panic!("{what} loaded"));
        assert!(
            matches!(error, Error::InvalidPresetList),
            "{what}: {error:?}"
        );
    }
    assert!(load(bytes(&[preset("a", 0, 1), preset("b", 1, 1)], &[inst(1)])).is_ok());
}

/// 7.6: the same for the instrument bag indices.
///
/// Mutation (run): dropping the terminal test in `Instrument::create` fails
/// the first row.
#[test]
fn an_instrument_bag_index_that_is_wrong_is_a_clean_error() {
    let file = bytes(&[preset("a", 0, 1)], &[inst(1), inst(1)]);
    let at = chunk(&file, b"inst");
    let mut past = file.clone();
    put(&mut past, at + 2 * INST_RECORD + 20, 9, 2);
    let mut backwards = file.clone();
    put(&mut backwards, at + 20, 2, 2);
    for (what, file) in [("past", past), ("backwards", backwards)] {
        let error = load(file).err().unwrap_or_else(|| panic!("{what} loaded"));
        assert!(
            matches!(error, Error::InvalidInstrumentList),
            "{what}: {error:?}"
        );
    }
}

/// 7.3: the generator indices of the bags only grow and end at the number of
/// generators.
///
/// Mutation (run): dropping the terminal test in `Zone::create` fails it.
#[test]
fn a_generator_index_past_the_generators_is_a_clean_error() {
    let mut file = bytes(&[preset("a", 0, 1)], &[inst(1)]);
    let ibag = chunk(&file, b"ibag");
    // The terminal bag's generator index.
    put(&mut file, ibag + 4, 40, 2);
    let error = load(file).expect_err("it loaded");
    assert!(matches!(error, Error::InvalidGeneratorList), "{error:?}");
}

/// 7.2: "any preset with no zones should be ignored". It is not in the font and
/// not an error; the presets after it keep their own.
///
/// Mutation (run): turning the empty preset back into an error fails the load.
#[test]
fn a_preset_with_no_zone_is_ignored() {
    let file = bytes(
        &[
            preset("empty", 0, 0),
            preset("real", 1, 1),
            preset("also empty", 2, 0),
        ],
        &[inst(1)],
    );
    let font = load(file).expect("a preset with no zone is not a defect");
    let names: Vec<_> = font.presets().iter().map(|p| p.name()).collect();
    assert_eq!(names, ["real"]);
}

/// An instrument with no zone stays in the list (so the indices presets name
/// keep their meaning) and plays nothing.
///
/// Mutation (run): dropping empty instruments from the list makes the preset's
/// instrument index (1) name nothing and fails the load.
#[test]
fn an_instrument_with_no_zone_plays_nothing() {
    let file = bytes(
        &[Preset {
            name: "a",
            bank: 0,
            program: 0,
            zones: vec![(vec![], 1)],
        }],
        &[inst(0), inst(1)],
    );
    let font = load(file).expect("it loads");
    assert_eq!(font.instruments().len(), 2);
    assert!(font.instruments()[0].regions().is_empty());
    assert_eq!(font.instruments()[1].regions().len(), 1);
}

/// 7.10: a header's `dwStart`, `dwEnd`, `dwStartloop`, `dwEndloop` and
/// `dwSampleRate` are DWORDs. An address of 2^31 or more is past the sample
/// data and refused cleanly, and a rate of 2^31 or more reads as positive.
///
/// Mutation (run): dropping `end >= len` from `SoundFont::sanity_check` loads
/// the first row and then fails the test; reading `dwSampleRate` as an `i32`
/// makes the rate negative and fails the last assertion.
#[test]
fn the_dwords_of_a_header_are_unsigned() {
    let file = bytes(&[preset("a", 0, 1)], &[inst(1)]);
    let shdr = chunk(&file, b"shdr");
    for (what, offset) in [
        ("dwStart", 20),
        ("dwEnd", 24),
        ("dwStartloop", 28),
        ("dwEndloop", 32),
    ] {
        for value in [0x8000_0000_u32, u32::MAX] {
            let mut file = file.clone();
            put(&mut file, shdr + offset, value, 4);
            let error = load(file)
                .err()
                .unwrap_or_else(|| panic!("{what} {value:#x} loaded"));
            assert!(
                matches!(error, Error::SanityCheckFailed),
                "{what} {value:#x}: {error:?}"
            );
        }
    }
    let mut file = file.clone();
    put(&mut file, shdr + 36, 0x8000_0000 + 44_100, 4);
    let font = load(file).expect("the rate is not a range");
    assert_eq!(
        font.sample_headers()[0].sample_rate().get(),
        f64::from(0x8000_0000_u32 + 44_100)
    );
    assert!(font.instruments()[0].regions()[0].sample_rate().get() > 0.0);
    let _ = SHDR_RECORD;
}

/// 7.2: `dwLibrary`, `dwGenre` and `dwMorphology` are DWORDs "preserved as
/// read".
///
/// Mutation (run): reading them as `i32` and widening fails the value.
#[test]
fn the_reserved_dwords_of_a_preset_are_preserved_as_read() {
    let mut file = bytes(&[preset("a", 0, 1)], &[inst(1)]);
    let phdr = chunk(&file, b"phdr");
    put(&mut file, phdr + 26, u32::MAX, 4);
    put(&mut file, phdr + 30, 0x8000_0001, 4);
    put(&mut file, phdr + 34, 7, 4);
    let font = load(file).expect("it loads");
    let p = &font.presets()[0];
    assert_eq!(
        (p.library(), p.genre(), p.morphology()),
        (u32::MAX, 0x8000_0001, 7)
    );
}
