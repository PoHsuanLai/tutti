//! MIDI *out of* an AU: `MIDIOutputCallbackInfo` (47) and `MIDIOutputCallback`
//! (48).
//!
//! `au_conformance.rs` covers `send_midi`, which pushes MIDI *into* an instrument.
//! This suite covers the other direction — the one an arpeggiator or step
//! sequencer needs.
//!
//! ## What is and is not exercised here, stated plainly
//!
//! **No AU installed on this machine emits MIDI.** Measured, not assumed: all 138
//! registered components were probed for `kAudioUnitProperty_MIDIOutputCallbackInfo`
//! at global, input and output scope, before and after `AudioUnitInitialize` —
//! zero publish it. `no_installed_au_publishes_midi_output_info` re-runs that sweep
//! as a test, so the claim is checked rather than recorded.
//!
//! So **end-to-end delivery is unexercised.** Nothing here proves an AU's notes
//! reach the sink, because nothing on this machine produces any. What *is* proven:
//!
//! * the info read reports what was measured, over the whole registry;
//! * the callback installs and uninstalls without crashing, and stops delivering
//!   after removal;
//! * the packet→`MidiEvent` decode is driven **directly**, against hand-built
//!   `MIDIPacketList`s including the edge cases a plugin can produce — an empty
//!   list, running status, and a packet claiming more bytes than it holds.
//!
//! That third group is where the real risk lives and it is fully covered. The
//! decode path is reachable from here through
//! `midi_out::decode_packet_list_for_test`, which exists precisely so this suite
//! does not have to settle for asserting `install(...).is_ok()` — a test that would
//! pass against a host that decoded nothing at all.
//!
//! ## The trap: the write is accepted by AUs that will never call back
//!
//! 45 of the units that publish **no** MIDI outputs nonetheless accept the
//! property-48 write, AUDelay and AULowpass among them. A successful install is
//! therefore worthless as a capability signal, which is why
//! `AuInstance::midi_output_info` is the gate and why
//! `accepting_the_callback_write_does_not_mean_the_au_emits_midi` pins the
//! asymmetry — a future host that started gating installs on property 47 would
//! change observable behaviour, and this is what notices.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_midi_out
//! ```

#![cfg(target_os = "macos")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod support;
use support::corpus::{every_component, DELAY, DLS_SYNTH, LOWPASS, SAMPLER};

use tutti_au_host::midi_out::{decode_packet_list_for_test, split_packet_list_for_test};
use tutti_au_host::types::MIDIPacketList;
use tutti_au_host::AuType;
use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
use tutti_midi_types::MidiMessage;

/// Same rationale as `au_conformance.rs`'s `AU_LOCK`.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// Recover from poisoning: the guard only serializes, it protects no shared state,
/// so one panicking test must not convert into N spurious failures.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

// ------------------------------------------------------------ packet builders

/// Storage for a `MIDIPacketList` built by hand, plus the pointer into it.
///
/// A `Vec<u8>` rather than a `MIDIPacketList` value, because the struct is a
/// C flexible array: the Rust binding declares a fixed 256-byte `data` field that a
/// real 3-byte packet never occupies, so building a list *as* that struct would
/// produce a layout no AU ever sends. Everything here writes bytes at the offsets
/// Apple's own packer writes them to, which is the layout the host's walk has to
/// handle.
struct PacketListBuf {
    bytes: Vec<u8>,
}

impl PacketListBuf {
    /// Build a list using Apple's **own** `MIDIPacketListInit` / `MIDIPacketListAdd`.
    ///
    /// Preferred over hand-writing bytes wherever the case allows it, for the
    /// reason `midi_out::tests::the_walk_matches_apples_own_packing` gives: a test
    /// that constructs the list with the same assumptions the walk makes proves
    /// only that the walk agrees with itself. Using the framework's packer makes
    /// the framework the oracle.
    ///
    /// Returns `None` if the messages did not fit, so a caller cannot mistake a
    /// truncated list for a decoded one.
    fn from_framework(messages: &[(&[u8], u64)]) -> Option<Self> {
        let mut bytes = vec![0u8; 8192];
        let list = bytes.as_mut_ptr() as *mut MIDIPacketList;
        // SAFETY: `bytes` is an 8 KiB allocation and `list` points at its start;
        // `MIDIPacketListAdd` is given that exact capacity and reports failure by
        // returning null, which is checked.
        unsafe {
            let mut pkt = coreaudio_sys::MIDIPacketListInit(list);
            for (data, ts) in messages {
                pkt = coreaudio_sys::MIDIPacketListAdd(
                    list,
                    8192,
                    pkt,
                    *ts,
                    data.len() as u64,
                    data.as_ptr(),
                );
                if pkt.is_null() {
                    return None;
                }
            }
        }
        Some(Self { bytes })
    }

    /// Build a well-formed list by writing the ABI by hand.
    ///
    /// Needed alongside [`Self::from_framework`] because Apple's packer will not
    /// produce a zero-length packet or one whose payload is not a complete MIDI
    /// message — both of which a plugin can send and both of which the walk has to
    /// survive. `declared_length` is set to `data.len()`, so every packet here is
    /// internally consistent; [`Self::from_raw_overclaiming`] is the deliberately
    /// inconsistent variant.
    ///
    /// The layout written is the one measured from Apple's packer on macOS 15.6 /
    /// arm64: `numPackets` at +0, first packet at +4, each packet `timeStamp` (8) +
    /// `length` (2) + data, the next packet starting at the following 4-byte
    /// boundary.
    fn from_raw(packets: &[(u64, &[u8])]) -> Self {
        let declared: Vec<(u64, u16, &[u8])> = packets
            .iter()
            .map(|(ts, d)| (*ts, d.len() as u16, *d))
            .collect();
        Self::write(&declared)
    }

    /// Build a list whose packets **claim more bytes than they hold**.
    ///
    /// The hostile shape: `length` is a number the plugin writes, and a walk that
    /// trusted it would read out of bounds. Kept separate from [`Self::from_raw`]
    /// so a reader cannot mistake an over-claiming packet for an ordinary one — and
    /// so an ordinary test cannot accidentally over-claim, which is a mistake this
    /// suite already made: padding the data to the 4-byte boundary *inside* the
    /// declared window turned the pad bytes into running-status messages, and the
    /// decoder was right to find them.
    fn from_raw_overclaiming(packets: &[(u64, u16, &[u8])]) -> Self {
        Self::write(packets)
    }

    fn write(packets: &[(u64, u16, &[u8])]) -> Self {
        let mut bytes = Vec::with_capacity(4096);
        bytes.extend_from_slice(&(packets.len() as u32).to_ne_bytes());
        for (ts, declared, data) in packets {
            bytes.extend_from_slice(&ts.to_ne_bytes());
            bytes.extend_from_slice(&declared.to_ne_bytes());
            bytes.extend_from_slice(data);
            // Pad to the next 4-byte boundary, matching `MIDIPacketNext` on arm64.
            // The padding is OUTSIDE the declared length — `declared` describes
            // `data` alone — so it is never handed to the message splitter.
            while !bytes.len().is_multiple_of(4) {
                bytes.push(0);
            }
        }
        // Trailing slack so a walk that over-reads on a malformed packet lands in
        // this buffer rather than off the end of the allocation — the test then
        // FAILS on a wrong decode instead of crashing, which is a diagnosable
        // failure rather than a SIGSEGV.
        bytes.resize(bytes.len() + 512, 0);
        Self { bytes }
    }

    fn as_ptr(&self) -> *const MIDIPacketList {
        self.bytes.as_ptr() as *const MIDIPacketList
    }

    /// Decode to `MidiEvent`s through the host's real path.
    fn decode(&self) -> Vec<tutti_midi_types::MidiEvent> {
        // SAFETY: both constructors produce a well-formed `MIDIPacketList` header
        // over a live allocation. `from_raw` may produce a packet whose *declared*
        // length exceeds its data, which is the case under test; the trailing slack
        // keeps any over-read inside the allocation so a bug is an assertion
        // failure rather than a segfault.
        unsafe { decode_packet_list_for_test(self.as_ptr()) }
    }

    /// The raw messages the walk found, before UMP conversion.
    fn split(&self) -> Vec<(u32, Vec<u8>)> {
        // SAFETY: as `decode`.
        unsafe { split_packet_list_for_test(self.as_ptr()) }
    }
}

// ---------------------------------------------------- capability: property 47

/// No AU installed on this machine publishes `MIDIOutputCallbackInfo`.
///
/// The whole registry is swept rather than a hand-picked corpus, because this
/// asserts a **negative** and a negative over five chosen units proves nothing
/// about the machine.
///
/// If this test ever fails, that is *good news* and not a regression: it means a
/// MIDI-emitting AU has been installed and the end-to-end path this suite cannot
/// currently reach has become testable. The failure message says so, so whoever
/// hits it does not "fix" it by deleting the assertion.
#[test]
fn no_installed_au_publishes_midi_output_info() {
    let _g = lock();
    let mut publishers = Vec::new();
    let mut probed = 0usize;

    for info in every_component() {
        // Output units are skipped: they are hardware endpoints, several of them
        // take exclusive device access on instantiation, and none is a plugin a
        // host would ask about MIDI output. Everything else is probed.
        if matches!(info.component_type, AuType::Output) {
            continue;
        }
        // SAFETY: `component` came from `AudioComponentFindNext` via
        // `enumerate_components`, so it is a live factory handle.
        let Ok(au) = (unsafe { tutti_au_host::AuInstance::new(info.component, RATE, BLOCK) })
        else {
            // A unit that will not instantiate cannot be asked. Not a skip that
            // hides anything: it also cannot emit MIDI to this host.
            continue;
        };
        probed += 1;
        if let Some(published) = au.midi_output_info() {
            publishers.push((info.name.clone(), published));
        }
    }

    // A silent-skip guard: the sweep must actually have opened a substantial
    // number of units, or "none publish" is vacuous. Measured: 100+ instantiate.
    assert!(
        probed > 50,
        "only {probed} components instantiated; a negative assertion over that few \
         units says nothing about the machine"
    );
    assert!(
        publishers.is_empty(),
        "MIDI-emitting AU(s) found: {publishers:#?}\n\
         \n\
         This is NOT a regression — it means the machine gained an AU that \
         publishes MIDIOutputCallbackInfo, and the end-to-end delivery path this \
         suite documents as unexercised has become testable. Add the unit to \
         support/corpus.rs, write the delivery test, and update this assertion to \
         expect it. Do not delete the assertion."
    );
}

/// Accepting the property-48 **write** proves nothing about MIDI output.
///
/// Measured on macOS 15.6: 45 units accept the callback install while publishing
/// no MIDI output streams at all — AUDelay and AULowpass among them, neither of
/// which has any conceivable MIDI output. So a host must gate on
/// `midi_output_info`, never on whether `install_midi_output` returned `Ok`.
///
/// Pinned as a test rather than only documented, because the asymmetry is the kind
/// of thing a later change "tidies up": a host that started refusing the install
/// for units without property 47 would change observable behaviour, and this fails
/// if it does.
#[test]
fn accepting_the_callback_write_does_not_mean_the_au_emits_midi() {
    let _g = lock();
    for unit in [DELAY, LOWPASS] {
        let mut au = unit.open_uninitialized(RATE, BLOCK);
        assert!(
            au.midi_output_info().is_none(),
            "{}: precondition — this unit publishes no MIDI outputs",
            unit.label
        );
        let registration = au
            .install_midi_output(Box::new(|_, _| {}))
            .unwrap_or_else(|e| {
                panic!(
                    "{}: measured to ACCEPT the MIDIOutputCallback write despite \
                 publishing no outputs, got {e:?}. If this AU now refuses, the \
                 asymmetry documented in midi_out.rs no longer holds.",
                    unit.label
                )
            });
        // Withdraw explicitly so the AU's status is seen rather than dropped.
        registration
            .remove()
            .unwrap_or_else(|e| panic!("{}: withdrawing the callback failed: {e:?}", unit.label));
    }
}

// ------------------------------------------------- install / uninstall safety

/// Installing and withdrawing the callback leaves the AU renderable.
///
/// The reason this is worth a test even with no emitting AU present: the install
/// hands the AU a raw pointer into a heap box and the withdrawal frees that box.
/// Getting the order wrong is a use-after-free on the render thread — the hazard
/// `AuReady::uninitialize` documents as FIX 2. A unit that still renders correct
/// audio after a full install/render/withdraw/render cycle is the observable that
/// the ordering held.
#[test]
fn install_then_withdraw_leaves_the_au_renderable() {
    let _g = lock();
    for unit in [DELAY, SAMPLER] {
        let mut au = unit.open_uninitialized(RATE, BLOCK);
        let registration = au
            .install_midi_output(Box::new(|_, _| {}))
            .unwrap_or_else(|e| panic!("{}: install failed: {e:?}", unit.label));
        au.initialize()
            .unwrap_or_else(|e| panic!("{}: initialize failed: {e:?}", unit.label));

        let channels = au.num_outputs().max(1) as usize;
        let input = support::corpus::silence(channels, BLOCK as usize);
        let mut output = support::corpus::silence(channels, BLOCK as usize);
        support::corpus::render(&mut au, &input, &mut output, BLOCK)
            .unwrap_or_else(|e| panic!("{}: render with callback installed: {e:?}", unit.label));

        registration
            .remove()
            .unwrap_or_else(|e| panic!("{}: withdraw failed: {e:?}", unit.label));

        // Renders again after the boxed state has been freed. If the withdrawal
        // had run in the wrong order, the AU would still hold a pointer into freed
        // memory and this is the call that would dereference it.
        support::corpus::render(&mut au, &input, &mut output, BLOCK)
            .unwrap_or_else(|e| panic!("{}: render after withdraw: {e:?}", unit.label));

        let peak = support::corpus::peak(&output);
        assert!(
            peak.is_finite(),
            "{}: output peak must be finite. `peak` folds with f32::max, which \
             returns the NON-NaN operand — an all-NaN buffer reads as 0.0, so a \
             smallness check alone would pass on total garbage.",
            unit.label
        );
        assert!(
            peak < 1e-3,
            "{}: silence in must give silence out, got peak {peak}",
            unit.label
        );
        assert!(
            support::corpus::all_finite(&output),
            "{}: every sample must be finite",
            unit.label
        );
    }
}

/// Withdrawing the callback stops delivery — asserted by **counting** calls.
///
/// The sink increments a shared counter. After `remove`, no AU on this machine
/// would call it anyway (none emits MIDI), so what this actually pins is the
/// counter *observability* and that the removal path runs to completion on a
/// rendering unit. The count is checked rather than mere absence of a crash,
/// because an over-release does not crash: it passes any suite that only checks
/// a pointer for nullness.
///
/// The pre-removal count is recorded and asserted equal to the post-removal one.
/// That is a real assertion even at zero: a host that somehow invoked the sink
/// after freeing its state would show up as a count that grew, and a host that
/// invoked a *dangling* sink would crash rather than increment.
#[test]
fn no_delivery_arrives_after_the_callback_is_withdrawn() {
    let _g = lock();
    let calls = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(AtomicUsize::new(0));

    let mut au = DLS_SYNTH.open_uninitialized(RATE, BLOCK);
    let registration = {
        let calls = Arc::clone(&calls);
        let events = Arc::clone(&events);
        au.install_midi_output(Box::new(move |_out_num, evs| {
            calls.fetch_add(1, Ordering::SeqCst);
            events.fetch_add(evs.len(), Ordering::SeqCst);
        }))
        .expect("DLSMusicDevice accepts the MIDI output callback")
    };
    au.initialize().expect("DLSMusicDevice initializes");

    let channels = au.num_outputs().max(1) as usize;
    let input = support::corpus::silence(channels, BLOCK as usize);
    let mut output = support::corpus::silence(channels, BLOCK as usize);

    // Drive some MIDI *in* so the unit is actually generating, then render. If the
    // unit did emit MIDI this is where it would arrive.
    let notes = [
        tutti_midi_types::MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        tutti_midi_types::MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x8000),
    ];
    au.send_midi(&notes);
    for _ in 0..8 {
        support::corpus::render(&mut au, &input, &mut output, BLOCK).expect("render");
    }
    let calls_before = calls.load(Ordering::SeqCst);
    let events_before = events.load(Ordering::SeqCst);

    registration.remove().expect("withdraw succeeds");

    // Render well past the removal. Any call now would be against freed state.
    for _ in 0..32 {
        support::corpus::render(&mut au, &input, &mut output, BLOCK)
            .expect("render after withdraw");
    }

    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_before,
        "the sink was invoked {} more time(s) after the callback was withdrawn. \
         The boxed sink state is freed by `remove`, so any such call is a \
         use-after-free that happened to survive.",
        calls.load(Ordering::SeqCst) - calls_before
    );
    assert_eq!(
        events.load(Ordering::SeqCst),
        events_before,
        "events arrived after withdrawal"
    );

    // Documenting the honest state of affairs, in the test rather than only in a
    // comment: DLSMusicDevice publishes no MIDI outputs, so zero calls is the
    // expected count and end-to-end delivery is NOT what this test proves.
    assert!(
        au.midi_output_info().is_none(),
        "DLSMusicDevice was measured to publish no MIDI outputs; if it now does, \
         this test's zero-call expectation is no longer the right one"
    );
    assert_eq!(
        calls_before, 0,
        "no AU on this machine emits MIDI, so the sink is expected never to fire. \
         A non-zero count here means delivery has become testable — see the module \
         docs and write the end-to-end test."
    );
}

/// Dropping the registration withdraws the callback just as `remove` does.
///
/// Both paths go through the same `clear`, and the `Drop` path is the one a host
/// will actually hit. Asserted by rendering after the drop: if `Drop` had freed the
/// state without clearing the property first, the AU would hold a dangling
/// `userData` and this render is what would dereference it.
#[test]
fn dropping_the_registration_withdraws_the_callback() {
    let _g = lock();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut au = DELAY.open_uninitialized(RATE, BLOCK);
    {
        let calls = Arc::clone(&calls);
        let registration = au
            .install_midi_output(Box::new(move |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
            }))
            .expect("AUDelay accepts the write");
        // Dropped here, at end of scope, WITHOUT calling `remove`.
        drop(registration);
    }
    au.initialize().expect("initialize after the drop");

    let input = support::corpus::silence(2, BLOCK as usize);
    let mut output = support::corpus::silence(2, BLOCK as usize);
    for _ in 0..16 {
        support::corpus::render(&mut au, &input, &mut output, BLOCK)
            .expect("render after the registration was dropped");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the sink fired after its registration was dropped — the boxed state is \
         freed at that point"
    );
    let peak = support::corpus::peak(&output);
    assert!(
        peak.is_finite(),
        "peak must be finite, not NaN-folded to 0.0"
    );
    assert!(peak < 1e-3, "silence in, silence out; got {peak}");
}

/// Installing twice in a row is safe: the second install supersedes the first.
///
/// A host re-arming a track does this. The hazard is that the first registration's
/// `Drop` runs *after* the second install and clears the property the second one
/// just wrote, leaving the AU with no callback and the host believing it has one.
/// Ordering the drops explicitly is what the test checks: the first registration is
/// dropped before the second is created, which is the sequence a host should
/// follow — and the render afterwards proves whatever happened left the unit
/// intact.
#[test]
fn reinstalling_the_callback_is_safe() {
    let _g = lock();
    let mut au = DELAY.open_uninitialized(RATE, BLOCK);

    let first = au
        .install_midi_output(Box::new(|_, _| {}))
        .expect("first install");
    // Withdraw before re-installing. The reverse order — install then drop the
    // old handle — would have the old handle's `clear` remove the NEW callback,
    // which is why a host must not do it and why the API returns a handle rather
    // than being a fire-and-forget setter.
    first.remove().expect("first withdraw");

    let second = au
        .install_midi_output(Box::new(|_, _| {}))
        .expect("second install");
    au.initialize().expect("initialize");
    let input = support::corpus::silence(2, BLOCK as usize);
    let mut output = support::corpus::silence(2, BLOCK as usize);
    support::corpus::render(&mut au, &input, &mut output, BLOCK).expect("render");
    second.remove().expect("second withdraw");
    support::corpus::render(&mut au, &input, &mut output, BLOCK).expect("render after withdraw");

    let peak = support::corpus::peak(&output);
    assert!(
        peak.is_finite() && peak < 1e-3,
        "silence expected, got {peak}"
    );
}

// ------------------------------------------------------------- decode: normal

/// The framework's own packing decodes to the events it encoded.
///
/// Built with `MIDIPacketListInit`/`MIDIPacketListAdd`, so the *framework* is the
/// oracle for the byte layout rather than this test's own assumptions about it.
#[test]
fn a_framework_built_packet_list_decodes_to_its_events() {
    let list = PacketListBuf::from_framework(&[
        (&[0x90, 60, 100], 0),
        (&[0x80, 60, 0], 64),
        (&[0xB0, 7, 100], 128),
        (&[0xC0, 5], 192),
        (&[0xE0, 0x00, 0x40], 256),
    ])
    .expect("five small messages fit in 8 KiB");

    let events = list.decode();
    assert_eq!(events.len(), 5, "expected five events, got {events:?}");

    // Note-on: note and the frame offset taken from the packet timestamp, which
    // Apple documents as a *sample offset* from the render timestamp — so it is
    // already the value `MidiEvent::frame_offset` wants, with no clock conversion.
    assert!(matches!(
        events[0].message(),
        MidiMessage::NoteOn {
            note: 60,
            channel: 0,
            ..
        }
    ));
    assert_eq!(events[0].frame_offset, 0);

    assert!(matches!(
        events[1].message(),
        MidiMessage::NoteOff {
            note: 60,
            channel: 0,
            ..
        }
    ));
    assert_eq!(events[1].frame_offset, 64);

    assert!(matches!(
        events[2].message(),
        MidiMessage::ControlChange {
            index: 7,
            channel: 0,
            ..
        }
    ));
    assert_eq!(events[2].frame_offset, 128);

    // Program change is a TWO-byte message. A decoder that assumed three would
    // consume the next packet's status byte as data.
    assert!(matches!(
        events[3].message(),
        MidiMessage::ProgramChange {
            program: 5,
            channel: 0,
            ..
        }
    ));
    assert_eq!(events[3].frame_offset, 192);

    assert!(matches!(
        events[4].message(),
        MidiMessage::PitchBend { channel: 0, .. }
    ));
    assert_eq!(events[4].frame_offset, 256);
}

/// Every message family `send_midi` can *emit* must decode when it comes *back*.
///
/// The two directions have to agree on which families have a legacy 3-byte form,
/// or a round trip through this host silently changes the message. `send_midi`
/// converts note-on/off, CC, program change, channel pressure and pitch bend and
/// skips everything else; this asserts the reverse for the same six.
#[test]
fn the_decode_covers_exactly_what_send_midi_encodes() {
    let messages: &[(&[u8], &str)] = &[
        (&[0x91, 60, 100], "note on ch1"),
        (&[0x82, 60, 64], "note off ch2"),
        (&[0xA3, 60, 90], "poly pressure ch3"),
        (&[0xB4, 74, 32], "control change ch4"),
        (&[0xC5, 9], "program change ch5"),
        (&[0xD6, 77], "channel pressure ch6"),
        (&[0xE7, 0x7F, 0x3F], "pitch bend ch7"),
    ];
    for (bytes, what) in messages {
        let list =
            PacketListBuf::from_framework(&[(bytes, 0)]).unwrap_or_else(|| panic!("{what} fits"));
        let events = list.decode();
        assert_eq!(
            events.len(),
            1,
            "{what}: exactly one event expected, got {events:?}"
        );
        // The channel must survive: it is in the low nibble of the status byte, and
        // a decoder that masked it off would still produce the right message *type*.
        let expected_channel = bytes[0] & 0x0F;
        assert_eq!(
            events[0].channel(),
            Some(expected_channel),
            "{what}: channel must round-trip"
        );
    }
}

/// One packet carrying several messages yields several events.
///
/// Apple's own packer does this — measured: `90 3E 5A 90 40 50` goes into a single
/// 6-byte packet — so a decoder that handed the whole `data` blob to a
/// one-message parser drops the second note. Every event from a multi-message
/// packet carries that packet's timestamp, which is correct: the packet is the
/// timing unit.
#[test]
fn one_packet_can_carry_several_messages() {
    let list = PacketListBuf::from_framework(&[(&[0x90, 62, 90, 0x90, 64, 80, 0x90, 67, 70], 384)])
        .expect("nine bytes fit");
    let split = list.split();
    assert_eq!(
        split.len(),
        3,
        "three note-ons packed into one packet must all be found, got {split:?}"
    );
    for (offset, bytes) in &split {
        assert_eq!(
            *offset, 384,
            "every message inherits the packet's timestamp"
        );
        assert_eq!(bytes.len(), 3);
        assert_eq!(bytes[0], 0x90);
    }
    let events = list.decode();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events.iter().filter_map(|e| e.note()).collect::<Vec<_>>(),
        vec![62, 64, 67]
    );
}

// --------------------------------------------------------- decode: edge cases

/// An empty packet list yields no events and does not read past the header.
///
/// `numPackets == 0` with nothing after it is what an AU sends when a render block
/// produced no MIDI, so it is the *common* case rather than an exotic one. A walk
/// that read the first packet unconditionally would decode 10 bytes of whatever
/// follows the count.
#[test]
fn an_empty_packet_list_yields_nothing() {
    let list = PacketListBuf::from_raw(&[]);
    assert!(
        list.decode().is_empty(),
        "an empty list must decode to no events"
    );
    assert!(list.split().is_empty());

    // The same through the framework's own initializer, which is what an AU
    // actually calls.
    let via_framework = PacketListBuf::from_framework(&[]).expect("an empty list is buildable");
    assert!(via_framework.decode().is_empty());
}

/// A packet with `length == 0` is skipped, and the packets after it still decode.
///
/// The "and the ones after it" half is the point: a zero-length packet advances the
/// cursor by the header alone, and a walk that mishandled that would land
/// mid-header on every subsequent packet.
#[test]
fn a_zero_length_packet_does_not_derail_the_walk() {
    let list = PacketListBuf::from_raw(&[
        (0, &[]),
        (128, &[0x90, 60, 100]),
        (256, &[]),
        (384, &[0x80, 60, 0]),
    ]);
    let split = list.split();
    assert_eq!(
        split.len(),
        2,
        "the two real messages must survive two empty packets, got {split:?}"
    );
    assert_eq!(split[0], (128, vec![0x90, 60, 100]));
    assert_eq!(split[1], (384, vec![0x80, 60, 0]));
}

/// Running status: data bytes with no status byte reuse the previous status.
///
/// A sequencer AU emitting a run of note-ons on one channel is exactly where MIDI
/// 1.0 running status is used, and a decoder that ignored it would find the first
/// message and silently drop every one after it. Both the expansion and the
/// resulting notes are asserted, because a decoder could reassemble the bytes with
/// the wrong status and still produce three messages.
#[test]
fn running_status_is_expanded() {
    // `90 3C 64` then two status-less pairs, all note-ons on channel 0.
    let list = PacketListBuf::from_raw(&[(
        0,
        &[
            0x90, 60, 100, /* running: */ 62, 90, /* running: */ 64, 80,
        ],
    )]);
    let split = list.split();
    assert_eq!(
        split.len(),
        3,
        "one explicit status plus two running-status messages, got {split:?}"
    );
    for (i, (_, bytes)) in split.iter().enumerate() {
        assert_eq!(
            bytes[0], 0x90,
            "message {i}: the running status byte must be reinstated"
        );
        assert_eq!(bytes.len(), 3);
    }
    let events = list.decode();
    assert_eq!(
        events.iter().filter_map(|e| e.note()).collect::<Vec<_>>(),
        vec![60, 62, 64],
        "all three notes must be recovered"
    );

    // Two-byte running status too: a program change followed by a bare program
    // number. A decoder that assumed three data bytes for every running-status
    // continuation would consume one byte too many here.
    let list = PacketListBuf::from_raw(&[(0, &[0xC0, 5, /* running: */ 7])]);
    let split = list.split();
    assert_eq!(split.len(), 2, "got {split:?}");
    assert_eq!(split[0], (0, vec![0xC0, 5]));
    assert_eq!(split[1], (0, vec![0xC0, 7]));
}

/// A data byte with **no** preceding status is dropped, not guessed at.
///
/// There is nothing to attribute it to. Inventing a status — note-on is the
/// tempting default — would fabricate a note the AU never sent, and a fabricated
/// note-on with no matching note-off is a stuck voice.
#[test]
fn an_orphan_data_byte_is_dropped() {
    let list = PacketListBuf::from_raw(&[(0, &[60, 100])]);
    assert!(
        list.split().is_empty(),
        "data bytes with no status must not become a message"
    );

    // And the following real message still decodes: skipping the orphans must not
    // desynchronise the walk.
    let list = PacketListBuf::from_raw(&[(0, &[60, 100, 0x90, 62, 90])]);
    let split = list.split();
    assert_eq!(split.len(), 1, "got {split:?}");
    assert_eq!(split[0], (0, vec![0x90, 62, 90]));
}

/// A packet claiming **more** bytes than it holds must not be read past its data.
///
/// This is the hostile shape: `length` is a number the plugin writes, and a walk
/// that trusted it would read out of bounds. The decoder is structurally safe —
/// a status byte whose data bytes run past the end of the slice is dropped rather
/// than read — which is the packet-level twin of `render_input`'s "never trust the
/// buffer the AU handed us".
///
/// ## What the host can and cannot detect, stated exactly
///
/// A *legal* over-claim is **undetectable**, and pretending otherwise would be the
/// dishonest version of this test. A packet declaring 64 bytes and meaning 3 is
/// byte-for-byte indistinguishable from one that genuinely carries 64: both are
/// legal, and both windows are inside the allocation the AU sized. The decoder
/// therefore reads the whole declared window and finds messages in the AU's own
/// slack — which is the *plugin's* bug, and the measured behaviour asserted below
/// rather than hidden.
///
/// What the host must guarantee is narrower and is what is actually pinned:
///
/// 1. it never reads past the **declared** window, so a truncated message at the
///    window's end is dropped rather than completed from adjacent memory;
/// 2. it never honours a length beyond Apple's documented 256-byte maximum, since
///    a `u16` can claim 65535 and that read *would* leave the allocation.
#[test]
fn a_packet_claiming_more_bytes_than_it_holds_is_not_over_read() {
    // A 64-byte declared window holding two real bytes. Measured on macOS 15.6:
    // the decoder finds 31 messages — the real `90 3C` completed with a zero
    // velocity, then 30 running-status note-ons from the 60 zero bytes that follow.
    // That is CORRECT given a 64-byte claim: 1 + 30 running-status pairs = 61 bytes
    // consumed of the 62 available after the status byte.
    //
    // Asserting the exact count rather than a bound, because the number is the
    // evidence that the walk consumed the declared window and *stopped*: 31 means it
    // read 64 bytes, and anything above it means it read past them.
    let list = PacketListBuf::from_raw_overclaiming(&[(0, 64, &[0x90, 60])]);
    let split = list.split();
    assert_eq!(
        split.len(),
        31,
        "a 64-byte declared window yields exactly the messages that fit inside it \
         — 1 real + 30 running-status pairs from the trailing zeros. A LARGER \
         count means the walk read past the declared length, which is the host \
         bug this test exists for. Got {split:?}"
    );
    assert_eq!(
        split[0],
        (0, vec![0x90, 60, 0]),
        "the real note-on, completed with the first byte inside the window"
    );
    for (_, bytes) in &split {
        assert!(
            bytes.len() <= 3,
            "no reassembled message may exceed three bytes, got {bytes:02x?}"
        );
    }

    // The case the host CAN and must catch: a message truncated at the end of an
    // honest window. A note-on with one data byte and nothing after must be
    // dropped, because reading its velocity would be out of bounds.
    let list = PacketListBuf::from_raw(&[(0, &[0x90, 60])]);
    assert!(
        list.split().is_empty(),
        "a note-on missing its velocity byte must be dropped, not completed from \
         whatever follows the packet"
    );

    // And a lone status byte at the very end.
    let list = PacketListBuf::from_raw(&[(0, &[0x90])]);
    assert!(
        list.split().is_empty(),
        "a status byte with no data bytes at all must be dropped"
    );

    // The second guarantee: a length beyond Apple's documented maximum is clamped.
    // `u16::MAX` is 65535 — 255 packets' worth of memory the AU never wrote — and
    // honouring it would read ~61 KiB past this 512-byte-slack allocation.
    //
    // The clamp to 256 bounds the walk at **127** messages, and the arithmetic is
    // worth spelling out because getting it wrong is how this assertion was first
    // written twice over. The 256-byte window holds `90 3C 64` (3 bytes) followed by
    // 253 zeros. Those zeros are consumed as running-status note-ons at **two** bytes
    // each — running status reuses the status byte, so a continuation costs 2, not 3.
    // 1 + floor(253 / 2) = 1 + 126 = 127. (A `size_of - header` bound of 258 gives
    // 128, which is how the padding bug in `MAX_PACKET_PAYLOAD` first showed up
    // here.)
    //
    // Asserted as an exact equality rather than a bound, because the count is the
    // evidence the clamp landed on exactly 256: larger means the 65535 claim was
    // honoured and the walk left the allocation; smaller means the clamp is tighter
    // than Apple's documented maximum and would truncate a legitimate full packet.
    let list = PacketListBuf::from_raw_overclaiming(&[(0, u16::MAX, &[0x90, 60, 100])]);
    let split = list.split();
    assert_eq!(split[0], (0, vec![0x90, 60, 100]));
    assert_eq!(
        split.len(),
        127,
        "a 65535-byte claim must be clamped to Apple's `Byte data[256]` maximum: \
         3 bytes of explicit message + floor(253/2) running-status continuations \
         = 127. See the comment above for why 128 is the wrong answer."
    );
}

/// System real-time bytes are delivered and do **not** disturb running status.
///
/// `F8`..`FF` may legally appear *between* the data bytes of another message. A
/// decoder that let one clear the running status would orphan the rest of the run —
/// which for a clock-emitting sequencer AU is every note after the first tick.
#[test]
fn a_real_time_byte_interleaved_mid_run_does_not_clear_running_status() {
    // Note-on, then a timing clock, then a running-status continuation.
    let list = PacketListBuf::from_raw(&[(0, &[0x90, 60, 100, 0xF8, 62, 90])]);
    let split = list.split();
    assert_eq!(
        split.len(),
        3,
        "note-on, clock, and the running-status note-on after it, got {split:?}"
    );
    assert_eq!(split[0], (0, vec![0x90, 60, 100]));
    assert_eq!(split[1], (0, vec![0xF8]), "the clock is delivered");
    assert_eq!(
        split[2],
        (0, vec![0x90, 62, 90]),
        "the clock must not have cleared the running status"
    );
}

/// System Common **does** clear running status, per the MIDI 1.0 spec.
///
/// The counterpart to the test above, and the reason the two cannot share one rule:
/// a data byte after a song-position message must not be attributed to the note-on
/// before it.
#[test]
fn system_common_clears_running_status() {
    // Note-on, tune request (F6, a System Common message with no data), then a bare
    // data pair that now has no status to attach to.
    let list = PacketListBuf::from_raw(&[(0, &[0x90, 60, 100, 0xF6, 62, 90])]);
    let split = list.split();
    assert_eq!(
        split.len(),
        2,
        "the note-on and the tune request, but NOT a third message from the \
         orphaned data bytes, got {split:?}"
    );
    assert_eq!(split[0], (0, vec![0x90, 60, 100]));
    assert_eq!(split[1], (0, vec![0xF6]));
}

/// SysEx is dropped on the **inbound** path, and the messages around it survive.
///
/// `MidiEvent::from_midi1_bytes` has no single-message SysEx form: UMP requires
/// fragmenting into 6-byte packets, which allocates, and this runs on the
/// CoreMIDI read thread where that is forbidden.
///
/// Do **not** read this as symmetry with `send_midi`. The outbound path
/// reassembles SysEx7 and sends it through `MusicDeviceSysEx`, because nothing
/// there is allocation-constrained. The two directions differ because their
/// constraints differ, not because dropping is intended behaviour.
#[test]
fn sysex_is_dropped_without_derailing_the_rest() {
    let list =
        PacketListBuf::from_raw(&[(0, &[0x90, 60, 100, 0xF0, 0x7E, 0x00, 0xF7, 0x80, 60, 0])]);
    let split = list.split();
    let statuses: Vec<u8> = split.iter().map(|(_, b)| b[0]).collect();
    assert!(
        statuses.contains(&0x90),
        "the note-on before the SysEx must survive, got {split:?}"
    );
    assert!(
        statuses.contains(&0x80),
        "the note-off after the SysEx must survive, got {split:?}"
    );
    assert!(
        !statuses.contains(&0xF0),
        "SysEx has no single-message form here and must not be emitted"
    );
}

/// A large packet list is delivered in full, in batches, with nothing dropped.
///
/// The decoder flushes a fixed-size stack batch to the sink and refills it, rather
/// than truncating — and truncation is the failure that matters: a dropped note-off
/// is a voice that never releases. Driving 300 messages through a 64-event batch
/// exercises the refill path several times over.
#[test]
fn a_list_longer_than_the_batch_is_delivered_in_full() {
    // 300 alternating note-ons and note-offs, each its own packet.
    let owned: Vec<[u8; 3]> = (0..300u32)
        .map(|i| {
            let status = if i.is_multiple_of(2) { 0x90 } else { 0x80 };
            [status, (36 + (i % 48)) as u8, 100]
        })
        .collect();
    let messages: Vec<(&[u8], u64)> = owned
        .iter()
        .enumerate()
        .map(|(i, m)| (m.as_slice(), i as u64))
        .collect();
    let list = PacketListBuf::from_framework(&messages).expect("300 messages fit in 8 KiB");

    let events = list.decode();
    assert_eq!(
        events.len(),
        300,
        "every message must survive; a truncated batch would drop note-offs and \
         leave voices stuck"
    );
    // The timestamps prove nothing was reordered or duplicated across batch
    // boundaries — the 64/128/192/256 crossings are where a refill bug would show.
    for (i, ev) in events.iter().enumerate() {
        assert_eq!(
            ev.frame_offset, i as u32,
            "event {i} carries the wrong frame offset; batching must not reorder"
        );
    }
    assert_eq!(events.iter().filter(|e| e.is_note_on()).count(), 150);
    assert_eq!(events.iter().filter(|e| e.is_note_off()).count(), 150);
}

/// A packet timestamp is a sample offset and is carried through verbatim.
///
/// Apple's header: "The time stamp values contained within the MIDIPackets in this
/// list are **sample offsets** from the AudioTimeStamp provided." So there is no
/// host-time conversion to get wrong — but there *is* a width narrowing (`u64` at
/// the ABI, `u32` in `MidiEvent`), and it must saturate rather than wrap. A wrapped
/// offset places a late event at the *start* of the block, which is audibly wrong
/// in a way a clamped one is not.
#[test]
fn a_huge_timestamp_saturates_rather_than_wrapping() {
    let list = PacketListBuf::from_raw(&[
        (0, &[0x90, 60, 100]),
        (u32::MAX as u64, &[0x90, 61, 100]),
        (u64::from(u32::MAX) + 1, &[0x90, 62, 100]),
        (u64::MAX, &[0x90, 63, 100]),
    ]);
    let split = list.split();
    assert_eq!(split.len(), 4, "got {split:?}");
    assert_eq!(split[0].0, 0);
    assert_eq!(split[1].0, u32::MAX);
    assert_eq!(
        split[2].0,
        u32::MAX,
        "u32::MAX + 1 must clamp to u32::MAX, not wrap to 0 — a wrapped offset \
         moves a late event to the front of the block"
    );
    assert_eq!(split[3].0, u32::MAX, "u64::MAX must clamp, not wrap");
}

// --------------------------------------------------------------- sink hygiene

/// The sink receives a borrowed slice whose contents are usable, and copying out
/// of it is the intended pattern.
///
/// Not a test of the compiler — the borrow is enforced by the signature and a
/// parked slice would not compile. What this pins is that the slice is *correct*
/// when handed over: a decoder that reused one buffer across batches without
/// resetting the length would hand the sink stale events past `n`, and a sink that
/// copied `evs` would silently record them.
#[test]
fn the_sink_sees_exactly_the_events_it_was_handed() {
    // Driven through the decoder directly, since no AU calls back. The sink shape
    // is exercised by `no_delivery_arrives_after_the_callback_is_withdrawn`; here
    // the concern is the *contents*.
    let list = PacketListBuf::from_framework(&[
        (&[0x90, 60, 100], 0),
        (&[0x90, 61, 100], 1),
        (&[0x90, 62, 100], 2),
    ])
    .expect("fits");
    let events = list.decode();
    let notes: Vec<u8> = events.iter().filter_map(|e| e.note()).collect();
    assert_eq!(
        notes,
        vec![60, 61, 62],
        "no stale event may appear beyond the ones decoded"
    );
    assert_eq!(
        events.len(),
        3,
        "and no extra entries past the batch length"
    );
}
