//! [`Devices`]: the MIDI devices present, kept current while it lives.
//!
//! A backend watches the OS on a thread of its own and reports each change to a
//! [`ChangeSink`]. The sink feeds the pure [`DeviceTable`] and hands the event
//! it returns to every [`DeviceEvents`] subscribed, so `list` reads one table
//! and no read is an OS call. The watch stops, and the backend's thread with
//! it, when the last [`Devices`] and the last [`DeviceEvents`] are dropped.
//!
//! A subscriber's queue is unbounded, and exists only while its
//! [`DeviceEvents`] does: an app that wants no events subscribes to none.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tutti_midi_device::{
    DeviceEvent, DeviceId, DeviceInfo, DeviceTable, Direction, EndpointId, Identity, OsChange,
    OsDevice, Resolved,
};
use tutti_midi_types::MidiOut;

use crate::backend;
use crate::capability::{EndpointInfo, UmpCapability};
use crate::endpoints::{InputConnection, MidiEndpoints, Watch};
use crate::error::{Error, Result};
use crate::{HardwareMidiInputs, PortId};

/// Why a device watch ended on its own.
#[derive(Clone, Debug, thiserror::Error)]
pub enum WatchFailure {
    /// The backend's watch thread panicked.
    #[error("the MIDI device watch thread panicked")]
    Panicked,
    /// The OS refused the watch, or it broke: the error is the OS's own, and
    /// its text is shown as it is.
    #[error(transparent)]
    Failed(Arc<Error>),
}

impl From<Error> for WatchFailure {
    fn from(error: Error) -> Self {
        Self::Failed(Arc::new(error))
    }
}

/// Why [`DeviceEvents::recv_timeout`] returned no event.
#[derive(Clone, Debug, thiserror::Error)]
pub enum RecvTimeoutError {
    /// No change came in time. The watch is still running.
    #[error("no MIDI device change before the timeout")]
    Timeout,
    /// The watch ended and every event it reported has been taken: nothing
    /// more will arrive.
    #[error("the MIDI device watch ended: {0}")]
    Ended(#[source] WatchFailure),
}

#[derive(Default)]
struct State {
    table: DeviceTable,
    subscribers: Vec<Weak<Queue>>,
    /// Why the watch ended, if it did. The table is then the last it saw.
    failure: Option<WatchFailure>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
}

/// One subscriber's inbox.
#[derive(Default)]
struct Queue {
    inbox: Mutex<Inbox>,
    woken: Condvar,
}

#[derive(Default)]
struct Inbox {
    events: VecDeque<DeviceEvent>,
    ended: Option<WatchFailure>,
}

impl Queue {
    fn inbox(&self) -> MutexGuard<'_, Inbox> {
        self.inbox.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, event: DeviceEvent) {
        self.inbox().events.push_back(event);
        self.woken.notify_all();
    }

    fn end(&self, failure: WatchFailure) {
        self.inbox().ended.get_or_insert(failure);
        self.woken.notify_all();
    }
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where a backend's watch reports what the OS says. Cloneable and `Send`: the
/// backend moves one onto its thread.
#[derive(Clone)]
pub struct ChangeSink {
    shared: Arc<Shared>,
}

impl ChangeSink {
    /// Reports one change. A change that alters nothing is dropped; one that
    /// does reaches every subscriber and wakes a waiting
    /// [`DeviceEvents::recv_timeout`].
    pub fn send(&self, change: OsChange) {
        self.send_all([change]);
    }

    /// Reports `changes` in order under one hold of the state, so no
    /// [`Devices::list`] or [`Devices::subscribe`] sees between them: a device
    /// reported gone and present again is never missing from the list.
    pub fn send_all(&self, changes: impl IntoIterator<Item = OsChange>) {
        let mut state = self.shared.state();
        for change in changes {
            if let Some(event) = state.table.apply(change) {
                // A subscriber that has dropped leaves the list.
                state.subscribers.retain(|queue| {
                    queue
                        .upgrade()
                        .inspect(|queue| queue.push(event.clone()))
                        .is_some()
                });
            }
        }
    }
}

impl ChangeSink {
    /// Records that the watch has ended without being dropped, with the reason.
    /// The list is frozen from here; every subscriber's
    /// [`recv_timeout`](DeviceEvents::recv_timeout) says so once it has taken
    /// what was queued, and so does the session's `Phase::Poll`.
    pub fn fail(&self, reason: impl Into<WatchFailure>) {
        let reason = reason.into();
        tracing::warn!("MIDI device watch ended: {reason}");
        let mut state = self.shared.state();
        let reason = state.failure.get_or_insert(reason).clone();
        state
            .subscribers
            .iter()
            .filter_map(Weak::upgrade)
            .for_each(|queue| queue.end(reason.clone()));
    }

    /// Lists `devices` as already present: they enter the table and queue no
    /// event, so a `Devices` that has just opened has nothing to poll. A backend
    /// seeds once, before any [`send`](Self::send).
    pub fn seed(&self, devices: Vec<OsDevice>) {
        let mut state = self.shared.state();
        for device in devices {
            state.table.apply(OsChange::Present(device));
        }
    }
}

impl core::fmt::Debug for ChangeSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ChangeSink").finish_non_exhaustive()
    }
}

/// What a backend's thread keeps between looks at the OS: the handles it has
/// reported, so a handle that has dropped out of a fresh enumeration is
/// reported gone.
///
/// An OS reuses a handle: ALSA hands a new client the number of one that just
/// exited, so two looks that straddle both see one handle. A handle that now
/// names another identity is a different device, and so is one the OS says
/// departed (`departed`): each is reported gone before it is reported present
/// again. A report is applied under one hold of the state. Where the identity is
/// the name (ALSA), a rename is therefore a removal and an addition, and closes a port held on the device; where it
/// survives a rename (CoreMIDI's unique id), the device is `Changed`.
#[cfg_attr(
    not(any(target_os = "macos", all(target_os = "linux", alsa_ump))),
    allow(
        dead_code,
        reason = "only the OS backends with a thread report through it"
    )
)]
#[derive(Debug)]
pub(crate) struct Reporter {
    sink: ChangeSink,
    known: BTreeMap<EndpointId, Identity>,
}

#[cfg_attr(
    not(any(target_os = "macos", all(target_os = "linux", alsa_ump))),
    allow(
        dead_code,
        reason = "only the OS backends with a thread report through it"
    )
)]
impl Reporter {
    pub(crate) fn new(sink: ChangeSink) -> Self {
        Self {
            sink,
            known: BTreeMap::new(),
        }
    }

    /// Lists `present` as the devices there already were.
    pub(crate) fn seed(&mut self, present: Vec<OsDevice>) {
        self.known = present
            .iter()
            .map(|d| (d.handle, d.identity.clone()))
            .collect();
        self.sink.seed(present);
    }

    /// The handles of `present` that are known under another identity.
    pub(crate) fn changed_in(&self, present: &[OsDevice]) -> BTreeSet<EndpointId> {
        present
            .iter()
            .filter(|d| {
                self.known
                    .get(&d.handle)
                    .is_some_and(|known| *known != d.identity)
            })
            .map(|d| d.handle)
            .collect()
    }

    /// Reports `present` as the whole device list: every device in it, and the
    /// departure of each known one that is not, that has another identity now,
    /// or that `departed` names. The table drops what repeats.
    pub(crate) fn report(&mut self, present: Vec<OsDevice>, departed: impl Fn(EndpointId) -> bool) {
        let now: BTreeMap<EndpointId, Identity> = present
            .iter()
            .map(|d| (d.handle, d.identity.clone()))
            .collect();
        let gone = self
            .known
            .iter()
            .filter(|&(handle, identity)| now.get(handle) != Some(identity) || departed(*handle))
            .map(|(handle, _)| OsChange::Gone(*handle));
        let changes: Vec<_> = gone
            .chain(present.into_iter().map(OsChange::Present))
            .collect();
        self.sink.send_all(changes);
        self.known = now;
    }
}

/// The devices present now, and a way to open one.
///
/// Cheap to clone: every clone is the same list over the same watch.
///
/// ```no_run
/// # fn main() -> Result<(), tutti_midi_hardware::Error> {
/// use std::time::Duration;
/// use tutti_midi_hardware::Devices;
///
/// let devices = Devices::open()?;
/// let events = devices.subscribe();
/// for device in events.initial() {
///     println!("{}: {}", device.id, device.name);
/// }
/// while let Ok(event) = events.recv_timeout(Duration::from_secs(5)) {
///     println!("{event:?}");
/// }
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Devices {
    inner: Arc<Inner>,
}

struct Inner {
    // Declared first, so the watch stops before the backend it came from goes.
    _watch: Box<dyn Watch>,
    backend: Box<dyn MidiEndpoints>,
    shared: Arc<Shared>,
}

impl core::fmt::Debug for Devices {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Devices")
            .field("devices", &self.list().len())
            .finish_non_exhaustive()
    }
}

impl Devices {
    /// Lists this platform's devices and starts watching them; the watch stops
    /// when the last `Devices` and [`DeviceEvents`] over it are dropped.
    ///
    /// Where the build has no MIDI backend (Windows, or alsa-lib older than
    /// 1.2.10) the list is empty and stays so, as enumeration is; opening a
    /// device reports [`Error::Unsupported`].
    ///
    /// # Errors
    ///
    /// The platform error from starting the watch, such as a thread that could
    /// not be spawned.
    pub fn open() -> Result<Self> {
        Self::open_over(backend::active())
    }

    /// As [`open`](Self::open), over a supplied backend: a test's
    /// `test_support::FakeBackend`.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn open_over(backend: Box<dyn MidiEndpoints>) -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let watch = backend.watch(ChangeSink {
            shared: Arc::clone(&shared),
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                _watch: watch,
                backend,
                shared,
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner.shared.state()
    }

    /// The devices present now, ordered by id.
    #[must_use]
    pub fn list(&self) -> Vec<DeviceInfo> {
        self.state().table.list()
    }

    /// Subscribes to the changes from now on. Each subscriber has a queue of
    /// its own, so one that reads does not take another's events.
    ///
    /// [`DeviceEvents::initial`] is the list at the same instant, which no
    /// event of the subscription repeats: there is no gap or overlap between
    /// reading the list and subscribing, whichever a caller does first.
    ///
    /// The queue is unbounded and a subscriber that never reads keeps one small
    /// event per change until it is dropped. Drop a `DeviceEvents` that is no
    /// longer read.
    pub fn subscribe(&self) -> DeviceEvents {
        let mut state = self.state();
        let queue = Arc::new(Queue::default());
        if let Some(failure) = &state.failure {
            queue.end(failure.clone());
        }
        state.subscribers.push(Arc::downgrade(&queue));
        DeviceEvents {
            initial: state.table.list(),
            queue,
            _devices: self.clone(),
        }
    }

    /// Opens `id` for input. Its events land in a ring of `ports`, drained by
    /// the audio thread; the returned [`OpenInput`] holds the connection and the
    /// ring's port, and dropping it closes both.
    ///
    /// Control thread only: it takes a ring from `ports` (which publishes) and
    /// the backend may start a thread.
    ///
    /// Opening an id that is already open opens a second connection to the
    /// device, and every event arrives twice. `Devices` does not track what is
    /// open; the session does, and its `connect_input` is idempotent.
    ///
    /// The list is not held while the OS opens the handle. A device that left
    /// in between, and whose handle the OS gave to another at once, opens that
    /// other one; the `Removed` that follows is how a holder finds out.
    ///
    /// # Errors
    ///
    /// [`Error::NoDevice`] when `id` is not present, [`Error::WrongDirection`]
    /// when it is not an input, or the platform error from opening it. A failed
    /// open leaves no port in `ports`.
    pub fn open_input(&self, id: &DeviceId, ports: &Arc<HardwareMidiInputs>) -> Result<OpenInput> {
        let (info, handle) = self.present(id, Direction::is_input)?;
        let producer = ports.open_input_port(info.name.as_str());
        let mut input = OpenInput {
            connection: None,
            port: producer.port_id(),
            ports: Arc::clone(ports),
            info,
        };
        // On an error `input` drops here, and takes the port with it.
        input.connection = Some(self.inner.backend.open_input(handle, producer)?);
        Ok(input)
    }

    /// Opens `id` for output.
    ///
    /// # Errors
    ///
    /// [`Error::NoDevice`] when `id` is not present, [`Error::WrongDirection`]
    /// when it is not an output, or the platform error from opening it.
    pub fn open_output(&self, id: &DeviceId) -> Result<OpenOutput> {
        let (info, handle) = self.present(id, Direction::is_output)?;
        Ok(OpenOutput {
            info,
            sink: self.inner.backend.open_output(handle)?,
        })
    }

    /// The listing and OS handle of `id`, if it is present and `wanted` takes
    /// its direction.
    fn present(
        &self,
        id: &DeviceId,
        wanted: fn(Direction) -> bool,
    ) -> Result<(DeviceInfo, EndpointId)> {
        let state = self.state();
        let (info, handle) = state
            .table
            .get(id)
            .zip(state.table.handle(id))
            .ok_or_else(|| Error::NoDevice(id.clone()))?;
        wanted(info.direction)
            .then(|| (info.clone(), handle))
            .ok_or_else(|| Error::WrongDirection {
                id: id.clone(),
                direction: info.direction,
            })
    }

    /// What `saved`, a device from an earlier run, names among the devices
    /// present now. Bind a [`Resolved::Exact`] without asking; a
    /// [`Resolved::SameName`] may be another unit of the same name (ALSA
    /// identifies a device by its name only). See
    /// [`DeviceTable::resolve`](tutti_midi_device::DeviceTable::resolve) for the
    /// rule. A host that holds a session reaches this through
    /// `MidiSession::devices`.
    #[must_use]
    pub fn resolve(&self, saved: &DeviceInfo) -> Resolved {
        self.state().table.resolve(saved)
    }

    /// What the OS says `id` can carry, asked of the backend now. `None` when
    /// `id` is not present.
    #[must_use]
    pub fn capability(&self, id: &DeviceId) -> Option<UmpCapability> {
        let handle = self.state().table.handle(id)?;
        let backend = &self.inner.backend;
        backend
            .inputs()
            .into_iter()
            .chain(backend.outputs())
            .find(|e| e.id == handle)
            .map(|e| e.capability)
    }
}

/// The changes to the device list since [`Devices::subscribe`], oldest first.
///
/// One consumer's view: it is not `Clone`, and another subscription's reads
/// leave it alone. Dropping it unsubscribes at once.
pub struct DeviceEvents {
    initial: Vec<DeviceInfo>,
    queue: Arc<Queue>,
    // Keeps the watch running while events can still be expected from it.
    _devices: Devices,
}

impl Drop for DeviceEvents {
    fn drop(&mut self) {
        let mine = Arc::as_ptr(&self.queue);
        self._devices
            .state()
            .subscribers
            .retain(|queue| queue.as_ptr() != mine);
    }
}

impl core::fmt::Debug for DeviceEvents {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceEvents").finish_non_exhaustive()
    }
}

impl DeviceEvents {
    /// The devices present when this was subscribed to; every later change is
    /// an event.
    #[must_use]
    pub fn initial(&self) -> &[DeviceInfo] {
        &self.initial
    }

    /// The changes queued now, oldest first, taking each as it is iterated.
    /// Never blocks.
    pub fn try_iter(&self) -> impl Iterator<Item = DeviceEvent> + '_ {
        std::iter::from_fn(|| self.queue.inbox().events.pop_front())
    }

    /// The oldest change, waiting up to `timeout` for one.
    ///
    /// # Errors
    ///
    /// [`RecvTimeoutError::Timeout`] when none came, and
    /// [`RecvTimeoutError::Ended`] once the watch has ended and everything it
    /// reported has been taken.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<DeviceEvent, RecvTimeoutError> {
        let (mut inbox, _) = self
            .queue
            .woken
            .wait_timeout_while(self.queue.inbox(), timeout, |inbox| {
                inbox.events.is_empty() && inbox.ended.is_none()
            })
            .unwrap_or_else(PoisonError::into_inner);
        inbox.events.pop_front().ok_or_else(|| {
            inbox
                .ended
                .clone()
                .map_or(RecvTimeoutError::Timeout, RecvTimeoutError::Ended)
        })
    }

    /// Why the watch ended on its own, if it did: the list is then the last it
    /// saw and nothing more arrives. `None` while the watch runs.
    #[must_use]
    pub fn failure(&self) -> Option<WatchFailure> {
        self.queue.inbox().ended.clone()
    }
}

/// An input held open: the OS connection to a device, and the ring port its
/// events land in.
///
/// Dropping it closes both, connection first, so nothing is still pushing into
/// a ring that is being retired. Drop it on a control thread only: closing the
/// connection joins the backend's thread and retiring the port publishes. A device that leaves does not close it: the
/// holder learns from a [`DeviceEvent::Removed`] naming [`id`](Self::id) and
/// drops it (the session does, in `Phase::Poll`).
pub struct OpenInput {
    /// `None` only while [`Devices::open_input`] is still opening it.
    connection: Option<Box<dyn InputConnection>>,
    port: PortId,
    ports: Arc<HardwareMidiInputs>,
    info: DeviceInfo,
}

impl core::fmt::Debug for OpenInput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpenInput")
            .field("device", &self.info.id)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl OpenInput {
    /// The device this input is open on.
    #[must_use]
    pub fn id(&self) -> &DeviceId {
        &self.info.id
    }

    /// The device's name when it was opened.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// Whether the connection is still delivering. `false` once the backend's
    /// reader has ended on an error (a warning is logged): the device is
    /// still listed, but nothing will arrive until the input is reopened.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.connection.as_ref().is_some_and(|c| c.is_alive())
    }

    /// The port its events land in, for the ring manager's
    /// `set_port_active` and the drain's tag.
    #[must_use]
    pub fn port_id(&self) -> PortId {
        self.port
    }
}

/// An output held open: the device, and the sink that sends to it. Dropping it
/// closes the connection.
pub struct OpenOutput {
    info: DeviceInfo,
    sink: Box<dyn MidiOut>,
}

impl core::fmt::Debug for OpenOutput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpenOutput")
            .field("device", &self.info.id)
            .finish_non_exhaustive()
    }
}

impl OpenOutput {
    /// The device this output is open on.
    #[must_use]
    pub fn id(&self) -> &DeviceId {
        &self.info.id
    }

    /// The device's name when it was opened.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.info.name
    }
}

impl MidiOut for OpenOutput {
    fn queue(&self, events: &[tutti_midi_types::ump::MidiEvent]) -> usize {
        self.sink.queue(events)
    }
}

impl Drop for OpenInput {
    fn drop(&mut self) {
        drop(self.connection.take());
        self.ports.remove_input_port(self.port);
    }
}

/// The devices of an enumeration, one per handle: an endpoint listed both ways
/// is [`Duplex`](Direction::Duplex).
pub(crate) fn os_devices(inputs: &[EndpointInfo], outputs: &[EndpointInfo]) -> Vec<OsDevice> {
    let mut devices: BTreeMap<EndpointId, OsDevice> = BTreeMap::new();
    let as_device = |e: &EndpointInfo, direction| OsDevice {
        handle: e.id,
        identity: e.identity.clone(),
        name: e.name.clone(),
        direction,
    };
    for e in inputs {
        devices.insert(e.id, as_device(e, Direction::Input));
    }
    for e in outputs {
        devices
            .entry(e.id)
            .and_modify(|d| d.direction = Direction::Duplex)
            .or_insert_with(|| as_device(e, Direction::Output));
    }
    devices.into_values().collect()
}

#[cfg(test)]
mod tests;
