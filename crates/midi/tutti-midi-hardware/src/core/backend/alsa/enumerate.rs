//! Walking the sequencer's clients and ports.

use std::os::raw::c_int;

use super::client::{cstr, SeqClient};
use super::sys;
use crate::core::capability::{EndpointId, EndpointInfo, UmpCapability};

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

/// Unpack an [`EndpointId`] back into `(client, port)`.
pub(super) fn unpack_id(id: EndpointId) -> (c_int, c_int) {
    let raw = id.raw();
    ((raw >> 32) as c_int, (raw & 0xFFFF_FFFF) as c_int)
}

/// Every port on the system usable in `direction`.
///
/// Skips this client's own ports — offering the engine's own port back as a
/// device invites a feedback loop, and it is never what a user meant.
///
/// Names are `"Client: Port"`, matching how `aconnect -l` presents them, unless
/// the port name already starts with the client's.
pub(super) fn endpoints(seq: &SeqClient, direction: Direction) -> Vec<EndpointInfo> {
    let mut out = Vec::new();
    let own_id = seq.id().unwrap_or(-1);

    let mut cinfo: *mut sys::snd_seq_client_info_t = std::ptr::null_mut();
    let mut pinfo: *mut sys::snd_seq_port_info_t = std::ptr::null_mut();

    // SAFETY: both out-pointers are valid; each allocation is freed below on
    // every path.
    unsafe {
        if sys::snd_seq_client_info_malloc(&mut cinfo) < 0 {
            return out;
        }
        if sys::snd_seq_port_info_malloc(&mut pinfo) < 0 {
            sys::snd_seq_client_info_free(cinfo);
            return out;
        }

        // `query_next_client` walks from the id set here; -1 starts at the first.
        sys::snd_seq_client_info_set_client(cinfo, -1);
        while sys::snd_seq_query_next_client(seq.raw(), cinfo) >= 0 {
            let client = sys::snd_seq_client_info_get_client(cinfo);
            if client == own_id {
                continue;
            }
            let client_name = cstr(sys::snd_seq_client_info_get_name(cinfo))
                .unwrap_or_else(|| format!("Client {client}"));

            sys::snd_seq_port_info_set_client(pinfo, client);
            sys::snd_seq_port_info_set_port(pinfo, -1);
            while sys::snd_seq_query_next_port(seq.raw(), pinfo) >= 0 {
                let caps = sys::snd_seq_port_info_get_capability(pinfo);
                let needed = direction.required_caps();
                if caps & needed != needed {
                    continue;
                }
                let port = sys::snd_seq_port_info_get_port(pinfo);
                let port_name = cstr(sys::snd_seq_port_info_get_name(pinfo))
                    .unwrap_or_else(|| format!("Port {port}"));

                out.push(EndpointInfo {
                    id: pack_id(client, port),
                    // "Client: Port" is how `aconnect -l` presents them, and a
                    // bare port name ("MIDI 1") is ambiguous across devices.
                    name: if port_name.starts_with(&client_name) {
                        port_name
                    } else {
                        format!("{client_name}: {port_name}")
                    },
                    capability: capability_of(seq, client),
                });
            }
        }

        sys::snd_seq_port_info_free(pinfo);
        sys::snd_seq_client_info_free(cinfo);
    }

    out
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
