//! The speaker bits of the WAVE channel mask, `ksmedia.h`'s `SPEAKER_*`.
//!
//! One table, read in both directions: the WAV encoder looks a speaker's bit
//! up for its `WAVE_FORMAT_EXTENSIBLE` mask, and the decoder maps a channel
//! flag back to its speaker (symphonia's flag values are WAVE's bits).

use tutti_types::Speaker;

/// Every speaker WAVE has a bit for, in ascending bit order.
const SPEAKER_BITS: [(u32, Speaker); 18] = [
    (0x1, Speaker::FrontLeft),
    (0x2, Speaker::FrontRight),
    (0x4, Speaker::FrontCenter),
    (0x8, Speaker::LowFrequency),
    (0x10, Speaker::BackLeft),
    (0x20, Speaker::BackRight),
    (0x40, Speaker::FrontLeftOfCenter),
    (0x80, Speaker::FrontRightOfCenter),
    (0x100, Speaker::BackCenter),
    (0x200, Speaker::SideLeft),
    (0x400, Speaker::SideRight),
    (0x800, Speaker::TopCenter),
    (0x1000, Speaker::TopFrontLeft),
    (0x2000, Speaker::TopFrontCenter),
    (0x4000, Speaker::TopFrontRight),
    (0x8000, Speaker::TopBackLeft),
    (0x10000, Speaker::TopBackCenter),
    (0x20000, Speaker::TopBackRight),
];

/// The bit of `speaker`, or `None` for a speaker WAVE has no bit for.
#[cfg(feature = "encode-wav")]
pub(crate) fn bit_of(speaker: Speaker) -> Option<u32> {
    SPEAKER_BITS
        .iter()
        .find_map(|&(bit, s)| (s == speaker).then_some(bit))
}

/// The speaker of the single-bit `flag`, or `None` for a bit with no speaker.
#[cfg(decode)]
pub(crate) fn speaker_of(flag: u32) -> Option<Speaker> {
    SPEAKER_BITS
        .iter()
        .find_map(|&(bit, s)| (bit == flag).then_some(s))
}
