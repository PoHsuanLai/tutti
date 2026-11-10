//! Asynchronous disk I/O for audio file streaming.
//!
//! A background "streamer" thread decodes from disk into one SPSC ring per
//! channel; the audio thread only ever pops. Nothing here blocks or allocates on
//! the audio thread — the split is what makes the streaming tier real-time safe.

mod cache;
mod command;
mod config;
pub(crate) mod control;
mod handlers;
mod io;
mod loop_body;
mod loops;
mod metrics;
mod plan;
mod prefetch;
mod preroll;
mod region_map;
mod rt_state;
mod step;
mod thread;

pub(crate) use command::{RegionId, StreamerCommand};
pub(crate) use config::BufferConfig;
pub(crate) use handlers::SessionRate;
#[cfg(test)]
pub(crate) use loops::GUARD_FRAMES;
pub(crate) use loops::{Arrangement, RingMap};
pub(crate) use plan::ChannelPlan;
pub(crate) use prefetch::SharedReader;
pub(crate) use rt_state::RtState;
pub(crate) use thread::StreamerThread;

/// The sending end of the streamer's command queue: a bounded
/// `crossbeam_channel`, sent on by control threads only. A send waits while
/// the queue is full and fails once the streamer has gone (every receiver
/// dropped: the thread's on its exit, the controller's on `stop`).
pub(crate) type StreamerSender = crossbeam_channel::Sender<StreamerCommand>;

// The streamer's public face: the handle a host holds to drive disk streaming,
// and the one failure every stream-control command can report.
pub use control::StreamerGone;
pub use prefetch::TakeVoiceError;

// The hand-driven cycle's verdict. Public only alongside the driver that
// produces it — the threaded streamer consumes its own outcomes.
#[cfg(any(test, feature = "test-support"))]
pub use step::StepOutcome;

// Test-only re-exports for unit tests outside the streamer module tree (e.g.
// `units::disk_voice`) that build readers directly. Gated so they
// do not count as dead code in normal builds.
#[cfg(test)]
pub(crate) use prefetch::RegionBuffer;

use crate::error::Result;
use crate::ports::{Commands, Status};
use std::sync::Arc;
use tutti_graph::PrerollTable;
use tutti_types::SampleRate;

/// The disk-streaming engine: owns the streamer thread that reads files into
/// per-stream rings for [`DiskVoice`](crate::DiskVoice)s to play.
///
/// A host builds one at startup with [`new`](Self::new) and holds it for the
/// lifetime of the audio engine; dropping it stops the streamer thread. Stream
/// control is split into two cloneable ports: the WRITE port
/// [`commands`](Self::commands) (a [`Commands`] over the streamer's command
/// channel) and the READ port [`status`](Self::status) (a [`Status`] carrying
/// the sample rate and building the [`DiskVoice`](crate::DiskVoice) for a
/// stream).
///
/// All of this is control-thread API. The audio thread only ever touches the
/// `DiskVoice` a `Status` built, which reads its ring without locking or
/// allocating.
///
/// # Examples
///
/// Stream a file on channel 0 and build the voice that plays it:
///
/// ```no_run
/// use tutti_types::{Beat, SamplePosition};
/// use tutti_sampler::{Command, DiskStreamer, TakeVoiceError};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let streamer = DiskStreamer::new(48_000.0, Default::default())?;
///
/// streamer.commands().send(Command::Stream {
///     channel_index: 0,
///     file_path: "take_1.wav".into(),
///     offset: SamplePosition(0.0),
/// })?;
///
/// // The streamer opens the file on its own thread; poll until the stream is
/// // ready, then place the voice at beat 0 for the file's full length.
/// let voice = loop {
///     match streamer.status().take_disk_voice(0, Beat(0.0), None) {
///         Ok(voice) => break voice,
///         Err(TakeVoiceError::NotStreaming) => std::thread::yield_now(),
///         Err(e) => return Err(e.into()),
///     }
/// };
/// // `voice` is a graph node: insert it into the graph that renders the track.
/// # drop(voice);
/// # Ok(())
/// # }
/// ```
pub struct DiskStreamer {
    streamer_tx: StreamerSender,
    streamer: StreamerThread,
}

// `streamer`/`streamer_tx` hold a thread handle + command channel that can't
// derive `Debug`; print the sample rate and note the live streamer thread.
impl std::fmt::Debug for DiskStreamer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskStreamer")
            .field("sample_rate", &self.streamer.session_rate().get())
            .finish_non_exhaustive()
    }
}

impl DiskStreamer {
    /// Creates the streaming engine at `sample_rate` (the engine's rate) and
    /// spawns the streamer thread.
    ///
    /// Configure with [`DiskStreamerConfig`] (`Default` + struct-update); pass
    /// `Default::default()` for the tuned defaults. Allocates and spawns a
    /// thread: control thread only.
    ///
    /// # Errors
    ///
    /// None at present: every path returns `Ok`. The `Result` is the crate's
    /// [`Error`](crate::Error) type, for initialization that touches the
    /// filesystem.
    ///
    /// # Panics
    ///
    /// If the OS refuses to spawn the streamer thread.
    pub fn new(sample_rate: impl Into<SampleRate>, config: DiskStreamerConfig) -> Result<Self> {
        let mut streamer =
            StreamerThread::with_config(256, sample_rate.into(), config.buffer_config);

        if let Some(ref pdc) = config.pdc {
            streamer = streamer.with_pdc(Arc::clone(pdc));
        }

        let streamer_tx = streamer.command_sender();
        streamer.start();

        Ok(DiskStreamer {
            streamer_tx,
            streamer,
        })
    }

    /// Creates the streaming engine without spawning the streamer thread;
    /// cycles are then run by hand with [`step_once`](Self::step_once).
    /// Requires the `test-support` feature.
    ///
    /// The step is the same one the thread runs, so this is the shipped path
    /// with its pacing removed rather than a parallel implementation. What it
    /// buys is a streamer whose progress is *counted* instead of waited for: a
    /// test can say "after this many cycles the ring holds audio", where the
    /// threaded streamer only lets it say "after this long it probably does".
    ///
    /// # Errors
    ///
    /// None at present, as for [`new`](Self::new).
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_sampler::{DiskStreamer, DiskStreamerConfig};
    ///
    /// # fn main() -> tutti_sampler::Result<()> {
    /// let mut sampler = DiskStreamer::manual(48_000.0, DiskStreamerConfig::default())?;
    /// sampler.step_once();   // nothing queued: an idle cycle
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(any(test, feature = "test-support"))]
    pub fn manual(sample_rate: impl Into<SampleRate>, config: DiskStreamerConfig) -> Result<Self> {
        let mut streamer =
            StreamerThread::with_config(256, sample_rate.into(), config.buffer_config);

        if let Some(ref pdc) = config.pdc {
            streamer = streamer.with_pdc(Arc::clone(pdc));
        }

        let streamer_tx = streamer.command_sender();

        Ok(DiskStreamer {
            streamer_tx,
            streamer,
        })
    }

    /// Runs one streamer cycle on the calling thread, reporting whether the
    /// streamer still has urgent work ([`Busy`](crate::StepOutcome::Busy)) or has
    /// caught up ([`Healthy`](crate::StepOutcome::Healthy) /
    /// [`Idle`](crate::StepOutcome::Idle)).
    ///
    /// That verdict is what makes a step-driven test terminate on a *condition*
    /// rather than on a step budget: stepping until `Healthy` primes every ring
    /// exactly as far as the threaded streamer would before it parks.
    ///
    /// # Panics
    ///
    /// If this streamer owns a running streamer thread — i.e. it came from
    /// [`new`](Self::new) rather than [`manual`](Self::manual). The two drivers
    /// own the same streamer-local state and cannot share it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn step_once(&mut self) -> StepOutcome {
        self.streamer.step_once()
    }

    /// Steps until the streamer stops making progress, or `budget` cycles have run
    /// — whichever comes first. Returns the number of cycles taken.
    ///
    /// "Stops making progress" is any outcome other than
    /// [`Busy`](StepOutcome::Busy): the rings are full enough
    /// ([`Healthy`](StepOutcome::Healthy)), nothing is streaming
    /// ([`Idle`](StepOutcome::Idle)), or a ring below threshold is one no
    /// further cycle can grow ([`Stalled`](StepOutcome::Stalled), the
    /// normal state near the end of a file). Waiting for `Healthy` alone would
    /// never return there.
    ///
    /// The budget is a liveness bound, not a pacing knob: reaching it means the
    /// streamer is refilling forever without ever catching up, and a caller should
    /// treat that as the failure it is rather than proceeding to measure.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use = "an exhausted budget means the rings never primed"]
    pub fn step_until_settled(&mut self, budget: usize) -> usize {
        for taken in 0..budget {
            if self.step_once() != StepOutcome::Busy {
                return taken + 1;
            }
        }
        budget
    }

    /// WRITE port: a cloneable [`Commands`] handle over the streamer command
    /// channel. Drive streaming with `commands().send(Command::…)`.
    #[must_use]
    pub fn commands(&self) -> Commands {
        Commands::new(self.streamer_tx.clone())
    }

    /// READ port: a cloneable [`Status`] snapshot carrying the sample rate and
    /// the channel-plan map (the reader-factory).
    #[must_use]
    pub fn status(&self) -> Status {
        Status::new(self.streamer.session_rate(), self.streamer.plans())
    }

    /// Moves the session rate to `sample_rate`: what a device restart at a new
    /// rate does, between the stream's stop and its start.
    ///
    /// Every open stream keeps its file and its position (in file frames, which
    /// the rate does not touch) and has its conversion ratio re-derived against
    /// the new rate, so it plays on at the right pitch and speed from the next
    /// block; a stream opened later derives its ratio against the new rate too,
    /// and every [`Status`] — clones taken earlier included — reports it.
    /// `&self`: the rate is one shared cell and the ratios are atomics, so a
    /// host holding the streamer behind a shared reference can call it.
    ///
    /// Control thread only. Nothing changes for a disk voice already in the
    /// graph beyond its ratio: its placement gate converts beats to file frames
    /// at the file's own rate, which is not the session's.
    pub fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        self.streamer.set_session_rate(sample_rate.into());
    }
}

// `streamer` has its own `Drop` impl; auto-drop handles cleanup.

/// Configuration for [`DiskStreamer::new`]. `Default` + struct-update.
#[derive(Clone, Default)]
pub struct DiskStreamerConfig {
    /// Streamer buffer / cache configuration. Default is tuned for
    /// 64-channel streaming on a typical desktop.
    pub buffer_config: BufferConfig,
    /// Subscription to a per-channel delay-compensation table, indexed by
    /// channel index and denominated in [`Samples`](tutti_types::Samples).
    ///
    /// While set, the streamer pre-rolls each stream by its channel's entry so
    /// downstream effects stay sample-aligned. Published by whoever runs
    /// `tutti_types::latency::compensate` over the audio graph — the table is
    /// [`Compensation::channels`](tutti_types::Compensation::channels).
    pub pdc: Option<Arc<PrerollTable>>,
}

// Hand-rolled: print the buffer config + whether a subscription is set, not
// the snapshot itself.
impl std::fmt::Debug for DiskStreamerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskStreamerConfig")
            .field("buffer_config", &self.buffer_config)
            .field("has_pdc", &self.pdc.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::Samples;

    /// A backward `Command::Seek` must actually reposition the live stream.
    ///
    /// The end-to-end test (`tests/tier_parity.rs`) observes only the audio,
    /// where a streamer that ignored the seek and one that repositioned while
    /// the reader drained pre-seek material can look alike. In-crate the
    /// streamer's own state is visible, so this observes its reposition signal
    /// directly: a regression that repositioned late (rather than not at all)
    /// fails here first and unambiguously.
    ///
    /// # Why the ring's window
    ///
    /// The ring publishes the positions it holds (`Ring::window`), on the
    /// shared ring the plan's link holds, and nothing but a refill moves it.
    /// A seek moves where the reader plays (`Ring::set_play`), and the refill
    /// follows: the window must hold the target within the one cycle that
    /// applied the command.
    ///
    /// Mutation (run): the seek not moving the play position (`set_play`
    /// dropped from `handle_seek_stream`) → the window stays at 20 s → fails.
    ///
    /// # No thread, no timeout
    ///
    /// The streamer is stepped by hand, so "has the streamer seen the command yet"
    /// is answered by *having stepped* rather than by polling a 5 s deadline at
    /// 10 ms. A step returns only once the command is applied, so the assertions
    /// below are unconditional: there is no state in which the answer is "not
    /// yet".
    #[test]
    fn a_backward_seek_repositions_the_live_stream() {
        use crate::ports::Command;
        use tutti_types::SamplePosition;

        const SR: f64 = 48_000.0;

        // 31 s of tone. `buffer_size_for_file` caps the ring at 30 s and sizes
        // it to hold the whole file below that, so a shorter file is prefilled
        // entire and a "seek" moves the writer inside a ring that already holds
        // everything — nothing to reposition, and the epoch would not move.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seek_probe.wav");
        {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: SR as u32,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            for i in 0..(SR * 31.0) as usize {
                let s = (std::f32::consts::TAU * 440.0 * i as f32 / SR as f32).sin() * 0.4;
                w.write_sample(s).unwrap();
                w.write_sample(s).unwrap();
            }
            w.finalize().unwrap();
        }

        let mut sampler = DiskStreamer::manual(SR, Default::default()).unwrap();
        sampler
            .commands()
            .send(Command::Stream {
                channel_index: 0,
                file_path: path.clone(),
                offset: SamplePosition(20.0 * SR),
            })
            .expect("the streamer is alive in this test");

        // One step applies the queued `Stream`, which is what installs the link.
        let _ = sampler.step_once();

        let plans = sampler.streamer.plans();
        assert!(
            plans.get(&0).is_some_and(|p| p.link.is_some()),
            "one streamer cycle after a Stream command and no link is installed"
        );

        let window = || {
            plans
                .get(&0)
                .and_then(|p| p.link.as_ref().map(|l| l.consumer.window()))
                .unwrap_or_default()
        };
        let target = (5.0 * SR) as u64;
        let (from, _) = window();
        assert!(from > target, "the stream starts at 20 s, not holding 5 s");

        // Seek backward, which is the direction a "refill forward from here"
        // implementation is most likely to drop.
        sampler
            .commands()
            .send(Command::Seek {
                channel_index: 0,
                file_position: SamplePosition(5.0 * SR),
            })
            .expect("the streamer is alive in this test");

        let _ = sampler.step_once();

        let (from, to) = window();
        assert!(
            from <= target && target < to,
            "the ring holds [{from}, {to}) in the cycle that applied a backward seek to \
             {target} — the streamer did not reposition the stream"
        );
    }

    /// A loop change on a streamed file reads only the frames it needs:
    /// its fade's lead-in and a short loop's body, through the stream's own
    /// decoder — never the whole file, which blocked every other stream while
    /// it decoded and held the plan map. Observed through
    /// the cache the whole-file read filled: a hard loop, a crossfaded one and
    /// a cleared one leave it empty, and the loop still plays (the ring holds
    /// the loop's frames past its end).
    ///
    /// Mutation (run): the handler reading the file whole for the lead-in
    /// (`load_wave`) → the cache holds the file → fails.
    #[test]
    fn a_loop_change_reads_only_the_frames_it_needs() {
        use crate::ports::Command;
        use crate::voice::LoopSetting;
        use tutti_types::SamplePosition;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loop.wav");
        write_tone(&path, 48_000, 2.0);
        let mut sampler = DiskStreamer::manual(48_000.0, Default::default()).unwrap();
        let send = |sampler: &DiskStreamer, command| {
            sampler
                .commands()
                .send(command)
                .expect("the streamer is alive")
        };
        send(
            &sampler,
            Command::Stream {
                channel_index: 0,
                file_path: path.clone(),
                offset: SamplePosition(0.0),
            },
        );
        let _ = sampler.step_until_settled(1_000);
        for fade in [0usize, 500] {
            send(
                &sampler,
                Command::Loop {
                    channel_index: 0,
                    setting: LoopSetting::On {
                        start: SamplePosition(10_000.0),
                        end: SamplePosition(20_000.0),
                        crossfade_frames: fade,
                    },
                },
            );
            let _ = sampler.step_until_settled(1_000);
        }
        let plans = sampler.streamer.plans();
        let window = plans
            .get(&0)
            .and_then(|p| p.link.as_ref().map(|l| l.consumer.window()))
            .unwrap_or_default();
        assert!(
            window.1 > 40_000,
            "the loop plays on past its end: {window:?}"
        );
        assert!(
            sampler.streamer.cache().get(&path).is_none(),
            "a loop change decoded the whole file"
        );
    }

    /// Write `seconds` of a stereo 440 Hz tone at `rate` to `path`.
    fn write_tone(path: &std::path::Path, rate: u32, seconds: f64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(f64::from(rate) * seconds) as usize {
            let s = (std::f32::consts::TAU * 440.0 * i as f32 / rate as f32).sin() * 0.4;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    /// A session-rate change re-rates every stream, open or opened later.
    /// A streamer built at 44.1 kHz streams a 48 kHz file (ratio 48/44.1);
    /// then the device restarts at 48 kHz (`set_sample_rate`):
    ///
    /// - the open stream's ratio is re-derived, to unity — it plays on at the
    ///   right pitch and speed rather than 8.8% sharp;
    /// - a `Status` taken *before* the restart reports the new rate;
    /// - a disk voice taken after it still converts beats to file frames at
    ///   the file's 48 kHz (its seek targets stay where they were);
    /// - a 44.1 kHz file streamed after it gets 44.1/48, not unity.
    ///
    /// Mutations (run):
    /// - `SessionRate::set` not re-deriving the open streams → the first
    ///   ratio stays 48/44.1 → fails;
    /// - `SessionRate::set` not storing the rate → the early `Status` reports
    ///   44.1 kHz → fails;
    /// - `handle_stream_file` deriving against the build's 44.1 kHz (what the
    ///   old `StreamerCycle::sample_rate` copy did) → the later stream gets
    ///   unity → fails;
    /// - `status()` copying the rate into a fresh cell → the early `Status`
    ///   reports 44.1 kHz → fails.
    #[test]
    fn a_rate_change_re_derives_every_streams_ratio() {
        use crate::ports::Command;
        use tutti_types::{Beat, SamplePosition, SrcRatio};

        let dir = tempfile::tempdir().unwrap();
        let (at_48, at_44) = (dir.path().join("48k.wav"), dir.path().join("44k.wav"));
        write_tone(&at_48, 48_000, 1.0);
        write_tone(&at_44, 44_100, 1.0);

        let mut sampler = DiskStreamer::manual(44_100.0, Default::default()).unwrap();
        let early = sampler.status();
        let stream = |sampler: &mut DiskStreamer, channel_index, path: &std::path::Path| {
            sampler
                .commands()
                .send(Command::Stream {
                    channel_index,
                    file_path: path.to_path_buf(),
                    offset: SamplePosition(0.0),
                })
                .expect("the streamer is alive in this test");
            let _ = sampler.step_once();
        };
        let plans = sampler.streamer.plans();
        let ratio = |channel| plans.get(&channel).expect("streaming").rt_state.src_ratio();

        stream(&mut sampler, 0, &at_48);
        assert_eq!(ratio(0), SrcRatio::for_rates(48_000.0, 44_100.0));
        assert!(ratio(0) != SrcRatio::UNITY, "setup: a converting stream");

        sampler.set_sample_rate(48_000.0);
        assert_eq!(ratio(0), SrcRatio::UNITY, "the open stream is re-rated");
        assert_eq!(early.sample_rate(), SampleRate(48_000.0));

        let voice = sampler
            .status()
            .take_disk_voice(0, Beat(0.0), None)
            .expect("the link is installed");
        assert_eq!(voice.file_sample_rate(), SampleRate(48_000.0));

        stream(&mut sampler, 1, &at_44);
        assert_eq!(ratio(1), SrcRatio::for_rates(44_100.0, 48_000.0));
    }

    /// A stream that a new `Stream` on the same channel replaces is freed:
    /// its ring (and with it the writer's decoder and any resident wave) goes
    /// with the old link, as it does on `StopStreaming`.
    ///
    /// Mutation (run): `handle_stream_file` not removing the replaced
    /// stream's region from `local.regions` -> the old ring is still held by
    /// its writer -> fails.
    #[test]
    fn a_replaced_stream_is_freed() {
        use crate::ports::Command;
        use tutti_types::SamplePosition;

        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
        write_tone(&a, 48_000, 1.0);
        write_tone(&b, 48_000, 1.0);

        let mut sampler = DiskStreamer::manual(48_000.0, Default::default()).unwrap();
        let plans = sampler.streamer.plans();
        let mut stream = |path: &std::path::Path| {
            sampler
                .commands()
                .send(Command::Stream {
                    channel_index: 0,
                    file_path: path.to_path_buf(),
                    offset: SamplePosition(0.0),
                })
                .expect("the streamer is alive in this test");
            let _ = sampler.step_until_settled(1_000);
        };

        stream(&a);
        let old = Arc::downgrade(&plans.get(&0).unwrap().link.as_ref().unwrap().consumer);
        stream(&b);
        assert!(
            old.upgrade().is_none(),
            "the replaced stream's ring is still alive: its region leaked"
        );
    }

    /// Stepping and threading are exclusive owners of the streamer's local state.
    ///
    /// This is the one thing the hand driver can get wrong that the thread
    /// cannot: two `Local`s, two region maps, and rings one of them does not
    /// know exist. Cheaper to fail loudly at `start` than to debug a stream that
    /// refills into a ring nobody reads.
    #[test]
    #[should_panic(expected = "start on a hand-stepped streamer")]
    fn a_hand_stepped_streamer_refuses_to_also_start_a_thread() {
        let mut streamer =
            StreamerThread::with_config(4, SampleRate(48_000.0), BufferConfig::default());
        let _ = streamer.step_once();
        streamer.start();
    }

    /// The streamer reads the caller's table, not a copy of it: a figure
    /// the caller sets after the streamer is built reaches the streamer's
    /// cycle (`manual`), and the threaded streamer (`new`) hands its
    /// threads the caller's own table.
    ///
    /// Mutations (run): `DiskStreamer::manual` handing the streamer a copy of
    /// the table → the cycle still reads 100, fails; `DiskStreamer::new`
    /// handing a copy → not the same table, fails.
    #[test]
    fn pdc_subscription_is_shared_not_copied() {
        let pdc = Arc::new(PrerollTable::new(vec![Samples(100), Samples(0)]));
        let config = DiskStreamerConfig {
            pdc: Some(Arc::clone(&pdc)),
            ..Default::default()
        };

        let mut stepped = DiskStreamer::manual(44100.0, config.clone()).unwrap();
        let _ = stepped.step_once();
        assert_eq!(stepped.streamer.manual_preroll(0), Some(100));
        pdc.set(&[Samples(512), Samples(0)]);
        let _ = stepped.step_once();
        assert_eq!(stepped.streamer.manual_preroll(0), Some(512));

        let threaded = DiskStreamer::new(44100.0, config).unwrap();
        let held = threaded.streamer.pdc_table().expect("subscribed");
        assert!(Arc::ptr_eq(held, &pdc), "the caller's table, not a copy");
    }
}
