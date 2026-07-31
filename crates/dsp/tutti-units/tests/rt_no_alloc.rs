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
use tutti_units::SpatialPannerNode;

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

/// Minimal format-valid HRIR sphere (a tetrahedron with delta impulse
/// responses) — enough for the crate to parse and convolve without a dataset.
#[cfg(feature = "hrtf")]
fn synthetic_hrir_sphere(sample_rate: u32, ir_len: usize) -> Vec<u8> {
    let verts: [[f32; 3]; 4] = [
        [1.0, 1.0, 1.0],
        [-1.0, -1.0, 1.0],
        [-1.0, 1.0, -1.0],
        [1.0, -1.0, -1.0],
    ];
    let faces: [[u32; 3]; 4] = [[0, 1, 2], [0, 3, 1], [0, 2, 3], [1, 3, 2]];
    let mut b = Vec::new();
    b.extend_from_slice(b"HRIR");
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.extend_from_slice(&(ir_len as u32).to_le_bytes());
    b.extend_from_slice(&(verts.len() as u32).to_le_bytes());
    b.extend_from_slice(&((faces.len() * 3) as u32).to_le_bytes());
    for f in faces {
        for idx in f {
            b.extend_from_slice(&idx.to_le_bytes());
        }
    }
    for v in verts {
        for comp in v {
            b.extend_from_slice(&comp.to_le_bytes());
        }
        for _ in 0..2 {
            for i in 0..ir_len {
                b.extend_from_slice(&(if i == 0 { 1.0f32 } else { 0.0 }).to_le_bytes());
            }
        }
    }
    b
}

#[cfg(feature = "hrtf")]
#[test]
fn hrtf_binaural_process_is_allocation_free() {
    use tutti_units::HrtfBinauralNode;

    let bytes = synthetic_hrir_sphere(48_000, 64);
    let mut node = HrtfBinauralNode::new(&bytes, 48_000.0).expect("synthetic sphere parses");
    node.set_position(30.0, 0.0);
    node.set_sample_rate(tutti_core::SampleRate(48_000.0));

    let input_vec = BufferVec::new(2);
    let mut output_vec = BufferVec::new(2);

    // Warm up past a full HRTF frame so the FFT-convolution path fires and any
    // lazy buffer growth has already happened before the assertion window.
    for _ in 0..64 {
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
    // Imported here rather than at file scope: the module-level `AudioUnit` /
    // `BufferVec` imports are gated on the spatial/convolution features, and
    // this node needs neither.
    use tutti_core::{AudioUnit, BufferVec, ChannelLayout};
    use tutti_units::DownmixUnit;

    let mut node = DownmixUnit::new(ChannelLayout::Multi(6), ChannelLayout::Stereo);

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
