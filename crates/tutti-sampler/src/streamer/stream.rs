//! [`Stream`], the control side of an open stream, and the port its commands
//! travel by.
//!
//! A stream is addressed by its [`StreamId`], which every command carries; the
//! streamer ignores an id it no longer holds, so a command can never reach
//! another stream (doc 025, B4). Nothing outside this crate names a command:
//! the control side is the [`Stream`] methods.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::StreamerSender;
use super::command::{StreamId, StreamerCommand};
use super::control::StreamerGone;
use super::rt_state::RtState;
use super::status::{StatusCell, StreamStatus};
use crate::voice::LoopSetting;

/// The way onto one streamer's command queue, cloned into every [`Stream`] and
/// handed to a [`VoicePool`](crate::VoicePool) that forwards its streaming
/// voices' loops there ([`DiskStreamer::port`](crate::DiskStreamer::port)).
///
/// It names a streamer and carries no command: a stream is steered through its
/// [`Stream`], and nothing here can address one. Control threads only: a send
/// waits while the bounded queue is full and fails once the streamer has gone
/// (every receiver dropped: the thread's on its exit, a built streamer's on
/// its drop).
#[derive(Clone, Debug)]
pub struct StreamerPort {
    tx: StreamerSender,
}

impl StreamerPort {
    pub(crate) fn new(tx: StreamerSender) -> Self {
        Self { tx }
    }

    /// Queues `cmd` for the streamer. `Ok` means it was *queued*: the streamer
    /// applies it on its own thread, and that outcome is not available
    /// synchronously.
    pub(crate) fn send(&self, cmd: StreamerCommand) -> Result<(), StreamerGone> {
        self.tx.send(cmd).map_err(|_| StreamerGone)
    }

    /// Sets or clears the loop of `stream`, which the streamer ignores once
    /// the stream is gone.
    pub(crate) fn send_loop(
        &self,
        stream: StreamId,
        setting: LoopSetting,
    ) -> Result<(), StreamerGone> {
        self.send(loop_command(stream, setting))
    }
}

/// The streamer's command for a loop `setting` on `stream`.
///
/// Negative positions are clamped to zero (a file has no frame before its
/// first); doc 025 cut 6 turns the clamp into a refusal.
fn loop_command(stream: StreamId, setting: LoopSetting) -> StreamerCommand {
    match setting {
        LoopSetting::On {
            start,
            end,
            crossfade_frames,
        } => StreamerCommand::SetStreamLoop {
            stream,
            range: (start.get().max(0.0) as u64, end.get().max(0.0) as u64),
            crossfade_frames,
        },
        LoopSetting::Off => StreamerCommand::ClearStreamLoop { stream },
    }
}

/// The control side of an open stream: a handle that ends the stream when it
/// is dropped.
///
/// What a stream's [`StreamReader`](crate::StreamReader) plays is the ring the
/// streamer keeps filled; this is how the control thread steers and ends it.
/// Dropping the `Stream` sends the streamer a close, and the streamer frees the
/// stream's region in the step that handles it. A voice built from the reader
/// outlives that only as long as the ring it holds (the last holder of the
/// ring frees it), so a host keeps the `Stream` for as long as the voice
/// plays. A `Stream` is control-thread only: it is never a part of a voice,
/// so its drop is never on the audio thread.
///
/// Dropping waits while the streamer's queue is full, as every control
/// command does.
///
/// ```compile_fail
/// # use tutti_sampler::Stream;
/// # fn two(stream: Stream) {
/// let second = stream.clone(); // a stream has one handle
/// # }
/// ```
///
/// Its compiling twin only borrows it:
///
/// ```
/// # use tutti_sampler::Stream;
/// # fn one(stream: Stream) {
/// let _again = &stream;
/// # }
/// ```
pub struct Stream {
    id: StreamId,
    port: StreamerPort,
    /// What the streamer publishes of the stream ([`status`](Self::status)).
    status: Arc<StatusCell>,
    /// The cell the voice counts its underruns in; this handle is its one
    /// reader.
    rt_state: Arc<RtState>,
    /// Underruns read out of `rt_state` so far.
    underruns: AtomicU64,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").field("id", &self.id).finish()
    }
}

impl Stream {
    pub(crate) fn new(
        id: StreamId,
        port: StreamerPort,
        status: Arc<StatusCell>,
        rt_state: Arc<RtState>,
    ) -> Self {
        Self {
            id,
            port,
            status,
            rt_state,
            underruns: AtomicU64::new(0),
        }
    }

    /// The stream's status, as it is now: a level read, never blocking.
    ///
    /// [`StreamStatus::phase`] is what the streamer last published: `Opening`
    /// until the ring is primed (wait for it to be anything else before
    /// starting a voice that must not underrun), `Live` while it serves the
    /// stream, `Ended` once the whole file is in the ring,
    /// `Faulted` for a decode fault (the voice reads silence from it), a panic
    /// the streamer contained to this stream, or a streamer that is gone.
    /// [`StreamStatus::underruns`] counts the frames the voice found missing
    /// in the ring, since the stream opened: the audio side counts them
    /// (the voice's underrun report) and this is the one place they are read.
    ///
    /// Control thread; no allocation.
    #[must_use]
    pub fn status(&self) -> StreamStatus {
        let drained = self.rt_state.take_underruns();
        let underruns = self.underruns.fetch_add(drained, Ordering::Relaxed) + drained;
        StreamStatus {
            phase: self.status.phase(),
            underruns,
        }
    }

    /// The stream this handle steers: the identity every command it sends
    /// carries.
    #[must_use]
    pub fn id(&self) -> StreamId {
        self.id
    }

    /// Sets, replaces or clears the loop the stream plays, in file frames
    /// ([`LoopSetting::On`]'s positions index the file, not the engine's
    /// rate).
    ///
    /// A change near the playhead is heard about 256 frames past the block
    /// being played, crossfaded; one further ahead, exactly where it takes
    /// effect. See [`LoopSetting::On`] for the whole rule.
    ///
    /// `Ok` means the command was *queued*; the streamer applies it on its
    /// own thread. Control thread: it waits while the streamer's bounded queue
    /// is full, so nothing reachable from a graph node's `process` calls it.
    ///
    /// # Errors
    ///
    /// [`StreamerGone`] means the streamer thread has exited and the command
    /// was discarded. It is permanent: every later command fails the same
    /// way, so a caller should stop rather than retry.
    #[must_use = "a discarded loop is a loop that never plays"]
    pub fn set_loop(&mut self, setting: LoopSetting) -> Result<(), StreamerGone> {
        self.port.send_loop(self.id, setting)
    }
}

/// A dropped `Stream` ends its stream. A streamer that is already gone has
/// nothing left to end.
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.port.send(StreamerCommand::Close { stream: self.id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::SamplePosition;

    /// A handle on no streamer's stream: only its sends are in question.
    fn detached(id: StreamId, port: StreamerPort) -> Stream {
        Stream::new(
            id,
            port,
            Arc::new(StatusCell::new()),
            Arc::new(RtState::new()),
        )
    }

    fn on(start: f64, end: f64) -> LoopSetting {
        LoopSetting::On {
            start: SamplePosition(start),
            end: SamplePosition(end),
            crossfade_frames: 7,
        }
    }

    /// Both loop variants report a dead streamer, and dropping a stream whose
    /// streamer is gone has nothing to end and does not fail.
    ///
    /// Covering both variants, not one representative: they are separate
    /// sends, so one whose `Result` went unpropagated would accept a loop
    /// indefinitely and do nothing, indistinguishable from working playback.
    ///
    /// Mutation (run): `Stream::set_loop` answering `Ok(())` whatever the send
    /// said -> fails.
    #[test]
    fn a_stream_on_a_gone_streamer_reports_it() {
        let (tx, queue) = crossbeam_channel::bounded(8);
        let mut stream = detached(StreamId(0), StreamerPort::new(tx));
        drop(queue);
        assert_eq!(stream.set_loop(LoopSetting::Off), Err(StreamerGone));
        assert_eq!(stream.set_loop(on(0.0, 100.0)), Err(StreamerGone));
        drop(stream);
    }

    /// Every command a stream sends names that stream, and only it: two streams
    /// on one queue each send their own id, for a loop, a clear and the close
    /// their drop sends (B4: a command meant for one cannot name another). A
    /// negative position is clamped to the file's first frame.
    ///
    /// Mutation (run): `set_loop` sending `StreamId(0)` for every stream -> the
    /// second stream's loop names the first -> fails. Mutation (run): `Drop`
    /// sending the close without the stream's id (`StreamId(0)`) -> fails.
    #[test]
    fn a_stream_names_itself_in_every_command() {
        let (tx, queue) = crossbeam_channel::bounded(16);
        let port = StreamerPort::new(tx);
        let (mut a, mut b) = (
            detached(StreamId(1), port.clone()),
            detached(StreamId(2), port),
        );
        a.set_loop(on(-5.0, 100.0)).unwrap();
        b.set_loop(LoopSetting::Off).unwrap();
        drop(a);
        drop(b);
        let sent: Vec<_> = queue.try_iter().collect();
        assert!(
            matches!(
                sent.as_slice(),
                [
                    StreamerCommand::SetStreamLoop {
                        stream: StreamId(1),
                        range: (0, 100),
                        crossfade_frames: 7,
                    },
                    StreamerCommand::ClearStreamLoop {
                        stream: StreamId(2)
                    },
                    StreamerCommand::Close {
                        stream: StreamId(1)
                    },
                    StreamerCommand::Close {
                        stream: StreamId(2)
                    },
                ]
            ),
            "{sent:?}"
        );
    }
}
