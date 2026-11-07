use super::*;

fn configs(panic_at: Option<usize>) -> Configs {
    Configs {
        full: encoder_config(FlacLevel::Standard).into_verified().unwrap(),
        short: short_block_config(FlacLevel::Standard)
            .into_verified()
            .unwrap(),
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
        let configs = Arc::new(configs(Some(1)));
        let info = StreamInfo::new(48_000, 2, 16).unwrap();
        let pcm = vec![0i32; BLOCK_SIZE * 2];
        let mut out = Vec::new();
        let mut writer = FrameWriter::start(4, configs, info, 2, 16);
        let pumped = (0..200).try_for_each(|_| writer.block(&pcm, &mut out));
        let finished = writer.finish(&mut out);
        let result = pumped.and(finished);
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
    let configs = Arc::new(configs(None));
    let info = StreamInfo::new(48_000, 2, 16).unwrap();
    let mut out = Vec::new();
    let info = FrameWriter::start(2, configs, info, 2, 16)
        .finish(&mut out)
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

/// After a frame is lost to a sink failure the writer neither waits for it nor
/// queues more: `write` and `finish` return errors promptly. (It once waited for
/// the lost frame forever once `2 x workers` blocks were in flight.)
///
/// Mutation (run): never setting the `failed` flag (`self.failed = false`)
/// fails it.
#[test]
#[cfg_attr(miri, ignore = "spawns encoder threads and waits on a clock")]
fn a_writer_that_lost_a_frame_fails_promptly() {
    use crate::test_sink::Flaky;
    use crate::{FormatWriter, Spec};
    use tutti_types::{ChannelLayout, Interleaved, SampleRate};

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let spec = Spec::new(
            crate::Format::Flac {
                depth: IntDepth::Int16,
                level: FlacLevel::Standard,
            },
            ChannelLayout::STEREO,
            SampleRate(44_100.0),
        );
        let mut w = FlacWriter::create(
            &spec,
            IntDepth::Int16,
            FlacLevel::Standard,
            Flaky::failing_write(3),
        )
        .unwrap();
        let samples: Vec<f32> = (0..BLOCK_SIZE * 2)
            .map(|i| ((i * 31) % 255) as f32 / 255.0 - 0.5)
            .collect();
        let mut failed = false;
        for _ in 0..200 {
            if w.write(Interleaved::new(&samples, ChannelLayout::STEREO))
                .is_err()
            {
                failed = true;
                break;
            }
        }
        // More writes and the finish must not wait on the lost frame.
        let again = w
            .write(Interleaved::new(&samples, ChannelLayout::STEREO))
            .is_err();
        let finished = w.finish().is_err();
        let _ = tx.send((failed, again, finished));
    });
    let (failed, again, finished) = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the writer waited for a frame that was lost");
    assert!(failed && again && finished);
}
