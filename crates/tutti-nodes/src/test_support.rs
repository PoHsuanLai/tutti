//! Rendering helpers shared by the width-generic nodes' unit tests.

use tutti_graph::Node;
use tutti_graph::contract::drive;
use tutti_types::SampleRate;

pub(crate) use tutti_fixtures::noise;

/// The rate the node helpers below prepare and drive nodes at.
pub(crate) const RATE: SampleRate = SampleRate::SR_48K;

/// A node prepared at [`RATE`] for blocks of up to 1024 frames.
pub(crate) fn prepared<N: Node>(node: N) -> N {
    tutti_graph::contract::prepared(node, RATE, 1024)
}

/// One `process` call of a node over `inputs` (one slice per input
/// port), its params at their bases.
pub(crate) fn drive_block(node: &mut dyn Node, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
    drive(node, RATE, inputs, &[])
}

/// Returns a [`prepared`] node of `make`, driven over `before` in one block, then
/// `change`d, then driven over `after` in blocks of `block` frames, with its render of
/// `after`.
pub(crate) fn render_change<N: Node>(
    make: impl Fn() -> N,
    change: impl Fn(&N),
    before: &[&[f32]],
    after: &[&[f32]],
    block: usize,
) -> (N, Vec<Vec<f32>>) {
    let mut node = prepared(make());
    drive_block(&mut node, before);
    change(&node);
    let mut out: Vec<Vec<f32>> = Vec::new();
    let mut at = 0;
    while at < after[0].len() {
        let end = (at + block).min(after[0].len());
        let chunk: Vec<&[f32]> = after.iter().map(|s| &s[at..end]).collect();
        let o = drive_block(&mut node, &chunk);
        out.resize(o.len(), Vec::new());
        for (c, v) in o.into_iter().enumerate() {
            out[c].extend(v);
        }
        at = end;
    }
    (node, out)
}

/// Asserts that a cell moved between blocks glides on the stream clock, so a write
/// renders the same whatever the host's blocks: the render of `after` is the same, bit
/// for bit, at blocks of 1, 17, 64 and 256 frames, and it is not the render with the cell
/// left alone. Returns the node rendered at blocks of 64.
#[track_caller]
pub(crate) fn assert_cell_glides<N: Node>(
    make: impl Fn() -> N,
    change: impl Fn(&N),
    before: &[&[f32]],
    after: &[&[f32]],
    what: &str,
) -> N {
    let (node, at64) = render_change(&make, &change, before, after, 64);
    for block in [1, 17, 256] {
        let (_, out) = render_change(&make, &change, before, after, block);
        assert!(
            out.iter()
                .flatten()
                .zip(at64.iter().flatten())
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "{what}: the glide at blocks of {block} is not the glide at blocks of 64"
        );
    }
    let (_, held) = render_change(&make, |_: &N| {}, before, after, 64);
    assert!(
        held != at64,
        "{what}: the change is inaudible, so this run cannot see a glide"
    );
    node
}

/// The rate the per-frame unit tests run their nodes at.
pub(crate) const RATE_44K: SampleRate = SampleRate::SR_44K1;

/// A node prepared at `rate` for blocks of up to 1024 frames.
pub(crate) fn prepared_at<N: Node>(node: N, rate: SampleRate) -> N {
    tutti_graph::contract::prepared(node, rate, 1024)
}

/// One frame of a node — a block of one: `input` holds one sample per audio
/// input, and `out` receives one per output.
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

/// `len` samples of a unit sine at `hz`, starting at phase 0.
pub(crate) fn sine(hz: f64, rate: f64, len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| libm::sin(2.0 * core::f64::consts::PI * hz * i as f64 / rate) as f32)
        .collect()
}

/// The complex amplitude of the `hz` component of `x`, as `(re, im)` of
/// `2/N * sum(x[n] * e^(-j w n))` (a unit sine at phase 0 reads `(0, -1)`).
///
/// Exact (no leakage) when `x` spans a whole number of periods of `hz`.
pub(crate) fn lock_in(x: &[f32], hz: f64, rate: f64) -> (f64, f64) {
    let w = 2.0 * core::f64::consts::PI * hz / rate;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &v) in x.iter().enumerate() {
        let a = w * n as f64;
        re += f64::from(v) * libm::cos(a);
        im -= f64::from(v) * libm::sin(a);
    }
    let k = 2.0 / x.len() as f64;
    (re * k, im * k)
}

/// The amplitude of [`lock_in`].
pub(crate) fn amplitude_at(x: &[f32], hz: f64, rate: f64) -> f64 {
    let (re, im) = lock_in(x, hz, rate);
    re.hypot(im)
}

/// A graph node over whole-signal `inputs` (one `Vec` per input), rendered in
/// `chunk`-frame blocks through [`drive_block`]; one `Vec` per output. The
/// node must be [`prepared`] for blocks of `chunk`.
pub(crate) fn render_chunked(
    node: &mut dyn Node,
    inputs: &[Vec<f32>],
    chunk: usize,
) -> Vec<Vec<f32>> {
    let len = inputs[0].len();
    let mut out: Vec<Vec<f32>> = Vec::new();
    for start in (0..len).step_by(chunk) {
        let end = (start + chunk).min(len);
        let block: Vec<&[f32]> = inputs.iter().map(|c| &c[start..end]).collect();
        let result = drive_block(node, &block);
        out.resize(result.len(), Vec::with_capacity(len));
        for (o, r) in out.iter_mut().zip(result) {
            o.extend(r);
        }
    }
    out
}

/// The mean of `x`.
pub(crate) fn mean(x: &[f32]) -> f64 {
    x.iter().map(|&v| f64::from(v)).sum::<f64>() / x.len() as f64
}
