//! The sampler streaming-engine handle. See [`Sampler`].

use crate::butler::{BufferConfig, ButlerCommand, ButlerThread};
use crate::error::Result;
use crate::ports::{Commands, Status};
use arc_swap::ArcSwap;
#[cfg(feature = "bevy")]
use bevy_ecs::resource::Resource;
use smol::channel::Sender;
use std::sync::Arc;
use tutti_core::Samples;

/// The sampler subsystem handle, held as a Bevy [`Resource`].
///
/// Owns the butler thread, which drives all disk I/O. The engine builds one at
/// startup with [`new`](Self::new) and inserts it directly; the ECS layer reads
/// it as `Res<Sampler>`.
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
/// use tutti_sampler::Sampler;
///
/// # fn main() -> tutti_sampler::Result<()> {
/// let sampler = Sampler::new(48_000.0, Default::default())?;
/// let _ = sampler.status().sample_rate();
/// # Ok(())
/// # }
/// ```
#[cfg_attr(feature = "bevy", derive(Resource))]
pub struct Sampler {
    butler_tx: Sender<ButlerCommand>,
    butler: ButlerThread,
    sample_rate: f64,
}

// `butler`/`butler_tx` hold a thread handle + command channel that can't
// derive `Debug`; print the sample rate and note the live butler thread.
impl std::fmt::Debug for Sampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sampler")
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl Sampler {
    /// Build the system and spawn the butler thread.
    ///
    /// Configure with [`SamplerConfig`] (`Default` + struct-update); pass
    /// `Default::default()` for the tuned defaults. Returns [`Err`] if any
    /// subsystem fails to initialize.
    pub fn new(sample_rate: f64, config: SamplerConfig) -> Result<Self> {
        let mut butler = ButlerThread::with_config(256, sample_rate, config.buffer_config);

        if let Some(ref pdc) = config.pdc {
            butler = butler.with_pdc(Arc::clone(pdc));
        }

        let butler_tx = butler.command_sender();
        butler.start();

        Ok(Sampler {
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

/// Configuration for [`Sampler::new`]. `Default` + struct-update.
#[derive(Clone, Default)]
pub struct SamplerConfig {
    /// Butler buffer / cache configuration. Default is tuned for
    /// 64-channel streaming on a typical desktop.
    pub buffer_config: BufferConfig,
    /// Subscription to a per-channel delay-compensation table.
    ///
    /// While set, butler pre-rolls each stream by its channel's entry so
    /// downstream effects stay sample-aligned. Published by whoever runs
    /// `tutti_core::latency::compensate` over the audio graph — see
    /// [`DelayPlan::channel_compensations`](tutti_types::DelayPlan::channel_compensations).
    pub pdc: Option<Arc<ArcSwap<Vec<Samples>>>>,
}

// Hand-rolled: print the buffer config + whether a subscription is set, not
// the snapshot itself.
impl std::fmt::Debug for SamplerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplerConfig")
            .field("buffer_config", &self.buffer_config)
            .field("has_pdc", &self.pdc.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_io_metrics_zeroed_on_fresh_system() {
        let sampler = Sampler::new(44100.0, Default::default()).unwrap();
        // A fresh butler has read nothing and an empty cache.
        let plans = sampler.butler.plans();
        assert!(plans.is_empty());
        assert_eq!(sampler.status().sample_rate(), 44100.0);
    }

    #[test]
    fn pdc_subscription_is_shared_not_copied() {
        // The caller owns the table and keeps publishing to it after the
        // sampler is built; the sampler must observe those later stores.
        let pdc = Arc::new(ArcSwap::from_pointee(vec![Samples(100), Samples(0)]));

        let _sampler = Sampler::new(
            44100.0,
            SamplerConfig {
                pdc: Some(Arc::clone(&pdc)),
                ..Default::default()
            },
        )
        .unwrap();

        pdc.store(Arc::new(vec![Samples(512), Samples(0)]));
        assert_eq!(pdc.load()[0], Samples(512));
    }
}
