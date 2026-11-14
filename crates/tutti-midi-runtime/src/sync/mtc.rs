//! MIDI Time Code: assembling quarter-frame messages into SMPTE positions.
//!
//! MTC transmits a wall-clock position one nibble at a time, eight messages per
//! two frames, so a decoder is a small state machine rather than a parse
//! function — [`MtcDecoder`] holds the partial frame between bytes.
//!
//! This is the *wall-clock* half of MIDI sync; the musical half (beat position
//! and inferred tempo) is [`MidiClockDecoder`](crate::MidiClockDecoder). A device sends one or the
//! other, and which you decode is decided by the source, not by preference.

use tutti_midi_protocol::{MtcRate, SmpteFrameRate, SmpteTimecode};
use tutti_ump::{Message, System, Ump};

/// Frames the eight pieces of a cycle take to send, and so how old the
/// assembled position is when it completes (MTC, Quarter Frame Message
/// Implementation).
const CYCLE_FRAMES: u64 = 2;

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

    /// Feeds one inbound [`Ump`], as [`MidiClockDecoder::feed`](crate::MidiClockDecoder::feed)
    /// does: a MTC Quarter Frame (0xF1) is taken, any other packet is ignored.
    /// Returns `true` if the packet was a Quarter Frame. It takes no time:
    /// the position is carried in the pieces, not in when they arrived.
    ///
    /// A quarter frame's upper nibble (bits 4-6) identifies the piece (0-7)
    /// and its lower nibble (bits 0-3) carries 4 bits of timecode data.
    ///
    /// Pieces must arrive in order. A gap discards the partial frame and waits
    /// for the next piece 0 rather than publishing a timecode assembled from two
    /// different frames — so a dropped byte costs one frame of position, not a
    /// wrong one. [`timecode`](Self::timecode) keeps its previous value across a
    /// discarded frame.
    ///
    /// Allocation-free, so it is safe to call from the audio thread.
    pub fn feed(&mut self, ump: &Ump) -> bool {
        let Ok(Message::System(_, System::TimeCode(code))) = (*ump).decode() else {
            return false;
        };
        self.piece(code.get());
        true
    }

    fn piece(&mut self, quarter_frame: u8) {
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

        // The bits the spec reserves are not read: a transmitter sets them to 0
        // and a receiver ignores them (MTC, Quarter Frame Messages): the top three bits of the
        // frame field, the top two of seconds and of minutes, and the top bit
        // of the hours nibble pair (bit 3 of piece 7, above the rate code).
        let frames = n[0] | ((n[1] & 0x01) << 4);
        let seconds = n[2] | ((n[3] & 0x03) << 4);
        let minutes = n[4] | ((n[5] & 0x03) << 4);
        let hours_low = n[6];
        let hours_high_and_rate = n[7];

        let hours = hours_low | ((hours_high_and_rate & 0x01) << 4);
        let rate_bits = (hours_high_and_rate >> 1) & 0x03;

        let frame_rate = SmpteFrameRate::from(MtcRate::masked(rate_bits));

        // A position the wire framed but no clock reads (hour 28, frame 31) is
        // a corrupt frame: it is dropped, and `timecode` keeps its last value.
        if let Some(timecode) = SmpteTimecode::new(hours, minutes, seconds, frames, frame_rate) {
            // The eight pieces took two frames to send, so the position they
            // describe is two frames old when the last arrives, so the MTC spec has a
            // receiver keep an internal offset of +2 frames. The decoder keeps it, so `timecode` is the time of
            // the last piece.
            self.timecode = Some(SmpteTimecode::from_frame_count(
                timecode.frame_count() + CYCLE_FRAMES,
                frame_rate,
            ));
        }
    }

    /// The most recently completed timecode, or `None` before the first full
    /// frame has been assembled.
    ///
    /// It is the time of the last quarter-frame that completed it: the eight
    /// pieces name the frame piece 0 fell on, and by the time the last
    /// quarter frame message (F1 7X) arrives the information is 2 frames old,
    /// so a receiver keeps an internal offset of +2 frames (MTC, Quarter Frame
    /// Messages). The decoder applies it (hours wrap at 24), so what the sender's label
    /// said plus two frames is reported here. It is for forward running time
    /// code, the only direction `feed` assembles. MTC transmits one full
    /// position per *two* frames, so this is current only at the instant the
    /// cycle completes.
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
