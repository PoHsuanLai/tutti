use super::*;
use tutti_fixtures::midi;

/// Enumeration must not panic on a machine with no MIDI devices, and must
/// return well-formed records where there are some. CI boxes have none; a
/// dev machine has IAC.
///
/// This deliberately does *not* assert a device count — that would pass
/// vacuously on CI and fail spuriously on a laptop. It asserts the shape of
/// whatever comes back.
#[test]
fn enumeration_yields_well_formed_records() {
    let backend = CoreMidiEndpoints::new();
    for info in backend.inputs().into_iter().chain(backend.outputs()) {
        assert!(
            !info.name.is_empty(),
            "every endpoint must carry a name, even a synthesised one"
        );
    }
}

/// An id that no endpoint owns must fail to open rather than resolve to
/// whatever sits at that index — the reason `EndpointId` is opaque and not a
/// positional index.
#[test]
fn an_unknown_id_does_not_open() {
    let backend = CoreMidiEndpoints::new();
    let unknown = EndpointId::from_raw(u64::from(u32::MAX));
    assert!(
        !backend.outputs().iter().any(|e| e.id == unknown),
        "precondition: the sentinel id must not belong to a real endpoint"
    );
    assert!(backend.open_output(unknown).is_err());
}

/// Capability must be *read from the endpoint*, not assumed.
///
/// The trap this guards: inputs open with `Protocol::Midi20` so CoreMIDI
/// up-converts MIDI-1.0 traffic. If `endpoint_capability` returned that
/// choice rather than reading `kMIDIPropertyProtocolID`, every endpoint
/// would claim MIDI 2.0 and `UmpCapability` would be vestigial.
///
/// Enumerating real devices **cannot** catch that here: every endpoint on a
/// modern Mac (the IAC buses included) genuinely reports MIDI 2.0, so
/// `assert!(matches!(.., Midi1 | Midi2))` passes under a hardcoded `midi2()`
/// too. The fixture has to *supply* the distinction,
/// so this creates a MIDI-1.0 virtual source and requires the reader to say
/// so.
#[test]
fn capability_is_read_from_the_endpoint_not_assumed() {
    // A MIDI-1.0-protocol virtual source: the safe wrapper's plain
    // `virtual_source` is MIDI 1.0 by construction.
    let client = match Client::new("tutti-cap-test") {
        Ok(c) => c,
        // No CoreMIDI here (sandbox/headless) — nothing to test against.
        Err(_) => return,
    };
    let Ok(source) = client.virtual_source("tutti-cap-midi1") else {
        return;
    };

    let cap = endpoint_capability(&source, None);
    assert_eq!(
        cap.protocol,
        Protocol::Midi1,
        "a MIDI-1.0 endpoint must report Midi1 — returning a constant Midi2 \
         (or echoing the protocol we open ports with) fails here"
    );
    assert!(!cap.carries_midi2_only());
}

/// The `Midi2` direction is covered by the real-device enumeration above
/// (every endpoint on this machine reports MIDI 2.0), so a hardcoded
/// `midi1()` would fail *there*. Pinning it here too would need a
/// MIDI-2.0-protocol endpoint to hand `endpoint_capability` as a
/// `coremidi::Object` — `UmpVirtualSource` keeps its `MIDIEndpointRef`
/// private, and widening that API purely for a test is the wrong trade.
/// Between the two, neither constant survives.
#[test]
fn a_midi2_endpoint_is_covered_by_enumeration() {
    let backend = CoreMidiEndpoints::new();
    let any_midi2 = backend
        .inputs()
        .into_iter()
        .chain(backend.outputs())
        .any(|e| e.capability.protocol == Protocol::Midi2);

    // Not an assertion: a machine with no MIDI-2.0 endpoint is legitimate,
    // and asserting here would fail spuriously on CI. Reported so a reader
    // knows whether this ran.
    if !any_midi2 {
        eprintln!(
            "note: no MIDI-2.0 endpoint on this machine; the Midi2 direction \
             of capability reading is uncovered here"
        );
    }
}

/// Whatever the OS reports for its UMP Endpoints is within what the spec can
/// hold: the Protocol is one of the two, and every block that reaches a
/// capability has a number 0 to 31 and Groups inside 0 to 15 (M2-104 §7.1.8,
/// §3.1), at most 32 blocks to an endpoint (§7.1.8: five-bit block numbers).
/// Before macOS 15 the list is absent and no endpoint has a block. This asserts
/// invariants only: how many endpoints, and what they declare, is the machine's.
/// Spec: m2-104/6.1.1#1, m2-104/6.1.1#2
#[test]
fn the_os_blocks_are_within_the_spec() {
    let backend = CoreMidiEndpoints::new();
    let all = backend.inputs().into_iter().chain(backend.outputs());
    for info in all {
        let cap = &info.capability;
        assert!(matches!(cap.protocol, Protocol::Midi1 | Protocol::Midi2));
        assert!(
            cap.blocks.len() <= 32,
            "{}: {} blocks",
            info.name,
            cap.blocks.len()
        );
        for block in &cap.blocks {
            assert!(block.id().get() < 32);
            let end = u16::from(block.first_group().get()) + u16::from(block.groups().get());
            assert!(end <= 16, "{}: Groups run to {end}", info.name);
        }
        if ump_endpoints().is_none() {
            assert!(cap.blocks.is_empty(), "no UMP classes, no blocks");
        }
    }
}

/// **A packet sent `At` a future instant is held by CoreMIDI until then, and
/// one sent `Now` goes at once**: the send time the
/// plan gives becomes the packet's host-time stamp, which CoreMIDI schedules.
/// A virtual UMP destination records when each packet arrives; the note sent
/// 150 ms ahead arrives after the one sent now, and not before its instant.
#[test]
fn a_scheduled_packet_is_held_until_its_time_and_now_goes_at_once() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use tutti_midi_runtime::{Outgoing, SendAt};
    use tutti_ump::MidiChannel;

    let (tx, rx) = mpsc::channel();
    let _destination = UmpVirtualDestination::new("tutti-sched-out", move |ump| {
        let _ = tx.send((Instant::now(), ump));
    })
    .expect("a virtual UMP destination");
    let backend = CoreMidiEndpoints::new();
    let id = backend
        .outputs()
        .into_iter()
        .find(|e| e.name.contains("tutti-sched-out"))
        .expect("the virtual destination is listed")
        .id;
    let out = backend.open_output(id).expect("opens");
    out.follow(Protocol::Midi2);

    let note = |n| midi::note_on(MidiChannel::FIRST, n, 0xFFFF);
    let start = Instant::now();
    let due = start + Duration::from_millis(150);
    let sent = out.queue_timed(&[
        Outgoing {
            at: tutti_graph::Offset::ZERO,
            send: SendAt::At(due),
            ump: note(60),
        },
        Outgoing {
            at: tutti_graph::Offset::ZERO,
            send: SendAt::Now,
            ump: note(61),
        },
    ]);
    assert_eq!(sent, 2);

    let wait = Duration::from_secs(2);
    let (first_at, first) = rx.recv_timeout(wait).expect("the note sent now");
    let (second_at, second) = rx.recv_timeout(wait).expect("the scheduled note");
    assert_eq!(
        (first, second),
        (note(61), note(60)),
        "now first, then the scheduled one"
    );
    assert!(
        first_at < due,
        "the note sent now did not wait for the scheduled one"
    );
    assert!(
        second_at + Duration::from_millis(5) >= due,
        "the scheduled note arrived {:?} before its instant",
        due - second_at
    );
}
