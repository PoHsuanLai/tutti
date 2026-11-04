//! WAV container facts the two WAV writers share: the speaker mask a
//! multichannel file declares, and the size a WAV data chunk can reach.
//!
//! `hound` writes both wrongly for tutti's purposes. Its `WAVE_FORMAT_EXTENSIBLE`
//! header sets `dwChannelMask` to the lowest `channels` bits (it has no API to
//! change it), which a mask-honouring reader takes as FL FR FC LFE ... rather than
//! tutti's order. And it counts data bytes in a `u32` with no guard, so a file
//! past 4 GiB wraps its header. The live sink (`tutti-io`'s `WavOut`) and the
//! offline encoder (`tutti-export`) both use this module, so they cannot
//! disagree.
//!
//! # The mask
//!
//! The speaker bits are the `SPEAKER_*` constants of `ksmedia.h` (checked
//! against the mingw-w64 copy), and a WAVE file stores channels in ascending
//! bit order. [`channel_mask`] returns a mask only when tutti's channel order
//! *is* that ascending order:
//!
//! | channels | tutti order | mask |
//! |---|---|---|
//! | 1 | C | `0x4` (`KSAUDIO_SPEAKER_MONO`) |
//! | 2 | FL FR | `0x3` |
//! | 4 | FL FR BL BR | `0x33` (`KSAUDIO_SPEAKER_QUAD`) |
//! | 6 | FL FR C LFE SL SR | `0x60F` (`KSAUDIO_SPEAKER_5POINT1_SURROUND`) |
//! | 8 | FL FR C LFE SL SR BL BR | `0`: ascending order would be `... BL BR SL SR` |
//! | other | not specified | `0` |
//!
//! `0` is the "unassigned" mask: a reader takes the channels as anonymous
//! rather than mislabelled. Reordering the samples of 7.1 on write is not done,
//! so a file's channel `n` is always the engine's channel `n`.

use crate::pcm::BitDepth;
use crate::ChannelLayout;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;
const SPEAKER_FRONT_CENTER: u32 = 0x4;
const SPEAKER_LOW_FREQUENCY: u32 = 0x8;
const SPEAKER_BACK_LEFT: u32 = 0x10;
const SPEAKER_BACK_RIGHT: u32 = 0x20;
const SPEAKER_SIDE_LEFT: u32 = 0x200;
const SPEAKER_SIDE_RIGHT: u32 = 0x400;

/// The speaker mask a WAV file declares for `layout`, or `0` (unassigned)
/// when tutti's channel order cannot be expressed as one. See the
/// [module docs](self) for the table.
pub const fn channel_mask(layout: ChannelLayout) -> u32 {
    match layout.count() {
        1 => SPEAKER_FRONT_CENTER,
        2 => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
        4 => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT | SPEAKER_BACK_LEFT | SPEAKER_BACK_RIGHT,
        6 => {
            SPEAKER_FRONT_LEFT
                | SPEAKER_FRONT_RIGHT
                | SPEAKER_FRONT_CENTER
                | SPEAKER_LOW_FREQUENCY
                | SPEAKER_SIDE_LEFT
                | SPEAKER_SIDE_RIGHT
        }
        _ => 0,
    }
}

/// File offset of `dwChannelMask` in a `WAVE_FORMAT_EXTENSIBLE` file whose
/// `fmt ` chunk follows `RIFF`/`WAVE` directly (12 + 8 header bytes, then 20
/// bytes into the format structure).
const MASK_OFFSET: u64 = 40;
/// File offset of `wFormatTag`.
const FORMAT_TAG_OFFSET: u64 = 20;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Rewrites the `dwChannelMask` of the WAV file in `file` to
/// [`channel_mask`]`(layout)`.
///
/// A header that is not `WAVE_FORMAT_EXTENSIBLE` (mono or stereo at 16 bit)
/// has no mask, and is left alone. Call it on a finalized file.
///
/// # Errors
///
/// The `io::Error` from reading or writing the header; a file shorter than
/// the extensible header reads as not extensible and is left alone.
pub fn patch_channel_mask<F: Read + Write + Seek>(
    file: &mut F,
    layout: ChannelLayout,
) -> io::Result<()> {
    file.seek(SeekFrom::Start(FORMAT_TAG_OFFSET))?;
    let mut tag = [0u8; 2];
    match file.read_exact(&mut tag) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(e) => return Err(e),
    }
    if u16::from_le_bytes(tag) != WAVE_FORMAT_EXTENSIBLE {
        return Ok(());
    }
    file.seek(SeekFrom::Start(MASK_OFFSET))?;
    file.write_all(&channel_mask(layout).to_le_bytes())?;
    file.flush()
}

/// [`patch_channel_mask`] on the file at `path`.
///
/// # Errors
///
/// The `io::Error` from opening the file or patching it.
pub fn patch_channel_mask_at(path: impl AsRef<Path>, layout: ChannelLayout) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    patch_channel_mask(&mut file, layout)
}

/// The largest header `hound` writes before the data (a
/// `WAVE_FORMAT_EXTENSIBLE` one), less the 8 bytes the RIFF size excludes: the
/// RIFF size field is `data + this` and must also fit a `u32`.
const RIFF_HEADER_BYTES: u64 = 60;

/// The most data bytes a (non-RF64) WAV file can hold: both the `data` chunk
/// size and the RIFF size, which adds the header, are `u32`.
pub const MAX_DATA_BYTES: u64 = u32::MAX as u64 - RIFF_HEADER_BYTES;

/// The most whole frames a WAV file of `layout` at `depth` can hold under
/// [`MAX_DATA_BYTES`]. A writer that stops here leaves a valid file ending on
/// a frame boundary.
pub fn max_data_frames(layout: ChannelLayout, depth: BitDepth) -> u64 {
    // `hound` stores every sample in a whole number of bytes.
    let sample_bytes = u64::from(depth.bits().div_ceil(8));
    let frame_bytes = sample_bytes * u64::from(layout.count().max(1));
    MAX_DATA_BYTES / frame_bytes
}

/// A write that would take a WAV file past what its 32-bit sizes can
/// describe. The file is finalized at the last whole frame under the limit
/// instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataLimitExceeded {
    /// The frames the file holds when it stops.
    pub limit_frames: u64,
}

impl core::fmt::Display for DataLimitExceeded {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "a WAV file cannot hold more than {} frames (its sizes are 32-bit, just under 4 GiB); \
             recording stopped at that frame",
            self.limit_frames
        )
    }
}

impl std::error::Error for DataLimitExceeded {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A 68-byte extensible header (mask at 40, tag 0xFFFE at 20), as `hound`
    /// writes it, with the mask `hound` would have picked.
    fn extensible(channels: u16) -> Vec<u8> {
        let mut h = vec![0u8; 68];
        h[20..22].copy_from_slice(&WAVE_FORMAT_EXTENSIBLE.to_le_bytes());
        h[22..24].copy_from_slice(&channels.to_le_bytes());
        h[40..44].copy_from_slice(&((1u32 << channels) - 1).to_le_bytes());
        h
    }

    /// The masks are the `ksmedia.h` constants, and 7.1 and unnamed widths are
    /// unassigned.
    ///
    /// Mutation: changing `SPEAKER_SIDE_LEFT` to `SPEAKER_BACK_LEFT` in the 6
    /// arm (the back-5.1 mask `0x3F`, what `hound` wrote) fails the `0x60F`
    /// assertion; giving 8 channels `0x63F` fails the 7.1 one.
    #[test]
    fn masks_match_ksmedia_and_are_zero_where_the_order_differs() {
        let m = |n: u16| channel_mask(ChannelLayout::from(n));
        assert_eq!(m(1), 0x4);
        assert_eq!(m(2), 0x3);
        assert_eq!(m(4), 0x33);
        assert_eq!(m(6), 0x60F);
        assert_eq!(
            m(8),
            0,
            "tutti's 7.1 is SL SR BL BR, WAVE's would be BL BR SL SR"
        );
        assert_eq!(m(3), 0);
        assert_eq!(m(12), 0);
    }

    /// Only the four mask bytes change, and only in an extensible header.
    ///
    /// Mutation: patching at offset 41, or patching without the tag check,
    /// fails the byte-for-byte comparison or the plain-header assertion.
    #[test]
    fn patching_rewrites_only_the_mask_of_an_extensible_header() {
        let before = extensible(6);
        let mut file = Cursor::new(before.clone());
        patch_channel_mask(&mut file, ChannelLayout::from(6u16)).unwrap();
        let after = file.into_inner();
        let mut expect = before;
        expect[40..44].copy_from_slice(&0x60Fu32.to_le_bytes());
        assert_eq!(after, expect);

        let mut plain = extensible(2);
        plain[20..22].copy_from_slice(&1u16.to_le_bytes());
        let mut file = Cursor::new(plain.clone());
        patch_channel_mask(&mut file, ChannelLayout::STEREO).unwrap();
        assert_eq!(file.into_inner(), plain, "WAVE_FORMAT_PCM has no mask");

        let mut file = Cursor::new(vec![0u8; 10]);
        patch_channel_mask(&mut file, ChannelLayout::STEREO).unwrap();
        assert_eq!(
            file.into_inner(),
            vec![0u8; 10],
            "a short file is left alone"
        );
    }

    /// The limit keeps both the data size and the RIFF size inside a `u32`,
    /// on a frame boundary.
    ///
    /// Mutation: dropping the `- RIFF_HEADER_BYTES` makes the RIFF size
    /// overflow for the 1-channel 16-bit case, failing the last assertion.
    #[test]
    fn the_frame_limit_fits_both_32_bit_sizes() {
        for (n, depth) in [
            (1u16, BitDepth::Int16),
            (2, BitDepth::Int24),
            (6, BitDepth::Float32),
            (8, BitDepth::Int24),
        ] {
            let layout = ChannelLayout::from(n);
            let frames = max_data_frames(layout, depth);
            let frame_bytes = u64::from(depth.bits().div_ceil(8)) * u64::from(n);
            let data = frames * frame_bytes;
            assert!(
                data + RIFF_HEADER_BYTES <= u64::from(u32::MAX),
                "{n} {depth:?}"
            );
            assert!(
                data + frame_bytes + RIFF_HEADER_BYTES > u64::from(u32::MAX),
                "{n} {depth:?}: one more frame must not fit"
            );
        }
    }
}
