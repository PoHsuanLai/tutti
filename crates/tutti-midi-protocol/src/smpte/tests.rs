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

/// **MTC's two-bit code names 24, 25, 29.97 drop-frame and 30 (M1 p.53)**: the
/// code reads back to the rate and the rate to its code, only the low two bits
/// are read, and each converts to the timecode rate of the same speed.
#[test]
fn the_mtc_code_is_the_two_bit_field_of_the_spec() {
    let table = [
        (0u8, MtcRate::Fps24, SmpteFrameRate::Fps24),
        (1, MtcRate::Fps25, SmpteFrameRate::Fps25),
        (2, MtcRate::Fps2997Df, SmpteFrameRate::Fps2997Df),
        (3, MtcRate::Fps30, SmpteFrameRate::Fps30),
    ];
    for (code, rate, smpte) in table {
        assert_eq!(MtcRate::masked(code), rate);
        assert_eq!(MtcRate::masked(code | 0x7C), rate);
        assert_eq!(rate.code(), code);
        assert_eq!(MtcRate::ALL[code as usize], rate);
        assert_eq!(SmpteFrameRate::from(rate), smpte);
        assert_eq!(smpte.mtc_rate(), Some(rate));
    }
}

/// **MTC has no code for 29.97 non-drop**: it has no `MtcRate`.
#[test]
fn non_drop_2997_has_no_mtc_rate() {
    assert_eq!(SmpteFrameRate::Fps2997Ndf.mtc_rate(), None);
}
