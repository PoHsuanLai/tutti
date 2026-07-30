//! MIDI *from* the AU: `kAudioUnitProperty_MIDIOutputCallback` (48) and its
//! discovery half, `kAudioUnitProperty_MIDIOutputCallbackInfo` (47).
//!
//! [`crate::instance::AuInstance::send_midi`] pushes MIDI *into* an instrument.
//! This module is the other direction: an arpeggiator, step sequencer, or chord
//! generator hosted as an AU emits notes during its render call, and with no
//! callback installed those notes have nowhere to go. The plugin still plays,
//! but the host cannot record the performance, route it to a second instrument,
//! or show it on a piano roll.
//!
//! # Honest status on this machine
//!
//! **No installed AU publishes `MIDIOutputCallbackInfo`.** Measured on macOS 15.6
//! across all 138 registered components (every Apple unit, plus TDR Nova,
//! TAL-NoiseMaker and TAL-Reverb-4), at global/input/output scope, before and
//! after `AudioUnitInitialize`: zero hits. Nothing here has been exercised
//! end-to-end against a real emitting plugin; this module's tests drive the
//! decoder directly against hand-built packet lists and assert the
//! *installation* is accepted and withdrawn cleanly. See `tests/au_midi_out.rs`.
//!
//! Worth knowing before trusting property 47 as a capability gate: **almost
//! every AU accepts the property-48 *write*** (measured: 45 of them, including
//! AUDelay and AULowpass, which have no conceivable MIDI output). A successful
//! install proves nothing about whether the AU will ever call back — property
//! 47 is the only real signal, which is why [`midi_output_info`] exists and why
//! `install` does not consult it: a host should ask 47 and decide, rather than
//! have this layer refuse a write the AU would have taken.
//!
//! # The two hard constraints, both on the render thread
//!
//! The AU invokes the callback from inside `AudioUnitRender`, on the audio
//! thread, forcing two things — both enforced by construction rather than review:
//!
//! 1. **No panic may unwind across `extern "C"`.** [`au_midi_output_callback`] is
//!    nothing but a `catch_unwind` around [`decode_and_dispatch`], exactly as
//!    `instance.rs::au_input_render_callback` is around its body.
//! 2. **No allocation.** See [`MidiOutSink`] for the contract that achieves it
//!    and why the sink is handed a *borrowed* slice.

#![cfg(target_os = "macos")]

use std::mem::align_of;
use std::os::raw::c_void;

use tutti_midi_types::MidiEvent;

use crate::error::Result;
use crate::ffi::{get_property, set_property};
use crate::types::*;

/// The `AUMIDIOutputCallbackStruct` from `AudioUnitProperties.h`.
///
/// Hand-declared for the same reason `bus.rs::AuChannelInfo` is: `coreaudio-sys`
/// does not export it. The layout is pinned by
/// [`tests::the_callback_struct_matches_the_c_abi`] against Apple's declaration —
/// function pointer first, `userData` second, 16 bytes on a 64-bit target — so a
/// wrong guess fails a test rather than handing the AU a `userData` it reads as a
/// code pointer.
#[repr(C)]
#[derive(Clone, Copy)]
struct AuMidiOutputCallbackStruct {
    midi_output_callback: Option<AuMidiOutputCallbackFn>,
    user_data: *mut c_void,
}

/// Apple's `AUMIDIOutputCallback` signature.
///
/// `pktlist` is a `MIDIPacketList` whose packet timestamps are **sample offsets**
/// from `timeStamp`, not host times — Apple's header is explicit about this, and
/// it is what makes the offsets directly usable as
/// [`MidiEvent::frame_offset`] without a clock conversion.
type AuMidiOutputCallbackFn = unsafe extern "C" fn(
    user_data: *mut c_void,
    time_stamp: *const AudioTimeStamp,
    midi_out_num: u32,
    pktlist: *const MIDIPacketList,
) -> OSStatus;

/// What a host does with MIDI the AU emitted.
///
/// # The no-allocation contract
///
/// The sink receives a **borrowed slice**, `&[MidiEvent]`, whose lifetime ends
/// when the call returns — the contract, not a convention:
///
/// * The decoder writes into a **fixed-size stack array** owned by the callback
///   frame ([`DECODE_BATCH`]), so any packet-list length is delivered in batches
///   without a heap touch. A `Vec<MidiEvent>` return — the obvious shape, and
///   where an allocation sneaks in unnoticed — would allocate once per packet
///   list on the audio thread: the same hazard class `CLAUDE.md`'s `RtPublish`
///   rule exists for (a malloc in the render callback is a lock acquisition,
///   and a lock acquisition there is a dropout).
/// * The borrow makes parking one impossible. A sink that wants to keep events
///   must copy them into storage **it already owns** — a preallocated ring
///   buffer is the intended shape.
///
/// The sink itself is boxed once, at [`install`] time, on the control thread.
/// Nothing is allocated per callback — a sink must not allocate either (nor
/// lock, block, or `println!`), which the type system cannot enforce and is
/// stated here as the caller's half of the bargain.
///
/// `Send` because the closure is constructed on the control thread and called
/// on the render thread. Not `Sync`: the AU calls back from one render thread
/// at a time, and requiring `Sync` would rule out the `&mut`-captured
/// ring-buffer producer that is the natural implementation.
pub type MidiOutSink = Box<dyn FnMut(u32, &[MidiEvent]) + Send>;

/// How many events the callback decodes before flushing a batch to the sink.
///
/// A stack array of this size lives in the callback frame. 64 was chosen against
/// the shape of the data: a `MIDIPacketList` reaching an AU host in one render
/// block carries at most a few dozen messages even from a dense arpeggiator (a
/// 512-frame block at 48 kHz is 10.7 ms), and `size_of::<MidiEvent>()` is small
/// enough that 64 of them is a few hundred bytes of stack.
///
/// A longer list is not truncated: the batch is flushed and refilled, so a
/// 1000-event list arrives as 16 calls. Truncating would silently drop notes,
/// which is the one failure a MIDI path must never have.
const DECODE_BATCH: usize = 64;

/// The state the AU's `userData` points at, and the thing whose lifetime the
/// removal ordering protects.
struct MidiOutState {
    sink: MidiOutSink,
}

/// A host's registration of a MIDI-output callback on one AU.
///
/// Holding this is what keeps the callback installed. Dropping it withdraws the
/// callback from the AU **before** the state the AU points at is freed — see
/// [`Drop for AuMidiOutput`](AuMidiOutput::drop) for why that order is the whole
/// point of the type existing rather than `install` returning `()`.
pub struct AuMidiOutput {
    unit: AudioUnit,
    /// Heap-pinned so its address is stable: the AU retains a `userData` derived
    /// from `&*state`, and this struct may be moved by its owner. Moving the
    /// `Box` moves only its 8-byte pointer, so the address the AU holds stays
    /// valid — the same reasoning `AuReady::scratch` and `AuLoaded::transport`
    /// are documented with.
    ///
    /// `Option` so [`Self::remove`] can take the box out *after* the property has
    /// been cleared, giving `Drop` nothing left to free.
    state: Option<Box<MidiOutState>>,
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
    /// tell "the AU named this output the empty string" from "the AU handed us
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
/// `None` is returned when the property read fails — which on this machine is
/// **every installed AU** (measured: 0 of 138 components answer it, at any
/// scope, initialized or not). Not a `Result`, because
/// `kAudioUnitProperty_MIDIOutputCallbackInfo` is optional and "the AU declines
/// to say" / "the AU has no MIDI outputs" put a host in exactly the same
/// position — the same reading [`crate::bus::supported_channel_configs`]
/// applies to its own optional property.
///
/// The distinction that *does* matter is against the property-48 write, which
/// almost every AU accepts (45 measured, AUDelay included) without any
/// intention of calling back. So a host must gate on **this** function, not on
/// whether [`install`] succeeded.
///
/// An AU that answers with an empty array is reported as
/// `Some(MidiOutputInfo { names: [] })`, distinct from `None`: it implements the
/// property and is telling you it currently has no output streams.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
pub(crate) unsafe fn midi_output_info(unit: AudioUnit) -> Option<MidiOutputInfo> {
    // The property's value is a `CFArrayRef` the AU *copies* for us — Apple's
    // header says outright "The host owns this array and its elements and should
    // release them". `CfArray::from_copied` takes that +1 under the Create rule
    // so the release happens on drop, on every path out including the early
    // returns below.
    let raw: CFArrayRef = get_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK_INFO,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )
    .ok()?;
    let array = crate::cf::CfArray::from_copied(raw)?;

    let names = (0..array.len())
        .map(|i| {
            let ptr = array.value_at(i)?;
            // GET rule, not Create: the array copy did not add a retain to each
            // element, so the elements belong to the array and are released with
            // it. Wrapping one in `CfString::from_copied` (Create) would release
            // a string the host never owned — the over-release that corrupts the
            // AU's own table and crashes on the *next* read, far from the cause.
            // This is the identical distinction `factory_presets` documents for
            // `presetName`.
            //
            // `checked` rather than the bare converter because the AU supplies
            // this pointer: an array that is not made of CFStrings hands back
            // non-null values that are not names, and the unchecked converter
            // takes the process down with SIGBUS on one. Element and output names
            // are also short strings, i.e. arm64 *tagged pointers* — legitimately
            // misaligned — which is why the checked converter's tag-bit
            // allowance, not a plain alignment test, is what admits them.
            cfstring_to_string_checked(ptr as CFStringRef)
        })
        .collect();

    Some(MidiOutputInfo { names })
}

/// Install `sink` as this AU's MIDI-output destination.
///
/// # Deliberately not gated on [`midi_output_info`]
///
/// A host that wants the gate should apply it — this does not, because the two
/// properties disagree in practice. Measured on macOS 15.6: 45 AUs accept this
/// write while **none** publishes the info property. Refusing the write here
/// would mean this layer overruling the AU's own answer, and an AU that takes the
/// callback and never calls it costs nothing but one boxed closure.
///
/// # Errors
///
/// [`AuError::OsStatus`] when the AU refuses the property. Measured refusals are
/// the output units and a handful of generators; the effects, instruments and
/// mixers all accept it.
///
/// # Safety
/// `unit` must be a live `AudioUnit` that outlives the returned
/// [`AuMidiOutput`]. The registration holds a raw `AudioUnit` and clears the
/// property on drop, so a unit disposed while a registration is alive would be
/// written to after death.
pub(crate) unsafe fn install(unit: AudioUnit, sink: MidiOutSink) -> Result<AuMidiOutput> {
    // Boxed once, here, on the control thread — the only allocation in the whole
    // path. `MidiOutState` is heap-pinned so the `userData` the AU retains stays
    // valid across moves of the returned handle.
    let state = Box::new(MidiOutState { sink });
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
        unit,
        state: Some(state),
    };
    set_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
        &callback,
    )?;
    Ok(handle)
}

impl AuMidiOutput {
    /// Withdraw the callback, returning any error the AU reported.
    ///
    /// Equivalent to dropping the handle, except that the AU's status reaches the
    /// caller. Use this when a failed withdrawal is something the host wants to
    /// know about; `Drop` cannot report one.
    ///
    /// # Errors
    /// [`AuError::OsStatus`] if the AU refuses the clearing write. The boxed
    /// state is freed **either way** — see [`Self::clear`] for why that is the
    /// safe choice and not a leak-versus-crash trade.
    pub fn remove(mut self) -> Result<()> {
        // SAFETY: `unit` is live per `install`'s contract, and this is the
        // documented ordering — clear before free.
        let result = unsafe { self.clear() };
        // `state` is now `None`; `Drop` will find nothing to do.
        result
    }

    /// The withdrawal, in one place so both [`Self::remove`] and `Drop` obey the
    /// same rule.
    ///
    /// # How the withdrawal is spelled, and why a null proc is NOT it
    ///
    /// The obvious withdrawal — a zeroed struct — **does not work**, and this was
    /// measured rather than reasoned about. Apple declares `midiOutputCallback`
    /// nullable, and `Drop for AuLoaded` withdraws
    /// `kAudioUnitProperty_HostCallbacks` with exactly that all-null trick. So the
    /// first version of this code did the same, and every teardown failed.
    ///
    /// Measured on macOS 15.6 against AUDelay, AULowpass, AUSampler,
    /// DLSMusicDevice and AUMatrixReverb — identical on all five:
    ///
    /// | write | status |
    /// |---|---|
    /// | real proc + real `userData` (the install) | `noErr` |
    /// | **all-null struct** | **-4** (`unimpErr`) |
    /// | null proc + non-null `userData` | **-4** |
    /// | `NULL` data pointer, size 0 | **-4** |
    /// | **real proc + null `userData`** | `noErr` |
    /// | `HostCallbacks` all-null, for contrast | `noErr` |
    ///
    /// So `MIDIOutputCallback` and `HostCallbacks` do **not** behave alike: the AU
    /// rejects any write whose proc is null, and the last row shows the difference
    /// is specific to this property rather than to the AU. The withdrawal is
    /// therefore a **real proc with a null `userData`**, which is inert by
    /// construction: [`decode_and_dispatch`] returns `-1` immediately on a null
    /// `user_data`, so an AU that calls the retained proc after this write reaches
    /// no host state at all.
    ///
    /// That is what makes the free below safe rather than merely ordered. The AU is
    /// left holding a pointer to a function that cannot reach the freed box,
    /// instead of a pointer to the box itself.
    ///
    /// # ORDERING INVARIANT
    ///
    /// The property write **MUST** complete before the boxed state is freed. This
    /// is the MIDI-output twin of the invariant
    /// [`crate::instance::AuReady::uninitialize`] documents as FIX 2 (there:
    /// `AudioUnitUninitialize` before the boxed `RenderScratch` is dropped) and of
    /// the one `Drop for AuLoaded` documents for `HostCallbackInfo`. All three are
    /// the same hazard: while the property holds the live `userData` the AU may
    /// dereference it **on its render thread**, and freeing the box first leaves a
    /// use-after-free a host would experience as a random crash inside
    /// `AudioUnitRender`.
    ///
    /// `AudioUnitSetProperty` is synchronous and the AU calls the callback only
    /// from inside its own render, so a returning write means no call is in flight
    /// and none can start against the old `userData`. That is what makes the
    /// removal race-free rather than merely ordered.
    ///
    /// # If the AU refuses even the inert write
    ///
    /// The state is freed anyway, and the error is surfaced through
    /// [`Self::remove`]. Keeping the box alive to be safe would leak it on every
    /// teardown of a refusing unit, and the alternative is unavailable: this handle
    /// does not own the AU, so it cannot dispose the unit to make the pointer
    /// unreachable. In practice a refusal means the AU is being torn down anyway,
    /// and `AuInstance`'s `Drop` disposes the unit right after — which is what
    /// actually ends its ability to call anything.
    ///
    /// # Safety
    /// `self.unit` must still be a live `AudioUnit`.
    unsafe fn clear(&mut self) -> Result<()> {
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
        let result = set_property(
            self.unit,
            K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            &withdrawn,
        );
        // Freed only after the write above returned — in both the success and the
        // failure arm, per the reasoning in this method's docs.
        drop(self.state.take());
        result
    }
}

impl Drop for AuMidiOutput {
    /// Withdraw the callback before freeing the state the AU points at.
    ///
    /// Unlike `Drop for AuLoaded`, which can lean on Rust's field drop order
    /// happening to be correct, this type has no such luck: the AU is not owned
    /// here, so nothing else will stop it calling back. The explicit clear is the
    /// only thing standing between a dropped registration and a use-after-free on
    /// the render thread.
    ///
    /// The status is ignored because a `Drop` has nowhere to report one; a host
    /// that needs it calls [`Self::remove`] instead.
    fn drop(&mut self) {
        // SAFETY: `unit` is live for the lifetime of this handle per `install`'s
        // safety contract.
        let _ = unsafe { self.clear() };
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
unsafe extern "C" fn au_midi_output_callback(
    user_data: *mut c_void,
    _time_stamp: *const AudioTimeStamp,
    midi_out_num: u32,
    pktlist: *const MIDIPacketList,
) -> OSStatus {
    // `AssertUnwindSafe`: the only state reachable is `&mut MidiOutState` and the
    // AU's own packet buffer. A panic mid-batch means some events were delivered
    // and the rest were not — a glitched block, not a broken invariant — so there
    // is nothing for unwind safety to protect. Same judgement as the input
    // render callback's.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_and_dispatch(user_data, midi_out_num, pktlist)
    }));
    match result {
        Ok(status) => status,
        Err(_) => {
            // Do NOT swallow this. `eprintln!` rather than a logging facade
            // because this crate has no logger dependency, and a write to stderr
            // is the one diagnostic guaranteed to survive a process whose audio
            // thread just panicked.
            eprintln!(
                "tutti-au-host: PANIC in au_midi_output_callback, \
                 contained to avoid unwinding into AudioToolbox"
            );
            K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT
        }
    }
}

/// The actual decode-and-deliver body. Split out so
/// [`au_midi_output_callback`] is nothing but the `catch_unwind` guard around it.
///
/// # Safety
/// `user_data` must be null or point at a live `MidiOutState`; `pktlist` must be
/// null or a well-formed `MIDIPacketList`.
unsafe fn decode_and_dispatch(
    user_data: *mut c_void,
    midi_out_num: u32,
    pktlist: *const MIDIPacketList,
) -> OSStatus {
    if user_data.is_null() || pktlist.is_null() {
        return -1;
    }
    let state = &mut *(user_data as *mut MidiOutState);

    // The stack batch. `MidiEvent` is `Copy` with no `Drop`, so a zeroed
    // placeholder costs nothing and no partial-initialisation dance is needed.
    let mut batch = [MidiEvent::from_ump(0, &[0u32]); DECODE_BATCH];
    let mut n = 0usize;

    for_each_message(pktlist, |frame_offset, bytes| {
        // `from_midi1_bytes` is the exact reverse of what
        // `AuInstance::send_midi` does, and mirroring it is deliberate: the two
        // directions must agree on which families have a legacy 3-byte form, or a
        // round trip through this host silently changes the message. It returns
        // `None` for anything unparseable — a truncated running-status byte, a
        // SysEx fragment — which is dropped rather than guessed at.
        if let Some(ev) = MidiEvent::from_midi1_bytes(frame_offset, bytes) {
            batch[n] = ev;
            n += 1;
            if n == DECODE_BATCH {
                // Flush and keep going. Truncating instead would drop notes, and
                // a dropped note-off is a stuck voice that never releases.
                (state.sink)(midi_out_num, &batch[..n]);
                n = 0;
            }
        }
    });

    if n > 0 {
        (state.sink)(midi_out_num, &batch[..n]);
    }
    NO_ERR
}

/// Walk a `MIDIPacketList`, calling `f(frame_offset, message_bytes)` once per
/// **MIDI message** — not once per packet.
///
/// # Why the walk is by byte offset rather than by struct read
///
/// `MIDIPacket` is a flexible-array struct (`data[256]` in the binding) that is
/// *never* stored at its declared size in a real list: Apple packs consecutive
/// packets tightly, `header + length` rounded up to the next 4-byte boundary on
/// arm64 and not rounded at all on x86_64 (`MIDIPacketNext` in `MIDIServices.h`
/// says the alignment "may differ between CPU architectures"). Reading a whole
/// `MIDIPacket` by value would read 268 bytes for a 3-byte message, past the end
/// of the list — so each field is read at its verified offset with
/// `read_unaligned` (verified because on x86_64 the packets genuinely are
/// unaligned, making an aligned read undefined behaviour, not merely slow).
///
/// Measured against Apple's own `MIDIPacketListInit`/`MIDIPacketListAdd` on
/// macOS 15.6 / arm64: four packets of 3,3,3,6 bytes land at list offsets +4,
/// +20, +36, +52 — `10-byte header + len`, rounded up to 4. This walk reproduces
/// those offsets exactly, pinned by `tests::the_walk_matches_apples_own_packing`.
///
/// # One packet can carry several messages
///
/// A packet's `data` is a stream of MIDI bytes, not one message: Apple's own
/// `MIDIPacketListAdd` will pack `90 3E 5A 90 40 50` — two note-ons — into a
/// single 6-byte packet (measured, not assumed). Handing the whole `data` blob
/// to a parser expecting one message drops the second note. So the bytes are
/// split on status bytes first, and **running status** (a data-only
/// continuation reusing the previous status byte) is expanded — exactly what a
/// sequencer AU emitting a run of note-ons on one channel uses.
///
/// # No allocation
///
/// Everything here is pointer arithmetic and a `[u8; 3]` reassembly buffer on the
/// stack. `f` is called with a borrowed slice.
///
/// # Safety
/// `pktlist` must be a well-formed, non-null `MIDIPacketList`: `numPackets`
/// packets laid out per `MIDIPacketNext`, each `length` bytes of `data` within the
/// allocation.
unsafe fn for_each_message(pktlist: *const MIDIPacketList, mut f: impl FnMut(u32, &[u8])) {
    let num_packets = std::ptr::read_unaligned(
        (pktlist as *const u8).add(std::mem::offset_of!(MIDIPacketList, numPackets)) as *const u32,
    );
    // The first packet begins at the `packet` field's offset — 4, immediately
    // after `numPackets`.
    let mut p = (pktlist as *const u8).add(std::mem::offset_of!(MIDIPacketList, packet));

    for _ in 0..num_packets {
        let time_stamp = std::ptr::read_unaligned(
            p.add(std::mem::offset_of!(MIDIPacket, timeStamp)) as *const u64,
        );
        let length =
            std::ptr::read_unaligned(p.add(std::mem::offset_of!(MIDIPacket, length)) as *const u16);
        // Clamp the plugin-supplied length to Apple's own documented maximum.
        // `MIDIServices.h` declares `Byte data[256]`, so a packet claiming more
        // than that is malformed by the framework's own definition, and honouring
        // the claim would read past the end of the list. This is the packet-level
        // twin of `render_input`'s "never trust the buffer the AU handed us": a
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
        let data = p.add(PACKET_HEADER_SIZE);

        // Apple's header: "The time stamp values contained within the MIDIPackets
        // in this list are **sample offsets** from the AudioTimeStamp provided."
        // So the value is already the frame offset this host wants — no host-time
        // conversion, and no clock to consult. It is a `u64` at the ABI and a
        // `u32` in `MidiEvent`; a value past `u32::MAX` would be nonsense from a
        // render block, and saturating is the honest narrowing (the alternative,
        // wrapping, would place a late event at the start of the block).
        let frame_offset = u32::try_from(time_stamp).unwrap_or(u32::MAX);

        let bytes = std::slice::from_raw_parts(data, length as usize);
        split_messages(bytes, frame_offset, &mut f);

        // `MIDIPacketNext`: advance past the header and payload, then align. On
        // arm64 Apple rounds up to 4; on x86_64 it does not round at all, and
        // rounding there would skip bytes of the next packet.
        let raw_next = p.add(PACKET_HEADER_SIZE + length as usize);
        p = next_packet(raw_next);
    }
}

/// The `MIDIPacket` header size — `timeStamp` (8) + `length` (2) = 10 bytes,
/// with `data` starting immediately after.
///
/// Taken from the binding's own field offset rather than written as `10`, and
/// pinned by [`tests::the_packet_abi_is_what_the_walk_assumes`]: the whole walk is
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
/// [`tests::the_packet_abi_is_what_the_walk_assumes`] instead, which fails if a
/// future SDK changes the array.
const MAX_PACKET_PAYLOAD: u16 = 256;

/// Advance a raw end-of-packet pointer to where the next packet starts,
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

/// Split one packet's byte stream into individual MIDI messages, expanding
/// running status, and call `f` once per complete message.
///
/// # What this defends against
///
/// The bytes come from a plugin, so every length is a claim rather than a fact:
///
/// * **A packet claiming more bytes than it holds.** Guarded structurally: a
///   status byte whose data bytes run past the end of the slice is dropped rather
///   than read, because reading it would be an out-of-bounds load. This is the
///   packet-level twin of `render_input`'s "never trust the buffer the AU handed
///   us".
/// * **Running status.** A data byte arriving with no preceding status byte is
///   dropped (there is nothing to attribute it to); one arriving after a
///   channel-voice status reuses that status, per the MIDI 1.0 spec. Getting this
///   wrong on a dense note run loses every message after the first.
/// * **Real-time bytes interleaved mid-message.** `F8`..`FF` may legally appear
///   between the data bytes of another message and must **not** clear the running
///   status. They are emitted immediately and the surrounding message continues.
/// * **System Common resets running status.** `F0`..`F7` clear it, per the spec,
///   so a data byte after a SysEx is not silently attributed to the note-on
///   before it.
/// * **SysEx.** Dropped, deliberately: `MidiEvent::from_midi1_bytes` has no
///   single-message SysEx form (UMP needs fragmenting, which allocates), and
///   `send_midi` skips SysEx in the other direction too. The two directions agree.
fn split_messages(bytes: &[u8], frame_offset: u32, f: &mut impl FnMut(u32, &[u8])) {
    /// Data-byte count for a channel-voice / system-common status byte, or `None`
    /// for a status this host does not deliver.
    fn data_len(status: u8) -> Option<usize> {
        match status & 0xF0 {
            // Note off / on, poly pressure, control change, pitch bend.
            0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => Some(2),
            // Program change, channel pressure.
            0xC0 | 0xD0 => Some(1),
            0xF0 => match status {
                0xF1 | 0xF3 => Some(1), // MTC quarter frame, song select
                0xF2 => Some(2),        // song position pointer
                0xF6 => Some(0),        // tune request
                0xF8..=0xFF => Some(0), // system real-time
                // 0xF0 SysEx start / 0xF7 end: no single-message form here.
                _ => None,
            },
            _ => None,
        }
    }

    let mut i = 0usize;
    // The status byte in effect for running status, or `None` when none is.
    let mut running: Option<u8> = None;

    while i < bytes.len() {
        let b = bytes[i];
        if b >= 0x80 {
            // A status byte.
            let Some(n) = data_len(b) else {
                // SysEx or something undeliverable. Per the MIDI spec a System
                // Common status (0xF0..=0xF7) cancels running status; a System
                // Real-Time one (0xF8..=0xFF) does not — but every real-time
                // status has `data_len == Some(0)` and so never reaches here.
                running = None;
                i += 1;
                continue;
            };
            // System real-time bytes must NOT disturb running status: they may be
            // interleaved into another message's data bytes, and clearing the
            // status would orphan the rest of that message.
            if !(0xF8..=0xFF).contains(&b) {
                running = if b < 0xF0 { Some(b) } else { None };
            }
            // A status byte whose data bytes are not all present. Dropping it is
            // the only safe answer — `bytes[i + 1]` would be out of bounds, which
            // is precisely the "packet claiming more bytes than it holds" case.
            if i + 1 + n > bytes.len() {
                return;
            }
            f(frame_offset, &bytes[i..i + 1 + n]);
            i += 1 + n;
        } else {
            // A data byte: running status, if one is in effect.
            let Some(status) = running else {
                // No status to attribute this to. Skip it rather than guessing.
                i += 1;
                continue;
            };
            let n = data_len(status).unwrap_or(0);
            if n == 0 || i + n > bytes.len() {
                return;
            }
            // Reassemble on the stack — `[u8; 3]` covers every channel-voice
            // message, and nothing longer can arrive by running status.
            let mut msg = [0u8; 3];
            msg[0] = status;
            msg[1..1 + n].copy_from_slice(&bytes[i..i + n]);
            f(frame_offset, &msg[..1 + n]);
            i += n;
        }
    }
}

/// Test-only: run the decoder over a packet list and collect what came out.
///
/// Exposed `pub` because `tests/au_midi_out.rs` is an integration test — a
/// separate crate — and the packet-decode path is the half of this module that
/// **can** be tested on this machine, no AU on it emitting MIDI. Without a way in
/// from an integration test the decoder's edge cases (empty list, running status,
/// a packet claiming more bytes than it holds) would be unreachable, and the
/// alternative — asserting only that `install` returns `Ok` — is the vacuous
/// shape this crate's test policy rejects.
///
/// `#[doc(hidden)]` and `cfg(test)`-free by necessity (integration tests do not
/// see `cfg(test)`), but it is not part of the supported surface.
///
/// # Safety
/// `pktlist` must be a well-formed, non-null `MIDIPacketList`.
#[doc(hidden)]
pub unsafe fn decode_packet_list_for_test(pktlist: *const MIDIPacketList) -> Vec<MidiEvent> {
    let mut out = Vec::new();
    for_each_message(pktlist, |frame_offset, bytes| {
        if let Some(ev) = MidiEvent::from_midi1_bytes(frame_offset, bytes) {
            out.push(ev);
        }
    });
    out
}

/// Test-only: the raw messages the walk found, before UMP conversion.
///
/// Separate from [`decode_packet_list_for_test`] so a test can tell "the walk
/// missed the message" from "the walk found it and `from_midi1_bytes` refused
/// it" — two failures with the same visible symptom.
///
/// # Safety
/// As [`decode_packet_list_for_test`].
#[doc(hidden)]
pub unsafe fn split_packet_list_for_test(pktlist: *const MIDIPacketList) -> Vec<(u32, Vec<u8>)> {
    let mut out = Vec::new();
    for_each_message(pktlist, |frame_offset, bytes| {
        out.push((frame_offset, bytes.to_vec()));
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

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

    /// Build a list with Apple's OWN `MIDIPacketListInit`/`MIDIPacketListAdd` and
    /// check this walk finds every message at the offsets Apple wrote them to.
    ///
    /// This is the test that makes the hand-rolled `next_packet` trustworthy:
    /// rather than asserting the walk agrees with itself, it asserts it agrees
    /// with the framework. Measured spacing on macOS 15.6 / arm64 for payloads of
    /// 3,3,3,6 bytes: +4, +20, +36, +52 — `10 + len` rounded up to 4.
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
            // which is why `split_messages` exists.
            (&[0x90, 62, 90, 0x90, 64, 80], 384),
        ];
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
        let events = unsafe { decode_packet_list_for_test(list) };
        assert_eq!(events.len(), 5);
        assert!(events[0].is_note_on());
        assert_eq!(events[0].note(), Some(60));
        assert_eq!(events[0].frame_offset, 0);
        assert!(events[1].is_note_off());
        assert_eq!(events[4].note(), Some(64));
        assert_eq!(events[4].frame_offset, 384);
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
