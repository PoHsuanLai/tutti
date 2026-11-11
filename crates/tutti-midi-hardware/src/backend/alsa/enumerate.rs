//! Walking the sequencer's clients and ports.

use std::os::raw::c_int;

use crate::capability::{EndpointInfo, UmpCapability};
use tutti_alsa_ffi::{CAP_READ, CAP_SUBS_READ, CAP_SUBS_WRITE, CAP_WRITE, Client, PortEntry};
use tutti_midi_device::{EndpointId, Identity};

/// Which direction an endpoint is being listed for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// Ports we can read from — they must be subscribe-readable.
    Input,
    /// Ports we can write to.
    Output,
}

impl Direction {
    /// The capability bits a port needs to be usable in this direction.
    ///
    /// Both the raw and the `SUBS_` bit: a port that is readable but not
    /// *subscribe*-readable cannot be connected to, so listing it would offer
    /// the user a device that fails to open.
    fn required_caps(self) -> u32 {
        match self {
            Direction::Input => CAP_READ | CAP_SUBS_READ,
            Direction::Output => CAP_WRITE | CAP_SUBS_WRITE,
        }
    }
}

/// Pack `(client, port)` into an [`EndpointId`].
///
/// ALSA addresses are a pair, and both halves are stable for as long as the
/// device is present — which is what an id has to be. Packing rather than
/// indexing means a stale id fails to open instead of silently resolving to
/// whatever now sits at that position.
pub(super) fn pack_id(client: c_int, port: c_int) -> EndpointId {
    EndpointId::from_raw(((client as u64) << 32) | (port as u32 as u64))
}

/// Unpack an [`EndpointId`] back into `(client, port)`; `None` if `pack_id`
/// could not have made it.
///
/// An `EndpointId` is a bare number any caller can build with `from_raw`, and a
/// sequencer address is a pair of bytes. A value with bits above either byte was
/// not minted here (a CoreMIDI id, a corrupted saved one): truncating it would
/// subscribe to whichever port its low bits name.
pub(super) fn unpack_id(id: EndpointId) -> Option<(c_int, c_int)> {
    let raw = id.raw();
    let (client, port) = (raw >> 32, raw & 0xFFFF_FFFF);
    let byte = u64::from(u8::MAX);
    (client <= byte && port <= byte).then_some((client as c_int, port as c_int))
}

#[cfg(test)]
mod tests;

/// Every port on the system usable in `direction`.
///
/// Skips this client's own ports — offering the engine's own port back as a
/// device invites a feedback loop, and it is never what a user meant.
///
/// Names are `"Client: Port"`, matching how `aconnect -l` presents them, unless
/// the port name already starts with the client's.
pub(super) fn endpoints(seq: &Client, direction: Direction) -> Vec<EndpointInfo> {
    walk(seq, [direction]).lists.into_iter().flatten().collect()
}

/// The inputs and the outputs from **one** walk, so a port is read once. Two
/// walks can see a port being created (the kernel lists it before it sets its
/// capabilities) in different states and report a duplex as an output.
///
/// `None` when the walk stopped on a query error, so the lists may be short:
/// a short list reported as a look would remove devices that are still there.
pub(super) fn both(seq: &Client) -> Option<(Vec<EndpointInfo>, Vec<EndpointInfo>)> {
    let Walk {
        lists: [inputs, outputs],
        complete,
    } = walk(seq, [Direction::Input, Direction::Output]);
    complete.then_some((inputs, outputs))
}

/// What a walk found.
struct Walk<const N: usize> {
    /// One list per direction asked for, in order.
    lists: [Vec<EndpointInfo>; N],
    /// Whether every query ended with `-ENOENT`, the end of the list.
    complete: bool,
}

/// One list per entry of `directions`, in order.
fn walk<const N: usize>(seq: &Client, directions: [Direction; N]) -> Walk<N> {
    let mut out: [Vec<EndpointInfo>; N] = std::array::from_fn(|_| Vec::new());
    // `-ENOENT` is the last client or port. A client that exits mid-walk answers
    // `-ENXIO` (`snd_seq_ioctl_query_next_port`): not the end of its ports, so
    // the walk is short and is taken again.
    let found = seq.list_ports();
    for port in found.ports {
        let caps = port.caps;
        let listed = |d: Direction| caps & d.required_caps() == d.required_caps();
        if !directions.iter().any(|&d| listed(d)) {
            continue;
        }
        let endpoint = endpoint_of(port);
        for (list, &d) in out.iter_mut().zip(&directions) {
            if listed(d) {
                list.push(endpoint.clone());
            }
        }
    }
    Walk {
        lists: out,
        complete: found.complete,
    }
}

/// The endpoint a listed port is.
///
/// A client with UMP block info is a genuine UMP endpoint and speaks MIDI 2.0;
/// one without is legacy, arriving through the kernel's converter. The
/// distinction is real and worth recording: a per-note controller sent to a
/// converted legacy port is dropped in translation, not carried.
fn endpoint_of(port: PortEntry) -> EndpointInfo {
    let PortEntry {
        client,
        client_name,
        port: number,
        name: port_name,
        ump,
        ..
    } = port;
    EndpointInfo {
        id: pack_id(client, number),
        identity: Identity::alsa(client_name.as_str(), port_name.as_str()),
        // "Client: Port" is how `aconnect -l` presents them, and a
        // bare port name ("MIDI 1") is ambiguous across devices.
        name: if port_name.starts_with(&client_name) {
            port_name
        } else {
            format!("{client_name}: {port_name}")
        },
        capability: if ump {
            UmpCapability::midi2()
        } else {
            UmpCapability::midi1()
        },
    }
}
