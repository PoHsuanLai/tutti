//! [`Device`]: the sound card, and restarting it at whatever rate it comes
//! back with ([`Session::restart`]).
//!
//! The driver owns the stream and the graph's *audio* side only, so on its
//! own it can re-open a device and nothing more: a device that comes back at
//! another rate would play a graph built for the old one, every oscillator
//! off pitch, the beat clock off tempo, every figure in samples (PDC, a
//! lookahead) wrong, with no error anywhere. A restart runs the driver's
//! restart with a hook that, while no callback runs, moves everything the
//! session owns to the new configuration:
//!
//! - the graph: both halves of `Editor::reprepare`, inside the hook (the
//!   engine settled from the control side while no callback runs,
//!   `Stopped::settle_graph`), so the first block at the new rate renders
//!   the re-prepared graph, not the silent block of a re-prepare with its
//!   units out;
//! - the engine follows by itself: it adopts the new rate on the first block
//!   that carries it, rescaling its frame clock and every scheduled
//!   `At::Frame` transport command to the same wall-clock time;
//! - the graph root, widened to a wider device as the build widens it
//!   ([`root_width`](crate::root_width)), never narrowed;
//! - the transport's rate, the hardware MIDI input's timestamp rate, the
//!   session's disk streamer (every open stream's conversion ratio) and,
//!   through the re-prepare, the MIDI clock node;
//! - the compensation figures in samples ([`Session::compensation`],
//!   [`Session::latency`]), published before the first block;
//! - then the host's own `follow` hook, for what it holds itself.
//!
//! Every unit instance is kept across the re-prepare, and only time-based
//! state resets (`Editor::reprepare`'s rule).
//!
//! **Recovery after a failed hook:** the stream is left stopped, and the
//! driver keeps the old spec and graph rate ([`Device::graph_rate`]). A
//! restart onto the old device at the old rate moves nothing and plays
//! again.

use std::marker::PhantomData;
use std::sync::Arc;

use tutti_core::{SampleRate, Samples};
use tutti_cpal::{
    DeviceInfo, DeviceSelector, OutputSpec, StreamDriver, StreamFault, StreamFaults, TuttiDriver,
};

use crate::{Error, Result, Session};

/// The sound card: owns the `TuttiDriver`. `!Send`, so it stays on the
/// thread that opened it.
///
/// `cpal::Stream` is `!Send`; `tutti-cpal` wraps it as `Send` on the
/// argument that the stream is created and dropped on the control thread. A
/// `Device` that could move would let a host drop the stream on another
/// thread, and that argument would be false with nothing to say so: here it
/// is a type error.
///
/// ```compile_fail
/// fn moves(device: tutti_session::Device) {
///     std::thread::spawn(move || drop(device));
/// }
/// ```
// Mutation (run): `_not_send` removed → the doctest above compiles, and
// fails. Everything else in it compiles as written (`Device` is a public
// type), so `Send` is the only thing it can be failing on.
pub struct Device {
    driver: TuttiDriver,
    _not_send: PhantomData<*const ()>,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("driver", &self.driver)
            .finish()
    }
}

impl Device {
    pub(crate) fn new(driver: TuttiDriver) -> Self {
        Self {
            driver,
            _not_send: PhantomData,
        }
    }

    /// Whether the stream is open and its device has not been reported gone.
    pub fn is_running(&self) -> bool {
        self.driver.is_running()
    }

    /// The name of the selected output device.
    ///
    /// # Errors
    ///
    /// When the host cannot name it.
    pub fn device_name(&self) -> Result<String> {
        Ok(self.driver.device_name()?)
    }

    /// The backend fault record, which survives stop and restart.
    /// [`Session::update`] reports each new fault as
    /// [`SessionEvent::DeviceFault`](crate::SessionEvent::DeviceFault).
    pub fn faults(&self) -> Arc<StreamFaults> {
        self.driver.faults()
    }

    /// Takes the most recent backend fault, clearing it.
    pub fn take_fault(&self) -> Option<StreamFault> {
        self.driver.take_fault()
    }

    /// The configuration of the stream that is playing, or that would be.
    pub fn spec(&self) -> &OutputSpec {
        self.driver.spec()
    }

    /// The rate the graph runs at, as far as the device knows: the build's,
    /// then that of the last restart whose hook succeeded.
    pub fn graph_rate(&self) -> SampleRate {
        self.driver.graph_rate()
    }

    /// Stops the stream. Idempotent.
    pub fn stop(&mut self) {
        self.driver.stop();
    }

    /// Selects a device by name or index for the next restart.
    pub fn select_device(&mut self, device: DeviceSelector) -> &mut Self {
        self.driver.select_device(device);
        self
    }

    /// Stops any running stream and starts a new one on `driver`, at the
    /// current [`spec`](Self::spec): the same lifecycle with no device open.
    ///
    /// # Errors
    ///
    /// What the driver's `open` reports.
    pub fn start_with<D: StreamDriver>(&mut self, driver: D) -> Result<()>
    where
        D::Running: 'static,
    {
        Ok(self.driver.start_with(driver)?)
    }

    /// The default host's output devices.
    ///
    /// # Errors
    ///
    /// When the host cannot enumerate.
    pub fn devices() -> Result<impl Iterator<Item = DeviceInfo>> {
        Ok(TuttiDriver::devices()?)
    }
}

/// What [`Session::restart`] asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceRestart {
    /// The output device; the host's default by default.
    pub device: DeviceSelector,
    /// The largest block the graph is prepared for from the restart on;
    /// `None` keeps the one it has. At most the engine's block capacity
    /// (`Engine::block_capacity`): past it the restart is refused before the
    /// stream stops.
    ///
    /// A host's choice, not the device's: the device hands over whatever
    /// callback it likes, and the engine renders one longer than this as
    /// consecutive graph blocks.
    pub max_block: Option<Samples>,
}

impl Session {
    /// Restarts `device` on the device `request` selects and moves the
    /// graph, the transport and every rate-derived figure to the
    /// configuration it comes back with (see the module docs).
    ///
    /// # Errors
    ///
    /// - [`Error::Reprepare`] **before the stream stops**, changing
    ///   nothing, when the graph cannot take the request: `max_block` past
    ///   the engine's block capacity (`CommitError::BlockTooLong`), a
    ///   re-prepare already between its halves (`Repreparing`; retry on a
    ///   later update), or a poisoned graph. A zero `max_block` is
    ///   [`Error::Device`] (`InvalidConfig`), also before the stop.
    /// - What resolving or opening the device reports ([`Error::Device`]).
    /// - [`Error::Reprepare`] from the re-prepare itself (a graph that no
    ///   longer compiles): the stream is then left **stopped**. Restart
    ///   again with another `max_block`, or onto the old device and rate,
    ///   which moves nothing and plays again.
    pub fn restart(&mut self, device: &mut Device, request: DeviceRestart) -> Result<()> {
        self.restart_following(device, request, |_| {})
    }

    /// [`restart`](Self::restart), running `follow` with the new spec at the
    /// end of the hook, while no callback runs: for what the host holds
    /// itself (a disk streamer it took, a device config it publishes).
    ///
    /// # Errors
    ///
    /// As [`restart`](Self::restart).
    pub fn restart_following(
        &mut self,
        device: &mut Device,
        request: DeviceRestart,
        follow: impl FnOnce(&OutputSpec),
    ) -> Result<()> {
        self.check_restart(request.max_block)?;
        let max_block = request.max_block;
        device.driver.restart_with(request.device, |spec, stopped| {
            self.rerate(spec, stopped, max_block, follow)
        })
    }

    /// [`restart_following`](Self::restart_following) on a driver of the
    /// caller's choosing, `spec` standing for the config the new device
    /// reports: the device-free restart, for a test or a stream a host
    /// drives itself.
    ///
    /// # Errors
    ///
    /// As [`restart`](Self::restart), without the device's own.
    pub fn restart_on<D: StreamDriver>(
        &mut self,
        device: &mut Device,
        spec: OutputSpec,
        driver: D,
        max_block: Option<Samples>,
        follow: impl FnOnce(&OutputSpec),
    ) -> Result<()>
    where
        D::Running: 'static,
    {
        self.check_restart(max_block)?;
        device.driver.restart_on(spec, driver, |spec, stopped| {
            self.rerate(spec, stopped, max_block, follow)
        })
    }

    /// The checks that must pass before the stream stops.
    fn check_restart(&self, max_block: Option<Samples>) -> Result<()> {
        if max_block.is_some_and(|b| b.is_zero()) {
            return Err(Error::Device(tutti_cpal::Error::InvalidConfig(
                "a zero-frame max block".into(),
            )));
        }
        self.graph.check_reprepare(max_block)?;
        Ok(())
    }

    /// The hook: move everything the session owns to `spec`, with no
    /// callback running.
    fn rerate(
        &mut self,
        spec: &OutputSpec,
        stopped: &tutti_cpal::Stopped<'_>,
        max_block: Option<Samples>,
        follow: impl FnOnce(&OutputSpec),
    ) -> Result<()> {
        let rate = spec.sample_rate;
        // The new device's callback size too: a hosted plugin's pipeline
        // follows it, and its declared latency with it.
        self.graph.reprepare(rate, max_block, spec.quantum)?;
        // The transport's rate first: it marks every frame-timed command sent
        // from here on as already in the new rate's frames, and the engine's
        // settle below rescales only the ones before the mark.
        self.transport.set_sample_rate(rate);
        // Both halves of the re-prepare, now, while no callback runs: the
        // executor checks its units out, the editor re-prepares them and
        // sends the resumed plan, and the executor adopts it.
        stopped.settle_graph();
        self.graph.collect();
        // A unit that panicked in `prepare`, or shapes that no longer
        // compile, poison the editor here: refuse, the stream still stopped.
        self.graph.check_reprepare(None)?;
        // A wider device widens the root, by the build's rule; a narrower one
        // keeps it, and the engine folds. Committed and settled here too, so
        // the first block has the width. A commit refused for now leaves the
        // edit flag set, and the next update retries.
        let width = crate::root_width(self.graph.outputs(), spec.channels);
        if width > self.graph.outputs() {
            self.graph.widen_outputs(width);
            self.commit();
        }
        let running = stopped.settle_graph();
        debug_assert!(running, "the re-prepare resumed inside the hook");
        // The figures the first block runs at, published before it.
        self.publish_sent();
        // The port manager turns an event's wall-clock arrival into a frame
        // offset at this rate; its contract is to be told before the stream
        // starts, which is where the hook runs.
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = &self.midi_io {
            io.ports().set_sample_rate(rate);
        }
        // Every open stream's ratio (file rate / session rate) is
        // re-derived, so it plays on at its pitch.
        #[cfg(feature = "sampler")]
        if let Some(streamer) = &self.disk_streamer {
            streamer.set_sample_rate(rate);
        }
        follow(spec);
        Ok(())
    }
}
