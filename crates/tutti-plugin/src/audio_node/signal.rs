//! fundsp signal-flow routing for the plugin node.

use tutti_core::{Signal, SignalFrame};

/// Propagate reported latency through fundsp's signal-flow analysis.
/// - Generator (no inputs): all outputs carry pure internal latency.
/// - Effect: each output delays the corresponding input, reusing the
///   last input for extra outputs (mirrors fundsp's limiter pattern).
pub(crate) fn route_with_latency(
    inputs: usize,
    outputs: usize,
    latency: f64,
    input: &SignalFrame,
) -> SignalFrame {
    let mut output = SignalFrame::new(outputs);
    if inputs == 0 {
        for i in 0..outputs {
            output.set(i, Signal::Latency(latency));
        }
    } else {
        for i in 0..outputs {
            let src = input.at(i.min(inputs - 1));
            output.set(i, src.delay(latency));
        }
    }
    output
}
