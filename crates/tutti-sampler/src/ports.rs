//! Public ports onto the sampler streaming engine, split MIDI-device-style into
//! a WRITE port ([`Commands`]) and a READ port ([`Status`]).
//!
//! The [`DiskStreamer`](crate::DiskStreamer) handle owns the streamer thread; these two
//! cloneable handles are the differentiated surfaces onto it:
//!
//! - [`Commands`] wraps the streamer command channel and exposes a single
//!   [`send`](Commands::send) over a public [`Command`] enum — mirroring the
//!   audio-thread [`VoicePoolHandle`](crate::VoicePoolHandle).
//! - [`Status`] wraps the sample rate + channel-plan map and exposes reads
//!   ([`sample_rate`](Status::sample_rate)) plus the reader-factory
//!   [`take_disk_voice`](Status::take_disk_voice).

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;

use crate::streamer::{
    ChannelPlan, RegionId, SessionRate, StreamerCommand, StreamerGone, StreamerSender,
    TakeVoiceError,
};
use crate::voice::{Direction, DiskVoice, DiskVoiceConfig, LoopSetting, VoiceWindow};
use tutti_codec::Wave;
use tutti_types::{Beat, BeatDuration, ChannelLayout, PlaybackRate, SamplePosition, SampleRate};

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
/// port performs that mapping in [`send`](Commands::send).
///
/// # Positions are in frames
///
/// Every [`SamplePosition`] here indexes the file in frames, not in
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
        /// Streamer channel this stream occupies. One live stream per index.
        channel_index: usize,
        /// The file to stream. Opened by the streamer thread, not here.
        file_path: PathBuf,
        /// Start position, in frames from the head of the file.
        offset: SamplePosition,
    },
    /// Reposition a stream's free-running reader (a bare `DiskSource`). Maps to
    /// `SeekStream`. A placed voice (`take_disk_voice`) follows its clock and
    /// ignores this: seek the clock.
    Seek {
        /// Streamer channel carrying the stream to move.
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
        /// Streamer channel carrying the stream to respeed.
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
        /// Streamer channel carrying the stream to loop.
        channel_index: usize,
        /// The loop intent. Its `start`/`end` are file frames and its
        /// `crossfade_frames` are frames, matching this enum's rule.
        setting: LoopSetting,
    },
    /// Stop the stream on a channel, dropping its ring and link. Maps to
    /// `StopStreaming`.
    Stop {
        /// Streamer channel to tear down.
        channel_index: usize,
    },
}

impl Command {
    /// The one `StreamerCommand` this maps to: the single place the public
    /// variants meet the streamer's.
    fn into_streamer(self) -> StreamerCommand {
        match self {
            Command::Stream {
                channel_index,
                file_path,
                offset,
            } => StreamerCommand::StreamAudioFile {
                channel_index,
                file_path,
                offset_samples: offset.get().max(0.0) as usize,
            },
            Command::Seek {
                channel_index,
                file_position,
            } => StreamerCommand::SeekStream {
                channel_index,
                file_position: file_position.get().max(0.0) as u64,
            },
            Command::SetSpeed {
                channel_index,
                speed,
                direction,
            } => StreamerCommand::SetVarispeed {
                channel_index,
                direction,
                speed,
            },
            Command::Loop {
                channel_index,
                setting,
            } => loop_command(channel_index, None, setting),
            Command::Stop { channel_index } => StreamerCommand::StopStreaming { channel_index },
        }
    }
}

/// The streamer's command for a loop `setting` on `channel_index`, meant for
/// `stream` when the sender knows it (see `StreamerCommand::SetStreamLoop`).
fn loop_command(
    channel_index: usize,
    stream: Option<RegionId>,
    setting: LoopSetting,
) -> StreamerCommand {
    match setting {
        LoopSetting::On {
            start,
            end,
            crossfade_frames,
        } => StreamerCommand::SetStreamLoop {
            channel_index,
            stream,
            range: (start.get().max(0.0) as u64, end.get().max(0.0) as u64),
            crossfade_frames,
        },
        LoopSetting::Off => StreamerCommand::ClearStreamLoop {
            channel_index,
            stream,
        },
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
    /// dead streamer silently accepting `Stream`, `Seek`, `Loop` and `Stop`
    /// forever is indistinguishable from working playback.
    #[must_use = "a discarded command is a stream that never starts, seeks, or stops"]
    pub fn send(&self, cmd: Command) -> Result<(), StreamerGone> {
        self.tx.send(cmd.into_streamer()).map_err(|_| StreamerGone)
    }

    /// Sets or clears the loop of the stream `stream` on `channel_index`:
    /// [`send`](Self::send)'s `Command::Loop`, which the streamer ignores once
    /// the channel carries another stream (the voice's stopped, or the
    /// channel restarted on another file). Control thread; waits as `send`
    /// does.
    pub(crate) fn send_stream_loop(
        &self,
        channel_index: usize,
        stream: RegionId,
        setting: LoopSetting,
    ) -> Result<(), StreamerGone> {
        self.tx
            .send(loop_command(channel_index, Some(stream), setting))
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

/// READ port onto the streaming engine: a cloneable snapshot of the sample rate
/// and the channel-plan map, exposing state reads plus the reader-factory.
#[derive(Clone)]
pub struct Status {
    sample_rate: SessionRate,
    plans: Arc<DashMap<usize, ChannelPlan>>,
}

// `ChannelPlan` isn't `Debug` (it holds streamer-internal cache/link state), so
// hand-roll a summary rather than deriving — never touch the map's contents to
// avoid contending with the streamer thread.
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

    /// Builds a [`DiskVoice`] for a channel whose streamer stream is ready, placed
    /// on the timeline at `start_beat` for `duration` (the transport it reads
    /// is its block's, from the graph).
    ///
    /// Pulls the ring consumer and shared RT state out of the channel's plan and
    /// wraps them in a placement-gated reader. `start_beat` and `duration` are
    /// musical time — the gate converts them to file frames using the file's
    /// own rate, not the session's: the rate the streamer recorded when it opened
    /// the stream, so a session rate that moves later (a device restart) leaves
    /// the gate's frames where they were.
    ///
    /// The voice also keeps a read-only handle onto the streamer's record of
    /// the stream, so a fork of it for an offline render (an export) can read
    /// the same file on its own rather than through the live ring; see
    /// `DiskVoice::fork_copy`.
    ///
    /// Control thread; the returned voice is then moved into the graph.
    ///
    /// # Errors
    ///
    /// [`TakeVoiceError::NotStreaming`] while the streamer has not opened the
    /// stream yet: a caller polls again next frame rather than treating it as
    /// a failure. [`TakeVoiceError::ReaderTaken`] once a voice has been taken
    /// from the stream: a stream serves one live voice. The voice is as wide
    /// as the file ([`voice_width`](crate::voice_width)) and plays its
    /// channels where they are, unless the file's order is not the engine's
    /// (a WAV 7.1): that is permuted to it. A file with a channel no speaker
    /// names (a second LFE, a wide or high speaker) is
    /// [`TakeVoiceError::Place`].
    pub fn take_disk_voice(
        &self,
        channel_index: usize,
        start_beat: Beat,
        duration: Option<BeatDuration>,
    ) -> Result<DiskVoice, TakeVoiceError> {
        self.take_placed_disk_voice(channel_index, None, start_beat, duration)
    }

    /// [`take_disk_voice`](Self::take_disk_voice) for a voice `width` channels
    /// wide: the file's channels are placed into it by their
    /// [`ChannelTopology`](tutti_types::ChannelTopology), as
    /// [`memory_voice`](crate::memory_voice) places a wave. A file in another
    /// order than the engine's (a WAV 7.1 lists the rears before the sides) is
    /// permuted to it, one wider than `width` with an order is folded into it,
    /// and a mono one takes the centre of a wider frame.
    ///
    /// The placement is built here, on the control side, before the stream's
    /// reader is taken; the audio thread only applies it.
    ///
    /// # Errors
    ///
    /// As [`take_disk_voice`](Self::take_disk_voice), and
    /// [`TakeVoiceError::Place`] when the file cannot be placed into `width`:
    /// a channel no speaker names (a second LFE, a wide or high speaker), or a
    /// narrowing with an unordered end (a 5.1 file into 5 channels, 10 into
    /// 8). The stream keeps its reader, so a caller may take again at another
    /// width.
    pub fn take_disk_voice_into(
        &self,
        channel_index: usize,
        width: ChannelLayout,
        start_beat: Beat,
        duration: Option<BeatDuration>,
    ) -> Result<DiskVoice, TakeVoiceError> {
        self.take_placed_disk_voice(channel_index, Some(width), start_beat, duration)
    }

    fn take_placed_disk_voice(
        &self,
        channel_index: usize,
        width: Option<ChannelLayout>,
        start_beat: Beat,
        duration: Option<BeatDuration>,
    ) -> Result<DiskVoice, TakeVoiceError> {
        let (inner, rt_state, file_sample_rate, origin) =
            crate::streamer::control::take_streaming_unit(&self.plans, channel_index, width, true)?;

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

    /// The loop the stream on `channel_index` runs, as the streamer last set
    /// it; `None` when the channel streams nothing (tests).
    #[cfg(test)]
    pub(crate) fn stream_loop(&self, channel_index: usize) -> Option<LoopSetting> {
        crate::streamer::control::stream_loop(&self.plans, channel_index)
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
            Arc<crate::streamer::RtState>,
        ),
        TakeVoiceError,
    > {
        let (unit, rt_state, _, _) =
            crate::streamer::control::take_streaming_unit(&self.plans, channel_index, None, false)?;
        Ok((unit, rt_state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// `Stream`, `Seek`, `Loop` or `Stop` indefinitely and do nothing —
    /// indistinguishable from working playback, and invisible to a test that
    /// only exercised its neighbours.
    #[test]
    fn every_command_reports_a_dead_streamer() {
        let cmds = dead_streamer();

        assert_eq!(
            cmds.send(Command::Stream {
                channel_index: 0,
                file_path: "/nonexistent.wav".into(),
                offset: SamplePosition(0.0),
            }),
            Err(StreamerGone)
        );
        assert_eq!(
            cmds.send(Command::Seek {
                channel_index: 0,
                file_position: SamplePosition(0.0),
            }),
            Err(StreamerGone)
        );
        assert_eq!(
            cmds.send(Command::SetSpeed {
                channel_index: 0,
                speed: PlaybackRate::new(1.0),
                direction: Direction::Forward,
            }),
            Err(StreamerGone)
        );
        assert_eq!(
            cmds.send(Command::Loop {
                channel_index: 0,
                setting: LoopSetting::Off,
            }),
            Err(StreamerGone),
            "the Loop/Off arm maps to its own streamer command"
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
            Err(StreamerGone),
            "the Loop/On arm is a separate send from Loop/Off"
        );
        assert_eq!(
            cmds.send(Command::Stop { channel_index: 0 }),
            Err(StreamerGone)
        );
    }

    /// A batch reports how many landed rather than vanishing wholesale.
    #[test]
    fn send_all_reports_nothing_queued_to_a_dead_streamer() {
        let cmds = dead_streamer();
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
        let (tx, queue) = crossbeam_channel::bounded(8);
        let cmds = Commands::new(tx);

        let queued = cmds.send_all([
            Command::Stop { channel_index: 0 },
            Command::Stop { channel_index: 1 },
        ]);

        assert_eq!(queued, 2);
        assert_eq!(queue.len(), 2, "both reached the queue");
    }
}
