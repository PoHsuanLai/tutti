//! Public ports onto the sampler streaming engine, split MIDI-device-style into
//! a WRITE port ([`Commands`]) and a READ port ([`Status`]).
//!
//! The [`DiskStreamer`](crate::DiskStreamer) handle owns the streamer thread; these two
//! cloneable handles are the differentiated surfaces onto it:
//!
//! - [`Commands`] wraps the streamer command channel and exposes a single
//!   [`send`](Commands::send) over a public [`Command`] enum — mirroring the
//!   audio-thread [`VoicePoolHandle`](crate::VoicePoolHandle).
//! - [`Status`] wraps the session rate and exposes reads
//!   ([`sample_rate`](Status::sample_rate)).
//!
//! A stream is opened with [`DiskStreamer::open`](crate::DiskStreamer::open),
//! whose reply is the stream's [`StreamReader`](crate::StreamReader).

use std::path::PathBuf;
use std::sync::Arc;

use crate::streamer::{SessionRate, StreamId, StreamerCommand, StreamerGone, StreamerSender};
use crate::voice::LoopSetting;
use tutti_codec::Wave;
use tutti_types::SampleRate;

/// The caller's stated choice of playback tier for a voice: whole-file in memory
/// (`Memory`) or incremental disk streaming (`Disk`).
///
/// Plain data. The sampler never decides the tier on its own; it plays whichever
/// variant it is handed, and the caller owns the decision — a host's tiering
/// policy, say. [`probe`](fn@crate::probe) reports what a file *allows* —
/// a `streamable: false` file has no `Disk` option at all.
///
/// An enum rather than a trait over the two tiers, for the reason the runtime
/// side documents on `VoiceSource`: the tiers differ only in the essential
/// per-sample read, and a trait makes every cold control operation
/// tier-conditional in its *meaning* while looking uniform at the call site.
#[derive(Clone)]
pub enum Source {
    /// Whole file already decoded into memory, played by a `MemorySource`.
    ///
    /// The `Wave` is shared by `Arc`, so several voices over one sample cost one
    /// copy of the audio.
    Memory(Arc<Wave>),
    /// File streamed incrementally from disk by the streamer thread.
    ///
    /// The path is opened on the streamer thread, not here, so constructing this
    /// variant does no I/O and cannot fail.
    Disk(PathBuf),
}

// `Wave` doesn't implement `Debug`, so hand-roll it (len/rate summary for
// `Memory`, path for `Disk`) rather than deriving.
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Memory(wave) => f
                .debug_struct("Memory")
                .field("len", &wave.len())
                .field("sample_rate", &wave.sample_rate())
                .finish(),
            Source::Disk(path) => f.debug_tuple("Disk").field(path).finish(),
        }
    }
}

/// A streamer stream-control command, one public variant per streaming operation.
///
/// Each variant maps to exactly one internal `StreamerCommand`; the [`Commands`]
/// port performs that mapping in [`send`](Commands::send). A stream is
/// addressed by its [`StreamId`] ([`StreamReader::id`](crate::StreamReader::id)):
/// a command for a stream the streamer no longer holds is ignored.
///
/// # Positions are in frames
///
/// Every [`SamplePosition`](tutti_types::SamplePosition) here indexes the file
/// in frames, not in interleaved samples and not in engine-rate frames. A
/// caller that divides by the channel count, or that converts through the
/// session rate instead of the file's own, addresses a fraction of the
/// intended point — a 6-channel file then seeks to a sixth of where it was
/// asked to.
///
/// Negative positions are clamped to zero on dispatch rather than rejected.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Command {
    /// Enable, replace or disable looping. `LoopSetting::On { .. }` maps to
    /// `SetStreamLoop`; `LoopSetting::Off` maps to `ClearStreamLoop`.
    ///
    /// A change near the playhead is heard about 256 frames past the block
    /// being played, crossfaded; one further ahead, exactly where it takes
    /// effect. See [`LoopSetting::On`] for the whole rule.
    Loop {
        /// The stream to loop.
        stream: StreamId,
        /// The loop intent. Its `start`/`end` are file frames and its
        /// `crossfade_frames` are frames, matching this enum's rule.
        setting: LoopSetting,
    },
    /// Stop a stream, dropping its ring and link. Maps to `StopStreaming`.
    Stop {
        /// The stream to tear down.
        stream: StreamId,
    },
}

impl Command {
    /// The one `StreamerCommand` this maps to: the single place the public
    /// variants meet the streamer's.
    fn into_streamer(self) -> StreamerCommand {
        match self {
            Command::Loop { stream, setting } => loop_command(stream, setting),
            Command::Stop { stream } => StreamerCommand::StopStreaming { stream },
        }
    }
}

/// The streamer's command for a loop `setting` on `stream`.
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

/// WRITE port onto the streamer: a cloneable wrapper over the streamer's
/// command queue that dispatches a public [`Command`] to the streamer.
///
/// Mirrors [`VoicePoolHandle`](crate::VoicePoolHandle) — a thin,
/// `Clone` handle over a sender with a single [`send`](Self::send) method.
#[derive(Clone, Debug)]
pub struct Commands {
    tx: StreamerSender,
}

impl Commands {
    pub(crate) fn new(tx: StreamerSender) -> Self {
        Self { tx }
    }

    /// Dispatches a single command to the streamer. Control thread: while
    /// the streamer's bounded queue is full it waits, so nothing reachable from
    /// a graph node's `process` calls it. No audio-thread code sends to the
    /// streamer: a voice pool's streaming loop is forwarded by its control-side
    /// handle ([`VoicePoolHandle::send`](crate::VoicePoolHandle::send)).
    ///
    /// `Ok` means the command was *queued* — the streamer applies it on its own
    /// thread, and that outcome is not available synchronously.
    ///
    /// # Errors
    ///
    /// `StreamerGone` means the streamer thread has exited and the command was
    /// discarded. It is permanent: every later command fails the same way,
    /// so a caller should stop rather than retry.
    ///
    /// Ignoring that error is the failure this port is shaped to prevent — a
    /// dead streamer silently accepting `Stream`, `Loop` and `Stop`
    /// forever is indistinguishable from working playback.
    #[must_use = "a discarded command is a stream that never starts, seeks, or stops"]
    pub fn send(&self, cmd: Command) -> Result<(), StreamerGone> {
        self.tx.send(cmd.into_streamer()).map_err(|_| StreamerGone)
    }

    /// Sets or clears the loop of `stream`: [`send`](Self::send)'s
    /// `Command::Loop`, which the streamer ignores once the stream is gone.
    /// Control thread; waits as `send` does.
    pub(crate) fn send_stream_loop(
        &self,
        stream: StreamId,
        setting: LoopSetting,
    ) -> Result<(), StreamerGone> {
        self.tx
            .send(loop_command(stream, setting))
            .map_err(|_| StreamerGone)
    }

    /// Dispatches a batch of commands, in order.
    ///
    /// Returns how many were queued. `< cmds.len()` means the streamer was gone
    /// and the rest were discarded — dispatch stops at the first failure,
    /// since the condition is permanent and every later command would fail too.
    ///
    /// A count rather than a bare `Result` because the batch is *partially*
    /// applied: commands queued before the failure still stand, so a caller
    /// needs to know where the stream of intent actually stopped. A bare
    /// `Result` cannot say whether one command landed or all of them.
    #[must_use = "a short count means the rest of the batch was discarded"]
    pub fn send_all(&self, cmds: impl IntoIterator<Item = Command>) -> usize {
        let mut queued = 0;
        for cmd in cmds {
            if self.send(cmd).is_err() {
                break;
            }
            queued += 1;
        }
        queued
    }
}

/// READ port onto the streaming engine: a cloneable snapshot of the session
/// rate.
#[derive(Clone)]
pub struct Status {
    sample_rate: SessionRate,
}

impl std::fmt::Debug for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Status")
            .field("sample_rate", &self.sample_rate.get())
            .finish_non_exhaustive()
    }
}

impl Status {
    pub(crate) fn new(sample_rate: SessionRate) -> Self {
        Self { sample_rate }
    }

    /// The session rate: the one the streamer was built with, or the one
    /// [`DiskStreamer::set_sample_rate`](crate::DiskStreamer::set_sample_rate)
    /// last moved it to. Read live, so a `Status` taken before a device
    /// restart reports the rate after it.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::SamplePosition;

    /// A `Commands` whose streamer is already gone — the state a real handle
    /// reaches once `StreamerThread::stop` has run.
    fn dead_streamer() -> Commands {
        let (tx, _) = crossbeam_channel::bounded(8);
        Commands::new(tx)
    }

    /// Every command variant reports a dead streamer.
    ///
    /// Covering every variant, not one representative: the arms send
    /// independently, so a variant whose `Result` went unpropagated would accept
    /// `Loop` or `Stop` indefinitely and do nothing —
    /// indistinguishable from working playback, and invisible to a test that
    /// only exercised its neighbours.
    #[test]
    fn every_command_reports_a_dead_streamer() {
        let cmds = dead_streamer();

        assert_eq!(
            cmds.send(Command::Loop {
                stream: StreamId(0),
                setting: LoopSetting::Off,
            }),
            Err(StreamerGone),
            "the Loop/Off arm maps to its own streamer command"
        );
        assert_eq!(
            cmds.send(Command::Loop {
                stream: StreamId(0),
                setting: LoopSetting::On {
                    start: SamplePosition(0.0),
                    end: SamplePosition(100.0),
                    crossfade_frames: 0,
                },
            }),
            Err(StreamerGone),
            "the Loop/On arm is a separate send from Loop/Off"
        );
        assert_eq!(
            cmds.send(Command::Stop {
                stream: StreamId(0)
            }),
            Err(StreamerGone)
        );
    }

    /// A batch reports how many landed rather than vanishing wholesale.
    #[test]
    fn send_all_reports_nothing_queued_to_a_dead_streamer() {
        let cmds = dead_streamer();
        let queued = cmds.send_all([
            Command::Stop {
                stream: StreamId(0),
            },
            Command::Stop {
                stream: StreamId(1),
            },
            Command::Stop {
                stream: StreamId(2),
            },
        ]);
        assert_eq!(
            queued, 0,
            "a whole batch vanished; the count is what says so"
        );
    }

    /// With a live receiver everything queues, and the batch count matches.
    #[test]
    fn send_all_counts_what_it_queued() {
        let (tx, queue) = crossbeam_channel::bounded(8);
        let cmds = Commands::new(tx);

        let queued = cmds.send_all([
            Command::Stop {
                stream: StreamId(0),
            },
            Command::Stop {
                stream: StreamId(1),
            },
        ]);

        assert_eq!(queued, 2);
        assert_eq!(queue.len(), 2, "both reached the queue");
    }
}
