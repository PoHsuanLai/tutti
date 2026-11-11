use super::*;

fn make_quarter_frame(piece: u8, nibble: u8) -> u8 {
    (piece << 4) | (nibble & 0x0F)
}

fn feed_timecode(decoder: &mut MtcDecoder, h: u8, m: u8, s: u8, f: u8, rate: u8) {
    let hours_high_and_rate = ((h >> 4) & 0x01) | ((rate & 0x03) << 1);
    let data: [u8; 8] = [
        make_quarter_frame(0, f & 0x0F),
        make_quarter_frame(1, (f >> 4) & 0x0F),
        make_quarter_frame(2, s & 0x0F),
        make_quarter_frame(3, (s >> 4) & 0x0F),
        make_quarter_frame(4, m & 0x0F),
        make_quarter_frame(5, (m >> 4) & 0x0F),
        make_quarter_frame(6, h & 0x0F),
        make_quarter_frame(7, hours_high_and_rate),
    ];
    for d in data {
        decoder.feed(d);
    }
}

// Joining mid-cycle: the first byte seen is piece 3, with no piece before it.
#[test]
fn joining_mid_cycle_waits_for_the_next_piece_zero() {
    let mut decoder = MtcDecoder::new();
    decoder.feed(make_quarter_frame(3, 5));
    assert_eq!(decoder.timecode(), None);
    // Pieces 4..=7 follow the unknown start; they must not assemble.
    for piece in 4..8 {
        decoder.feed(make_quarter_frame(piece, 0));
    }
    assert_eq!(decoder.timecode(), None);
    feed_timecode(&mut decoder, 1, 2, 3, 4, 1);
    let tc = decoder.timecode().expect("a full cycle after piece 0");
    // 1:02:03:04 and the receiver's +2 frames.
    assert_eq!(
        (tc.hours(), tc.minutes(), tc.seconds(), tc.frames()),
        (1, 2, 3, 6)
    );
}

/// Eight quarter-frames assemble into the timecode they spell, at each of
/// the four SMPTE rates.
///
/// One table rather than a test per rate: the assembly is the same property
/// in every row, and the rate bits are the column. Written out this way the
/// rate-bits → [`SmpteFrameRate`] mapping is readable as a mapping — the
/// thing most likely to be transposed — instead of being spread over four
/// functions where a swapped pair reads as two independent tests.
#[test]
fn eight_quarter_frames_assemble_into_a_timecode_at_every_rate() {
    // (fed h, m, s, f, rate bits, expected rate, reported h, m, s, f): the
    // reported position is the fed one plus the receiver's +2 frames.
    let cases = [
        (
            1u8,
            2u8,
            3u8,
            4u8,
            0u8,
            SmpteFrameRate::Fps24,
            (1u8, 2u8, 3u8, 6u8),
        ),
        (10, 30, 0, 0, 1, SmpteFrameRate::Fps25, (10, 30, 0, 2)),
        (0, 0, 0, 0, 2, SmpteFrameRate::Fps2997Df, (0, 0, 0, 2)),
        (1, 2, 3, 4, 3, SmpteFrameRate::Fps30, (1, 2, 3, 6)),
        // A frame number needing both nibbles, so the high-nibble piece is
        // not silently dropped; two frames on, it carries into the second.
        (0, 0, 1, 23, 0, SmpteFrameRate::Fps24, (0, 0, 2, 1)),
    ];

    for (h, m, s, f, rate_bits, expected_rate, shown) in cases {
        let mut decoder = MtcDecoder::new();
        feed_timecode(&mut decoder, h, m, s, f, rate_bits);

        let tc = decoder.timecode().expect("eight pieces is a full timecode");
        assert_eq!(
            (tc.hours, tc.minutes, tc.seconds, tc.frames),
            shown,
            "rate bits {rate_bits}"
        );
        assert_eq!(tc.frame_rate, expected_rate, "rate bits {rate_bits}");
    }
}

#[test]
fn test_no_timecode_before_8_messages() {
    let mut decoder = MtcDecoder::new();
    assert!(decoder.timecode().is_none());

    for piece in 0..7 {
        decoder.feed(make_quarter_frame(piece, 0));
    }
    assert!(decoder.timecode().is_none());
}

#[test]
fn test_out_of_sequence_pieces_discarded() {
    let mut decoder = MtcDecoder::new();
    // A gap (0,1,3,…) must not assemble a timecode from mismatched
    // nibble slots — the partial frame is discarded at the gap.
    for piece in [0u8, 1, 3, 4, 5, 6, 7] {
        decoder.feed(make_quarter_frame(piece, 0));
    }
    assert!(
        decoder.timecode().is_none(),
        "out-of-sequence pieces must not produce a timecode"
    );
}

#[test]
fn test_resyncs_on_next_piece_zero() {
    let mut decoder = MtcDecoder::new();
    // Feed a broken run, then a clean full frame: the clean one wins.
    decoder.feed(make_quarter_frame(0, 0));
    decoder.feed(make_quarter_frame(2, 0)); // gap → discard, wait for 0
    assert!(decoder.timecode().is_none());

    feed_timecode(&mut decoder, 1, 2, 3, 4, 3);
    let tc = decoder.timecode().unwrap();
    // 1:02:03:04 and the receiver's +2 frames.
    assert_eq!(
        (tc.hours(), tc.minutes(), tc.seconds(), tc.frames()),
        (1, 2, 3, 6)
    );
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

#[test]
fn test_to_beats() {
    let tc = SmpteTimecode::new(0, 1, 0, 0, SmpteFrameRate::Fps30).unwrap();
    // 60 seconds at 120 BPM = 120 beats
    assert!((tc.to_beats(Bpm::of(120.0)).get() - 120.0).abs() < 0.001);
}

#[test]
fn test_reset() {
    let mut decoder = MtcDecoder::new();
    feed_timecode(&mut decoder, 1, 2, 3, 4, 3);
    assert!(decoder.timecode().is_some());

    decoder.reset();
    assert!(decoder.timecode().is_none());
}

#[test]
fn test_sequential_updates() {
    let mut decoder = MtcDecoder::new();

    feed_timecode(&mut decoder, 0, 0, 0, 0, 3);
    let tc1 = decoder.timecode().unwrap();
    assert_eq!(tc1.frames, 2, "0 and the receiver's +2 frames");

    feed_timecode(&mut decoder, 0, 0, 0, 1, 3);
    let tc2 = decoder.timecode().unwrap();
    assert_eq!(tc2.frames, 3);
}

#[test]
fn test_hours_above_15() {
    let mut decoder = MtcDecoder::new();
    // Hour 23: low nibble = 7 (0x07), bit4 = 1
    feed_timecode(&mut decoder, 23, 59, 59, 25, 3);
    let tc = decoder.timecode().unwrap();
    assert_eq!(tc.hours, 23);
    assert_eq!(tc.minutes, 59);
    assert_eq!(tc.seconds, 59);
    assert_eq!(tc.frames, 27, "25 and the receiver's +2 frames");
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

/// A frame the wire framed into a position no clock reads (hour 28) is
/// dropped; the decoder keeps the last good timecode.
#[test]
fn a_corrupt_position_is_dropped_and_the_last_good_one_kept() {
    let mut decoder = MtcDecoder::new();
    feed_timecode(&mut decoder, 1, 2, 3, 4, 3);
    let good = decoder.timecode();
    assert!(good.is_some());
    // Hour 28 at 30 fps: the five hour bits hold it, no clock does.
    feed_timecode(&mut decoder, 28, 2, 3, 4, 3);
    assert_eq!(decoder.timecode(), good);
}

/// MTC, Quarter Frame Message Implementation: "A receiver of this time must
/// keep an internal offset of +2 frames for displaying." The reported time
/// carries across the second, the minute (drop-frame labels `;00` and `;01`
/// are skipped: 00:00:59:29 plus two frames is 00:01:00:03) and the day.
#[test]
fn the_reported_time_is_two_frames_on_across_every_carry() {
    for (fed, rate_bits, expected) in [
        ((0u8, 0u8, 0u8, 29u8), 3u8, (0u8, 0u8, 1u8, 1u8)), // 30 fps, second
        ((0, 0, 59, 29), 2, (0, 1, 0, 3)),                  // drop-frame minute
        ((0, 59, 59, 29), 3, (1, 0, 0, 1)),                 // hour
        ((23, 59, 59, 29), 3, (0, 0, 0, 1)),                // the day wraps
    ] {
        let mut decoder = MtcDecoder::new();
        feed_timecode(&mut decoder, fed.0, fed.1, fed.2, fed.3, rate_bits);
        let tc = decoder.timecode().unwrap();
        assert_eq!(
            (tc.hours(), tc.minutes(), tc.seconds(), tc.frames()),
            expected,
            "from {fed:?}"
        );
    }
}

/// MTC, Quarter Frame Messages: the bits above each count are "Undefined and
/// reserved for future use. Transmitter must set these bits to 0 and receiver
/// should ignore!" A cycle with every reserved bit set (frame bits 5-7,
/// seconds and minutes bits 6-7, hours bit 7, which is bit 3 of piece 7) reads
/// as the cycle with them clear, not as a corrupt frame.
#[test]
fn reserved_bits_of_the_quarter_frames_are_ignored() {
    let mut decoder = MtcDecoder::new();
    // 01:02:03:04 at 30 fps (rate 3).
    let nibbles = [
        4,              // frames low
        0xE,            // frames high: bit 0 is the 16s, bits 1-3 reserved
        3,              // seconds low
        0xC,            // seconds high: bits 0-1 count, bits 2-3 reserved
        2,              // minutes low
        0xC,            // minutes high, as seconds
        1,              // hours low
        (3 << 1) | 0x8, // hours high bit 0 clear, rate 3, bit 3 reserved
    ];
    for (piece, nibble) in nibbles.into_iter().enumerate() {
        decoder.feed(make_quarter_frame(piece as u8, nibble));
    }
    let tc = decoder.timecode().expect("not a corrupt frame");
    // 1:02:03:04 and the receiver's +2 frames.
    assert_eq!(
        (tc.hours(), tc.minutes(), tc.seconds(), tc.frames()),
        (1, 2, 3, 6)
    );
}
