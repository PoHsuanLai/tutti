use super::*;

#[test]
fn clamps_out_of_range_input() {
    assert_eq!(f32_to_i32(2.0, BitDepth::Int16), 32767);
    assert_eq!(f32_to_i32(-2.0, BitDepth::Int16), -32767);
}

/// FLAC must quantize exactly as the rest of the engine does.
///
/// The two tests above pass under truncation *and* under rounding, because
/// 0.0 and ±1.0 land on exact integers either way — which is how a private
/// truncating converter survives unnoticed. These levels have a fractional
/// part above 0.5, so the two disagree: 0.7 × 32767 = 22936.9, which
/// truncates to 22936 and rounds to 22937.
#[test]
fn quantization_matches_the_engines_canonical_converter() {
    for level in [0.7f32, -0.7, 0.3, -0.3, 0.123_45, 0.999] {
        assert_eq!(
            f32_to_i32(level, BitDepth::Int16),
            i32::from(tutti_types::f32_to_i16(level)),
            "Int16: FLAC and tutti_types::pcm disagree on {level}"
        );
        assert_eq!(
            f32_to_i32(level, BitDepth::Int24),
            tutti_types::f32_to_i24(level),
            "Int24: FLAC and tutti_types::pcm disagree on {level}"
        );
    }
}

fn configs(panic_at: Option<usize>) -> Configs {
    Configs {
        full: encoder_config(5).into_verified().unwrap(),
        short: short_block_config(5).into_verified().unwrap(),
        panic_at,
    }
}

/// A worker that panics ends the export with an error naming the panic,
/// within bounded time, instead of leaving the producer waiting on a
/// frame that never comes. 200 blocks keep the queue full, so the
/// producer is blocked on `done` when the panic lands.
///
/// Mutation (run): drop the `catch_unwind` (encode directly, the hook's
/// `panic!` unwinding the worker) → the export never returns and the
/// 30 s guard below fails the test.
#[test]
#[cfg_attr(miri, ignore = "spawns encoder threads and waits on a clock")]
fn a_panicking_worker_ends_the_export_with_an_error() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let configs = configs(Some(1));
        let info = StreamInfo::new(48_000, 2, 16).unwrap();
        let pcm = vec![0i32; BLOCK_SIZE * 2];
        let mut out = Vec::new();
        let result = std::thread::scope(|scope| {
            let mut writer = FrameWriter::start(scope, 4, &configs, info, 2, 16);
            let pumped = (0..200).try_for_each(|_| writer.block(&pcm, &mut out));
            let finished = writer.finish(&mut out);
            pumped.and(finished)
        });
        let _ = tx.send(result.map(|_| ()).map_err(|e| e.to_string()));
    });
    let result = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the export hung on a panicked worker");
    let err = result.expect_err("a panicked frame cannot be written");
    assert!(err.contains("injected at frame 1"), "{err}");
}

/// An empty export states valid block sizes (the stream's, 4 096 both)
/// and "unknown" frame sizes, not flacenc's unset 65 535 / 0 and
/// `u32::MAX` / 0.
///
/// Mutation (run): set the block sizes only when a frame was written
/// (`if self.written > 0`) → min 65 535 → fails.
#[test]
#[cfg_attr(miri, ignore = "spawns encoder threads")]
fn an_empty_stream_states_valid_sizes() {
    let configs = configs(None);
    let info = StreamInfo::new(48_000, 2, 16).unwrap();
    let mut out = Vec::new();
    let info = std::thread::scope(|scope| {
        FrameWriter::start(scope, 2, &configs, info, 2, 16).finish(&mut out)
    })
    .unwrap();
    assert!(out.is_empty(), "no frames");
    assert_eq!(
        (info.min_block_size(), info.max_block_size()),
        (BLOCK_SIZE, BLOCK_SIZE)
    );
    assert_eq!((info.min_frame_size(), info.max_frame_size()), (0, 0));
    assert_eq!(info.total_samples(), 0);
    assert_eq!(header_bytes(&info).unwrap().len(), 42);
}
