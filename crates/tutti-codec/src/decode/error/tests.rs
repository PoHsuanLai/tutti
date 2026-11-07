use super::*;

/// Every decoder error maps to its own variant with its message intact.
///
/// Mutation (run): mapping `Symphonia::DecodeError` to `Unsupported` fails the
/// first assertion; mapping `LimitError` to `Malformed` fails the fourth.
#[test]
fn each_decoder_error_has_its_own_variant() {
    assert!(matches!(
        convert(Symphonia::DecodeError("bad")),
        DecodeError::Malformed(m) if m == "bad"
    ));
    assert!(matches!(
        convert(Symphonia::Unsupported("codec")),
        DecodeError::Unsupported(m) if m == "codec"
    ));
    let io = convert(Symphonia::IoError(io::Error::from(io::ErrorKind::NotFound)));
    assert!(matches!(io, DecodeError::Io(e) if e.kind() == io::ErrorKind::NotFound));
    assert!(matches!(
        convert(Symphonia::LimitError("big")),
        DecodeError::Limit(m) if m == "big"
    ));
    assert!(matches!(
        convert(Symphonia::ResetRequired),
        DecodeError::Malformed(_)
    ));
}

/// Each way a seek can fail reads differently, and names no decoder type.
///
/// Mutation (run): `OutOfRange` reusing `ForwardOnly`'s message fails the
/// distinctness check.
#[test]
fn seek_failures_keep_their_reason() {
    use symphonia::core::errors::SeekErrorKind::*;
    let reasons: Vec<String> = [Unseekable, ForwardOnly, OutOfRange, InvalidTrack]
        .into_iter()
        .map(|k| convert(Symphonia::SeekError(k)).to_string())
        .collect();
    for (i, a) in reasons.iter().enumerate() {
        assert!(a.starts_with("cannot seek: "), "{a}");
        assert!(!reasons[i + 1..].contains(a), "{a} is not distinct");
    }
}

/// A copy keeps an I/O error's kind and text, which `FileIn` relies on to report
/// a sticky fault again.
///
/// Mutation (run): rebuilding the `Io` copy with `ErrorKind::Other` fails.
#[test]
fn a_copy_keeps_the_io_kind_and_message() {
    let error = DecodeError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "cut short"));
    let copy = error.clone();
    assert!(matches!(&copy, DecodeError::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof));
    assert_eq!(copy.to_string(), error.to_string());
}
