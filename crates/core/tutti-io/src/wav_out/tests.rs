use super::*;

/// A failed open reports *why*, and takes a path without ceremony.
///
/// The kind matters: a caller distinguishing "make the directory and
/// retry" from "give up" can only do so from the error.
#[test]
fn a_failed_open_reports_the_cause() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-dir").join("take.wav");

    let err = WavOut::create(&missing, 48_000.0, 2u16, BitDepth::Float32)
        .expect_err("a file under a missing directory cannot be created");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "the cause must survive to the caller, not flatten to a sentinel"
    );
}

/// The sink writes INCREMENTALLY: feeding frames across many `write` calls
/// and finalizing must yield a valid WAV whose frame count is the sum of
/// every block — the sink never has to see the whole recording at once.
#[test]
fn wav_out_writes_incrementally_and_finalizes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capture.wav");

    let mut sink =
        WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32).expect("sink should open");

    // Flat interleaved stereo: 256 frames, 512 samples.
    let block: Vec<f32> = (0..256)
        .flat_map(|i| [i as f32 / 256.0, -(i as f32) / 256.0])
        .collect();
    let blocks = 5;
    for _ in 0..blocks {
        sink.write(&block);
    }
    sink.finalize()
        .expect("finalize should back-patch the header");

    let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    let spec = reader.spec();
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.sample_rate, 48_000);
    assert_eq!(reader.len() as usize, block.len() * blocks);
}

/// A stereo source folded into a mono sink must AVERAGE `(l + r) * 0.5`,
/// not drop the right channel.
///
/// Asymmetric inputs are what make the two distinguishable — with
/// `[0.5, 0.5]` a dropped channel and an average agree, which is exactly how
/// a channel-dropping mono fold survives a suite that already covers mono.
#[test]
fn folding_to_mono_averages_rather_than_dropping_the_right_channel() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fold_mono.wav");

    let mut sink = WavOut::create(&path, 48_000.0, ChannelLayout::MONO, BitDepth::Float32)
        .expect("sink should open");
    // L=0.5 R=0.9 → average 0.7. Dropping R would write 0.5.
    sink.write_folding(&[0.5, 0.9, 1.0, 0.0], ChannelLayout::STEREO);
    sink.finalize().unwrap();

    let mut reader = hound::WavReader::open(&path).unwrap();
    let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
    assert_eq!(samples.len(), 2, "two stereo frames → two mono samples");
    assert!(
        (samples[0] - 0.7).abs() < 1e-6,
        "expected the AVERAGE 0.7, got {} — the right channel was dropped",
        samples[0]
    );
    assert!(
        (samples[1] - 0.5).abs() < 1e-6,
        "expected 0.5, got {}",
        samples[1]
    );
}

/// A 5.1 frame folded to stereo or mono must keep the centre and surrounds,
/// not truncate to channels 0/1.
///
/// A centre-only frame is the sharpest case: truncation writes pure
/// silence, so the assertion is "any signal at all reached the file". This
/// is the same fold the mic callback applies, exercised at the one seam
/// that needs no hardware.
#[test]
fn folding_a_surround_frame_keeps_the_centre_and_surrounds() {
    let dir = tempfile::tempdir().unwrap();

    // 5.1, energy ONLY in the centre (idx 2). FL FR C LFE SL SR.
    let centre_only = [0.0f32, 0.0, 1.0, 0.0, 0.0, 0.0];

    // → stereo: centre must reach BOTH sides at −3 dB.
    let stereo_path = dir.path().join("surround_to_stereo.wav");
    let mut stereo = WavOut::create(
        &stereo_path,
        48_000.0,
        ChannelLayout::STEREO,
        BitDepth::Float32,
    )
    .expect("sink should open");
    stereo.write_folding(&centre_only, ChannelLayout::from(6u16));
    stereo.finalize().unwrap();

    let mut reader = hound::WavReader::open(&stereo_path).unwrap();
    let s: Vec<f32> = reader.samples::<f32>().map(|x| x.unwrap()).collect();
    assert_eq!(s.len(), 2);
    assert!(
        s[0] > 0.1 && s[1] > 0.1,
        "the centre channel (dialogue) was truncated away: {s:?}"
    );
    assert!(
        (s[0] - s[1]).abs() < 1e-6,
        "a centre source must fold symmetrically"
    );

    // → mono: still non-silent. A rear-only 7.1 frame likewise.
    let mono_path = dir.path().join("surround_to_mono.wav");
    let mut mono = WavOut::create(&mono_path, 48_000.0, ChannelLayout::MONO, BitDepth::Float32)
        .expect("sink should open");
    // 7.1 with energy only in the rears (idx 6, 7) — front-pair truncation
    // would write silence here too.
    let rears_only = [0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0];
    mono.write_folding(&rears_only, ChannelLayout::from(8u16));
    mono.finalize().unwrap();

    let mut reader = hound::WavReader::open(&mono_path).unwrap();
    let m: Vec<f32> = reader.samples::<f32>().map(|x| x.unwrap()).collect();
    assert_eq!(m.len(), 1);
    assert!(
        m[0] > 0.1,
        "the surround channels were truncated away: got {}",
        m[0]
    );
}

/// `write_folding` never emits a short frame, whatever the widths.
///
/// Alignment is the invariant a fold must not trade away: a frame short by
/// even one sample rotates every channel for the rest of the file, which
/// reads as "the recording is subtly wrong" rather than as an error.
#[test]
fn folding_always_emits_whole_frames() {
    let dir = tempfile::tempdir().unwrap();
    for (src_w, dst_w) in [(2u16, 6u16), (6, 2), (1, 4), (6, 1), (2, 12)] {
        let path = dir.path().join(format!("align_{src_w}_{dst_w}.wav"));
        let dst = ChannelLayout::from(dst_w);
        let mut sink =
            WavOut::create(&path, 48_000.0, dst, BitDepth::Float32).expect("sink should open");

        const FRAMES: usize = 7; // odd, so a stride slip cannot alias
        let src = vec![0.25f32; FRAMES * src_w as usize];
        sink.write_folding(&src, ChannelLayout::from(src_w));
        sink.finalize().unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, dst_w);
        assert_eq!(
            reader.len() as usize,
            FRAMES * dst_w as usize,
            "{src_w}ch → {dst_w}ch must write whole frames"
        );
    }
}

/// A 6-channel sink must write SIX samples per frame, matching the header it
/// declared. If the header takes the layout width while `write` emits two,
/// a reader interleave-misaligns and the channels rotate by `2 mod 6` every
/// frame.
#[test]
fn six_channel_sink_writes_six_samples_per_frame() {
    let dir = std::env::temp_dir();
    let path = dir.join("tutti_wav_out_six_channel.wav");
    let _ = std::fs::remove_file(&path);

    let mut sink =
        WavOut::create(&path, 48_000.0, 6u16, BitDepth::Float32).expect("create 6ch sink");
    assert_eq!(sink.layout(), ChannelLayout::from(6u16));

    let frames: Vec<f32> = (0..128)
        .flat_map(|i| (0..6).map(move |c| (i * 6 + c) as f32 * 0.001))
        .collect();
    sink.write_interleaved(&frames);
    AudioOut::finalize(sink).expect("finalize");

    let reader = hound::WavReader::open(&path).expect("reopen");
    assert_eq!(reader.spec().channels, 6, "header must declare 6 channels");
    assert_eq!(
        reader.len() as usize,
        128 * 6,
        "128 frames of 6 channels must write 768 samples, not 256"
    );
    let _ = std::fs::remove_file(&path);
}

/// A stereo source folded into a wider declared width produces a well-formed
/// file: it zero-fills the channels it cannot supply rather than emitting
/// short frames, and never synthesises an upmix.
#[test]
fn stereo_write_into_a_wide_sink_stays_frame_aligned() {
    let dir = std::env::temp_dir();
    let path = dir.join("tutti_wav_out_stereo_into_quad.wav");
    let _ = std::fs::remove_file(&path);

    let mut sink =
        WavOut::create(&path, 48_000.0, 4u16, BitDepth::Float32).expect("create 4ch sink");
    let frames: Vec<f32> = std::iter::repeat_n([0.25f32, -0.25], 16)
        .flatten()
        .collect();
    sink.write_folding(&frames, ChannelLayout::STEREO);
    AudioOut::finalize(sink).expect("finalize");

    let mut reader = hound::WavReader::open(&path).expect("reopen");
    assert_eq!(reader.spec().channels, 4);
    assert_eq!(
        reader.len() as usize,
        16 * 4,
        "must be a whole number of frames"
    );
    let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
    assert!((samples[0] - 0.25).abs() < 1e-6);
    assert!((samples[1] + 0.25).abs() < 1e-6);
    assert_eq!(samples[2], 0.0, "unsupplied channels are silent");
    assert_eq!(samples[3], 0.0);
    let _ = std::fs::remove_file(&path);
}

/// A failed write is reported from `finalize`, not swallowed.
///
/// This is what the `first_error` field exists for: `AudioOut::write`
/// returns `()`, so a fault mid-recording has nowhere else to surface, and
/// a short file with an `Ok` finalize reads as a complete take.
///
/// The fault is provoked deterministically rather than simulated: an 8-bit
/// header accepts creation, but `hound` rejects any `i16` too wide for it
/// with `TooWide` on the *write*. Quantizing at `Int16` into that header
/// therefore fails at the first loud sample — a real mid-stream write
/// failure, which is exactly the shape a disk-full has.
#[test]
fn a_failed_write_surfaces_from_finalize() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("failed.wav");

    let mut sink = WavOut {
        writer: {
            let spec = WavSpec {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: 8,
                sample_format: SampleFormat::Int,
            };
            let file = File::create(&path).expect("create");
            WavWriter::new(BufWriter::new(file), spec).expect("header")
        },
        layout: ChannelLayout::from(2usize),
        depth: BitDepth::Int16,
        sample_rate: SampleRate::SR_48K,
        first_error: None,
        topology: None,
        path: path.clone(),
        frames_written: 0,
        max_frames: u64::MAX,
    };

    // Full scale at Int16 is 32767 — far too wide for the 8-bit stream.
    sink.write(&[1.0, -1.0, 1.0, -1.0]);

    let err = sink
        .finalize()
        .expect_err("a failed write must be reported, not swallowed");
    // The underlying `hound::Error` is carried, not flattened to a bare
    // "write failed" — a caller can tell what went wrong.
    assert!(
        err.get_ref()
            .is_some_and(|e| e.downcast_ref::<hound::Error>().is_some()),
        "the hound failure must ride out intact, got: {err}"
    );
}

/// 16-bit capture round-trips through a real file, so the header and the
/// data have to agree.
#[test]
fn sixteen_bit_capture_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("i16.wav");

    let mut sink =
        WavOut::create(&path, 44_100.0, 2u16, BitDepth::Int16).expect("sink should open");
    sink.write(&[1.0, -1.0, 0.0, 0.0]);
    sink.finalize().expect("finalize");

    let mut reader = hound::WavReader::open(&path).expect("readable");
    assert_eq!(reader.spec().bits_per_sample, 16);
    assert_eq!(reader.spec().sample_format, SampleFormat::Int);

    let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
    // Full scale quantizes through the shared `f32_to_i16`, not a local copy.
    assert_eq!(samples, vec![32767, -32767, 0, 0]);
}

/// After a failure the sink stops writing and the bytes that did land stay
/// readable — both halves of what `finalize`'s error path promises.
///
/// The short-circuit at the top of `write_interleaved` is otherwise
/// untested: deleting it would leave every other test green while the sink
/// kept hammering a writer it already knows is broken.
#[test]
fn a_failed_sink_stops_writing_and_leaves_a_readable_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.wav");

    // 8-bit header, Int16 depth: in-range samples write, loud ones fail.
    let mut sink = WavOut {
        writer: {
            let spec = WavSpec {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: 8,
                sample_format: SampleFormat::Int,
            };
            let file = File::create(&path).expect("create");
            WavWriter::new(BufWriter::new(file), spec).expect("header")
        },
        layout: ChannelLayout::from(2usize),
        depth: BitDepth::Int16,
        sample_rate: SampleRate::SR_48K,
        first_error: None,
        topology: None,
        path: path.clone(),
        frames_written: 0,
        max_frames: u64::MAX,
    };

    // Quiet enough to fit in 8 bits: these land.
    sink.write(&[0.001, -0.001, 0.001, -0.001]);
    // Full scale at Int16 is 32767 — too wide, so this fails.
    sink.write(&[1.0, -1.0]);
    assert!(
        sink.first_error.is_some(),
        "the loud frame must have failed"
    );

    // Everything after the failure is ignored rather than retried.
    sink.write(&[0.001, -0.001, 0.001, -0.001, 0.001, -0.001]);

    let err = sink.finalize().expect_err("the failure must be reported");
    assert!(err.get_ref().is_some(), "carrying the hound error");

    // The prefix survived: finalize still back-patched the header, so the
    // frames written before the fault are recoverable rather than lost.
    let reader = hound::WavReader::open(&path).expect("a partially-written take must still open");
    assert_eq!(
        reader.len(),
        4,
        "exactly the two good frames (2 samples each), and nothing written after the failure"
    );
}

/// `Int24` — the default depth, and so the one a caller gets by writing
/// `BitDepth::default()` — round-trips through a real file.
#[test]
fn twenty_four_bit_capture_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("i24.wav");

    let mut sink =
        WavOut::create(&path, 48_000.0, 2u16, BitDepth::Int24).expect("sink should open");
    sink.write(&[1.0, -1.0, 0.0, 0.0]);
    sink.finalize().expect("finalize");

    let mut reader = hound::WavReader::open(&path).expect("readable");
    assert_eq!(reader.spec().bits_per_sample, 24);
    assert_eq!(reader.spec().sample_format, SampleFormat::Int);

    let samples: Vec<i32> = reader.samples::<i32>().map(|s| s.unwrap()).collect();
    // Full scale through the shared `f32_to_i24`, not a local copy.
    assert_eq!(samples, vec![8_388_607, -8_388_607, 0, 0]);
}

/// The speaker mask of a multichannel file is the one for tutti's order, not
/// `hound`'s lowest-N-bits, and the rest of the header is untouched.
///
/// Read back from the bytes at offset 40, the `dwChannelMask` of the
/// `WAVE_FORMAT_EXTENSIBLE` header. Manual check (not run here): `ffprobe`
/// names the 4-channel file `quad`, the 6-channel one `5.1(side)`, and the
/// 8-channel one `8 channels` (unassigned).
///
/// Mutation: removing the `patch_channel_mask_at` call from `finalize`
/// leaves `0xF`, `0x3F`, `0xFF` and fails every row (run); the limit test
/// asserts the mask on the error path too.
#[test]
fn a_multichannel_file_declares_the_mask_of_tutti_s_channel_order() {
    let dir = tempfile::tempdir().unwrap();
    for (channels, depth, mask) in [
        (4u16, BitDepth::Float32, 0u32),
        (6, BitDepth::Float32, 0x60F),
        (6, BitDepth::Int24, 0x60F),
        (8, BitDepth::Float32, 0),
        (3, BitDepth::Int16, 0),
        // Extensible because of the depth; hound would write 0x1.
        (1, BitDepth::Int24, 0x4),
        (2, BitDepth::Float32, 0x3),
    ] {
        let path = dir
            .path()
            .join(format!("mask_{channels}_{}.wav", depth.bits()));
        let mut sink = WavOut::create(&path, 48_000.0, channels, depth).unwrap();
        sink.write_interleaved(&vec![0.25; 5 * channels as usize]);
        sink.finalize().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[20..22], &0xFFFEu16.to_le_bytes(), "extensible");
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            mask,
            "{channels} channels at {depth:?}"
        );
        // The patch left the file readable and whole.
        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.len() as usize, 5 * channels as usize);
    }
}

/// Quad is declared only by a caller that says it is quad: the same four
/// channels with `ChannelTopology::quad()` carry `0x33`, and a topology of
/// another width is refused.
///
/// Mutation: ignoring `self.topology` in `mask` leaves `0` and fails the
/// quad assertion; deleting the width check in `with_topology` fails the
/// refusal.
#[test]
fn a_declared_quad_topology_sets_the_quad_mask() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quad.wav");
    let sink = WavOut::create(&path, 48_000.0, 4u16, BitDepth::Float32)
        .unwrap()
        .with_topology(ChannelTopology::quad())
        .unwrap();
    sink.write_interleaved_and_finish_for_test();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 0x33);

    let err = WavOut::create(dir.path().join("x.wav"), 48_000.0, 2u16, BitDepth::Float32)
        .unwrap()
        .with_topology(ChannelTopology::quad())
        .expect_err("a quad topology is not stereo");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

/// A header with no mask (stereo, 16 bit) is not touched.
///
/// Mutation: dropping the format-tag check in `patch_channel_mask` writes
/// four bytes into the `fmt ` chunk's tail and the data length, and fails
/// the readback.
#[test]
fn a_plain_header_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.wav");
    let mut sink = WavOut::create(&path, 44_100.0, 2u16, BitDepth::Int16).unwrap();
    sink.write_interleaved(&[0.5, -0.5, 0.25, -0.25]);
    sink.finalize().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[20..22], &1u16.to_le_bytes());
    assert_eq!(hound::WavReader::open(&path).unwrap().len(), 4);
}

/// The write that would pass the frame limit stops the sink; `finalize`
/// reports a typed [`DataLimitExceeded`] and the file holds exactly the
/// frames under the limit, with correct header sizes and mask. Later writes
/// are ignored, and a folded write is limited the same way.
///
/// The limit is lowered through `with_max_frames_for_test`; the real one,
/// `wav::max_data_frames`, is pinned in `tutti-types`.
///
/// Mutation: changing `>=` to `>` in `begin_frame` writes four frames and
/// fails the length assertion; deleting the `begin_frame` call in
/// `write_interleaved` writes all eight and fails it too; deleting the one
/// in `write_folding` fails the folded half.
#[test]
fn the_frame_limit_stops_at_a_whole_frame_and_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    for folded in [false, true] {
        let path = dir.path().join(format!("limit_{folded}.wav"));
        let mut sink = WavOut::create(&path, 48_000.0, 6u16, BitDepth::Int24)
            .unwrap()
            .with_max_frames_for_test(3);
        // Eight frames, in two writes, the limit falling in the first.
        let block: Vec<f32> = (0..5 * 6).map(|i| i as f32 * 0.001).collect();
        if folded {
            sink.write_folding(&block, ChannelLayout::from(6u16));
            sink.write_folding(&block[..3 * 6], ChannelLayout::from(6u16));
        } else {
            sink.write_interleaved(&block);
            sink.write_interleaved(&block[..3 * 6]);
        }
        assert!(sink.first_error.is_some());

        let err = sink.finalize().expect_err("the limit must be reported");
        let limit = err
            .get_ref()
            .and_then(|e| e.downcast_ref::<DataLimitExceeded>())
            .expect("the typed error rides out of finalize");
        assert_eq!(limit.limit_frames, 3);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[64..68].try_into().unwrap()),
            3 * 6 * 3,
            "data chunk holds three whole frames"
        );
        assert_eq!(bytes.len(), 68 + 3 * 6 * 3);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 0x60F);
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.len(), 18);
        let first: i32 = reader.samples::<i32>().next().unwrap().unwrap();
        assert_eq!(first, 0, "the prefix is the start of the take");
    }
}

/// A take that ends exactly at the limit is not an error.
///
/// Mutation: erroring when `frames_written == max_frames` after the last
/// frame (checking after the write instead of before) fails this.
#[test]
fn a_take_that_ends_at_the_limit_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("exact.wav");
    let mut sink = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32)
        .unwrap()
        .with_max_frames_for_test(4);
    sink.write_interleaved(&[0.1; 8]);
    sink.finalize().expect("exactly the limit is fine");
    assert_eq!(hound::WavReader::open(&path).unwrap().len(), 8);
}
