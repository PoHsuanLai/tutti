use crate::sync::SmpteFrameRate;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmpteTimecode {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    pub frame_rate: SmpteFrameRate,
}

impl SmpteTimecode {
    pub fn to_seconds(&self) -> f64 {
        f64::from(self.hours) * 3600.0
            + f64::from(self.minutes) * 60.0
            + f64::from(self.seconds)
            + f64::from(self.frames) / self.frame_rate.fps()
    }

    pub fn to_beats(&self, tempo_bpm: f64) -> f64 {
        self.to_seconds() * tempo_bpm / 60.0
    }
}

/// Assembles 8 MTC quarter-frame messages into full SMPTE timecode.
///
/// MTC sends timecode as 8 sequential quarter-frame messages (0xF1 data),
/// each carrying a nibble of the full HH:MM:SS:FF timecode.
pub struct MtcDecoder {
    nibbles: [u8; 8],
    count: u8,
    last_piece: u8,
    timecode: Option<SmpteTimecode>,
}

impl MtcDecoder {
    pub fn new() -> Self {
        Self {
            nibbles: [0; 8],
            count: 0,
            last_piece: 0xFF,
            timecode: None,
        }
    }

    /// Feed a quarter-frame data byte (the data byte from 0xF1 messages).
    ///
    /// The upper nibble (bits 4-6) identifies the piece (0-7).
    /// The lower nibble (bits 0-3) carries 4 bits of timecode data.
    pub fn feed(&mut self, quarter_frame: u8) {
        let piece = (quarter_frame >> 4) & 0x07;
        let nibble = quarter_frame & 0x0F;

        // Pieces must arrive strictly in order 0,1,…,7. Piece 0 starts a
        // fresh frame; any other piece that isn't exactly one past the last
        // is out of sequence — discard the partial frame and wait for the
        // next piece 0. (Pieces never wrap here: piece 0 is handled above, so
        // `last_piece + 1` stays in 1..=7 and needs no masking.)
        if piece == 0 {
            self.count = 0;
        } else if piece != self.last_piece + 1 {
            self.last_piece = piece;
            self.count = 0;
            return;
        }

        self.last_piece = piece;
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

    pub fn timecode(&self) -> Option<SmpteTimecode> {
        self.timecode
    }

    pub fn reset(&mut self) {
        self.nibbles = [0; 8];
        self.count = 0;
        self.last_piece = 0xFF;
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

    #[test]
    fn test_assemble_full_timecode() {
        let mut decoder = MtcDecoder::new();
        // 01:02:03:04 at 30fps (rate bits = 3)
        feed_timecode(&mut decoder, 1, 2, 3, 4, 3);

        let tc = decoder.timecode().unwrap();
        assert_eq!(tc.hours, 1);
        assert_eq!(tc.minutes, 2);
        assert_eq!(tc.seconds, 3);
        assert_eq!(tc.frames, 4);
        assert_eq!(tc.frame_rate, SmpteFrameRate::Fps30);
    }

    #[test]
    fn test_24fps() {
        let mut decoder = MtcDecoder::new();
        feed_timecode(&mut decoder, 0, 0, 1, 23, 0);

        let tc = decoder.timecode().unwrap();
        assert_eq!(tc.frames, 23);
        assert_eq!(tc.frame_rate, SmpteFrameRate::Fps24);
    }

    #[test]
    fn test_25fps() {
        let mut decoder = MtcDecoder::new();
        feed_timecode(&mut decoder, 10, 30, 0, 0, 1);

        let tc = decoder.timecode().unwrap();
        assert_eq!(tc.hours, 10);
        assert_eq!(tc.minutes, 30);
        assert_eq!(tc.frame_rate, SmpteFrameRate::Fps25);
    }

    #[test]
    fn test_2997_drop_frame() {
        let mut decoder = MtcDecoder::new();
        feed_timecode(&mut decoder, 0, 0, 0, 0, 2);

        let tc = decoder.timecode().unwrap();
        assert_eq!(tc.frame_rate, SmpteFrameRate::Fps2997Df);
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
