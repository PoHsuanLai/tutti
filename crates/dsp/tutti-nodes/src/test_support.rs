//! Rendering helpers shared by the width-generic nodes' unit tests.

use tutti_core::{AudioUnit, BufferVec};

/// Deterministic broadband input in `[-1, 1)`.
pub(crate) fn noise(seed: u32, len: usize) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        })
        .collect()
}

/// One `process` call over `inputs[c][..n]` (one slice per input port),
/// returning one `Vec` per output.
pub(crate) fn process_block(node: &mut dyn AudioUnit, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
    assert_eq!(inputs.len(), node.inputs(), "one input slice per port");
    let n = inputs[0].len();
    assert!(
        n <= tutti_core::MAX_BUFFER_SIZE,
        "one block is at most 64 frames"
    );
    let mut ib = BufferVec::new(node.inputs());
    let mut ob = BufferVec::new(node.outputs());
    for (c, sig) in inputs.iter().enumerate() {
        for (i, &x) in sig.iter().enumerate() {
            ib.set_f32(c, i, x);
        }
    }
    node.process(n, &ib.buffer_ref(), &mut ob.buffer_mut());
    (0..node.outputs())
        .map(|c| (0..n).map(|i| ob.at_f32(c, i)).collect())
        .collect()
}

/// `tick` over `inputs[c][..n]`, frame by frame.
pub(crate) fn tick_block(node: &mut dyn AudioUnit, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
    let n = inputs[0].len();
    let mut out = vec![vec![0.0f32; n]; node.outputs()];
    let mut fi = vec![0.0f32; node.inputs()];
    let mut fo = vec![0.0f32; node.outputs()];
    for i in 0..n {
        for (c, sig) in inputs.iter().enumerate() {
            fi[c] = sig[i];
        }
        node.tick(&fi, &mut fo);
        for (c, o) in out.iter_mut().enumerate() {
            o[i] = fo[c];
        }
    }
    out
}

/// The "control changed between two blocks" experiment.
///
/// Three nodes from `make` render `block1` identically. Then `change` is
/// applied to two of them: `ramped` renders `block2` through `process` (the
/// per-block read, which ramps), `jumped` through `tick` (a block of one per
/// sample, so the change lands whole on the first sample — the old per-sample
/// behaviour). `held` never sees the change.
pub(crate) struct ChangeRun<N> {
    pub ramped: N,
    pub jumped: N,
    pub r: Vec<Vec<f32>>,
    pub j: Vec<Vec<f32>>,
    pub h: Vec<Vec<f32>>,
}

pub(crate) fn change_between_blocks<N: AudioUnit>(
    make: impl Fn() -> N,
    change: impl Fn(&N),
    block1: &[&[f32]],
    block2: &[&[f32]],
) -> ChangeRun<N> {
    let (mut ramped, mut jumped, mut held) = (make(), make(), make());
    // The history is rendered in full blocks, so a node can be primed with
    // more than one block's worth (a delay line reaching back further).
    for start in (0..block1[0].len()).step_by(tutti_core::MAX_BUFFER_SIZE) {
        let end = (start + tutti_core::MAX_BUFFER_SIZE).min(block1[0].len());
        let chunk: Vec<&[f32]> = block1.iter().map(|s| &s[start..end]).collect();
        for n in [&mut ramped, &mut jumped, &mut held] {
            process_block(n, &chunk);
        }
    }
    change(&ramped);
    change(&jumped);
    let r = process_block(&mut ramped, block2);
    let j = tick_block(&mut jumped, block2);
    let h = process_block(&mut held, block2);
    ChangeRun {
        ramped,
        jumped,
        r,
        j,
        h,
    }
}

impl<N> ChangeRun<N> {
    /// The change is ramped *into* the block: on the block's first sample the
    /// ramped render is still near the unchanged one — at most half as far from
    /// it as the render that took the change whole — on every channel, and the
    /// change is audible there at all (or the comparison proves nothing).
    #[track_caller]
    pub fn assert_ramps_in(&self, what: &str) {
        for c in 0..self.r.len() {
            let (r, j, h) = (self.r[c][0], self.j[c][0], self.h[c][0]);
            assert!(
                (j - h).abs() > 1e-4,
                "{what}: channel {c}: the change is inaudible on the first sample \
                 ({j} vs {h}), so this run cannot tell a ramp from a jump"
            );
            assert!(
                (r - h).abs() < 0.5 * (j - h).abs(),
                "{what}: channel {c}: the first sample after the change is {r}; \
                 unchanged it is {h}, jumped {j} — the change was not ramped in"
            );
        }
    }
}
