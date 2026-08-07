//! [`MidiSession`] — what is connected, and the sink to reach it.
//!
//! Replaces `MidiIo`. Same job, three differences that matter:
//!
//! 1. **It owns no driver code.** Everything OS-specific is behind
//!    [`MidiEndpoints`], so this file has no `#[cfg]` at all.
//! 2. **The output sink is `Option<Box<dyn MidiOut>>`, absent when nothing is
//!    connected.** `MidiIo::send` queued whether or not a port was open, so the
//!    caller had to pre-check `is_output_connected()` — and one that forgot
//!    dropped events into a `debug!` forever. An unconnected sink is now
//!    *absent*, so a caller cannot accidentally send into nothing.
//! 3. **No background threads.** `MidiIo` spawned an input thread (to own
//!    `midir` connection handles) and an output thread (because `midir`'s send
//!    blocks). Neither is needed: a native-UMP send is a non-blocking syscall,
//!    and connections are held right here.
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
//! [`RtPublish`]: tutti_midi_types::tutti_types::RtPublish
//! [`HardwareMidiInputs`]: crate::core::HardwareMidiInputs

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::core::backend;
use crate::core::capability::{EndpointId, EndpointInfo};
use crate::core::endpoints::{InputConnection, MidiEndpoints};
use crate::core::error::{Error, Result};
use crate::core::{HardwareMidiInputs, InputProducerHandle};
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

struct Inner {
    backend: Box<dyn MidiEndpoints>,
    /// Ring manager the backend pushes inbound events into. Held so
    /// `connect_input` can mint a producer handle per connection.
    ports: Arc<HardwareMidiInputs>,
    open: Mutex<Open>,
}

/// What is currently connected.
#[derive(Default)]
struct Open {
    /// Live input connections, keyed by endpoint. Dropping a value closes its
    /// port, which is the whole reason they are held rather than discarded.
    inputs: HashMap<EndpointId, Box<dyn InputConnection>>,
    /// Names, parallel to `inputs`, so a caller can report what is connected
    /// without re-enumerating (which would miss a device that has since gone).
    input_names: HashMap<EndpointId, String>,
    /// The single output sink, if one is connected.
    output: Option<OpenOutput>,
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
    /// including one with no MIDI hardware. `MidiIo`'s tests could only assert
    /// "does not panic" for exactly this reason.
    pub fn with_backend(backend: Box<dyn MidiEndpoints>, ports: Arc<HardwareMidiInputs>) -> Self {
        Self {
            inner: Arc::new(Inner {
                backend,
                ports,
                open: Mutex::new(Open::default()),
            }),
        }
    }

    /// The ring manager inbound events land in. The audio thread polls this.
    pub fn ports(&self) -> &Arc<HardwareMidiInputs> {
        &self.inner.ports
    }

    // --- Enumeration ---

    /// Endpoints that can send us MIDI, as of now.
    pub fn inputs(&self) -> Vec<EndpointInfo> {
        self.inner.backend.inputs()
    }

    /// Endpoints we can send MIDI to, as of now.
    pub fn outputs(&self) -> Vec<EndpointInfo> {
        self.inner.backend.outputs()
    }

    /// Find an endpoint by case-insensitive substring, as the old
    /// `connect_*_by_name` did.
    fn find(list: Vec<EndpointInfo>, name: &str) -> Option<EndpointInfo> {
        let needle = name.to_lowercase();
        list.into_iter()
            .find(|e| e.name.to_lowercase().contains(&needle))
    }

    // --- Input ---

    /// Open an input endpoint. Idempotent — connecting an already-open endpoint
    /// succeeds without reopening it.
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

        let producer = self.producer_for(&info.name);
        let connection = self.inner.backend.open_input(id, producer)?;

        open.inputs.insert(id, connection);
        open.input_names.insert(id, info.name);
        Ok(())
    }

    /// Open the first input endpoint whose name contains `name`.
    pub fn connect_input_by_name(&self, name: &str) -> Result<()> {
        let info = Self::find(self.inputs(), name)
            .ok_or_else(|| Error::MidiDevice(format!("no MIDI input matching '{name}'")))?;
        self.connect_input(info.id)
    }

    /// Mint a ring + producer handle for a newly connected input.
    fn producer_for(&self, name: &str) -> InputProducerHandle {
        let port_index = self.inner.ports.create_input_port(name);
        self.inner
            .ports
            .get_input_producer_handle(port_index)
            .expect("port was just created")
    }

    /// Close an input endpoint. Closing one that is not open is a no-op.
    pub fn disconnect_input(&self, id: EndpointId) {
        let mut open = self.inner.open.lock().unwrap();
        open.inputs.remove(&id);
        open.input_names.remove(&id);
    }

    /// Close the first open input whose name contains `name`.
    pub fn disconnect_input_by_name(&self, name: &str) {
        let needle = name.to_lowercase();
        let id = {
            let open = self.inner.open.lock().unwrap();
            open.input_names
                .iter()
                .find(|(_, n)| n.to_lowercase().contains(&needle))
                .map(|(id, _)| *id)
        };
        if let Some(id) = id {
            self.disconnect_input(id);
        }
    }

    /// Close every open input.
    pub fn disconnect_all_inputs(&self) {
        let mut open = self.inner.open.lock().unwrap();
        open.inputs.clear();
        open.input_names.clear();
    }

    /// Whether any input is open.
    pub fn is_any_input_connected(&self) -> bool {
        !self.inner.open.lock().unwrap().inputs.is_empty()
    }

    /// The names of every open input.
    pub fn connected_input_names(&self) -> Vec<String> {
        self.inner
            .open
            .lock()
            .unwrap()
            .input_names
            .values()
            .cloned()
            .collect()
    }

    // --- Output ---

    /// Open an output endpoint, replacing any currently open one.
    ///
    /// One at a time, as before: a session sends to a single destination.
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

    /// Open the first output endpoint whose name contains `name`.
    pub fn connect_output_by_name(&self, name: &str) -> Result<()> {
        let info = Self::find(self.outputs(), name)
            .ok_or_else(|| Error::MidiDevice(format!("no MIDI output matching '{name}'")))?;
        self.connect_output(info.id)
    }

    /// Close the output endpoint, if one is open.
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

    /// Send events to the open output.
    ///
    /// Returns how many the device **accepted**: `0` when nothing is connected,
    /// and `< events.len()` when the endpoint refused part of the batch. That is
    /// the number a caller needs in order to count what was lost.
    ///
    /// It reported `events.len()` for anything sent while an output was open,
    /// which made "the device refused every event" indistinguishable from
    /// success — the backends log a refusal at `debug!` and there was no other
    /// channel for it. The count now comes from the sink, so the two differ.
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
    use crate::core::capability::UmpCapability;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A backend with a fixed device list and no OS behind it.
    ///
    /// This is what makes every path below deterministic and platform-free —
    /// the thing `MidiIo`'s tests could not do, which is why they asserted only
    /// "does not panic".
    struct FakeBackend {
        inputs: Vec<EndpointInfo>,
        outputs: Vec<EndpointInfo>,
        sent: Arc<AtomicUsize>,
        opens: Arc<AtomicUsize>,
        /// How many events per batch the sink it hands out will accept.
        accepts: usize,
    }

    struct FakeConn(EndpointId);
    impl InputConnection for FakeConn {
        fn endpoint(&self) -> EndpointId {
            self.0
        }
    }

    /// A sink that counts what it was handed and accepts `accepts` of each
    /// batch.
    ///
    /// `accepts` is what lets a test express a device that refuses — the shape
    /// `MidiOut`'s old `()` return made unrepresentable, and therefore the shape
    /// no test could catch `send` getting wrong.
    struct FakeSink {
        sent: Arc<AtomicUsize>,
        accepts: usize,
    }
    impl MidiOut for FakeSink {
        fn queue(&self, events: &[MidiEvent]) -> usize {
            self.sent.fetch_add(events.len(), Ordering::SeqCst);
            self.accepts.min(events.len())
        }
    }

    fn endpoint(raw: u64, name: &str) -> EndpointInfo {
        EndpointInfo {
            id: EndpointId::from_raw(raw),
            name: name.to_string(),
            capability: UmpCapability::midi2(),
        }
    }

    impl FakeBackend {
        /// The backend plus the two counters its fakes bump: events sent, and
        /// ports opened. Its sink accepts everything.
        fn build() -> (Box<dyn MidiEndpoints>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            Self::build_accepting(usize::MAX)
        }

        /// As [`build`](Self::build), but the sink accepts at most `accepts`
        /// events per batch — `0` models a device refusing everything.
        fn build_accepting(
            accepts: usize,
        ) -> (Box<dyn MidiEndpoints>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let sent = Arc::new(AtomicUsize::new(0));
            let opens = Arc::new(AtomicUsize::new(0));
            let b = FakeBackend {
                inputs: vec![endpoint(1, "Keystep Pro"), endpoint(2, "IAC Bus 1")],
                outputs: vec![endpoint(10, "Synth A"), endpoint(11, "IAC Bus 1")],
                sent: sent.clone(),
                opens: opens.clone(),
                accepts,
            };
            (Box::new(b), sent, opens)
        }
    }

    impl MidiEndpoints for FakeBackend {
        fn inputs(&self) -> Vec<EndpointInfo> {
            self.inputs.clone()
        }
        fn outputs(&self) -> Vec<EndpointInfo> {
            self.outputs.clone()
        }
        fn open_input(
            &self,
            id: EndpointId,
            _producer: InputProducerHandle,
        ) -> Result<Box<dyn InputConnection>> {
            if !self.inputs.iter().any(|e| e.id == id) {
                return Err(Error::MidiDevice("no such input".into()));
            }
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeConn(id)))
        }
        fn open_output(&self, id: EndpointId) -> Result<Box<dyn MidiOut>> {
            if !self.outputs.iter().any(|e| e.id == id) {
                return Err(Error::MidiDevice("no such output".into()));
            }
            Ok(Box::new(FakeSink {
                sent: self.sent.clone(),
                accepts: self.accepts,
            }))
        }
    }

    fn session() -> (MidiSession, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (backend, sent, opens) = FakeBackend::build();
        let ports = Arc::new(HardwareMidiInputs::new(64));
        (MidiSession::with_backend(backend, ports), sent, opens)
    }

    fn note() -> MidiEvent {
        use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
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
    /// accepts nothing and says so. `MidiIo::send` queued regardless and
    /// reported the loss at `debug!`, so a caller could drain into silence
    /// forever with no way to count it.
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
    /// This is the case `send` got wrong: it returned the *attempted* count
    /// whenever an output was open, so a device rejecting every event was
    /// indistinguishable from one accepting them all. No test could catch it
    /// while `MidiOut::queue` returned `()` — refusal was unrepresentable, which
    /// is why the fake sink gained an `accepts` field along with the fix.
    ///
    /// Note it asserts `is_output_connected()` too: without that, this would
    /// also pass for a session that had silently dropped its output, which is a
    /// different bug wearing the same number.
    #[test]
    fn a_refusing_output_reports_zero_accepted() {
        let (backend, sent, _) = FakeBackend::build_accepting(0);
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
        let (backend, _, _) = FakeBackend::build_accepting(1);
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
}
