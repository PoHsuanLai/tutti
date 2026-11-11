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
//!   `Stopped::apply_pending`), so the first block at the new rate renders
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

use tutti_cpal::{
    DeviceInfo, DeviceSelector, OutputSpec, SnapshotSource, StreamDriver, StreamFault,
    StreamFaults, TuttiDriver,
};
use tutti_types::{SampleRate, Samples};

use crate::graph::Graph;
use crate::{DevicePolicy, DeviceWatch, Error, FollowEvent, Result, Session, SessionEvent};

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
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.driver.engine().is_running()
    }

    /// The identity of the device `selector` opened: the id it names, or for
    /// the default the device's name (cpal names the default and nothing
    /// else, so ordinal 0). `None` if the host cannot name it.
    #[must_use]
    pub fn opened_id(&self, selector: &DeviceSelector) -> Option<tutti_cpal::DeviceId> {
        match selector {
            DeviceSelector::Id(id) => Some(id.clone()),
            DeviceSelector::Default => self
                .device_name()
                .ok()
                .flatten()
                .map(tutti_cpal::DeviceId::new),
        }
    }

    /// The name of the output device the engine last opened, as it was then.
    /// It asks the host nothing, so it never enumerates the devices (which
    /// can block for good) and is safe on the thread that builds a frame.
    ///
    /// `Ok(None)` means the engine runs on a driver the host supplied
    /// ([`SessionBuilder::build_device_on`](crate::SessionBuilder::build_device_on)): it names no
    /// device, by design.
    ///
    /// # Errors
    ///
    /// When the device opened but the host could not name it.
    pub fn device_name(&self) -> Result<Option<String>> {
        Ok(self.driver.engine().opened_name()?)
    }

    /// The backend fault record, which survives stop and restart.
    /// [`Session::update`] reports each new fault as
    /// [`SessionEvent::DeviceFault`].
    pub fn faults(&self) -> Arc<StreamFaults> {
        self.driver.engine().faults()
    }

    /// Takes the most recent backend fault, clearing it.
    pub fn take_fault(&self) -> Option<StreamFault> {
        self.driver.engine().faults().take_last()
    }

    /// The configuration of the stream that is playing, or that would be.
    pub fn spec(&self) -> &OutputSpec {
        self.driver.engine().spec()
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
    /// **Blocks** for as long as the host's enumeration does: on ALSA it opens
    /// every PCM, which can wait on a sound server for good. Call it off the
    /// thread that renders or builds frames; the session's own listing
    /// ([`Session::list_outputs`]) is that, and a host may choose to call
    /// this one anyway.
    ///
    /// # Errors
    ///
    /// When the host cannot enumerate.
    pub fn devices() -> Result<impl Iterator<Item = DeviceInfo>> {
        Ok(tutti_cpal::AudioEngine::output_devices()?.into_iter())
    }
}

/// A restart that moved the stream ([`SessionEvent::DeviceRestarted`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceRestarted {
    /// The rate the graph ran at before.
    pub previous_rate: SampleRate,
    /// The rate it runs at now.
    pub rate: SampleRate,
}

impl DeviceRestarted {
    /// Whether the rate moved: a capture stream opened at the old rate now
    /// feeds the graph frames of the wrong rate until it is reopened.
    #[must_use]
    pub fn rate_changed(&self) -> bool {
        self.previous_rate != self.rate
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
        self.note_chosen(request.device.clone());
        self.run_restart(device, request, follow)
    }

    /// Runs a restart the device policy asked for
    /// ([`SessionEvent::RestartWanted`]), with `follow` as in
    /// [`restart_following`](Self::restart_following). Unlike a restart of
    /// the host's, it is not a choice: it never changes what the policy
    /// follows. It is dropped, returning `Ok(false)`, when the host has chosen
    /// something else since it was asked for (the request is stale), or
    /// there is no watch.
    ///
    /// # Errors
    ///
    /// As [`restart`](Self::restart).
    pub fn restart_wanted(
        &mut self,
        device: &mut Device,
        request: DeviceRestart,
        follow: impl FnOnce(&OutputSpec),
    ) -> Result<bool> {
        if self
            .device_watch
            .as_ref()
            .is_none_or(|w| *w.follow().want() != request.device)
        {
            return Ok(false);
        }
        self.run_restart(device, request, follow).map(|()| true)
    }

    /// While held, the device policy asks for no restart (the watch still
    /// reports devices). For a host that must not have the stream moved
    /// under it, a take being recorded: a restart can change the rate.
    pub fn hold_device_restarts(&mut self, held: bool) {
        if let Some(watch) = &mut self.device_watch {
            watch.hold(held);
        }
    }

    /// The selector the device policy follows, if a watch runs.
    #[must_use]
    pub fn watch_devices_want(&self) -> Option<DeviceSelector> {
        self.device_watch
            .as_ref()
            .map(|w| w.follow().want().clone())
    }

    /// The devices the watch last saw, `None` before its first look or
    /// without a watch.
    #[must_use]
    pub fn watched_devices(&self) -> Option<&tutti_cpal::Snapshot> {
        self.device_watch.as_ref().and_then(|w| w.snapshot())
    }

    fn run_restart(
        &mut self,
        device: &mut Device,
        request: DeviceRestart,
        follow: impl FnOnce(&OutputSpec),
    ) -> Result<()> {
        let selector = request.device.clone();
        let outcome = self.check_restart(request.max_block).and_then(|()| {
            let max_block = request.max_block;
            let previous = device.graph_rate();
            device
                .driver
                .restart_with(request.device, |spec, stopped| {
                    self.rerate(spec, stopped, max_block, follow)
                })
                .map(|()| previous)
        });
        self.note_restart(device, Some(&selector), outcome)
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
        let outcome = self.check_restart(max_block).and_then(|()| {
            let previous = device.graph_rate();
            device
                .driver
                .restart_on(spec, driver, |spec, stopped| {
                    self.rerate(spec, stopped, max_block, follow)
                })
                .map(|()| previous)
        });
        self.note_restart(device, None, outcome)
    }

    /// Records a restart's outcome: the policy is told, and the next poll
    /// reports it ([`SessionEvent::DeviceRestarted`] or
    /// [`SessionEvent::RestartFailed`]). A failure that left the stream
    /// stopped keeps the old spec and graph rate, as the module docs say.
    fn note_restart(
        &mut self,
        device: &Device,
        selector: Option<&DeviceSelector>,
        outcome: Result<SampleRate>,
    ) -> Result<()> {
        let now = std::time::Instant::now();
        match outcome {
            Ok(previous_rate) => {
                // A custom driver (`restart_on`) names no device: unknown.
                let name = selector.and_then(|s| device.opened_id(s));
                // The restart cleared the fault record; its count restarts.
                if let Some((faults, seen)) = &mut self.faults {
                    *seen = faults.count();
                }
                self.device_events
                    .push(SessionEvent::DeviceRestarted(DeviceRestarted {
                        previous_rate,
                        rate: device.graph_rate(),
                    }));
                self.note_device(FollowEvent::Opened(name), now);
                Ok(())
            }
            Err(e) => {
                self.device_events
                    .push(SessionEvent::RestartFailed(e.to_string()));
                self.note_device(
                    FollowEvent::RestartFailed {
                        running: device.is_running(),
                    },
                    now,
                );
                Err(e)
            }
        }
    }

    fn note_chosen(&mut self, selector: DeviceSelector) {
        self.note_device(FollowEvent::Chose(selector), std::time::Instant::now());
    }

    /// Feeds the policy and queues a restart it asks for.
    fn note_device(&mut self, event: FollowEvent, now: std::time::Instant) {
        if let Some(watch) = &mut self.device_watch
            && let Some(request) = watch.note(event, now)
        {
            self.device_events
                .push(SessionEvent::RestartWanted(request));
        }
    }

    /// Starts watching the host's devices (design 018): `source` is polled on
    /// every [`Phase::Poll`](crate::Phase::Poll), its changes are reported as events, and
    /// `policy` decides which of them restart the stream. `want` is the
    /// selector the stream was started with, which the policy never
    /// overrides, and `running_on` the device it opened ([`Device::opened_id`];
    /// `None`: unknown, which is never taken for a different device). Replaces a watch already running.
    pub fn watch_devices(
        &mut self,
        source: impl SnapshotSource + Send + 'static,
        policy: DevicePolicy,
        want: DeviceSelector,
        running_on: Option<tutti_cpal::DeviceId>,
    ) {
        self.device_watch = Some(DeviceWatch::new(source, policy, want, running_on));
    }

    /// Starts listing the host's output devices on a thread of its own
    /// (design 018): enumeration can block for good, so it never runs here.
    /// The answer is taken without blocking by a later
    /// [`Phase::Poll`](crate::Phase::Poll) and reported once, as
    /// [`SessionEvent::OutputsListed`] or [`SessionEvent::OutputsListFailed`];
    /// [`SessionEvent::OutputsListSlow`] is reported once if it takes
    /// [`LISTING_WARNING`](crate::LISTING_WARNING). Replaces a listing still
    /// waiting, whose thread is left to end on its own.
    pub fn list_outputs(&mut self, listing: tutti_cpal::DeviceListing) {
        self.output_listing = Some(crate::watch::OutputListing::new(listing));
    }

    /// [`list_outputs`](Self::list_outputs), once: `make` builds the listing
    /// the first time this is called and is not called again. A `make` that
    /// fails starts nothing and is asked again by the next call; its error
    /// is returned.
    ///
    /// # Errors
    ///
    /// What `make` reports.
    pub fn list_outputs_once(
        &mut self,
        make: impl FnOnce() -> tutti_cpal::Result<tutti_cpal::DeviceListing>,
    ) -> Result<()> {
        if self.outputs_listed {
            return Ok(());
        }
        self.list_outputs(make()?);
        self.outputs_listed = true;
        Ok(())
    }

    /// The [`Phase::Poll`](crate::Phase::Poll) body: what restarts reported, the stream's own
    /// faults, then the watch.
    pub(crate) fn device_poll(&mut self, now: std::time::Instant, out: &mut Vec<SessionEvent>) {
        out.append(&mut self.device_events);
        let mut lost = false;
        if let Some((faults, seen)) = &mut self.faults {
            let count = faults.count();
            if count != *seen {
                *seen = count;
                if let Some(fault) = faults.last() {
                    lost = fault.kind == tutti_cpal::StreamFaultKind::Disconnected;
                    out.push(SessionEvent::DeviceFault(fault));
                }
            }
        }
        if let Some(listing) = &mut self.output_listing {
            let (event, finished) = listing.poll(now);
            out.extend(event);
            if finished {
                self.output_listing = None;
            }
        }
        if let Some(watch) = &mut self.device_watch {
            // The loss is taken in first and decided on with the devices this
            // poll finds: a stream lost as its device left must not be sent
            // back to the device the last look still listed.
            if lost {
                watch.observe(FollowEvent::Lost);
            }
            watch.poll(now, out);
        }
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
        // A declared root has to be placeable on the new device: refused
        // before the graph moves, the stream still stopped, like any refusal
        // here. An undeclared root is a bare width and is never refused.
        if let Some(root) = self.graph.root_topology() {
            Graph::check_root(root, spec.channels)?;
        }
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
        stopped.apply_pending();
        self.graph.collect();
        // A unit that panicked in `prepare`, or shapes that no longer
        // compile, poison the editor here: refuse, the stream still stopped.
        self.graph.check_reprepare(None)?;
        // The hardware out's tap reads frames of one rate: reopened at the
        // new one, committed with the rest below.
        #[cfg(feature = "midi")]
        if self.midi_state.follow_rate(&mut self.graph) {
            self.commit();
        }
        // A declaration a device refused is asked again of this one: if it
        // places, it is in force, committed and settled below with the rest.
        if self.root_refusal.is_some() {
            match self
                .graph
                .declare_root(self.root_request.clone(), Some(spec.channels))
            {
                Ok(()) => {
                    self.root_refusal = None;
                    self.commit();
                }
                Err(e) => self.root_refusal = Some(e),
            }
        }
        // A wider device widens the root, by the build's rule; a narrower one
        // keeps it, and the engine folds. Committed and settled here too, so
        // the first block has the width. A commit refused for now leaves the
        // edit flag set, and the next update retries.
        // A declared root keeps the width it declared: the device is no floor
        // under it.
        let width = crate::root_width(self.graph.outputs(), spec.channels);
        if self.graph.root_topology().is_none() && width > self.graph.outputs() {
            // Undeclared, so it cannot be refused.
            let widened = self.graph.widen_outputs(width);
            debug_assert!(widened.is_ok());
            self.commit();
        }
        let running = stopped.apply_pending();
        debug_assert!(running, "the re-prepare resumed inside the hook");
        // The figures the first block runs at, published before it.
        self.publish_sent();
        // The hardware inlets place an arrival a block after it came, a
        // block of this device's callback; the clock map follows the rate
        // from the engine's own readings.
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = &self.midi_io {
            io.ports().inlets().set_lead(crate::midi::input_lead(
                spec.quantum
                    .unwrap_or(self.graph.prepared().max_block().into()),
                rate,
            ));
        }
        // Every open stream's ratio (file rate / session rate) is
        // re-derived, so it plays on at its pitch.
        #[cfg(feature = "sampler")]
        if let Some(streamer) = &self.disk_streamer {
            streamer.set_sample_rate(rate);
        }
        // The heard tap opens at the device's width from here on; one open
        // across this change keeps counting its blocks lost (see
        // `Session::open_heard_tap`).
        if let Some((_, layout)) = &mut self.heard {
            *layout = spec.channels;
        }
        follow(spec);
        Ok(())
    }
}
