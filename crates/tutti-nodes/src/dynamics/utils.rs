//! Sidechain detection and gain application over the graph's planar buffers,
//! shared by the dynamics nodes. The gain curves are `tutti-dsp`'s.

use tutti_graph::{Inputs, Outputs};

use crate::buses::Keyed;

/// Max-abs sidechain detector level for each frame of a block, written to
/// `levels`, one channel at a time.
///
/// The key's channels are the ports `ports` declares as the `Sidechain` bus. A
/// graph node declares them all, and the graph feeds an unconnected one
/// silence, so every key channel is there to read.
/// `levels.len()` is the block's frame count. Each frame folds its channels
/// in ascending order from 0, as a per-frame loop does, so the lane holds
/// the same values (`max` ignores a NaN either way).
#[inline]
pub(crate) fn sidechain_levels(input: &Inputs<'_>, ports: &Keyed, levels: &mut [f32]) {
    let n = levels.len();
    levels.fill(0.0);
    for c in 0..usize::from(ports.width().count()) {
        for (level, &x) in levels.iter_mut().zip(&input.get(ports.key(c))[..n]) {
            *level = level.max(x.abs());
        }
    }
}

/// Apply a per-frame gain lane to the audio, channel-outer over planar slices:
/// `output[c][i] = input[ports.audio(c)][i] * gains[i]`.
///
/// The dynamics processors compute one linked gain per frame (the detector is
/// sample-outer by nature — a recursive envelope) and apply it to every
/// channel; doing the apply as a slice multiply per channel is the memoryless
/// half, and the half that vectorizes.
#[inline]
pub(crate) fn apply_gain_lane(
    gains: &[f32],
    ports: &Keyed,
    input: &Inputs<'_>,
    output: &mut Outputs<'_, '_>,
) {
    let n = gains.len();
    for c in 0..usize::from(ports.width().count()) {
        let x = &input.get(ports.audio(c))[..n];
        let y = &mut output.get(c)[..n];
        for ((y, &x), &g) in y.iter_mut().zip(x).zip(gains) {
            *y = x * g;
        }
    }
}
