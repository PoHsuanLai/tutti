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
//! through [`tutti_core::fold_frame`], as every other engine edge does.
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
use tutti_core::io::{AudioIn, OnEmpty};
use tutti_core::{ChannelLayout, Samples};

use crate::WaveError;
use crate::decode::{decode_packet_into, first_audio_track, probe_path};

/// What [`FileIn::error`] reports when the stream ends before the container's
/// frame count.
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

const TRUNCATED: &str = "stream ended before the container's frame count";

/// A streaming decoder over an audio file's first audio track, as an
/// [`AudioIn`].
///
/// Decodes sequentially without loading the whole file into memory (compare
/// [`Wave::read`](crate::Wave::read)). [`poll_into`](AudioIn::poll_into) fills a
/// flat interleaved `f32` buffer **at the file's own channel width** and
/// returns the frames produced; a short count and then `0` mark the end of the
/// file. It does not downmix: a caller wanting stereo folds each frame through
/// [`tutti_core::fold_frame`].
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
///
/// let mut file = FileIn::open("take.flac")?;
/// let width = file.layout().count() as usize;
/// let mut buf = vec![0.0f32; 1024 * width];
/// file.seek(48_000)?; // start one second in, at 48 kHz
/// let frames = file.poll_into(&mut buf);
/// println!("read {} frames of {width} channels", frames.get());
/// # Ok::<(), tutti_io::WaveError>(())
/// ```
pub struct FileIn {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    channels: usize,
    sample_rate: u32,
    total_frames: Option<u64>,
    /// Scratch buffer reused by `decode_packet_into` (allocated on first decode).
    convert_buf: Option<AudioBuffer<f32>>,
    /// Decoded-but-unconsumed frames from the last packet, **interleaved at the
    /// file's own [`channels`](Self::channels)** — not folded to stereo. Frame
    /// `f` channel `c` is at `leftover[f * channels + c]`.
    leftover: Vec<f32>,
    /// Read offset into `leftover` in **frames** (not samples); frames
    /// `[leftover_pos..]` are unconsumed. Kept frame-denominated because
    /// [`seek`](Self::seek) advances it by a decoded frame count.
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
            .field("channels", &self.channels)
            .field("sample_rate", &self.sample_rate)
            .field("total_frames", &self.total_frames)
            .field(
                "leftover_frames",
                &(self.leftover.len() / self.channels.max(1)),
            )
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
    pub fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    /// Returns the file's channel count (2 if the container does not say).
    /// The same number as [`AudioIn::layout`], as a `usize`.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Returns the file's sample rate in Hz (44 100 if the container does not
    /// say). Nothing is resampled.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
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
        let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
        let channels = track
            .codec_params
            .channels
            .map(|ch| ch.count())
            .unwrap_or(2);

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
            channels,
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

    /// Unconsumed retained **frames** (not samples). `leftover` is interleaved,
    /// so its raw length is a sample count and must be divided by the width to
    /// stay in the same units as `leftover_pos`.
    #[inline]
    fn leftover_len(&self) -> usize {
        let ch = self.channels.max(1);
        (self.leftover.len() / ch).saturating_sub(self.leftover_pos)
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
            // Interleave every decoded channel at the file's own width; a
            // caller wanting stereo folds.
            //
            // A packet whose channel count disagrees with the one read at
            // `open` (rare, malformed) is trusted per-packet for the read and
            // zero-filled up to `self.channels`, so the interleave stride stays
            // constant and never runs past the buffer.
            let ch = num_ch.min(self.channels);
            // Channel by channel into a zeroed buffer, so the channels the
            // packet lacks are already silence.
            self.leftover.resize(frames * self.channels, 0.0);
            for c in 0..ch {
                let dst = self.leftover[c..].iter_mut().step_by(self.channels);
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
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Fills `out` with the next sequential frames at the file's **own**
    /// channel width, interleaved, and returns how many frames were produced
    /// (fewer than fit only at the end of the file).
    ///
    /// `out.len()` should be a multiple of [`channels`](Self::channels); a
    /// partial trailing frame is not filled. This is the fallible form of
    /// [`poll_into`](AudioIn::poll_into).
    ///
    /// # Errors
    ///
    /// A [`WaveError`] if reading or decoding a packet fails. Frames filled
    /// before the failure are returned as `Ok` first, and the failure comes
    /// with the next call (and stays, see [`error`](Self::error)). The same
    /// goes for a FLAC stream that ends before its frame count.
    pub fn fill_sequential_interleaved(&mut self, out: &mut [f32]) -> Result<usize, WaveError> {
        if let Some(fault) = &self.fault {
            return Err(repeat(fault));
        }
        let (filled, fault) = self.fill(out);
        self.fault = fault;
        match &self.fault {
            // The frames before the fault are the caller's; the fault is
            // reported by the next call, which finds nothing more to read.
            Some(fault) if filled == 0 => Err(repeat(fault)),
            _ => Ok(filled),
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
        let ch = self.channels.max(1);
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
    pub fn seek(&mut self, start: u64) -> Result<(), WaveError> {
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
/// [`tutti_core::fold_frame`].
impl AudioIn for FileIn {
    /// A file has an end, and a decode error ends the stream there too (see the
    /// doc above): either way `0` means there is no more to read, and
    /// [`FileIn::error`] says which.
    const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

    /// The file's own width — what [`poll_into`](Self::poll_into) actually
    /// produces. [`FileIn::channels`] is the same number as a `usize`.
    fn layout(&self) -> ChannelLayout {
        ChannelLayout::from(self.channels.max(1) as u16)
    }

    /// Flat interleaved at [`layout`](Self::layout)'s width. `out` holds
    /// `out.len() / channels` frames, and the return is that many FRAMES at
    /// most — never samples.
    ///
    /// A trailing partial frame is not filled: a short frame desynchronises the
    /// interleave for everything after it.
    ///
    /// The inherent [`fill_sequential_interleaved`](FileIn::fill_sequential_interleaved)
    /// returns the same count as a bare `usize`.
    fn poll_into(&mut self, out: &mut [f32]) -> Samples {
        Samples(self.fill_sequential_interleaved(out).unwrap_or(0))
    }
}

#[cfg(all(test, feature = "wav"))]
mod tests {
    use super::*;
    use crate::Wave;
    use crate::decode::probe_path;

    /// Write `wave` as a 16-bit WAV named `name` and return its path.
    ///
    /// `Wave` has no writer (the engine's WAV sink is `WavOut`), so this uses
    /// `hound` directly. The directory is per process, and nextest runs each
    /// test in its own, so no two tests share a file.
    fn save_wav16(wave: &Wave, name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tutti_io_file_in_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels: wave.channels() as u16,
            sample_rate: wave.sample_rate().get() as u32,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).expect("create wav");
        for i in 0..wave.len() {
            for c in 0..wave.channels() {
                let s = (wave.at(c, i).clamp(-1.0, 1.0) * 32767.0).round() as i16;
                w.write_sample(s).expect("write sample");
            }
        }
        w.finalize().expect("finalize wav");
        path
    }

    /// Write a scratch WAV for one test. `tag` must be unique per caller:
    /// tests run in parallel, and a shared filename means one test reads the
    /// file while another is still writing it.
    fn write_test_wav(frames: usize, tag: &str) -> std::path::PathBuf {
        let sample_rate = 44100.0;
        let mut wave = Wave::new(2, sample_rate);
        for i in 0..frames {
            // Distinct per-frame values so slice comparisons are meaningful.
            let l = (i as f32 / frames as f32) - 0.5;
            let r = 0.25 - (i as f32 / frames as f32);
            wave.push_frame(&[l, r]);
        }
        save_wav16(&wave, &format!("tutti_stream_decoder_{tag}_{frames}.wav"))
    }

    fn loaded_stereo(path: &std::path::Path) -> Vec<[f32; 2]> {
        let w = Wave::read(path).expect("load");
        (0..w.len()).map(|i| [w.at(0, i), w.at(1, i)]).collect()
    }

    #[test]
    fn full_sequential_read_matches_full_load() {
        let frames = 20_000usize;
        let path = write_test_wav(frames, "sequential");
        let expected = loaded_stereo(&path);

        let mut dec = FileIn::open(&path).expect("open");
        assert!(dec.seekable());

        // Poll the whole file in several sequential chunks (all fast-path, no
        // seek — the cursor advances on its own).
        let mut got = vec![[0.0f32; 2]; frames];
        let chunk = 3000usize;
        let mut pos = 0usize;
        while pos < frames {
            let end = (pos + chunk).min(frames);
            let n = dec.poll_into(got[pos..end].as_flattened_mut());
            assert_eq!(n, Samples(end - pos));
            pos = end;
        }

        for i in 0..frames {
            assert!(
                (got[i][0] - expected[i][0]).abs() < 1e-4
                    && (got[i][1] - expected[i][1]).abs() < 1e-4,
                "frame {} mismatch: got {:?} expected {:?}",
                i,
                got[i],
                expected[i]
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn seek_then_poll_matches_slice() {
        let frames = 20_000usize;
        let path = write_test_wav(frames, "seek");
        let expected = loaded_stereo(&path);

        let mut dec = FileIn::open(&path).expect("open");

        let start = 12_345u64;
        let len = 2_000usize;
        dec.seek(start).expect("seek");
        assert_eq!(dec.cursor(), start);
        let mut got = vec![[0.0f32; 2]; len];
        let n = dec.poll_into(got.as_flattened_mut());
        assert_eq!(n, Samples(len));

        for i in 0..len {
            let e = expected[start as usize + i];
            assert!(
                (got[i][0] - e[0]).abs() < 1e-4 && (got[i][1] - e[1]).abs() < 1e-4,
                "frame {} (file {}) mismatch: got {:?} expected {:?}",
                i,
                start as usize + i,
                got[i],
                e
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn poll_past_eof_returns_short_count() {
        let frames = 5_000usize;
        let path = write_test_wav(frames, "eof");
        let expected = loaded_stereo(&path);

        let mut dec = FileIn::open(&path).expect("open");

        // Straddle EOF: seek to 500 before the end, then ask for 1000 frames.
        let start = (frames - 500) as u64;
        let len = 1000usize;
        dec.seek(start).expect("seek");
        // Pre-fill with a sentinel so we can assert the untouched tail stays put.
        let mut got = vec![[1.0f32; 2]; len];
        let n = dec.poll_into(got.as_flattened_mut());
        assert_eq!(n, Samples(500), "only 500 frames of real audio remain");

        for i in 0..500 {
            let e = expected[start as usize + i];
            assert!(
                (got[i][0] - e[0]).abs() < 1e-4 && (got[i][1] - e[1]).abs() < 1e-4,
                "frame {} mismatch",
                i
            );
        }
        // AudioIn leaves the tail past the returned count untouched (no zero-pad).
        for (i, frame) in got.iter().enumerate().skip(500) {
            assert_eq!(*frame, [1.0, 1.0], "frame {} should be left untouched", i);
        }
        // A further poll at EOF yields nothing.
        let mut more = [[0.0f32; 2]; 8];
        assert_eq!(dec.poll_into(more.as_flattened_mut()), Samples::ZERO);
        let _ = std::fs::remove_file(&path);
    }

    fn asset(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../assets/audio")
            .join(name)
    }

    /// The repo's `name` fixture cut to its first `keep` fraction.
    fn truncated_asset(name: &str, keep: f64) -> std::path::PathBuf {
        let bytes = std::fs::read(asset(name)).expect("fixture");
        let dir = std::env::temp_dir().join(format!("tutti_io_file_in_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(format!("cut_{name}"));
        std::fs::write(&path, &bytes[..(bytes.len() as f64 * keep) as usize]).expect("write");
        path
    }

    /// Frames a poll loop reads before `0`.
    fn drain(dec: &mut FileIn) -> usize {
        let mut buf = vec![0.0f32; 1024 * dec.channels()];
        let mut frames = 0;
        loop {
            let n = dec.poll_into(&mut buf).get();
            if n == 0 {
                return frames;
            }
            frames += n;
        }
    }

    /// A FLAC cut short ends the stream early, and that is a fault: the
    /// demuxer reports a cut (or a damaged frame) as a clean end of file, so
    /// the evidence is the STREAMINFO frame count. The frames before the cut
    /// are kept, the cause is readable, the fallible read reports it after
    /// them and keeps reporting it, and a seek clears it.
    ///
    /// Mutation (run): `exact_length` always `false` fails the `error()`
    /// assertion.
    #[test]
    fn a_truncated_flac_is_a_fault_not_a_clean_end() {
        let path = truncated_asset("stereo.flac", 0.9);
        let mut dec = FileIn::open(&path).expect("a cut file still opens");
        let total = dec.total_frames().expect("flac reports a length") as usize;
        let mut buf = vec![0.0f32; 1024 * dec.channels()];
        let mut frames = 0;
        loop {
            match dec.fill_sequential_interleaved(&mut buf) {
                Ok(0) => panic!("a fault must surface as Err, not as a bare end"),
                Ok(n) => frames += n,
                Err(e) => {
                    assert!(e.to_string().contains("frame count"), "{e}");
                    break;
                }
            }
        }
        assert!(frames > 0 && frames < total, "{frames} of {total}");
        assert!(dec.error().is_some());
        assert!(dec.fill_sequential_interleaved(&mut buf).is_err(), "sticky");
        assert_eq!(dec.poll_into(&mut buf), Samples::ZERO);
        dec.seek(0).expect("seek");
        assert!(dec.error().is_none(), "a successful seek clears the fault");
        let _ = std::fs::remove_file(&path);
    }

    /// Where the container's frame count is not exact, a short stream is not
    /// a fault: a trimmed or zero-padded MP3, a trimmed WAV (its header still
    /// claims the full data length) and a whole Ogg (Vorbis priming) all read
    /// to their end clean. Ogg cannot be trimmed: the demuxer needs its last
    /// page to open.
    ///
    /// Mutation (run): `exact_length` always `true` fails the WAV case.
    #[test]
    fn a_file_without_an_exact_count_reads_clean() {
        let mut padded = std::fs::read(asset("stereo.mp3")).expect("fixture");
        padded.extend_from_slice(&[0; 4096]);
        let dir = std::env::temp_dir().join(format!("tutti_io_file_in_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let padded_path = dir.join("padded.mp3");
        std::fs::write(&padded_path, padded).expect("write");

        let cases = [
            ("trimmed mp3", truncated_asset("stereo.mp3", 0.9)),
            ("padded mp3", padded_path),
            ("trimmed wav", truncated_asset("stereo_s16.wav", 0.9)),
            ("whole ogg", asset("stereo.ogg")),
        ];
        for (what, path) in cases {
            let mut dec = FileIn::open(&path).expect("open");
            assert!(drain(&mut dec) > 0, "{what}");
            assert_eq!(dec.error().map(ToString::to_string), None, "{what}");
        }
    }

    /// A demuxer that fails with a decode error after `left` packets.
    struct FailAfter {
        inner: Box<dyn symphonia::core::formats::FormatReader>,
        left: usize,
    }

    impl symphonia::core::formats::FormatReader for FailAfter {
        fn try_new(
            _: symphonia::core::io::MediaSourceStream,
            _: &symphonia::core::formats::FormatOptions,
        ) -> Result<Self, Error> {
            Err(Error::Unsupported("test double"))
        }
        fn cues(&self) -> &[symphonia::core::formats::Cue] {
            self.inner.cues()
        }
        fn metadata(&mut self) -> symphonia::core::meta::Metadata<'_> {
            self.inner.metadata()
        }
        fn seek(
            &mut self,
            mode: SeekMode,
            to: SeekTo,
        ) -> Result<symphonia::core::formats::SeekedTo, Error> {
            self.inner.seek(mode, to)
        }
        fn tracks(&self) -> &[symphonia::core::formats::Track] {
            self.inner.tracks()
        }
        fn next_packet(&mut self) -> Result<symphonia::core::formats::Packet, Error> {
            if self.left == 0 {
                return Err(Error::DecodeError("injected"));
            }
            self.left -= 1;
            self.inner.next_packet()
        }
        fn into_inner(self: Box<Self>) -> symphonia::core::io::MediaSourceStream {
            self.inner.into_inner()
        }
    }

    /// A decode error in mid-stream keeps the frames before it: the first
    /// read returns them as `Ok`, the next returns the cause, `poll_into`
    /// returns the same frames then `0`, and `error()` holds the cause.
    ///
    /// Mutation (run): returning `Err` at once when the fill is cut short
    /// (dropping `filled`, as `unwrap_or(0)` did) fails the frame-count
    /// assertion.
    #[test]
    fn a_mid_stream_decode_error_keeps_the_frames_before_it() {
        let path = write_test_wav(20_000, "inject");
        let open = |left| {
            let mut dec = FileIn::open(&path).expect("open");
            let reader = std::mem::replace(
                &mut dec.reader,
                Box::new(FailAfter {
                    inner: probe_path(&path).expect("probe"),
                    left,
                }),
            );
            drop(reader);
            dec
        };

        let mut clean = FileIn::open(&path).expect("open");
        let mut first = vec![0.0f32; 4096 * 2];
        assert_eq!(clean.poll_into(&mut first).get(), 4096);

        // Fail on the packet after the first: the first packet's frames are
        // what the stream holds. Read more than they cover.
        let mut dec = open(1);
        let mut buf = vec![0.0f32; 20_000 * 2];
        let got = dec
            .fill_sequential_interleaved(&mut buf)
            .expect("frames first");
        assert!(got > 0 && got < 20_000, "{got}");
        assert!(
            dec.error()
                .map(ToString::to_string)
                .unwrap()
                .contains("injected")
        );
        let again = dec.fill_sequential_interleaved(&mut buf).unwrap_err();
        assert!(again.to_string().contains("injected"), "{again}");
        assert_eq!(
            &buf[..got * 2],
            &{
                let mut c = FileIn::open(&path).expect("open");
                let mut v = vec![0.0f32; got * 2];
                c.poll_into(&mut v);
                v
            }[..]
        );

        let mut dec = open(1);
        assert_eq!(dec.poll_into(&mut buf).get(), got);
        assert_eq!(dec.poll_into(&mut buf), Samples::ZERO);
        let _ = std::fs::remove_file(&path);
    }

    /// A finished file has no error to report.
    ///
    /// Mutation (run): `truncation` returning `true` fails this.
    #[test]
    fn a_clean_end_of_file_has_no_error() {
        let path = write_test_wav(3_000, "clean_end");
        let mut dec = FileIn::open(&path).expect("open");
        let mut buf = vec![0.0f32; 4096 * 2];
        while dec.poll_into(&mut buf).get() > 0 {}
        assert!(dec.error().is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// Write an N-channel scratch WAV where channel `c` of frame `i` carries a
    /// value encoding both, so a wrong-channel or wrong-frame read is a wrong
    /// number rather than a plausible one.
    fn write_indexed_wav(channels: usize, frames: usize, tag: &str) -> std::path::PathBuf {
        let sample_rate = 44100.0;
        let mut wave = Wave::zero(channels, sample_rate, frames as f64 / sample_rate);
        for i in 0..frames {
            for c in 0..channels {
                wave.set(c, i, (c + 1) as f32 * 0.1 + (i % 64) as f32 * 0.0005);
            }
        }
        save_wav16(
            &wave,
            &format!("tutti_stream_indexed_{tag}_{channels}x{frames}.wav"),
        )
    }

    /// The native read delivers every channel at the file's own width. Before
    /// this, `decode_next_packet` decoded all N channels and then kept only 0
    /// and 1 — a 6-channel file arrived as its front pair with no trace that it
    /// had ever been wider.
    #[test]
    fn interleaved_read_delivers_every_channel() {
        let frames = 128;
        let path = write_indexed_wav(6, frames, "every_channel");
        let mut decoder = FileIn::open(&path).expect("open");
        assert_eq!(decoder.channels(), 6);

        let mut out = vec![0.0f32; 16 * 6];
        let got = decoder
            .fill_sequential_interleaved(&mut out)
            .expect("interleaved read");
        assert_eq!(got, 16, "expected 16 frames of 6 channels");

        for i in 0..got {
            for c in 0..6 {
                let want = (c + 1) as f32 * 0.1 + (i % 64) as f32 * 0.0005;
                let got_s = out[i * 6 + c];
                assert!(
                    (got_s - want).abs() < 1e-3,
                    "frame {i} channel {c}: expected {want}, got {got_s}"
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// `leftover_pos` counts FRAMES while `leftover` holds interleaved SAMPLES.
    /// A read that crosses a packet boundary is where that distinction bites: a
    /// sample-denominated position would desynchronise the interleave and rotate
    /// channels for the rest of the stream.
    #[test]
    fn interleaved_read_stays_frame_aligned_across_packets() {
        let frames = 4096;
        let path = write_indexed_wav(6, frames, "packet_boundary");
        let mut decoder = FileIn::open(&path).expect("open");

        // Read in small odd-sized chunks so reads land mid-packet repeatedly.
        let mut produced = 0usize;
        let mut chunk = vec![0.0f32; 7 * 6];
        while produced < 2048 {
            let got = decoder
                .fill_sequential_interleaved(&mut chunk)
                .expect("read");
            if got == 0 {
                break;
            }
            for f in 0..got {
                let i = produced + f;
                for c in 0..6 {
                    let want = (c + 1) as f32 * 0.1 + (i % 64) as f32 * 0.0005;
                    let got_s = chunk[f * 6 + c];
                    assert!(
                        (got_s - want).abs() < 1e-3,
                        "frame {i} channel {c} lost alignment: expected {want}, got {got_s}"
                    );
                }
            }
            produced += got;
        }
        assert!(produced >= 2048, "only produced {produced} frames");
        let _ = std::fs::remove_file(&path);
    }

    /// **`poll_into` preserves every channel.** A centre-only 5.1 file must come
    /// back with the energy still in channel 2 and the other five silent — no
    /// fold, no truncation.
    #[test]
    fn poll_into_delivers_the_files_own_width() {
        let frames = 64;
        let sample_rate = 44100.0;
        // 5.1 order L R C LFE Ls Rs — centre only.
        let mut wave = Wave::zero(6, sample_rate, frames as f64 / sample_rate);
        for i in 0..frames {
            wave.set(2, i, 0.5);
        }
        let path = save_wav16(&wave, "tutti_stream_centre_only.wav");

        let mut decoder = FileIn::open(&path).expect("open");
        assert_eq!(decoder.layout().count(), 6u16, "the trait reports the file");

        let mut out = [0.0f32; 6 * 16];
        let got = decoder.poll_into(&mut out);
        assert!(!got.is_zero());

        // Frame 0, channel by channel: the centre survived and nothing leaked
        // into the front pair. A fold would put ~0.35 in both L and R.
        assert!(
            (out[2] - 0.5).abs() < 1e-2,
            "centre channel was not preserved: {:?}",
            &out[..6]
        );
        for c in [0usize, 1, 3, 4, 5] {
            assert!(
                out[c].abs() < 1e-3,
                "channel {c} should be silent — did this fold? {:?}",
                &out[..6]
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    /// **A mono file stays one channel wide.** It is not duplicated to stereo.
    #[test]
    fn a_mono_file_is_not_widened() {
        let frames = 64;
        let sample_rate = 44100.0;
        let mut wave = Wave::zero(1, sample_rate, frames as f64 / sample_rate);
        // A ramp, not a constant: a constant cannot distinguish "one frame
        // duplicated" from "two frames read".
        for i in 0..frames {
            wave.set(0, i, i as f32 / frames as f32);
        }
        let path = save_wav16(&wave, "tutti_stream_mono_native.wav");

        let mut decoder = FileIn::open(&path).expect("open");
        assert_eq!(decoder.channels(), 1);
        assert_eq!(decoder.layout(), ChannelLayout::MONO, "no widening");

        let mut out = [0.0f32; 16];
        let got = decoder.poll_into(&mut out);
        assert!(!got.is_zero());

        // One sample per frame, so consecutive slots differ by one ramp step.
        // A duplicating read would deliver them in equal pairs.
        assert!(
            (out[1] - out[0]).abs() > 1e-3,
            "consecutive samples are equal — is this still duplicating? {:?}",
            &out[..4]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A caller that *wants* stereo still gets the engine's fold — it just asks
    /// for it, as the impl docs describe.
    #[test]
    fn a_caller_can_still_fold_to_stereo_itself() {
        let frames = 64;
        let sample_rate = 44100.0;
        let mut wave = Wave::zero(6, sample_rate, frames as f64 / sample_rate);
        for i in 0..frames {
            wave.set(2, i, 0.5); // centre only, as above
        }
        let path = save_wav16(&wave, "tutti_stream_caller_fold.wav");

        let mut decoder = FileIn::open(&path).expect("open");
        let ch = decoder.layout().count() as usize;
        let mut native = vec![0.0f32; ch * 16];
        let got = decoder.poll_into(&mut native);
        assert!(!got.is_zero());

        let mut stereo = [0.0f32; 2];
        tutti_core::fold_frame(&native[..ch], &mut stereo);

        // The centre reaches both sides rather than being dropped with the
        // surrounds.
        assert!(
            stereo[0].abs() > 0.1 && stereo[1].abs() > 0.1,
            "centre was lost in the caller-side fold: {stereo:?}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
