use super::*;
use hound::SampleFormat;
use tutti_codec::Error as CodecError;

/// A failed open reports *why*, and takes a path without ceremony.
///
/// The kind matters: a caller distinguishing "make the directory and
/// retry" from "give up" can only do so from the error.
#[test]
fn a_failed_open_reports_the_cause() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-dir").join("take.wav");

    let err = WavOut::create(&missing, 48_000.0, 2u16, Pcm::Float32)
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

    let mut sink = WavOut::create(&path, 48_000.0, 2u16, Pcm::Float32).expect("sink should open");

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

/// A 6-channel sink must write SIX samples per frame, matching the header it
/// declared. If the header takes the layout width while `write` emits two,
/// a reader interleave-misaligns and the channels rotate by `2 mod 6` every
/// frame.
#[test]
fn six_channel_sink_writes_six_samples_per_frame() {
    let dir = std::env::temp_dir();
    let path = dir.join("tutti_wav_out_six_channel.wav");
    let _ = std::fs::remove_file(&path);

    let mut sink = WavOut::create(&path, 48_000.0, 6u16, Pcm::Float32).expect("create 6ch sink");
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

/// A failed write is reported from `finalize`, not swallowed, with its cause.
///
/// This is what the `first_error` field exists for: `AudioOut::write`
/// returns `()`, so a fault mid-recording has nowhere else to surface, and
/// a short file with an `Ok` finalize reads as a complete take.
///
/// The fault is the kernel's own: `/dev/full` refuses every write with
/// `ENOSPC`, which is what a full disk does. The take is large enough to push
/// the buffer through to it, so the failure lands on a write rather than on
/// the final flush.
///
/// Mutation: MUT_FULL
#[cfg(target_os = "linux")]
#[test]
fn a_failed_write_surfaces_from_finalize() {
    let mut sink = WavOut::create("/dev/full", 48_000.0, 2u16, Pcm::Float32)
        .expect("/dev/full opens for writing");

    // 64 KiB of samples: well past the 8 KiB buffer.
    sink.write(&vec![0.5; 16 * 1024]);
    assert!(
        sink.is_failed(),
        "the full device must have refused a write"
    );

    let err = sink
        .finalize()
        .expect_err("a failed write must be reported, not swallowed");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::StorageFull,
        "the cause rides out as the OS gave it, not flattened: {err}"
    );
}

/// After a failure the sink stops writing and the bytes that did land stay
/// readable — both halves of what `finalize`'s error path promises.
///
/// The short-circuit at the top of `write_interleaved` is otherwise
/// untested: deleting it would leave every other test green while the sink
/// kept hammering a writer it already knows is broken. The failure is the
/// frame limit, lowered to two frames: the same latch a full disk sets.
///
/// Mutation: MUT_STOPS
#[test]
fn a_failed_sink_stops_writing_and_leaves_a_readable_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.wav");
    let mut sink = WavOut::create(&path, 48_000.0, 2u16, Pcm::Int16)
        .expect("create")
        .with_max_frames_for_test(2);

    sink.write(&[0.001, -0.001, 0.001, -0.001]);
    // The third frame is past the limit, so this fails.
    sink.write(&[0.5, -0.5]);
    assert!(sink.is_failed(), "the third frame must have failed");

    // Everything after the failure is ignored rather than retried.
    sink.write(&[0.001, -0.001, 0.001, -0.001, 0.001, -0.001]);

    let err = sink.finalize().expect_err("the failure must be reported");
    assert!(
        err.get_ref()
            .and_then(|e| e.downcast_ref::<CodecError>())
            .is_some_and(|e| matches!(e, CodecError::WavTooLarge { limit_frames: 2 })),
        "carrying the typed error, got: {err}"
    );

    // The prefix survived: finalize still back-patched the header, so the
    // frames written before the fault are recoverable rather than lost.
    let reader = hound::WavReader::open(&path).expect("a partially-written take must still open");
    assert_eq!(
        reader.len(),
        4,
        "exactly the two good frames (2 samples each), and nothing written after the failure"
    );
}

/// 16-bit capture round-trips through a real file, so the header and the
/// data have to agree.
#[test]
fn sixteen_bit_capture_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("i16.wav");

    let mut sink = WavOut::create(&path, 44_100.0, 2u16, Pcm::Int16).expect("sink should open");
    sink.write(&[1.0, -1.0, 0.0, 0.0]);
    sink.finalize().expect("finalize");

    let mut reader = hound::WavReader::open(&path).expect("readable");
    assert_eq!(reader.spec().bits_per_sample, 16);
    assert_eq!(reader.spec().sample_format, SampleFormat::Int);

    let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
    // Full scale quantizes through the shared `f32_to_i16`, not a local copy.
    assert_eq!(samples, vec![32767, -32767, 0, 0]);
}

/// `Int24` — the default depth, and so the one a caller gets by writing
/// `Pcm::default()` — round-trips through a real file.
#[test]
fn twenty_four_bit_capture_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("i24.wav");

    let mut sink = WavOut::create(&path, 48_000.0, 2u16, Pcm::Int24).expect("sink should open");
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
/// Mutation: MUT_MASK
#[test]
fn a_multichannel_file_declares_the_mask_of_tutti_s_channel_order() {
    let dir = tempfile::tempdir().unwrap();
    for (channels, depth, mask) in [
        (4u16, Pcm::Float32, 0u32),
        (6, Pcm::Float32, 0x60F),
        (6, Pcm::Int24, 0x60F),
        (8, Pcm::Float32, 0),
        (3, Pcm::Int16, 0),
        // Extensible because of the depth; hound would write 0x1.
        (1, Pcm::Int24, 0x4),
        (2, Pcm::Float32, 0x3),
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
/// channels with `ChannelTopology::quad()` carry `0x33`. (A topology of
/// another width than the file cannot be asked for: the constructor takes the
/// width from the topology.)
///
/// Mutation: MUT_QUAD
#[test]
fn a_declared_quad_topology_sets_the_quad_mask() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quad.wav");
    let sink = WavOut::create_with_topology(&path, 48_000.0, ChannelTopology::quad(), Pcm::Float32)
        .unwrap();
    assert_eq!(sink.layout(), ChannelLayout::from(4u16));
    sink.write_interleaved_and_finish_for_test();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 0x33);
}

/// A header with no mask (stereo, 16 bit) is the plain `PCMWAVEFORMAT` one.
///
/// Mutation: MUT_PLAIN
#[test]
fn a_plain_header_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.wav");
    let mut sink = WavOut::create(&path, 44_100.0, 2u16, Pcm::Int16).unwrap();
    sink.write_interleaved(&[0.5, -0.5, 0.25, -0.25]);
    sink.finalize().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[20..22], &1u16.to_le_bytes());
    assert_eq!(hound::WavReader::open(&path).unwrap().len(), 4);
}

/// The write that would pass the frame limit stops the sink; `finalize`
/// reports a typed [`CodecError::WavTooLarge`] and the file holds exactly the
/// frames under the limit, with correct header sizes and mask. Later writes
/// are ignored.
///
/// The limit is lowered through `with_max_frames_for_test`; the real one is
/// pinned in `tutti-codec`.
///
/// Mutation: MUT_LIMIT
#[test]
fn the_frame_limit_stops_at_a_whole_frame_and_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("limit.wav");
    let mut sink = WavOut::create(&path, 48_000.0, 6u16, Pcm::Int24)
        .unwrap()
        .with_max_frames_for_test(3);
    // Eight frames, in two writes, the limit falling in the first.
    let block: Vec<f32> = (0..5 * 6).map(|i| i as f32 * 0.001).collect();
    sink.write_interleaved(&block);
    sink.write_interleaved(&block[..3 * 6]);
    assert!(sink.is_failed());

    let err = sink.finalize().expect_err("the limit must be reported");
    let limit = err
        .get_ref()
        .and_then(|e| e.downcast_ref::<CodecError>())
        .expect("the typed error rides out of finalize");
    assert!(matches!(limit, CodecError::WavTooLarge { limit_frames: 3 }));

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

/// A take that ends exactly at the limit is not an error.
///
/// Mutation: MUT_EXACT
#[test]
fn a_take_that_ends_at_the_limit_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("exact.wav");
    let mut sink = WavOut::create(&path, 48_000.0, 2u16, Pcm::Float32)
        .unwrap()
        .with_max_frames_for_test(4);
    sink.write_interleaved(&[0.1; 8]);
    sink.finalize().expect("exactly the limit is fine");
    assert_eq!(hound::WavReader::open(&path).unwrap().len(), 8);
}
