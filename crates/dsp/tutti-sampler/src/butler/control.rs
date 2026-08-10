//! The single definition of the butler stream-control operations, expressed
//! over the raw handles (command channel + channel-plan map).
//!
//! Both the timeline path ([`DiskStreamer`](crate::DiskStreamer)) and the browser-preview
//! path ([`Auditioner`](crate::Auditioner)) drive butler streaming through these
//! functions, so "start a stream on a channel", "stop", "set varispeed", and
//! "take the streaming consumer" each have exactly one implementation.

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use smol::channel::Sender;
use tutti_core::PlaybackRate;

use super::{ButlerCommand, ChannelPlan, RtState};
use crate::voice::disk_voice::DiskSource;
use crate::voice::types::Direction;

/// The butler thread is gone, so a stream-control command was discarded.
///
/// There is one failure mode and it is **permanent**: `send_blocking` on an
/// unbounded channel only fails once the receiver is dropped, which happens when
/// the butler thread exits. Every later command fails the same way, so a caller
/// that sees this should stop rather than retry.
///
/// It is an error type rather than a silent `let _ =` because swallowing it
/// leaves a dead butler accepting `Stream`/`Seek`/`Loop`/`Stop` indefinitely and
/// doing nothing — indistinguishable, from the caller's side, from working
/// playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButlerGone;

impl core::fmt::Display for ButlerGone {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "butler thread is gone; the command was discarded")
    }
}

impl std::error::Error for ButlerGone {}

/// Start a disk stream on `channel_index` from `file_path` at `offset_samples`.
pub(crate) fn stream(
    tx: &Sender<ButlerCommand>,
    channel_index: usize,
    file_path: PathBuf,
    offset_samples: usize,
) -> Result<(), ButlerGone> {
    tx.send_blocking(ButlerCommand::StreamAudioFile {
        channel_index,
        file_path,
        offset_samples,
    })
    .map_err(|_| ButlerGone)
}

/// Stop the stream on `channel_index` — drops its ring + link.
pub(crate) fn stop_stream(
    tx: &Sender<ButlerCommand>,
    channel_index: usize,
) -> Result<(), ButlerGone> {
    tx.send_blocking(ButlerCommand::StopStreaming { channel_index })
        .map_err(|_| ButlerGone)
}

/// Set varispeed (speed + direction) on `channel_index`.
pub(crate) fn set_varispeed(
    tx: &Sender<ButlerCommand>,
    channel_index: usize,
    speed: PlaybackRate,
    direction: Direction,
) -> Result<(), ButlerGone> {
    tx.send_blocking(ButlerCommand::SetVarispeed {
        channel_index,
        direction,
        speed,
    })
    .map_err(|_| ButlerGone)
}

/// Build a bare `DiskSource` over a channel whose butler link is
/// ready, alongside the channel's shared [`RtState`]. `None` while the butler
/// hasn't installed the [`ChannelPlan`] link yet. This is the un-gated consumer
/// handoff the timeline path wraps in a placement-gated reader (using the
/// returned `RtState` to recover the file sample rate) and the preview path
/// uses free-running.
pub(crate) fn take_streaming_unit(
    plans: &Arc<DashMap<usize, ChannelPlan>>,
    channel_index: usize,
) -> Option<(DiskSource, Arc<RtState>)> {
    let plan = plans.get(&channel_index)?;
    let consumer = plan.link.as_ref().map(|l| l.consumer.clone())?;
    let rt_state = plan.rt_state();
    let unit = DiskSource::new(consumer, Arc::clone(&rt_state));
    Some((unit, rt_state))
}
