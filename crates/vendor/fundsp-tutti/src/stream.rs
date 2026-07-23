//! Incremental disk streaming on top of Symphonia's public API.
//!
//! [`FileIn`] decodes an audio file sequentially, frame by frame, without
//! loading the whole file into RAM. It is an [`AudioIn`]: [`poll_into`] fills a
//! caller buffer of `[f32; 2]` frames from the current cursor and reports how
//! many it produced (a short count then `0` at end-of-stream). To read from an
//! arbitrary position, call [`seek`] first — it hooks
//! `FormatReader::seek(SeekMode::Accurate, ...)` (which lands *before* the
//! requested frame) and decodes-and-discards the preroll to hit the exact frame,
//! so a following `poll_into` is a clean sequential read from there.
//!
//! [`poll_into`]: FileIn::poll_into
//! [`seek`]: FileIn::seek
//!
//! It shares `read.rs`'s codec feature gates and the `decode_packet_into`
//! one-packet helper. All decode/seek/file I/O runs on the butler thread; the
//! audio thread never touches this type.

use super::read::{decode_packet_into, WaveResult};
use tutti_types::io::AudioIn;
use std::fs::File;
use std::path::Path;
extern crate alloc;
use alloc::boxed::Box;
use symphonia::core::audio::{AudioBuffer, Signal};
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Incremental range decoder over a single audio track.
///
/// Holds a live `FormatReader` + `Decoder` positioned at `cursor` (the next
/// file sample-frame that a sequential read will produce). The `leftover`
/// buffer retains the tail of the last-decoded packet so back-to-back
/// sequential reads consume it before pulling another packet — keeping the
/// common refill path both seek-free and allocation-free.
pub struct FileIn {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    channels: usize,
    sample_rate: u32,
    total_frames: Option<u64>,
    /// Scratch buffer reused by `decode_packet_into` (allocated on first decode).
    convert_buf: Option<AudioBuffer<f32>>,
    /// Decoded-but-unconsumed stereo frames from the last packet.
    leftover: Vec<[f32; 2]>,
    /// Read offset into `leftover`; frames `[leftover_pos..]` are unconsumed.
    leftover_pos: usize,
    /// Next file sample-frame a sequential read produces.
    cursor: u64,
    /// Whether the container reports a frame count and supports accurate seek.
    seekable: bool,
}

impl FileIn {
    /// Total sample frames if the container reports it.
    pub fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Whether this decoder can serve arbitrary (seeking) ranges. When false,
    /// callers must fall back to the whole-file load path.
    pub fn seekable(&self) -> bool {
        self.seekable
    }

    /// Open `path`, selecting `track` (or the first known-codec track).
    ///
    /// Probes the container, makes a decoder, and allocates the persistent
    /// scratch (`convert_buf` lazily on first decode; `leftover` reserved
    /// here). Detects seekability from the reported frame count.
    pub fn open<P: AsRef<Path>>(path: P, track: Option<usize>) -> WaveResult<Self> {
        let path = path.as_ref();
        let mut hint = Hint::new();
        if let Some(extension) = path.extension()
            && let Some(extension_str) = extension.to_str()
        {
            hint.with_extension(extension_str);
        }

        let source: Box<dyn MediaSource> = match File::open(path) {
            Ok(file) => Box::new(file),
            Err(error) => return Err(Error::IoError(error)),
        };

        let stream = MediaSourceStream::new(source, Default::default());
        let format_opts = FormatOptions {
            enable_gapless: false,
            ..Default::default()
        };
        let metadata_opts: MetadataOptions = Default::default();

        let probed =
            symphonia::default::get_probe().format(&hint, stream, &format_opts, &metadata_opts)?;
        let reader = probed.format;

        // Select the requested track, else the first track with a known codec.
        let track = track.and_then(|t| reader.tracks().get(t)).or_else(|| {
            reader
                .tracks()
                .iter()
                .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        });
        let track = track.ok_or(Error::DecodeError("Could not find track."))?;
        let track_id = track.id;

        let total_frames = track.codec_params.n_frames;
        let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
        let channels = track
            .codec_params
            .channels
            .map(|ch| ch.count())
            .unwrap_or(2);

        let decode_opts = DecoderOptions::default();
        let decoder =
            symphonia::default::get_codecs().make(&track.codec_params, &decode_opts)?;

        // Seekable only when the container reports a frame count. Formats
        // without n_frames (some VBR/streamed) fall back to whole-file load.
        let seekable = total_frames.is_some();

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
        })
    }

    /// Number of unconsumed frames currently held in `leftover`.
    #[inline]
    fn leftover_len(&self) -> usize {
        self.leftover.len() - self.leftover_pos
    }

    /// Refill `leftover` from exactly one more packet of the selected track.
    /// Returns the number of frames decoded (0 at EOF). Retains a `convert_buf`
    /// scratch to stay allocation-free after warmup.
    fn decode_next_packet(&mut self) -> WaveResult<usize> {
        loop {
            let packet = match self.reader.next_packet() {
                Ok(p) => p,
                Err(Error::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(0);
                }
                Err(e) => return Err(e),
            };
            if packet.track_id() != self.track_id {
                continue;
            }

            let (buf, frames) =
                decode_packet_into(&mut *self.decoder, &packet, &mut self.convert_buf)?;
            let num_ch = buf.spec().channels.count();

            self.leftover.clear();
            self.leftover_pos = 0;
            if num_ch > 1 {
                let left = &buf.chan(0)[..frames];
                let right = &buf.chan(1)[..frames];
                for i in 0..frames {
                    self.leftover.push([left[i], right[i]]);
                }
            } else {
                let mono = &buf.chan(0)[..frames];
                for &s in mono.iter() {
                    self.leftover.push([s, s]);
                }
            }
            return Ok(frames);
        }
    }

    /// The next file sample-frame a sequential [`poll_into`](Self::poll_into)
    /// will produce.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Fill the front of `out` with the next sequential frames from the current
    /// cursor and return how many were produced (`0..=out.len()`). A short count
    /// (then `0`) marks end-of-stream; frames past the returned count are left
    /// untouched. This is the fallible core of the [`AudioIn`] impl — the trait
    /// method calls it and treats a decode error as end-of-stream.
    ///
    /// The `leftover` remainder from the last packet is drained first
    /// (seek-free, alloc-free), then packets are pulled forward. To read from a
    /// non-current position, call [`seek`](Self::seek) first.
    pub fn fill_sequential(&mut self, out: &mut [[f32; 2]]) -> WaveResult<usize> {
        let mut filled = 0usize;

        while filled < out.len() {
            // Drain any retained remainder first (seek-free, alloc-free).
            if self.leftover_len() > 0 {
                let avail = self.leftover_len();
                let want = (out.len() - filled).min(avail);
                let src = &self.leftover[self.leftover_pos..self.leftover_pos + want];
                out[filled..filled + want].copy_from_slice(src);
                self.leftover_pos += want;
                filled += want;
                self.cursor += want as u64;
                continue;
            }

            // Need another packet.
            let decoded = self.decode_next_packet()?;
            if decoded == 0 {
                // End-of-stream — leave the untouched tail for the caller.
                break;
            }
        }

        Ok(filled)
    }

    /// Accurate-seek to `start` so the next [`poll_into`](Self::poll_into)
    /// produces frame `start`. Discards the preroll so `cursor == start`.
    pub fn seek(&mut self, start: u64) -> WaveResult<()> {
        self.leftover.clear();
        self.leftover_pos = 0;

        let seeked = self.reader.seek(
            SeekMode::Accurate,
            SeekTo::TimeStamp {
                ts: start,
                track_id: self.track_id,
            },
        )?;
        self.decoder.reset();

        // Accurate seek lands at actual_ts <= start; decode-and-discard the
        // preroll frames to reach the exact requested frame.
        let mut discard = start.saturating_sub(seeked.actual_ts);
        while discard > 0 {
            let decoded = self.decode_next_packet()?;
            if decoded == 0 {
                // Seeked past EOF; nothing more to discard.
                break;
            }
            let skip = (decoded as u64).min(discard);
            self.leftover_pos += skip as usize;
            discard -= skip;
        }

        self.cursor = start;
        Ok(())
    }
}

/// Sequential stereo-`f32` read half. A decode error surfaces as end-of-stream
/// (`0`): the butler refill treats a short/zero poll as a boundary, and the
/// fallible detail is available through [`fill_sequential`](FileIn::fill_sequential)
/// for callers that want it.
impl AudioIn for FileIn {
    fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
        self.fill_sequential(out).unwrap_or(0)
    }
}

#[cfg(all(test, feature = "wav"))]
mod tests {
    use super::*;
    use crate::wave::Wave;

    fn write_test_wav(frames: usize) -> std::path::PathBuf {
        let sample_rate = 44100.0;
        let mut wave = Wave::new(2, sample_rate);
        for i in 0..frames {
            // Distinct per-frame values so slice comparisons are meaningful.
            let l = (i as f32 / frames as f32) - 0.5;
            let r = 0.25 - (i as f32 / frames as f32);
            wave.push((l, r));
        }
        let mut path = std::env::temp_dir();
        path.push(format!("tutti_stream_decoder_{}.wav", frames));
        wave.save_wav16(&path).expect("save wav");
        path
    }

    fn loaded_stereo(path: &std::path::Path) -> Vec<[f32; 2]> {
        let w = Wave::load(path).expect("load");
        (0..w.len()).map(|i| [w.at(0, i), w.at(1, i)]).collect()
    }

    #[test]
    fn full_sequential_read_matches_full_load() {
        let frames = 20_000usize;
        let path = write_test_wav(frames);
        let expected = loaded_stereo(&path);

        let mut dec = FileIn::open(&path, None).expect("open");
        assert!(dec.seekable());

        // Poll the whole file in several sequential chunks (all fast-path, no
        // seek — the cursor advances on its own).
        let mut got = vec![[0.0f32; 2]; frames];
        let chunk = 3000usize;
        let mut pos = 0usize;
        while pos < frames {
            let end = (pos + chunk).min(frames);
            let n = dec.poll_into(&mut got[pos..end]);
            assert_eq!(n, end - pos);
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
        let path = write_test_wav(frames);
        let expected = loaded_stereo(&path);

        let mut dec = FileIn::open(&path, None).expect("open");

        let start = 12_345u64;
        let len = 2_000usize;
        dec.seek(start).expect("seek");
        assert_eq!(dec.cursor(), start);
        let mut got = vec![[0.0f32; 2]; len];
        let n = dec.poll_into(&mut got);
        assert_eq!(n, len);

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
        let path = write_test_wav(frames);
        let expected = loaded_stereo(&path);

        let mut dec = FileIn::open(&path, None).expect("open");

        // Straddle EOF: seek to 500 before the end, then ask for 1000 frames.
        let start = (frames - 500) as u64;
        let len = 1000usize;
        dec.seek(start).expect("seek");
        // Pre-fill with a sentinel so we can assert the untouched tail stays put.
        let mut got = vec![[1.0f32; 2]; len];
        let n = dec.poll_into(&mut got);
        assert_eq!(n, 500, "only 500 frames of real audio remain");

        for i in 0..500 {
            let e = expected[start as usize + i];
            assert!(
                (got[i][0] - e[0]).abs() < 1e-4 && (got[i][1] - e[1]).abs() < 1e-4,
                "frame {} mismatch",
                i
            );
        }
        // AudioIn leaves the tail past the returned count untouched (no zero-pad).
        for i in 500..len {
            assert_eq!(got[i], [1.0, 1.0], "frame {} should be left untouched", i);
        }
        // A further poll at EOF yields nothing.
        let mut more = [[0.0f32; 2]; 8];
        assert_eq!(dec.poll_into(&mut more), 0);
        let _ = std::fs::remove_file(&path);
    }
}
