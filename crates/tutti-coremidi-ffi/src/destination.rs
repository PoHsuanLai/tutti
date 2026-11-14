//! A **native-UMP** virtual MIDI destination, the inbound counterpart to
//! [`UmpSource`](crate::UmpSource).
//!
//! The safe `coremidi` wrapper has `Client::virtual_destination_with_protocol`,
//! but its receive block reads the connection's reference constant as a
//! `&mut ()`. A virtual destination has no connection, so CoreMIDI passes a
//! null one, and the wrapper makes a null reference out of it on the first
//! packet ("null reference produced" in a debug build, undefined behaviour in
//! a release one). [`UmpDestination`] creates the endpoint with
//! `MIDIDestinationCreateWithProtocol` itself, with a block that never reads
//! the reference constant, and hands each packet's timestamp and UMP words to
//! the caller.

use block2::RcBlock;
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{
    MIDIDestinationCreateWithProtocol, MIDIEndpointDispose, MIDIEndpointRef, MIDIEventList,
    MIDIEventPacket, MIDIEventPacketNext, MIDIReceiveBlock, MIDITimeStamp,
};
use std::ffi::c_void;
use std::fmt;
use std::sync::Mutex;

use crate::source::{Error, Protocol, shared_raw_client};

/// The receive block CoreMIDI calls with each event list: the list, and the
/// connection's reference constant, which this block never reads. The list is
/// typed `*const c_void` here, since a block's arguments must have an
/// Objective-C encoding and `coremidi-sys` declares none for `MIDIEventList`;
/// a pointer is a pointer at the ABI, and the block casts it back.
type ReceiveBlock = RcBlock<dyn Fn(*const c_void, *mut c_void)>;

/// A CoreMIDI virtual destination that speaks the **MIDI 2.0 (UMP) protocol**:
/// other apps see a UMP-capable destination, and every packet sent to it is
/// handed to the callback given at creation, on CoreMIDI's delivery thread, as
/// its timestamp (host time, 0 for "now") and its UMP words (several messages
/// back to back, each one's length in its type nibble). Owns its endpoint,
/// which is disposed on drop.
pub struct UmpDestination {
    endpoint: MIDIEndpointRef,
    // Kept alive while the endpoint is: CoreMIDI holds a copy of the block, and
    // this one owns the closure the copy calls.
    _block: ReceiveBlock,
}

// SAFETY: the endpoint is an opaque `u32` handle into CoreMIDI, whose objects are
// internally synchronised, and `MIDIEndpointDispose` may run on any thread. The
// block is never called from Rust: it is only retained by CoreMIDI and released
// by the drop, both atomic on a block's reference count, and the closure it
// owns is `Send` (`new` requires it) and is called under a mutex.
unsafe impl Send for UmpDestination {}

impl fmt::Debug for UmpDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UmpDestination")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl UmpDestination {
    /// Publishes a virtual destination named `name` carrying `protocol`;
    /// `on_packet` gets each packet sent to it.
    ///
    /// # Errors
    ///
    /// [`Error`] when the client or the endpoint cannot be created.
    pub fn new<F>(name: &str, protocol: Protocol, on_packet: F) -> Result<Self, Error>
    where
        F: FnMut(MIDITimeStamp, &[u32]) + Send + 'static,
    {
        let client = shared_raw_client().map_err(|status| Error {
            operation: "MIDIClientCreate",
            status,
        })?;
        // CoreMIDI calls the block on its own thread, one list at a time; the
        // mutex makes the `FnMut` callable through the block's `Fn`.
        let on_packet = Mutex::new(on_packet);
        let block: ReceiveBlock = RcBlock::new(move |list: *const c_void, _: *mut c_void| {
            let mut on_packet = on_packet
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // SAFETY: CoreMIDI passes a valid event list for the length of the
            // call (MIDIServices.h, `MIDIReceiveBlock`), or null, which
            // `packets` reads as empty.
            unsafe { packets(list.cast::<MIDIEventList>(), &mut *on_packet) };
        });
        let name = CFString::new(name);
        let mut endpoint: MIDIEndpointRef = 0;
        // SAFETY: `client` is a live client, `name` outlives the call, `endpoint`
        // is a valid out-pointer, and the block is a valid Objective-C block
        // CoreMIDI copies (and so retains) before the call returns; the block
        // never reads its reference-constant argument.
        let status = unsafe {
            MIDIDestinationCreateWithProtocol(
                client,
                name.as_concrete_TypeRef(),
                protocol.id(),
                &mut endpoint,
                RcBlock::as_ptr(&block).cast::<c_void>() as MIDIReceiveBlock,
            )
        };
        if status != 0 {
            return Err(Error {
                operation: "MIDIDestinationCreateWithProtocol",
                status,
            });
        }
        Ok(Self {
            endpoint,
            _block: block,
        })
    }
}

impl Drop for UmpDestination {
    fn drop(&mut self) {
        // SAFETY: the endpoint is ours, created above and disposed once.
        unsafe {
            MIDIEndpointDispose(self.endpoint);
        }
    }
}

/// Hands each packet of `list` to `on_packet` as its timestamp and words.
///
/// # Safety
///
/// `list` is null or points to a valid `MIDIEventList` whose `numPackets`
/// packets each hold `wordCount` words, as CoreMIDI delivers it.
unsafe fn packets(list: *const MIDIEventList, on_packet: &mut impl FnMut(MIDITimeStamp, &[u32])) {
    if list.is_null() {
        return;
    }
    // SAFETY: `list` is valid (the caller's contract); the list is packed to
    // four bytes, so its fields are read unaligned.
    let count = unsafe { std::ptr::addr_of!((*list).numPackets).read_unaligned() };
    // SAFETY: the first packet is inside the list.
    let mut packet: *const MIDIEventPacket = unsafe { std::ptr::addr_of!((*list).packet).cast() };
    for _ in 0..count {
        // SAFETY: `packet` is one of the list's `count` packets (the first, or
        // `MIDIEventPacketNext` of the one before), each `wordCount` words long;
        // its fields are read unaligned for the four-byte packing.
        let (stamp, words) = unsafe {
            let stamp = std::ptr::addr_of!((*packet).timeStamp).read_unaligned();
            let len = std::ptr::addr_of!((*packet).wordCount).read_unaligned() as usize;
            let words = std::ptr::addr_of!((*packet).words).cast::<u32>();
            (stamp, std::slice::from_raw_parts(words, len))
        };
        on_packet(stamp, words);
        // SAFETY: `packet` is a valid packet of the list (above).
        packet = unsafe { MIDIEventPacketNext(packet) };
    }
}
