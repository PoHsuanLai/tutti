//! MIDI *from* the AU: `kAudioUnitProperty_MIDIOutputCallback` (48) and its
//! discovery half, `kAudioUnitProperty_MIDIOutputCallbackInfo` (47).
//!
//! `send_midi` pushes MIDI *into* an instrument.
//! This module is the other direction: an arpeggiator, step sequencer, or chord
//! generator hosted as an AU emits notes during its render call, and with no
//! callback installed those notes have nowhere to go. The plugin still plays,
//! but the host cannot record the performance, route it to a second instrument,
//! or show it on a piano roll.

use std::mem::align_of;
use std::os::raw::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use coreaudio_sys::{AudioTimeStamp, MIDIPacket, MIDIPacketList, OSStatus};
use tutti_plugin_types::{Midi1Decoder, WireMidi};
#[cfg(feature = "test-support")]
use tutti_plugin_types::{Midi1Message, Midi1Splitter};

use crate::callback::with_state_mut;
use crate::error::Result;
use crate::property::{Element, MidiOutputCallbackInfo, MidiOutputCallbackProperty, Scope};
use crate::status::{CANNOT_DO_IN_CURRENT_CONTEXT, NO_ERR};

/// The answer to a call with no state or no packet list behind it.
const NOT_INSTALLED: OSStatus = -1;
use crate::unit::Unit;

/// The `AUMIDIOutputCallbackStruct` from `AudioUnitProperties.h`.
///
/// Hand-declared for the same reason `bus.rs::AuChannelInfo` is: `coreaudio-sys`
/// does not export it. The layout is pinned by
/// `tests::the_callback_struct_matches_the_c_abi` against Apple's declaration —
/// function pointer first, `userData` second, 16 bytes on a 64-bit target — so a
/// wrong guess fails a test rather than handing the AU a `userData` it reads as a
/// code pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AuMidiOutputCallbackStruct {
    midi_output_callback: Option<AuMidiOutputCallbackFn>,
    user_data: *mut c_void,
}

/// Apple's `AUMIDIOutputCallback` signature.
///
/// `pktlist` is a `MIDIPacketList` whose packet timestamps are **sample offsets**
/// from `timeStamp`, not host times — Apple's header is explicit about this, and
/// it is what makes the offsets directly usable as
/// [`WireMidi::offset`] without a clock conversion.
impl crate::plain::Sealed for AuMidiOutputCallbackStruct {}
// SAFETY: an `Option<unsafe extern "C" fn>` and a raw pointer in a
// `#[repr(C)]` struct: every bit pattern, zero (`None`, null) included, is a
// valid value.
unsafe impl crate::plain::Plain for AuMidiOutputCallbackStruct {}

type AuMidiOutputCallbackFn = unsafe extern "C" fn(
    user_data: *mut c_void,
    time_stamp: *const AudioTimeStamp,
    midi_out_num: u32,
    pktlist: *const MIDIPacketList,
) -> OSStatus;

/// What a host does with MIDI the AU emitted.
///
/// # The no-allocation contract
pub type MidiOutSink = Box<dyn FnMut(u32, &[WireMidi]) + Send>;

/// The state the AU's `userData` points at, and the thing whose lifetime the
/// removal ordering protects.
struct MidiOutState {
    sink: MidiOutSink,
    /// Reads the packets' bytes; reserved at [`install`], never grown.
    decoder: Midi1Decoder,
    /// [`Midi1Decoder::sysex_dropped`] as of the last callback, for
    /// [`AuMidiOutput::sysex_dropped`] to read from another thread.
    sysex_dropped: Arc<AtomicU64>,
}

/// A host's registration of a MIDI-output callback on one AU.
///
/// Holding this is what keeps the callback installed. Dropping it withdraws the
/// callback from the AU **before** the state the AU points at is freed — see
/// [`Drop for AuMidiOutput`](AuMidiOutput::drop) for why that order is the whole
/// point of the type existing rather than `install` returning `()`.
#[must_use = "dropping it uninstalls the output"]
pub struct AuMidiOutput {
    unit: Unit,
    /// Heap-pinned so its address is stable: the AU retains a `userData` derived
    /// from `&*state`, and this struct may be moved by its owner. Moving the
    /// `Box` moves only its 8-byte pointer, so the address the AU holds stays
    /// valid — the same reasoning the `Renderer` scratch and `HostCallbacks`
    /// are documented with.
    ///
    /// `Option` so [`Self::remove`] can take the box out *after* the property has
    /// been cleared, giving `Drop` nothing left to free.
    state: Option<Box<MidiOutState>>,
    /// Shared with `state`, so it can be read while the AU holds the state.
    sysex_dropped: Arc<AtomicU64>,
}

impl std::fmt::Debug for AuMidiOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuMidiOutput")
            .field("sysex_dropped", &self.sysex_dropped.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// What the AU says about its MIDI output streams.
///
/// The stream count is `names.len()`. It is not stored separately: Apple defines
/// the array's length *as* the number of outputs, so a second field would be a
/// derivable value stored beside its source and could disagree with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiOutputInfo {
    /// One name per MIDI output stream, in AU-assigned order. The index into this
    /// vec is the `midi_out_num` the AU passes to the callback.
    ///
    /// An entry is `None` when the AU published a value at that index that is not
    /// a usable `CFString` — a `None` rather than an empty string, so a host can
    /// tell "the AU named this output the empty string" from "the AU returned
    /// something that was not a name". The slot is kept rather than skipped
    /// because dropping it would renumber every output after it, and those
    /// numbers are what the callback is keyed by.
    pub names: Vec<Option<String>>,
}

impl MidiOutputInfo {
    /// How many MIDI output streams the AU offers.
    pub fn stream_count(&self) -> usize {
        self.names.len()
    }
}

/// What the AU publishes about its MIDI output streams, or `None` if it publishes
/// nothing.
///
/// # `None` means "not a MIDI source", and that is the real capability gate
///
/// An AU that answers with an empty array is reported as
/// `Some(MidiOutputInfo { names: [] })`, distinct from `None`: it implements the
/// property and is telling you it currently has no output streams.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub fn midi_output_info(unit: &Unit) -> Option<MidiOutputInfo> {
    // The property's value is a `CFArrayRef` the AU *copies* for the caller — Apple's
    // header says outright "The host owns this array and its elements and should
    // release them". `copy_string_array` takes that +1 under the Create rule so
    // the release happens on drop, on every path out.
    //
    // GET rule for the elements, not Create: the array copy did not add a
    // retain to each element, so the elements belong to the array and are
    // released with it. Wrapping one in `CfString::from_copied` (Create) would
    // release a string the host never owned — the over-release that corrupts the
    // AU's own table and crashes on the *next* read, far from the cause.
    //
    // The checked converter, because the AU supplies these pointers: an array
    // that is not made of CFStrings hands back non-null values that are not
    // names, and the unchecked converter takes the process down with SIGBUS on
    // one. Element and output names are also short strings, i.e. arm64 *tagged
    // pointers* — legitimately misaligned — which is why the checked
    // converter's tag-bit allowance, not a plain alignment test, is what admits
    // them.
    let names = unit
        .copy_string_array::<MidiOutputCallbackInfo>(Scope::GLOBAL, Element::ZERO)
        .ok()?;
    Some(MidiOutputInfo { names })
}

/// Installs `sink` as this AU's MIDI-output destination.
///
/// # Deliberately not gated on [`midi_output_info`]
///
/// # Errors
///
/// The registration holds a share of the unit, so the unit outlives it.
pub fn install(unit: &Unit, sink: MidiOutSink) -> Result<AuMidiOutput> {
    // Boxed once, here, on the control thread — the only allocation in the whole
    // path. `MidiOutState` is heap-pinned so the `userData` the AU retains stays
    // valid across moves of the returned handle.
    let sysex_dropped = Arc::new(AtomicU64::new(0));
    let state = Box::new(MidiOutState {
        sink,
        decoder: Midi1Decoder::new(),
        sysex_dropped: Arc::clone(&sysex_dropped),
    });
    let user_data: *mut c_void = &*state as *const MidiOutState as *mut c_void;

    let callback = AuMidiOutputCallbackStruct {
        midi_output_callback: Some(au_midi_output_callback),
        user_data,
    };
    // Build the handle BEFORE the property write, so that if the write succeeds
    // and anything after it fails, the handle's `Drop` is what withdraws the
    // callback. There is nothing after it today; the ordering is written this way
    // so adding something later cannot leave the AU holding a pointer no handle
    // owns.
    let handle = AuMidiOutput {
        unit: unit.clone(),
        state: Some(state),
        sysex_dropped,
    };
    unit.set::<MidiOutputCallbackProperty>(Scope::GLOBAL, Element::ZERO, &callback)?;
    Ok(handle)
}

impl AuMidiOutput {
    /// SysEx messages the callback dropped whole since this registration was
    /// made: past `tutti_plugin_types::SYSEX_OUT_CAPACITY` bytes, or ended by
    /// another status byte before their `0xF7`. As of the AU's last call.
    pub fn sysex_dropped(&self) -> u64 {
        self.sysex_dropped.load(Ordering::Relaxed)
    }

    /// Withdraw the callback, returning any error the AU reported.
    ///
    /// Equivalent to dropping the handle, except that the AU's status reaches the
    /// caller. Use this when a failed withdrawal is something the host wants to
    /// know about; `Drop` cannot report one.
    ///
    /// # Errors
    /// `Error::Os` if the AU refuses the clearing write; the boxed state is then
    /// leaked, since the unit may still call into it.
    pub fn remove(mut self) -> Result<()> {
        let result = self.clear();
        // `state` is now `None`; `Drop` will find nothing to do.
        result
    }

    /// The withdrawal, in one place so both [`Self::remove`] and `Drop` obey the
    /// same rule.
    ///
    /// # How the withdrawal is spelled, and why a null proc is NOT it
    ///
    /// # ORDERING INVARIANT
    ///
    /// The property write **MUST** complete before the boxed state is freed. This
    /// is the MIDI-output twin of the invariant
    /// `uninitialize` documents (there:
    /// `AudioUnitUninitialize` before the boxed `RenderScratch` is dropped) and of
    /// the one `Drop for HostCallbacks` documents for `HostCallbackInfo`. All three are
    /// the same hazard: while the property holds the live `userData` the AU may
    /// dereference it **on its render thread**, and freeing the box first leaves a
    /// use-after-free a host would experience as a random crash inside
    /// `AudioUnitRender`.
    ///
    /// # Safety
    /// `self.unit` must still be a live `AudioUnit`.
    fn clear(&mut self) -> Result<()> {
        // Nothing installed (already removed): nothing to withdraw.
        if self.state.is_none() {
            return Ok(());
        }
        // A REAL proc with a null `userData` — see this method's docs for the
        // measured reason a null proc is refused with -4 on every unit.
        let withdrawn = AuMidiOutputCallbackStruct {
            midi_output_callback: Some(au_midi_output_callback),
            user_data: std::ptr::null_mut(),
        };
        let result =
            self.unit
                .set::<MidiOutputCallbackProperty>(Scope::GLOBAL, Element::ZERO, &withdrawn);
        let state = self.state.take();
        if result.is_ok() {
            drop(state);
        } else {
            // The unit may still hold the pointer, and other `Unit` clones can
            // keep it alive and calling: leak the state rather than free memory
            // the callback could read.
            std::mem::forget(state);
        }
        result
    }
}

impl Drop for AuMidiOutput {
    /// Withdraw the callback before freeing the state the AU points at.
    ///
    /// Unlike `Drop for HostCallbacks`, which can lean on Rust's field drop order
    /// happening to be correct, this type has no such luck: the AU is not owned
    /// here, so nothing else will stop it calling back. The explicit clear is the
    /// only thing standing between a dropped registration and a use-after-free on
    /// the render thread.
    ///
    /// The status is ignored because a `Drop` has nowhere to report one; a host
    /// that needs it calls [`Self::remove`] instead.
    fn drop(&mut self) {
        let _ = self.clear();
    }
}

/// The AU calls this on its **render thread** to hand the host MIDI it generated.
///
/// `extern "C"`, so a panic must never escape: unwinding across the FFI boundary
/// into AudioToolbox is undefined behaviour. The whole body runs inside
/// [`catch_unwind`](std::panic::catch_unwind) and a caught panic becomes an error
/// status — the identical discipline
/// `instance.rs::au_input_render_callback` applies, and for the identical reason.
/// A host sink is arbitrary user code; a `[]` index in it must degrade to a
/// dropped block, not to undefined behaviour.
///
/// # Safety
/// Called by AudioToolbox with the `userData` registered through
/// `kAudioUnitProperty_MIDIOutputCallback`. `user_data` must be null or point at
/// a live `MidiOutState` box, which the withdrawing property write outlives —
/// that write is what makes the pointer unreachable before the box is freed.
///
/// The body forms a `&mut MidiOutState` from it, so no other reference to that
/// box may exist for the duration of the call. That holds because the AU invokes
/// this only from inside its own render, one call at a time, and the host side
/// touches the box only before installing and after the withdrawing write
/// returns.
///
/// `pktlist` must be null or a well-formed `MIDIPacketList` valid for the
/// duration of the call.
unsafe extern "C" fn au_midi_output_callback(
    user_data: *mut c_void,
    _time_stamp: *const AudioTimeStamp,
    midi_out_num: u32,
    pktlist: *const MIDIPacketList,
) -> OSStatus {
    // SAFETY: `user_data` is the `MidiOutState` box `install` registered, which
    // the withdrawing property write outlives; the AU calls only from inside its
    // own render, one call at a time, and the host touches the box only before
    // installing and after withdrawing, so this is the only reference.
    unsafe {
        with_state_mut::<MidiOutState, _>(
            "the MIDI output callback",
            user_data,
            NOT_INSTALLED,
            CANNOT_DO_IN_CURRENT_CONTEXT,
            |state| {
                if pktlist.is_null() {
                    return NOT_INSTALLED;
                }
                dispatch(state, midi_out_num, pktlist)
            },
        )
    }
}

/// The actual decode-and-deliver body. Split out so
/// [`au_midi_output_callback`] is nothing but the `catch_unwind` guard around it.
///
/// # Safety
/// `user_data` must be null or point at a live `MidiOutState`; `pktlist` must be
/// null or a well-formed `MIDIPacketList`.
fn dispatch(
    state: &mut MidiOutState,
    midi_out_num: u32,
    pktlist: *const MIDIPacketList,
) -> OSStatus {
    // A list cut short by a panic last time leaves events staged; they belong
    // to a block that is gone.
    state.decoder.begin_list();
    let MidiOutState { sink, decoder, .. } = &mut *state;
    let mut flush = |events: &[WireMidi]| (*sink)(midi_out_num, events);
    // SAFETY: `pktlist` is non-null (checked) and a valid packet list for the
    // call, which is `for_each_packet`'s contract.
    unsafe {
        for_each_packet(pktlist, |frame_offset, bytes| {
            decoder.decode(bytes, frame_offset, &mut flush);
        })
    };
    // The tail of the list is flushed whole, never held for the next one: a
    // note-off held back is a stuck voice.
    decoder.flush(&mut flush);
    state
        .sysex_dropped
        .store(state.decoder.sysex_dropped(), Ordering::Relaxed);
    NO_ERR
}

/// Walks a `MIDIPacketList`, calling `f(frame_offset, packet_bytes)` once per
/// packet. A packet's bytes are a stream, not a message: [`Midi1Decoder`] splits
/// them (see below).
///
/// # Why the walk is by byte offset rather than by struct read
///
/// # Safety
/// `pktlist` must be a well-formed, non-null `MIDIPacketList`: `numPackets`
/// packets laid out per `MIDIPacketNext`, each `length` bytes of `data` within the
/// allocation.
unsafe fn for_each_packet(pktlist: *const MIDIPacketList, mut f: impl FnMut(u32, &[u8])) {
    // SAFETY: `pktlist` is a valid packet list: its `numPackets` header field lies within it (unaligned read, as the list may be packed).
    let num_packets = unsafe {
        std::ptr::read_unaligned(
            (pktlist as *const u8).add(std::mem::offset_of!(MIDIPacketList, numPackets))
                as *const u32,
        )
    };
    // The first packet begins at the `packet` field's offset — 4, immediately
    // after `numPackets`.
    // SAFETY: the first packet starts at `offset_of!(MIDIPacketList, packet)`, within the list.
    let mut p = unsafe { (pktlist as *const u8).add(std::mem::offset_of!(MIDIPacketList, packet)) };

    for _ in 0..num_packets {
        // SAFETY: `p` points at a packet header inside the list: the walk advances only by each packet's own length.
        let time_stamp = unsafe {
            std::ptr::read_unaligned(
                p.add(std::mem::offset_of!(MIDIPacket, timeStamp)) as *const u64
            )
        };
        // SAFETY: as for the timestamp read above: within the packet header.
        let length = unsafe {
            std::ptr::read_unaligned(p.add(std::mem::offset_of!(MIDIPacket, length)) as *const u16)
        };
        // Clamp the plugin-supplied length to Apple's own documented maximum.
        // `MIDIServices.h` declares `Byte data[256]`, so a packet claiming more
        // than that is malformed by the framework's own definition, and honouring
        // the claim would read past the end of the list. This is the packet-level
        // twin of `render_input`'s "never trust the buffer the AU handed over": a
        // `u16` length can say 65535, which is 255 packets' worth of memory the AU
        // never wrote.
        //
        // It cannot catch a *legal* over-claim — a packet declaring 64 bytes and
        // meaning 3 is indistinguishable from one that really carries 64, and those
        // bytes are inside the allocation the AU sized. What that produces is
        // garbage messages from the AU's own slack, which is the plugin's bug; what
        // this prevents is a read outside the allocation entirely, which would be
        // ours.
        let length = length.min(MAX_PACKET_PAYLOAD);
        // SAFETY: the data follows the packet header, within the packet.
        let data = unsafe { p.add(PACKET_HEADER_SIZE) };

        // Apple's header: "The time stamp values contained within the MIDIPackets
        // in this list are **sample offsets** from the AudioTimeStamp provided."
        // So the value is already the frame offset this host wants — no host-time
        // conversion, and no clock to consult. It is a `u64` at the ABI and a
        // `u32` in `WireMidi`; a value past `u32::MAX` would be nonsense from a
        // render block, and saturating is the honest narrowing (the alternative,
        // wrapping, would place a late event at the start of the block).
        let frame_offset = u32::try_from(time_stamp).unwrap_or(u32::MAX);

        // SAFETY: `length` bytes follow the header (clamped to the maximum packet size), within the list the AU built.
        let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) };
        f(frame_offset, bytes);

        // `MIDIPacketNext`: advance past the header and payload, then align. On
        // arm64 Apple rounds up to 4; on x86_64 it does not round at all, and
        // rounding there would skip bytes of the next packet.
        // SAFETY: the end of this packet, from which the next header starts (rounded on ARM by `next_packet`).
        let raw_next = unsafe { p.add(PACKET_HEADER_SIZE + length as usize) };
        p = next_packet(raw_next);
    }
}

/// The `MIDIPacket` header size — `timeStamp` (8) + `length` (2) = 10 bytes,
/// with `data` starting immediately after.
///
/// Taken from the binding's own field offset rather than written as `10`, and
/// pinned by `tests::the_packet_abi_is_what_the_walk_assumes`: the whole walk is
/// arithmetic on this number, so a drift here mis-decodes every packet while
/// nothing else notices.
const PACKET_HEADER_SIZE: usize = std::mem::offset_of!(MIDIPacket, data);

/// The largest `length` a `MIDIPacket` may legally declare.
///
/// `MIDIServices.h` declares the payload as `Byte data[256]`, Apple's own bound
/// rather than a number picked here. Written as the literal, not derived — the
/// derivation was tried first: `size_of::<MIDIPacket>() - PACKET_HEADER_SIZE`
/// gives **258**, not 256, because the 4-byte-aligned struct (header 10 + data
/// 256 = 266, rounded to 268) carries two trailing padding bytes, and a bound
/// two bytes too generous lets a hostile packet address slack the AU never
/// wrote — exactly what the clamp exists to stop. Checked against the struct in
/// `tests::the_packet_abi_is_what_the_walk_assumes` instead, which fails if a
/// future SDK changes the array.
const MAX_PACKET_PAYLOAD: u16 = 256;

/// Advances a raw end-of-packet pointer to where the next packet starts,
/// reproducing Apple's `MIDIPacketNext` macro.
///
/// The architecture split is Apple's, and it is load-bearing in both directions:
/// on arm64 `MIDIPacket` must be 4-byte aligned so the macro rounds up, and
/// skipping that lands mid-header on every packet after the first whose payload
/// length is not a multiple of 4 (measured: a 3-byte message ends at +17 and the
/// next packet is at +20). On x86_64 packets are packed unaligned and rounding up
/// would *skip* bytes of the next packet's header.
#[inline]
fn next_packet(raw_next: *const u8) -> *const u8 {
    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    {
        const ALIGN: usize = align_of::<u32>();
        ((raw_next as usize + (ALIGN - 1)) & !(ALIGN - 1)) as *const u8
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
    {
        let _ = align_of::<u32>();
        raw_next
    }
}

/// Test-only: run the decoder over a packet list and collect what came out.
///
/// `#[doc(hidden)]` and `cfg(test)`-free by necessity (integration tests do not
/// see `cfg(test)`), but it is not part of the supported surface.
///
/// # Safety
/// `pktlist` must be a well-formed, non-null `MIDIPacketList`.
#[doc(hidden)]
#[cfg(feature = "test-support")]
pub unsafe fn decode_packet_list_for_test(pktlist: *const MIDIPacketList) -> Vec<WireMidi> {
    let mut out = Vec::new();
    let mut decoder = Midi1Decoder::new();
    let mut flush = |events: &[WireMidi]| out.extend_from_slice(events);
    // SAFETY: the test passes a packet list it built; see `for_each_packet`.
    unsafe {
        for_each_packet(pktlist, |frame_offset, bytes| {
            decoder.decode(bytes, frame_offset, &mut flush);
        })
    };
    decoder.flush(&mut flush);
    out
}

/// Test-only: the messages the walk found, before UMP conversion; a SysEx
/// message appears whole, `0xF0` to `0xF7`.
///
/// Separate from [`decode_packet_list_for_test`] so a test can tell "the walk
/// missed the message" from "the walk found it and `from_midi1_bytes` refused
/// it" — two failures with the same visible symptom.
///
/// # Safety
/// As [`decode_packet_list_for_test`].
#[doc(hidden)]
#[cfg(feature = "test-support")]
pub unsafe fn split_packet_list_for_test(pktlist: *const MIDIPacketList) -> Vec<(u32, Vec<u8>)> {
    let mut out = Vec::new();
    let mut splitter = Midi1Splitter::new();
    // SAFETY: the test passes a packet list it built; see `for_each_packet`.
    unsafe {
        for_each_packet(pktlist, |frame_offset, bytes| {
            splitter.split(bytes, frame_offset, &mut |frame, message| match message {
                Midi1Message::Short(bytes) | Midi1Message::SysEx(bytes) => {
                    out.push((frame, bytes.to_vec()));
                }
            });
        })
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;
    use tutti_ump::KeyEvent;

    /// The hand-declared callback struct must match Apple's: an
    /// `AUMIDIOutputCallback` function pointer then a `void *userData`, 16 bytes
    /// on a 64-bit target. If the fields were transposed the AU would call
    /// `userData` as code on its render thread.
    #[test]
    fn the_callback_struct_matches_the_c_abi() {
        assert_eq!(
            size_of::<AuMidiOutputCallbackStruct>(),
            2 * size_of::<usize>()
        );
        assert_eq!(
            align_of::<AuMidiOutputCallbackStruct>(),
            align_of::<usize>()
        );
        assert_eq!(
            std::mem::offset_of!(AuMidiOutputCallbackStruct, midi_output_callback),
            0,
            "the function pointer is first"
        );
        assert_eq!(
            std::mem::offset_of!(AuMidiOutputCallbackStruct, user_data),
            size_of::<usize>()
        );
        // `Option<fn>` must be niche-optimised to a bare pointer, or the null
        // withdrawal writes the wrong bytes.
        assert_eq!(
            size_of::<Option<AuMidiOutputCallbackFn>>(),
            size_of::<usize>()
        );
    }

    /// The packet ABI the walk's arithmetic rests on, taken from the binding
    /// rather than assumed. Verified against Apple's `MIDIServices.h` on macOS
    /// 15.6: `timeStamp` (`MIDITimeStamp`, 8 bytes) then `length` (`UInt16`) then
    /// `data`, with the list's `numPackets` first and `packet` at 4.
    #[test]
    fn the_packet_abi_is_what_the_walk_assumes() {
        assert_eq!(std::mem::offset_of!(MIDIPacketList, numPackets), 0);
        assert_eq!(
            std::mem::offset_of!(MIDIPacketList, packet),
            4,
            "the first packet begins right after the u32 count"
        );
        assert_eq!(std::mem::offset_of!(MIDIPacket, timeStamp), 0);
        assert_eq!(std::mem::offset_of!(MIDIPacket, length), 8);
        assert_eq!(
            PACKET_HEADER_SIZE, 10,
            "timeStamp (8) + length (2); the whole walk is arithmetic on this"
        );
        // The reason the walk never reads a `MIDIPacket` by value: the declared
        // size is vastly larger than a real 3-byte packet occupies, so a by-value
        // read would run past the end of the list.
        assert!(
            size_of::<MIDIPacket>() > PACKET_HEADER_SIZE + 3,
            "the binding's MIDIPacket includes a fixed data array, which is why \
             the walk reads fields at offsets instead"
        );
        // The clamp bound, checked against the struct rather than restated. Apple
        // declares `Byte data[256]`; the struct rounds 10 + 256 up to 268 for its
        // 4-byte alignment, so the payload sits in `size_of - header` MINUS the two
        // padding bytes. If a future SDK widens the array, this inequality is what
        // catches `MAX_PACKET_PAYLOAD` having gone stale.
        assert_eq!(
            size_of::<MIDIPacket>(),
            268,
            "MIDIServices.h declares timeStamp(8) + length(2) + data[256], aligned \
             to 4 = 268. If this changed, MAX_PACKET_PAYLOAD needs revisiting."
        );
        assert_eq!(
            MAX_PACKET_PAYLOAD, 256,
            "the documented payload maximum, which must be the array's length and \
             NOT `size_of - header` (that is 258 — it counts the trailing padding, \
             and a bound two bytes too generous is slack the AU never wrote)"
        );
        assert!(
            (MAX_PACKET_PAYLOAD as usize) < size_of::<MIDIPacket>() - PACKET_HEADER_SIZE + 1,
            "the bound must not exceed the addressable payload"
        );
        // And it must be well under what a `u16` length can claim, or the clamp is
        // not doing anything: 65535 is 255 packets' worth of memory the AU never
        // wrote. A `const` block so this is a compile-time check rather than a
        // runtime one — both operands are constants, and clippy is right that a
        // runtime `assert!` on them is the wrong tool.
        const { assert!(MAX_PACKET_PAYLOAD < u16::MAX) };
    }

    /// A packet claiming more than Apple's 256-byte maximum is clamped, not
    /// honoured.
    ///
    /// A `u16` length can say 65535. Reading that many bytes from a packet the AU
    /// sized for three is a read far outside the list allocation — the one failure
    /// in this walk that is the *host's* bug rather than the plugin's, since a
    /// legal-but-inflated length is indistinguishable from an honest one.
    #[test]
    fn a_length_beyond_apples_maximum_is_clamped() {
        // Hand-built: `numPackets = 1`, one packet declaring u16::MAX with three
        // real bytes, then 4 KiB of slack so a failure to clamp lands inside this
        // allocation and fails an assertion rather than segfaulting.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_ne_bytes());
        bytes.extend_from_slice(&0u64.to_ne_bytes());
        bytes.extend_from_slice(&u16::MAX.to_ne_bytes());
        bytes.extend_from_slice(&[0x90, 60, 100]);
        bytes.resize(bytes.len() + 4096, 0);

        // SAFETY: a well-formed header over a live 4 KiB+ allocation; the packet's
        // declared length is the thing under test and the slack bounds any
        // over-read.
        let found = unsafe { split_packet_list_for_test(bytes.as_ptr() as *const MIDIPacketList) };

        // The real message is found, and the clamp bounds how much slack can be
        // misread. 256 bytes yields 1 explicit message plus running-status
        // continuations at TWO bytes each (the status byte is reused), so ~128 —
        // not ~85, which is the answer if one forgets running status. Without the
        // clamp the walk would consume 65535 bytes and run 61 KiB past the
        // allocation.
        assert!(!found.is_empty(), "the real message must still be decoded");
        assert_eq!(found[0], (0, vec![0x90, 60, 100]));
        assert!(
            found.len() <= 129,
            "a 65535-byte claim must be clamped to Apple's 256-byte maximum, \
             which bounds the walk at ~128 messages; {} means the claim was \
             honoured and the walk left the allocation",
            found.len()
        );
    }

    /// Builds a list with Apple's OWN `MIDIPacketListInit`/`MIDIPacketListAdd` and
    /// check this walk finds every message at the offsets Apple wrote them to.
    #[test]
    fn the_walk_matches_apples_own_packing() {
        let mut storage = vec![0u8; 4096];
        let list = storage.as_mut_ptr() as *mut MIDIPacketList;
        // SAFETY: `storage` is a 4096-byte allocation, far more than the four
        // small packets added below need, and `list` points at its start.
        let messages: [(&[u8], u64); 4] = [
            (&[0x90, 60, 100], 0),
            (&[0x80, 60, 0], 128),
            (&[0xB0, 7, 64], 256),
            // Two note-ons packed into ONE packet — Apple's own packer does this,
            // which is why the splitter exists.
            (&[0x90, 62, 90, 0x90, 64, 80], 384),
        ];
        // SAFETY: `list` is a buffer this test sized for the packets it adds with `MIDIPacketListAdd`, within its capacity.
        unsafe {
            let mut pkt = coreaudio_sys::MIDIPacketListInit(list);
            for (bytes, ts) in messages {
                pkt = coreaudio_sys::MIDIPacketListAdd(
                    list,
                    storage.len() as u64,
                    pkt,
                    ts,
                    bytes.len() as u64,
                    bytes.as_ptr(),
                );
                assert!(!pkt.is_null(), "packet did not fit in 4 KiB");
            }
        }

        // SAFETY: `list` is the packet list this test just built.
        let found = unsafe { split_packet_list_for_test(list) };
        // Five messages from four packets: the last packet held two.
        assert_eq!(
            found.len(),
            5,
            "expected 5 messages from 4 packets (the last packet carries two); \
             got {found:?}"
        );
        assert_eq!(found[0], (0, vec![0x90, 60, 100]));
        assert_eq!(found[1], (128, vec![0x80, 60, 0]));
        assert_eq!(found[2], (256, vec![0xB0, 7, 64]));
        // Both messages of the multi-message packet carry that packet's offset.
        assert_eq!(found[3], (384, vec![0x90, 62, 90]));
        assert_eq!(found[4], (384, vec![0x90, 64, 80]));

        // And the UMP conversion, so the two halves are pinned separately.
        // SAFETY: `list` is the packet list this test just built.
        let events = unsafe { decode_packet_list_for_test(list) };
        assert_eq!(events.len(), 5);
        let note_on = |e: &WireMidi| match e.ump.key_event() {
            Some(KeyEvent::On(key)) => Some(key.note()),
            _ => None,
        };
        assert_eq!(note_on(&events[0]), Some(60));
        assert_eq!(events[0].offset, 0);
        assert!(events[1].ump.is_note_off());
        assert_eq!(note_on(&events[4]), Some(64));
        assert_eq!(events[4].offset, 384);
    }

    /// `next_packet` must reproduce `MIDIPacketNext` for this architecture.
    /// Asserted arithmetically as well as against the framework above, because
    /// this is the one place a cross-architecture mistake would be invisible on
    /// the machine it was written on.
    #[test]
    fn next_packet_follows_the_architecture_rule() {
        let base = 0x1000usize;
        for len in 0..8usize {
            let raw = (base + PACKET_HEADER_SIZE + len) as *const u8;
            let got = next_packet(raw) as usize;
            #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
            {
                assert_eq!(
                    got % 4,
                    0,
                    "on ARM MIDIPacket must be 4-byte aligned; len={len}"
                );
                assert!(got >= raw as usize && got - (raw as usize) < 4);
            }
            #[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
            assert_eq!(
                got, raw as usize,
                "on Intel packets are packed unaligned; rounding up would skip \
                 bytes of the next packet"
            );
        }
    }
}
