//! A `Design::design` call allocates nothing (design 022 section 8.1, the
//! "pure" case).
//!
//! The crate carries no `assert_no_alloc` dev-dependency (see its Cargo.toml),
//! so this binary counts allocations with its own global allocator, per thread
//! as `chroma_alloc.rs` does. The check is generic over a [`Design`], so a later
//! design runs it on its own controls.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use tutti_dsp::{Design, FreeverbControls, FreeverbDesign};
use tutti_types::{Mix, SampleRate};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
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

fn allocations(f: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.with(Cell::get);
    f();
    ALLOCATIONS.with(Cell::get) - before
}

/// Designing at each of `controls` allocates nothing.
fn design_allocates_nothing<D: Design>(design: &D, controls: &[D::Controls], rate: SampleRate) {
    for c in controls {
        let n = allocations(|| {
            std::hint::black_box(design.design(c, rate));
        });
        assert_eq!(n, 0, "design allocated");
    }
}

/// Freeverb's design, over in-range and hostile controls and several rates,
/// allocates nothing.
///
/// Mutation (run): a `Vec` built inside `FreeverbDesign::design` (for instance
/// `let _v = vec![0u8; 1];`) fails with "design allocated".
#[test]
fn freeverb_design_allocates_nothing() {
    let controls: Vec<FreeverbControls> = [0.0, 0.5, 1.0, f32::NAN, 9.0]
        .iter()
        .flat_map(|&r| {
            [0.0, 0.5, f32::INFINITY].map(|d| FreeverbControls {
                room: Mix(r),
                damp: Mix(d),
            })
        })
        .collect();
    for rate in [44_100.0, 48_000.0, 192_000.0] {
        design_allocates_nothing(&FreeverbDesign, &controls, SampleRate(rate));
        let bounds = allocations(|| {
            std::hint::black_box(
                FreeverbDesign.bounds(&tutti_dsp::Ceiling::at(SampleRate(rate)).unwrap()),
            );
        });
        assert_eq!(bounds, 0, "bounds allocated");
    }
}

/// Freeverb's `design_into`, `retarget` and `process` allocate nothing, with a
/// different network every block (design 022 section 8.2, "no allocation"): the
/// node's audio-thread path, the network designed in place into one built
/// beforehand.
///
/// Mutation (run): a `Vec` built inside `Freeverb::retarget` (for instance
/// `let _v = vec![0u8; 1];`) fails with "the block allocated"; `Vec::new()` in
/// `Inline::push` of the design would too.
#[test]
fn freeverb_retargets_and_renders_without_allocating() {
    use tutti_dsp::{Ceiling, ChannelLayout, Freeverb, Kernel, Network, Setup};
    use tutti_types::{MaxBlock, Samples};

    let rate = SampleRate(48_000.0);
    let design = FreeverbDesign;
    let mut kernel = design.kernel();
    kernel.prepare(
        &design.bounds(&Ceiling::at(rate).unwrap()),
        &Setup {
            rate,
            max_block: MaxBlock::of(Samples(64)),
            input: ChannelLayout::MONO,
            output: ChannelLayout::STEREO,
        },
    );
    let mut network = Box::new(Network::new(rate));
    let input = vec![0.25f32; 64];
    let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
    for block in 0..64u32 {
        let c = FreeverbControls {
            room: Mix((block % 8) as f32 / 7.0),
            damp: Mix(((block / 8) % 8) as f32 / 7.0),
        };
        let n = allocations(|| {
            design.design_into(&c, rate, &mut network);
            kernel.retarget(&network);
            Kernel::process(&mut kernel, &[&input], &mut [&mut left, &mut right]);
        });
        assert_eq!(n, 0, "the block allocated");
    }
    let _: Freeverb = kernel;
}

/// `design_into` builds no value of the network's size (about 56 KB): it runs
/// on a 96 KiB stack, as an audio thread's callback may. An unoptimised build
/// copies a network about three times, so building one needs well over that.
///
/// Mutation (run): `out.clear(rate)` in `FreeverbDesign::design_into` replaced
/// by `*out = Network::new(rate)` overflows the stack and aborts the test.
#[test]
fn design_into_needs_no_network_sized_stack() {
    let rate = SampleRate(44_100.0);
    let mut network = Box::new(tutti_dsp::Network::new(rate));
    let handle = std::thread::Builder::new()
        .stack_size(96 * 1024)
        .spawn(move || {
            FreeverbDesign.design_into(&FreeverbControls::GENERAL_MIDI, rate, &mut network);
            network.stages.len()
        })
        .unwrap();
    assert_eq!(handle.join().unwrap(), 5);
}
