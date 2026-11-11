//! A spectrum step allocates nothing once its state is built.
//!
//! Counted with this binary's own global allocator, as in `chroma_alloc.rs`. It
//! counts per thread, so the harness's other threads do not show up.

use tutti_dsp::{FftScratch, FrameCount, StftGeometry};
use tutti_fixtures::{ThreadCounting, allocations};
use tutti_measure::{Banding, Smoothing, SpectrumConfig, SpectrumState, step_spectrum};
use tutti_types::{Db, Hz, SampleRate, Samples, Seconds};

#[global_allocator]
static A: ThreadCounting = ThreadCounting;

/// Construction allocates (so the counter is live); then many frames across
/// block sizes that split frames, a reset, and more frames allocate nothing,
/// for bins and for bands, with smoothing, peaks and a wrapping spectrogram.
#[test]
fn a_spectrum_step_does_not_allocate() {
    let geometry =
        StftGeometry::new(SampleRate::new(48_000.0), Samples(1024), Samples(256)).unwrap();
    let plain = SpectrumConfig::new(geometry).unwrap();
    let banded = plain
        .with_banding(Banding::Octave {
            per_octave: 3,
            min: Hz(400.0),
            max: Hz(20_000.0),
        })
        .unwrap()
        .with_smoothing(Smoothing::Average(Seconds(0.2)))
        .unwrap()
        .with_peak_hold(Seconds(0.5), Db(20.0))
        .unwrap()
        .with_spectrogram(FrameCount(7))
        .unwrap();
    let signal: Vec<f32> = (0..48_000)
        .map(|i| 0.4 * (std::f32::consts::TAU * 1234.5 * i as f32 / 48_000.0).sin())
        .collect();

    for cfg in [plain, banded] {
        let mut fft = FftScratch::new();
        let mut state = None;
        let built = allocations(|| state = Some(SpectrumState::new(&cfg, &mut fft)));
        assert!(built > 0, "construction builds the buffers");
        let mut state = state.unwrap();

        let steps = allocations(|| {
            for size in [1, 100, 256, 777, 4096] {
                for block in signal.chunks(size) {
                    step_spectrum(&cfg, &mut state, block, &mut fft).unwrap();
                }
            }
        });
        assert_eq!(steps, 0, "steps");
        assert!(state.frames_seen() > 300);

        let after_reset = allocations(|| {
            state.reset();
            for block in signal.chunks(500) {
                step_spectrum(&cfg, &mut state, block, &mut fft).unwrap();
            }
            let _ = (state.levels(), state.peaks(), state.spectrogram().count());
        });
        assert_eq!(after_reset, 0, "reset and steps");
    }
}
