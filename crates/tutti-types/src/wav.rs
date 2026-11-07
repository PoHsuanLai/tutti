//! WAV container facts the two WAV writers share: the speaker mask a
//! multichannel file declares, and the size a WAV data chunk can reach.
//!
//! `hound` writes both wrongly for tutti's purposes. Its `WAVE_FORMAT_EXTENSIBLE`
//! header sets `dwChannelMask` to the lowest `channels` bits (it has no API to
//! change it), which a mask-honouring reader takes as FL FR FC LFE ... rather than
//! tutti's order. And it counts data bytes in a `u32` with no guard, so a file
//! past 4 GiB wraps its header. A live sink and an offline encoder can
//! both use this module, so they cannot disagree.
//!
//! # The mask
//!
//! A WAVE file stores channels in ascending speaker-bit order, so a mask can
//! describe a [`ChannelTopology`] only when its speakers already are in that
//! order. [`channel_mask`] returns the mask of such a topology, and `0` (the
//! "unassigned" mask: the channels are anonymous rather than mislabelled) when
//! a speaker has no WAVE bit or the order differs. Samples are never
//! reordered, so file channel `n` is always the engine's channel `n`.
//!
//! A writer that is given only a width uses [`default_channel_mask`], which is
//! [`channel_mask`] of [`ChannelTopology::smpte`], so it never guesses where
//! the engine declines to: 1 channel is `0x4` (`KSAUDIO_SPEAKER_MONO`), 2 is
//! `0x3`, 6 is `0x60F` (`KSAUDIO_SPEAKER_5POINT1_SURROUND`), and every other
//! width is `0`, including 8 (tutti's 7.1 is `... SL SR BL BR`, WAVE's would be
//! `... BL BR SL SR`) and 4 (quad and B-format are both four channels).
//! Quad's `0x33` (`KSAUDIO_SPEAKER_QUAD`) needs the caller to pass
//! [`ChannelTopology::quad`].

use crate::pcm::BitDepth;
use crate::{ChannelLayout, ChannelTopology, Speaker};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// The `SPEAKER_*` bit of `speaker` in `ksmedia.h`, or `None` for a speaker
/// WAVE has no bit for.
const fn speaker_bit(speaker: Speaker) -> Option<u32> {
    Some(match speaker {
        Speaker::FrontLeft => 0x1,
        Speaker::FrontRight => 0x2,
        Speaker::FrontCenter => 0x4,
        Speaker::LowFrequency => 0x8,
        Speaker::BackLeft => 0x10,
        Speaker::BackRight => 0x20,
        Speaker::FrontLeftOfCenter => 0x40,
        Speaker::FrontRightOfCenter => 0x80,
        Speaker::BackCenter => 0x100,
        Speaker::SideLeft => 0x200,
        Speaker::SideRight => 0x400,
        Speaker::TopCenter => 0x800,
        Speaker::TopFrontLeft => 0x1000,
        Speaker::TopFrontCenter => 0x2000,
        Speaker::TopFrontRight => 0x4000,
        Speaker::TopBackLeft => 0x8000,
        Speaker::TopBackCenter => 0x10000,
        Speaker::TopBackRight => 0x20000,
        _ => return None,
    })
}

/// The speaker mask a WAV file declares for `topology`, or `0` (unassigned)
/// when a speaker has no WAVE bit or the speakers are not in ascending bit
/// order, which is the only order a WAVE file can state.
pub fn channel_mask(topology: &ChannelTopology) -> u32 {
    let mut mask = 0u32;
    for &speaker in topology.positions() {
        match speaker_bit(speaker) {
            Some(bit) if bit > mask => mask |= bit,
            _ => return 0,
        }
    }
    mask
}

/// [`channel_mask`] of the engine's own order for a bare width
/// ([`ChannelTopology::smpte`]), `0` for a width that has none.
#[must_use]
pub fn default_channel_mask(layout: ChannelLayout) -> u32 {
    ChannelTopology::smpte(layout).map_or(0, |t| channel_mask(&t))
}

/// File offset of `dwChannelMask` in a `WAVE_FORMAT_EXTENSIBLE` file whose
/// `fmt ` chunk follows `RIFF`/`WAVE` directly (12 + 8 header bytes, then 20
/// bytes into the format structure).
const MASK_OFFSET: u64 = 40;
/// File offset of `wFormatTag`.
const FORMAT_TAG_OFFSET: u64 = 20;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Rewrites the `dwChannelMask` of the WAV file in `file` to `mask`.
///
/// A header that is not `WAVE_FORMAT_EXTENSIBLE` (mono or stereo at 16 bit)
/// has no mask, and is left alone. Call it on a finalized file.
///
/// # Errors
///
/// The `io::Error` from reading or writing the header; a file shorter than
/// the extensible header reads as not extensible and is left alone.
pub fn patch_channel_mask<F: Read + Write + Seek>(file: &mut F, mask: u32) -> io::Result<()> {
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
    file.write_all(&mask.to_le_bytes())?;
    file.flush()
}

/// [`patch_channel_mask`] on the file at `path`.
///
/// # Errors
///
/// The `io::Error` from opening the file or patching it.
pub fn patch_channel_mask_at(path: impl AsRef<Path>, mask: u32) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    patch_channel_mask(&mut file, mask)
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

    /// The masks are the `ksmedia.h` constants. Quad comes only from a quad
    /// topology; a bare width of 4, 7.1 and unnamed widths are unassigned, and
    /// so are a topology out of ascending order or with a speaker WAVE has no
    /// bit for.
    ///
    /// Mutation: giving `default_channel_mask` a `4 => 0x33` arm (the count
    /// guess) fails the bare-4 assertion; making `channel_mask` skip the
    /// ascending check gives the swapped pair `0x3`, failing that assertion;
    /// SideLeft mapped to BackLeft's bit gives `0x3F` for 5.1.
    #[test]
    fn masks_match_ksmedia_and_are_zero_where_they_cannot_be_stated() {
        let m = |n: u16| default_channel_mask(ChannelLayout::from(n));
        assert_eq!(m(1), 0x4);
        assert_eq!(m(2), 0x3);
        assert_eq!(m(6), 0x60F);
        assert_eq!(m(4), 0, "a bare 4 may be quad or B-format");
        assert_eq!(
            m(8),
            0,
            "tutti's 7.1 is SL SR BL BR, WAVE's would be BL BR SL SR"
        );
        assert_eq!(m(3), 0);
        assert_eq!(m(12), 0);
        assert_eq!(channel_mask(&ChannelTopology::quad()), 0x33);
        assert_eq!(
            channel_mask(&ChannelTopology::smpte(ChannelLayout::from(6u16)).unwrap()),
            0x60F
        );
        use Speaker::*;
        assert_eq!(
            channel_mask(&ChannelTopology::new([FrontRight, FrontLeft])),
            0
        );
        assert_eq!(
            channel_mask(&ChannelTopology::new([FrontLeft, TopSideLeft])),
            0
        );
        assert_eq!(
            channel_mask(&ChannelTopology::new([FrontLeft, Unknown(3)])),
            0
        );
        assert_eq!(
            channel_mask(&ChannelTopology::new([FrontLeft, FrontLeft])),
            0
        );
    }

    /// Only the four mask bytes change, and only in an extensible header.
    ///
    /// Mutation: patching at offset 41, or patching without the tag check,
    /// fails the byte-for-byte comparison or the plain-header assertion.
    #[test]
    fn patching_rewrites_only_the_mask_of_an_extensible_header() {
        let before = extensible(6);
        let mut file = Cursor::new(before.clone());
        patch_channel_mask(&mut file, 0x60F).unwrap();
        let after = file.into_inner();
        let mut expect = before;
        expect[40..44].copy_from_slice(&0x60Fu32.to_le_bytes());
        assert_eq!(after, expect);

        let mut plain = extensible(2);
        plain[20..22].copy_from_slice(&1u16.to_le_bytes());
        let mut file = Cursor::new(plain.clone());
        patch_channel_mask(&mut file, 0x3).unwrap();
        assert_eq!(file.into_inner(), plain, "WAVE_FORMAT_PCM has no mask");

        let mut file = Cursor::new(vec![0u8; 10]);
        patch_channel_mask(&mut file, 0x3).unwrap();
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
