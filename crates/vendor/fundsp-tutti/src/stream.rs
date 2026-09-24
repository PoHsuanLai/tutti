//! Incremental disk streaming on top of Symphonia's public API.
//!
//! [`FileIn`] decodes an audio file sequentially, frame by frame, without
//! loading the whole file into RAM. It is an [`AudioIn`]: [`poll_into`] fills a
//! caller buffer of flat interleaved `f32` **at the file's own channel width**
//! from the current cursor and reports how many **frames** it produced (a short
//! count then `0` at end-of-stream). [`layout`](AudioIn::layout) reports that
//! width, so a caller sizes its buffer from the trait and never has to ask the
//! concrete type.
//!
//! **It does not downmix.** A caller wanting stereo folds the frames itself
//! through [`tutti_types::fold_frame`], as every other engine edge does. This
//! used to fold internally and present a fixed stereo `layout()`, which meant a
//! 6-channel file came back silently downmixed — see the [`AudioIn`] impl for
//! why that was wrong and what replaced it.
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
//! It shares `read.rs`'s codec feature gates and the `decode_packet_into`
//! one-packet helper. All decode/seek/file I/O runs on the butler thread; the
//! audio thread never touches this type.

use super::read::{WaveResult, decode_packet_into};
use std::fs::File;
use std::path::Path;
use tutti_types::io::{AudioIn, OnEmpty};
use tutti_types::{ChannelLayout, Samples};
extern crate alloc;
use alloc::boxed::Box;
use symphonia::core::audio::{AudioBuffer, Signal};
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

// `MAX_FILE_CHANNELS = 16` was removed with the stereo fold. It bounded the
// stack scratch that fold chunked through and constrained nothing else — reads
// were always at the file's own width, and still are. This decoder now has no
// channel ceiling of its own.

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
        let decoder = symphonia::default::get_codecs().make(&track.codec_params, &decode_opts)?;

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

    /// Unconsumed retained **frames** (not samples). `leftover` is interleaved,
    /// so its raw length is a sample count and must be divided by the width to
    /// stay in the same units as `leftover_pos`.
    #[inline]
    fn leftover_len(&self) -> usize {
        let ch = self.channels.max(1);
        (self.leftover.len() / ch).saturating_sub(self.leftover_pos)
    }

    /// Refill `leftover` from exactly one more packet of the selected track.
    /// Returns the number of frames decoded (0 at EOF). Retains a `convert_buf`
    /// scratch to stay allocation-free after warmup.
    fn decode_next_packet(&mut self) -> WaveResult<usize> {
        loop {
            let packet = match self.reader.next_packet() {
                Ok(p) => p,
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
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
            // Interleave every decoded channel at the file's own width. This
            // used to keep channels 0 and 1 and drop the rest, so a 6-channel
            // file was decoded in full and then thrown away down to its front
            // pair. The mono→stereo duplication that lived here moved up to the
            // `AudioIn<f32, 2>` impl, where the stereo contract actually is.
            //
            // A packet whose channel count disagrees with the one read at
            // `open` (rare, malformed) is trusted per-packet for the read and
            // zero-filled up to `self.channels`, so the interleave stride stays
            // constant and never runs past the buffer.
            let ch = num_ch.min(self.channels);
            for i in 0..frames {
                for c in 0..ch {
                    self.leftover.push(buf.chan(c)[i]);
                }
                for _ in ch..self.channels {
                    self.leftover.push(0.0);
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

    // `fill_sequential(&mut [[f32; 2]])` — the stereo-folding read — was
    // **removed** with the stereo `AudioIn` impl it existed to serve. It had no
    // other caller: the butler reads natively at both refill sites, and so does
    // the waveform summariser.
    //
    // The replacement is the pair every other engine edge already uses:
    // `fill_sequential_interleaved` for the read, then
    // `tutti_types::fold_frame` per frame if the caller genuinely wants stereo.
    // That is exactly what this method did internally, minus the decision being
    // made on the caller's behalf. `mic.rs`, `wav_out.rs` and `engine.rs` are
    // worked examples.

    /// Fill `out` with the next sequential frames at the file's **own** channel
    /// width, interleaved, and return how many frames were produced.
    ///
    /// `out.len()` should be a multiple of [`channels`](Self::channels); a
    /// partial trailing frame is not filled. This is the native read — the
    /// [`AudioIn<f32, 2>`](AudioIn) impl folds it to stereo for callers that
    /// still speak stereo frames.
    pub fn fill_sequential_interleaved(&mut self, out: &mut [f32]) -> WaveResult<usize> {
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

/// Sequential read half, at the file's **own** channel width. A decode error
/// surfaces as end-of-stream (`0`): the butler refill treats a short/zero poll
/// as a boundary, and the fallible detail is available through
/// [`fill_sequential_interleaved`](FileIn::fill_sequential_interleaved) for
/// callers that want it.
///
/// # This impl used to fold to stereo, and stopping was the fix
///
/// `layout()` returned `STEREO` unconditionally while `channels()` returned the
/// truth, so a 6-channel file polled through the trait came back downmixed with
/// nothing to indicate it. That is the one thing a runtime `layout()` exists to
/// prevent: `AudioIn`'s contract is that `layout()` describes what `poll_into`
/// produces, and a fixed answer over a variable source cannot.
///
/// It was survivable only because nothing used it — every real consumer
/// (`butler/io/refill.rs` at both sites, `dawai-waveform`) already called
/// `fill_sequential_interleaved` and read `channels()`, precisely to escape the
/// fold. The trait path's only callers were this file's own tests.
///
/// **Folding is not lost, it moved to the caller**, which is where every other
/// engine edge already puts it: `mic.rs`, `wav_out.rs`, `engine.rs` and
/// `tutti-nodes`' `DownmixNode` all narrow through [`tutti_types::fold_frame`]
/// themselves. A caller wanting stereo does the same, and now *chooses* to.
impl AudioIn for FileIn {
    /// A file has an end, and this impl folds a decode error into it (see the
    /// doc above): either way `0` means there is no more to read.
    const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

    /// The file's own width — what [`poll_into`](Self::poll_into) actually
    /// produces.
    ///
    /// `channels()` is the same number. It stays because it is `usize` and
    /// predates the trait; this returns the layout the trait is denominated in.
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
    /// The inherent [`fill_sequential_interleaved`](Self::fill_sequential_interleaved)
    /// keeps its bare `usize` frame count — this is vendored code, and the
    /// engine's frame type is applied here, at the trait boundary, rather than
    /// threaded through the decoder.
    fn poll_into(&mut self, out: &mut [f32]) -> Samples {
        Samples(self.fill_sequential_interleaved(out).unwrap_or(0))
    }
}

#[cfg(all(test, feature = "wav"))]
mod tests {
    use super::*;
    use crate::wave::Wave;

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
            wave.push((l, r));
        }
        let mut path = std::env::temp_dir();
        path.push(format!("tutti_stream_decoder_{tag}_{frames}.wav"));
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
        let path = write_test_wav(frames, "sequential");
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

        let mut dec = FileIn::open(&path, None).expect("open");

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

        let mut dec = FileIn::open(&path, None).expect("open");

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
        for i in 500..len {
            assert_eq!(got[i], [1.0, 1.0], "frame {} should be left untouched", i);
        }
        // A further poll at EOF yields nothing.
        let mut more = [[0.0f32; 2]; 8];
        assert_eq!(dec.poll_into(more.as_flattened_mut()), Samples::ZERO);
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
        let mut path = std::env::temp_dir();
        path.push(format!(
            "tutti_stream_indexed_{tag}_{channels}x{frames}.wav"
        ));
        wave.save_wav16(&path).expect("save wav");
        path
    }

    /// The native read delivers every channel at the file's own width. Before
    /// this, `decode_next_packet` decoded all N channels and then kept only 0
    /// and 1 — a 6-channel file arrived as its front pair with no trace that it
    /// had ever been wider.
    #[test]
    fn interleaved_read_delivers_every_channel() {
        let frames = 128;
        let path = write_indexed_wav(6, frames, "every_channel");
        let mut decoder = FileIn::open(&path, None).expect("open");
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
        let mut decoder = FileIn::open(&path, None).expect("open");

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
    ///
    /// This replaces `stereo_poll_folds_surround_instead_of_truncating`, which
    /// asserted the opposite and was correct until the impl stopped folding.
    #[test]
    fn poll_into_delivers_the_files_own_width() {
        let frames = 64;
        let sample_rate = 44100.0;
        // 5.1 order L R C LFE Ls Rs — centre only.
        let mut wave = Wave::zero(6, sample_rate, frames as f64 / sample_rate);
        for i in 0..frames {
            wave.set(2, i, 0.5);
        }
        let mut path = std::env::temp_dir();
        path.push("tutti_stream_centre_only.wav");
        wave.save_wav16(&path).expect("save wav");

        let mut decoder = FileIn::open(&path, None).expect("open");
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

    /// **A mono file stays one channel wide.** It is not duplicated to stereo,
    /// which is what the old impl did via `fold_frame`'s 1→2 arm.
    ///
    /// The predecessor of this test (`mono_file_still_duplicates_to_both_stereo_sides`)
    /// kept passing after the fold was removed, for the wrong reason: it read a
    /// flattened `[[f32; 2]]` buffer, so `out[0]` was two *consecutive frames*
    /// of a constant signal rather than one duplicated frame, and `out[0][0] ==
    /// out[0][1]` held either way. Asserting the width is what makes it real.
    #[test]
    fn a_mono_file_is_not_widened() {
        let frames = 64;
        let sample_rate = 44100.0;
        let mut wave = Wave::zero(1, sample_rate, frames as f64 / sample_rate);
        // A ramp, not a constant: a constant cannot distinguish "one frame
        // duplicated" from "two frames read", which is exactly how the old test
        // fooled itself.
        for i in 0..frames {
            wave.set(0, i, i as f32 / frames as f32);
        }
        let mut path = std::env::temp_dir();
        path.push("tutti_stream_mono_native.wav");
        wave.save_wav16(&path).expect("save wav");

        let mut decoder = FileIn::open(&path, None).expect("open");
        assert_eq!(decoder.channels(), 1);
        assert_eq!(decoder.layout(), ChannelLayout::MONO, "no widening");

        let mut out = [0.0f32; 16];
        let got = decoder.poll_into(&mut out);
        assert!(!got.is_zero());

        // One sample per frame, so consecutive slots differ by one ramp step.
        // Under the old duplicating impl they would have come in equal pairs.
        assert!(
            (out[1] - out[0]).abs() > 1e-3,
            "consecutive samples are equal — is this still duplicating? {:?}",
            &out[..4]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A caller that *wants* stereo still gets the engine's fold — it just asks
    /// for it. This is the replacement path named in the impl docs, exercised so
    /// the removal shipped with a working substitute rather than a promise.
    #[test]
    fn a_caller_can_still_fold_to_stereo_itself() {
        let frames = 64;
        let sample_rate = 44100.0;
        let mut wave = Wave::zero(6, sample_rate, frames as f64 / sample_rate);
        for i in 0..frames {
            wave.set(2, i, 0.5); // centre only, as above
        }
        let mut path = std::env::temp_dir();
        path.push("tutti_stream_caller_fold.wav");
        wave.save_wav16(&path).expect("save wav");

        let mut decoder = FileIn::open(&path, None).expect("open");
        let ch = decoder.layout().count() as usize;
        let mut native = vec![0.0f32; ch * 16];
        let got = decoder.poll_into(&mut native);
        assert!(!got.is_zero());

        let mut stereo = [0.0f32; 2];
        tutti_types::fold_frame(&native[..ch], &mut stereo);

        // The centre reaches both sides rather than being dropped with the
        // surrounds — the property the old in-decoder fold guaranteed.
        assert!(
            stereo[0].abs() > 0.1 && stereo[1].abs() > 0.1,
            "centre was lost in the caller-side fold: {stereo:?}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
