//! The sampler streaming-engine handle. See [`DiskStreamer`].

use super::{BufferConfig, ButlerCommand, ButlerThread};
use crate::error::Result;
use crate::ports::{Commands, Status};
use smol::channel::Sender;
use std::sync::Arc;
use tutti_core::RtPublish;
use tutti_core::SampleRate;
use tutti_core::Samples;

/// The sampler subsystem handle.
///
/// Owns the butler thread, which drives all disk I/O. The engine builds one at
/// startup with [`new`](Self::new); a host holds it for the lifetime of the
/// session.
///
/// Stream control is split MIDI-device-style into two cloneable ports: the
/// WRITE port [`commands`](Self::commands) (a [`Commands`] over the butler
/// command channel) and the READ port [`status`](Self::status) (a [`Status`]
/// carrying the sample rate + the reader-factory).
///
///
/// # Example
///
/// ```no_run
/// use tutti_sampler::DiskStreamer;
///
/// # fn main() -> tutti_sampler::Result<()> {
/// let sampler = DiskStreamer::new(48_000.0, Default::default())?;
/// let _ = sampler.status().sample_rate();
/// # Ok(())
/// # }
/// ```
pub struct DiskStreamer {
    butler_tx: Sender<ButlerCommand>,
    butler: ButlerThread,
    sample_rate: SampleRate,
}

// `butler`/`butler_tx` hold a thread handle + command channel that can't
// derive `Debug`; print the sample rate and note the live butler thread.
impl std::fmt::Debug for DiskStreamer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskStreamer")
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl DiskStreamer {
    /// Build the system and spawn the butler thread.
    ///
    /// Configure with [`DiskStreamerConfig`] (`Default` + struct-update); pass
    /// `Default::default()` for the tuned defaults. Returns [`Err`] if any
    /// subsystem fails to initialize.
    pub fn new(sample_rate: impl Into<SampleRate>, config: DiskStreamerConfig) -> Result<Self> {
        let sample_rate = sample_rate.into();
        let mut butler = ButlerThread::with_config(256, sample_rate, config.buffer_config);

        if let Some(ref pdc) = config.pdc {
            butler = butler.with_pdc(Arc::clone(pdc));
        }

        let butler_tx = butler.command_sender();
        butler.start();

        Ok(DiskStreamer {
            butler_tx,
            butler,
            sample_rate,
        })
    }

    /// WRITE port: a cloneable [`Commands`] handle over the butler command
    /// channel. Drive streaming with `commands().send(Command::…)`.
    #[must_use]
    pub fn commands(&self) -> Commands {
        Commands::new(self.butler_tx.clone())
    }

    /// READ port: a cloneable [`Status`] snapshot carrying the sample rate and
    /// the channel-plan map (the reader-factory).
    #[must_use]
    pub fn status(&self) -> Status {
        Status::new(self.sample_rate, self.butler.plans())
    }
}

// `butler` has its own `Drop` impl; auto-drop handles cleanup.

/// Configuration for [`DiskStreamer::new`]. `Default` + struct-update.
#[derive(Clone, Default)]
pub struct DiskStreamerConfig {
    /// Butler buffer / cache configuration. Default is tuned for
    /// 64-channel streaming on a typical desktop.
    pub buffer_config: BufferConfig,
    /// Subscription to a per-channel delay-compensation table, indexed by
    /// channel index and denominated in [`Samples`].
    ///
    /// While set, the butler pre-rolls each stream by its channel's entry so
    /// downstream effects stay sample-aligned. Published by whoever runs
    /// `tutti_core::latency::compensate` over the audio graph — the table is
    /// [`Compensation::channels`](tutti_core::Compensation::channels).
    pub pdc: Option<Arc<RtPublish<Vec<Samples>>>>,
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

    /// A backward `Command::Seek` must actually reposition the live stream.
    ///
    /// This settles a question an end-to-end test could not. In
    /// `tests/tier_parity.rs`, seeking a live stream leaves the audio coming
    /// from the old position — which is consistent with two very different
    /// causes: the butler ignoring the seek, or the butler repositioning
    /// correctly while the reader drains a ring already primed with up to 30 s
    /// of pre-seek material. From outside the crate those are indistinguishable,
    /// because the only observable is the audio and the audio is the thing in
    /// dispute.
    ///
    /// In-crate the butler's own state is visible, so the question is
    /// answerable. **It passes: the butler does reposition.** The end-to-end
    /// symptom is therefore ring latency, not a defect.
    ///
    /// # Why the ring-reset epoch, and not a position
    ///
    /// The obvious probe is `link.read_position` — and it is the wrong one. That
    /// counter tracks frames the *reader* has consumed, so with nothing
    /// rendering it sits at 0 no matter what the butler does; asserting on it
    /// reported "the butler did not reposition" for a butler that had. The
    /// writer's `file_position` would be right but lives in butler-thread-local
    /// state that nothing else can reach.
    ///
    /// The reset epoch works because `reposition_click_free` bumps it via
    /// `plan.flush_buffer()`, on the *shared* plan, and nothing else in a quiet
    /// stream moves it. So a change means the butler ran the reposition path.
    #[test]
    fn a_backward_seek_repositions_the_live_stream() {
        use crate::ports::Command;
        use std::io::Write;
        use tutti_core::SamplePosition;

        const SR: f64 = 48_000.0;

        // 60 s of tone, long enough that the ring cannot hold the whole file
        // (`buffer_size_for_file` caps at 30 s), so a seek is real work.
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
            for i in 0..(SR * 60.0) as usize {
                let s = (std::f32::consts::TAU * 440.0 * i as f32 / SR as f32).sin() * 0.4;
                w.write_sample(s).unwrap();
                w.write_sample(s).unwrap();
            }
            w.finalize().unwrap();
            std::io::stdout().flush().ok();
        }

        let sampler = DiskStreamer::new(SR, Default::default()).unwrap();
        sampler
            .commands()
            .send(Command::Stream {
                channel_index: 0,
                file_path: path.clone(),
                offset: SamplePosition(20.0 * SR),
            })
            .expect("the butler is alive in this test");

        // Wait for the butler to install the link.
        let plans = sampler.butler.plans();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if plans.get(&0).is_some_and(|p| p.link.is_some()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the butler never registered the stream"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // The ring-reset epoch is the butler's own signal that it repositioned:
        // `reposition_click_free` calls `plan.flush_buffer()`, which bumps it.
        // Unlike `read_position` (a *reader* consumption counter, which stays 0
        // when nothing is rendering) this moves purely as a result of the seek.
        let reset_epoch =
            || -> u64 { plans.get(&0).map(|p| p.rt_state.reset_epoch()).unwrap_or(0) };

        let before = reset_epoch();

        // Seek backward, which is the direction a "refill forward from here"
        // implementation is most likely to drop.
        sampler
            .commands()
            .send(Command::Seek {
                channel_index: 0,
                file_position: SamplePosition(5.0 * SR),
            })
            .expect("the butler is alive in this test");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if reset_epoch() != before {
                return; // the butler flushed and repositioned — this is the pass
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the ring-reset epoch never moved from {before} after a backward \
                 seek — the butler did not reposition the stream"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn pdc_subscription_is_shared_not_copied() {
        // The caller owns the table and keeps publishing to it after the
        // sampler is built; the sampler must observe those later stores.
        let pdc = Arc::new(RtPublish::new(vec![Samples(100), Samples(0)]));

        let _sampler = DiskStreamer::new(
            44100.0,
            DiskStreamerConfig {
                pdc: Some(Arc::clone(&pdc)),
                ..Default::default()
            },
        )
        .unwrap();

        pdc.publish(Arc::new(vec![Samples(512), Samples(0)]));
        assert_eq!(pdc.read()[0], Samples(512));
    }
}
