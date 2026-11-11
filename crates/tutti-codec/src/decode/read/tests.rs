use super::*;

/// A path that is not there is an I/O error of kind `NotFound`, from both
/// entry points.
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
#[test]
fn bytes_that_are_no_audio_are_an_error() {
    assert!(Wave::from_bytes(vec![0x5a; 4096]).is_err());
    assert!(Wave::from_bytes(Vec::new()).is_err());
}

/// A track that states no sample rate is refused with `NoSampleRate`, never
/// given one of ours: the file would play at the wrong pitch with no error.
/// (A container that states none cannot be built from bytes here, since the
/// demuxers refuse a zero rate themselves, so the track's parameters are.)
#[test]
fn a_track_with_no_sample_rate_is_refused() {
    use symphonia::core::codecs::CodecParameters;
    let mut params = CodecParameters::new();
    assert!(matches!(
        sample_rate_of(&params),
        Err(DecodeError::NoSampleRate)
    ));
    params.with_sample_rate(48_000);
    assert_eq!(sample_rate_of(&params).expect("stated"), 48_000);
}
