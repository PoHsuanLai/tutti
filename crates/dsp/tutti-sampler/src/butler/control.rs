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
use tutti_core::{PlaybackRate, SampleRate};

use super::prefetch::TakeVoiceError;
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
/// the stream's [`StreamOrigin`]: the stream's one live reader, `placed` when
/// the caller wraps it in a voice that follows a clock.
///
/// [`TakeVoiceError::NotStreaming`] while the butler hasn't installed the
/// [`ChannelPlan`] link yet; [`TakeVoiceError::ReaderTaken`] once the stream
/// has its reader.
pub(crate) fn take_streaming_unit(
    plans: &Arc<DashMap<usize, ChannelPlan>>,
    channel_index: usize,
    placed: bool,
) -> Result<(DiskSource, Arc<RtState>, SampleRate, StreamOrigin), TakeVoiceError> {
    let plan = plans
        .get(&channel_index)
        .ok_or(TakeVoiceError::NotStreaming)?;
    let link = plan.link.as_ref().ok_or(TakeVoiceError::NotStreaming)?;
    link.consumer.take_reader(placed)?;
    let (consumer, file_rate) = (link.consumer.clone(), link.file_rate);
    let origin = StreamOrigin(Arc::clone(&link.record));
    let rt_state = plan.rt_state();
    let unit = DiskSource::new(consumer, Arc::clone(&rt_state));
    Ok((unit, rt_state, file_rate, origin))
}

/// A butler stream's own record of what it plays: the file, the file's rate,
/// the loop set on it, and whether it is still running.
///
/// Built when the stream starts and held by its [`Link`](super::plan::Link);
/// every voice taken from the stream holds it too, through a
/// [`StreamOrigin`]. It is deliberately small: a live voice may be the last
/// holder, and its drop can land on the audio thread, where it frees a path
/// and this record (the same class of free as the voice's `Arc<RtState>`),
/// never the butler's plan map or a wave. The cache is held weakly for the
/// same reason.
pub(crate) struct StreamRecord {
    path: PathBuf,
    file_rate: SampleRate,
    /// The loop the butler runs, told by [`Link::set_loop`](super::plan::Link::set_loop),
    /// the one place it changes. A lock, not an atomic: a loop is three
    /// values that change together, and neither side is the audio thread
    /// (the butler writes; a fork reads, on the control thread).
    loop_: std::sync::Mutex<LoopSetting>,
    /// Set when the stream's link goes: stopped, or replaced by a new stream
    /// on the channel.
    ended: std::sync::atomic::AtomicBool,
    /// The butler's wave cache, to hand a fork the file already decoded
    /// when it holds it (a file that cannot seek is always there, pinned for
    /// the stream's life).
    cache: std::sync::Weak<super::cache::LruCache>,
}

impl StreamRecord {
    pub(crate) fn new(
        path: PathBuf,
        file_rate: SampleRate,
        cache: std::sync::Weak<super::cache::LruCache>,
    ) -> Self {
        Self {
            path,
            file_rate,
            loop_: std::sync::Mutex::new(LoopSetting::Off),
            ended: std::sync::atomic::AtomicBool::new(false),
            cache,
        }
    }

    pub(crate) fn set_loop(&self, loop_: LoopSetting) {
        *self
            .loop_
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = loop_;
    }

    pub(crate) fn end(&self) {
        self.ended.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Which butler stream a voice consumes: a read-only handle on the stream's
/// [`StreamRecord`].
///
/// Nothing reachable through it can drive a stream. It exists so a fork of a
/// disk voice can play the same material without the live butler (see
/// `DiskVoice::rebind_offline`). The description is read when it is asked
/// for rather than copied into the voice when the voice is built: a loop is
/// set on the stream later (`Command::Loop`), and a copy would miss it.
#[derive(Clone)]
pub(crate) struct StreamOrigin(Arc<StreamRecord>);

/// What a butler stream plays, read from its record: the file, the file's own
/// rate and the loop, and the file decoded when the butler's cache holds it.
/// Enough to play the same material with no butler at all.
#[derive(Clone)]
pub(crate) struct StreamFile {
    /// The file the stream was started on. A fork re-opens it by this path.
    pub(crate) path: PathBuf,
    /// The file's own rate, as the butler recorded it from the header.
    pub(crate) file_rate: SampleRate,
    /// The loop set on the stream, in file frames; `Off` when there is none.
    pub(crate) loop_: LoopSetting,
    /// The whole file, when the butler's cache holds it: read in place
    /// rather than decoded (or, for a file that cannot seek, decoded whole)
    /// a second time.
    pub(crate) resident: Option<Arc<tutti_io::Wave>>,
}

impl std::fmt::Debug for StreamFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamFile")
            .field("path", &self.path)
            .field("file_rate", &self.file_rate)
            .field("loop_", &self.loop_)
            .field("resident", &self.resident.is_some())
            .finish()
    }
}

impl StreamOrigin {
    /// Describe the stream as its record says now, or `None` once it has
    /// ended (stopped, or its channel restarted on another file): a voice on
    /// it then plays nothing live either.
    ///
    /// **Control thread only**: it takes the record's lock, and a cache
    /// lookup.
    pub(crate) fn describe(&self) -> Option<StreamFile> {
        let record = &self.0;
        if record.ended.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let loop_ = *record
            .loop_
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(StreamFile {
            path: record.path.clone(),
            file_rate: record.file_rate,
            loop_,
            resident: record
                .cache
                .upgrade()
                .and_then(|cache| cache.get(&record.path)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::cache::LruCache;

    /// **A fork is handed the butler's decoded copy of the file when its cache
    /// holds one** — the same `Arc`, not a second decode — and nothing once
    /// the cache is gone. A file that cannot seek is always there (the stream
    /// pins it), so a fork of one never decodes it again.
    ///
    /// Mutation (run): `describe` answering `resident: None` → fails.
    #[test]
    fn a_fork_is_handed_the_butlers_decoded_copy() {
        let path = PathBuf::from("/cached.wav");
        let cache = Arc::new(LruCache::new(4, 1 << 20));
        let wave = Arc::new(tutti_io::Wave::new(1, 48_000.0));
        cache.insert(path.clone(), Arc::clone(&wave));
        let origin = StreamOrigin(Arc::new(StreamRecord::new(
            path,
            SampleRate(48_000.0),
            Arc::downgrade(&cache),
        )));
        let resident = origin.describe().expect("running").resident;
        assert!(resident.is_some_and(|r| Arc::ptr_eq(&r, &wave)));
        drop(cache);
        assert!(origin.describe().expect("running").resident.is_none());
        origin.0.end();
        assert!(
            origin.describe().is_none(),
            "an ended stream describes nothing"
        );
    }
}
