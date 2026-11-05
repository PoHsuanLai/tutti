//! A spectrum step allocates nothing once its state is built.
//!
//! Counted with this binary's own global allocator, as in `chroma_alloc.rs`. It
//! holds a single test, so no other thread allocates while it counts.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use tutti_analysis::{
    Banding, FftScratch, Smoothing, SpectrumConfig, SpectrumState, StftGeometry, step_spectrum,
};
use tutti_types::{Db, Hz, Samples, Seconds};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to `System` unchanged; the counter is the only
// addition.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn allocations(f: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.load(Ordering::SeqCst);
    f();
    ALLOCATIONS.load(Ordering::SeqCst) - before
}

/// Construction allocates (so the counter is live); then many frames across
/// block sizes that split frames, a reset, and more frames allocate nothing,
/// for bins and for bands, with smoothing, peaks and a wrapping spectrogram.
///
/// Mutations (run):
/// - `reset` as `*self = Self::new(..)`-style reallocation (`self.power =
///   vec![..]`) -> the reset case counts allocations.
/// - an `FftScratch` not planned in `SpectrumState::new` (drop the warming
///   `fft.forward`) -> the first step counts allocations.
#[test]
fn a_spectrum_step_does_not_allocate() {
    let geometry = StftGeometry::new(48_000.0, Samples(1024), Samples(256)).unwrap();
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
        .with_spectrogram(7)
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
            let _ = (state.bins(), state.peaks(), state.spectrogram().count());
        });
        assert_eq!(after_reset, 0, "reset and steps");
    }
}
