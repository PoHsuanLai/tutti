//! Walking the sequencer's clients and ports.

use std::ffi::CStr;
use std::os::raw::c_char;

use crate::client::Client;
use crate::records::{BlockRecord, ClientRecord, PortRecord};
use crate::sys;

/// `-ENOENT`: no client or port after this one, the end of a query.
const ENOENT: i32 = -2;

/// One port of another client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortEntry {
    /// The owning client's id.
    pub client: i32,
    /// The owning client's name, or `Client <id>` when it reports none.
    pub client_name: String,
    /// The port's number within its client.
    pub port: i32,
    /// The port's name, or `Port <n>` when it reports none.
    pub name: String,
    /// The port's `CAP_*` bits.
    pub caps: u32,
    /// Whether the client declares UMP function blocks: a genuine UMP endpoint
    /// rather than a legacy client reached through the kernel's converter.
    pub ump: bool,
}

/// What [`Client::list_ports`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ports {
    /// The ports found, in the sequencer's order.
    pub ports: Vec<PortEntry>,
    /// Whether every query ended with `-ENOENT`, the end of the list. A walk
    /// that stopped on another error (a client that exits mid-walk answers
    /// `-ENXIO`) is short, and is not a look.
    pub complete: bool,
}

/// A C string alsa-lib owns, copied. `None` for NULL: "the library reported no
/// name" and "the name is empty" are different, and the caller substitutes its
/// own label for the former.
fn owned(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: non-NULL, and alsa-lib's info getters return NUL-terminated
    // strings valid until the owning info record is freed or changed; copied
    // at once, so nothing borrows past that point.
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

impl Client {
    /// Every port of every client but this one and the kernel's own (Timer,
    /// Announce), which are not devices.
    ///
    /// Offering this client's own port back as a device invites a feedback
    /// loop, and it is never what a caller meant.
    #[must_use]
    pub fn list_ports(&self) -> Ports {
        let mut out = Ports {
            ports: Vec::new(),
            complete: true,
        };
        let (Ok(cinfo), Ok(pinfo)) = (ClientRecord::new(), PortRecord::new()) else {
            out.complete = false;
            return out;
        };
        let seq = self.raw();

        // SAFETY: `cinfo` is a live record; `query_next_client` walks from the
        // id set here, and -1 starts at the first.
        unsafe { sys::snd_seq_client_info_set_client(cinfo.as_ptr(), -1) };
        loop {
            // SAFETY: `seq` is open and `cinfo` is a live record.
            let rc = unsafe { sys::snd_seq_query_next_client(seq, cinfo.as_ptr()) };
            if rc < 0 {
                out.complete &= rc == ENOENT;
                break;
            }
            // SAFETY: `cinfo` is a live record the query just filled.
            let client = unsafe { sys::snd_seq_client_info_get_client(cinfo.as_ptr()) };
            if client == self.id() || client == sys::SND_SEQ_CLIENT_SYSTEM {
                continue;
            }
            // SAFETY: as above; the name is copied before the record changes.
            let client_name = owned(unsafe { sys::snd_seq_client_info_get_name(cinfo.as_ptr()) })
                .unwrap_or_else(|| format!("Client {client}"));

            // SAFETY: `pinfo` is a live record.
            unsafe {
                sys::snd_seq_port_info_set_client(pinfo.as_ptr(), client);
                sys::snd_seq_port_info_set_port(pinfo.as_ptr(), -1);
            }
            let mut ump = None;
            loop {
                // SAFETY: `seq` is open and `pinfo` is a live record.
                let rc = unsafe { sys::snd_seq_query_next_port(seq, pinfo.as_ptr()) };
                if rc < 0 {
                    out.complete &= rc == ENOENT;
                    break;
                }
                // SAFETY: `pinfo` is a live record the query just filled.
                let (port, caps) = unsafe {
                    (
                        sys::snd_seq_port_info_get_port(pinfo.as_ptr()),
                        sys::snd_seq_port_info_get_capability(pinfo.as_ptr()),
                    )
                };
                // SAFETY: as above; the name is copied before the record changes.
                let name = owned(unsafe { sys::snd_seq_port_info_get_name(pinfo.as_ptr()) })
                    .unwrap_or_else(|| format!("Port {port}"));
                out.ports.push(PortEntry {
                    client,
                    client_name: client_name.clone(),
                    port,
                    name,
                    caps,
                    ump: *ump.get_or_insert_with(|| self.has_ump_blocks(client)),
                });
            }
        }
        out
    }

    /// Whether `client` declares UMP function blocks. Block 0 is enough to
    /// answer "is this a UMP endpoint at all": a negative return means the
    /// client has none, i.e. it is legacy.
    fn has_ump_blocks(&self, client: i32) -> bool {
        let Ok(info) = BlockRecord::new() else {
            return false;
        };
        // SAFETY: `self.raw()` is open and `info` is a live record; the call's
        // `info` is typed `void *` in alsa-lib's header.
        let rc =
            unsafe { sys::snd_seq_get_ump_block_info(self.raw(), client, 0, info.as_ptr().cast()) };
        rc >= 0
    }
}
