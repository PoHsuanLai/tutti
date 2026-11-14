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

#[test]
fn test_to_seconds() {
    let tc = SmpteTimecode::new(1, 0, 0, 0, SmpteFrameRate::Fps30).unwrap();
    assert!((tc.to_seconds().get() - 3600.0).abs() < 0.001);
}

#[test]
fn drop_frame_labels_skip_00_and_01_except_every_tenth_minute() {
    let df = SmpteFrameRate::Fps2997Df;
    let at = |n| SmpteTimecode::from_frame_count(n, df).to_string();
    assert_eq!(at(1799), "00:00:59:29");
    assert_eq!(at(1800), "00:01:00:02");
    assert_eq!(at(3597), "00:01:59:29");
    assert_eq!(at(3598), "00:02:00:02");
    assert_eq!(at(17_981), "00:09:59:29");
    // The tenth minute keeps frames 00 and 01.
    assert_eq!(at(17_982), "00:10:00:00");
    assert_eq!(at(17_983), "00:10:00:01");
    assert_eq!(at(17_982 * 6), "01:00:00:00");
    // Non-drop labels count every frame.
    let ndf = SmpteFrameRate::Fps2997Ndf;
    assert_eq!(
        SmpteTimecode::from_frame_count(1800, ndf).to_string(),
        "00:01:00:00"
    );
}

#[test]
fn invalid_drop_frame_labels_count_as_the_next_valid_one() {
    let at = |m, s, f| SmpteTimecode::new(0, m, s, f, SmpteFrameRate::Fps2997Df).unwrap();
    assert_eq!(at(1, 0, 0).frame_count(), 1800);
    assert_eq!(at(1, 0, 1).frame_count(), 1800);
    assert_eq!(at(1, 0, 2).frame_count(), 1800);
    // The tenth minute has them; later seconds in a minute do too.
    assert_eq!(at(10, 0, 0).frame_count(), 17_982);
    assert_eq!(at(1, 1, 0).frame_count(), 1830 - 2);
}

#[test]
fn frame_count_inverts_from_frame_count_and_to_seconds_follows_it() {
    for rate in [
        SmpteFrameRate::Fps24,
        SmpteFrameRate::Fps25,
        SmpteFrameRate::Fps2997Df,
        SmpteFrameRate::Fps2997Ndf,
        SmpteFrameRate::Fps30,
    ] {
        for n in [0, 1, 1799, 1800, 17_981, 17_982, 107_892, 2_000_000] {
            let tc = SmpteTimecode::from_frame_count(n, rate);
            assert_eq!(tc.frame_count(), n, "{rate:?} {n}");
        }
    }
    // 10 minutes of drop-frame is 17_982 frames: 599.999… s (SMPTE 12M).
    let tc = SmpteTimecode::from_frame_count(17_982, SmpteFrameRate::Fps2997Df);
    assert!(
        (tc.to_seconds().get() - 600.0).abs() < 0.001,
        "{}",
        tc.to_seconds().get()
    );
}

/// `new` refuses each field one past its range, per rate, and accepts the
/// last in-range value.
#[test]
fn new_refuses_a_field_out_of_range() {
    use SmpteFrameRate::*;
    assert!(SmpteTimecode::new(23, 59, 59, 23, Fps24).is_some());
    assert!(SmpteTimecode::new(24, 0, 0, 0, Fps30).is_none());
    assert!(SmpteTimecode::new(0, 60, 0, 0, Fps30).is_none());
    assert!(SmpteTimecode::new(0, 0, 60, 0, Fps30).is_none());
    assert!(SmpteTimecode::new(0, 0, 0, 24, Fps24).is_none());
    assert!(SmpteTimecode::new(0, 0, 0, 25, Fps25).is_none());
    assert!(SmpteTimecode::new(0, 0, 0, 24, Fps25).is_some());
    for rate in [Fps2997Df, Fps2997Ndf, Fps30] {
        assert!(SmpteTimecode::new(0, 0, 0, 29, rate).is_some());
        assert!(SmpteTimecode::new(0, 0, 0, 30, rate).is_none());
    }
}
