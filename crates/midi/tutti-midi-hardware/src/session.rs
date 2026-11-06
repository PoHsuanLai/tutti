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

use crate::backend;
use crate::capability::{EndpointId, EndpointInfo};
use crate::endpoints::{InputConnection, MidiEndpoints};
use crate::error::{Error, Result};
use crate::{HardwareMidiInputs, PortId};
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
/// Stated, not left to field order alone: [`Drop`] closes every input first
/// (each connection, then its port), and only then do the fields drop, in
/// declaration order: what is left open (the output), then the ring manager
/// handle, then the backend whose client every connection was opened on.
/// Closing a connection stops its driver: the ALSA backend joins its pump
/// thread, which drops the producer handle before the port is retired. On
/// macOS the handle lives in the input callback, which CoreMIDI retains and
/// may release (and so drop the handle) after the port is disposed; a push
/// in that window lands in a ring nobody drains, which is harmless. The order
/// is about not leaving ports behind, not about soundness: each handle owns
/// its half of the ring and cannot outlive the memory it pushes into.
struct Inner {
    open: Mutex<Open>,
    /// Ring manager the backend pushes inbound events into. Held so
    /// `connect_input` can mint a producer handle per connection, and
    /// `disconnect_input` can retire the port it fed.
    ports: Arc<HardwareMidiInputs>,
    backend: Box<dyn MidiEndpoints>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let open = self
            .open
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, input) in open.inputs.drain() {
            input.close(&self.ports);
        }
    }
}

/// What is currently connected.
#[derive(Default)]
struct Open {
    /// Live input connections, keyed by endpoint.
    inputs: HashMap<EndpointId, OpenInput>,
    /// The single output sink, if one is connected.
    output: Option<OpenOutput>,
}

/// One open input: the driver connection, and the port it feeds.
struct OpenInput {
    /// Dropping it closes the driver side, which is the whole reason it is
    /// held rather than discarded.
    connection: Box<dyn InputConnection>,
    /// The manager port this connection's producer handle feeds.
    port: PortId,
    /// So a caller can report what is connected without re-enumerating (which
    /// would miss a device that has since gone).
    name: String,
}

impl OpenInput {
    /// Closes the connection, then retires its port.
    ///
    /// In that order: dropping the connection stops the driver (on ALSA it
    /// joins the pump thread, which drops the producer handle; on macOS the
    /// handle can outlive this, and anything it pushes is discarded with the
    /// ring). The port's slot is then free for the next connection, which is
    /// what keeps a hot-plug cycle from growing the port set by one ring each
    /// time.
    fn close(self, ports: &HardwareMidiInputs) {
        drop(self.connection);
        ports.remove_input_port(self.port);
    }
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

struct OpenOutput {
    id: EndpointId,
    name: String,
    sink: Box<dyn MidiOut>,
}

impl core::fmt::Debug for MidiSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let open = self.inner.open();
        f.debug_struct("MidiSession")
            .field("inputs", &open.inputs.len())
            .field("output", &open.output.as_ref().map(|o| &o.name))
            .finish_non_exhaustive()
    }
}

impl MidiSession {
    /// A session over this platform's backend.
    pub fn new(ports: Arc<HardwareMidiInputs>) -> Self {
        Self::with_backend(backend::active(), ports)
    }

    /// A session over a supplied backend.
    ///
    /// This is what makes the session testable: a fake [`MidiEndpoints`] drives
    /// every connect/disconnect path deterministically, on any platform,
    /// including one with no MIDI hardware. Without it a test can assert only
    /// "does not panic".
    pub fn with_backend(backend: Box<dyn MidiEndpoints>, ports: Arc<HardwareMidiInputs>) -> Self {
        Self {
            inner: Arc::new(Inner {
                open: Mutex::new(Open::default()),
                ports,
                backend,
            }),
        }
    }

    /// The ring manager inbound events land in. The audio thread polls this.
    pub fn ports(&self) -> &Arc<HardwareMidiInputs> {
        &self.inner.ports
    }

    // --- Enumeration ---

    /// Endpoints that can send MIDI to this engine, as of now.
    ///
    /// A fresh snapshot per call, in the backend's own order — device lists go
    /// stale on hot-plug, so nothing here is cached.
    pub fn inputs(&self) -> Vec<EndpointInfo> {
        self.inner.backend.inputs()
    }

    /// Endpoints this engine can send MIDI to, as of now. A fresh snapshot per
    /// call, as [`inputs`](Self::inputs) is.
    pub fn outputs(&self) -> Vec<EndpointInfo> {
        self.inner.backend.outputs()
    }

    /// The shared name match for every `*_by_name` method here: **case-
    /// insensitive substring, first hit wins**, in the backend's enumeration
    /// order. See [`connect_input_by_name`](MidiSession::connect_input_by_name)
    /// for what that means for a caller.
    fn find(list: Vec<EndpointInfo>, name: &str) -> Option<EndpointInfo> {
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
    /// [`Error::MidiDevice`] when `id` names no current input — the shape a
    /// stale id takes after a hot-plug. Otherwise whatever the backend raises
    /// while opening the port.
    pub fn connect_input(&self, id: EndpointId) -> Result<()> {
        let mut open = self.inner.open();
        if open.inputs.contains_key(&id) {
            return Ok(());
        }

        let info = self
            .inner
            .backend
            .inputs()
            .into_iter()
            .find(|e| e.id == id)
            .ok_or_else(|| Error::MidiDevice(format!("no MIDI input with id {}", id.raw())))?;

        // One ring per connection, and its only producer goes to the backend.
        let producer = self.inner.ports.open_input_port(info.name.as_str());
        let port = producer.port_id();
        let connection = match self.inner.backend.open_input(id, producer) {
            Ok(connection) => connection,
            Err(e) => {
                // The backend dropped the handle; the port it would have fed
                // must not outlive the failed open.
                self.inner.ports.remove_input_port(port);
                return Err(e);
            }
        };

        open.inputs.insert(
            id,
            OpenInput {
                connection,
                port,
                name: info.name,
            },
        );
        Ok(())
    }

    /// Opens the first input endpoint whose name contains `name`.
    ///
    /// # Matching
    ///
    /// **Case-insensitive substring, first hit wins.** `name` is lowercased and
    /// tested with `contains` against each endpoint's lowercased name, in the
    /// order [`inputs`](Self::inputs) returns them — which is the backend's
    /// enumeration order, not sorted and not stable across a hot-plug. So
    /// `"iac"` matches `"IAC Driver Bus 1"`, and a `name` matching two devices
    /// silently picks whichever the OS listed first. Connect by
    /// [`EndpointId`] when that matters.
    ///
    /// # Errors
    ///
    /// [`Error::MidiDevice`] when nothing matches. A device that is absent is an
    /// error here, never a silent no-op.
    pub fn connect_input_by_name(&self, name: &str) -> Result<()> {
        let info = Self::find(self.inputs(), name)
            .ok_or_else(|| Error::MidiDevice(format!("no MIDI input matching '{name}'")))?;
        self.connect_input(info.id)
    }

    /// Closes an input endpoint and retires the port its events landed in.
    /// Closing one that is not open is a no-op.
    ///
    /// Events still queued in that port are discarded with it.
    pub fn disconnect_input(&self, id: EndpointId) {
        let mut open = self.inner.open();
        if let Some(input) = open.inputs.remove(&id) {
            input.close(&self.inner.ports);
        }
    }

    /// Closes one open input whose name contains `name`, matched
    /// case-insensitively.
    ///
    /// Closes **at most one**, and the search runs over a `HashMap` of open
    /// connections — so unlike the `connect_*_by_name` pair there is no
    /// "first" to speak of: a `name` matching two open inputs closes an
    /// arbitrary one of them. Pass an [`EndpointId`] to
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
                .find(|(_, input)| input.name.to_lowercase().contains(&needle))
                .map(|(id, _)| *id)
        };
        if let Some(id) = id {
            self.disconnect_input(id);
        }
    }

    /// Closes every open input, retiring each one's port.
    pub fn disconnect_all_inputs(&self) {
        let mut open = self.inner.open();
        for (_, input) in open.inputs.drain() {
            input.close(&self.inner.ports);
        }
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
            .map(|input| input.name.clone())
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
    /// [`Error::MidiDevice`] when `id` names no current output, plus whatever
    /// the backend raises while opening.
    pub fn connect_output(&self, id: EndpointId) -> Result<()> {
        let info = self
            .inner
            .backend
            .outputs()
            .into_iter()
            .find(|e| e.id == id)
            .ok_or_else(|| Error::MidiDevice(format!("no MIDI output with id {}", id.raw())))?;

        let sink = self.inner.backend.open_output(id)?;
        let mut open = self.inner.open();
        open.output = Some(OpenOutput {
            id,
            name: info.name,
            sink,
        });
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
    /// [`Error::MidiDevice`] when nothing matches.
    pub fn connect_output_by_name(&self, name: &str) -> Result<()> {
        let info = Self::find(self.outputs(), name)
            .ok_or_else(|| Error::MidiDevice(format!("no MIDI output matching '{name}'")))?;
        self.connect_output(info.id)
    }

    /// Closes the output endpoint, if one is open.
    pub fn disconnect_output(&self) {
        self.inner.open().output = None;
    }

    /// Whether an output is open.
    #[must_use]
    pub fn is_output_connected(&self) -> bool {
        self.inner.open().output.is_some()
    }

    /// The open output's name, if any.
    pub fn output_device_name(&self) -> Option<String> {
        self.inner.open().output.as_ref().map(|o| o.name.clone())
    }

    /// The open output's endpoint, if any.
    pub fn output_endpoint(&self) -> Option<EndpointId> {
        self.inner.open().output.as_ref().map(|o| o.id)
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
            Some(out) => out.sink.queue(events),
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
