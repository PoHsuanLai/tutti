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
//! The connection map is a plain `Mutex`, not an [`RtPublish`]: nothing here is
//! read from the audio thread, and `CLAUDE.md` names device enumeration in this
//! crate as a deliberate exception to the publish rule.
//!
//! [`RtPublish`]: tutti_midi_types::RtPublish
//! [`HardwareMidiInputs`]: crate::HardwareMidiInputs
//! [`InputProducerHandle`]: crate::InputProducerHandle

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::backend;
use crate::capability::{EndpointId, EndpointInfo};
use crate::endpoints::{InputConnection, MidiEndpoints};
use crate::error::{Error, Result};
use crate::{HardwareMidiInputs, PortId};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiOut;

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

struct OpenOutput {
    id: EndpointId,
    name: String,
    sink: Box<dyn MidiOut>,
}

impl core::fmt::Debug for MidiSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let open = self.inner.open.lock().unwrap();
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
        let mut open = self.inner.open.lock().unwrap();
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
        let producer = self.inner.ports.create_input_port(info.name.as_str());
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
        let mut open = self.inner.open.lock().unwrap();
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
            let open = self.inner.open.lock().unwrap();
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
        let mut open = self.inner.open.lock().unwrap();
        for (_, input) in open.inputs.drain() {
            input.close(&self.inner.ports);
        }
    }

    /// Whether any input is open.
    pub fn is_any_input_connected(&self) -> bool {
        !self.inner.open.lock().unwrap().inputs.is_empty()
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
            .open
            .lock()
            .unwrap()
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
        let mut open = self.inner.open.lock().unwrap();
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
        self.inner.open.lock().unwrap().output = None;
    }

    /// Whether an output is open.
    pub fn is_output_connected(&self) -> bool {
        self.inner.open.lock().unwrap().output.is_some()
    }

    /// The open output's name, if any.
    pub fn output_device_name(&self) -> Option<String> {
        self.inner
            .open
            .lock()
            .unwrap()
            .output
            .as_ref()
            .map(|o| o.name.clone())
    }

    /// The open output's endpoint, if any.
    pub fn output_endpoint(&self) -> Option<EndpointId> {
        self.inner
            .open
            .lock()
            .unwrap()
            .output
            .as_ref()
            .map(|o| o.id)
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
        let open = self.inner.open.lock().unwrap();
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
mod tests {
    use super::*;
    use crate::test_support::FakeBackend;
    use crate::InputProducerHandle;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The fake backend is in `crate::test_support` so `bevy-tutti` can test
    // its own session wiring against it: a headless CI box has no MIDI port.

    /// A session over the shared fake backend, plus its two counters.
    ///
    /// Returned as a tuple rather than the `FakeCounters` struct so each test
    /// names only the counter it uses — `(s, _, opens)` says at a glance that
    /// this one is about opens.
    fn session() -> (MidiSession, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (backend, counters) = FakeBackend::build();
        let ports = Arc::new(HardwareMidiInputs::new(64));
        (
            MidiSession::with_backend(backend, ports),
            counters.sent,
            counters.opens,
        )
    }

    fn note() -> MidiEvent {
        use tutti_midi_types::{MidiChannel, MidiGroup};
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
    }

    #[test]
    fn enumeration_passes_the_backend_through() {
        let (s, _, _) = session();
        assert_eq!(s.inputs().len(), 2);
        assert_eq!(s.outputs().len(), 2);
        assert_eq!(s.inputs()[0].name, "Keystep Pro");
    }

    #[test]
    fn connecting_an_input_makes_it_connected() {
        let (s, _, _) = session();
        assert!(!s.is_any_input_connected());
        s.connect_input(EndpointId::from_raw(1)).unwrap();
        assert!(s.is_any_input_connected());
        assert_eq!(s.connected_input_names(), vec!["Keystep Pro".to_string()]);
    }

    /// Reconnecting must not reopen the port — a second driver connection to one
    /// device duplicates every inbound event.
    #[test]
    fn connecting_an_open_input_is_idempotent() {
        let (s, _, opens) = session();
        s.connect_input(EndpointId::from_raw(1)).unwrap();
        s.connect_input(EndpointId::from_raw(1)).unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 1, "the port opens once");
        assert_eq!(s.connected_input_names().len(), 1);
    }

    #[test]
    fn by_name_matching_is_case_insensitive_substring() {
        let (s, _, _) = session();
        s.connect_input_by_name("keystep").unwrap();
        assert_eq!(s.connected_input_names(), vec!["Keystep Pro".to_string()]);
    }

    #[test]
    fn connecting_an_absent_device_is_an_error_not_a_silent_noop() {
        let (s, _, _) = session();
        assert!(s.connect_input_by_name("nonexistent").is_err());
        assert!(s.connect_input(EndpointId::from_raw(999)).is_err());
        assert!(!s.is_any_input_connected());
    }

    #[test]
    fn disconnecting_closes_only_the_named_input() {
        let (s, _, _) = session();
        s.connect_input(EndpointId::from_raw(1)).unwrap();
        s.connect_input(EndpointId::from_raw(2)).unwrap();
        s.disconnect_input_by_name("iac");
        assert_eq!(s.connected_input_names(), vec!["Keystep Pro".to_string()]);
        s.disconnect_all_inputs();
        assert!(!s.is_any_input_connected());
    }

    /// **The reason the sink is an `Option`.** With nothing connected, `send`
    /// accepts nothing and says so. A sink that queued regardless and reported
    /// the loss only at `debug!` would let a caller drain into silence forever
    /// with no way to count it.
    #[test]
    fn sending_with_no_output_accepts_nothing() {
        let (s, sent, _) = session();
        assert_eq!(s.send(&[note()]), 0);
        assert_eq!(sent.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn sending_reaches_a_connected_output() {
        let (s, sent, _) = session();
        s.connect_output_by_name("synth a").unwrap();
        assert!(s.is_output_connected());
        assert_eq!(s.output_device_name(), Some("Synth A".to_string()));
        assert_eq!(s.send(&[note(), note()]), 2);
        assert_eq!(sent.load(Ordering::SeqCst), 2);
    }

    /// A connected device that refuses everything must report `0`, not
    /// `events.len()`.
    ///
    /// The failure this pins: returning the *attempted* count whenever an output
    /// is open makes a device rejecting every event indistinguishable from one
    /// accepting them all. Catching that needs both halves — a `MidiOut::queue`
    /// that can express refusal, and a fake sink with an `accepts` budget.
    ///
    /// Note it asserts `is_output_connected()` too: without that, this would
    /// also pass for a session that had silently dropped its output, which is a
    /// different bug wearing the same number.
    #[test]
    fn a_refusing_output_reports_zero_accepted() {
        let (backend, counters) = FakeBackend::build_accepting(0);
        let sent = counters.sent;
        let ports = Arc::new(HardwareMidiInputs::new(64));
        let s = MidiSession::with_backend(backend, ports);
        s.connect_output_by_name("synth a").unwrap();

        assert!(s.is_output_connected(), "the output is open");
        assert_eq!(s.send(&[note(), note()]), 0, "the device accepted none");
        assert_eq!(
            sent.load(Ordering::SeqCst),
            2,
            "both were offered to the sink"
        );
    }

    /// A partial accept reports the prefix that landed, not the batch size.
    #[test]
    fn a_partial_accept_reports_what_landed() {
        let (backend, _counters) = FakeBackend::build_accepting(1);
        let ports = Arc::new(HardwareMidiInputs::new(64));
        let s = MidiSession::with_backend(backend, ports);
        s.connect_output_by_name("synth a").unwrap();

        assert_eq!(s.send(&[note(), note(), note()]), 1);
    }

    #[test]
    fn disconnecting_output_stops_delivery() {
        let (s, sent, _) = session();
        s.connect_output_by_name("synth a").unwrap();
        s.send(&[note()]);
        s.disconnect_output();
        assert!(!s.is_output_connected());
        assert_eq!(s.send(&[note()]), 0);
        assert_eq!(
            sent.load(Ordering::SeqCst),
            1,
            "only the pre-disconnect one"
        );
    }

    /// Connecting a second output replaces the first rather than fanning out —
    /// a session sends to one destination.
    #[test]
    fn connecting_a_second_output_replaces_the_first() {
        let (s, _, _) = session();
        s.connect_output(EndpointId::from_raw(10)).unwrap();
        s.connect_output(EndpointId::from_raw(11)).unwrap();
        assert_eq!(s.output_endpoint(), Some(EndpointId::from_raw(11)));
        assert_eq!(s.output_device_name(), Some("IAC Bus 1".to_string()));
    }

    /// The `MidiOut` impl must route to the same place as the inherent `send`,
    /// or a caller reaching the session through the trait would silently take a
    /// different path.
    #[test]
    fn the_midi_out_impl_routes_to_the_open_output() {
        let (s, sent, _) = session();
        s.connect_output_by_name("synth a").unwrap();
        MidiOut::queue(&s, &[note()]);
        assert_eq!(sent.load(Ordering::SeqCst), 1);
    }

    /// Repeated connect/disconnect cycles keep the port set bounded: a
    /// disconnect retires its port, and the reconnect takes its slot back.
    ///
    /// Device 2 stays connected throughout, so device 1's slot is an interior
    /// one on every cycle after the first.
    ///
    /// Mutation: `OpenInput::close` dropping the connection without
    /// `remove_input_port` (the old behaviour) → every cycle appends a port and
    /// the list is 101 long at the end.
    #[test]
    fn hot_plug_cycles_keep_the_port_set_bounded() {
        let (s, _, opens) = session();
        s.connect_input(EndpointId::from_raw(1)).unwrap();
        s.connect_input(EndpointId::from_raw(2)).unwrap();
        for cycle in 0..100 {
            s.disconnect_input(EndpointId::from_raw(1));
            assert_eq!(s.ports().list_input_ports().len(), 1, "cycle {cycle}");
            s.connect_input(EndpointId::from_raw(1)).unwrap();
            assert_eq!(s.ports().list_input_ports().len(), 2, "cycle {cycle}");
            assert_eq!(s.ports().slot_count(), 2, "cycle {cycle}");
        }
        assert_eq!(
            opens.load(Ordering::SeqCst),
            102,
            "each cycle really reopened"
        );

        s.disconnect_all_inputs();
        assert!(s.ports().list_input_ports().is_empty());
        assert_eq!(s.ports().slot_count(), 0, "the set shrinks back to nothing");
    }

    /// A backend whose connections, and itself, log their drop, and which can
    /// be told to refuse every open.
    struct LoggingBackend {
        log: Arc<Mutex<Vec<&'static str>>>,
        refuse: bool,
    }

    struct LoggingConnection {
        log: Arc<Mutex<Vec<&'static str>>>,
        _producer: InputProducerHandle,
    }

    impl InputConnection for LoggingConnection {}

    impl Drop for LoggingConnection {
        fn drop(&mut self) {
            self.log.lock().unwrap().push("connection");
        }
    }

    impl Drop for LoggingBackend {
        fn drop(&mut self) {
            self.log.lock().unwrap().push("backend");
        }
    }

    impl MidiEndpoints for LoggingBackend {
        fn inputs(&self) -> Vec<EndpointInfo> {
            vec![crate::test_support::endpoint(1, "Logged")]
        }
        fn outputs(&self) -> Vec<EndpointInfo> {
            Vec::new()
        }
        fn open_input(
            &self,
            _id: EndpointId,
            producer: InputProducerHandle,
        ) -> Result<Box<dyn InputConnection>> {
            if self.refuse {
                return Err(Error::MidiDevice("refused".into()));
            }
            Ok(Box::new(LoggingConnection {
                log: Arc::clone(&self.log),
                _producer: producer,
            }))
        }
        fn open_output(&self, _id: EndpointId) -> Result<Box<dyn MidiOut>> {
            Err(Error::MidiDevice("no outputs".into()))
        }
    }

    fn logging_session(refuse: bool) -> (MidiSession, Arc<Mutex<Vec<&'static str>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let backend = LoggingBackend {
            log: Arc::clone(&log),
            refuse,
        };
        let ports = Arc::new(HardwareMidiInputs::new(16));
        (MidiSession::with_backend(Box::new(backend), ports), log)
    }

    /// Dropping the session closes its inputs before the backend they were
    /// opened on, and retires their ports from a manager that outlives it.
    ///
    /// Mutations: deleting `impl Drop for Inner` and declaring `backend`
    /// before `open` (the old field order) → the log reads
    /// `["backend", "connection"]`; deleting only the `Drop` impl → the
    /// order holds by field order, but the shared manager still lists the
    /// session's port.
    #[test]
    fn dropping_the_session_closes_inputs_then_the_backend_and_retires_ports() {
        let (s, log) = logging_session(false);
        s.connect_input(EndpointId::from_raw(1)).unwrap();
        let ports = Arc::clone(s.ports());
        assert_eq!(ports.list_input_ports().len(), 1);

        drop(s);
        assert_eq!(*log.lock().unwrap(), vec!["connection", "backend"]);
        assert!(
            ports.list_input_ports().is_empty(),
            "the session's port does not outlive it in a shared manager"
        );
    }

    /// An open the backend refuses leaves no port behind.
    ///
    /// Mutation: dropping the `remove_input_port` in `connect_input`'s error
    /// arm → the manager lists one dead port per failed attempt.
    #[test]
    fn a_refused_open_leaves_no_port() {
        let (s, _log) = logging_session(true);
        for _ in 0..3 {
            assert!(s.connect_input(EndpointId::from_raw(1)).is_err());
        }
        assert!(!s.is_any_input_connected());
        assert!(s.ports().list_input_ports().is_empty());
        assert_eq!(s.ports().slot_count(), 0);
    }
}
