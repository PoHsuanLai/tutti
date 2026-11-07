//! Applying a `Fold` allocates nothing: the frame, interleaved and planar
//! entry points, over every shape of row (copy, zero, sum, summed group, mono
//! mix), 1 000 blocks under `assert_no_alloc`. Building the fold allocates and
//! happens outside the gate.

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use tutti_types::{ChannelLayout, ChannelTopology, Fold, Interleaved, InterleavedMut, Speaker};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn smpte(n: u16) -> ChannelTopology {
    ChannelTopology::smpte(ChannelLayout::from(n)).unwrap()
}

/// 12 -> 2 is the summed group, 6 -> 1 the mono mix, the permutation and
/// 2 -> 6 the copy and zero rows, 5.1 -> 2 plain sums. 200 frames is past the
/// planar group's 64-frame chunk.
///
/// Mutation (run): the planar group scratch `[0.0f32; CHUNK]` made
/// `vec![0.0f32; CHUNK]` -> the 12 -> 2 gate fails; the same in
/// `planar_to_interleaved` -> every gate fails; `Vec` collected in
/// `Fold::frame` -> every gate fails.
#[test]
fn applying_a_fold_is_allocation_free() {
    use Speaker::*;
    let dts = ChannelTopology::new([
        FrontLeft,
        FrontCenter,
        FrontRight,
        SideLeft,
        SideRight,
        LowFrequency,
    ]);
    let pairs = [
        (smpte(12), smpte(2)),
        (smpte(6), smpte(1)),
        (smpte(6), dts),
        (smpte(2), smpte(6)),
        (smpte(6), smpte(2)),
    ];
    const FRAMES: usize = 200;
    for (src, dst) in &pairs {
        let fold = Fold::between(src, dst).unwrap();
        let (sw, dw) = (src.positions().len(), dst.positions().len());
        let data: Vec<f32> = (0..sw * FRAMES).map(|i| (i as f32 * 0.37).sin()).collect();
        let planes: Vec<Vec<f32>> = (0..sw)
            .map(|c| data.chunks_exact(sw).map(|f| f[c]).collect())
            .collect();
        let mut block = vec![0.0f32; dw * FRAMES];
        let mut out = vec![0.0f32; FRAMES];
        let mut frame = vec![0.0f32; dw];
        // Warm up: first-touch faults are not the fold's.
        fold.frame(&data[..sw], &mut frame);
        assert_no_alloc(|| {
            for _ in 0..1_000 {
                fold.frame(&data[..sw], &mut frame);
                fold.interleaved(
                    Interleaved::new(&data, src.layout()),
                    InterleavedMut::new(&mut block, dst.layout()),
                );
                for c in 0..dw {
                    fold.planar_channel(|k| &planes[k][..], c, &mut out);
                }
                fold.planar_to_interleaved(
                    |k| &planes[k][..],
                    InterleavedMut::new(&mut block, dst.layout()),
                );
            }
        });
        assert!(
            block.iter().chain(&out).any(|s| s.abs() > 1e-6),
            "the fold rendered silence; the gate would walk no DSP"
        );
    }
}
