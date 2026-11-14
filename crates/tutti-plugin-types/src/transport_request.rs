//! What a plugin asks the host's transport to do.
//!
//! A plugin may ask; the host decides. [`TransportRequest`] is the value a
//! format's request callback is decoded into, and the application hears it
//! as [`Request::Transport`](crate::Request::Transport) from
//! [`Control::take_requests`](crate::Control::take_requests) and then as a
//! notice.

use tutti_types::{Beat, BeatDuration};

/// One transport request from a plugin (CLAP `transport-control`).
///
/// Exhaustive: the requests the extension defines.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TransportRequest {
    /// Jump back to the start point and start the transport.
    Start,
    /// Stop the transport and jump to the start point.
    Stop,
    /// Start from the current position when not playing.
    Continue,
    /// Stop at the current position when playing.
    Pause,
    /// Play when stopped, pause when playing.
    TogglePlay,
    /// Move to a position without starting.
    Jump(Beat),
    /// Set the loop region.
    LoopRegion {
        /// Where the region starts.
        start: Beat,
        /// How long it is.
        duration: BeatDuration,
    },
    /// Switch looping.
    ToggleLoop,
    /// Switch looping on or off.
    EnableLoop(bool),
    /// Switch recording on or off.
    Record(bool),
    /// Switch recording.
    ToggleRecord,
}
