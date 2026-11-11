//! Walking the sequencer's clients and ports.

use std::os::raw::c_int;

use super::client::{SeqClient, cstr};
use crate::capability::{EndpointInfo, UmpCapability};
use tutti_alsa_ump_sys as sys;
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
            Direction::Input => sys::SND_SEQ_PORT_CAP_READ | sys::SND_SEQ_PORT_CAP_SUBS_READ,
            Direction::Output => sys::SND_SEQ_PORT_CAP_WRITE | sys::SND_SEQ_PORT_CAP_SUBS_WRITE,
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
pub(super) fn endpoints(seq: &SeqClient, direction: Direction) -> Vec<EndpointInfo> {
    walk(seq, [direction]).lists.into_iter().flatten().collect()
}

/// The inputs and the outputs from **one** walk, so a port is read once. Two
/// walks can see a port being created (the kernel lists it before it sets its
/// capabilities) in different states and report a duplex as an output.
///
/// `None` when the walk stopped on a query error, so the lists may be short:
/// a short list reported as a look would remove devices that are still there.
pub(super) fn both(seq: &SeqClient) -> Option<(Vec<EndpointInfo>, Vec<EndpointInfo>)> {
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

/// `-ENOENT`: no client or port after this one.
const ENOENT: c_int = -2;

/// One list per entry of `directions`, in order.
fn walk<const N: usize>(seq: &SeqClient, directions: [Direction; N]) -> Walk<N> {
    let mut out: [Vec<EndpointInfo>; N] = std::array::from_fn(|_| Vec::new());
    let mut complete = true;
    let own_id = seq.id().unwrap_or(-1);

    let mut cinfo: *mut sys::snd_seq_client_info_t = std::ptr::null_mut();
    let mut pinfo: *mut sys::snd_seq_port_info_t = std::ptr::null_mut();

    // SAFETY: both out-pointers are valid; each allocation is freed below on
    // every path.
    unsafe {
        if sys::snd_seq_client_info_malloc(&mut cinfo) < 0 {
            return Walk {
                lists: out,
                complete: false,
            };
        }
        if sys::snd_seq_port_info_malloc(&mut pinfo) < 0 {
            sys::snd_seq_client_info_free(cinfo);
            return Walk {
                lists: out,
                complete: false,
            };
        }

        // `query_next_client` walks from the id set here; -1 starts at the first.
        sys::snd_seq_client_info_set_client(cinfo, -1);
        loop {
            let rc = sys::snd_seq_query_next_client(seq.raw(), cinfo);
            if rc < 0 {
                complete &= rc == ENOENT;
                break;
            }
            let client = sys::snd_seq_client_info_get_client(cinfo);
            // The kernel's own client (Timer, Announce) is not a device.
            if client == own_id || client == sys::SND_SEQ_CLIENT_SYSTEM {
                continue;
            }
            let client_name = cstr(sys::snd_seq_client_info_get_name(cinfo))
                .unwrap_or_else(|| format!("Client {client}"));

            sys::snd_seq_port_info_set_client(pinfo, client);
            sys::snd_seq_port_info_set_port(pinfo, -1);
            loop {
                let rc = sys::snd_seq_query_next_port(seq.raw(), pinfo);
                if rc < 0 {
                    // `-ENOENT` is the last port. A client that exits mid-walk
                    // answers `-ENXIO` (`snd_seq_ioctl_query_next_port`): not the end
                    // of its ports, so the walk is short and is taken again.
                    complete &= rc == ENOENT;
                    break;
                }
                let caps = sys::snd_seq_port_info_get_capability(pinfo);
                let listed = |d: Direction| caps & d.required_caps() == d.required_caps();
                if !directions.iter().any(|&d| listed(d)) {
                    continue;
                }
                let port = sys::snd_seq_port_info_get_port(pinfo);
                let port_name = cstr(sys::snd_seq_port_info_get_name(pinfo))
                    .unwrap_or_else(|| format!("Port {port}"));

                let endpoint = EndpointInfo {
                    id: pack_id(client, port),
                    identity: Identity::alsa(client_name.as_str(), port_name.as_str()),
                    // "Client: Port" is how `aconnect -l` presents them, and a
                    // bare port name ("MIDI 1") is ambiguous across devices.
                    name: if port_name.starts_with(&client_name) {
                        port_name
                    } else {
                        format!("{client_name}: {port_name}")
                    },
                    capability: capability_of(seq, client),
                };
                for (list, &d) in out.iter_mut().zip(&directions) {
                    if listed(d) {
                        list.push(endpoint.clone());
                    }
                }
            }
        }

        sys::snd_seq_port_info_free(pinfo);
        sys::snd_seq_client_info_free(cinfo);
    }

    Walk {
        lists: out,
        complete,
    }
}

/// What a client can carry.
///
/// A client with UMP block info is a genuine UMP endpoint and speaks MIDI 2.0;
/// one without is legacy, arriving through the kernel's converter. The
/// distinction is real and worth recording — a per-note controller sent to a
/// converted legacy port is dropped in translation, not carried.
fn capability_of(seq: &SeqClient, client: c_int) -> UmpCapability {
    let mut binfo: *mut sys::snd_ump_block_info_t = std::ptr::null_mut();
    // SAFETY: valid out-pointer; freed on both paths below.
    unsafe {
        if sys::snd_ump_block_info_malloc(&mut binfo) < 0 {
            return UmpCapability::midi1();
        }
        // Block 0 is enough to answer "is this a UMP endpoint at all". A
        // negative return means the client has no UMP blocks — i.e. legacy.
        let rc = sys::snd_seq_get_ump_block_info(seq.raw(), client, 0, binfo as *mut _);
        sys::snd_ump_block_info_free(binfo);
        if rc < 0 {
            UmpCapability::midi1()
        } else {
            UmpCapability::midi2()
        }
    }
}
