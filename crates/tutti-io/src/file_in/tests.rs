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
            (got[i][0] - expected[i][0]).abs() < 1e-4 && (got[i][1] - expected[i][1]).abs() < 1e-4,
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
    dec.seek(Samples(start as usize)).expect("seek");
    assert_eq!(dec.cursor(), Samples(start as usize));
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
    dec.seek(Samples(start as usize)).expect("seek");
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
        .join("../../assets/audio")
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
    let mut buf = vec![0.0f32; 1024 * usize::from(dec.layout().count())];
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
    let total = dec.total_frames().expect("flac reports a length").get();
    let mut buf = vec![0.0f32; 1024 * usize::from(dec.layout().count())];
    let mut frames = 0;
    loop {
        match dec.fill_sequential_interleaved(&mut buf) {
            Ok(Samples(0)) => panic!("a fault must surface as Err, not as a bare end"),
            Ok(n) => frames += n.get(),
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
    dec.seek(Samples(0)).expect("seek");
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
        .expect("frames first")
        .get();
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
    assert_eq!(decoder.layout(), ChannelLayout::from(6u16));

    let mut out = vec![0.0f32; 16 * 6];
    let got = decoder
        .fill_sequential_interleaved(&mut out)
        .expect("interleaved read")
        .get();
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
            .expect("read")
            .get();
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
