//! [`MidiSession`] — what is connected, and the sink to reach it.
//!
//! Three properties this type is built around:
//!
//! 1. **It owns no driver code.** Everything OS-specific is behind
//!    [`MidiEndpoints`], so this file has no `#[cfg]` at all.
//! 2. **The output sink is `Option<Box<dyn MidiOut>>`, absent when nothing is
//!    connected.** An unconnected sink is *absent* rather than a sink that
//!    silently swallows, so a caller cannot accidentally send into nothing —
//!    [`send`](MidiSession::send) reports `0` and the loss is countable.
//! 3. **No background threads.** A native-UMP send is a non-blocking syscall
//!    and connections are held right here, so nothing needs an output thread.
//!    (The ALSA *input* pump is the backend's own thread, not this type's.)
//!
//! # Threading
//!
//! Everything here is control-thread. Inbound events never pass through this
//! type — a backend pushes them straight into the
//! [`InputProducerHandle`] of a [`HardwareMidiInputs`] ring, which the audio
//! thread drains via `MidiIn`. That is why there is no `poll` on this type.
//!
//! The connection map is a plain `Mutex`, not an [`ArcLend`]: nothing here is
//! read from the audio thread, and `CLAUDE.md` names device enumeration in this
//! crate as a deliberate exception to the publish rule.
//!
//! [`ArcLend`]: tutti_midi_types::ArcLend
//! [`HardwareMidiInputs`]: crate::HardwareMidiInputs
//! [`InputProducerHandle`]: crate::InputProducerHandle

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::HardwareMidiInputs;
use crate::backend;
use crate::devices::{Devices, OpenInput, OpenOutput};
use crate::endpoints::MidiEndpoints;
use crate::error::{Error, Result};
use tutti_midi_device::{DeviceId, DeviceInfo};
use tutti_midi_types::MidiOut;
use tutti_midi_types::ump::MidiEvent;

/// The set of open MIDI connections, and the endpoints available to open.
///
/// Cheap to clone (everything is behind one `Arc`), so a host can hold it in a
/// resource and hand clones to whatever needs to connect or send.
#[derive(Clone)]
pub struct MidiSession {
    inner: Arc<Inner>,
}

/// The session's state.
///
/// # Drop order
///
/// Fields drop in declaration order: the open endpoints first (each
/// [`OpenInput`] closes its connection, then retires its port; the output
/// closes), then the ring manager handle, then the devices and the backend
/// whose client every connection was opened on. Closing a connection stops its
/// driver: the ALSA backend joins its pump thread, which drops the producer
/// handle before the port is retired. On macOS the handle lives in the input
/// callback, which CoreMIDI retains and may release (and so drop the handle)
/// after the port is disposed; a push in that window lands in a ring nobody
/// drains, which is harmless. The order is about not leaving ports behind, not
/// about soundness: each handle owns its half of the ring and cannot outlive
/// the memory it pushes into.
struct Inner {
    open: Mutex<Open>,
    /// Ring manager the backend pushes inbound events into: each open input
    /// takes a port from it and retires the port on close.
    ports: Arc<HardwareMidiInputs>,
    /// The devices present, and the backend that opens them.
    devices: Devices,
}

/// What is currently connected.
#[derive(Default)]
struct Open {
    /// Live input connections, keyed by device.
    inputs: HashMap<DeviceId, OpenInput>,
    /// The single output sink, if one is connected.
    output: Option<OpenOutput>,
}

impl Inner {
    /// The open endpoints. A panic while the lock was held leaves plain maps
    /// behind, so a poisoned lock is taken as it is rather than panicking
    /// every later call.
    fn open(&self) -> std::sync::MutexGuard<'_, Open> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl core::fmt::Debug for MidiSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let open = self.inner.open();
        f.debug_struct("MidiSession")
            .field("inputs", &open.inputs.len())
            .field("output", &open.output.as_ref().map(OpenOutput::name))
            .finish_non_exhaustive()
    }
}

impl MidiSession {
    /// Opens a session over this platform's backend.
    ///
    /// # Errors
    ///
    /// As [`Devices::open`].
    pub fn open(ports: Arc<HardwareMidiInputs>) -> Result<Self> {
        Self::open_over(backend::active(), ports)
    }

    /// A session over a supplied backend.
    ///
    /// This is what makes the session testable: a fake [`MidiEndpoints`] drives
    /// every connect/disconnect path deterministically, on any platform,
    /// including one with no MIDI hardware. Without it a test can assert only
    /// "does not panic".
    ///
    /// # Errors
    ///
    /// As [`Devices::open_over`].
    pub fn open_over(
        backend: Box<dyn MidiEndpoints>,
        ports: Arc<HardwareMidiInputs>,
    ) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                open: Mutex::new(Open::default()),
                ports,
                devices: Devices::open_over(backend)?,
            }),
        })
    }

    /// The devices present. A host that wants hot-plug subscribes
    /// ([`Devices::subscribe`]); the session's `Phase::Poll` has a
    /// subscription of its own.
    pub fn devices(&self) -> &Devices {
        &self.inner.devices
    }

    /// The ring manager inbound events land in. The audio thread polls this.
    pub fn ports(&self) -> &Arc<HardwareMidiInputs> {
        &self.inner.ports
    }

    // --- Enumeration ---

    /// Devices that can send MIDI to this engine, as of now, ordered by id.
    pub fn inputs(&self) -> Vec<DeviceInfo> {
        self.devices_where(|d| d.direction.is_input())
    }

    /// Devices this engine can send MIDI to, as of now, ordered by id.
    pub fn outputs(&self) -> Vec<DeviceInfo> {
        self.devices_where(|d| d.direction.is_output())
    }

    fn devices_where(&self, keep: impl Fn(&DeviceInfo) -> bool) -> Vec<DeviceInfo> {
        let mut list = self.inner.devices.list();
        list.retain(keep);
        list
    }

    /// The shared name match for every `*_by_name` method here: **case-
    /// insensitive substring, first hit wins**, in the backend's enumeration
    /// order. See [`connect_input_by_name`](MidiSession::connect_input_by_name)
    /// for what that means for a caller.
    fn find(list: Vec<DeviceInfo>, name: &str) -> Option<DeviceInfo> {
        let needle = name.to_lowercase();
        list.into_iter()
            .find(|e| e.name.to_lowercase().contains(&needle))
    }

    // --- Input ---

    /// Opens an input endpoint. Idempotent — connecting an already-open endpoint
    /// succeeds without reopening it, because a second driver connection to one
    /// device duplicates every inbound event.
    ///
    /// Each newly opened input gets its own ring in [`ports`](Self::ports); the
    /// backend pushes into it from the driver's thread and the audio thread
    /// drains it. Events never pass through this type.
    ///
    /// # Errors
    ///
    /// [`Error::NoDevice`] when `id` names no present device — the shape a
    /// stale id takes after a hot-plug — and [`Error::WrongDirection`] when it
    /// is not an input. Otherwise whatever the backend raises while opening the
    /// port.
    pub fn connect_input(&self, id: &DeviceId) -> Result<()> {
        let mut open = self.inner.open();
        // An input whose reader died is replaced, not kept: it is silent.
        if open.inputs.get(id).is_some_and(OpenInput::is_alive) {
            return Ok(());
        }
        open.inputs.remove(id);

        // One ring per connection, retired with it.
        let input = self.inner.devices.open_input(id, &self.inner.ports)?;
        open.inputs.insert(id.clone(), input);
        Ok(())
    }

    /// Opens the first input endpoint whose name contains `name`.
    ///
    /// # Matching
    ///
    /// **Case-insensitive substring, first hit wins.** `name` is lowercased and
    /// tested with `contains` against each device's lowercased name, in the
    /// order [`inputs`](Self::inputs) returns them: by id. So `"iac"` matches
    /// `"IAC Driver Bus 1"`, and a `name` matching two devices picks the one
    /// with the lowest id. Connect by [`DeviceId`] when that matters.
    ///
    /// # Errors
    ///
    /// [`Error::NoDeviceNamed`] when nothing matches. A device that is absent is
    /// an error here, never a silent no-op.
    pub fn connect_input_by_name(&self, name: &str) -> Result<()> {
        let info =
            Self::find(self.inputs(), name).ok_or_else(|| Error::NoDeviceNamed(name.to_owned()))?;
        self.connect_input(&info.id)
    }

    /// Closes an input endpoint and retires the port its events landed in.
    /// Returns whether it was open; closing one that is not is a no-op.
    ///
    /// Events still queued in that port are discarded with it.
    pub fn disconnect_input(&self, id: &DeviceId) -> bool {
        self.inner.open().inputs.remove(id).is_some()
    }

    /// Closes one open input whose name contains `name`, matched
    /// case-insensitively.
    ///
    /// Closes **at most one**, and the search runs over a `HashMap` of open
    /// connections — so unlike the `connect_*_by_name` pair there is no
    /// "first" to speak of: a `name` matching two open inputs closes an
    /// arbitrary one of them. Pass a [`DeviceId`] to
    /// [`disconnect_input`](Self::disconnect_input) to be exact, or
    /// [`disconnect_all_inputs`](Self::disconnect_all_inputs) to close every
    /// one.
    ///
    /// Matching nothing is a no-op, not an error.
    pub fn disconnect_input_by_name(&self, name: &str) {
        let needle = name.to_lowercase();
        let id = {
            let open = self.inner.open();
            open.inputs
                .iter()
                .find(|(_, input)| input.name().to_lowercase().contains(&needle))
                .map(|(id, _)| id.clone())
        };
        if let Some(id) = id {
            self.disconnect_input(&id);
        }
    }

    /// Closes every open input, retiring each one's port.
    pub fn disconnect_all_inputs(&self) {
        self.inner.open().inputs.clear();
    }

    /// Whether any input is open.
    #[must_use]
    pub fn is_any_input_connected(&self) -> bool {
        !self.inner.open().inputs.is_empty()
    }

    /// The names of every open input.
    ///
    /// Returns an owned `Vec` rather than an iterator because the names live
    /// behind a `Mutex`: an iterator would borrow the guard, and the guard
    /// cannot outlive this call. Making the container generic over
    /// `FromIterator` was tried and reverted — it saves one `collect` at the
    /// single caller that wants a `HashSet`, and costs a turbofish at every
    /// caller that just wants the names, because `String` and `Box<str>` both
    /// satisfy the bound and inference has nothing to go on.
    pub fn connected_input_names(&self) -> Vec<String> {
        self.inner
            .open()
            .inputs
            .values()
            .map(|input| input.name().to_owned())
            .collect()
    }

    // --- Output ---

    /// Opens an output endpoint, replacing any currently open one.
    ///
    /// One at a time: a session sends to a single destination, so connecting a
    /// second output closes the first rather than fanning out.
    ///
    /// # Errors
    ///
    /// [`Error::NoDevice`] when `id` names no present device,
    /// [`Error::WrongDirection`] when it is not an output, plus whatever the
    /// backend raises while opening.
    pub fn connect_output(&self, id: &DeviceId) -> Result<()> {
        let output = self.inner.devices.open_output(id)?;
        self.inner.open().output = Some(output);
        Ok(())
    }

    /// Opens the first output endpoint whose name contains `name`, replacing any
    /// currently open one.
    ///
    /// Matches exactly as
    /// [`connect_input_by_name`](Self::connect_input_by_name) does:
    /// case-insensitive substring, first hit in [`outputs`](Self::outputs)
    /// order.
    ///
    /// # Errors
    ///
    /// [`Error::NoDeviceNamed`] when nothing matches.
    pub fn connect_output_by_name(&self, name: &str) -> Result<()> {
        let info = Self::find(self.outputs(), name)
            .ok_or_else(|| Error::NoDeviceNamed(name.to_owned()))?;
        self.connect_output(&info.id)
    }

    /// Closes the output endpoint, if one is open. Returns whether one was.
    pub fn disconnect_output(&self) -> bool {
        self.inner.open().output.take().is_some()
    }

    /// Whether an output is open.
    #[must_use]
    pub fn is_output_connected(&self) -> bool {
        self.inner.open().output.is_some()
    }

    /// The open output's name, if any.
    pub fn output_device_name(&self) -> Option<String> {
        self.inner
            .open()
            .output
            .as_ref()
            .map(|o| o.name().to_owned())
    }

    /// The open output's device, if any.
    pub fn output_device(&self) -> Option<DeviceId> {
        self.inner.open().output.as_ref().map(|o| o.id().clone())
    }

    /// Whether `id` is an open input that is still delivering: one whose
    /// reader ended on an error is not (see [`OpenInput::is_alive`]), and
    /// [`connect_input`](Self::connect_input) reopens it.
    #[must_use]
    pub fn is_input_connected(&self, id: &DeviceId) -> bool {
        self.inner
            .open()
            .inputs
            .get(id)
            .is_some_and(OpenInput::is_alive)
    }

    /// Sends events to the open output.
    ///
    /// Returns how many the device **accepted**: `0` when nothing is connected,
    /// and `< events.len()` when the endpoint refused part of the batch. That is
    /// the number a caller needs in order to count what was lost.
    ///
    /// The count comes from the sink, not from `events.len()`, which is what
    /// makes "the device refused every event" distinguishable from success —
    /// the backends only log a refusal at `debug!`, so this return is the sole
    /// programmatic channel for it.
    ///
    /// Both backends stop at the first failure rather than skipping it, so the
    /// count names an unbroken **prefix** of `events`. Control-thread only: this
    /// takes a lock and the backends allocate per send.
    pub fn send(&self, events: &[MidiEvent]) -> usize {
        let open = self.inner.open();
        match open.output.as_ref() {
            Some(out) => out.queue(events),
            None => 0,
        }
    }
}

/// Sending through the session is sending to its open output.
///
/// The thin trait impl over the inherent [`send`](MidiSession::send), which
/// keeps the accepted count — the same idiom `MidiSender` uses.
impl MidiOut for MidiSession {
    fn queue(&self, events: &[MidiEvent]) -> usize {
        self.send(events)
    }
}

#[cfg(test)]
mod tests;
