//! Decoding a file into a [`Wave`], and reading its header without decoding.
//!
//! Moved from the fundsp fork's `read.rs` by design doc 013, Phase 0, and
//! trimmed to what the engine calls: [`Wave::load`], [`Wave::probe_metadata`]
//! and, for Bevy, [`WaveAsset::from_bytes`]. The fork's progress-reporting,
//! streaming-peaks and explicit-track loaders had no caller outside it.
//!
//! The decode loop is the fork's, statement for statement, so decoded samples
//! are bit-identical; `tests/decode_golden.rs` pins that against committed
//! fixtures. Codec-gated: with no `wav`/`flac`/`mp3`/`ogg` feature there is no
//! symphonia and this module does not exist.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::{AudioBuffer, Signal};
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, Packet};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::Wave;

/// Why a file could not be probed or decoded.
///
/// symphonia's own error, named rather than wrapped: every variant is already
/// the distinction a caller wants (I/O, unsupported format, malformed data),
/// and the Bevy loader forwards it as its `#[source]`.
pub type WaveError = Error;

/// Container/codec metadata read from an audio file without decoding any
/// audio. Cheap, format-agnostic (anything symphonia can probe in this build),
/// and the single source of truth for "how long is this file" decisions.
///
/// The fields are the container's raw answers — `u32` rate, `usize` width —
/// and the sampler types them at its boundary (`tutti_sampler::probe`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaveMetadata {
    /// Total sample frames, if the container reports it (`None` for some
    /// streamed/VBR formats that don't store a frame count).
    pub total_frames: Option<u64>,
    /// The file's own rate.
    pub sample_rate: u32,
    /// The file's channel count.
    pub channels: usize,
}

/// Decode one packet into `dest`, reusing the buffer across calls to avoid
/// per-packet allocation. On success returns the filled buffer together with
/// its frame count. This is the single decode-a-packet primitive shared by the
/// whole-file load and [`FileIn`](crate::FileIn): given a packet already known
/// to belong to the selected track, it runs `decoder.decode` then `clear` /
/// `render_silence` / `convert` into the planar `AudioBuffer<f32>` that callers
/// read from.
pub(crate) fn decode_packet_into<'d>(
    decoder: &mut dyn Decoder,
    packet: &Packet,
    dest: &'d mut Option<AudioBuffer<f32>>,
) -> Result<(&'d AudioBuffer<f32>, usize), WaveError> {
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
/// [`Wave::probe_metadata`] stop here, [`Wave::load`] decodes on from it.
pub(crate) fn probe_path(path: &Path) -> Result<Box<dyn FormatReader>, WaveError> {
    let mut hint = Hint::new();
    if let Some(extension_str) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension_str);
    }
    let source: Box<dyn MediaSource> = Box::new(File::open(path).map_err(Error::IoError)?);
    probe_source(source, &hint)
}

fn probe_source(
    source: Box<dyn MediaSource>,
    hint: &Hint,
) -> Result<Box<dyn FormatReader>, WaveError> {
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
pub(crate) fn first_audio_track(
    reader: &dyn FormatReader,
) -> Result<&symphonia::core::formats::Track, WaveError> {
    reader
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or(Error::DecodeError("Could not find track."))
}

impl Wave {
    /// Decode the first audio track of the file at `path`.
    ///
    /// Supported formats are whatever this build's codec features enable; see
    /// [`decodable_extensions`](crate::decodable_extensions).
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Wave, WaveError> {
        decode(probe_path(path.as_ref())?)
    }

    /// Probe an audio file's metadata (frame count, sample rate, channels)
    /// without decoding any audio packets.
    ///
    /// The same probe head as [`Wave::load`], minus the decode loop — so it is
    /// fast and works for every format this build can decode. Use this for
    /// duration/size decisions instead of a format-specific header reader.
    pub fn probe_metadata<P: AsRef<Path>>(path: P) -> Result<WaveMetadata, WaveError> {
        let reader = probe_path(path.as_ref())?;
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

    /// Decode the first audio track of an in-memory file. No extension hint,
    /// so the container is identified from its bytes alone.
    #[cfg(feature = "bevy")]
    pub(crate) fn load_slice(bytes: Vec<u8>) -> Result<Wave, WaveError> {
        decode(probe_source(
            Box::new(std::io::Cursor::new(bytes)),
            &Hint::new(),
        )?)
    }
}

/// Decode the first audio track of a probed container to its end.
fn decode(mut reader: Box<dyn FormatReader>) -> Result<Wave, WaveError> {
    let track = first_audio_track(&*reader)?;
    let track_id = track.id;

    let total_frames = track.codec_params.n_frames;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100) as f64;
    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;
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
            // of file as an `IoError`, and this is the fork's rule unchanged.
            Err(err) => return wave.ok_or(err),
        };

        // A packet of another track is not ours to decode.
        if packet.track_id() != track_id {
            continue;
        }

        let (buf, buffer_len) = decode_packet_into(&mut *decoder, &packet, &mut dest)?;
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

#[cfg(feature = "bevy")]
mod asset {
    use std::sync::Arc;

    use super::WaveError;
    use crate::Wave;

    /// Bevy asset wrapper around `Arc<Wave>`. Cheap to clone for use sites
    /// that share the decoded samples (a sampler voice takes `Arc<Wave>`).
    #[derive(Clone, bevy_asset::Asset, bevy_reflect::TypePath)]
    pub struct WaveAsset(pub Arc<Wave>);

    impl core::ops::Deref for WaveAsset {
        type Target = Wave;
        fn deref(&self) -> &Wave {
            &self.0
        }
    }

    impl WaveAsset {
        /// File extensions the Bevy asset loader recognises.
        pub const EXTENSIONS: &'static [&'static str] = &["wav", "flac", "mp3", "ogg"];

        /// Decode a complete wave from an in-memory byte slice.
        pub fn from_bytes(bytes: &[u8]) -> Result<Self, WaveError> {
            Wave::load_slice(bytes.to_vec()).map(|w| Self(Arc::new(w)))
        }
    }
}

#[cfg(feature = "bevy")]
pub use asset::WaveAsset;
