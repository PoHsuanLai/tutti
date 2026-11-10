//! How the reader treats the RIFF framing (SF2 2.04 section 4, with the RIFF
//! rules it builds on), on files made from the fixture font with crafted
//! bytes: sub-chunks it does not know, odd sizes and their pad byte, the RIFF
//! size, and sizes that lie. Mutations are written beside each test, as run.

use std::io::Cursor;

use tutti_fixtures::sf2::{Inst, Preset, bytes};
use tutti_sf2::{Error, SoundFont};

fn base() -> Vec<u8> {
    bytes(
        &[Preset {
            name: "p",
            bank: 0,
            program: 0,
            zones: vec![(vec![], 0)],
        }],
        &[Inst {
            name: "i",
            zones: vec![vec![]],
        }],
    )
}

fn load(file: Vec<u8>) -> Result<SoundFont, Error> {
    SoundFont::new(&mut Cursor::new(file))
}

fn word(file: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(file[at..at + 4].try_into().unwrap())
}

fn set_word(file: &mut [u8], at: usize, value: u32) {
    file[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// A chunk's bytes: id, size, data, and the pad byte an odd size takes.
fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut bytes = id.to_vec();
    bytes.extend((data.len() as u32).to_le_bytes());
    bytes.extend(data);
    if data.len() % 2 == 1 {
        bytes.push(0);
    }
    bytes
}

/// Puts `chunk` first in the list of `kind`, growing the list and the RIFF.
fn insert_first(mut file: Vec<u8>, kind: &[u8; 4], chunk: &[u8]) -> Vec<u8> {
    let mut at = 12;
    while &file[at + 8..at + 12] != kind {
        at += 8 + word(&file, at + 4) as usize;
    }
    let size = word(&file, at + 4);
    set_word(&mut file, at + 4, size + chunk.len() as u32);
    let riff = word(&file, 4);
    set_word(&mut file, 4, riff + chunk.len() as u32);
    file.splice(at + 12..at + 12, chunk.iter().copied());
    file
}

/// Section 4: a reader ignores a sub-chunk it does not know, in each of the
/// three lists, whatever its size.
///
/// Mutation (run): the `_ =>` arms of the three lists back to an `Err` fail it.
#[test]
fn an_unknown_sub_chunk_is_ignored_in_every_list() {
    for kind in [b"INFO", b"sdta", b"pdta"] {
        let junk = chunk(b"JUNK", &[1, 2, 3, 4]);
        let font = load(insert_first(base(), kind, &junk))
            .unwrap_or_else(|e| panic!("{}: {e}", String::from_utf8_lossy(kind)));
        assert_eq!(font.presets().len(), 1);
    }
}

/// RIFF pads a chunk of odd size to an even length, and the pad is not in the
/// chunk's size. An odd-sized chunk (an unknown one, and a known string one)
/// before the rest leaves the rest where it was: the comment reads whole and
/// the file still parses.
///
/// Mutation (run): `skip_pad` doing nothing misreads the chunk after the pad
/// and fails every row.
#[test]
fn an_odd_sized_chunk_is_padded_to_even() {
    for kind in [b"INFO", b"sdta", b"pdta"] {
        let font = load(insert_first(base(), kind, &chunk(b"JUNK", &[7, 7, 7]))).unwrap();
        assert_eq!(font.presets().len(), 1);
    }
    let font = load(insert_first(base(), b"INFO", &chunk(b"ICMT", b"abcd\0"))).unwrap();
    assert_eq!(font.info().comments(), "abcd");
    assert_eq!(font.presets().len(), 1);
}

/// The RIFF size bounds the file: bytes after it are not read, and a list that
/// reaches past it is a short read, an error and not a panic or an over-read.
///
/// Mutation (run): reading the RIFF size and not limiting the reader (`take`)
/// fails the second assertion (the whole file parses).
#[test]
fn the_riff_size_is_honoured() {
    let mut trailing = base();
    trailing.extend(chunk(b"JUNK", &[0; 16]));
    assert!(load(trailing).is_ok(), "bytes after the RIFF are ignored");

    let mut short = base();
    let riff = word(&short, 4);
    set_word(&mut short, 4, riff - 64);
    let error = load(short).unwrap_err();
    assert!(matches!(error, Error::Io(_)), "{error:?}");
}

/// A size that reaches past its list is refused by name; one that claims
/// gigabytes is too, without reading or allocating them.
///
/// Mutation (run): dropping the bound in `next_chunk` lets the first row read
/// into the next list and fails the JUNK row. (`read_bytes` allocating `size`
/// up front would make the later rows take gigabytes; not run, for that reason.)
#[test]
fn a_size_that_reaches_past_its_list_is_refused() {
    let mut file = base();
    // The `ifil` chunk is the INFO list's first: its size is at 12 + 12 + 4.
    set_word(&mut file, 28, 0x0100_0000);
    let error = load(file).unwrap_err();
    assert!(
        matches!(error, Error::InvalidChunkSize(id) if &id == b"ifil"),
        "{error:?}"
    );

    // An unknown chunk is skipped by its size, which is held to the list too.
    let mut junk = chunk(b"JUNK", &[0; 4]);
    set_word(&mut junk, 4, 2000);
    let error = load(insert_first(base(), b"INFO", &junk)).unwrap_err();
    assert!(
        matches!(error, Error::InvalidChunkSize(id) if &id == b"JUNK"),
        "{error:?}"
    );

    let mut huge = insert_first(base(), b"INFO", &chunk(b"ICMT", b"x"));
    set_word(&mut huge, 28, u32::MAX);
    assert!(load(huge).is_err());

    // The sample data's `smpl` too, which has its own read.
    let mut file = base();
    let at = file.windows(4).position(|w| w == b"smpl").unwrap();
    set_word(&mut file, at + 4, u32::MAX);
    assert!(load(file).is_err());
}

/// `ifil` is four bytes. Another size is refused, not read as four.
///
/// Mutation (run): removing `expect_size` for `iver` fails it.
#[test]
fn a_version_chunk_of_the_wrong_size_is_refused() {
    let mut file = insert_first(base(), b"INFO", &chunk(b"iver", &[2, 0, 1, 0, 9, 9]));
    let error = load(std::mem::take(&mut file)).unwrap_err();
    assert!(
        matches!(error, Error::InvalidChunkSize(id) if &id == b"iver"),
        "{error:?}"
    );
}

/// A file cut anywhere is an error and never a panic.
#[test]
fn a_file_cut_at_any_length_is_an_error() {
    let file = base();
    for length in 0..file.len() {
        assert!(load(file[..length].to_vec()).is_err(), "cut at {length}");
    }
}
