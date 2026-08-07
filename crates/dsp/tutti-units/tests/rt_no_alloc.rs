//! Regression gate for RT-safety: built-in AudioUnits must not allocate on the
//! process path. The panner tests live in `tutti-spatial`'s own copy of this
//! gate, alongside the nodes they cover.

use assert_no_alloc::AllocDisabler;
#[cfg(feature = "convolution")]
use tutti_core::{AudioUnit, BufferVec};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

#[cfg(feature = "convolution")]
#[test]
fn convolver_process_is_allocation_free() {
    use tutti_units::{generate_test_ir, ConvolverNode};

    let ir = generate_test_ir(2048, 0.3, 48_000.0);
    let mut node = ConvolverNode::new(&ir, 512);
    node.set_sample_rate(tutti_core::SampleRate(48_000.0));

    let input_vec = BufferVec::new(1);
    let mut output_vec = BufferVec::new(1);

    // Warm up past the first FFT block so `process` takes the hot path.
    for _ in 0..16 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

#[cfg(feature = "convolution")]
#[test]
fn stereo_convolver_process_is_allocation_free() {
    use tutti_units::{generate_test_ir, StereoConvolverNode};

    let ir_l = generate_test_ir(2048, 0.3, 48_000.0);
    let ir_r = generate_test_ir(2048, 0.4, 48_000.0);
    let mut node = StereoConvolverNode::stereo(&ir_l, &ir_r, 512);
    node.set_sample_rate(tutti_core::SampleRate(48_000.0));

    let input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(2);

    for _ in 0..16 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

/// `DownmixUnit::process` gathers an interleaved frame per sample and folds it.
/// Both scratch buffers are sized at construction and taken with `mem::take`
/// precisely so that path never allocates — this is the test that makes those
/// two fields load-bearing rather than incidental.
///
/// 6→2 is the shape that matters: it is the only one where the fold does real
/// work (an equal-width or upmix fold is a copy), and it is what a 5.1 bus
/// feeding a stereo master looks like.
#[test]
fn downmix_process_is_allocation_free() {
    // Imported here rather than at file scope: the module-level imports are
    // gated on `convolution`, and this node needs it not.
    use tutti_core::{AudioUnit, BufferVec, ChannelLayout};
    use tutti_units::DownmixUnit;

    let mut node = DownmixUnit::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);

    let input_vec = BufferVec::new(6);
    let mut output_vec = BufferVec::new(2);

    // Warm up: first-touch faults must not be counted against the RT path.
    {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}
