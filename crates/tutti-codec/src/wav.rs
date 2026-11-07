//! WAV, written by hand over any `Write + Seek` sink.
//!
//! The format is a header and a run of little-endian samples, so this writes
//! both itself rather than through a library: the header carries the speaker
//! mask (`hound` writes the lowest-N-bits one and cannot change it) and the
//! sink stays the caller's to get back.
//!
//! Layout: a 44-byte `PCMWAVEFORMAT` header for at most two channels of at most
//! 16 bits, else a 68-byte `WAVE_FORMAT_EXTENSIBLE` one (which also names the
//! speakers). The RIFF and `data` sizes are written as zero first and
//! back-patched by [`finish`](FormatWriter::finish).
//!
//! A WAV file's sizes are 32-bit, so a file holds at most [`max_data_frames`]
//! frames (just under 4 GiB: about 62 minutes of 8-channel 24-bit audio at
//! 48 kHz). The write that would cross it writes the frames that fit and fails
//! with [`Error::WavTooLarge`]; [`finish`](FormatWriter::finish) then closes the file
//! at the last whole frame, which stays readable. RF64 is not written.

mod mask;

use crate::error::{Error, Result, SpecError};
use crate::pcm::{Pcm, Sample};
use crate::spec::Spec;
use crate::writer::FormatWriter;
use mask::{channel_mask, default_channel_mask};
use std::io::{Seek, SeekFrom, Write};
use tutti_types::{ChannelLayout, Interleaved};

const KSDATAFORMAT_SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// The largest header written before the data (the extensible one), less the 8
/// bytes the RIFF size excludes: the RIFF size field is `data + this` and must
/// also fit a `u32`.
const RIFF_HEADER_BYTES: u64 = 60;

/// The most data bytes a (non-RF64) WAV file can hold: both the `data` chunk
/// size and the RIFF size, which adds the header, are `u32`.
const MAX_DATA_BYTES: u64 = u32::MAX as u64 - RIFF_HEADER_BYTES;

/// The most whole frames a WAV file of `layout` at `pcm` can hold under
/// [`MAX_DATA_BYTES`]. A writer that stops here leaves a valid file ending on a
/// frame boundary.
pub(crate) fn max_data_frames(layout: ChannelLayout, pcm: Pcm) -> u64 {
    MAX_DATA_BYTES / frame_bytes(layout, pcm)
}

/// Bytes per frame: every sample in a whole number of bytes.
fn frame_bytes(layout: ChannelLayout, pcm: Pcm) -> u64 {
    u64::from(pcm.bits().div_ceil(8)) * u64::from(layout.count().max(1))
}

/// The header of a file of `spec`'s shape, and the offset of its `data` size.
struct Header {
    bytes: Vec<u8>,
    data_len_offset: usize,
}

impl Header {
    fn new(spec: &Spec, pcm: Pcm, rate: u32, mask: u32) -> Self {
        let channels = spec.layout.count();
        let sample_bytes = pcm.bits().div_ceil(8);
        let extensible = channels > 2 || pcm.bits() > 16;
        let float = pcm == Pcm::Float32;
        let bytes_per_sec = rate
            .wrapping_mul(u32::from(sample_bytes))
            .wrapping_mul(u32::from(channels));
        let block_align = sample_bytes.wrapping_mul(channels);

        let mut b = Vec::with_capacity(68);
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        if extensible {
            b.extend_from_slice(&40u32.to_le_bytes());
            b.extend_from_slice(&0xFFFEu16.to_le_bytes());
        } else {
            b.extend_from_slice(&16u32.to_le_bytes());
            // 16-bit integer: a float is 32 bits and always extensible.
            b.extend_from_slice(&1u16.to_le_bytes());
        }
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&bytes_per_sec.to_le_bytes());
        b.extend_from_slice(&block_align.to_le_bytes());
        if extensible {
            b.extend_from_slice(&(sample_bytes * 8).to_le_bytes());
            b.extend_from_slice(&22u16.to_le_bytes());
            b.extend_from_slice(&pcm.bits().to_le_bytes());
            b.extend_from_slice(&mask.to_le_bytes());
            b.extend_from_slice(if float {
                &KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            } else {
                &KSDATAFORMAT_SUBTYPE_PCM
            });
        } else {
            b.extend_from_slice(&pcm.bits().to_le_bytes());
        }
        b.extend_from_slice(b"data");
        let data_len_offset = b.len();
        b.extend_from_slice(&0u32.to_le_bytes());
        Self {
            bytes: b,
            data_len_offset,
        }
    }
}

/// Writes a WAV file through the [`FormatWriter`] contract.
///
/// A writer dropped without [`finish`](FormatWriter::finish) still back-patches
/// the sizes, best effort, so what was written stays a readable file (a
/// recording whose pump thread panicked is not lost). The data chunk is not
/// padded to an even length: odd-length data is left as it is, which is what
/// the `hound`-written files this replaces did.
pub struct WavWriter<W: Write + Seek> {
    /// `None` once `finish` has taken it.
    sink: Option<W>,
    pcm: Pcm,
    layout: ChannelLayout,
    header: Header,
    /// Whole frames written so far.
    frames: u64,
    /// The frames the file may hold: [`max_data_frames`], lowered by a caller.
    max_frames: u64,
    /// One block's samples as bytes, reused.
    bytes: Vec<u8>,
}

impl<W: Write + Seek> std::fmt::Debug for WavWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WavWriter").finish_non_exhaustive()
    }
}

impl<W: Write + Seek> WavWriter<W> {
    /// Starts the file in `sink`, writing its header.
    ///
    /// # Errors
    ///
    /// As [`Encoder::create`](crate::Encoder::create).
    pub fn create(spec: &Spec, pcm: Pcm, mut sink: W) -> Result<Self> {
        let rate = spec.header_rate()?.get();
        let mask = match &spec.topology {
            Some(t) if t.layout() != spec.layout => {
                return Err(SpecError::TopologyWidth {
                    topology: t.layout(),
                    file: spec.layout,
                }
                .into());
            }
            Some(t) => channel_mask(t),
            None => default_channel_mask(spec.layout),
        };
        let header = Header::new(spec, pcm, rate, mask);
        sink.write_all(&header.bytes)?;
        Ok(Self {
            sink: Some(sink),
            pcm,
            layout: spec.layout,
            header,
            frames: 0,
            max_frames: max_data_frames(spec.layout, pcm),
            bytes: Vec::new(),
        })
    }

    /// Lowers the number of frames the file may hold to `frames`; it never
    /// raises the limit the format's 32-bit sizes set.
    pub fn limit_frames(&mut self, frames: u64) {
        self.max_frames = self.max_frames.min(frames);
    }

    /// Back-patches the RIFF and `data` sizes, leaves the sink at the end of the
    /// file and flushes it.
    fn patch_sizes(&mut self) -> std::io::Result<()> {
        let data = self.frames * frame_bytes(self.layout, self.pcm);
        // Both fit a `u32`: `max_frames` keeps the data under `MAX_DATA_BYTES`.
        let data = data as u32;
        let riff = data + (self.header.data_len_offset as u32 + 4 - 8);
        let at = self.header.data_len_offset as u64;
        let Some(sink) = self.sink.as_mut() else {
            return Ok(());
        };
        sink.seek(SeekFrom::Start(4))?;
        sink.write_all(&riff.to_le_bytes())?;
        sink.seek(SeekFrom::Start(at))?;
        sink.write_all(&data.to_le_bytes())?;
        sink.seek(SeekFrom::End(0))?;
        sink.flush()
    }
}

impl<W: Write + Seek> Drop for WavWriter<W> {
    fn drop(&mut self) {
        if self.sink.is_some() {
            let _ = self.patch_sizes();
        }
    }
}

impl<W: Write + Seek> FormatWriter for WavWriter<W> {
    type Sink = W;

    fn write(&mut self, block: Interleaved<'_>) -> Result<()> {
        let room = self.max_frames - self.frames;
        let fits = (block.len() as u64).min(room) as usize;
        let samples = &block.samples()[..fits * block.stride()];
        self.bytes.clear();
        for &s in samples {
            match self.pcm.quantize(s) {
                Sample::I16(v) => self.bytes.extend_from_slice(&v.to_le_bytes()),
                Sample::I24(v) => self.bytes.extend_from_slice(&v.to_le_bytes()[..3]),
                Sample::F32(v) => self.bytes.extend_from_slice(&v.to_le_bytes()),
            }
        }
        if let Some(sink) = self.sink.as_mut() {
            sink.write_all(&self.bytes)?;
        }
        self.frames += fits as u64;
        if fits < block.len() {
            return Err(Error::WavTooLarge {
                limit_frames: self.max_frames,
            });
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Self::Sink> {
        self.patch_sizes()?;
        Ok(self
            .sink
            .take()
            .expect("`finish` runs once, and takes the sink"))
    }
}

#[cfg(test)]
mod tests;
