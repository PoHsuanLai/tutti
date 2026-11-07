use super::*;
use crate::InputProducerHandle;
use crate::test_support::FakeBackend;
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
    use tutti_ump::{MidiChannel, MidiGroup};
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
