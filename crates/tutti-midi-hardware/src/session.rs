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
//! Everything here is control-thread. Inbound **events** never pass through this
//! type — a backend pushes them straight into the
//! [`InputProducerHandle`] of a [`HardwareMidiInputs`] port, which places each
//! arrival on a frame and pushes it into the device's event inlet, read by the
//! graph's ingest node. What [`poll`](MidiSession::poll) steps is only the
//! devices' *talk about themselves*: the UMP Stream negotiation of each open
//! input, read from the port's stream ring.
//!
//! The connection map is a plain `Mutex`, not an [`ArcLend`]: nothing here is
//! read from the audio thread.
//!
//! [`ArcLend`]: tutti_rt::ArcLend
//! [`HardwareMidiInputs`]: crate::HardwareMidiInputs
//! [`InputProducerHandle`]: crate::InputProducerHandle

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tutti_graph::UmpAt;

use crate::HardwareMidiInputs;
use crate::backend;
use crate::ci::{CiChange, CiPeerInfo, Heard};
use crate::devices::{Devices, OpenInput, OpenOutput};
use crate::endpoints::MidiEndpoints;
use crate::error::{Error, Result};
use crate::stream::{DeviceStream, Negotiation};
use tutti_midi_device::{DeviceId, DeviceInfo, Learned, SavedDevice};
use tutti_midi_runtime::{DeviceInlets, MidiOut, Outgoing};
use tutti_ump::{DeviceIdentity, StreamConfig};

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
    /// The identity MIDI-CI is enabled with; none while it is off, which is
    /// the default ([`MidiSession::enable_ci`]).
    ci_identity: Mutex<Option<DeviceIdentity>>,
}

/// What is currently connected.
#[derive(Default)]
struct Open {
    /// Live input connections, keyed by device.
    inputs: HashMap<DeviceId, Held>,
    /// The single output sink, if one is connected.
    output: Option<OpenOutput>,
}

/// One open input, and the UMP Stream negotiation of its device.
///
/// The negotiation drops first: it holds the output it asks on, and the input
/// it reads its answers from goes after.
struct Held {
    stream: Negotiation,
    /// Whether the port's input edge is copying SysEx7 packets to the stream
    /// ring, for the Identity Request or MIDI-CI.
    hearing: bool,
    /// How many stream-ring packets the port had lost when it was last read.
    lost_seen: u64,
    input: OpenInput,
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

/// Which device a name or an id picks: what
/// [`connect_input`](MidiSession::connect_input), its output and disconnect
/// forms take, from a `&`[`DeviceId`] or a `&str`.
///
/// # Matching a name
///
/// **Case-insensitive substring, first hit wins.** The name is lowercased and
/// tested with `contains` against each device's lowercased name, in the order
/// [`inputs`](MidiSession::inputs) or [`outputs`](MidiSession::outputs)
/// returns them: by id. So `"iac"` matches `"IAC Driver Bus 1"`, and a name
/// matching two devices picks the one with the lowest id. Connect by
/// [`DeviceId`] when that matters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceQuery(Query);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Query {
    Id(DeviceId),
    Name(String),
}

impl From<&DeviceId> for DeviceQuery {
    fn from(id: &DeviceId) -> Self {
        Self(Query::Id(id.clone()))
    }
}

impl From<&str> for DeviceQuery {
    fn from(name: &str) -> Self {
        Self(Query::Name(name.to_owned()))
    }
}

impl DeviceQuery {
    /// The device this picks among `list`, or the error naming what was
    /// searched. An id is taken as it is (the open reports a stale one).
    fn pick(&self, list: Vec<DeviceInfo>) -> Result<DeviceId> {
        match &self.0 {
            Query::Id(id) => Ok(id.clone()),
            Query::Name(name) => {
                let needle = name.to_lowercase();
                match list
                    .iter()
                    .find(|d| d.name.to_lowercase().contains(&needle))
                {
                    Some(found) => Ok(found.id.clone()),
                    None => Err(Error::NoDeviceNamed {
                        name: name.clone(),
                        searched: list.into_iter().map(|d| d.name).collect(),
                    }),
                }
            }
        }
    }
}

/// The open output: what [`MidiSession::output`] reads of it in one look.
#[derive(Clone, Debug)]
pub struct OutputInfo {
    /// The device it is open on.
    pub id: DeviceId,
    /// The device's name when it was opened.
    pub name: String,
    /// The Protocol and JR Timestamps it sends in: the OS's description of
    /// its device, or what the negotiation settled.
    pub config: StreamConfig,
}

/// What one [`MidiSession::poll`] found, for the device it names.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Polled {
    /// The device's UMP Stream negotiation settled or its settled state
    /// changed.
    /// (Boxed: an Endpoint is large.)
    Stream(DeviceId, Box<DeviceStream>),
    /// A MIDI-CI peer of the device was found, changed or lost.
    Ci(DeviceId, CiChange),
}

impl MidiSession {
    /// Opens a session over this platform's backend, its input ports slots of
    /// `inlets` (the session builds and owns the [`HardwareMidiInputs`] over
    /// them: [`ports`](Self::ports)).
    ///
    /// # Errors
    ///
    /// As [`Devices::open`].
    pub fn open(inlets: Arc<DeviceInlets>) -> Result<Self> {
        Self::open_over(backend::active(), inlets)
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
    pub fn open_over(backend: Box<dyn MidiEndpoints>, inlets: Arc<DeviceInlets>) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                open: Mutex::new(Open::default()),
                ports: Arc::new(HardwareMidiInputs::new(inlets)),
                devices: Devices::open_over(backend)?,
                ci_identity: Mutex::new(None),
            }),
        })
    }

    /// The devices present. A host that wants hot-plug subscribes
    /// ([`Devices::subscribe`]); the session's `Phase::Poll` has a
    /// subscription of its own.
    pub fn devices(&self) -> &Devices {
        &self.inner.devices
    }

    /// The input ports the session opened, and the inlets they are slots of
    /// ([`HardwareMidiInputs::inlets`]): the graph's owner takes the inlets'
    /// changes from it.
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

    // --- Input ---

    /// Opens an input endpoint, by [`DeviceId`] or by name (see
    /// [`DeviceQuery`]). Idempotent — connecting an already-open endpoint
    /// succeeds without reopening it, because a second driver connection to one
    /// device duplicates every inbound event.
    ///
    /// Each newly opened input gets its own port in [`ports`](Self::ports); the
    /// backend pushes into it from the driver's thread and the audio thread
    /// drains it. Events never pass through this type.
    ///
    /// The device's output, if it has one, is opened too, for discovery alone
    /// (the UMP Stream negotiation, or a MIDI 1.0 port's Identity Request, then
    /// MIDI-CI once enabled, all stepped by [`poll`](Self::poll)): it is not the session's output, which
    /// [`output`](Self::output) reports and [`connect_output`](Self::connect_output)
    /// chooses, and it closes with the input. An output that will not open
    /// leaves the input connected and the device as the OS describes it.
    ///
    /// # Errors
    ///
    /// [`Error::NoDeviceNamed`] when a name matches no input (listing the
    /// inputs it searched; an absent device is an error, never a silent
    /// no-op). [`Error::NoDevice`] when an id names no present device — the
    /// shape a stale id takes after a hot-plug — and [`Error::WrongDirection`]
    /// when it is not an input. Otherwise whatever the backend raises while
    /// opening the port.
    pub fn connect_input(&self, device: impl Into<DeviceQuery>) -> Result<()> {
        let id = device.into().pick(self.inputs())?;
        let mut open = self.inner.open();
        // An input whose reader died is replaced, not kept: it is silent.
        if open
            .inputs
            .get(&id)
            .is_some_and(|held| held.input.is_alive())
        {
            return Ok(());
        }
        if let Some(mut held) = open.inputs.remove(&id) {
            held.stream.close();
        }

        // One ring per connection, retired with it.
        let input = self.inner.devices.open_input(&id, &self.inner.ports)?;
        // A UMP Endpoint with an output is asked what it is by the next
        // `poll`. Its output is the negotiation's own, not the session's one
        // destination: asking must not wait for, or disturb, that choice.
        let os = self.inner.devices.capability(&id).unwrap_or_default();
        // A MIDI 1.0 port with an output is asked for its identity instead.
        let out = self.inner.devices.open_output(&id).ok();
        open.inputs.insert(
            id,
            Held {
                stream: Negotiation::new(os, out),
                hearing: false,
                lost_seen: 0,
                input,
            },
        );
        Ok(())
    }

    /// Closes an open input and retires the port its events landed in, by
    /// [`DeviceId`] or by name. Returns whether one was open; closing one that
    /// is not is a no-op.
    ///
    /// A name is matched against the **open** inputs' names, case-insensitively
    /// as a substring, and the lowest [`DeviceId`] among those matching is the
    /// one closed (at most one: use [`disconnect_all_inputs`](Self::disconnect_all_inputs)
    /// to close every one).
    ///
    /// Events still queued in that port are discarded with it. MIDI-CI devices
    /// running on it send an Invalidate MUID for their own MUID first (best
    /// effort), and no [`CiChange::Lost`] is reported: the caller closed it.
    /// Dropping the session sends nothing.
    pub fn disconnect_input(&self, device: impl Into<DeviceQuery>) -> bool {
        let mut open = self.inner.open();
        let id = match device.into().0 {
            Query::Id(id) => id,
            Query::Name(name) => {
                let needle = name.to_lowercase();
                let Some(id) = open
                    .inputs
                    .iter()
                    .filter(|(_, held)| held.input.name().to_lowercase().contains(&needle))
                    .map(|(id, _)| id)
                    .min()
                    .cloned()
                else {
                    return false;
                };
                id
            }
        };
        match open.inputs.remove(&id) {
            Some(mut held) => {
                held.stream.close();
                true
            }
            None => false,
        }
    }

    /// Closes every open input, retiring each one's port.
    pub fn disconnect_all_inputs(&self) {
        for (_, mut held) in self.inner.open().inputs.drain() {
            held.stream.close();
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
            .map(|held| held.input.name().to_owned())
            .collect()
    }

    // --- Output ---

    /// Opens an output endpoint, by [`DeviceId`] or by name (see
    /// [`DeviceQuery`]), replacing any currently open one.
    ///
    /// One at a time: a session sends to a single destination, so connecting a
    /// second output closes the first rather than fanning out.
    ///
    /// # Errors
    ///
    /// [`Error::NoDeviceNamed`] when a name matches no output,
    /// [`Error::NoDevice`] when an id names no present device,
    /// [`Error::WrongDirection`] when it is not an output, plus whatever the
    /// backend raises while opening.
    pub fn connect_output(&self, device: impl Into<DeviceQuery>) -> Result<()> {
        let id = device.into().pick(self.outputs())?;
        let mut output = self.inner.devices.open_output(&id)?;
        let mut open = self.inner.open();
        // A device whose input is held may have settled already.
        if let Some(stream) = open.inputs.get(&id).and_then(|held| held.stream.settled()) {
            output.set_config(stream.config());
        }
        open.output = Some(output);
        Ok(())
    }

    /// Closes the output endpoint, if one is open. Returns whether one was.
    pub fn disconnect_output(&self) -> bool {
        self.inner.open().output.take().is_some()
    }

    /// The open output, if one is: its device, name and the Protocol and JR
    /// Timestamps it sends in, as of this look.
    #[must_use]
    pub fn output(&self) -> Option<OutputInfo> {
        self.inner.open().output.as_ref().map(|o| OutputInfo {
            id: o.id().clone(),
            name: o.name().to_owned(),
            config: o.config(),
        })
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
            .is_some_and(|held| held.input.is_alive())
    }

    /// Steps the UMP Stream negotiation and the MIDI-CI of every open input at
    /// `now`: starts the ones connected since, hands each the Stream messages
    /// and SysEx its device sent, and sends what they answer or ask again.
    /// Returns, once each, the devices whose negotiation settled in this call
    /// and those whose settled state changed ([`Polled::Stream`]: a Stream
    /// Configuration Notification, or a Function Block of an Endpoint that did
    /// not declare them static), and the MIDI-CI peers that were found,
    /// changed or lost ([`Polled::Ci`]). The open output follows its device's
    /// Protocol and JR Timestamps.
    ///
    /// The stepping is done when this returns; the iterator only hands out what
    /// it found (a list built under the session's lock, which is not held
    /// while the caller reads it), so ignoring it loses nothing but the report.
    ///
    /// `now` is the caller's time: nothing here reads a clock or waits, so a
    /// device that never answers costs a few packets over a few seconds of
    /// polls and then reads as [`Unanswered`](crate::Basis::Unanswered). Call
    /// it from the control tick (the session's `Phase::Poll`); it only reads
    /// rings and sends.
    pub fn poll(&self, now: Instant) -> std::vec::IntoIter<Polled> {
        let mut polled = Vec::new();
        let ci = self.ci_identity();
        let mut open = self.inner.open();
        for (id, held) in &mut open.inputs {
            let slot = held.input.slot();
            // The copy of the SysEx7 packets is on before a request goes out
            // and before MIDI-CI starts, which a step decides a poll ahead
            // (`Ci::Idle`), and off once nothing listens.
            self.sync_hearing(held, ci.is_some());
            // The ring's loss is read either side of the drain (`Heard`).
            let lost = || {
                self.inner
                    .ports
                    .port_info(slot)
                    .map_or(0, |p| p.stream_lost)
            };
            let before = lost();
            let said: Vec<_> = self.inner.ports.drain_stream(slot).collect();
            let after = lost();
            let heard = Heard {
                packets: &said,
                lost_before: before > held.lost_seen,
                lost_during: after > before,
            };
            held.lost_seen = after;
            let stepped = held.stream.poll(now, &heard, ci);
            // Off the moment nothing listens, so no packet is kept for no one.
            self.sync_hearing(held, ci.is_some());
            if let Some(stream) = stepped.stream {
                polled.push(Polled::Stream(id.clone(), Box::new(stream)));
            }
            polled.extend(
                stepped
                    .ci
                    .into_iter()
                    .map(|change| Polled::Ci(id.clone(), change)),
            );
        }
        // The output the session holds sends in what its device settled.
        if let Some(out) = &mut open.output
            && let Some(stream) = polled.iter().rev().find_map(|p| match p {
                Polled::Stream(id, stream) if id == out.id() => Some(stream),
                _ => None,
            })
        {
            out.set_config(stream.config());
        }
        polled.into_iter()
    }

    /// Makes the port's copy of SysEx7 packets what the negotiation wants.
    fn sync_hearing(&self, held: &mut Held, ci: bool) {
        let slot = held.input.slot();
        let want = held.stream.hears_sysex(ci);
        if want && !held.hearing {
            held.hearing = self.inner.ports.hear_sysex(slot, true);
        } else if !want && held.hearing {
            self.inner.ports.hear_sysex(slot, false);
            held.hearing = false;
        }
    }

    /// Turns MIDI-CI on, announcing `identity` in Discovery. **It is off until
    /// this is called**: a Discovery carries a System Exclusive manufacturer
    /// id, tutti has none, and the one MIDI 1.0 sets aside for non-commercial use
    /// (`7D`) is not to be used on any product released to the public. So
    /// the app says who it is, with the id it registered. An identity under
    /// [`ManufacturerId::NON_COMMERCIAL`](tutti_ump::ManufacturerId::NON_COMMERCIAL)
    /// is for development and research only, never for a released product.
    ///
    /// It reaches every open input (a device on one starts at the next
    /// [`poll`](Self::poll), after any Identity Request in progress has ended)
    /// and every input opened later. Calling it again with the identity already
    /// in use changes nothing; with another, the running devices end as
    /// [`disable_ci`](Self::disable_ci) ends them (an Invalidate MUID for their
    /// own MUID, their peers lost) and start again under a new MUID.
    pub fn enable_ci(&self, identity: DeviceIdentity) {
        *self.ci_identity_lock() = Some(identity);
    }

    /// Turns MIDI-CI off: at the next [`poll`](Self::poll) every running device
    /// sends an Invalidate MUID for its own MUID (M2-101 §5.9 lists a shutdown
    /// among its reasons: its peers learn at once, not by silence), its peers
    /// are reported as lost, and the ports copy no more SysEx for it.
    pub fn disable_ci(&self) {
        *self.ci_identity_lock() = None;
    }

    /// The identity MIDI-CI is enabled with, or `None` while it is off.
    #[must_use]
    pub fn ci_identity(&self) -> Option<DeviceIdentity> {
        *self.ci_identity_lock()
    }

    fn ci_identity_lock(&self) -> std::sync::MutexGuard<'_, Option<DeviceIdentity>> {
        self.inner
            .ci_identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The MIDI-CI peers found on the open input `id`, by Group then MUID:
    /// empty when it is not open or nothing was found (the first
    /// [`poll`](Self::poll) after its negotiation settled starts Discovery,
    /// and a peer is found when its Reply arrives). The list is shared, not
    /// copied: it is rebuilt when a [`poll`](Self::poll) reports a change, and a
    /// read clones the pointer, so holding one keeps the peers as they were.
    #[must_use]
    pub fn ci_peers(&self, id: &DeviceId) -> Arc<[CiPeerInfo]> {
        self.inner
            .open()
            .inputs
            .get(id)
            .map_or_else(Arc::default, |held| held.stream.ci_peers())
    }

    /// What is known of the UMP Stream state of the open input `id`: `None`
    /// when it is not open, or its negotiation has not settled yet (the first
    /// [`poll`](Self::poll) starts it). Its
    /// [`config`](DeviceStream::config) is the Protocol and JR Timestamps in
    /// use.
    #[must_use]
    pub fn stream(&self, id: &DeviceId) -> Option<DeviceStream> {
        self.inner.open().inputs.get(id)?.stream.settled()
    }

    /// Returns what the open input `id` has said about itself so far: its Device
    /// Identity and its Product Instance Id, empty when it is not open or has
    /// said nothing yet (the answers arrive over the first seconds, and
    /// MIDI-CI's Endpoint Inquiry later: read again after the
    /// [`Polled`] that carries them).
    ///
    /// The UMP Endpoint, or a MIDI 1.0 port's Identity Reply, speaks for the
    /// whole device and goes first; a MIDI-CI peer (by Group then MUID) fills
    /// what it left. The OS's description carries neither, so a device that never
    /// answered says nothing. Compare it with a saved device's
    /// through [`Learned::compare`](tutti_midi_device::Learned::compare).
    #[must_use]
    pub fn learned(&self, id: &DeviceId) -> Learned {
        let open = self.inner.open();
        let Some(held) = open.inputs.get(id) else {
            return Learned::new();
        };
        let mut learned = held
            .stream
            .settled()
            .map_or_else(Learned::new, |stream| stream.said());
        for peer in held.stream.ci_peers().iter() {
            let peer = peer.peer();
            let mut said = Learned::new().with_identity(peer.discovery().identity);
            if let Some(instance) = peer.product_instance_id() {
                said = said.with_instance_id(*instance);
            }
            learned = learned.or(said);
        }
        learned
    }

    /// Returns the open input `id` as an app saves it: the listing's row and what
    /// [`learned`](Self::learned) says now, `None` when it is not open or no
    /// longer listed. Save it once the device has spoken; restore it with
    /// [`Devices::resolve`](crate::Devices::resolve) on
    /// [`SavedDevice::info`] and check it with [`SavedDevice::confirm`].
    #[must_use]
    pub fn saved(&self, id: &DeviceId) -> Option<SavedDevice> {
        let info = self
            .inner
            .devices
            .list()
            .into_iter()
            .find(|d| d.id == *id)?;
        self.is_input_connected(id)
            .then(|| SavedDevice::new(info, self.learned(id)))
    }

    /// Sends events to the open output, in the wire form of its device (see
    /// [`OpenOutput`]).
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
    pub fn send(&self, events: &[UmpAt]) -> usize {
        self.send_timed(&Outgoing::now_batch(events))
    }

    /// [`send`](Self::send) with a time for each event: the device is handed
    /// each at its [`SendAt`](tutti_midi_runtime::SendAt) (the backend
    /// schedules it natively), after the conversion to its wire form.
    pub fn send_timed(&self, events: &[Outgoing]) -> usize {
        let mut open = self.inner.open();
        match open.output.as_mut() {
            Some(out) => out.queue_timed(events),
            None => 0,
        }
    }
}

/// Sending through the session is sending to its open output.
///
/// The thin trait impl over the inherent [`send`](MidiSession::send), which
/// keeps the accepted count — the same idiom `MidiSender` uses.
impl MidiOut for MidiSession {
    fn queue(&self, events: &[UmpAt]) -> usize {
        self.send(events)
    }

    fn queue_timed(&self, events: &[Outgoing]) -> usize {
        self.send_timed(events)
    }
}

#[cfg(test)]
mod tests;
