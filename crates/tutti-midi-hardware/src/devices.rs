//! [`Devices`]: the MIDI devices present, kept current while it lives.
//!
//! A backend watches the OS on a thread of its own and reports each change to a
//! [`ChangeSink`]. The sink feeds the pure [`DeviceTable`] and queues the event
//! it returns, so `list`, `poll` and `wait` read one table and never an OS call.
//! Dropping [`Devices`] drops the watch, which stops the backend's thread.
//!
//! The queue is unbounded: an app that never polls keeps one small event per
//! hot-plug.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tutti_midi_device::{
    DeviceEvent, DeviceId, DeviceInfo, DeviceTable, Direction, EndpointId, Identity, OsChange,
    OsDevice,
};
use tutti_midi_types::MidiOut;

use crate::InputProducerHandle;
use crate::backend;
use crate::capability::{EndpointInfo, UmpCapability};
use crate::endpoints::{InputConnection, MidiEndpoints, Watch};
use crate::error::{Error, Result};

#[derive(Default)]
struct State {
    table: DeviceTable,
    pending: VecDeque<DeviceEvent>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    woken: Condvar,
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
    /// does wakes a [`Devices::wait`].
    pub fn send(&self, change: OsChange) {
        let mut state = self.shared.state();
        if let Some(event) = state.table.apply(change) {
            state.pending.push_back(event);
            self.shared.woken.notify_all();
        }
    }
}

impl ChangeSink {
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
/// reported and what each was, so a handle that has dropped out of a fresh
/// enumeration is reported gone.
///
/// An OS reuses a handle: ALSA hands a new client the number of one that just
/// exited, and two looks that straddle both see one handle. A handle that now
/// names another identity is a different device, so it is reported gone first.
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

    /// Reports `present` as the whole device list: every device in it, and the
    /// departure of each known one that is not. The table drops what repeats.
    pub(crate) fn report(&mut self, present: Vec<OsDevice>) {
        let now: BTreeMap<EndpointId, Identity> = present
            .iter()
            .map(|d| (d.handle, d.identity.clone()))
            .collect();
        for (handle, identity) in &self.known {
            if now.get(handle) != Some(identity) {
                self.sink.send(OsChange::Gone(*handle));
            }
        }
        for device in present {
            self.sink.send(OsChange::Present(device));
        }
        self.known = now;
    }
}

/// The devices present, and the changes since they were last read.
///
/// ```no_run
/// # fn main() -> Result<(), tutti_midi_hardware::Error> {
/// use std::time::Duration;
/// use tutti_midi_hardware::Devices;
///
/// let devices = Devices::open()?;
/// for device in devices.list() {
///     println!("{}: {}", device.id, device.name);
/// }
/// for event in devices.wait(Duration::from_secs(5)) {
///     println!("{event:?}");
/// }
/// # Ok(()) }
/// ```
pub struct Devices {
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
    /// Lists this platform's devices and starts watching them; dropping the
    /// value stops the watch.
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
        Self::with_backend(backend::active())
    }

    /// As [`open`](Self::open), over a supplied backend: a test's
    /// `test_support::FakeBackend`.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn with_backend(backend: Box<dyn MidiEndpoints>) -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let watch = backend.watch(ChangeSink {
            shared: Arc::clone(&shared),
        })?;
        Ok(Self {
            _watch: watch,
            backend,
            shared,
        })
    }

    /// The devices present now, ordered by id.
    #[must_use]
    pub fn list(&self) -> Vec<DeviceInfo> {
        self.shared.state().table.list()
    }

    /// The changes since the last `poll` or `wait`, oldest first.
    pub fn poll(&self) -> Vec<DeviceEvent> {
        self.shared.state().pending.drain(..).collect()
    }

    /// Blocks until there is a change or `timeout` passes, then returns the
    /// changes as [`poll`](Self::poll) does: empty on a timeout.
    pub fn wait(&self, timeout: Duration) -> Vec<DeviceEvent> {
        let state = self.shared.state();
        let (mut state, _) = self
            .shared
            .woken
            .wait_timeout_while(state, timeout, |s| s.pending.is_empty())
            .unwrap_or_else(PoisonError::into_inner);
        state.pending.drain(..).collect()
    }

    /// Opens `id` for input, delivering events into `producer`; the connection
    /// must be held, as [`MidiEndpoints::open_input`] says.
    ///
    /// # Errors
    ///
    /// [`Error::MidiDevice`] when `id` is not a present input, or the platform
    /// error from opening it.
    pub fn open_input(
        &self,
        id: &DeviceId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        let handle = self.handle(id, Direction::is_input, "input")?;
        self.backend.open_input(handle, producer)
    }

    /// Opens `id` for output.
    ///
    /// # Errors
    ///
    /// [`Error::MidiDevice`] when `id` is not a present output, or the platform
    /// error from opening it.
    pub fn open_output(&self, id: &DeviceId) -> Result<Box<dyn MidiOut>> {
        let handle = self.handle(id, Direction::is_output, "output")?;
        self.backend.open_output(handle)
    }

    fn handle(
        &self,
        id: &DeviceId,
        wanted: fn(Direction) -> bool,
        what: &str,
    ) -> Result<EndpointId> {
        let state = self.shared.state();
        match state.table.get(id) {
            Some(info) if wanted(info.direction) => state.table.handle(id),
            _ => None,
        }
        .ok_or_else(|| Error::MidiDevice(format!("no MIDI {what} {id}")))
    }

    /// What the OS says `id` can carry, asked of the backend now. `None` when
    /// `id` is not present.
    #[must_use]
    pub fn capability(&self, id: &DeviceId) -> Option<UmpCapability> {
        let handle = self.shared.state().table.handle(id)?;
        let (inputs, outputs) = (self.backend.inputs(), self.backend.outputs());
        inputs
            .into_iter()
            .chain(outputs)
            .find(|e| e.id == handle)
            .map(|e| e.capability)
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
