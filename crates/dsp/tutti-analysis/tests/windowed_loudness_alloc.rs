//! Whether a windowed-loudness step allocates once its state is built.
//!
//! `ebur128` does not document it, so this counts: it feeds whole, ragged and
//! empty chunks and reads both windows, with the crate's own global allocator
//! as in `chroma_alloc.rs`. It counts per thread, so the harness's
//! other threads do not show up.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use tutti_analysis::{WindowedLoudnessConfig, WindowedLoudnessState, step_windowed_loudness};
use tutti_types::{ChannelLayout, Interleaved, SampleRate};

struct Counting;

thread_local! {
    // Per thread: the harness's own threads allocate while a test counts.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
    // `try_with`: an allocation during thread teardown finds no slot.
    let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
}

// SAFETY: every method forwards to `System` unchanged; the counter is the only
// addition.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn stereo(s: &[f32]) -> Interleaved<'_> {
    Interleaved::new(s, ChannelLayout::STEREO)
}

fn allocations(f: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.with(Cell::get);
    f();
    ALLOCATIONS.with(Cell::get) - before
}

/// Steps after construction allocate nothing, the first included: frame-aligned chunks, a ragged
/// sequence that splices a part frame every call, an empty chunk, and a reset.
///
/// This is a measurement, not a guarantee of `ebur128`: it holds for the
/// version in `Cargo.lock`. Mutation (run):
/// `std::hint::black_box(Vec::<u8>::with_capacity(8));` as a statement in
/// `step_windowed_loudness` before the feed -> every case counts allocations.
#[test]
fn a_windowed_loudness_step_does_not_allocate() {
    let cfg = WindowedLoudnessConfig::new(SampleRate(48_000.0), ChannelLayout::STEREO).unwrap();
    let mut state = WindowedLoudnessState::new(&cfg).unwrap();
    let tone: Vec<f32> = (0..48_000 * 2)
        .map(|i| 0.1 * (std::f32::consts::TAU * 997.0 * (i / 2) as f32 / 48_000.0).sin())
        .collect();

    // The first step too, and then enough to fill the three-second window so
    // every getter reads a full one.
    let first = allocations(|| {
        for _ in 0..4 {
            step_windowed_loudness(&cfg, &mut state, stereo(&tone)).unwrap();
        }
    });
    assert_eq!(first, 0, "the first steps, which fill the windows");

    let aligned = allocations(|| {
        for chunk in tone.chunks(512) {
            step_windowed_loudness(&cfg, &mut state, stereo(chunk)).unwrap();
        }
    });
    assert_eq!(aligned, 0, "frame-aligned chunks");

    // 777 samples is an odd count: every call ends mid-frame and the next
    // completes it.
    let ragged = allocations(|| {
        for chunk in tone.chunks(777) {
            step_windowed_loudness(&cfg, &mut state, stereo(chunk)).unwrap();
        }
    });
    assert_eq!(ragged, 0, "ragged chunks");

    let empty = allocations(|| {
        step_windowed_loudness(&cfg, &mut state, stereo(&[])).unwrap();
    });
    assert_eq!(empty, 0, "an empty chunk");

    let reset = allocations(|| {
        state.reset();
        step_windowed_loudness(&cfg, &mut state, stereo(&tone)).unwrap();
    });
    assert_eq!(reset, 0, "a reset and a step");
}
