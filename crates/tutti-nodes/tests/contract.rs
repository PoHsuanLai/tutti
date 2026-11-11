//! The sample-accuracy contract for this crate's
//! latency-bearing nodes, as a graph runs them, on the audio-impulse path.
//! An impulse at frame `F` must leave at exactly
//! `F + arrival + latency`, where `latency` is what the node *declares* — on
//! every path the harness has (direct, behind PDC, across a recompile,
//! across ragged blocks). See `tutti_graph::contract` for the paths and the
//! mutation each was seen to fail under.
//!
//! Each row is a class of latency defect:
//!
//! - **LimiterNode** — a lookahead: its latency is real, and the
//!   ring must delay by exactly the figure its shape declares.
//! - **ConvolverNode** — one FFT block of latency, declared for the whole
//!   output (its dry half is the graph's mix, aligned by that figure:
//!   `node_mix.rs`, `reported_latency.rs`).
//! - **CrossoverNode** and **MultibandCompressorNode** — IIR splits with no
//!   latency: an impulse leaves its first band on its own frame, on the right
//!   channel.
//! - **DelayLineNode** — a musical delay is not latency: it declares
//!   none (its dry half is the graph's mix: `reported_latency.rs`).
//!
//! A failing row here is a real latency regression: fix the node, never
//! the row.
//!
//! Mutations (run):
//!
//! - In `LimiterNode::shape`, declare one frame more than the ring's
//!   `lookahead_samples` → both limiter rows fail every path.
//! - Declare one frame more or less than the FFT block in
//!   `ConvolverNode::shape` → `convolver` fails every path.
//! - Declare the echo time as latency in `DelayLineNode::shape` →
//!   `delay_line` fails every path.
//! - Declare one frame of latency in `CrossoverNode::shape` or
//!   `MultibandCompressorNode::shape` → their rows fail every path.
//!
//! Every row is a graph node (`Row::new`).

use tutti_graph::contract::{Detect, Excite, Row};
use tutti_graph::contract_tests;
use tutti_nodes::{CrossoverNode, DelayLineNode, LimiterNode, MultibandCompressorNode};
use tutti_types::{ChannelLayout, Db, Feedback, Hz, Samples, Seconds};

fn impulse(port: u16) -> Excite {
    Excite::Impulse {
        port,
        amplitude: 0.25,
    }
}

/// A 5 ms lookahead at 48 kHz: 240 frames, `_ceil`ed.
fn limiter_row() -> Row {
    Row::new(
        "LimiterNode (mono, 5 ms lookahead)",
        || {
            Box::new(LimiterNode::with_channels(
                ChannelLayout::MONO,
                Db(-3.0),
                Db(-0.3),
            ))
        },
        impulse(0),
        Detect::Threshold(0.0),
    )
    .with_expected_latency(Samples(240))
}

/// Linked gain across a stereo pair: the impulse on the right channel must
/// leave the right channel on the same frame.
fn stereo_limiter_row() -> Row {
    Row::new(
        "LimiterNode (stereo, right channel)",
        || Box::new(LimiterNode::new(Db(-3.0), Db(-0.3))),
        impulse(1),
        Detect::Threshold(0.0),
    )
    .output(1)
    .with_expected_latency(Samples(240))
}

/// A delay of zero seconds: the wet path is the response, on the excitation's
/// own frame. (A 500 ms echo's response is at 24 000, not at its declared
/// latency of 0, and the dry half it once blended in is the graph's mix, pinned
/// in `node_mix.rs`.) A `tutti_graph::Node`, so the row runs it as itself.
fn delay_line_row() -> Row {
    Row::new(
        "DelayLineNode (zero delay)",
        || {
            Box::new(DelayLineNode::new(
                Seconds::new(1.0_f64),
                Seconds::new(0.0_f64),
                Feedback::new(0.0_f32),
            ))
        },
        impulse(0),
        Detect::Threshold(0.0),
    )
    .with_expected_latency(Samples(0))
}

/// The low band of a stereo crossover: an impulse on the right channel leaves
/// the right channel's low band (output 1), on its own frame. The IIR's first
/// sample is nonzero, so the detector fires on it.
fn crossover_row() -> Row {
    Row::new(
        "CrossoverNode (stereo, 2 bands, right low band)",
        || Box::new(CrossoverNode::new(ChannelLayout::STEREO, &[Hz(1_000.0)]).unwrap()),
        impulse(1),
        Detect::Threshold(0.0),
    )
    .output(1)
    .with_expected_latency(Samples(0))
}

/// The summed output of a stereo multiband compressor, impulse on the right.
fn multiband_row() -> Row {
    Row::new(
        "MultibandCompressorNode (stereo, 3 bands, right channel)",
        || Box::new(MultibandCompressorNode::new(ChannelLayout::STEREO, 3).unwrap()),
        impulse(1),
        Detect::Threshold(0.0),
    )
    .output(1)
    .with_expected_latency(Samples(0))
}

contract_tests!(audio limiter => limiter_row());
contract_tests!(audio crossover => crossover_row());
contract_tests!(audio multiband => multiband_row());
contract_tests!(audio stereo_limiter => stereo_limiter_row());
contract_tests!(audio delay_line => delay_line_row());

#[cfg(feature = "convolution")]
mod convolution {
    use super::*;
    use tutti_nodes::ConvolverNode;

    /// A unit-impulse IR, so the wet path is a pure delay of the block: the
    /// response is the impulse, `256` frames late. The FFT leaves noise far under
    /// the threshold. A `tutti_graph::Node`, so the row runs it as itself. (The
    /// dry half the node once blended in is the graph's mix, pinned in
    /// `node_mix.rs`.)
    fn convolver_row() -> Row {
        Row::new(
            "ConvolverNode (256-frame block)",
            move || Box::new(ConvolverNode::new(&[1.0], 256)),
            impulse(0),
            Detect::Threshold(1e-3),
        )
        .with_expected_latency(Samples(256))
    }

    contract_tests!(audio convolver => convolver_row());
}
