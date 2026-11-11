//! The error types of the process edge.
//!
//! [`BridgeError`] (in `tutti-plugin-types`) is the host-side error most calls
//! return. [`PluginRenderFault`] describes how a forked plugin instance
//! failed while rendering.

use thiserror::Error;
use tutti_plugin_types::BridgeError;
use tutti_types::SampleRate;

/// How a forked plugin instance failed while rendering.
///
/// This is the cause behind a `tutti_graph::ForkFault` for a plugin node. After
/// a crash or a timeout the fork renders silence; after a latency change it
/// renders misaligned.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum PluginRenderFault {
    /// The fork's plugin-server died.
    #[error("the forked instance's plugin-server died: {cause}")]
    Crashed {
        /// What the bridge latched when it noticed.
        cause: String,
    },
    /// The fork's plugin refused the sample rate it renders at, so it renders
    /// silence: the whole export is wrong from the first block.
    #[error("the forked instance refused its sample rate ({rate}): {reason}")]
    RateRefused {
        /// The rate it refused.
        rate: SampleRate,
        /// The plugin's reason.
        reason: String,
    },
    /// The fork's server did not publish a block within the budget
    /// (`BridgeConfig::timeout_ms`). The fork stopped waiting: every later
    /// block is silence, at once.
    #[error(
        "the forked instance did not answer a block within {budget:?}; \
         it renders silence from then on"
    )]
    TimedOut {
        /// The per-block budget it missed.
        budget: std::time::Duration,
    },
    /// The forked instance's latency is not the one its graph was compiled
    /// against: the plugin changed it after the fork's plan was made, so the
    /// render's delay compensation (and an export's latency trim) is off by
    /// the difference. The fork waits for a latency its render-mode and rate
    /// changes cause before its plan is made, so this is a change the plugin
    /// made later, on its own.
    #[error(
        "the forked instance's latency moved from {planned:?} to {now:?} after \
         its graph was compiled; the render is misaligned by the difference"
    )]
    LatencyChanged {
        /// What the fork's graph compensates for.
        planned: tutti_types::Latency,
        /// What the instance declares now.
        now: tutti_types::Latency,
    },
}

/// Builds an `UnexpectedMessage` error by Debug-formatting the received
/// `BridgeMessage`.
pub(crate) fn unexpected_message(
    expected: &'static str,
    got: &crate::protocol::BridgeMessage,
) -> BridgeError {
    BridgeError::UnexpectedMessage {
        expected,
        got: format!("{got:?}"),
    }
}

/// A bincode failure as the [`BridgeError`] it is on the wire, keeping the
/// codec's message.
pub(crate) fn codec(e: bincode::Error) -> BridgeError {
    BridgeError::Serialization(e.to_string())
}
