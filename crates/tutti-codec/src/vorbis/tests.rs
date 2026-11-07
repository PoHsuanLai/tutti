use super::*;
use std::io::{Cursor, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_types::SampleRate;

/// A sink that is slow to write and says when it is dropped.
struct Watched {
    inner: Cursor<Vec<u8>>,
    dropped: Arc<AtomicBool>,
}

impl Write for Watched {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::thread::sleep(std::time::Duration::from_millis(15));
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Watched {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// Dropping the writer stops its thread before `drop` returns, so the sink (a
/// file, in a staged encoder) is closed by then. The sink is slow, so the
/// thread is still working through its queue when the drop starts.
///
/// Mutation (run): deleting the `Drop` impl returns from `drop` with the thread
/// still encoding, the sink not yet dropped, and fails the assertion.
#[test]
fn a_dropped_writer_has_stopped_its_thread() {
    let dropped = Arc::new(AtomicBool::new(false));
    let spec = Spec::new(
        crate::Format::Vorbis {
            quality: Quality::LOWEST,
        },
        ChannelLayout::STEREO,
        SampleRate(44_100.0),
    );
    let sink = Watched {
        inner: Cursor::new(Vec::new()),
        dropped: Arc::clone(&dropped),
    };
    let mut w = VorbisWriter::create(&spec, Quality::LOWEST, sink).unwrap();
    let samples: Vec<f32> = (0..8_192 * 2)
        .map(|i| ((i * 7) % 255) as f32 / 255.0 - 0.5)
        .collect();
    for _ in 0..4 {
        w.write(Interleaved::new(&samples, ChannelLayout::STEREO))
            .unwrap();
    }
    drop(w);
    assert!(
        dropped.load(Ordering::SeqCst),
        "the thread outlived the drop"
    );
}
