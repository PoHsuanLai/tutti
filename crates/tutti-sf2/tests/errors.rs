//! What a failed load says.

use std::error::Error as _;
use std::io::{self, Cursor, Read};

use tutti_sf2::{Error, SoundFont};

/// A reader that fails.
struct Broken;

impl Read for Broken {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("the disk went away"))
    }
}

/// `Io` does not echo its source as its message: the message says what was
/// being done, and the cause is the `source`.
#[test]
fn an_io_failure_names_the_task_and_keeps_the_cause_as_its_source() {
    let error = SoundFont::from_reader(&mut Broken).unwrap_err();
    assert!(matches!(error, Error::Io(_)));
    assert_eq!(error.to_string(), "reading the SoundFont failed");
    let cause = error.source().expect("the cause is the source");
    assert_eq!(cause.to_string(), "the disk went away");
}

/// `open` of a path with no file is the file system's error, `NotFound`, as
/// `Io`: not a parse error and not a panic.
#[test]
fn opening_a_missing_path_is_an_io_not_found() {
    let dir = std::env::temp_dir().join("tutti-sf2-no-such-dir");
    let error = SoundFont::open(dir.join("missing.sf2")).unwrap_err();
    match error {
        Error::Io(cause) => assert_eq!(cause.kind(), io::ErrorKind::NotFound),
        other => panic!("expected Io, got {other:?}"),
    }
}

/// A truncated file is an I/O error too, not a panic.
/// Spec: sf2.riff.damage.rejected
#[test]
fn a_truncated_file_is_an_io_error() {
    let error = SoundFont::from_reader(&mut Cursor::new(b"RIFF".to_vec())).unwrap_err();
    assert!(matches!(error, Error::Io(_)), "{error:?}");
}

/// A chunk code in a message is the four ASCII bytes the file has, a byte
/// that is not printable shown as `?`, and the four-character code is not a
/// public type: the variants carry the bytes.
/// Spec: sf2.riff.form.sfbk
#[test]
fn a_wrong_form_type_names_what_it_found() {
    // The RIFF size is honoured, so it is true: the form type and 16 more bytes.
    let mut file = b"RIFF\x14\0\0\0WAV\x01".to_vec();
    file.extend([0; 16]);
    let error = SoundFont::from_reader(&mut Cursor::new(file)).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("'sfbk'"), "{message}");
    assert!(message.contains("'WAV?'"), "{message}");
    match error {
        Error::InvalidRiffChunkType { expected, actual } => {
            assert_eq!(&expected, b"sfbk");
            assert_eq!(&actual, b"WAV?");
        }
        other => panic!("{other:?}"),
    }
}

/// A file that is not RIFF at all.
/// Spec: sf2.riff.signature
#[test]
fn a_file_that_is_not_riff_is_refused() {
    let error = SoundFont::from_reader(&mut Cursor::new(b"OggS\0\0\0\0".to_vec())).unwrap_err();
    assert!(matches!(error, Error::RiffChunkNotFound), "{error:?}");
}
