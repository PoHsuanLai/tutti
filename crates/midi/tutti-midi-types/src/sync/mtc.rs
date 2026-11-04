//! MIDI Time Code: assembling quarter-frame messages into SMPTE positions.
//!
//! MTC transmits a wall-clock position one nibble at a time, eight messages per
//! two frames, so a decoder is a small state machine rather than a parse
//! function — [`MtcDecoder`] holds the partial frame between bytes.
//!
//! This is the *wall-clock* half of MIDI sync; the musical half (beat position
//! and inferred tempo) is [`crate::sync::clock`]. A device sends one or the
//! other, and which you decode is decided by the source, not by preference.

use crate::sync::SmpteFrameRate;
use tutti_types::Bpm;

/// An assembled SMPTE position: `HH:MM:SS:FF` plus the rate that gives the frame
/// count meaning.
///
/// The fields are the timecode's own *labels*, not a duration — under
/// [`SmpteFrameRate::Fps2997Df`] some frame numbers never occur, so arithmetic on
/// these fields directly is wrong. Convert with [`to_seconds`](Self::to_seconds)
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmpteTimecode {
    /// Hours, 0..=23. MTC carries this in 5 bits split across two nibbles.
    pub hours: u8,
    /// Minutes, 0..=59.
    pub minutes: u8,
    /// Seconds, 0..=59.
    pub seconds: u8,
    /// Frames within the second, 0..`frame_rate`.
    pub frames: u8,
    /// The rate this position is counted at. Carried in the same quarter-frame as
    /// the top hours bit, so it arrives with the position rather than being
    /// configured out of band.
    pub frame_rate: SmpteFrameRate,
}

impl core::fmt::Display for SmpteTimecode {
    /// Canonical `HH:MM:SS:FF` SMPTE form (colon-separated, zero-padded).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}:{:02}",
            self.hours, self.minutes, self.seconds, self.frames
        )
    }
}

impl SmpteTimecode {
    /// Elapsed time from `00:00:00:00`, in seconds.
    ///
    /// `f64` rather than `Seconds`, which is `f32`: an hour-scale position at
    /// 29.97 fps needs more mantissa than `f32` has, and this is the documented
    /// precision carve-out from the units rule.
    ///
    /// Counts through [`frame_count`](Self::frame_count), so drop-frame labels
    /// are mapped to the frames that elapsed. Non-drop 29.97 labels are counted
    /// at face value, at 30000/1001 frames per second.
    pub fn to_seconds(&self) -> f64 {
        let (num, den) = self.frame_rate.ratio();
        self.frame_count() as f64 * den as f64 / num as f64
    }

    /// Frames elapsed since `00:00:00:00`.
    ///
    /// Under [`SmpteFrameRate::Fps2997Df`] this skips the labels `;00` and
    /// `;01` that each minute not divisible by ten omits (SMPTE 12M). Labels
    /// that cannot occur in drop-frame are counted as the next label that does.
    pub fn frame_count(&self) -> u64 {
        let per_sec = self.frame_rate.labels_per_second();
        let minutes = u64::from(self.hours) * 60 + u64::from(self.minutes);
        let mut frames = u64::from(self.frames);
        // `;00` and `;01` do not exist in a minute not divisible by ten: they
        // count as `;02`, the next label that does.
        let dropped_label =
            self.frame_rate.is_drop_frame() && self.seconds == 0 && minutes % 10 != 0;
        if dropped_label {
            frames = frames.max(2);
        }
        let labelled = (minutes * 60 + u64::from(self.seconds)) * per_sec + frames;
        if self.frame_rate.is_drop_frame() {
            labelled.saturating_sub(2 * (minutes - minutes / 10))
        } else {
            labelled
        }
    }

    /// The label of frame number `count` since `00:00:00:00` at `frame_rate`;
    /// the inverse of [`frame_count`](Self::frame_count). Hours wrap at 24.
    pub fn from_frame_count(count: u64, frame_rate: SmpteFrameRate) -> Self {
        let per_sec = frame_rate.labels_per_second();
        let labelled = if frame_rate.is_drop_frame() {
            // 17_982 frames per ten minutes: the first minute keeps all 1800
            // labels, the other nine drop two.
            let (tens, rem) = (count / 17_982, count % 17_982);
            let dropped = if rem < 2 { 0 } else { 2 * ((rem - 2) / 1798) };
            count + 18 * tens + dropped
        } else {
            count
        };
        let secs = labelled / per_sec;
        Self {
            hours: ((secs / 3600) % 24) as u8,
            minutes: ((secs / 60) % 60) as u8,
            seconds: (secs % 60) as u8,
            frames: (labelled % per_sec) as u8,
            frame_rate,
        }
    }

    /// Position in beats at a constant `tempo`, from `00:00:00:00`.
    ///
    /// Assumes the tempo held for the whole span — there is no tempo map here, so
    /// this is exact only for a fixed-tempo project.
    ///
    /// The tempo is a [`Bpm`]; the return stays `f64`, like
    /// [`to_seconds`](Self::to_seconds), because SMPTE timecode is the
    /// documented precision carve-out — `Beat` would carry it, but the
    /// hour-scale seconds it is derived from would not survive `Seconds`.
    pub fn to_beats(&self, tempo: impl Into<Bpm>) -> f64 {
        self.to_seconds() * tempo.into().get() / 60.0
    }
}

/// Assembles 8 MTC quarter-frame messages into full SMPTE timecode.
///
/// MTC sends timecode as 8 sequential quarter-frame messages (0xF1 data),
/// each carrying a nibble of the full HH:MM:SS:FF timecode.
#[derive(Debug, Clone)]
pub struct MtcDecoder {
    nibbles: [u8; 8],
    count: u8,
    /// The piece accepted last; `None` before the first, so a decoder may join mid-cycle.
    last_piece: Option<u8>,
    timecode: Option<SmpteTimecode>,
}

impl MtcDecoder {
    /// Builds a decoder holding no partial frame and no timecode.
    ///
    /// [`timecode`](Self::timecode) stays `None` until eight in-order
    /// quarter-frames have arrived.
    pub fn new() -> Self {
        Self {
            nibbles: [0; 8],
            count: 0,
            last_piece: None,
            timecode: None,
        }
    }

    /// Feeds a quarter-frame data byte (the data byte from 0xF1 messages).
    ///
    /// The upper nibble (bits 4-6) identifies the piece (0-7).
    /// The lower nibble (bits 0-3) carries 4 bits of timecode data.
    ///
    /// Pieces must arrive in order. A gap discards the partial frame and waits
    /// for the next piece 0 rather than publishing a timecode assembled from two
    /// different frames — so a dropped byte costs one frame of position, not a
    /// wrong one. [`timecode`](Self::timecode) keeps its previous value across a
    /// discarded frame.
    ///
    /// Allocation-free, so it is safe to call from the audio thread.
    pub fn feed(&mut self, quarter_frame: u8) {
        let piece = (quarter_frame >> 4) & 0x07;
        let nibble = quarter_frame & 0x0F;

        // Pieces must arrive strictly in order 0,1,…,7. Piece 0 starts a fresh
        // frame; any other piece that isn't exactly one past the last (or that
        // is the first seen) is out of sequence: discard the partial frame and
        // wait for the next piece 0.
        if piece == 0 {
            self.count = 0;
        } else if self.last_piece != Some(piece - 1) {
            self.last_piece = Some(piece);
            self.count = 0;
            return;
        }

        self.last_piece = Some(piece);
        self.nibbles[piece as usize] = nibble;
        self.count += 1;

        if self.count >= 8 {
            self.assemble();
            self.count = 0;
        }
    }

    fn assemble(&mut self) {
        let n = &self.nibbles;

        let frames = n[0] | (n[1] << 4);
        let seconds = n[2] | (n[3] << 4);
        let minutes = n[4] | (n[5] << 4);
        let hours_low = n[6];
        let hours_high_and_rate = n[7];

        let hours = hours_low | ((hours_high_and_rate & 0x01) << 4);
        let rate_bits = (hours_high_and_rate >> 1) & 0x03;

        let frame_rate = match rate_bits {
            0 => SmpteFrameRate::Fps24,
            1 => SmpteFrameRate::Fps25,
            2 => SmpteFrameRate::Fps2997Df,
            _ => SmpteFrameRate::Fps30,
        };

        self.timecode = Some(SmpteTimecode {
            hours,
            minutes,
            seconds,
            frames,
            frame_rate,
        });
    }

    /// The most recently completed timecode, or `None` before the first full
    /// frame has been assembled.
    ///
    /// MTC transmits one full position per *two* frames, so this lags the sender
    /// by up to two frames even when every byte arrives.
    pub fn timecode(&self) -> Option<SmpteTimecode> {
        self.timecode
    }

    /// Drops the partial frame and the last completed timecode.
    ///
    /// Call this on a locate or a transport stop: without it,
    /// [`timecode`](Self::timecode) keeps reporting the old position until a
    /// whole new frame arrives.
    pub fn reset(&mut self) {
        self.nibbles = [0; 8];
        self.count = 0;
        self.last_piece = None;
        self.timecode = None;
    }
}

impl Default for MtcDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
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
    // Mutation: restore `last_piece: u8 = 0xFF` with `last_piece + 1` and the
    // first feed panics on overflow.
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
        assert_eq!((tc.hours, tc.minutes, tc.seconds, tc.frames), (1, 2, 3, 4));
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
        // (h, m, s, f, rate bits, expected rate)
        let cases = [
            (1u8, 2u8, 3u8, 4u8, 0u8, SmpteFrameRate::Fps24),
            (10, 30, 0, 0, 1, SmpteFrameRate::Fps25),
            (0, 0, 0, 0, 2, SmpteFrameRate::Fps2997Df),
            (1, 2, 3, 4, 3, SmpteFrameRate::Fps30),
            // A frame number needing both nibbles, so the high-nibble piece is
            // not silently dropped.
            (0, 0, 1, 23, 0, SmpteFrameRate::Fps24),
        ];

        for (h, m, s, f, rate_bits, expected_rate) in cases {
            let mut decoder = MtcDecoder::new();
            feed_timecode(&mut decoder, h, m, s, f, rate_bits);

            let tc = decoder.timecode().expect("eight pieces is a full timecode");
            assert_eq!(
                (tc.hours, tc.minutes, tc.seconds, tc.frames),
                (h, m, s, f),
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
        assert_eq!((tc.hours, tc.minutes, tc.seconds, tc.frames), (1, 2, 3, 4));
    }

    #[test]
    fn test_to_seconds() {
        let tc = SmpteTimecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 0,
            frame_rate: SmpteFrameRate::Fps30,
        };
        assert!((tc.to_seconds() - 3600.0).abs() < 0.001);
    }

    // Mutation: drop the `2 * (minutes - minutes / 10)` term (or the `/ 10`)
    // in `frame_count`, or the `18 * tens` term in `from_frame_count`: the
    // labels at the minute marks fail.
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

    // Mutation: drop the `frames.max(2)` and `00:01:00:00` counts as frame 1798.
    #[test]
    fn invalid_drop_frame_labels_count_as_the_next_valid_one() {
        let at = |m, s, f| SmpteTimecode {
            hours: 0,
            minutes: m,
            seconds: s,
            frames: f,
            frame_rate: SmpteFrameRate::Fps2997Df,
        };
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
            (tc.to_seconds() - 600.0).abs() < 0.001,
            "{}",
            tc.to_seconds()
        );
    }

    #[test]
    fn test_to_beats() {
        let tc = SmpteTimecode {
            hours: 0,
            minutes: 0,
            seconds: 60,
            frames: 0,
            frame_rate: SmpteFrameRate::Fps30,
        };
        // 60 seconds at 120 BPM = 120 beats
        assert!((tc.to_beats(120.0) - 120.0).abs() < 0.001);
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
        assert_eq!(tc1.frames, 0);

        feed_timecode(&mut decoder, 0, 0, 0, 1, 3);
        let tc2 = decoder.timecode().unwrap();
        assert_eq!(tc2.frames, 1);
    }

    #[test]
    fn test_hours_above_15() {
        let mut decoder = MtcDecoder::new();
        // Hour 23: low nibble = 7 (0x07), bit4 = 1
        feed_timecode(&mut decoder, 23, 59, 59, 29, 3);
        let tc = decoder.timecode().unwrap();
        assert_eq!(tc.hours, 23);
        assert_eq!(tc.minutes, 59);
        assert_eq!(tc.seconds, 59);
        assert_eq!(tc.frames, 29);
    }
}
