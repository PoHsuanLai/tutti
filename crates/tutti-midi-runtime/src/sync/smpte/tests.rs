use super::*;

/// **Every rate reads back from its discriminant, and no other byte does**:
/// a byte past 4 is refused with the byte in the error, in every build (it was
/// a debug-only assertion that fell back to 29.97 drop-frame in release).
#[test]
fn a_frame_rate_reads_back_from_its_discriminant_and_nothing_else_does() {
    for rate in [
        SmpteFrameRate::Fps24,
        SmpteFrameRate::Fps25,
        SmpteFrameRate::Fps2997Df,
        SmpteFrameRate::Fps2997Ndf,
        SmpteFrameRate::Fps30,
    ] {
        assert_eq!(SmpteFrameRate::try_from(rate as u8), Ok(rate));
    }
    for raw in 5..=u8::MAX {
        assert_eq!(SmpteFrameRate::try_from(raw), Err(UnknownFrameRate(raw)));
    }
}
