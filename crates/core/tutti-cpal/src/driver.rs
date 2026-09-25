//! `TuttiDriver` — CPAL audio I/O lifecycle.
//!
//! Owns the CPAL stream and the [`AudioCallbackState`] that feeds it. A host
//! builds one at startup and may call [`set_device`](TuttiDriver::set_device) /
//! [`restart`](TuttiDriver::restart) to switch device without rebuilding the
//! graph. A device that comes back at another rate needs the graph re-rated
//! with it, which this driver cannot do (it holds only the graph's audio
//! side): [`restart_with`](TuttiDriver::restart_with) runs the host's hook for
//! that between the stop and the start, and a plain `restart` refuses the
//! rate change rather than play the graph at the old rate.
//!
//! `&mut self` lifecycle — no `Mutex`. Hold it in one place. The `cpal::Stream`
//! it owns is `Send` but not `Sync`, so a host that stores it in a shared
//! context must pin it to one thread.

use std::sync::Arc;

use crate::output::{AudioCallbackState, AudioEngine};
use crate::Result;

/// One enumerated audio output device.
///
/// Returned from [`TuttiDriver::devices`]. `index` is the value to pass to
/// [`TuttiDriver::set_device`] or [`TuttiDriver::restart`].
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Position in the host's output-device enumeration — the value
    /// [`TuttiDriver::set_device`] and [`TuttiDriver::restart`] take. Positional,
    /// so it is only valid against the enumeration that produced it: devices
    /// appearing or disappearing renumber the rest.
    pub index: usize,
    /// The device's human-readable name, as the OS reports it. Empty if the
    /// host failed to name it.
    pub name: String,
}

/// The engine while its stream is stopped: handed to a restart hook
/// ([`TuttiDriver::restart_with`]), and constructible nowhere else, so what it
/// reaches of the audio thread's state is reached only while no callback can
/// run.
pub struct Stopped<'a> {
    state: &'a AudioCallbackState,
}

impl Stopped<'_> {
    /// Install every commit the graph's editor has sent, following a
    /// re-prepare's rate with the engine's clock
    /// ([`Engine::settle_graph`](tutti_core::Engine::settle_graph)). Returns
    /// whether the graph runs a plan with no re-prepare between its halves.
    ///
    /// A host drives a re-prepare's two halves through it — settle (the
    /// executor checks its units out), let the editor collect and resume,
    /// settle again (the executor adopts the resumed plan) — so the first
    /// block at the new rate renders the re-prepared graph instead of the
    /// silent block a re-prepare otherwise costs.
    pub fn settle_graph(&self) -> bool {
        self.state.engine.settle_graph()
    }
}

/// Owns the CPAL stream and drives the audio thread.
pub struct TuttiDriver {
    audio_engine: AudioEngine,
    callback_state: Arc<AudioCallbackState>,
    /// The rate the graph runs at: the device's at build, then only what a
    /// restart hook returned `Ok` for. Kept apart from the spec, which a
    /// restart resolves to the new device's config before its hook runs —
    /// comparing against that would let a second plain `restart` onto the
    /// same new-rate device find "no change" and start over the old graph.
    graph_rate: tutti_core::SampleRate,
}

// Hand-rolled: `AudioCallbackState` holds the RT-side graph handles and is not
// `Debug`. Forwards the engine, which carries the configuration a host logs.
impl std::fmt::Debug for TuttiDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuttiDriver")
            .field("audio_engine", &self.audio_engine)
            .finish_non_exhaustive()
    }
}

impl TuttiDriver {
    /// Construct from an opened device and the state its callback will read.
    pub fn from_parts(audio_engine: AudioEngine, callback_state: Arc<AudioCallbackState>) -> Self {
        Self {
            graph_rate: audio_engine.sample_rate(),
            audio_engine,
            callback_state,
        }
    }

    /// The rate the graph runs at, as far as this driver knows: the device's
    /// when it was built, then the rate of the last restart whose hook
    /// returned `Ok` (a plain [`restart`](Self::restart) keeps it, since it
    /// refuses any other). What a plain `restart` compares a device against.
    pub fn graph_rate(&self) -> tutti_core::SampleRate {
        self.graph_rate
    }

    /// Is the audio stream currently running?
    pub fn is_running(&self) -> bool {
        self.audio_engine.is_running()
    }

    /// Name of the currently selected output device.
    pub fn device_name(&self) -> Result<String> {
        self.audio_engine.device_name()
    }

    /// Select a different output device. Takes effect on next [`restart`].
    ///
    /// [`restart`]: Self::restart
    pub fn set_device(&mut self, index: Option<usize>) -> &mut Self {
        self.audio_engine.set_device(index);
        self
    }

    /// Restart the audio stream on a (possibly different) output device,
    /// which must run at the rate the graph was built at.
    ///
    /// Stops the current stream, resets RT processor owner thread-IDs, reads
    /// `device_index`'s config (the default device if `None`) and starts
    /// fresh on it.
    ///
    /// # Errors
    ///
    /// [`Error::RateChanged`](crate::Error::RateChanged) when the device now
    /// runs at another rate, **with the stream left stopped**: this driver
    /// holds the graph's audio side only, so it can re-rate nothing, and a
    /// graph left at the old rate plays every node off pitch and off tempo
    /// without a word. Restart through
    /// [`restart_with`](Self::restart_with) to re-rate the graph between
    /// the stop and the start. Otherwise what [`AudioEngine::start`] reports.
    pub fn restart(&mut self, device_index: Option<usize>) -> Result<()> {
        self.restart_with(device_index, refuse_rate_change(self.graph_rate))
    }

    /// [`restart`](Self::restart), with `rerate` run between the stop and the
    /// start, handed the config the new stream will run at.
    ///
    /// The hook is where a host re-rates what the driver does not own — the
    /// graph's control side, the transport's rate, the device config it
    /// publishes — while no callback runs: the stream starts only once it has
    /// returned `Ok`, so the first block at the new rate renders the
    /// re-rated graph. A hook that fails leaves the stream **stopped** and
    /// its error returned: nothing plays at a rate the graph was not moved
    /// to. Its `Ok` is the claim that the graph now runs at the spec's rate
    /// ([`graph_rate`](Self::graph_rate)); after a failure the driver keeps
    /// the old rate and the old [`spec`](Self::spec), so a later restart
    /// onto the old device and rate (a plain `restart` of it) recovers.
    ///
    /// The hook is also handed a [`Stopped`]: the engine, reachable from the
    /// control side for as long as the hook runs and no callback can, so the
    /// host can finish a graph re-prepare before the first block
    /// ([`Stopped::settle_graph`]).
    ///
    /// # Errors
    ///
    /// What resolving or opening the device reports (as `E`), or the hook's
    /// own error.
    pub fn restart_with<E: From<crate::Error>>(
        &mut self,
        device_index: Option<usize>,
        rerate: impl FnOnce(&crate::OutputSpec, &Stopped<'_>) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), E> {
        self.audio_engine.stop();
        self.callback_state.reset_owners();
        self.audio_engine.set_device(device_index);
        let old = self.audio_engine.spec().clone();
        let device = self.audio_engine.resolve()?;
        self.rerate_or_restore(old, rerate)?;
        self.audio_engine.start_with(
            self.callback_state.clone(),
            crate::CpalDriver::from_device(device),
        )?;
        Ok(())
    }

    /// [`restart_with`](Self::restart_with) on a driver of the caller's
    /// choosing, at the caller's `spec` — the device-free restart, as
    /// [`start_with`](Self::start_with) is the device-free start. A host (or
    /// a test) runs the shipped restart lifecycle over a
    /// [`ManualStreamDriver`](crate::ManualStreamDriver): `spec` stands for
    /// the config the new device reports.
    ///
    /// # Errors
    ///
    /// The hook's error (the stream left stopped), or what the driver's
    /// `open` reports.
    pub fn restart_on<D: crate::StreamDriver, E: From<crate::Error>>(
        &mut self,
        spec: crate::OutputSpec,
        driver: D,
        rerate: impl FnOnce(&crate::OutputSpec, &Stopped<'_>) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), E>
    where
        D::Running: 'static,
    {
        self.audio_engine.stop();
        self.callback_state.reset_owners();
        let old = self.audio_engine.spec().clone();
        self.audio_engine.set_spec(spec);
        self.rerate_or_restore(old, rerate)?;
        self.audio_engine
            .start_with(self.callback_state.clone(), driver)?;
        Ok(())
    }

    /// Run a restart's hook against the resolved spec. `Ok` adopts its rate
    /// as the graph's; an error puts `old` back, so neither the spec nor the
    /// graph rate claims a rate the graph was not moved to.
    fn rerate_or_restore<E>(
        &mut self,
        old: crate::OutputSpec,
        rerate: impl FnOnce(&crate::OutputSpec, &Stopped<'_>) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), E> {
        let stopped = Stopped {
            state: &self.callback_state,
        };
        match rerate(self.audio_engine.spec(), &stopped) {
            Ok(()) => {
                self.graph_rate = self.audio_engine.spec().sample_rate;
                Ok(())
            }
            Err(e) => {
                self.audio_engine.set_spec(old);
                Err(e)
            }
        }
    }

    /// The configuration of the stream that is playing, or that would be:
    /// the rate and width the last start or restart resolved.
    pub fn spec(&self) -> &crate::OutputSpec {
        self.audio_engine.spec()
    }

    /// Select a device by name or index. Takes effect on the next
    /// [`restart`](Self::restart).
    ///
    /// Prefer a name: `DeviceInfo::index` is positional within one
    /// enumeration, so any hot-plug renumbers it, and cpal 0.15 offers no
    /// device-change notification to re-enumerate on.
    pub fn select_device(&mut self, sel: crate::DeviceSelector) -> &mut Self {
        self.audio_engine.select_device(sel);
        self
    }

    /// The backend fault sink, which survives stop and restart.
    ///
    /// Take it once at startup. CPAL's error callback returns nothing, so a
    /// device unplugged mid-session has nowhere else to surface — before this
    /// existed it surfaced *nowhere*, and [`is_running`](Self::is_running)
    /// went on reporting a healthy stream.
    pub fn faults(&self) -> std::sync::Arc<crate::StreamFaults> {
        self.audio_engine.faults()
    }

    /// The most recent backend fault, clearing it. For a host that shows each
    /// one once.
    pub fn take_fault(&self) -> Option<crate::StreamFault> {
        self.audio_engine.faults().take_last()
    }

    /// Start on a driver of the caller's choosing.
    ///
    /// The production path is [`restart`](Self::restart), which builds a real
    /// CPAL stream. This exists so a host — or a test — can run the same
    /// lifecycle over a
    /// [`ManualStreamDriver`](crate::ManualStreamDriver) with no device open.
    pub fn start_with<D: crate::StreamDriver>(&mut self, driver: D) -> Result<()>
    where
        D::Running: 'static,
    {
        self.audio_engine.stop();
        self.callback_state.reset_owners();
        self.audio_engine
            .start_with(self.callback_state.clone(), driver)
    }

    /// Stop the stream. Idempotent.
    pub fn stop(&mut self) {
        self.audio_engine.stop();
    }

    /// Enumerate output devices as [`DeviceInfo`] records.
    pub fn devices() -> Result<impl Iterator<Item = DeviceInfo>> {
        Ok(AudioEngine::output_devices()?.map(|(index, name)| DeviceInfo { index, name }))
    }
}

/// The hook a plain [`TuttiDriver::restart`] runs: the new device must run
/// at `graph`, the rate the graph was built at, or the restart stops there
/// with [`Error::RateChanged`](crate::Error::RateChanged).
fn refuse_rate_change(
    graph: tutti_core::SampleRate,
) -> impl FnOnce(&crate::OutputSpec, &Stopped<'_>) -> Result<()> {
    move |spec, _| {
        if spec.sample_rate == graph {
            Ok(())
        } else {
            Err(crate::Error::RateChanged {
                device: spec.sample_rate,
                graph,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioEngine, ManualStreamDriver, OutputSpec};
    use tutti_core::dsp::Net;
    use tutti_core::{AudioTap, ChannelLayout, Engine, MasterMeter, SampleRate, Transport};

    fn spec_at(rate: f64) -> OutputSpec {
        OutputSpec::new(
            SampleRate(rate),
            ChannelLayout::STEREO,
            cpal::SampleFormat::F32,
        )
    }

    /// **A plain restart refuses a device at another rate, and leaves the
    /// stream stopped** — its hook, run through the device-free restart
    /// (`restart` itself needs a device to resolve). At the same rate it
    /// starts.
    ///
    /// Mutations (run): `refuse_rate_change` answering `Ok` regardless → the
    /// 48 kHz restart starts the stream over the 44.1 kHz graph → fails;
    /// `rerate_or_restore` adopting the rate (or keeping the new spec) on
    /// failure → the second attempt compares 48 kHz with 48 kHz and starts
    /// → fails. (Checking against `spec().sample_rate` instead of
    /// `graph_rate` is equivalent while a failure restores the spec; it
    /// fails here only together with the restore removed.)
    #[test]
    fn a_plain_restart_refuses_a_new_rate_and_stays_stopped() {
        let transport = Transport::new(44_100.0);
        let mut net = Net::new(0, 2);
        let engine = Engine::new(transport.motion.clone(), net.backend());
        let state = Arc::new(AudioCallbackState::new(
            engine,
            MasterMeter::new(),
            AudioTap::new(),
        ));
        let mut driver = TuttiDriver::from_parts(AudioEngine::from_spec(spec_at(44_100.0)), state);
        let graph = SampleRate(44_100.0);

        // Twice onto the same 48 kHz device: the second attempt must not
        // find the first one's resolved spec and call it "no change".
        for attempt in 0..2 {
            let (d, stream) = ManualStreamDriver::new();
            let err = driver
                .restart_on(
                    spec_at(48_000.0),
                    d,
                    refuse_rate_change(driver.graph_rate()),
                )
                .expect_err("a new rate is refused");
            assert!(
                matches!(
                    err,
                    crate::Error::RateChanged { device, graph: g }
                        if device == SampleRate(48_000.0) && g == graph
                ),
                "attempt {attempt}: {err}"
            );
            assert!(!driver.is_running(), "nothing plays at the wrong rate");
            assert!(!stream.is_open());
            assert_eq!(driver.graph_rate(), graph, "the graph did not move");
            assert_eq!(
                driver.spec().sample_rate,
                graph,
                "the spec does not claim the refused rate"
            );
        }

        let (d, stream) = ManualStreamDriver::new();
        driver
            .restart_on(spec_at(44_100.0), d, refuse_rate_change(graph))
            .expect("the same rate restarts");
        assert!(driver.is_running() && stream.is_open());
        drop(net);
    }
}
