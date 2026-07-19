//! Regression gate for RT-safety: built-in AudioUnits must not
//! allocate on the process path. Originally covered the spatial
//! panner `scratch_output.resize` hazard; now also exercises the
//! convolution reverb FFT path.

use assert_no_alloc::AllocDisabler;
#[cfg(any(feature = "spatial", feature = "convolution"))]
use tutti_core::{AudioUnit, BufferVec};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

#[cfg(feature = "spatial")]
use tutti_units::{BinauralPannerNode, SpatialPannerNode};

#[cfg(feature = "spatial")]
#[test]
fn spatial_panner_stereo_process_is_allocation_free() {
    let mut node = SpatialPannerNode::stereo().expect("stereo preset");
    node.set_sample_rate(tutti_core::SampleRate(48_000.0));
    node.set_position(30.0, 0.0);

    let input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(2);

    // Warm up.
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

#[cfg(feature = "spatial")]
#[test]
fn binaural_panner_process_is_allocation_free() {
    let mut node = BinauralPannerNode::new(48_000.0);
    node.set_position(30.0, 0.0);
    node.set_sample_rate(tutti_core::SampleRate(48_000.0));

    let input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(2);

    // Warm up: the smoother needs a few samples to prime.
    for _ in 0..8 {
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

#[cfg(feature = "spatial")]
#[test]
fn binaural_panner_stereo_width_process_is_allocation_free() {
    // Exercises the two-virtual-source branch of process_stereo, which
    // used to call set_position() three times per sample.
    let mut node = BinauralPannerNode::new(48_000.0);
    node.set_position(30.0, 0.0);
    node.set_width(1.0);
    node.set_sample_rate(tutti_core::SampleRate(48_000.0));

    let input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(2);

    for _ in 0..8 {
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
