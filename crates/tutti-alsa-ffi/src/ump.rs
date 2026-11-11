//! What the sequencer says about a client's UMP Endpoint and its Function
//! Blocks (the kernel's Group Terminal Blocks).
//!
//! For a USB MIDI 2.0 device the kernel runs the UMP discovery itself when the
//! device is plugged in and keeps the answer: the Endpoint's protocol now, and
//! the protocols it offers, are the kernel's record of it, and the blocks are
//! the ones the device declared (or its Group Terminal Blocks). A user-space
//! client may publish the same records for itself.

use std::ffi::CStr;
use std::os::raw::c_char;

use crate::client::Client;
use crate::records::{BlockRecord, EndpointRecord};
use crate::sys;

/// The Endpoint's protocol bits (`SND_UMP_EP_INFO_PROTO_MIDI_MASK`): which of
/// the two are set, in `protocol` (the one in use) or `protocol_caps` (those
/// offered).
pub const PROTO_MIDI1: u32 = 0x0100;
/// `SND_UMP_EP_INFO_PROTO_MIDI2`.
pub const PROTO_MIDI2: u32 = 0x0200;
/// `SND_UMP_EP_INFO_PROTO_JRTS_TX`: the Endpoint sends JR Timestamps.
pub const PROTO_JRTS_TX: u32 = 0x0001;
/// `SND_UMP_EP_INFO_PROTO_JRTS_RX`: the Endpoint receives JR Timestamps.
pub const PROTO_JRTS_RX: u32 = 0x0002;

/// `SND_UMP_DIR_INPUT`.
pub const DIR_INPUT: u32 = 0x01;
/// `SND_UMP_DIR_OUTPUT`.
pub const DIR_OUTPUT: u32 = 0x02;
/// `SND_UMP_DIR_BIDIRECTION`.
pub const DIR_BIDIRECTION: u32 = 0x03;

/// `SND_UMP_BLOCK_IS_MIDI1`: the block is a MIDI 1.0 port.
pub const BLOCK_IS_MIDI1: u32 = 1;
/// `SND_UMP_BLOCK_IS_LOWSPEED`: the block's port is limited to 31.25 kbps.
pub const BLOCK_IS_LOWSPEED: u32 = 2;

/// One UMP Endpoint as the sequencer records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpEndpoint {
    /// The protocol bits in use now (`PROTO_*`: one of the two MIDI
    /// protocols, and the JR Timestamp directions).
    pub protocol: u32,
    /// The protocol bits the Endpoint offers.
    pub protocol_caps: u32,
    /// The Endpoint's name, when it has one.
    pub name: Option<String>,
    /// Its blocks, by number, as many as the sequencer answered for.
    pub blocks: Vec<UmpBlock>,
}

/// One Function Block (Group Terminal Block) as the sequencer records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpBlock {
    /// Its number.
    pub id: u32,
    /// Whether it is active.
    pub active: bool,
    /// `BLOCK_*` bits.
    pub flags: u32,
    /// `DIR_*`.
    pub direction: u32,
    /// The first Group it spans, from 0.
    pub first_group: u32,
    /// How many Groups it spans.
    pub num_groups: u32,
    /// Its MIDI-CI version, `0` for none.
    pub midi_ci_version: u32,
    /// Its SysEx 8 streams.
    pub sysex8_streams: u32,
    /// The UI hint: `0` unknown, `1` receiver, `2` sender, `3` both.
    pub ui_hint: u32,
    /// Its name, when it has one.
    pub name: Option<String>,
}

/// A C string alsa-lib owns, copied; `None` for NULL or an empty name.
fn name(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: non-NULL, and alsa-lib's info getters return NUL-terminated
    // strings valid until the owning record is freed or changed; copied at once.
    let text = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    (!text.is_empty()).then_some(text)
}

impl Client {
    /// The UMP Endpoint of `client`, and its blocks; `None` when the sequencer
    /// has none for it: a legacy client, reached through the kernel's
    /// converter, or a UMP client that never published one.
    #[must_use]
    pub fn ump_endpoint(&self, client: i32) -> Option<UmpEndpoint> {
        let info = EndpointRecord::new().ok()?;
        // SAFETY: `self.raw()` is open and `info` is a live record; the call's
        // `info` is typed `void *` in alsa-lib's header.
        let rc =
            unsafe { sys::snd_seq_get_ump_endpoint_info(self.raw(), client, info.as_ptr().cast()) };
        if rc < 0 {
            return None;
        }
        let ptr = info.as_ptr();
        // SAFETY: `info` is a live record the call just filled; each getter
        // reads it and the name is copied before the record is used again.
        let (protocol, protocol_caps, count, ep_name) = unsafe {
            (
                sys::snd_ump_endpoint_info_get_protocol(ptr),
                sys::snd_ump_endpoint_info_get_protocol_caps(ptr),
                sys::snd_ump_endpoint_info_get_num_blocks(ptr),
                name(sys::snd_ump_endpoint_info_get_name(ptr)),
            )
        };
        // The UMP limit: blocks are numbered 0 to 31.
        let blocks = (0..count.min(32))
            .filter_map(|id| self.ump_block(client, id))
            .collect();
        Some(UmpEndpoint {
            protocol,
            protocol_caps,
            name: ep_name,
            blocks,
        })
    }

    /// Block `id` of `client`'s Endpoint, if the sequencer has it.
    fn ump_block(&self, client: i32, id: u32) -> Option<UmpBlock> {
        let info = BlockRecord::new().ok()?;
        // SAFETY: `self.raw()` is open and `info` is a live record; the call's
        // `info` is typed `void *` in alsa-lib's header.
        let rc = unsafe {
            sys::snd_seq_get_ump_block_info(
                self.raw(),
                client,
                i32::try_from(id).ok()?,
                info.as_ptr().cast(),
            )
        };
        if rc < 0 {
            return None;
        }
        let p = info.as_ptr();
        // SAFETY: `info` is a live record the call just filled.
        unsafe {
            Some(UmpBlock {
                id: sys::snd_ump_block_info_get_block_id(p),
                active: sys::snd_ump_block_info_get_active(p) != 0,
                flags: sys::snd_ump_block_info_get_flags(p),
                direction: sys::snd_ump_block_info_get_direction(p),
                first_group: sys::snd_ump_block_info_get_first_group(p),
                num_groups: sys::snd_ump_block_info_get_num_groups(p),
                midi_ci_version: sys::snd_ump_block_info_get_midi_ci_version(p),
                sysex8_streams: sys::snd_ump_block_info_get_sysex8_streams(p),
                ui_hint: sys::snd_ump_block_info_get_ui_hint(p),
                name: name(sys::snd_ump_block_info_get_name(p)),
            })
        }
    }

    /// Publishes `endpoint` and its blocks as this client's own UMP Endpoint,
    /// as a user-space device does; the sequencer announces the change.
    ///
    /// # Errors
    ///
    /// The code of the call that failed.
    #[cfg(feature = "test-support")]
    pub fn publish_ump_endpoint(&self, endpoint: &UmpEndpoint) -> crate::Result<()> {
        use crate::error::check;
        use std::ffi::CString;
        let info = EndpointRecord::new()?;
        let p = info.as_ptr();
        let cname = endpoint
            .name
            .as_deref()
            .and_then(|n| CString::new(n).ok())
            .unwrap_or_default();
        let count = u32::try_from(endpoint.blocks.len()).unwrap_or(32);
        // SAFETY: `info` is a live record; `cname` outlives the call that copies it.
        unsafe {
            sys::snd_ump_endpoint_info_set_protocol_caps(p, endpoint.protocol_caps);
            sys::snd_ump_endpoint_info_set_protocol(p, endpoint.protocol);
            sys::snd_ump_endpoint_info_set_num_blocks(p, count);
            sys::snd_ump_endpoint_info_set_name(p, cname.as_ptr());
        }
        // SAFETY: `self.raw()` is open and `info` is a live record.
        let rc = unsafe { sys::snd_seq_set_ump_endpoint_info(self.raw(), p.cast()) };
        check(rc, "snd_seq_set_ump_endpoint_info")?;
        for block in &endpoint.blocks {
            let info = BlockRecord::new()?;
            let p = info.as_ptr();
            let cname = block
                .name
                .as_deref()
                .and_then(|n| CString::new(n).ok())
                .unwrap_or_default();
            // SAFETY: `info` is a live record; `cname` outlives the call that copies it.
            unsafe {
                sys::snd_ump_block_info_set_block_id(p, block.id);
                sys::snd_ump_block_info_set_active(p, u32::from(block.active));
                sys::snd_ump_block_info_set_direction(p, block.direction);
                sys::snd_ump_block_info_set_first_group(p, block.first_group);
                sys::snd_ump_block_info_set_num_groups(p, block.num_groups);
                sys::snd_ump_block_info_set_ui_hint(p, block.ui_hint);
                sys::snd_ump_block_info_set_name(p, cname.as_ptr());
            }
            let id = i32::try_from(block.id).unwrap_or(0);
            // SAFETY: `self.raw()` is open and `info` is a live record.
            let rc = unsafe { sys::snd_seq_set_ump_block_info(self.raw(), id, p.cast()) };
            check(rc, "snd_seq_set_ump_block_info")?;
        }
        Ok(())
    }
}
