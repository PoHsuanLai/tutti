//! Decoding a file into a [`Wave`], and reading its header without decoding.
//!
//! The entry points are [`Wave::read`], [`Wave::read_metadata`] and, for
//! Bevy, `WaveAsset::from_bytes`. Decoded samples are pinned bit-for-bit by
//! `tests/decode_golden.rs` against committed fixtures. Codec-gated: with no `wav`/`flac`/`mp3`/`ogg` feature there is no
//! symphonia and this module does not exist.

//! Decoding a file into a [`Wave`], and reading its header without decoding.
//!
//! The entry points are [`Wave::read`], [`Wave::read_metadata`] and
//! [`Wave::from_bytes`]. Decoded samples are pinned bit-for-bit by
//! `tests/decode_golden.rs` against committed fixtures.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::{AudioBuffer, Signal};
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, Packet};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use super::error::{DecodeError, convert};
use crate::Wave;

/// Container metadata read from an audio file without decoding any audio.
///
/// Returned by [`Wave::read_metadata`]. The fields are the container's raw
/// answers (`u32` rate, `usize` width).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaveMetadata {
    /// Total sample frames, if the container reports it (`None` for some
    /// streamed/VBR formats that don't store a frame count).
    pub total_frames: Option<u64>,
    /// The file's sample rate in Hz; 44 100 if the container does not say.
    pub sample_rate: u32,
    /// The file's channel count; 2 if the container does not say.
    pub channels: usize,
}

/// Decode one packet into `dest`, reusing the buffer across calls to avoid
/// per-packet allocation. On success returns the filled buffer together with
/// its frame count. This is the single decode-a-packet primitive shared by the
/// whole-file load and [`FileIn`](crate::FileIn): given a packet already known
/// to belong to the selected track, it runs `decoder.decode` then `clear` /
/// `render_silence` / `convert` into the planar `AudioBuffer<f32>` that callers
/// read from.
pub(super) fn decode_packet_into<'d>(
    decoder: &mut dyn Decoder,
    packet: &Packet,
    dest: &'d mut Option<AudioBuffer<f32>>,
) -> Result<(&'d AudioBuffer<f32>, usize), Error> {
    let decoded = decoder.decode(packet)?;
    let buf = dest
        .get_or_insert_with(|| AudioBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec()));
    buf.clear();
    buf.render_silence(Some(decoded.frames()));
    decoded.convert(buf);
    let buffer_len = decoded.frames();
    Ok((buf, buffer_len))
}

/// Open `path` and probe its container, hinting the format from the extension.
///
/// The head every path-based entry point shares; `FileIn::open` and
/// [`Wave::read_metadata`] stop here, [`Wave::read`] decodes on from it.
pub(super) fn probe_path(path: &Path) -> Result<Box<dyn FormatReader>, Error> {
    let mut hint = Hint::new();
    if let Some(extension_str) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension_str);
    }
    let source: Box<dyn MediaSource> = Box::new(File::open(path).map_err(Error::IoError)?);
    probe_source(source, &hint)
}

fn probe_source(source: Box<dyn MediaSource>, hint: &Hint) -> Result<Box<dyn FormatReader>, Error> {
    let stream = MediaSourceStream::new(source, Default::default());
    let format_opts = FormatOptions {
        enable_gapless: false,
        ..Default::default()
    };
    let metadata_opts: MetadataOptions = Default::default();
    let probed =
        symphonia::default::get_probe().format(hint, stream, &format_opts, &metadata_opts)?;
    Ok(probed.format)
}

/// The first track with a known codec — the one every loader here decodes.
pub(super) fn first_audio_track(
    reader: &dyn FormatReader,
) -> Result<&symphonia::core::formats::Track, DecodeError> {
    reader
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or(DecodeError::NoAudioTrack)
}

impl Wave {
    /// Decodes the first audio track of the file at `path` into memory.
    ///
    /// Requires a decode feature (`decode-wav`, `decode-flac`, `decode-mp3` or
    /// `decode-ogg`). Supported formats are whatever those enable; see
    /// [`decodable_extensions`](crate::decodable_extensions). The wave keeps
    /// the file's own sample rate and channel count (44.1 kHz and 2 channels
    /// when the container states neither); nothing is resampled. Reads and
    /// decodes the whole file on the calling thread, so do not call it on the
    /// audio thread.
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] if the file cannot be opened, its container is not one
    /// this build reads, it has no audio track, or a packet fails to decode.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tutti_codec::Wave;
    ///
    /// let wave = Wave::read("kick.wav")?;
    /// println!("{} channels, {} s", wave.channels(), wave.duration());
    /// # Ok::<(), tutti_codec::DecodeError>(())
    /// ```
    pub fn read(path: impl AsRef<Path>) -> Result<Wave, DecodeError> {
        decode(probe_path(path.as_ref()).map_err(convert)?)
    }

    /// Reads an audio file's frame count, sample rate and channel count
    /// without decoding any audio.
    ///
    /// Works for every format [`Wave::read`] can read, and is fast enough for
    /// duration and size decisions before a full load.
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] if the file cannot be opened, its container is not one
    /// this build reads, or it has no audio track.
    pub fn read_metadata(path: impl AsRef<Path>) -> Result<WaveMetadata, DecodeError> {
        let reader = probe_path(path.as_ref()).map_err(convert)?;
        let track = first_audio_track(&*reader)?;
        Ok(WaveMetadata {
            total_frames: track.codec_params.n_frames,
            sample_rate: track.codec_params.sample_rate.unwrap_or(44100),
            channels: track
                .codec_params
                .channels
                .map(|ch| ch.count())
                .unwrap_or(2),
        })
    }

    /// Decodes the first audio track of a complete file held in memory. There
    /// is no extension hint, so the container is identified from the bytes
    /// alone.
    ///
    /// # Errors
    ///
    /// As [`Wave::read`].
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Wave, DecodeError> {
        decode(probe_source(Box::new(std::io::Cursor::new(bytes)), &Hint::new()).map_err(convert)?)
    }
}

/// Decode the first audio track of a probed container to its end.
fn decode(mut reader: Box<dyn FormatReader>) -> Result<Wave, DecodeError> {
    let track = first_audio_track(&*reader)?;
    let track_id = track.id;

    let total_frames = track.codec_params.n_frames;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100) as f64;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(convert)?;
    let channels = track
        .codec_params
        .channels
        .map(|ch| ch.count())
        .unwrap_or(2);

    // Pre-allocate using the metadata frame count when available (WAV, FLAC).
    let mut wave: Option<Wave> =
        total_frames.map(|total| Wave::with_capacity(channels, sample_rate, total as usize));

    // Reuse a single decode buffer across all packets to avoid per-packet
    // allocation.
    let mut dest: Option<AudioBuffer<f32>> = None;

    loop {
        let packet = match reader.next_packet() {
            Ok(packet) => packet,
            // Any read error ends the stream — symphonia reports a clean end
            // of file as an `IoError`.
            Err(err) => return wave.ok_or_else(|| convert(err)),
        };

        // A packet of another track is not ours to decode.
        if packet.track_id() != track_id {
            continue;
        }

        let (buf, buffer_len) =
            decode_packet_into(&mut *decoder, &packet, &mut dest).map_err(convert)?;
        let wave_output = wave.get_or_insert_with(|| {
            let spec = *buf.spec();
            Wave::new(spec.channels.count(), spec.rate as f64)
        });

        // Batch-append all channels at once.
        let num_ch = buf.spec().channels.count();
        let old_len = wave_output.len();
        for ch in 0..num_ch {
            wave_output
                .channel_vec_mut(ch)
                .extend_from_slice(&buf.chan(ch)[..buffer_len]);
        }
        wave_output.set_len(old_len + buffer_len);
    }
}

#[cfg(all(test, feature = "decode-wav"))]
mod tests;
