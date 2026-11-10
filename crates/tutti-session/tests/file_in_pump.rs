//! A pump over a decoded file: `FileIn` is a `Source` whose end (`Ended`) and
//! whose fault (`Failed`) the pump must tell apart.

use tutti_codec::FileIn;
use tutti_session::{PumpLoop, PumpPass};
use tutti_types::{ChannelLayout, Samples, Sink, SinkFailed, Source};

struct Discard(ChannelLayout);

impl Sink for Discard {
    fn layout(&self) -> ChannelLayout {
        self.0
    }
    fn write(&mut self, _: &[f32]) -> Result<(), SinkFailed> {
        Ok(())
    }
    fn finalize(self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The repo's FLAC fixture cut to its first 90 percent, in a directory of the
/// test's own.
fn truncated_flac(dir: &std::path::Path) -> std::path::PathBuf {
    let bytes = std::fs::read(tutti_fixtures::asset("audio/stereo.flac")).expect("fixture");
    let path = dir.join("cut_stereo.flac");
    std::fs::write(&path, &bytes[..bytes.len() * 9 / 10]).expect("write");
    path
}

fn run_to_the_end(path: &std::path::Path) -> PumpPass {
    let src = FileIn::open(path).expect("open");
    let sink = Discard(src.layout());
    let mut pump = PumpLoop::new(src, sink, Samples(1024)).expect("widths agree");
    loop {
        match pump.pump_once() {
            PumpPass::Wrote(_) => {}
            end => return end,
        }
    }
}

/// A pump over a cut file finishes as `Failed`, over a whole one as `Ended`.
///
/// Mutation (run): answering `Ended` instead of `Failed` for a fault in `impl
/// Source for FileIn` makes the cut file finish as `Ended` and fails the first
/// assertion.
#[test]
fn a_pump_over_a_cut_file_finishes_failed_and_over_a_whole_one_ended() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert_eq!(
        run_to_the_end(&truncated_flac(dir.path())),
        PumpPass::Failed
    );
    assert_eq!(
        run_to_the_end(&tutti_fixtures::asset("audio/stereo.flac")),
        PumpPass::Ended
    );
}
