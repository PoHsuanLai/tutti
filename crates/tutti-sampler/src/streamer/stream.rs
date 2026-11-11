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
use super::cells::Cells;
use super::command::{StreamId, StreamerCommand};
use super::control::{LoopError, StreamFile, StreamerGone};
use super::loops::RingControl;
use super::status::{StatusCell, StreamStatus};
use crate::voice::{Direction, LoopSetting};
use tutti_rt::ArcLend;
use tutti_types::SamplePosition;

/// The way onto one streamer's command queue, cloned into every [`Stream`].
///
/// Private to the crate: the public command surface is the `Stream` methods.
/// Control threads only: a send waits while the bounded queue is full and
/// fails once the streamer has gone (every receiver dropped: the thread's on
/// its exit, a built streamer's on its drop).
#[derive(Clone, Debug)]
pub(crate) struct StreamerPort {
    tx: StreamerSender,
}

impl StreamerPort {
    pub(crate) fn new(tx: impl Into<StreamerSender>) -> Self {
        Self { tx: tx.into() }
    }

    /// Queues `cmd` for the streamer. `Ok` means it was *queued*: the streamer
    /// applies it on its own thread, and that outcome is not available
    /// synchronously.
    pub(crate) fn send(&self, cmd: StreamerCommand) -> Result<(), StreamerGone> {
        self.tx.send(cmd).map_err(|_| StreamerGone)
    }
}

/// The streamer's command for a loop `setting` on `stream`.
///
/// A position that is not a frame of a file (negative, or not a number) is
/// refused, never clamped: a loop that began at the first frame when it was
/// asked to begin before it is a loop nobody asked for (doc 025 §9, decision
/// 22).
fn loop_command(stream: StreamId, setting: LoopSetting) -> Result<StreamerCommand, LoopError> {
    match setting {
        LoopSetting::On {
            start,
            end,
            crossfade_frames,
        } => {
            let frame = |p: SamplePosition| {
                let p = p.get();
                // `!(p >= 0.0)` also refuses NaN.
                if p >= 0.0 {
                    Ok(p as u64)
                } else {
                    Err(LoopError::Position)
                }
            };
            Ok(StreamerCommand::SetStreamLoop {
                stream,
                range: (frame(start)?, frame(end)?),
                crossfade_frames,
            })
        }
        LoopSetting::Off => Ok(StreamerCommand::ClearStreamLoop { stream }),
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
    /// The cells the voice counts its underruns in; this handle is the one
    /// reader of the count.
    cells: Arc<Cells>,
    /// Underruns read out of `cells` so far.
    underruns: AtomicU64,
    /// What the stream plays, immutable ([`file`](Self::file)).
    file: Arc<StreamFile>,
    /// The ring's published control: the loop as applied is read from it
    /// ([`StreamStatus::loop_`]).
    control: Arc<ArcLend<RingControl>>,
    /// The loop as last set through this handle, whose send succeeded: the
    /// loop a caller asked for, which a voice's own record of its loop is
    /// kept equal to.
    loop_: LoopSetting,
    /// The direction as last set through this handle, whose send succeeded.
    direction: Direction,
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
        cells: Arc<Cells>,
        file: Arc<StreamFile>,
        control: Arc<ArcLend<RingControl>>,
    ) -> Self {
        Self {
            id,
            port,
            status,
            cells,
            underruns: AtomicU64::new(0),
            file,
            control,
            loop_: LoopSetting::Off,
            direction: Direction::Forward,
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
    /// [`StreamStatus::loop_`] is the loop the streamer applied: what an
    /// export fork of the stream's voice plays, and what a downgrade (a lead-in
    /// that could not be read) shows as, where [`loop_`](Self::loop_) is the
    /// request.
    ///
    /// Control thread; no allocation.
    #[must_use]
    pub fn status(&self) -> StreamStatus {
        let drained = self.cells.take_underruns();
        let underruns = self.underruns.fetch_add(drained, Ordering::Relaxed) + drained;
        StreamStatus {
            phase: self.status.phase(),
            loop_: self.control.current().loop_,
            underruns,
        }
    }

    /// The file the stream plays: its path and rate, and the wave when the
    /// streamer holds it whole. What a fork of the stream's voice reads.
    #[must_use]
    pub fn file(&self) -> &StreamFile {
        &self.file
    }

    /// The loop as last set through this handle: the request, which the
    /// streamer applies on its own thread
    /// ([`status`](Self::status)`.loop_` is what it applied).
    /// [`LoopSetting::Off`] until one is set. A set whose send failed is not
    /// recorded here.
    #[must_use]
    pub fn loop_(&self) -> LoopSetting {
        self.loop_
    }

    /// The direction as last set through this handle.
    #[must_use]
    pub fn direction(&self) -> Direction {
        self.direction
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
    /// [`LoopError::Position`]: a point of the loop is negative or not a
    /// number. Nothing was sent and the stream is as it was.
    ///
    /// [`LoopError::StreamerGone`] means the streamer thread has exited and
    /// the command was discarded. It is permanent: every later command fails
    /// the same way, so a caller should stop rather than retry.
    #[must_use = "a discarded loop is a loop that never plays"]
    pub fn set_loop(&mut self, setting: LoopSetting) -> Result<(), LoopError> {
        self.port.send(loop_command(self.id, setting)?)?;
        // Recorded only once the streamer has the command: a failed send leaves
        // the stream as it was.
        self.loop_ = setting;
        Ok(())
    }

    /// Plays the stream forwards or backwards.
    ///
    /// Reverse reads the file mirrored and ignores the loop, as the memory
    /// tier's reverse does. The streamer turns the ring's mapping on its own
    /// thread, and the change is heard just past the block the voice is in,
    /// crossfaded (a loop change's rule). The direction is published with the
    /// ring's control: the voice and a fork read it from there, and nothing
    /// polls a flag.
    ///
    /// `Ok` means the command was *queued*. Control thread: it waits while the
    /// streamer's bounded queue is full.
    ///
    /// # Errors
    ///
    /// [`StreamerGone`] means the streamer thread has exited and the command
    /// was discarded; the stream's direction is as it was. It is permanent.
    #[must_use = "a discarded direction is a stream that never turns"]
    pub fn set_direction(&mut self, direction: Direction) -> Result<(), StreamerGone> {
        self.port.send(StreamerCommand::SetDirection {
            stream: self.id,
            direction,
        })?;
        self.direction = direction;
        Ok(())
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
impl Stream {
    /// A handle on no streamer's stream: only its sends are in question (tests).
    pub(crate) fn detached(id: StreamId, port: StreamerPort) -> Stream {
        Stream::new(
            id,
            port,
            Arc::new(StatusCell::new()),
            Arc::new(Cells::new()),
            Arc::new(StreamFile::new(
                std::path::PathBuf::new(),
                tutti_types::SampleRate(48_000.0),
                None,
            )),
            Arc::new(ArcLend::new(RingControl::new(
                super::loops::RingMap::plain(
                    super::loops::Arrangement::plain(tutti_types::Samples::new(0)),
                    0,
                ),
                LoopSetting::Off,
            ))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    #[test]
    fn a_stream_on_a_gone_streamer_reports_it() {
        let (tx, queue) = crossbeam_channel::bounded(8);
        let mut stream = Stream::detached(StreamId(0), StreamerPort::new(tx));
        drop(queue);
        assert_eq!(
            stream.set_loop(LoopSetting::Off),
            Err(LoopError::StreamerGone)
        );
        assert_eq!(
            stream.set_loop(on(0.0, 100.0)),
            Err(LoopError::StreamerGone)
        );
        drop(stream);
    }

    /// **A loop point that is not a file frame is refused, not clamped**: a
    /// negative or NaN start or end answers `LoopError::Position`, sends
    /// nothing, and leaves the stream's loop as it was; the first frame is a
    /// valid point.
    #[test]
    fn a_negative_loop_position_is_refused() {
        let (tx, queue) = crossbeam_channel::bounded(8);
        let mut stream = Stream::detached(StreamId(5), StreamerPort::new(tx));
        for bad in [on(-5.0, 100.0), on(0.0, -1.0), on(f64::NAN, 100.0)] {
            assert_eq!(stream.set_loop(bad), Err(LoopError::Position), "{bad:?}");
        }
        assert_eq!(stream.loop_(), LoopSetting::Off, "the stream is as it was");
        assert!(queue.try_recv().is_err(), "nothing was sent");
        stream
            .set_loop(on(0.0, 100.0))
            .expect("the first frame is a frame");
    }

    /// Every command a stream sends names that stream, and only it: two streams
    /// on one queue each send their own id, for a loop, a clear and the close
    /// their drop sends (B4: a command meant for one cannot name another).
    #[test]
    fn a_stream_names_itself_in_every_command() {
        let (tx, queue) = crossbeam_channel::bounded(16);
        let port = StreamerPort::new(tx);
        let (mut a, mut b) = (
            Stream::detached(StreamId(1), port.clone()),
            Stream::detached(StreamId(2), port),
        );
        a.set_loop(on(5.0, 100.0)).unwrap();
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
                        range: (5, 100),
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

    /// **`set_loop` and `set_direction` leave the stream unchanged when the
    /// send fails**: a stream whose streamer is gone keeps the loop and the
    /// direction it was last given, and says so, rather than claim a
    /// setting nobody received.
    #[test]
    fn a_failed_send_leaves_the_stream_as_it_was() {
        let (tx, queue) = crossbeam_channel::bounded(8);
        let mut stream = Stream::detached(StreamId(3), StreamerPort::new(tx));
        assert_eq!(stream.loop_(), LoopSetting::Off);
        assert_eq!(stream.direction(), Direction::Forward);
        stream.set_loop(on(0.0, 100.0)).expect("the streamer lives");
        stream
            .set_direction(Direction::Reverse)
            .expect("the streamer lives");
        assert_eq!(stream.loop_(), on(0.0, 100.0), "recorded once sent");
        assert_eq!(stream.direction(), Direction::Reverse);

        drop(queue);
        assert_eq!(
            stream.set_loop(LoopSetting::Off),
            Err(LoopError::StreamerGone)
        );
        assert_eq!(stream.set_direction(Direction::Forward), Err(StreamerGone));
        assert_eq!(
            stream.loop_(),
            on(0.0, 100.0),
            "the failed loop is not recorded"
        );
        assert_eq!(stream.direction(), Direction::Reverse);
    }
}
