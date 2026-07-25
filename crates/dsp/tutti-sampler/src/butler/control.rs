//! The single definition of the butler stream-control operations, expressed
//! over the raw handles (command channel + channel-plan map).
//!
//! Both the timeline path ([`Sampler`](crate::Sampler)) and the browser-preview
//! path ([`Auditioner`](crate::Auditioner)) drive butler streaming through these
//! functions, so "start a stream on a channel", "stop", "set varispeed", and
//! "take the streaming consumer" each have exactly one implementation.

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use smol::channel::Sender;
use tutti_core::PlaybackRate;

use super::{ButlerCommand, ChannelPlan, RtState};
use crate::clip::streaming_sampler::StreamingSamplerUnit;
use crate::clip::track_clip_reader::Direction;

/// Start a disk stream on `channel_index` from `file_path` at `offset_samples`.
pub(crate) fn stream(
    tx: &Sender<ButlerCommand>,
    channel_index: usize,
    file_path: PathBuf,
    offset_samples: usize,
) {
    let _ = tx.send_blocking(ButlerCommand::StreamAudioFile {
        channel_index,
        file_path,
        offset_samples,
    });
}

/// Stop the stream on `channel_index` — drops its ring + link.
pub(crate) fn stop_stream(tx: &Sender<ButlerCommand>, channel_index: usize) {
    let _ = tx.send_blocking(ButlerCommand::StopStreaming { channel_index });
}

/// Set varispeed (speed + direction) on `channel_index`.
pub(crate) fn set_varispeed(
    tx: &Sender<ButlerCommand>,
    channel_index: usize,
    speed: PlaybackRate,
    direction: Direction,
) {
    let _ = tx.send_blocking(ButlerCommand::SetVarispeed {
        channel_index,
        direction,
        speed,
    });
}

/// Build a bare `StreamingSamplerUnit` over a channel whose butler link is
/// ready, alongside the channel's shared [`RtState`]. `None` while the butler
/// hasn't installed the [`ChannelPlan`] link yet. This is the un-gated consumer
/// handoff the timeline path wraps in a placement-gated reader (using the
/// returned `RtState` to recover the file sample rate) and the preview path
/// uses free-running.
pub(crate) fn take_streaming_unit(
    plans: &Arc<DashMap<usize, ChannelPlan>>,
    channel_index: usize,
) -> Option<(StreamingSamplerUnit, Arc<RtState>)> {
    let plan = plans.get(&channel_index)?;
    let consumer = plan.link.as_ref().map(|l| l.consumer.clone())?;
    let rt_state = plan.rt_state();
    let unit = StreamingSamplerUnit::new(consumer, Arc::clone(&rt_state));
    Some((unit, rt_state))
}
