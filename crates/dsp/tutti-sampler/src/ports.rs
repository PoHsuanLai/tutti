//! Public ports onto the sampler streaming engine, split MIDI-device-style into
//! a WRITE port ([`Commands`]) and a READ port ([`Status`]).
//!
//! The [`DiskStreamer`](crate::DiskStreamer) handle owns the butler thread; these two
//! cloneable handles are the differentiated surfaces onto it:
//!
//! - [`Commands`] wraps the butler command channel and exposes a single
//!   [`send`](Commands::send) over a public [`Command`] enum — mirroring the
//!   audio-thread [`VoicePoolHandle`](crate::VoicePoolHandle).
//! - [`Status`] wraps the sample rate + channel-plan map and exposes reads
//!   ([`sample_rate`](Status::sample_rate)) plus the reader-factory
//!   [`take_disk_voice`](Status::take_disk_voice).

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;

use crate::butler::{
    ButlerCommand, ButlerGone, ButlerSender, ChannelPlan, SessionRate, TakeVoiceError, TrySendError,
};
use crate::voice::{Direction, DiskVoice, DiskVoiceConfig, LoopSetting, VoiceWindow};
use tutti_core::{Beat, BeatDuration, PlaybackRate, SamplePosition, SampleRate};
use tutti_io::Wave;

/// The caller's stated choice of playback tier for a voice: whole-file in memory
/// (`Memory`) or incremental disk streaming (`Disk`).
///
/// Plain data. The sampler never decides the tier on its own; it plays whichever
/// variant it is handed, and the caller owns the decision — a host's tiering
/// policy, say. [`probe`](crate::probe) reports what a file *allows* —
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
    /// File streamed incrementally from disk by the butler thread.
    ///
    /// The path is opened on the butler thread, not here, so constructing this
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

/// A butler stream-control command, one public variant per streaming operation.
///
/// Each variant maps to exactly one internal `ButlerCommand`; the [`Commands`]
/// port performs that mapping in [`send`](Commands::send).
///
/// # Positions are in frames
///
/// Every [`SamplePosition`] here indexes the **file** in frames, not in
/// interleaved samples and not in engine-rate frames. A caller that divides by
/// the channel count, or that converts through the session rate instead of the
/// file's own, addresses a fraction of the intended point — a 6-channel file
/// then seeks to a sixth of where it was asked to.
///
/// Negative positions are clamped to zero on dispatch rather than rejected.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Command {
    /// Register a disk-streaming source for a timeline clip on `channel_index`,
    /// starting `offset` frames into the file. Maps to `StreamAudioFile`.
    Stream {
        /// Butler channel this stream occupies. One live stream per index.
        channel_index: usize,
        /// The file to stream. Opened by the butler thread, not here.
        file_path: PathBuf,
        /// Start position, in frames from the head of the file.
        offset: SamplePosition,
    },
    /// Reposition a stream's free-running reader (a bare `DiskSource`). Maps to
    /// `SeekStream`. A placed voice (`take_disk_voice`) follows its clock and
    /// ignores this: seek the clock.
    Seek {
        /// Butler channel carrying the stream to move.
        channel_index: usize,
        /// Absolute target, in frames from the head of the file.
        file_position: SamplePosition,
    },
    /// Set varispeed: playback speed magnitude plus direction. Maps to
    /// `SetVarispeed`.
    ///
    /// [`PlaybackRate`] is the pitch-coupled kind of speed change — reading
    /// faster transposes up. The pitch-independent kind is `StretchFactor`, and
    /// it does not travel this port; it lives on the voice's stretch filter.
    SetSpeed {
        /// Butler channel carrying the stream to respeed.
        channel_index: usize,
        /// Magnitude only; the sign is carried by `direction`.
        speed: PlaybackRate,
        /// Forward or reverse.
        direction: Direction,
    },
    /// Enable, replace or disable looping. `LoopSetting::On { .. }` maps to
    /// `SetStreamLoop`; `LoopSetting::Off` maps to `ClearStreamLoop`.
    ///
    /// A change near the playhead is heard about 256 frames past the block
    /// being played, crossfaded; one further ahead, exactly where it takes
    /// effect. See [`LoopSetting::On`] for the whole rule.
    Loop {
        /// Butler channel carrying the stream to loop.
        channel_index: usize,
        /// The loop intent. Its `start`/`end` are file frames and its
        /// `crossfade_frames` are frames, matching this enum's rule.
        setting: LoopSetting,
    },
    /// Stop the stream on a channel, dropping its ring and link. Maps to
    /// `StopStreaming`.
    Stop {
        /// Butler channel to tear down.
        channel_index: usize,
    },
}

impl Command {
    /// The one `ButlerCommand` this maps to: the single place the public
    /// variants meet the butler's, so the blocking and the non-blocking send
    /// cannot disagree about one.
    fn into_butler(self) -> ButlerCommand {
        match self {
            Command::Stream {
                channel_index,
                file_path,
                offset,
            } => ButlerCommand::StreamAudioFile {
                channel_index,
                file_path,
                offset_samples: offset.get().max(0.0) as usize,
            },
            Command::Seek {
                channel_index,
                file_position,
            } => ButlerCommand::SeekStream {
                channel_index,
                file_position: file_position.get().max(0.0) as u64,
            },
            Command::SetSpeed {
                channel_index,
                speed,
                direction,
            } => ButlerCommand::SetVarispeed {
                channel_index,
                direction,
                speed,
            },
            Command::Loop {
                channel_index,
                setting:
                    LoopSetting::On {
                        start,
                        end,
                        crossfade_frames,
                    },
            } => ButlerCommand::SetStreamLoop {
                channel_index,
                range: (start.get().max(0.0) as u64, end.get().max(0.0) as u64),
                crossfade_frames,
            },
            Command::Loop {
                channel_index,
                setting: LoopSetting::Off,
            } => ButlerCommand::ClearStreamLoop { channel_index },
            Command::Stop { channel_index } => ButlerCommand::StopStreaming { channel_index },
        }
    }
}

/// Why [`Commands::try_send`] refused a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButlerRefused {
    /// The butler's queue was full: it is stalled. Transient.
    Full,
    /// The butler thread has exited ([`ButlerGone`]). Permanent.
    Gone,
}

/// WRITE port onto the butler: a cloneable wrapper over the butler's
/// command queue that dispatches a public [`Command`] to the butler.
///
/// Mirrors [`VoicePoolHandle`](crate::VoicePoolHandle) — a thin,
/// `Clone` handle over a sender with a single [`send`](Self::send) method.
#[derive(Clone, Debug)]
pub struct Commands {
    tx: ButlerSender,
}

impl Commands {
    pub(crate) fn new(tx: ButlerSender) -> Self {
        Self { tx }
    }

    /// Dispatches a single command to the butler. **Control thread**: while
    /// the butler's bounded queue is full it waits, waking the butler and
    /// retrying every 100 µs, so nothing reachable from a graph node's
    /// `process` calls it (the audio thread's one send, a voice pool
    /// forwarding a streaming loop, uses `try_send`).
    ///
    /// `Ok` means the command was *queued* — the butler applies it on its own
    /// thread, and that outcome is not available synchronously.
    ///
    /// # Errors
    ///
    /// `ButlerGone` means the butler thread has exited and the command was
    /// **discarded**. It is permanent: every later command fails the same way,
    /// so a caller should stop rather than retry.
    ///
    /// Ignoring that error is the failure this port is shaped to prevent — a
    /// dead butler silently accepting `Stream`, `Seek`, `Loop` and `Stop`
    /// forever is indistinguishable from working playback.
    #[must_use = "a discarded command is a stream that never starts, seeks, or stops"]
    pub fn send(&self, cmd: Command) -> Result<(), ButlerGone> {
        self.tx
            .send_blocking(cmd.into_butler())
            .map_err(|_| ButlerGone)
    }

    /// Dispatches a command without blocking: the audio thread's send.
    ///
    /// A full queue (the butler is stalled, on I/O or descheduled) refuses
    /// the command rather than holding the callback until the butler drains
    /// it. The caller decides what a refusal means; the voice pool counts it
    /// and does not record the intent.
    ///
    /// Takes no lock and allocates nothing, on the queue's first send as on
    /// any other: a push into a fixed-capacity lock-free queue and an
    /// `unpark` of the butler thread (`butler::queue`). It frees nothing on
    /// refusal either (no variant the audio thread sends owns heap data).
    ///
    /// Lock-free, not wait-free: on a full queue the push can spin briefly
    /// behind a pop the butler has begun on the same slot, and senders
    /// retry against each other's claims. The wait-free alternative (a
    /// single-producer ring per sender) is deferred; see `butler::queue`.
    ///
    /// `Gone` means the butler has exited or is exiting. A command sent as
    /// it exits may still have run, or been dropped by its final drain.
    pub(crate) fn try_send(&self, cmd: Command) -> Result<(), ButlerRefused> {
        self.tx.try_send(cmd.into_butler()).map_err(|e| match e {
            TrySendError::Full(_) => ButlerRefused::Full,
            TrySendError::Closed => ButlerRefused::Gone,
        })
    }

    /// Dispatches a batch of commands, in order.
    ///
    /// Returns how many were queued. `< cmds.len()` means the butler was gone
    /// and the rest were **discarded** — dispatch stops at the first failure,
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

/// READ port onto the streaming engine: a cloneable snapshot of the sample rate
/// and the channel-plan map, exposing state reads plus the reader-factory.
#[derive(Clone)]
pub struct Status {
    sample_rate: SessionRate,
    plans: Arc<DashMap<usize, ChannelPlan>>,
}

// `ChannelPlan` isn't `Debug` (it holds butler-internal cache/link state), so
// hand-roll a summary rather than deriving — never touch the map's contents to
// avoid contending with the butler thread.
impl std::fmt::Debug for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Status")
            .field("sample_rate", &self.sample_rate.get())
            .finish_non_exhaustive()
    }
}

impl Status {
    pub(crate) fn new(sample_rate: SessionRate, plans: Arc<DashMap<usize, ChannelPlan>>) -> Self {
        Self { sample_rate, plans }
    }

    /// The session rate: the one the streamer was built with, or the one
    /// [`DiskStreamer::set_sample_rate`](crate::DiskStreamer::set_sample_rate)
    /// last moved it to. Read live, so a `Status` taken before a device
    /// restart reports the rate after it.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate.get()
    }

    /// Builds a [`DiskVoice`] for a channel whose butler stream is ready, placed
    /// on the timeline at `start_beat` for `duration` (the transport it reads
    /// is its block's, from the graph).
    ///
    /// Pulls the ring consumer and shared RT state out of the channel's plan and
    /// wraps them in a placement-gated reader. `start_beat` and `duration` are
    /// musical time — the gate converts them to **file frames** using the file's
    /// own rate, not the session's: the rate the butler recorded when it opened
    /// the stream, so a session rate that moves later (a device restart) leaves
    /// the gate's frames where they were.
    ///
    /// The voice also keeps a read-only handle onto the butler's record of
    /// the stream, so a fork of it for an offline render (an export) can read
    /// the same file on its own rather than through the live ring; see
    /// `DiskVoice::fork_copy`.
    ///
    /// Control thread; the returned voice is then moved into the graph.
    ///
    /// # Errors
    ///
    /// [`TakeVoiceError::NotStreaming`] while the butler has not opened the
    /// stream yet: a caller polls again next frame rather than treating it as
    /// a failure. [`TakeVoiceError::ReaderTaken`] once a voice has been taken
    /// from the stream: a stream serves one live voice.
    pub fn take_disk_voice(
        &self,
        channel_index: usize,
        start_beat: Beat,
        duration: Option<BeatDuration>,
    ) -> Result<DiskVoice, TakeVoiceError> {
        let (inner, rt_state, file_sample_rate, origin) =
            crate::butler::control::take_streaming_unit(&self.plans, channel_index, true)?;

        Ok(DiskVoice::new(
            inner,
            rt_state,
            DiskVoiceConfig {
                window: VoiceWindow {
                    start: start_beat,
                    duration,
                },
                file_sample_rate,
            },
        )
        .with_origin(origin))
    }

    /// The stream's live reader as a bare free-running source (tests: no
    /// public caller takes one), and its shared state.
    #[cfg(test)]
    pub(crate) fn take_free_running(
        &self,
        channel_index: usize,
    ) -> Result<
        (
            crate::voice::disk_voice::DiskSource,
            Arc<crate::butler::RtState>,
        ),
        TakeVoiceError,
    > {
        let (unit, rt_state, _, _) =
            crate::butler::control::take_streaming_unit(&self.plans, channel_index, false)?;
        Ok((unit, rt_state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Commands` whose butler is already gone — the state a real handle
    /// reaches once `ButlerThread::stop` has run.
    fn dead_butler() -> Commands {
        let queue = crate::butler::ButlerQueue::new(8);
        queue.close();
        Commands::new(ButlerSender::new(queue))
    }

    /// Every command variant reports a dead butler.
    ///
    /// Covering every variant, not one representative: the arms send
    /// independently, so a variant whose `Result` went unpropagated would accept
    /// `Stream`, `Seek`, `Loop` or `Stop` indefinitely and do nothing —
    /// indistinguishable from working playback, and invisible to a test that
    /// only exercised its neighbours.
    #[test]
    fn every_command_reports_a_dead_butler() {
        let cmds = dead_butler();

        assert_eq!(
            cmds.send(Command::Stream {
                channel_index: 0,
                file_path: "/nonexistent.wav".into(),
                offset: SamplePosition(0.0),
            }),
            Err(ButlerGone)
        );
        assert_eq!(
            cmds.send(Command::Seek {
                channel_index: 0,
                file_position: SamplePosition(0.0),
            }),
            Err(ButlerGone)
        );
        assert_eq!(
            cmds.send(Command::SetSpeed {
                channel_index: 0,
                speed: PlaybackRate::new(1.0),
                direction: Direction::Forward,
            }),
            Err(ButlerGone)
        );
        assert_eq!(
            cmds.send(Command::Loop {
                channel_index: 0,
                setting: LoopSetting::Off,
            }),
            Err(ButlerGone),
            "the Loop/Off arm maps to its own butler command"
        );
        assert_eq!(
            cmds.send(Command::Loop {
                channel_index: 0,
                setting: LoopSetting::On {
                    start: SamplePosition(0.0),
                    end: SamplePosition(100.0),
                    crossfade_frames: 0,
                },
            }),
            Err(ButlerGone),
            "the Loop/On arm is a separate send from Loop/Off"
        );
        assert_eq!(
            cmds.send(Command::Stop { channel_index: 0 }),
            Err(ButlerGone)
        );
    }

    /// A batch reports how many landed rather than vanishing wholesale.
    #[test]
    fn send_all_reports_nothing_queued_to_a_dead_butler() {
        let cmds = dead_butler();
        let queued = cmds.send_all([
            Command::Stop { channel_index: 0 },
            Command::Stop { channel_index: 1 },
            Command::Stop { channel_index: 2 },
        ]);
        assert_eq!(
            queued, 0,
            "a whole batch vanished; the count is what says so"
        );
    }

    /// With a live receiver everything queues, and the batch count matches.
    #[test]
    fn send_all_counts_what_it_queued() {
        let queue = crate::butler::ButlerQueue::new(8);
        let cmds = Commands::new(ButlerSender::new(std::sync::Arc::clone(&queue)));

        let queued = cmds.send_all([
            Command::Stop { channel_index: 0 },
            Command::Stop { channel_index: 1 },
        ]);

        assert_eq!(queued, 2);
        assert_eq!(queue.len(), 2, "both reached the queue");
    }
}
