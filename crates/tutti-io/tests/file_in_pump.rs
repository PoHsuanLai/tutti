//! A pump over a decoded file: `FileIn` is an `AudioIn` whose end and whose
//! fault both read as a zero-frame poll, and the pump must tell them apart.

use tutti_codec::FileIn;
use tutti_io::{ManualDriver, PumpPass, Recorder};
use tutti_types::{AudioIn, AudioOut, ChannelLayout};

struct Discard(ChannelLayout);

impl AudioOut for Discard {
    fn layout(&self) -> ChannelLayout {
        self.0
    }
    fn write(&mut self, _: &[f32]) {}
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
    let (driver, pump) = ManualDriver::new();
    let _rec = Recorder::start_with(src, sink, driver).expect("widths agree");
    loop {
        match pump.pump_once().expect("the take is running") {
            PumpPass::Wrote(_) => {}
            end => return end,
        }
    }
}

/// A pump over a cut file finishes as `Failed`, over a whole one as `Ended`.
///
/// Mutation (run): removing the `is_failed` override from `impl AudioIn for
/// FileIn` makes the cut file finish as `Ended` and fails the first
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
