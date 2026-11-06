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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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
mod tests;
