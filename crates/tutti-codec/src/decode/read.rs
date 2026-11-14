//! Decoding a file into a [`Wave`], and reading its header without decoding.
//!
//! The entry points are [`Wave::read`], [`Wave::read_metadata`] and
//! [`Wave::from_bytes`]. Decoded samples are pinned bit-for-bit by
//! `tests/decode_golden.rs` against committed fixtures.

use std::fs::File;
use std::path::Path;
use tutti_types::{SampleRate, Samples};

use symphonia::core::audio::AudioSpec;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_VORBIS;
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, CODEC_ID_NULL_AUDIO,
};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, Track};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::packet::Packet;

use super::error::convert;
use super::placement::{topology_of, unstated};
use crate::DecodeError;
use crate::Wave;
use tutti_types::ChannelTopology;

/// Container metadata read from an audio file without decoding any audio.
///
/// Returned by [`Wave::read_metadata`]. The fields are the container's raw
/// answers (the rate it states), and the topology it names, the one a full read would
/// give the [`Wave`]. It carries a [`ChannelTopology`], which owns its list of
/// speakers, so this is `Clone` and not `Copy`.
#[derive(Debug, Clone, PartialEq)]
pub struct WaveMetadata {
    /// Total sample frames, if the container reports it (`None` for some
    /// streamed/VBR formats that don't store a frame count).
    pub total_frames: Option<u64>,
    /// The file's sample rate. A container that states none, or 0, is
    /// [`DecodeError::NoSampleRate`].
    pub sample_rate: SampleRate,
    /// Which speaker each channel feeds, in the order the samples are in (as
    /// [`Wave::topology`] and [`FileIn::topology`](crate::FileIn::topology)
    /// give it): a lone channel is the centre, a speaker the vocabulary has no
    /// name for is [`Speaker::Unknown`](tutti_types::Speaker::Unknown), and a
    /// container that does not say is stereo. A caller deciding whether a file
    /// can be placed asks this before it loads or opens the file.
    pub topology: ChannelTopology,
}

impl WaveMetadata {
    /// The file's channel count: the width of its [`topology`](Self::topology).
    #[must_use]
    pub fn channels(&self) -> usize {
        self.topology.positions().len()
    }
}

/// A decoder for one audio track, producing planar `f32` a packet at a time.
///
/// Gapless trimming is off, so it hands back every frame its codec decodes,
/// delay and padding included. A Vorbis stream's first packet after a reset
/// only primes the codec's overlap: it decodes to no frames.
pub(super) struct PacketDecoder {
    decoder: Box<dyn AudioDecoder>,
    /// Whether the codec's first packet after a reset decodes to no frames.
    primes: bool,
    /// Whether a packet has decoded since the last reset.
    primed: bool,
    /// The last packet's frames, one vector per channel.
    planes: Vec<Vec<f32>>,
}

impl PacketDecoder {
    /// A decoder for the track whose parameters are `params`.
    pub(super) fn new(params: &AudioCodecParameters) -> Result<Self, Error> {
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default().gapless(false))?;
        Ok(Self {
            decoder,
            primes: params.codec == CODEC_ID_VORBIS,
            primed: false,
            planes: Vec::new(),
        })
    }

    /// Forgets the codec's overlap: the next packet is not continuous with the
    /// last one, as after a seek.
    pub(super) fn reset(&mut self) {
        self.decoder.reset();
        self.primed = false;
    }

    /// Decodes one packet into [`planes`](Self::planes), reusing the vectors
    /// across calls to avoid per-packet allocation. Returns the frame count,
    /// which every plane holds. This is the single decode-a-packet primitive
    /// shared by the whole-file load and [`FileIn`](crate::FileIn): given a
    /// packet already known to belong to the selected track.
    pub(super) fn decode(&mut self, packet: &Packet) -> Result<usize, Error> {
        let decoded = self.decoder.decode(packet)?;
        let priming = self.primes && !self.primed;
        self.primed = true;
        if priming {
            self.planes.clear();
            return Ok(0);
        }
        decoded.copy_to_vecs_planar::<f32>(&mut self.planes);
        Ok(decoded.frames())
    }

    /// The frames of the last packet, one slice per channel.
    pub(super) fn planes(&self) -> &[Vec<f32>] {
        &self.planes
    }

    /// The format of the last decoded packet.
    pub(super) fn spec(&self) -> AudioSpec {
        self.decoder.last_decoded().spec().clone()
    }
}

/// The frame count of a track's whole decode: the container's count of valid
/// frames with the delay and padding frames the decoder also returns. A codec
/// whose first packet only primes (Vorbis) returns no frames for the delay,
/// which is that packet.
pub(super) fn total_frames_of(track: &Track, params: &AudioCodecParameters) -> Option<u64> {
    let delay = if params.codec == CODEC_ID_VORBIS {
        0
    } else {
        track.delay.unwrap_or(0)
    };
    track
        .num_frames
        .map(|valid| valid + u64::from(delay) + u64::from(track.padding.unwrap_or(0)))
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
    symphonia::default::get_probe().probe(
        hint,
        stream,
        FormatOptions::default(),
        MetadataOptions::default(),
    )
}

/// The first track with a known audio codec, with its audio parameters: the
/// one every loader here decodes.
pub(super) fn first_audio_track(
    reader: &dyn FormatReader,
) -> Result<(&Track, &AudioCodecParameters), DecodeError> {
    reader
        .tracks()
        .iter()
        .find_map(|track| match &track.codec_params {
            Some(CodecParameters::Audio(params)) if params.codec != CODEC_ID_NULL_AUDIO => {
                Some((track, params))
            }
            _ => None,
        })
        .ok_or(DecodeError::NoAudioTrack)
}

/// The track's sample rate, as the container states it. A container that
/// states none is refused, never given a rate of ours.
///
/// A rate of 0 states none.
pub(super) fn sample_rate_of(params: &AudioCodecParameters) -> Result<SampleRate, DecodeError> {
    params
        .sample_rate
        .and_then(SampleRate::from_hz)
        .ok_or(DecodeError::NoSampleRate)
}

impl Wave {
    /// Decodes the first audio track of the file at `path` into memory.
    ///
    /// Requires a decode feature (`decode-wav`, `decode-flac`, `decode-mp3` or
    /// `decode-ogg`). Supported formats are whatever those enable; see
    /// [`decodable_extensions`](crate::decodable_extensions). The wave keeps
    /// the file's own sample rate and channel count; nothing is resampled. Its
    /// [`topology`](Wave::topology) is what the container names, in the order
    /// the samples are in: a mono file is the centre, a channel that names no
    /// position is [`Speaker::Unassigned`](tutti_types::Speaker::Unassigned), and
    /// a position the vocabulary has no name for is
    /// [`Speaker::Unknown`](tutti_types::Speaker::Unknown). Reads and
    /// decodes the whole file on the calling thread, so do not call it on the
    /// audio thread.
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] if the file cannot be opened, its container is not one
    /// this build reads, it has no audio track, the track states no sample
    /// rate ([`DecodeError::NoSampleRate`]), or a packet fails to decode.
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
    /// this build reads, it has no audio track, or the track states no sample
    /// rate ([`DecodeError::NoSampleRate`]).
    pub fn read_metadata(path: impl AsRef<Path>) -> Result<WaveMetadata, DecodeError> {
        let reader = probe_path(path.as_ref()).map_err(convert)?;
        let (track, params) = first_audio_track(&*reader)?;
        Ok(WaveMetadata {
            total_frames: total_frames_of(track, params),
            sample_rate: sample_rate_of(params)?,
            topology: params.channels.as_ref().map_or_else(unstated, topology_of),
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
    let (track, params) = first_audio_track(&*reader)?;
    let track_id = track.id;

    let total_frames = total_frames_of(track, params);
    let sample_rate = sample_rate_of(params)?;
    let mut decoder = PacketDecoder::new(params).map_err(convert)?;
    let topology = params.channels.as_ref().map_or_else(unstated, topology_of);

    // Pre-allocate using the metadata frame count when available (WAV, FLAC).
    let mut wave: Option<Wave> = total_frames.map(|total| {
        let mut wave = Wave::with_topology(topology, sample_rate);
        for ch in 0..wave.channels() {
            wave.channel_vec_mut(ch).reserve_exact(total as usize);
        }
        wave
    });

    loop {
        let packet = match reader.next_packet() {
            Ok(Some(packet)) => packet,
            // A clean end of stream. A file with no packet at all is empty, not a
            // wave.
            Ok(None) => return wave.ok_or_else(end_of_stream),
            // Any read error ends the stream.
            Err(err) => return wave.ok_or_else(|| convert(err)),
        };

        // A packet of another track is not ours to decode.
        if packet.track_id != track_id {
            continue;
        }

        let buffer_len = decoder.decode(&packet).map_err(convert)?;
        let wave_output = match &mut wave {
            Some(wave) => wave,
            none => {
                let spec = decoder.spec();
                let rate = SampleRate::from_hz(spec.rate()).ok_or(DecodeError::NoSampleRate)?;
                none.insert(Wave::with_topology(topology_of(spec.channels()), rate))
            }
        };

        // Batch-append all channels at once.
        let old_len = wave_output.len();
        for (ch, plane) in decoder.planes().iter().enumerate() {
            wave_output.channel_vec_mut(ch).extend_from_slice(plane);
        }
        wave_output.set_len(old_len + Samples(buffer_len));
    }
}

/// What a stream with no packet at all is refused with.
pub(super) fn end_of_stream() -> DecodeError {
    DecodeError::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "end of stream",
    ))
}

#[cfg(all(test, feature = "decode-wav"))]
mod tests;
