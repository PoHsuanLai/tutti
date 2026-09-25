//! The sample-accuracy contract (doc 013 §6, "Proof") for this crate's
//! latency-bearing nodes, as a graph runs them today: through `Legacy`, on
//! the audio-impulse path. An impulse at frame `F` must leave at exactly
//! `F + arrival + latency`, where `latency` is what the node *declares* — on
//! every path the harness has (direct, behind PDC, across a recompile,
//! across ragged blocks). See `tutti_graph::contract` for the paths and the
//! mutation each was seen to fail under.
//!
//! Each row is a defect class from doc 013:
//!
//! - **LimiterNode** — a lookahead: its latency is real, and the ring must
//!   delay by exactly the figure `route` reports.
//! - **ConvolverNode** — one FFT block of latency for the *whole* output
//!   (D3): at mix 0 the output is all dry, so a dry half that is not delayed
//!   leaves `latency` frames early.
//! - **DelayLineNode** — a musical delay is not latency (D1): it declares
//!   none, and the dry half of its blend leaves on the excitation's frame.
//!
//! A failing row here is a real D1–D3-class regression: fix the node, never
//! the row.
//!
//! Mutations (run):
//!
//! - In `Legacy::probe`, declare one frame more than `route` reports →
//!   every row fails every path.
//! - In `Legacy::process`, call the unit for a whole `MAX_BUFFER_SIZE` chunk
//!   even when fewer frames remain (its clock runs ahead of the block) → the
//!   limiter and convolver rows fail `blocks_1`, `blocks_63`, `blocks_65`
//!   and `blocks_random`, and pass `blocks_64` and `blocks_max`.
//! - In `ConvolverNode::blend_channel`, blend the undelayed input again (D3)
//!   → `convolver_dry` and `convolver_half` fail every path.
//! - Restore a `.delay(..)` of the echo time in `DelayLineNode::route` (D1)
//!   → `delay_line` fails every path.

use tutti_core::{ChannelLayout, Db, Samples};
use tutti_graph::contract::{Detect, Excite, Row};
use tutti_graph::contract_tests;
use tutti_nodes::{DelayLineNode, LimiterNode};

fn impulse(port: u16) -> Excite {
    Excite::Impulse {
        port,
        amplitude: 0.25,
    }
}

/// A 5 ms lookahead at 48 kHz: 240 frames, `_ceil`ed.
fn limiter_row() -> Row {
    Row::legacy(
        "LimiterNode (mono, 5 ms lookahead)",
        || LimiterNode::with_channels(ChannelLayout::MONO, Db(-3.0), Db(-0.3)),
        impulse(0),
        Detect::Threshold(0.0),
    )
    .expect_latency(Samples(240))
}

/// Linked gain across a stereo pair: the impulse on the right channel must
/// leave the right channel on the same frame.
fn stereo_limiter_row() -> Row {
    Row::legacy(
        "LimiterNode (stereo, right channel)",
        || LimiterNode::new(Db(-3.0), Db(-0.3)),
        impulse(1),
        Detect::Threshold(0.0),
    )
    .output(1)
    .expect_latency(Samples(240))
}

/// A 500 ms echo at mix 0.5: the dry half is the response, on the
/// excitation's own frame.
fn delay_line_row() -> Row {
    Row::legacy(
        "DelayLineNode (500 ms echo, mix 0.5)",
        || {
            let node = DelayLineNode::new(1.0_f32, 0.5_f32, 0.0_f32);
            node.set_mix(0.5_f32);
            node
        },
        impulse(0),
        Detect::Threshold(0.0),
    )
    .expect_latency(Samples(0))
}

contract_tests!(audio limiter => limiter_row());
contract_tests!(audio stereo_limiter => stereo_limiter_row());
contract_tests!(audio delay_line => delay_line_row());

#[cfg(feature = "convolution")]
mod convolution {
    use super::*;
    use tutti_nodes::ConvolverNode;

    /// A unit-impulse IR, so the wet path is a pure delay of the block; at
    /// `mix` the dry and wet shares leave on one frame. The FFT leaves noise
    /// far under the threshold; the response is `0.25` (dry + wet shares of
    /// the impulse).
    fn convolver_row(mix: f32) -> Row {
        Row::legacy(
            &format!("ConvolverNode (256-frame block, mix {mix})"),
            move || {
                let node = ConvolverNode::new(&[1.0], 256);
                node.set_mix(mix);
                node
            },
            impulse(0),
            Detect::Threshold(1e-3),
        )
        .expect_latency(Samples(256))
    }

    // Mix 0: all dry — D3's case, where the undelayed dry half left first.
    contract_tests!(audio convolver_dry => convolver_row(0.0));
    contract_tests!(audio convolver_half => convolver_row(0.5));
    contract_tests!(audio convolver_wet => convolver_row(1.0));
}
