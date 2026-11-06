//! Incremental disk streaming on top of symphonia's public API.
//!
//! [`FileIn`] decodes an audio file sequentially, frame by frame, without
//! loading the whole file into RAM. It is an [`AudioIn`]: [`poll_into`] fills a
//! caller buffer of flat interleaved `f32` **at the file's own channel width**
//! from the current cursor and reports how many **frames** it produced (a short
//! count then `0` at end-of-stream). [`layout`](AudioIn::layout) reports that
//! width, so a caller sizes its buffer from the trait and never has to ask the
//! concrete type.
//!
//! It is this crate's read edge, the counterpart of [`WavOut`](crate::WavOut).
//!
//! **It does not downmix.** A caller wanting stereo folds the frames itself
//! through [`tutti_types::fold_frame`], as every other engine edge does.
//!
//! To read from an
//! arbitrary position, call [`seek`] first — it hooks
//! `FormatReader::seek(SeekMode::Accurate, ...)` (which lands *before* the
//! requested frame) and decodes-and-discards the preroll to hit the exact frame,
//! so a following `poll_into` is a clean sequential read from there.
//!
//! [`poll_into`]: FileIn::poll_into
//! [`seek`]: FileIn::seek
//!
//! It shares [`decode`](crate::Wave::read)'s codec feature gates, its probe
//! head and its one-packet helper. All decode/seek/file I/O runs on the butler
//! thread; the audio thread never touches this type.

use std::path::Path;

use symphonia::core::audio::{AudioBuffer, Signal};
use symphonia::core::codecs::{CODEC_TYPE_FLAC, Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatReader, SeekMode, SeekTo};
use tutti_types::{ChannelLayout, SampleRate, Samples};
use tutti_types::{io::AudioIn, io::OnEmpty};

use crate::WaveError;
use crate::decode::{decode_packet_into, first_audio_track, probe_path};

/// A frame count from the file's `u64`, saturating where `usize` is narrower.
fn frames(n: u64) -> Samples {
    Samples(usize::try_from(n).unwrap_or(usize::MAX))
}

/// A copy of `error` for a call that reports a fault the stream already
/// holds: symphonia's error is not `Clone`.
fn repeat(error: &WaveError) -> WaveError {
    match error {
        Error::IoError(io) => Error::IoError(std::io::Error::new(io.kind(), error.to_string())),
        Error::DecodeError(msg) => Error::DecodeError(msg),
        Error::Unsupported(msg) => Error::Unsupported(msg),
        Error::LimitError(msg) => Error::LimitError(msg),
        _ => Error::DecodeError("the stream failed earlier; see FileIn::error"),
    }
}

/// What [`FileIn::error`] reports when the stream ends before the container's
/// frame count.
const TRUNCATED: &str = "stream ended before the container's frame count";

/// A streaming decoder over an audio file's first audio track, as an
/// [`AudioIn`].
///
/// Decodes sequentially without loading the whole file into memory (compare
/// [`Wave::read`](crate::Wave::read)). [`poll_into`](AudioIn::poll_into) fills a
/// flat interleaved `f32` buffer **at the file's own channel width** and
/// returns the frames produced; a short count and then `0` mark the end of the
/// file. It does not downmix: a caller wanting stereo folds each frame through
/// [`tutti_types::fold_frame`].
///
/// [`seek`](Self::seek) positions it at an exact frame. Decoding, seeking and
/// file I/O all block, so use it from a loader or streaming thread, never the
/// audio thread. Sequential reads reuse their decode buffers across calls.
/// Requires a codec feature.
///
/// # Examples
///
/// ```no_run
/// use tutti_io::{AudioIn, FileIn};
/// use tutti_types::Samples;
///
/// let mut file = FileIn::open("take.flac")?;
/// let width = usize::from(file.layout().count());
/// let mut buf = vec![0.0f32; 1024 * width];
/// file.seek(Samples(48_000))?; // start one second in, at 48 kHz
/// let frames = file.poll_into(&mut buf);
/// println!("read {} frames of {width} channels", frames.get());
/// # Ok::<(), tutti_io::WaveError>(())
/// ```
pub struct FileIn {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    layout: ChannelLayout,
    sample_rate: SampleRate,
    total_frames: Option<u64>,
    /// Scratch buffer reused by `decode_packet_into` (allocated on first decode).
    convert_buf: Option<AudioBuffer<f32>>,
    /// Decoded-but-unconsumed frames from the last packet, interleaved at the
    /// file's own width. Frame `f` channel `c` is at `leftover[f * width + c]`.
    leftover: Vec<f32>,
    /// Read offset into `leftover` in frames; `[leftover_pos..]` is unconsumed.
    leftover_pos: usize,
    /// Next file sample-frame a sequential read produces.
    cursor: u64,
    /// Whether the container reports a frame count and supports accurate seek.
    seekable: bool,
    /// Timestamp of the first zero-frame packet the latest
    /// [`decode_next_packet`](Self::decode_next_packet) call skipped, if any —
    /// the decoder primer. [`seek`](Self::seek) steps back past it when the
    /// frames it swallowed include the one asked for.
    skipped_primer: Option<u64>,
    /// The decode error that ended a [`poll_into`](AudioIn::poll_into) stream
    /// early, kept for [`error`](Self::error). Sticky until a
    /// [`seek`](Self::seek) succeeds.
    fault: Option<WaveError>,
    /// Whether `total_frames` is exact, so a stream that ends short of it is
    /// damaged. Only FLAC's is (its STREAMINFO count); MP3 delay and padding,
    /// Vorbis priming and a WAV header's data length can all disagree with
    /// the frames a file really holds.
    exact_length: bool,
}

impl std::fmt::Debug for FileIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileIn")
            .field("track_id", &self.track_id)
            .field("layout", &self.layout)
            .field("sample_rate", &self.sample_rate)
            .field("total_frames", &self.total_frames)
            .field("leftover_frames", &(self.leftover.len() / self.width()))
            .field("leftover_pos", &self.leftover_pos)
            .field("cursor", &self.cursor)
            .field("seekable", &self.seekable)
            .field("skipped_primer", &self.skipped_primer)
            .field("faulted", &self.fault.is_some())
            .finish_non_exhaustive()
    }
}

impl FileIn {
    /// Returns the file's length in frames, if the container reports it.
    pub fn total_frames(&self) -> Option<Samples> {
        self.total_frames.map(frames)
    }

    /// Returns the file's sample rate (44 100 Hz if the container does not
    /// say). Nothing is resampled.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// The interleave stride: the file's channel count.
    fn width(&self) -> usize {
        usize::from(self.layout.count())
    }

    /// Returns whether [`seek`](Self::seek) can serve arbitrary positions:
    /// `true` when the container reports a frame count. When `false`, load the
    /// whole file with [`Wave::read`](crate::Wave::read) instead.
    pub fn seekable(&self) -> bool {
        self.seekable
    }

    /// Opens `path` at its first audio track, positioned at frame 0.
    ///
    /// Probes the container and creates a decoder; no audio is decoded yet.
    /// It reads the same track [`Wave::read`](crate::Wave::read) decodes.
    ///
    /// # Errors
    ///
    /// A [`WaveError`] if the file cannot be opened, its container is not one
    /// this build reads, or it has no audio track this build can decode.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, WaveError> {
        let reader = probe_path(path.as_ref())?;
        let track = first_audio_track(&*reader)?;
        let track_id = track.id;

        let total_frames = track.codec_params.n_frames;
        let sample_rate = SampleRate::from(track.codec_params.sample_rate.unwrap_or(44100));
        // At least one channel, so the interleave stride is never zero. Untested:
        // no container this build reads yields a zero-channel track, and a
        // fixture cannot be built or stubbed (symphonia's `Channels` come from
        // the demuxer).
        let layout = ChannelLayout::from(
            track
                .codec_params
                .channels
                .map_or(2, |ch| ch.count().max(1)),
        );

        let decode_opts = DecoderOptions::default();
        let decoder = symphonia::default::get_codecs().make(&track.codec_params, &decode_opts)?;

        // Seekable only when the container reports a frame count. Formats
        // without n_frames (some VBR/streamed) fall back to whole-file load.
        let seekable = total_frames.is_some();
        let exact_length = track.codec_params.codec == CODEC_TYPE_FLAC;

        // Reserve leftover for a typical packet's worth of frames so
        // sequential reads don't reallocate after warmup. WAV packets are
        // small (~a few thousand frames); FLAC blocks up to ~4608.
        let leftover = Vec::with_capacity(1 << 14);

        Ok(Self {
            reader,
            decoder,
            track_id,
            layout,
            sample_rate,
            total_frames,
            convert_buf: None,
            leftover,
            leftover_pos: 0,
            cursor: 0,
            seekable,
            skipped_primer: None,
            fault: None,
            exact_length,
        })
    }

    /// Unconsumed retained frames.
    #[inline]
    fn leftover_len(&self) -> usize {
        (self.leftover.len() / self.width()).saturating_sub(self.leftover_pos)
    }

    /// Refill `leftover` from the next packet of the selected track that
    /// decodes to audio. Returns the number of frames decoded, which is 0 only
    /// at EOF, and the timestamp (`packet.ts()`, in frames) of the packet that
    /// produced them. Retains a `convert_buf` scratch to stay allocation-free
    /// after warmup.
    ///
    /// A packet can decode to **zero** frames without the stream ending —
    /// Vorbis's first packet, and its first after a `reset`, only prime the
    /// decoder's overlap — and both callers read a 0 as end-of-stream, so such
    /// a packet is skipped here rather than returned (returning it would make
    /// every Ogg file stream as empty). The skip is also why the timestamp comes back:
    /// the frames in `leftover` start at the returned packet's `ts`, not at
    /// the first packet read, and [`seek`](Self::seek) must count its preroll
    /// from there.
    fn decode_next_packet(&mut self) -> Result<(usize, u64), WaveError> {
        self.skipped_primer = None;
        loop {
            let packet = match self.reader.next_packet() {
                Ok(p) => p,
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok((0, 0));
                }
                Err(e) => return Err(e),
            };
            if packet.track_id() != self.track_id {
                continue;
            }

            let (buf, frames) =
                decode_packet_into(&mut *self.decoder, &packet, &mut self.convert_buf)?;
            if frames == 0 {
                self.skipped_primer.get_or_insert(packet.ts());
                continue;
            }
            let num_ch = buf.spec().channels.count();
            let ts = packet.ts();

            self.leftover.clear();
            self.leftover_pos = 0;
            // A packet whose channel count disagrees with `open`'s is
            // zero-filled up to the file's width, so the stride stays constant.
            let width = usize::from(self.layout.count());
            let ch = num_ch.min(width);
            self.leftover.resize(frames * width, 0.0);
            for c in 0..ch {
                let dst = self.leftover[c..].iter_mut().step_by(width);
                for (d, &s) in dst.zip(&buf.chan(c)[..frames]) {
                    *d = s;
                }
            }
            return Ok((frames, ts));
        }
    }

    /// The fault that cut a sequential read short, if one did: a packet that
    /// failed to decode, or (FLAC only, whose frame count is exact) a stream
    /// that ended before that count, where a damaged frame can read as a
    /// clean end of file. A [`WaveError`] is symphonia's error type.
    ///
    /// [`poll_into`](AudioIn::poll_into) has no error channel: a failed read
    /// returns the frames decoded before it and then `0`, the same as
    /// end-of-file, so a consumer that sees `0` asks here to tell a corrupt
    /// file from a finished one. The fault is sticky (later reads return it
    /// without decoding on) and cleared by a successful [`seek`](Self::seek).
    pub fn error(&self) -> Option<&WaveError> {
        self.fault.as_ref()
    }

    /// Returns the frame the next sequential read will produce.
    pub fn cursor(&self) -> Samples {
        frames(self.cursor)
    }

    /// Fills `out` with the next sequential frames at the file's **own**
    /// channel width, interleaved, and returns how many frames were produced
    /// (fewer than fit only at the end of the file).
    ///
    /// `out.len()` should be a multiple of the file's channel count; a
    /// partial trailing frame is not filled. This is the fallible form of
    /// [`poll_into`](AudioIn::poll_into).
    ///
    /// # Errors
    ///
    /// A [`WaveError`] if reading or decoding a packet fails. Frames filled
    /// before the failure are returned as `Ok` first, and the failure comes
    /// with the next call (and stays, see [`error`](Self::error)). The same
    /// goes for a FLAC stream that ends before its frame count.
    pub fn fill_sequential_interleaved(&mut self, out: &mut [f32]) -> Result<Samples, WaveError> {
        if let Some(fault) = &self.fault {
            return Err(repeat(fault));
        }
        let (filled, fault) = self.fill(out);
        self.fault = fault;
        match &self.fault {
            // The frames before the fault are the caller's; the fault is
            // reported by the next call, which finds nothing more to read.
            Some(fault) if filled == 0 => Err(repeat(fault)),
            _ => Ok(Samples(filled)),
        }
    }

    /// Whether the stream has ended short of the container's own frame count.
    ///
    /// A damaged frame can read as a clean end of file: the demuxer reports
    /// end-of-stream where it lost sync, so the only evidence is the count.
    fn truncation(&self) -> bool {
        self.exact_length && self.total_frames.is_some_and(|total| self.cursor < total)
    }

    /// [`fill_sequential_interleaved`](Self::fill_sequential_interleaved),
    /// keeping the frames filled before a decode error.
    fn fill(&mut self, out: &mut [f32]) -> (usize, Option<WaveError>) {
        let ch = self.width();
        let capacity_frames = out.len() / ch;
        let mut filled = 0usize;

        while filled < capacity_frames {
            // Drain any retained remainder first (seek-free, alloc-free).
            if self.leftover_len() > 0 {
                let avail = self.leftover_len();
                let want = (capacity_frames - filled).min(avail);
                let src_start = self.leftover_pos * ch;
                let src = &self.leftover[src_start..src_start + want * ch];
                out[filled * ch..(filled + want) * ch].copy_from_slice(src);
                self.leftover_pos += want;
                filled += want;
                self.cursor += want as u64;
                continue;
            }

            // Need another packet.
            match self.decode_next_packet() {
                // End-of-stream: leave the untouched tail for the caller.
                Ok((0, _)) => {
                    let fault = self.truncation().then_some(Error::DecodeError(TRUNCATED));
                    return (filled, fault);
                }
                Ok(_) => {}
                Err(error) => return (filled, Some(error)),
            }
        }

        (filled, None)
    }

    /// Positions the stream so the next read produces frame `start` exactly.
    ///
    /// Seeks the container to the packet holding `start`, then decodes and
    /// discards the frames before it. Seeking past the end leaves the stream
    /// at its end. Only meaningful when [`seekable`](Self::seekable).
    ///
    /// # Errors
    ///
    /// A [`WaveError`] if the container cannot seek or a packet fails to
    /// decode.
    pub fn seek(&mut self, start: Samples) -> Result<(), WaveError> {
        let start = start.get() as u64;
        // Accurate seek lands on the packet containing `target`; decoding then
        // discards the preroll up to `start`. Two things make that subtler
        // than `start - actual_ts`:
        //
        // 1. **The first packet after `reset` may decode to nothing.** A
        //    Vorbis decoder needs one packet to prime its overlap, and
        //    `decode_next_packet` skips it. The frames that come out then start
        //    at the *next* packet's `ts`, so the preroll is counted from the
        //    timestamp of the first packet that produced frames — never from
        //    `actual_ts`; counting from `actual_ts` would land every Ogg seek
        //    about 1024 frames late, with no error.
        // 2. **That packet can start after `start`** — or not exist, when the
        //    primer was the file's last packet. The primer was then the packet
        //    containing `start`, and the frames at `start` cannot be discarded
        //    back into existence. So the seek is retried from one frame before
        //    the primer, which makes the packet before it the primer instead.
        //    Each retry targets strictly earlier, and target 0 ends it.
        //
        // For codecs whose first packet decodes (PCM, FLAC, MP3 here) the
        // first attempt succeeds and this is one seek.
        let mut target = start;
        loop {
            self.leftover.clear();
            self.leftover_pos = 0;
            self.reader.seek(
                SeekMode::Accurate,
                SeekTo::TimeStamp {
                    ts: target,
                    track_id: self.track_id,
                },
            )?;
            self.decoder.reset();

            let (decoded, ts) = self.decode_next_packet()?;
            if (decoded == 0 || ts > start)
                && let Some(primer) = self.skipped_primer.filter(|&p| target > 0 && p <= target)
            {
                target = primer.saturating_sub(1);
                continue;
            }
            if decoded == 0 {
                // Seeked past EOF; nothing to discard.
                break;
            }

            // The frames in `leftover` start at `ts`; discard up to `start`.
            let mut discard = start.saturating_sub(ts);
            loop {
                let avail = self.leftover_len() as u64;
                let skip = avail.min(discard);
                self.leftover_pos += skip as usize;
                discard -= skip;
                if discard == 0 {
                    break;
                }
                if self.decode_next_packet()?.0 == 0 {
                    break; // EOF inside the preroll
                }
            }
            break;
        }

        self.cursor = start;
        self.fault = None;
        Ok(())
    }
}

/// Sequential reads at the file's **own** channel width. A decode error ends
/// the stream (`0` from then on) and is kept for [`FileIn::error`]; use
/// [`fill_sequential_interleaved`](FileIn::fill_sequential_interleaved) to get
/// it back as a `Result`.
///
/// No downmix happens here: `layout()` describes exactly what `poll_into`
/// produces. A caller wanting stereo folds each frame through
/// [`tutti_types::fold_frame`].
impl AudioIn for FileIn {
    /// A file has an end, and a decode error ends the stream there too (see the
    /// doc above): either way `0` means there is no more to read, and
    /// [`FileIn::error`] says which.
    const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

    /// The file's own width, which is what [`poll_into`](Self::poll_into) produces.
    fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Flat interleaved at [`layout`](Self::layout)'s width. `out` holds
    /// `out.len() / channels` frames, and the return is that many FRAMES at
    /// most — never samples.
    ///
    /// A trailing partial frame is not filled: a short frame desynchronises the
    /// interleave for everything after it.
    fn poll_into(&mut self, out: &mut [f32]) -> Samples {
        self.fill_sequential_interleaved(out).unwrap_or(Samples(0))
    }
}

#[cfg(all(test, feature = "wav"))]
mod tests;
