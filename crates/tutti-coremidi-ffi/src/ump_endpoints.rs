//! The UMP Endpoints and Function Blocks CoreMIDI reports (macOS 15).
//!
//! `MIDIUMPEndpointManager`, `MIDIUMPEndpoint` and `MIDIUMPFunctionBlock` are
//! Objective-C classes, so `coremidi-sys` (C functions) does not carry them;
//! `objc2-core-midi`'s generated bindings do, and this module is the only code
//! that names them. What it returns is plain values holding the OS's own
//! numbers; `tutti-midi-hardware` turns them into `tutti-midi-device`'s
//! Function Blocks and checks them against M2-104 there.
//!
//! The classes do not exist before macOS 15. [`ump_endpoints`] asks the
//! Objective-C runtime for the manager class first and returns `None` where it
//! is absent, so a caller on an older system behaves as it did before.

use coremidi_sys::{MIDIObjectGetIntegerProperty, kMIDIPropertyUniqueID};
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyClass;
use objc2_core_midi::{MIDIUMPEndpoint, MIDIUMPEndpointManager, MIDIUMPFunctionBlock};

/// One Function Block as CoreMIDI reports it (`MIDIUMPFunctionBlock`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpBlockInfo {
    /// `functionBlockID`: the block's number.
    pub id: u8,
    /// `isEnabled`.
    pub active: bool,
    /// `direction`: `kMIDIUMPFunctionBlockDirection*`, 0 unknown, 1 input, 2
    /// output, 3 bidirectional.
    pub direction: i32,
    /// `firstGroup`, from 0.
    pub first_group: u8,
    /// `totalGroupsSpanned`.
    pub groups: u8,
    /// `MIDI1Info`: 0 not a MIDI 1.0 port, 1 unrestricted, 2 restricted.
    pub midi1: i32,
    /// `UIHint`: 0 unknown, 1 receiver, 2 sender, 3 both.
    pub ui_hint: i32,
    /// `maxSysEx8Streams`.
    pub sysex8_streams: u8,
    /// `name`.
    pub name: String,
}

/// One UMP Endpoint as CoreMIDI reports it (`MIDIUMPEndpoint`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpEndpointInfo {
    /// `MIDIProtocol`, the protocol in use: `kMIDIProtocol_1_0` is 1,
    /// `kMIDIProtocol_2_0` is 2.
    pub protocol: i32,
    /// The `kMIDIPropertyUniqueID` of the endpoint's source (what a device
    /// sends from), when it has one.
    pub source_unique_id: Option<i32>,
    /// The `kMIDIPropertyUniqueID` of its destination, when it has one.
    pub destination_unique_id: Option<i32>,
    /// Its Function Blocks.
    pub blocks: Vec<UmpBlockInfo>,
}

/// Every UMP Endpoint now, or `None` where the OS has no
/// `MIDIUMPEndpointManager` (before macOS 15).
#[must_use]
pub fn ump_endpoints() -> Option<Vec<UmpEndpointInfo>> {
    // `ClassType::class` panics on a missing class; the lookup by name is the
    // availability check.
    AnyClass::get(c"MIDIUMPEndpointManager")?;
    Some(autoreleasepool(|_| {
        // SAFETY: the class exists (checked above), `sharedInstance` and
        // `UMPEndpoints` take no arguments and are safe to call from any thread
        // (Apple documents the manager as a shared singleton), and the arrays
        // they return are retained for the length of the pool.
        let endpoints = unsafe { MIDIUMPEndpointManager::sharedInstance().UMPEndpoints() };
        endpoints.iter().map(|e| endpoint_info(&e)).collect()
    }))
}

fn endpoint_info(endpoint: &MIDIUMPEndpoint) -> UmpEndpointInfo {
    // SAFETY: plain getters on a live endpoint object.
    let (protocol, source, destination, blocks) = unsafe {
        (
            endpoint.MIDIProtocol().0,
            endpoint.MIDISource(),
            endpoint.MIDIDestination(),
            endpoint.functionBlocks(),
        )
    };
    UmpEndpointInfo {
        protocol,
        source_unique_id: unique_id(source),
        destination_unique_id: unique_id(destination),
        blocks: blocks.iter().map(|b| block_info(&b)).collect(),
    }
}

fn block_info(block: &MIDIUMPFunctionBlock) -> UmpBlockInfo {
    // SAFETY: plain getters on a live function block object.
    unsafe {
        UmpBlockInfo {
            id: block.functionBlockID(),
            active: block.isEnabled(),
            direction: block.direction().0,
            first_group: block.firstGroup(),
            groups: block.totalGroupsSpanned(),
            midi1: block.MIDI1Info().0,
            ui_hint: block.UIHint().0,
            sysex8_streams: block.maxSysEx8Streams(),
            name: block.name().to_string(),
        }
    }
}

/// `kMIDIPropertyUniqueID` of an endpoint, `None` for the null reference (an
/// endpoint with no source or no destination) or one that has none.
fn unique_id(endpoint: u32) -> Option<i32> {
    if endpoint == 0 {
        return None;
    }
    let mut id: i32 = 0;
    // SAFETY: `kMIDIPropertyUniqueID` is a constant CFString CoreMIDI exports,
    // and `id` is a valid out-pointer for the call. A stale reference makes the
    // call return an error status, which reads as `None`.
    let status = unsafe { MIDIObjectGetIntegerProperty(endpoint, kMIDIPropertyUniqueID, &mut id) };
    (status == 0).then_some(id)
}
