use super::*;

/// A path that is not there is an I/O error of kind `NotFound`, from both
/// entry points.
///
/// Mutation (run): mapping `probe_path`'s open failure to `Unsupported` fails.
#[test]
fn a_missing_file_is_an_io_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("none.wav");
    for error in [
        Wave::read(&missing).unwrap_err(),
        Wave::read_metadata(&missing).unwrap_err(),
    ] {
        assert!(
            matches!(&error, DecodeError::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
            "{error}"
        );
    }
}

/// Bytes that are no container are refused, not decoded as silence.
///
/// Mutation (run): `Wave::from_bytes` returning an empty `Wave` instead of the
/// probe's error (for a 4096-byte input) fails the first assertion.
#[test]
fn bytes_that_are_no_audio_are_an_error() {
    assert!(Wave::from_bytes(vec![0x5a; 4096]).is_err());
    assert!(Wave::from_bytes(Vec::new()).is_err());
}
