//! An index in a 16-bit generator is read as unsigned: SF2 2.04 section 8.1.2
//! (generators 41 and 53) give them as indexes into the instrument and sample
//! lists, and a list may be longer than 32768.

use std::io::Cursor;

use tutti_fixtures::sf2::{Inst, Preset, bytes_with_samples, generator};
use tutti_sf2::{Error, SoundFont};

const BIG: usize = 32_769;

/// A zone naming sample 32768, a 16-bit word of 0x8000, which a signed read
/// makes negative and a sign-extending cast turns into a huge index.
#[test]
fn a_sample_id_of_32768_or_more_loads_and_reads_back() {
    let zone = vec![(generator::SAMPLE_ID, 0x8000_u16 as i16)];
    let file = bytes_with_samples(
        &[Preset {
            name: "p",
            bank: 0,
            program: 0,
            zones: vec![(vec![], 0)],
        }],
        &[Inst {
            name: "i",
            zones: vec![zone],
        }],
        BIG,
    );
    let font = SoundFont::new(&mut Cursor::new(file)).expect("a sample id of 32768 is valid");
    assert_eq!(font.sample_headers().len(), BIG);
    assert_eq!(font.instruments()[0].regions()[0].sample_id(), 32_768);
}

/// A sample id past the list is still refused, with its value.
#[test]
fn a_sample_id_past_the_list_is_refused_with_its_value() {
    let zone = vec![(generator::SAMPLE_ID, 3)];
    let file = bytes_with_samples(
        &[Preset {
            name: "p",
            bank: 0,
            program: 0,
            zones: vec![(vec![], 0)],
        }],
        &[Inst {
            name: "i",
            zones: vec![zone],
        }],
        2,
    );
    match SoundFont::new(&mut Cursor::new(file)) {
        Err(Error::InvalidSampleId {
            instrument_id: 0,
            sample_id: 3,
        }) => {}
        other => panic!("{other:?}"),
    }
}

/// The same for the instrument a preset zone names.
#[test]
fn an_instrument_id_of_32768_or_more_loads_and_reads_back() {
    let insts: Vec<Inst> = (0..BIG)
        .map(|_| Inst {
            name: "i",
            // One generator a zone: a generator index is 16 bits, and 32769
            // instruments of two would not fit.
            zones: vec![vec![(generator::SAMPLE_ID, 0)]],
        })
        .collect();
    let file = bytes_with_samples(
        &[Preset {
            name: "p",
            bank: 0,
            program: 0,
            zones: vec![(vec![], 32_768)],
        }],
        &insts,
        1,
    );
    let font = SoundFont::new(&mut Cursor::new(file)).expect("an instrument id of 32768 is valid");
    assert_eq!(font.presets()[0].regions()[0].instrument_id(), 32_768);
}
