//! A `Design::design` call allocates nothing (design 022 section 8.1, the
//! "pure" case).
//!
//! The crate carries no `assert_no_alloc` dev-dependency (see its Cargo.toml),
//! so this binary counts allocations with its own global allocator, per thread
//! as `chroma_alloc.rs` does. The check is generic over a [`Design`], so a later
//! design runs it on its own controls.

use tutti_dsp::{Design, Effect, FreeverbControls, FreeverbDesign};
use tutti_fixtures::{ThreadCounting, allocations};
use tutti_types::{Mix, SampleRate};

#[global_allocator]
static A: ThreadCounting = ThreadCounting;

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
#[test]
fn freeverb_retargets_and_renders_without_allocating() {
    use tutti_dsp::{Ceiling, FreeverbKernel, Network, Realization, Realized, Setup};
    use tutti_types::{MaxBlock, Samples};

    let rate = SampleRate(48_000.0);
    let design = FreeverbDesign;
    let mut realization = design.realization();
    realization.prepare(
        &design.bounds(&Ceiling::at(rate).unwrap()),
        &Setup {
            rate,
            max_block: MaxBlock::of(Samples(64)),
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
            realization.retarget(&network);
            Effect::process(&mut realization, &[&input], &mut [&mut left, &mut right]);
        });
        assert_eq!(n, 0, "the block allocated");
    }
    let _: Realized<FreeverbKernel> = realization;
}

/// `design_into` builds no value of the network's size (about 56 KB): it runs
/// on a 96 KiB stack, as an audio thread's callback may. An unoptimised build
/// copies a network about three times, so building one needs well over that.
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

/// The FDN's `design_into`, `retarget` and `process` allocate nothing, with a
/// different network every block (a decay, a size, a diffusion, a pre-delay, a
/// width, a wet gain and a freeze all move), after the one `prepare` that sizes
/// the realization from the bounds. The ramps the realization starts are the work of the
/// following blocks, which allocate nothing either.
#[test]
fn the_fdn_retargets_and_renders_without_allocating() {
    use tutti_dsp::{Ceiling, FdnControls, FdnDesign, Network, Realization, Setup};
    use tutti_types::{Db, Depth, Hz, MaxBlock, Samples, Seconds, StereoWidth};

    let rate = SampleRate(48_000.0);
    let design = FdnDesign::new(8, Seconds(8.0), true).unwrap();
    let mut realization = design.realization();
    realization.prepare(
        &design.bounds(&Ceiling::at(rate).unwrap()),
        &Setup {
            rate,
            max_block: MaxBlock::of(Samples(64)),
        },
    );
    let mut network = Box::new(Network::new(rate));
    let input = vec![0.25f32; 64];
    let (mut left, mut right) = (vec![0.0f32; 64], vec![0.0f32; 64]);
    for block in 0..200u32 {
        let k = (block % 7) as f32 / 6.0;
        let c = FdnControls {
            decay: [
                Seconds::from_f32(0.2 + 7.0 * k),
                Seconds::from_f32(1.0 + k),
                Seconds::from_f32(0.5 + 3.0 * k),
            ],
            size: Mix(k),
            diffusion: Mix(1.0 - k),
            pre_delay: Seconds::from_f32(0.05 * k),
            modulation: Depth(k),
            rate: Hz(0.1 + k),
            width: StereoWidth(2.0 * k),
            wet: Db(-12.0 * k),
            freeze: block % 11 == 0,
        };
        let n = allocations(|| {
            design.design_into(&c, rate, &mut network);
            realization.retarget(&network);
            Effect::process(&mut realization, &[&input], &mut [&mut left, &mut right]);
        });
        assert_eq!(n, 0, "the block allocated");
    }
}

/// The FDN's `design_into` runs on a 96 KiB stack, like Freeverb's.
#[test]
fn the_fdns_design_into_needs_no_network_sized_stack() {
    use tutti_dsp::{FdnControls, FdnDesign};
    let rate = SampleRate(44_100.0);
    let mut network = Box::new(tutti_dsp::Network::new(rate));
    let handle = std::thread::Builder::new()
        .stack_size(96 * 1024)
        .spawn(move || {
            FdnDesign::default().design_into(&FdnControls::DEFAULT, rate, &mut network);
            network.stages.len()
        })
        .unwrap();
    assert_eq!(handle.join().unwrap(), 1);
}
