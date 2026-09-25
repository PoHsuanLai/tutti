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
use tutti_core::{PlaybackRate, SamplePosition, SampleRate};

use super::{ButlerCommand, ChannelPlan, RtState};
use crate::voice::disk_voice::DiskSource;
use crate::voice::types::Direction;
use crate::voice::LoopSetting;

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
/// ready, alongside the channel's shared [`RtState`], the file's own rate and
/// the stream's [`StreamOrigin`].
/// `None` while the butler hasn't installed the [`ChannelPlan`] link yet. This
/// is the un-gated consumer handoff the timeline path wraps in a
/// placement-gated reader (which converts beats to file frames at the file's
/// rate) and the preview path uses free-running.
pub(crate) fn take_streaming_unit(
    plans: &Arc<DashMap<usize, ChannelPlan>>,
    channel_index: usize,
) -> Option<(DiskSource, Arc<RtState>, SampleRate, StreamOrigin)> {
    let plan = plans.get(&channel_index)?;
    let link = plan.link.as_ref()?;
    let (consumer, file_rate) = (link.consumer.clone(), link.file_rate);
    let origin = StreamOrigin {
        plans: Arc::clone(plans),
        channel_index,
        region_id: link.region_id,
    };
    let rt_state = plan.rt_state();
    let unit = DiskSource::new(consumer, Arc::clone(&rt_state));
    Some((unit, rt_state, file_rate, origin))
}

/// Which butler stream a voice consumes: its channel, and the region its ring
/// belongs to.
///
/// A **read-only** handle onto the butler's own record of that stream (the
/// channel's [`Link`](super::plan::Link)): it can describe the stream, and
/// nothing reachable through it can drive one. It exists so a fork of a disk
/// voice can play the same material without the live butler (see
/// `DiskVoice::rebind_offline`). The description is read when it is asked
/// for, from the record the butler keeps, rather than copied into the voice
/// when the voice is built: a loop is set on the stream later
/// (`Command::Loop`), and a copy would miss it.
///
/// The region id is what makes the answer this voice's. A channel can be
/// restarted on another file (`Command::Stream` again), and a voice still
/// holding the old ring is then on a stream the butler no longer feeds; asked
/// about it, this answers `None`, which is what that voice plays live.
#[derive(Clone)]
pub(crate) struct StreamOrigin {
    plans: Arc<DashMap<usize, ChannelPlan>>,
    channel_index: usize,
    region_id: super::command::RegionId,
}

/// What a butler stream plays, read from the butler's record of it: the file,
/// the file's own rate and the loop. Enough to play the same material with no
/// butler at all.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StreamFile {
    /// The file the stream was started on.
    pub(crate) path: PathBuf,
    /// The file's own rate, as the butler recorded it from the header.
    pub(crate) file_rate: SampleRate,
    /// The loop set on the stream, in file frames; `Off` when there is none.
    pub(crate) loop_: LoopSetting,
}

impl StreamOrigin {
    /// Describe the stream as the butler records it now, or `None` when it
    /// no longer streams this voice's region (stopped, or restarted on
    /// another file).
    ///
    /// **Control thread only**: it takes a read lock on one shard of the plan
    /// map, which the butler also locks. Never call it on the audio thread.
    pub(crate) fn describe(&self) -> Option<StreamFile> {
        let plan = self.plans.get(&self.channel_index)?;
        let link = plan
            .link
            .as_ref()
            .filter(|link| link.region_id == self.region_id)?;
        let loop_ = link
            .loop_config
            .as_ref()
            .map_or(LoopSetting::Off, |config| LoopSetting::On {
                start: SamplePosition(config.range.0 as f64),
                end: SamplePosition(config.range.1 as f64),
                crossfade_frames: config.crossfade_frames,
            });
        Some(StreamFile {
            path: link.file_path.clone(),
            file_rate: link.file_rate,
            loop_,
        })
    }
}
