//! [`TuttiDriver`]: the CPAL stream lifecycle.
//!
//! Owns the [`AudioEngine`] and the [`AudioCallbackState`] it renders. A device
//! that comes back at another rate needs the graph re-rated, which this driver
//! cannot do (it holds only the graph's audio side): [`restart_with`] runs the
//! host's hook between the stop and the start, and a plain [`restart`] refuses
//! the rate change.
//!
//! [`restart_with`]: TuttiDriver::restart_with
//! [`restart`]: TuttiDriver::restart

use std::sync::Arc;

use crate::Result;
use crate::host::DeviceSelector;
use crate::output::{AudioCallbackState, AudioEngine};
use crate::roster::DeviceId;

/// One enumerated audio device.
///
/// Returned by [`AudioEngine::output_devices`] and
/// [`DeviceHost::output_devices`](crate::DeviceHost::output_devices) /
/// [`input_devices`](crate::DeviceHost::input_devices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The identity [`DeviceSelector::Id`] takes.
    pub id: DeviceId,
}

impl DeviceInfo {
    /// The device's human-readable name, as the OS reports it. Empty if the
    /// host failed to name it.
    #[must_use]
    pub fn name(&self) -> &str {
        self.id.name()
    }

    /// Devices for `names`, in enumeration order.
    pub(crate) fn list(names: impl IntoIterator<Item = String>) -> Vec<DeviceInfo> {
        DeviceId::list(names)
            .into_iter()
            .map(|id| DeviceInfo { id })
            .collect()
    }
}

/// Access to the engine while its stream is stopped, handed to a restart hook.
///
/// Only [`TuttiDriver::restart_with`] and [`TuttiDriver::restart_on`] create
/// one, and only for the duration of the hook, so what it reaches of the
/// audio thread's state is reached only while no callback can run.
#[derive(Debug)]
pub struct Stopped<'a> {
    state: &'a AudioCallbackState,
}

impl Stopped<'_> {
    /// Installs every commit the graph's editor has sent, moving the engine's
    /// clock to a re-prepare's rate
    /// ([`Engine::apply_pending`](tutti_core::Engine::apply_pending)). Returns
    /// whether the graph now runs a plan with no re-prepare pending (`false`
    /// while a re-prepare is half done). It is the engine's call, not
    /// `Executor::apply_pending`, which drains only the commit queue.
    ///
    /// A host drives a re-prepare's two halves through it — settle (the
    /// executor checks its units out), let the editor collect and resume,
    /// settle again (the executor adopts the resumed plan) — so the first
    /// block at the new rate renders the re-prepared graph instead of the
    /// silent block a re-prepare otherwise costs.
    pub fn apply_pending(&self) -> bool {
        // `Engine::apply_pending` panics if a callback renders at the same
        // moment. None can: a `Stopped` exists only inside a restart hook, after
        // `AudioEngine::stop` dropped the stream and before the next opens, and
        // `TuttiDriver::new` makes the driver's streams the state's only renderer.
        self.state.engine.apply_pending()
    }
}

/// The output device lifecycle a host holds: start, restart on another device
/// or rate, stop.
///
/// Built with [`new`](Self::new) from an opened [`AudioEngine`] and the
/// [`AudioCallbackState`] its callback renders. Reads (spec, running state,
/// faults) go through [`engine`](Self::engine). Every method runs on the control
/// thread and there is no internal lock; the stream is not `Sync`, so keep the
/// driver on one thread.
///
/// See the [crate-level example](crate) for building one.
pub struct TuttiDriver {
    audio_engine: AudioEngine,
    callback_state: Arc<AudioCallbackState>,
    /// The rate the graph runs at: the device's at build, then only what a
    /// restart hook returned `Ok` for. Kept apart from the spec, which a
    /// restart resolves to the new device's config before its hook runs —
    /// comparing against that would let a second plain `restart` onto the
    /// same new-rate device find "no change" and start over the old graph.
    graph_rate: tutti_types::SampleRate,
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
    /// Creates a driver from an opened device and the state its callback will
    /// read. Does not start or stop anything.
    ///
    /// From here on the driver's streams are the only thing that renders
    /// `callback_state`: a host must not also call
    /// [`process_audio`](crate::process_audio) on a clone of it. A restart
    /// relies on that — its hook reaches the engine from the control side
    /// ([`Stopped::apply_pending`]) because the stream it stopped was the only
    /// renderer.
    pub fn new(audio_engine: AudioEngine, callback_state: Arc<AudioCallbackState>) -> Self {
        Self {
            graph_rate: audio_engine.sample_rate(),
            audio_engine,
            callback_state,
        }
    }

    /// Returns the rate the graph runs at, as far as this driver knows.
    ///
    /// This is the device's rate at [`new`](Self::new), then the
    /// rate of the last restart whose hook returned `Ok`. A plain
    /// [`restart`](Self::restart) compares the new device against it.
    pub fn graph_rate(&self) -> tutti_types::SampleRate {
        self.graph_rate
    }

    /// Restarts the stream on a (possibly different) output device, which
    /// must run at the rate the graph was built at.
    ///
    /// Stops the current stream, selects `device` (a
    /// [`DeviceSelector`], or an `Option<usize>`
    /// position with `None` for the default), reads its config and starts a
    /// new stream on it.
    ///
    /// # Errors
    ///
    /// [`Error::RateChanged`](crate::Error::RateChanged) when the device now
    /// runs at another rate, **with the stream left stopped**: this driver
    /// holds the graph's audio side only, so it can re-rate nothing, and a
    /// graph at the previous rate would play every node off pitch and off
    /// tempo without a word. Restart through
    /// [`restart_with`](Self::restart_with) to re-rate the graph between
    /// the stop and the start. Otherwise what [`AudioEngine::start`] reports.
    pub fn restart(&mut self, device: impl Into<DeviceSelector>) -> Result<()> {
        self.restart_with(device, refuse_rate_change(self.graph_rate))
    }

    /// Restarts like [`restart`](Self::restart), running `rerate` between the
    /// stop and the start with the config the new stream will run at.
    ///
    /// The hook is where a host re-rates what the driver does not own — the
    /// graph's control side, the transport's rate, the device config it
    /// publishes — while no callback runs: the stream starts only once it has
    /// returned `Ok`, so the first block at the new rate renders the
    /// re-rated graph. A hook that fails leaves the stream **stopped** and
    /// its error returned: nothing plays at a rate the graph was not moved
    /// to. Its `Ok` is the claim that the graph now runs at the spec's rate
    /// ([`graph_rate`](Self::graph_rate)); after a failure the driver keeps
    /// the previous rate and [`spec`](AudioEngine::spec), so a plain `restart` onto
    /// the previous device recovers.
    ///
    /// The hook is also handed a [`Stopped`]: the engine, reachable from the
    /// control side for as long as the hook runs and no callback can, so the
    /// host can finish a graph re-prepare before the first block
    /// ([`Stopped::apply_pending`]).
    ///
    /// # Errors
    ///
    /// What resolving or opening the device reports (as `E`), or the hook's
    /// own error.
    pub fn restart_with<E: From<crate::Error>>(
        &mut self,
        device: impl Into<DeviceSelector>,
        rerate: impl FnOnce(&crate::OutputSpec, &Stopped<'_>) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), E> {
        self.audio_engine.stop();
        self.audio_engine.select_device(device.into());
        let old = self.audio_engine.spec().clone();
        let device = self.audio_engine.resolve()?;
        self.rerate_or_restore(old, rerate)?;
        self.audio_engine.start_with(
            self.callback_state.clone(),
            crate::CpalDriver::from_device(device),
        )?;
        Ok(())
    }

    /// Restarts like [`restart_with`](Self::restart_with), but on a driver of
    /// the caller's choosing at the caller's `spec`, resolving no device.
    ///
    /// The device-free counterpart of `restart_with`, as
    /// [`start_with`](Self::start_with) is of `restart`: over a
    /// [`ManualStreamDriver`](crate::ManualStreamDriver), `spec` stands for
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

    /// The engine this driver runs, for reads: [`spec`](AudioEngine::spec),
    /// [`is_running`](AudioEngine::is_running), [`faults`](AudioEngine::faults),
    /// [`device_name`](AudioEngine::device_name).
    #[must_use]
    pub fn engine(&self) -> &AudioEngine {
        &self.audio_engine
    }

    /// Selects a device for the next [`restart`](Self::restart).
    pub fn select_device(&mut self, sel: DeviceSelector) -> &mut Self {
        self.audio_engine.select_device(sel);
        self
    }

    /// Stops any running stream and starts a new one on a driver of the
    /// caller's choosing, at the current [`spec`](AudioEngine::spec).
    ///
    /// The device path is [`restart`](Self::restart), which builds a real
    /// CPAL stream. This lets a host or a test run the same lifecycle over a
    /// [`ManualStreamDriver`](crate::ManualStreamDriver) with no device open.
    ///
    /// # Errors
    /// As [`AudioEngine::start_with`].
    pub fn start_with<D: crate::StreamDriver>(&mut self, driver: D) -> Result<()>
    where
        D::Running: 'static,
    {
        self.audio_engine.stop();
        self.audio_engine
            .start_with(self.callback_state.clone(), driver)
    }

    /// Stops the stream. Idempotent.
    pub fn stop(&mut self) {
        self.audio_engine.stop();
    }
}

/// The hook a plain [`TuttiDriver::restart`] runs: the new device must run
/// at `graph`, the rate the graph was built at, or the restart stops there
/// with [`Error::RateChanged`](crate::Error::RateChanged).
fn refuse_rate_change(
    graph: tutti_types::SampleRate,
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
mod tests;
