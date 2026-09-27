//! Rendering helpers shared by the width-generic nodes' unit tests.

use tutti_core::SampleRate;
use tutti_graph::contract::drive;
use tutti_graph::Node;

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

/// The rate the node helpers below prepare and drive nodes at.
pub(crate) const RATE: SampleRate = SampleRate(48_000.0);

/// A node prepared at [`RATE`] for blocks of up to 1024 frames.
pub(crate) fn prepared<N: Node>(node: N) -> N {
    tutti_graph::contract::prepared(node, RATE, 1024)
}

/// One `process` call of a node over `inputs` (one slice per input
/// port), its params at their bases.
pub(crate) fn drive_block(node: &mut dyn Node, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
    drive(node, RATE, inputs, &[])
}

/// A node over `inputs`, one frame per call: a change lands whole on
/// the first frame, as `tick` landed it.
pub(crate) fn drive_frames(node: &mut dyn Node, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
    let n = inputs[0].len();
    let mut out: Vec<Vec<f32>> = Vec::new();
    for i in 0..n {
        let frame: Vec<&[f32]> = inputs.iter().map(|s| &s[i..=i]).collect();
        let o = drive(node, RATE, &frame, &[]);
        out.resize(o.len(), Vec::with_capacity(n));
        for (c, v) in o.into_iter().enumerate() {
            out[c].push(v[0]);
        }
    }
    out
}

/// [`change_between_blocks`] for a graph node: `make` builds it unprepared,
/// and each copy is [`prepared`]. `jumped` renders `block2` one frame per
/// call ([`drive_frames`]).
pub(crate) fn change_between_node_blocks<N: Node>(
    make: impl Fn() -> N,
    change: impl Fn(&N),
    block1: &[&[f32]],
    block2: &[&[f32]],
) -> ChangeRun<N> {
    let (mut ramped, mut jumped, mut held) = (prepared(make()), prepared(make()), prepared(make()));
    for n in [&mut ramped, &mut jumped, &mut held] {
        drive_block(n, block1);
    }
    change(&ramped);
    change(&jumped);
    let r = drive_block(&mut ramped, block2);
    let j = drive_frames(&mut jumped, block2);
    let h = drive_block(&mut held, block2);
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

/// The rate the per-frame unit tests ran their nodes at before the port.
pub(crate) const RATE_44K: SampleRate = SampleRate(44_100.0);

/// A node prepared at `rate` for blocks of up to 1024 frames: what
/// `set_sample_rate(rate)` did for an `AudioUnit`.
pub(crate) fn prepared_at<N: Node>(node: N, rate: SampleRate) -> N {
    tutti_graph::contract::prepared(node, rate, 1024)
}

/// One frame of a node — a block of one, what `tick` was: `input`
/// holds one sample per audio input, and `out` receives one per output.
pub(crate) fn tick(node: &mut dyn Node, input: &[f32], out: &mut [f32]) {
    tick_fed(node, input, &[], out);
}

/// [`tick`] with declared param `k` fed `params[k]` for the frame (`None`
/// reads its base).
pub(crate) fn tick_fed(
    node: &mut dyn Node,
    input: &[f32],
    params: &[Option<f32>],
    out: &mut [f32],
) {
    let ins: Vec<&[f32]> = input.iter().map(std::slice::from_ref).collect();
    let ps: Vec<Option<&[f32]>> = params
        .iter()
        .map(|p| p.as_ref().map(std::slice::from_ref))
        .collect();
    for (o, c) in out.iter_mut().zip(drive(node, RATE, &ins, &ps)) {
        *o = c[0];
    }
}
